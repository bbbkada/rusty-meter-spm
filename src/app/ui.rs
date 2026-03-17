use egui::{FontFamily, FontId, SliderClamping, Vec2};
use egui_dock::{DockArea, DockState, Style, TabViewer};
use egui_dropdown::DropDownBox;
use mio_serial::{DataBits, SerialPort, SerialPortBuilderExt};
use std::collections::VecDeque;

use crate::helpers::{format_measurement, powered_by};
use crate::multimeter::{GenScpi, MeterMode};

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
        // Save recording data if recording is active
        if self.recording_active {
            self.save_recording_data();
        }
        eframe::set_value(storage, eframe::APP_KEY, self);
    }

    /// Called each time the UI needs repainting, which may be many times per second.
    pub fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let is_web = cfg!(target_arch = "wasm32");
        
        // Handle spacebar to toggle hold
        if ctx.input(|i| i.key_pressed(egui::Key::Space)) {
            self.hold_enabled = !self.hold_enabled;
            *self.hold_enabled_shared.lock().unwrap() = self.hold_enabled;
            if self.value_debug { println!("Hold toggled via spacebar: {}", self.hold_enabled); }
            // Reset last_graph_update when resuming to allow immediate update
            if !self.hold_enabled {
                self.last_graph_update = 0.0;
            }
            // Only send hardware hold command if device supports it
            let supports_hold = self.device_type.lock().unwrap().plugin().supports_hold();
            if self.value_debug { println!("Device supports hold: {}", supports_hold); }
            if supports_hold {
                if let Some(tx) = self.serial_tx.clone() {
                    let cmd = if self.hold_enabled {
                        "MULT:HOLD ON\n".to_string()
                    } else {
                        "MULT:HOLD OFF\n".to_string()
                    };
                    if self.value_debug { println!("Sending hold command: {:?}", cmd); }
                    let value_debug = self.value_debug;
                    tokio::spawn(async move {
                        if let Err(e) = tx.send(cmd).await {
                            if value_debug { println!("Failed to queue hold command: {}", e); }
                        }
                    });
                } else {
                    if self.value_debug { println!("serial_tx is None, cannot send hold command"); }
                }
            }
        }

        // On startup, handle certain items once
        if !self.is_init {
            if let Ok(ports) = mio_serial::available_ports() {
                for p in ports {
                    self.portlist.push_front(p.port_name);
                }
            }
            // Apply initial sampling rate
            self.confstring = self
                .ratecmd
                .gen_scpi(self.ratecmd.get_opt(self.curr_rate).0);
            if let Some(tx) = self.serial_tx.clone() {
                let cmd = self.confstring.clone();
                let value_debug = self.value_debug;
                tokio::spawn(async move {
                    if let Err(e) = tx.send(cmd).await {
                        if value_debug {
                            println!("Failed to queue initial rate command: {}", e);
                        }
                    }
                });
            }
            // Initialize dock state
            let tabs = vec![PlotTab::Graph, PlotTab::Histogram];
            self.plot_dock_state = DockState::new(tabs);
            
            // Auto-connect if enabled and serial port is configured
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
                        self.connection_error =
                            Some(format!("Failed to auto-connect: {}", e));
                    }
                }
            }
            
            self.is_init = true;
        }

        // Process all available measurements
        if let Some(ref mut rx) = self.serial_rx {
            while let Ok(meas_opt) = rx.try_recv() {
                if let Some((meas, decimals)) = meas_opt {
                    if self.value_debug {
                        println!("UI received measurement: {} ({}dp)", meas, decimals);
                    }
                    self.curr_meas = meas;
                    self.curr_decimals = decimals;
                }
            }
        }

        // Process all available mode updates
        if let Some(ref mut rx) = self.mode_rx {
            while let Ok(mode) = rx.try_recv() {
                if mode != self.metermode {
                    self.metermode = mode;
                    self.values = VecDeque::with_capacity(self.mem_depth);
                    self.hist_values = VecDeque::with_capacity(self.hist_mem_depth); // Reset histogram buffer
                    match mode {
                        MeterMode::Vdc => {
                            self.curr_unit = "VDC".to_owned();
                        }
                        MeterMode::Vac => {
                            self.curr_unit = "VAC".to_owned();
                        }
                        MeterMode::Adc => {
                            self.curr_unit = "ADC".to_owned();
                        }
                        MeterMode::Aac => {
                            self.curr_unit = "AAC".to_owned();
                        }
                        MeterMode::Res => {
                            self.curr_unit = "Ohm".to_owned();
                        }
                        MeterMode::Cap => {
                            self.curr_unit = "F".to_owned();
                        }
                        MeterMode::Freq => {
                            self.curr_unit = "Hz".to_owned();
                        }
                        MeterMode::Per => {
                            self.curr_unit = "s".to_owned();
                        }
                        MeterMode::Diod => {
                            self.curr_unit = "V".to_owned();
                        }
                        MeterMode::Cont => {
                            self.curr_unit = "Ohm".to_owned();
                        }
                        MeterMode::Temp => {
                            self.curr_unit = "°C".to_owned();
                        }
                    }
                    
                    if self.value_debug {
                        println!("Updated metermode to: {:?}", mode);
                    }
                    
                    // Reset curr_range to 0 (AUTO) for the new mode
                    self.curr_range = 0;
                    
                    // Clear pending mode change flag - mode is now confirmed by instrument
                    self.pending_mode_change = None;
                    
                    // Clear pending mode from PendingChanges so serial stops re-sending
                    {
                        let mut pending = self.pending_changes.lock().unwrap();
                        if let Some((target, _, _)) = &pending.mode {
                            if *target == mode {
                                pending.mode = None;
                            }
                        }
                    }
                    
                    // Range index updates from instrument will now be accepted from the new mode
                }
            }
        }
        
        // Handle range updates from serial task (from instrument)
        // Always update curr_range to match what instrument reports
        if let Some(rx) = &mut self.range_rx {
            while let Ok((mode, range_idx)) = rx.try_recv() {
                // Ignore range updates during mode transitions to prevent old mode's range leaking to new mode
                if self.pending_mode_change.is_some() {
                    if self.value_debug {
                        println!("UI: Ignoring range update {} for mode {:?} during mode transition to {:?}", range_idx, mode, self.pending_mode_change);
                    }
                    continue;
                }
                
                // Ignore range updates from old mode (stale data)
                if mode != self.metermode {
                    if self.value_debug {
                        println!("UI: Ignoring stale range update {} for old mode {:?} (current mode: {:?})", range_idx, mode, self.metermode);
                    }
                    continue;
                }
                
                if self.value_debug {
                    println!("UI: Received range index update: {} for mode {:?}", range_idx, mode);
                }
                
                // Skip range updates while user has a pending range change
                {
                    let mut pending = self.pending_changes.lock().unwrap();
                    if let Some((expected_idx, _)) = &pending.range {
                        if *expected_idx == 0 {
                            // Auto range: instrument will never report index 0,
                            // it reports the auto-selected range. Accept any readback
                            // and clear pending.
                            pending.range = None;
                            self.curr_range = 0; // Show "auto" in UI
                        } else if range_idx == *expected_idx {
                            // Fixed range: instrument confirmed expected range
                            pending.range = None;
                            self.curr_range = range_idx;
                        } else if self.value_debug {
                            println!("UI: Ignoring range update {} (pending change to {})", range_idx, expected_idx);
                        }
                        continue;
                    }
                }
                self.curr_range = range_idx;
                
                if self.value_debug {
                    println!("Updated curr_range to {} from instrument", range_idx);
                }
            }
        }
        
        // Handle power supply state updates from serial task
        if let Some(rx) = &mut self.ps_rx {
            while let Ok(ps_state) = rx.try_recv() {
                // Lock pending_changes MUTABLY — we'll clear fields when readback confirms them.
                let mut pending = self.pending_changes.lock().unwrap();
                
                // Output ON/OFF: accept only if no pending, or if readback confirms expected state
                if let Some(expected) = pending.output_on {
                    if ps_state.output_on == expected {
                        // Instrument confirmed the expected state — clear pending, accept
                        pending.output_on = None;
                        self.ps_output_on = ps_state.output_on;
                    }
                    // Otherwise keep pending set, keep UI showing what user clicked
                } else {
                    self.ps_output_on = ps_state.output_on;
                }
                
                // Always accept readback (actual measurements)
                self.ps_voltage_readback = ps_state.voltage_readback;
                self.ps_current_readback = ps_state.current_readback;
                self.ps_power_readback = ps_state.power_readback;
                
                // Only update set values from full polls (fast polls carry stale set values)
                if ps_state.includes_settings {
                    // For each field: if pending matches readback (within tolerance), clear and accept.
                    // If pending is set but doesn't match, skip readback (user's value wins).
                    // If no pending, accept readback.
                    // ALSO skip text buffer writes if user is actively editing (has focus from last frame).
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
                    if self.value_debug {
                        println!("PS sync: V={:.3} A={:.3} OVP={:.3} OCP={:.3} OUT={}",
                            ps_state.voltage_set, ps_state.current_set,
                            ps_state.ovp, ps_state.ocp, ps_state.output_on);
                    }
                }
            }
        }
        
        // Detect if connected device is confirmed non-SPM (has connected and device detected)
        if self.connection_state == super::ConnectionState::Connected {
            let device = self.device.lock().unwrap();
            if !device.is_empty() {
                let has_ps = self.device_type.lock().unwrap().plugin().supports_power_supply();
                if !has_ps {
                    self.ps_confirmed_non_spm = true;
                }
            }
        }

        // Handle graph and histogram updates and recording based on the configured interval
        // Skip updates when hold is enabled to freeze the graph
        let current_time = ctx.input(|i| i.time); // Get current time in seconds
        let graph_interval = *self.graph_update_interval_shared.lock().unwrap() as f64 / 1000.0; // Convert ms to seconds
        if !self.hold_enabled && current_time - self.last_graph_update >= graph_interval {
            if !self.curr_meas.is_nan() {
                self.values.push_back(self.curr_meas);
                self.update_histogram(self.curr_meas); // Update histogram with new measurement
                while self.values.len() > self.mem_depth {
                    self.values.pop_front();
                }
                // Record measurement for fixed interval mode
                if self.recording_active
                    && matches!(self.recording_mode, super::RecordingMode::FixedInterval)
                    && current_time - self.last_record_time
                        >= self.recording_interval_ms as f64 / 1000.0
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
                        self.disconnect(); // Use disconnect method instead of partial cleanup
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

        // Power Supply right side panel
        // Visible by default. Hidden only when connected to a confirmed non-SPM device.
        let show_ps_panel = !self.ps_confirmed_non_spm;
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
                                            self.connection_state =
                                                super::ConnectionState::Connected;
                                            self.spawn_serial_task();
                                            self.spawn_graph_update_task(ctx.clone());
                                        }
                                    }
                                    Err(e) => {
                                        self.connection_state =
                                            super::ConnectionState::Disconnected;
                                        self.connection_error =
                                            Some(format!("Failed to connect: {}", e));
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

                    // Recording button
                    if ui.button("Start Recording").clicked() {
                        self.recording_open = true;
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
                // Determine if the background and shadow should be dark red based on mode and threshold
                let is_below_threshold = match self.metermode {
                    MeterMode::Cont => self
                        .values
                        .back()
                        .is_some_and(|&val| val <= self.cont_threshold as f64),
                    MeterMode::Diod => self
                        .values
                        .back()
                        .is_some_and(|&val| val <= self.diod_threshold as f64),
                    _ => false,
                };
                let background_color = if is_below_threshold {
                    egui::Color32::from_rgb(139, 0, 0) // Dark red for threshold condition
                } else {
                    self.box_background_color // Use custom background color
                };
                let shadow_color = if is_below_threshold {
                    // don't do this for now egui::Color32::from_rgba_unmultiplied(139, 0, 0, 180) // Dark red shadow with alpha
                    egui::Color32::from_black_alpha(180) // Default black shadow
                } else {
                    egui::Color32::from_black_alpha(180) // Default black shadow
                };

                let meter_frame = egui::Frame {
                    inner_margin: 12.0.into(),
                    outer_margin: 24.0.into(),
                    corner_radius: 5.0.into(),
                    shadow: epaint::Shadow {
                        offset: [8, 12],
                        blur: 16,
                        spread: 0,
                        color: shadow_color,
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
                                self.curr_meas,
                                10,
                                1_000_000.0,
                                0.0001,
                                &self.metermode,
                                Some(self.curr_decimals),
                            );
                            ui.label(
                                egui::RichText::new(formatted_value)
                                    .color(self.measurement_font_color)
                                    .font(FontId {
                                        size: 60.0,
                                        family: FontFamily::Name("B612Mono-Bold".into()),
                                    }),
                            );
                            ui.label(
                                egui::RichText::new(format!("{:>10}", display_unit))
                                    .color(self.measurement_font_color)
                                    .font(FontId {
                                        size: 20.0,
                                        family: FontFamily::Name("B612Mono-Bold".into()),
                                    }),
                            );
                        },
                    );
                });

                let control_frame = egui::Frame {
                    inner_margin: 12.0.into(),
                    outer_margin: 24.0.into(),
                    corner_radius: 5.0.into(),
                    shadow: epaint::Shadow {
                        offset: [8, 12],
                        blur: 16,
                        spread: 0,
                        color: egui::Color32::from_black_alpha(180),
                    },
                    fill: self.box_background_color,
                    stroke: egui::Stroke::new(1.0, egui::Color32::GRAY),
                };
                
                control_frame.show(ui, |ui| {
                    ui.vertical(|ui| {
                        let btn_size = Vec2 { x: 70.0, y: 20.0 };
                        ui.horizontal(|ui| {
                            let vdc_btn = egui::Button::new("VDC")
                                .selected(self.metermode == MeterMode::Vdc)
                                .min_size(btn_size);
                            if ui.add(vdc_btn).clicked() {
                                self.set_mode(
                                    MeterMode::Vdc,
                                    "VDC",
                                    "CONF:VOLT:DC AUTO\n",
                                    Some("VDC"),
                                    None,
                                    None,
                                );
                            }
                            let vac_btn = egui::Button::new("VAC")
                                .selected(self.metermode == MeterMode::Vac)
                                .min_size(btn_size);
                            if ui.add(vac_btn).clicked() {
                                self.set_mode(
                                    MeterMode::Vac,
                                    "VAC",
                                    "CONF:VOLT:AC AUTO\n",
                                    Some("VAC"),
                                    None,
                                    None,
                                );
                            }
                            let adc_btn = egui::Button::new("ADC")
                                .selected(self.metermode == MeterMode::Adc)
                                .min_size(btn_size);
                            if ui.add(adc_btn).clicked() {
                                self.set_mode(
                                    MeterMode::Adc,
                                    "ADC",
                                    "CONF:CURR:DC AUTO\n",
                                    Some("ADC"),
                                    None,
                                    None,
                                );
                            }
                            let aac_btn = egui::Button::new("AAC")
                                .selected(self.metermode == MeterMode::Aac)
                                .min_size(btn_size);
                            if ui.add(aac_btn).clicked() {
                                self.set_mode(
                                    MeterMode::Aac,
                                    "AAC",
                                    "CONF:CURR:AC AUTO\n",
                                    Some("AAC"),
                                    None,
                                    None,
                                );
                            }
                        });
                        ui.horizontal(|ui| {
                            let res_btn = egui::Button::new("Ohm")
                                .selected(self.metermode == MeterMode::Res)
                                .min_size(btn_size);
                            if ui.add(res_btn).clicked() {
                                self.set_mode(
                                    MeterMode::Res,
                                    "Ohm",
                                    "CONF:RES AUTO\n",
                                    Some("RES"),
                                    None,
                                    None,
                                );
                            }
                            let cap_btn = egui::Button::new("C")
                                .selected(self.metermode == MeterMode::Cap)
                                .min_size(btn_size);
                            if ui.add(cap_btn).clicked() {
                                self.set_mode(
                                    MeterMode::Cap,
                                    "F",
                                    "CONF:CAP AUTO\n",
                                    Some("CAP"),
                                    None,
                                    None,
                                );
                            }
                            let freq_btn = egui::Button::new("Freq")
                                .selected(self.metermode == MeterMode::Freq)
                                .min_size(btn_size);
                            let freq_supported = self.device_type.lock().unwrap().supports_mode(MeterMode::Freq);
                            if ui.add_enabled(freq_supported, freq_btn).clicked() {
                                self.set_mode(
                                    MeterMode::Freq,
                                    "Hz",
                                    "CONF:FREQ\n",
                                    Some("FREQ"),
                                    None,
                                    None,
                                );
                            }
                            let per_btn = egui::Button::new("Period")
                                .selected(self.metermode == MeterMode::Per)
                                .min_size(btn_size);
                            let per_supported = self.device_type.lock().unwrap().supports_mode(MeterMode::Per);
                            if ui.add_enabled(per_supported, per_btn).clicked() {
                                self.set_mode(MeterMode::Per, "s", "CONF:PER\n", Some("PER"), None, None);
                            }
                        });
                        ui.horizontal(|ui| {
                            let diod_btn = egui::Button::new("Diode")
                                .selected(self.metermode == MeterMode::Diod)
                                .min_size(btn_size);
                            if ui.add(diod_btn).clicked() {
                                self.set_mode(
                                    MeterMode::Diod,
                                    "V",
                                    "CONF:DIOD\n",
                                    None,
                                    Some(self.beeper_enabled),
                                    None,
                                );
                            }
                            let cont_btn = egui::Button::new("Cont")
                                .selected(self.metermode == MeterMode::Cont)
                                .min_size(btn_size);
                            if ui.add(cont_btn).clicked() {
                                self.set_mode(
                                    MeterMode::Cont,
                                    "Ohm",
                                    "CONF:CONT\n",
                                    None,
                                    Some(self.beeper_enabled),
                                    None,
                                );
                            }
                            let temp_btn = egui::Button::new("Temp")
                                .selected(self.metermode == MeterMode::Temp)
                                .min_size(btn_size);
                            let temp_supported = self.device_type.lock().unwrap().supports_mode(MeterMode::Temp);
                            if ui.add_enabled(temp_supported, temp_btn).clicked() {
                                self.set_mode(
                                    MeterMode::Temp,
                                    "°C",
                                    "CONF:TEMP\n",
                                    Some("TEMP"),
                                    None,
                                    None,
                                );
                            }
                            let hold_btn = egui::Button::new("Hold")
                                .selected(self.hold_enabled)
                                .min_size(btn_size);
                            if ui.add(hold_btn).clicked() {
                                self.hold_enabled = !self.hold_enabled;
                                *self.hold_enabled_shared.lock().unwrap() = self.hold_enabled;
                                if self.value_debug { println!("Hold toggled via button: {}", self.hold_enabled); }
                                // Reset last_graph_update when resuming to allow immediate update
                                if !self.hold_enabled {
                                    self.last_graph_update = 0.0;
                                }
                                // Only send hardware hold command if device supports it
                                let supports_hold = self.device_type.lock().unwrap().plugin().supports_hold();
                                if self.value_debug { println!("Device supports hold: {}", supports_hold); }
                                if supports_hold {
                                    if let Some(tx) = self.serial_tx.clone() {
                                        let cmd = if self.hold_enabled {
                                            "MULT:HOLD ON\n".to_string()
                                        } else {
                                            "MULT:HOLD OFF\n".to_string()
                                        };
                                        if self.value_debug { println!("Sending hold command: {:?}", cmd); }
                                        let value_debug = self.value_debug;
                                        tokio::spawn(async move {
                                            if let Err(e) = tx.send(cmd).await {
                                                if value_debug { println!("Failed to queue hold command: {}", e); }
                                            }
                                        });
                                    } else {
                                        if self.value_debug { println!("serial_tx is None, cannot send hold command"); }
                                    }
                                }
                            }
                        });
                    });
                });

                let options_frame = egui::Frame {
                    inner_margin: 12.0.into(),
                    outer_margin: 24.0.into(),
                    corner_radius: 5.0.into(),
                    shadow: epaint::Shadow {
                        offset: [8, 12],
                        blur: 16,
                        spread: 0,
                        color: egui::Color32::from_black_alpha(180),
                    },
                    fill: self.box_background_color,
                    stroke: egui::Stroke::new(1.0, egui::Color32::GRAY),
                };
                options_frame.show(ui, |ui| {
                    ui.vertical(|ui| {
                        let rate_supported = self.device_type.lock().unwrap().supports_rate_control();
                        ui.add_enabled_ui(rate_supported, |ui| {
                            let ratebox = egui::ComboBox::from_label("Sampling Rate").show_index(
                                ui,
                                &mut self.curr_rate,
                                self.ratecmd.len(),
                                |i| self.ratecmd.get_opt(i).0,
                            );
                            if ratebox.changed() {
                                self.confstring = self
                                    .ratecmd
                                    .gen_scpi(self.ratecmd.get_opt(self.curr_rate).0);
                                if let Some(tx) = self.serial_tx.clone() {
                                    let cmd = self.confstring.clone();
                                    let value_debug = self.value_debug;
                                    tokio::spawn(async move {
                                        if let Err(e) = tx.send(cmd).await {
                                            if value_debug { println!("Failed to queue command: {}", e); }
                                        }
                                    });
                                }
                                if self.value_debug {
                                    println!("Selected Rate changed: {}", self.confstring);
                                }
                            }
                        });
                        
                        // Range control using plugin
                        {
                            let device_type = self.device_type.lock().unwrap();
                            if let Some(range_info) = device_type.plugin().range_info(self.metermode) {
                                // Clamp curr_range to valid index for this mode
                                if self.curr_range >= range_info.ranges.len() {
                                    self.curr_range = 0;
                                }
                                
                                // Get current range text
                                let current_text = range_info.ranges.get(self.curr_range)
                                    .map(|r| r.0)
                                    .unwrap_or("auto");
                                
                                let mut changed = false;
                                // Use unique ID per mode to prevent egui from reusing state between modes
                                let combo_id = format!("range_combo_{:?}", self.metermode);
                                // Disable combobox if only one range (e.g. Cap with auto only)
                                let range_enabled = range_info.ranges.len() > 1;
                                ui.add_enabled_ui(range_enabled, |ui| {
                                    egui::ComboBox::from_id_salt(combo_id)
                                        .selected_text(current_text)
                                        .show_ui(ui, |ui| {
                                            for (idx, (name, _)) in range_info.ranges.iter().enumerate() {
                                                if ui.selectable_value(&mut self.curr_range, idx, *name).changed() {
                                                    changed = true;
                                                }
                                            }
                                        });
                                });
                                
                                if changed {
                                    if let Some((name, scpi_val)) = range_info.ranges.get(self.curr_range) {
                                        // Build the SCPI command string for the selected range
                                        let cmd_string = if *name == "auto" {
                                            // Auto mode: use plugin's auto_on_cmd, or fallback to scpi_prefix + AUTO
                                            if let Some(cmd) = range_info.auto_on_cmd {
                                                cmd.to_string()
                                            } else {
                                                format!("{}AUTO\n", range_info.scpi_prefix)
                                            }
                                        } else {
                                            // Fixed range: optionally prepend auto-off, then range value
                                            let mut cmds = String::new();
                                            if let Some(auto_off) = range_info.auto_off_cmd {
                                                cmds.push_str(auto_off);
                                            }
                                            cmds.push_str(&format!("{}{}\n", range_info.scpi_prefix, scpi_val));
                                            cmds
                                        };
                                        if self.value_debug {
                                            println!("User selected range {}: {} - pending cmd: {}", self.curr_range, name, cmd_string.trim());
                                        }
                                        self.pending_changes.lock().unwrap().range = Some((self.curr_range, cmd_string));
                                    }
                                }
                            }
                        }
                        
                        // Add beeper and threshold controls for CONT and DIOD modes
                        if self.metermode == MeterMode::Cont || self.metermode == MeterMode::Diod {
                            let device_type = self.device_type.lock().unwrap();
                            let supports_beeper = device_type.plugin().supports_beeper();
                            let supports_threshold = device_type.plugin().supports_threshold();
                            drop(device_type); // Release the lock early
                            
                            // Beeper control - only show if device supports it
                            if supports_beeper {
                                let mut beeper = self.beeper_enabled;
                                if ui.checkbox(&mut beeper, "Beeper").changed() {
                                    self.beeper_enabled = beeper;
                                    if let Some(tx) = self.serial_tx.clone() {
                                        let cmd = if beeper {
                                            "SYST:BEEP:STATe ON\n".to_string()
                                        } else {
                                            "SYST:BEEP:STATe OFF\n".to_string()
                                        };
                                        let value_debug = self.value_debug;
                                        tokio::spawn(async move {
                                            if let Err(e) = tx.send(cmd).await {
                                                if value_debug {
                                                    println!("Failed to queue beeper command: {}", e);
                                                }
                                            }
                                        });
                                    }
                                }
                            }

                            if self.metermode == MeterMode::Cont {
                                let threshold_slider = ui.add_enabled(
                                    supports_threshold,
                                    egui::Slider::new(&mut self.cont_threshold, 0..=1000)
                                        .text("Threshold (Ω)")
                                        .step_by(1.0)
                                        .clamping(SliderClamping::Always),
                                );
                                if supports_threshold && (threshold_slider.drag_stopped() || threshold_slider.lost_focus())
                                {
                                    if let Some(tx) = self.serial_tx.clone() {
                                        let cmd =
                                            format!("CONT:THREshold {}\n", self.cont_threshold);
                                        let value_debug = self.value_debug;
                                        tokio::spawn(async move {
                                            if let Err(e) = tx.send(cmd).await {
                                                if value_debug {
                                                    println!(
                                                        "Failed to queue threshold command: {}",
                                                        e
                                                    );
                                                }
                                            }
                                        });
                                    }
                                }
                            } else if self.metermode == MeterMode::Diod {
                                let threshold_slider = ui.add_enabled(
                                    supports_threshold,
                                    egui::Slider::new(&mut self.diod_threshold, 0.0..=3.0)
                                        .text("Threshold (V)")
                                        .step_by(0.1)
                                        .clamping(SliderClamping::Always),
                                );
                                if supports_threshold && (threshold_slider.drag_stopped() || threshold_slider.lost_focus())
                                {
                                    if let Some(tx) = self.serial_tx.clone() {
                                        let cmd =
                                            format!("DIOD:THREshold {}\n", self.diod_threshold);
                                        let value_debug = self.value_debug;
                                        tokio::spawn(async move {
                                            if let Err(e) = tx.send(cmd).await {
                                                if value_debug {
                                                    println!(
                                                        "Failed to queue threshold command: {}",
                                                        e
                                                    );
                                                }
                                            }
                                        });
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
                // Scope to limit the mutable borrow of plot_dock_state
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

            // Show settings and recording windows
            self.show_settings(ctx);
            self.show_recording_window(ctx);
        });
    }
}

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
                egui::RichText::new(output_text)
                    .size(18.0)
                    .strong()
                    .color(egui::Color32::WHITE),
            )
            .fill(output_color);

            let btn_response = ui.add_enabled_ui(is_connected, |ui| {
                ui.add_sized([ui.available_width(), 40.0], output_btn)
            }).inner;
            if btn_response.clicked() {
                let new_state = !self.ps_output_on;
                self.ps_output_on = new_state;
                self.pending_changes.lock().unwrap().output_on = Some(new_state);
            }

            ui.add_space(8.0);

            // Output display — compact V/A with smaller unit labels, W row below
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
                    ui.label(
                        egui::RichText::new(format!("{:.3}", self.ps_voltage_readback))
                            .size(22.0)
                            .color(cyan)
                            .family(mono.clone()),
                    );
                    ui.label(
                        egui::RichText::new("V")
                            .size(12.0)
                            .color(cyan)
                            .family(mono.clone()),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new(format!("{:.3}", self.ps_current_readback))
                            .size(22.0)
                            .color(cyan)
                            .family(mono.clone()),
                    );
                    ui.label(
                        egui::RichText::new("A")
                            .size(12.0)
                            .color(cyan)
                            .family(mono.clone()),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!("{:.3}", self.ps_power_readback))
                            .size(14.0)
                            .color(egui::Color32::from_rgb(180, 220, 255))
                            .family(mono.clone()),
                    );
                    ui.label(
                        egui::RichText::new("W")
                            .size(10.0)
                            .color(egui::Color32::from_rgb(180, 220, 255))
                            .family(mono),
                    );
                });
            });

            ui.add_space(8.0);
            ui.separator();

            // Track focus across all PS text inputs this frame
            let mut any_ps_focus = false;

            // Voltage setting with up/down buttons
            ui.label(egui::RichText::new("Voltage (V)").strong());
            ui.horizontal(|ui| {
                let response = ui.add_enabled(
                    is_connected,
                    egui::TextEdit::singleline(&mut self.ps_voltage_input)
                        .desired_width(100.0)
                        .hint_text("0.000"),
                );
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

            // Current setting with up/down buttons
            ui.label(egui::RichText::new("Current (A)").strong());
            ui.horizontal(|ui| {
                let response = ui.add_enabled(
                    is_connected,
                    egui::TextEdit::singleline(&mut self.ps_current_input)
                        .desired_width(100.0)
                        .hint_text("0.000"),
                );
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

            // OVP setting with up/down buttons
            ui.label("OVP (V)");
            ui.horizontal(|ui| {
                let response = ui.add_enabled(
                    is_connected,
                    egui::TextEdit::singleline(&mut self.ps_ovp_input)
                        .desired_width(100.0)
                        .hint_text("0.000"),
                );
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

            // OCP setting with up/down buttons
            ui.label("OCP (A)");
            ui.horizontal(|ui| {
                let response = ui.add_enabled(
                    is_connected,
                    egui::TextEdit::singleline(&mut self.ps_ocp_input)
                        .desired_width(100.0)
                        .hint_text("0.000"),
                );
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

            // Store focus state for next frame - the PS receiver uses this
            // to avoid overwriting text buffers while user is typing
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
