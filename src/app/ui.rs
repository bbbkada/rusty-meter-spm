use egui::{FontFamily, FontId, SliderClamping, Vec2};
use egui_dock::{DockArea, DockState, Style, TabViewer};
use egui_dropdown::DropDownBox;
use mio_serial::{DataBits, SerialPort, SerialPortBuilderExt};
use std::collections::VecDeque;

use crate::helpers::{format_measurement, powered_by};
use crate::multimeter::MeterMode;

// Enum to represent tab types
#[derive(Clone, PartialEq)]
pub enum PlotTab {
    Graph,
    Histogram,
}

// Tab viewer implementation for PlotTab
struct PlotTabViewer<'a> {
    values: &'a VecDeque<f64>,
    hist_values: &'a mut VecDeque<f64>,
    reverse_graph: &'a mut bool,
    graph_line_color: egui::Color32,
    hist_bar_color: egui::Color32,
    mem_depth: &'a mut usize,
    curr_meas: f64,
    metermode: MeterMode,
    graph_config: &'a mut super::graph::GraphConfig,
    hist_collect_active: &'a mut bool,
    hist_collect_interval_ms: &'a mut u64,
    hist_mem_depth: &'a mut usize,
    mem_depth_max: usize,
    graph_update_interval_ms: &'a mut u64,
    graph_update_interval_max: u64,
    hist_mem_depth_max: usize,
    curr_unit: &'a str,
}

impl TabViewer for PlotTabViewer<'_> {
    type Tab = PlotTab;

    fn title(&mut self, tab: &mut Self::Tab) -> egui::WidgetText {
        match tab {
            PlotTab::Graph => "Graph".into(),
            PlotTab::Histogram => "Histogram".into(),
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, tab: &mut Self::Tab) {
        match tab {
            PlotTab::Graph => super::graph::show_line_graph(
                ui,
                self.values,
                *self.reverse_graph,
                self.graph_line_color,
                self.mem_depth,
                self.graph_update_interval_ms,
                self.reverse_graph,
                self.mem_depth_max,
                self.graph_update_interval_max,
                self.curr_unit,
            ),
            PlotTab::Histogram => super::graph::show_histogram(
                ui,
                self.hist_values,
                self.curr_meas,
                self.metermode,
                self.graph_config,
                self.hist_bar_color,
                self.hist_collect_active,
                self.hist_collect_interval_ms,
                self.hist_mem_depth,
                self.hist_mem_depth_max,
            ),
        }
    }
}

impl super::MyApp {
    /// Called by the framework to save state before shutdown.
    pub fn save(&mut self, storage: &mut dyn eframe::Storage) {
        if self.recording_active {
            self.save_recording_data();
        }
        eframe::set_value(storage, eframe::APP_KEY, self);
    }

    /// Called each time the UI needs repainting.
    pub fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let is_web = cfg!(target_arch = "wasm32");

        // Apply always-on-top on first frame
        if !self.is_init && self.always_on_top {
            ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
                egui::viewport::WindowLevel::AlwaysOnTop,
            ));
        }

        // Handle spacebar to toggle hold
        if ctx.input(|i| i.key_pressed(egui::Key::Space)) {
            self.hold_enabled = !self.hold_enabled;
            *self.hold_enabled_shared.lock().unwrap() = self.hold_enabled;
            if self.value_debug { println!("Hold toggled via spacebar: {}", self.hold_enabled); }
            if !self.hold_enabled {
                self.last_graph_update = 0.0;
            }
            let supports_hold = self.plugin.lock().unwrap().capabilities().has_hold;
            if supports_hold {
                if let Some(tx) = self.serial_tx.clone() {
                    let plugin = self.plugin.lock().unwrap();
                    let cmd = plugin.hold_command(self.hold_enabled);
                    drop(plugin);
                    if let Some(cmd) = cmd {
                        let value_debug = self.value_debug;
                        tokio::spawn(async move {
                            if let Err(e) = tx.send(cmd).await {
                                if value_debug { println!("Failed to queue hold command: {}", e); }
                            }
                        });
                    }
                }
            }
        }

        // On startup, handle initialization
        if !self.is_init {
            if let Ok(ports) = mio_serial::available_ports() {
                for p in ports {
                    self.portlist.push_front(p.port_name);
                }
            }

            // Initialize dock state
            let tabs = vec![PlotTab::Graph, PlotTab::Histogram];
            self.plot_dock_state = DockState::new(tabs);

            // Auto-connect if enabled
            if self.connect_on_startup && !self.serial_port.is_empty() {
                self.connection_state = super::ConnectionState::Connecting;
                self.connection_error = None;
                match mio_serial::new(&self.serial_port, self.baud_rate)
                    .open_native_async()
                {
                    Ok(serial) => {
                        self.serial = Some(serial);
                        if let Some(ref mut serial) = self.serial {
                            let _ = serial.set_data_bits(DataBits::Eight);
                            let _ = serial.set_stop_bits(mio_serial::StopBits::One);
                            let _ = serial.set_parity(mio_serial::Parity::None);
                            self.connection_state = super::ConnectionState::Connected;
                            self.spawn_serial_task();
                            self.spawn_graph_update_task(ctx.clone());
                        }
                    }
                    Err(e) => {
                        self.connection_state = super::ConnectionState::Disconnected;
                        self.connection_error = Some(format!("Failed to auto-connect: {}", e));
                    }
                }
            }

            self.is_init = true;
        }

        // Process all available measurements
        if let Some(ref mut rx) = self.serial_rx {
            while let Ok(meas_opt) = rx.try_recv() {
                if let Some((meas, precision)) = meas_opt {
                    self.curr_meas = meas;
                    self.curr_precision = precision;
                }
            }
        }

        // Process mode updates — use plugin for unit lookup
        if let Some(ref mut rx) = self.mode_rx {
            while let Ok(mode) = rx.try_recv() {
                if mode != self.metermode {
                    self.metermode = mode;
                    self.values = VecDeque::with_capacity(self.mem_depth);
                    self.hist_values = VecDeque::with_capacity(self.hist_mem_depth);
                    let plugin = self.plugin.lock().unwrap();
                    self.curr_unit = plugin.mode_unit(mode).to_owned();
                    drop(plugin);
                    self.curr_range = 0;
                }

                // Clear pending mode flags when instrument confirms
                if self.pending_mode_change == Some(mode) {
                    if self.value_debug {
                        println!("Mode {:?} confirmed by instrument, clearing pending mode change", mode);
                    }
                    self.pending_mode_change = None;
                    let mut pending = self.pending_changes.lock().unwrap();
                    if let Some((target, _, _)) = &pending.mode {
                        if *target == mode {
                            pending.mode = None;
                        }
                    }
                }
            }
        }

        // Handle range updates from serial task
        if let Some(rx) = &mut self.range_rx {
            while let Ok((mode, range_idx)) = rx.try_recv() {
                if self.pending_mode_change.is_some() {
                    continue;
                }
                if mode != self.metermode {
                    continue;
                }
                {
                    let mut pending = self.pending_changes.lock().unwrap();
                    if let Some((expected_idx, _)) = &pending.range {
                        if *expected_idx == 0 && range_idx == 0 {
                            // Auto range confirmed by instrument (status=AUTO → index 0)
                            pending.range = None;
                            self.curr_range = 0;
                        } else if *expected_idx != 0 && range_idx == *expected_idx {
                            // Fixed range confirmed by instrument
                            pending.range = None;
                            self.curr_range = range_idx;
                        }
                        // Otherwise keep pending — instrument hasn't confirmed yet
                        continue;
                    }
                }
                self.curr_range = range_idx;
            }
        }

        // Handle power supply state updates
        if let Some(rx) = &mut self.ps_rx {
            while let Ok(ps_state) = rx.try_recv() {
                let mut pending = self.pending_changes.lock().unwrap();

                if let Some(expected) = pending.output_on {
                    if ps_state.output_on == expected {
                        pending.output_on = None;
                        self.ps_output_on = ps_state.output_on;
                    }
                } else {
                    self.ps_output_on = ps_state.output_on;
                }

                self.ps_voltage_readback = ps_state.voltage_readback;
                self.ps_current_readback = ps_state.current_readback;
                self.ps_power_readback = ps_state.power_readback;

                if ps_state.includes_settings {
                    let editing = self.ps_input_has_focus;
                    if let Some(pv) = pending.voltage_set {
                        if (ps_state.voltage_set - pv).abs() < 0.002 {
                            pending.voltage_set = None;
                            self.ps_voltage_set = ps_state.voltage_set;
                            if !editing { self.ps_voltage_input = format!("{:.3}", ps_state.voltage_set); }
                        }
                    } else {
                        self.ps_voltage_set = ps_state.voltage_set;
                        if !editing { self.ps_voltage_input = format!("{:.3}", ps_state.voltage_set); }
                    }
                    if let Some(pv) = pending.current_set {
                        if (ps_state.current_set - pv).abs() < 0.002 {
                            pending.current_set = None;
                            self.ps_current_set = ps_state.current_set;
                            if !editing { self.ps_current_input = format!("{:.3}", ps_state.current_set); }
                        }
                    } else {
                        self.ps_current_set = ps_state.current_set;
                        if !editing { self.ps_current_input = format!("{:.3}", ps_state.current_set); }
                    }
                    if let Some(pv) = pending.ovp {
                        if (ps_state.ovp - pv).abs() < 0.002 {
                            pending.ovp = None;
                            self.ps_ovp = ps_state.ovp;
                            if !editing { self.ps_ovp_input = format!("{:.3}", ps_state.ovp); }
                        }
                    } else {
                        self.ps_ovp = ps_state.ovp;
                        if !editing { self.ps_ovp_input = format!("{:.3}", ps_state.ovp); }
                    }
                    if let Some(pv) = pending.ocp {
                        if (ps_state.ocp - pv).abs() < 0.002 {
                            pending.ocp = None;
                            self.ps_ocp = ps_state.ocp;
                            if !editing { self.ps_ocp_input = format!("{:.3}", ps_state.ocp); }
                        }
                    } else {
                        self.ps_ocp = ps_state.ocp;
                        if !editing { self.ps_ocp_input = format!("{:.3}", ps_state.ocp); }
                    }
                }
                if !self.ps_initial_sync_done {
                    self.ps_initial_sync_done = true;
                }
            }
        }

        // Detect confirmed non-PS device
        if self.connection_state == super::ConnectionState::Connected {
            let device = self.device.lock().unwrap();
            if !device.is_empty() {
                let has_ps = self.plugin.lock().unwrap().capabilities().has_power_supply;
                if !has_ps {
                    self.ps_confirmed_non_ps = true;
                }
            }
        }

        // Handle graph and histogram updates
        let current_time = ctx.input(|i| i.time);
        let graph_interval = *self.graph_update_interval_shared.lock().unwrap() as f64 / 1000.0;
        if !self.hold_enabled && current_time - self.last_graph_update >= graph_interval {
            if !self.curr_meas.is_nan() {
                self.values.push_back(self.curr_meas);
                self.update_histogram(self.curr_meas);
                while self.values.len() > self.mem_depth {
                    self.values.pop_front();
                }
                if self.recording_active
                    && matches!(self.recording_mode, super::RecordingMode::FixedInterval)
                    && current_time - self.last_record_time >= self.recording_interval_ms as f64 / 1000.0
                {
                    self.record_measurement();
                    self.last_record_time = current_time;
                }
            }
            self.last_graph_update = current_time;
        }

        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Settings").clicked() {
                        self.settings_open = true;
                    }
                    if !is_web && ui.button("Quit").clicked() {
                        self.disconnect();
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
                ui.add_space(16.0);
                egui::widgets::global_theme_preference_buttons(ui);
            });
        });

        egui::TopBottomPanel::bottom("bottom_panel").show(ctx, |ui| {
            ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                powered_by(ui);
                ui.hyperlink_to(
                    format!("Version: v{}", super::VERSION),
                    "https://github.com/bbbkada/rusty-meter-spm/releases/latest",
                );
                egui::warn_if_debug_build(ui);
            });
        });

        // Power Supply right side panel — hidden only when device confirmed to have no PS
        let show_ps_panel = !self.ps_confirmed_non_ps;
        if show_ps_panel {
            let is_connected = self.connection_state == super::ConnectionState::Connected;
            egui::SidePanel::right("power_supply_panel")
                .resizable(true)
                .default_width(260.0)
                .min_width(220.0)
                .max_width(320.0)
                .show(ctx, |ui| {
                    self.show_power_supply_panel(ui, is_connected);
                });
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            if is_web {
                ui.heading("RustyMeter");
            }

            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label("Serial port: ");
                    ui.add(
                        DropDownBox::from_iter(
                            &self.portlist,
                            "portlistbox",
                            &mut self.serial_port,
                            |ui, text| ui.selectable_label(false, text),
                        )
                        .desired_width(150.0)
                        .select_on_focus(true)
                        .filter_by_input(false),
                    );

                    match self.connection_state {
                        super::ConnectionState::Disconnected => {
                            if ui.button("Connect").clicked() {
                                self.connection_state = super::ConnectionState::Connecting;
                                self.connection_error = None;
                                match mio_serial::new(&self.serial_port, self.baud_rate)
                                    .open_native_async()
                                {
                                    Ok(serial) => {
                                        self.serial = Some(serial);
                                        if let Some(ref mut serial) = self.serial {
                                            let _ = serial.set_data_bits(DataBits::Eight);
                                            let _ = serial.set_stop_bits(mio_serial::StopBits::One);
                                            let _ = serial.set_parity(mio_serial::Parity::None);
                                            self.connection_state = super::ConnectionState::Connected;
                                            self.spawn_serial_task();
                                            self.spawn_graph_update_task(ctx.clone());
                                        }
                                    }
                                    Err(e) => {
                                        self.connection_state = super::ConnectionState::Disconnected;
                                        self.connection_error = Some(format!("Failed to connect: {}", e));
                                    }
                                }
                            }
                        }
                        super::ConnectionState::Connecting => {
                            ui.label("Connecting...");
                        }
                        super::ConnectionState::Connected => {
                            if ui.button("Disconnect").clicked() {
                                self.disconnect();
                            }
                        }
                    }

                    if ui.button("Start Recording").clicked() {
                        self.recording_open = true;
                    }

                    if ui.checkbox(&mut self.always_on_top, "Always on top").changed() {
                        let level = if self.always_on_top {
                            egui::viewport::WindowLevel::AlwaysOnTop
                        } else {
                            egui::viewport::WindowLevel::Normal
                        };
                        ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(level));
                    }
                });

                ui.horizontal(|ui| {
                    let device = self.device.lock().unwrap();
                    match self.connection_state {
                        super::ConnectionState::Disconnected => {
                            if let Some(ref error) = self.connection_error {
                                ui.label(egui::RichText::new(error).color(egui::Color32::RED));
                            } else {
                                ui.label("Not connected.");
                            }
                        }
                        super::ConnectionState::Connecting => {
                            ui.label("Attempting to connect...");
                        }
                        super::ConnectionState::Connected => {
                            if !device.is_empty() {
                                ui.label("Connected to: ");
                                ui.label(&*device);
                            } else {
                                ui.label("Connected, awaiting device ID...");
                            }
                        }
                    }
                });
            });

            ui.separator();

            ui.horizontal(|ui| {
                // Measurement display
                let is_below_threshold = match self.metermode {
                    MeterMode::Cont => self.values.back().is_some_and(|&val| val <= self.cont_threshold as f64),
                    MeterMode::Diod => self.values.back().is_some_and(|&val| val <= self.diod_threshold as f64),
                    _ => false,
                };
                let background_color = if is_below_threshold {
                    egui::Color32::from_rgb(139, 0, 0)
                } else {
                    self.box_background_color
                };

                let meter_frame = egui::Frame {
                    inner_margin: 12.0.into(),
                    outer_margin: 24.0.into(),
                    corner_radius: 5.0.into(),
                    shadow: epaint::Shadow {
                        offset: [8, 12],
                        blur: 16,
                        spread: 0,
                        color: egui::Color32::from_black_alpha(180),
                    },
                    fill: background_color,
                    stroke: egui::Stroke::new(1.0, egui::Color32::GRAY),
                };
                meter_frame.show(ui, |ui| {
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                    ui.allocate_ui_with_layout(
                        Vec2 { x: 400.0, y: 300.0 },
                        egui::Layout::top_down(egui::Align::RIGHT).with_cross_justify(false),
                        |ui| {
                            let (formatted_value, display_unit) = format_measurement(
                                self.curr_meas, 10, 1_000_000.0, 0.0001,
                                &self.metermode, Some(self.curr_precision),
                            );
                            ui.label(
                                egui::RichText::new(formatted_value)
                                    .color(self.measurement_font_color)
                                    .font(FontId { size: 60.0, family: FontFamily::Name("B612Mono-Bold".into()) }),
                            );
                            ui.label(
                                egui::RichText::new(format!("{:>10}", display_unit))
                                    .color(self.measurement_font_color)
                                    .font(FontId { size: 20.0, family: FontFamily::Name("B612Mono-Bold".into()) }),
                            );
                        },
                    );
                });

                // ── Dynamic Mode Buttons ──
                let control_frame = egui::Frame {
                    inner_margin: 12.0.into(),
                    outer_margin: 24.0.into(),
                    corner_radius: 5.0.into(),
                    shadow: epaint::Shadow {
                        offset: [8, 12], blur: 16, spread: 0,
                        color: egui::Color32::from_black_alpha(180),
                    },
                    fill: self.box_background_color,
                    stroke: egui::Stroke::new(1.0, egui::Color32::GRAY),
                };

                control_frame.show(ui, |ui| {
                    ui.vertical(|ui| {
                        let btn_size = Vec2 { x: 70.0, y: 20.0 };

                        // Get supported modes from plugin
                        let caps = self.plugin.lock().unwrap().capabilities().clone();
                        let supported = caps.supported_modes.clone();

                        // Collect labels for each supported mode
                        let mode_labels: Vec<(MeterMode, String)> = {
                            let plugin = self.plugin.lock().unwrap();
                            supported.iter().map(|&m| (m, plugin.mode_label(m).to_string())).collect()
                        };

                        // Lay out mode buttons in rows of 4
                        let mut clicked_mode: Option<MeterMode> = None;
                        for chunk in mode_labels.chunks(4) {
                            ui.horizontal(|ui| {
                                for (mode, label) in chunk {
                                    let btn = egui::Button::new(label.as_str())
                                        .selected(self.metermode == *mode)
                                        .min_size(btn_size);
                                    if ui.add(btn).clicked() {
                                        clicked_mode = Some(*mode);
                                    }
                                }
                            });
                        }

                        // Hold button (always in last row if device supports it, standalone otherwise)
                        ui.horizontal(|ui| {
                            if caps.has_hold {
                                let hold_btn = egui::Button::new("Hold")
                                    .selected(self.hold_enabled)
                                    .min_size(btn_size);
                                if ui.add(hold_btn).clicked() {
                                    self.hold_enabled = !self.hold_enabled;
                                    *self.hold_enabled_shared.lock().unwrap() = self.hold_enabled;
                                    if !self.hold_enabled {
                                        self.last_graph_update = 0.0;
                                    }
                                    let plugin = self.plugin.lock().unwrap();
                                    let cmd = plugin.hold_command(self.hold_enabled);
                                    drop(plugin);
                                    if let Some(cmd) = cmd {
                                        if let Some(tx) = self.serial_tx.clone() {
                                            let value_debug = self.value_debug;
                                            tokio::spawn(async move {
                                                if let Err(e) = tx.send(cmd).await {
                                                    if value_debug { println!("Failed to queue hold command: {}", e); }
                                                }
                                            });
                                        }
                                    }
                                }
                            }
                        });

                        // Handle mode button click (after layout to avoid borrow issues)
                        if let Some(mode) = clicked_mode {
                            self.set_mode(mode);
                        }
                    });
                });

                // ── Options Panel (rate, range, beeper, threshold) ──
                let options_frame = egui::Frame {
                    inner_margin: 12.0.into(),
                    outer_margin: 24.0.into(),
                    corner_radius: 5.0.into(),
                    shadow: epaint::Shadow {
                        offset: [8, 12], blur: 16, spread: 0,
                        color: egui::Color32::from_black_alpha(180),
                    },
                    fill: self.box_background_color,
                    stroke: egui::Stroke::new(1.0, egui::Color32::GRAY),
                };
                options_frame.show(ui, |ui| {
                    ui.vertical(|ui| {
                        let caps = self.plugin.lock().unwrap().capabilities().clone();

                        // Sampling rate control — only if device supports it
                        if !caps.rate_options.is_empty() {
                            let rate_options = caps.rate_options.clone();
                            let ratebox = egui::ComboBox::from_label("Sampling Rate").show_index(
                                ui,
                                &mut self.curr_rate,
                                rate_options.len(),
                                |i| rate_options.get(i).copied().unwrap_or("?"),
                            );
                            if ratebox.changed() {
                                let plugin = self.plugin.lock().unwrap();
                                if let Some(cmd) = plugin.rate_command(self.curr_rate) {
                                    self.confstring = cmd.clone();
                                    drop(plugin);
                                    if let Some(tx) = self.serial_tx.clone() {
                                        let cmd = self.confstring.clone();
                                        let value_debug = self.value_debug;
                                        tokio::spawn(async move {
                                            if let Err(e) = tx.send(cmd).await {
                                                if value_debug { println!("Failed to queue rate command: {}", e); }
                                            }
                                        });
                                    }
                                }
                            }
                        }

                        // Range control — from plugin
                        {
                            let plugin = self.plugin.lock().unwrap();
                            if let Some(range_info) = plugin.range_info(self.metermode) {
                                if self.curr_range >= range_info.ranges.len() {
                                    self.curr_range = 0;
                                }
                                let current_text = range_info.ranges.get(self.curr_range)
                                    .copied().unwrap_or("auto");
                                let range_enabled = range_info.ranges.len() > 1;
                                let ranges_clone = range_info.ranges.clone();
                                drop(plugin); // Release lock before UI

                                let mut changed = false;
                                let combo_id = format!("range_combo_{:?}", self.metermode);
                                ui.add_enabled_ui(range_enabled, |ui| {
                                    egui::ComboBox::from_id_salt(combo_id)
                                        .selected_text(current_text)
                                        .show_ui(ui, |ui| {
                                            for (idx, name) in ranges_clone.iter().enumerate() {
                                                if ui.selectable_value(&mut self.curr_range, idx, *name).changed() {
                                                    changed = true;
                                                }
                                            }
                                        });
                                });

                                if changed {
                                    let plugin = self.plugin.lock().unwrap();
                                    if let Some(cmd) = plugin.range_command(self.metermode, self.curr_range) {
                                        drop(plugin);
                                        self.pending_changes.lock().unwrap().range = Some((self.curr_range, cmd.clone()));
                                        // Also send immediately via serial channel
                                        if let Some(tx) = self.serial_tx.clone() {
                                            let value_debug = self.value_debug;
                                            tokio::spawn(async move {
                                                for line in cmd.lines() {
                                                    let line = line.trim();
                                                    if !line.is_empty() {
                                                        if let Err(e) = tx.send(format!("{}\n", line)).await {
                                                            if value_debug { println!("Failed to queue range command: {}", e); }
                                                        }
                                                    }
                                                }
                                            });
                                        }
                                    }
                                }
                            }
                        }

                        // Beeper and threshold controls for CONT/DIOD
                        if self.metermode == MeterMode::Cont || self.metermode == MeterMode::Diod {
                            // Beeper
                            if caps.has_beeper {
                                let mut beeper = self.beeper_enabled;
                                if ui.checkbox(&mut beeper, "Beeper").changed() {
                                    self.beeper_enabled = beeper;
                                    let plugin = self.plugin.lock().unwrap();
                                    if let Some(cmd) = plugin.beeper_command(beeper) {
                                        drop(plugin);
                                        if let Some(tx) = self.serial_tx.clone() {
                                            let value_debug = self.value_debug;
                                            tokio::spawn(async move {
                                                if let Err(e) = tx.send(cmd).await {
                                                    if value_debug { println!("Failed to queue beeper command: {}", e); }
                                                }
                                            });
                                        }
                                    }
                                }
                            }

                            // Threshold
                            if self.metermode == MeterMode::Cont {
                                let threshold_slider = ui.add_enabled(
                                    caps.has_threshold,
                                    egui::Slider::new(&mut self.cont_threshold, 0..=1000)
                                        .text("Threshold (Ω)")
                                        .step_by(1.0)
                                        .clamping(SliderClamping::Always),
                                );
                                if caps.has_threshold && (threshold_slider.drag_stopped() || threshold_slider.lost_focus()) {
                                    let plugin = self.plugin.lock().unwrap();
                                    if let Some(cmd) = plugin.cont_threshold_command(self.cont_threshold) {
                                        drop(plugin);
                                        if let Some(tx) = self.serial_tx.clone() {
                                            let value_debug = self.value_debug;
                                            tokio::spawn(async move {
                                                if let Err(e) = tx.send(cmd).await {
                                                    if value_debug { println!("Failed to queue threshold command: {}", e); }
                                                }
                                            });
                                        }
                                    }
                                }
                            } else if self.metermode == MeterMode::Diod {
                                let threshold_slider = ui.add_enabled(
                                    caps.has_threshold,
                                    egui::Slider::new(&mut self.diod_threshold, 0.0..=3.0)
                                        .text("Threshold (V)")
                                        .step_by(0.1)
                                        .clamping(SliderClamping::Always),
                                );
                                if caps.has_threshold && (threshold_slider.drag_stopped() || threshold_slider.lost_focus()) {
                                    let plugin = self.plugin.lock().unwrap();
                                    if let Some(cmd) = plugin.diod_threshold_command(self.diod_threshold) {
                                        drop(plugin);
                                        if let Some(tx) = self.serial_tx.clone() {
                                            let value_debug = self.value_debug;
                                            tokio::spawn(async move {
                                                if let Err(e) = tx.send(cmd).await {
                                                    if value_debug { println!("Failed to queue threshold command: {}", e); }
                                                }
                                            });
                                        }
                                    }
                                }
                            }
                        }
                    });
                });
            });

            ui.separator();

            // Dock area for graph and histogram
            {
                let dock_state = &mut self.plot_dock_state;
                let mut viewer = PlotTabViewer {
                    values: &self.values,
                    hist_values: &mut self.hist_values,
                    reverse_graph: &mut self.reverse_graph,
                    graph_line_color: self.graph_line_color,
                    hist_bar_color: self.hist_bar_color,
                    mem_depth: &mut self.mem_depth,
                    curr_meas: self.curr_meas,
                    metermode: self.metermode,
                    graph_config: &mut self.graph_config,
                    hist_collect_active: &mut self.hist_collect_active,
                    hist_collect_interval_ms: &mut self.hist_collect_interval_ms,
                    hist_mem_depth: &mut self.hist_mem_depth,
                    mem_depth_max: self.mem_depth_max,
                    graph_update_interval_ms: &mut self.graph_update_interval_ms,
                    graph_update_interval_max: self.graph_update_interval_max,
                    hist_mem_depth_max: self.hist_mem_depth_max,
                    curr_unit: &self.curr_unit,
                };
                DockArea::new(dock_state)
                    .style(Style::from_egui(ui.style()))
                    .show_close_buttons(false)
                    .show_inside(ui, &mut viewer);
            }

            self.show_settings(ctx);
            self.show_recording_window(ctx);
        });
    }
}

// ── Power Supply Panel ──────────────────────────────────────────

impl super::MyApp {
    fn show_power_supply_panel(&mut self, ui: &mut egui::Ui, is_connected: bool) {
        ui.vertical(|ui| {
            ui.heading("Power Supply");
            ui.separator();

            if !is_connected {
                ui.label(egui::RichText::new("Not connected").italics().color(egui::Color32::GRAY));
                ui.add_space(8.0);
            }

            // Output ON/OFF toggle
            let output_color = if self.ps_output_on {
                egui::Color32::from_rgb(0, 200, 0)
            } else {
                egui::Color32::from_rgb(180, 0, 0)
            };
            let output_text = if self.ps_output_on { "OUTPUT ON" } else { "OUTPUT OFF" };
            let output_btn = egui::Button::new(
                egui::RichText::new(output_text).size(18.0).strong().color(egui::Color32::WHITE),
            ).fill(output_color);

            let btn_response = ui.add_enabled_ui(is_connected, |ui| {
                ui.add_sized([ui.available_width(), 40.0], output_btn)
            }).inner;
            if btn_response.clicked() {
                let new_state = !self.ps_output_on;
                self.ps_output_on = new_state;
                self.pending_changes.lock().unwrap().output_on = Some(new_state);
            }

            ui.add_space(8.0);

            // Output readback display
            let readback_frame = egui::Frame {
                inner_margin: 10.0.into(),
                corner_radius: 4.0.into(),
                fill: self.box_background_color,
                stroke: egui::Stroke::new(1.0, egui::Color32::GRAY),
                ..Default::default()
            };
            readback_frame.show(ui, |ui| {
                let cyan = egui::Color32::from_rgb(0, 255, 255);
                let mono = egui::FontFamily::Name("B612Mono-Bold".into());
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(format!("{:.3}", self.ps_voltage_readback))
                        .size(22.0).color(cyan).family(mono.clone()));
                    ui.label(egui::RichText::new("V").size(12.0).color(cyan).family(mono.clone()));
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new(format!("{:.3}", self.ps_current_readback))
                        .size(22.0).color(cyan).family(mono.clone()));
                    ui.label(egui::RichText::new("A").size(12.0).color(cyan).family(mono.clone()));
                });
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(format!("{:.3}", self.ps_power_readback))
                        .size(14.0).color(egui::Color32::from_rgb(180, 220, 255)).family(mono.clone()));
                    ui.label(egui::RichText::new("W")
                        .size(10.0).color(egui::Color32::from_rgb(180, 220, 255)).family(mono));
                });
            });

            ui.add_space(8.0);
            ui.separator();

            let mut any_ps_focus = false;

            // Voltage setting
            ui.label(egui::RichText::new("Voltage (V)").strong());
            ui.horizontal(|ui| {
                let response = ui.add_enabled(is_connected,
                    egui::TextEdit::singleline(&mut self.ps_voltage_input).desired_width(100.0).hint_text("0.000"));
                if response.has_focus() { any_ps_focus = true; }
                if ui.add_enabled(is_connected, egui::Button::new("+").min_size(egui::Vec2::new(24.0, 20.0))).clicked() {
                    if let Ok(v) = self.ps_voltage_input.parse::<f64>() {
                        let new_v = ((v * 10.0).floor() / 10.0 + 0.1).min(60.0);
                        self.ps_voltage_input = format!("{:.3}", new_v);
                        self.ps_voltage_set = new_v;
                        self.pending_changes.lock().unwrap().voltage_set = Some(new_v);
                    }
                }
                if ui.add_enabled(is_connected, egui::Button::new("-").min_size(egui::Vec2::new(24.0, 20.0))).clicked() {
                    if let Ok(v) = self.ps_voltage_input.parse::<f64>() {
                        let new_v = ((v * 10.0).ceil() / 10.0 - 0.1).max(0.0);
                        self.ps_voltage_input = format!("{:.3}", new_v);
                        self.ps_voltage_set = new_v;
                        self.pending_changes.lock().unwrap().voltage_set = Some(new_v);
                    }
                }
                if ui.add_enabled(is_connected, egui::Button::new("Set")).clicked()
                    || (response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                {
                    if let Ok(v) = self.ps_voltage_input.parse::<f64>() {
                        self.ps_voltage_set = v;
                        self.pending_changes.lock().unwrap().voltage_set = Some(v);
                    }
                }
            });

            ui.add_space(4.0);

            // Current setting
            ui.label(egui::RichText::new("Current (A)").strong());
            ui.horizontal(|ui| {
                let response = ui.add_enabled(is_connected,
                    egui::TextEdit::singleline(&mut self.ps_current_input).desired_width(100.0).hint_text("0.000"));
                if response.has_focus() { any_ps_focus = true; }
                if ui.add_enabled(is_connected, egui::Button::new("+").min_size(egui::Vec2::new(24.0, 20.0))).clicked() {
                    if let Ok(v) = self.ps_current_input.parse::<f64>() {
                        let new_v = ((v * 10.0).floor() / 10.0 + 0.1).min(3.2);
                        self.ps_current_input = format!("{:.3}", new_v);
                        self.ps_current_set = new_v;
                        self.pending_changes.lock().unwrap().current_set = Some(new_v);
                    }
                }
                if ui.add_enabled(is_connected, egui::Button::new("-").min_size(egui::Vec2::new(24.0, 20.0))).clicked() {
                    if let Ok(v) = self.ps_current_input.parse::<f64>() {
                        let new_v = ((v * 10.0).ceil() / 10.0 - 0.1).max(0.0);
                        self.ps_current_input = format!("{:.3}", new_v);
                        self.ps_current_set = new_v;
                        self.pending_changes.lock().unwrap().current_set = Some(new_v);
                    }
                }
                if ui.add_enabled(is_connected, egui::Button::new("Set")).clicked()
                    || (response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                {
                    if let Ok(v) = self.ps_current_input.parse::<f64>() {
                        self.ps_current_set = v;
                        self.pending_changes.lock().unwrap().current_set = Some(v);
                    }
                }
            });

            ui.add_space(8.0);
            ui.separator();
            ui.label(egui::RichText::new("Protection").strong());
            ui.add_space(4.0);

            // OVP
            ui.label("OVP (V)");
            ui.horizontal(|ui| {
                let response = ui.add_enabled(is_connected,
                    egui::TextEdit::singleline(&mut self.ps_ovp_input).desired_width(100.0).hint_text("0.000"));
                if response.has_focus() { any_ps_focus = true; }
                if ui.add_enabled(is_connected, egui::Button::new("+").min_size(egui::Vec2::new(24.0, 20.0))).clicked() {
                    if let Ok(v) = self.ps_ovp_input.parse::<f64>() {
                        let new_v = ((v * 10.0).floor() / 10.0 + 0.1).min(63.0);
                        self.ps_ovp_input = format!("{:.3}", new_v);
                        self.ps_ovp = new_v;
                        self.pending_changes.lock().unwrap().ovp = Some(new_v);
                    }
                }
                if ui.add_enabled(is_connected, egui::Button::new("-").min_size(egui::Vec2::new(24.0, 20.0))).clicked() {
                    if let Ok(v) = self.ps_ovp_input.parse::<f64>() {
                        let new_v = ((v * 10.0).ceil() / 10.0 - 0.1).max(0.0);
                        self.ps_ovp_input = format!("{:.3}", new_v);
                        self.ps_ovp = new_v;
                        self.pending_changes.lock().unwrap().ovp = Some(new_v);
                    }
                }
                if ui.add_enabled(is_connected, egui::Button::new("Set")).clicked()
                    || (response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                {
                    if let Ok(v) = self.ps_ovp_input.parse::<f64>() {
                        self.ps_ovp = v;
                        self.pending_changes.lock().unwrap().ovp = Some(v);
                    }
                }
            });

            ui.add_space(4.0);

            // OCP
            ui.label("OCP (A)");
            ui.horizontal(|ui| {
                let response = ui.add_enabled(is_connected,
                    egui::TextEdit::singleline(&mut self.ps_ocp_input).desired_width(100.0).hint_text("0.000"));
                if response.has_focus() { any_ps_focus = true; }
                if ui.add_enabled(is_connected, egui::Button::new("+").min_size(egui::Vec2::new(24.0, 20.0))).clicked() {
                    if let Ok(v) = self.ps_ocp_input.parse::<f64>() {
                        let new_v = ((v * 10.0).floor() / 10.0 + 0.1).min(3.5);
                        self.ps_ocp_input = format!("{:.3}", new_v);
                        self.ps_ocp = new_v;
                        self.pending_changes.lock().unwrap().ocp = Some(new_v);
                    }
                }
                if ui.add_enabled(is_connected, egui::Button::new("-").min_size(egui::Vec2::new(24.0, 20.0))).clicked() {
                    if let Ok(v) = self.ps_ocp_input.parse::<f64>() {
                        let new_v = ((v * 10.0).ceil() / 10.0 - 0.1).max(0.0);
                        self.ps_ocp_input = format!("{:.3}", new_v);
                        self.ps_ocp = new_v;
                        self.pending_changes.lock().unwrap().ocp = Some(new_v);
                    }
                }
                if ui.add_enabled(is_connected, egui::Button::new("Set")).clicked()
                    || (response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                {
                    if let Ok(v) = self.ps_ocp_input.parse::<f64>() {
                        self.ps_ocp = v;
                        self.pending_changes.lock().unwrap().ocp = Some(v);
                    }
                }
            });

            self.ps_input_has_focus = any_ps_focus;
        });
    }
}

impl eframe::App for super::MyApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.save(storage);
    }

    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.update(ctx, frame);
    }
}
