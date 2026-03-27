use chrono::DateTime;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};

use egui::{Color32, Context, FontData, FontDefinitions, FontFamily};
use egui_dock::DockState;
use mio::{Events, Poll};
use mio_serial::{SerialPortInfo, SerialStream};
use tokio::sync::{mpsc, oneshot};

use crate::multimeter::MeterMode;
use crate::plugins::{DevicePlugin, PowerSupplyState};

// Submodules for split impl blocks
mod graph;
mod recording;
mod serial;
mod settings;
mod ui;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const MEM_DEPTH_DEFAULT: usize = 100; // Default slider value
const MEM_DEPTH_MAX_DEFAULT: usize = 2000; // Default maximum
const HIST_MEM_DEPTH_DEFAULT: usize = 1000; // Default histogram memory depth
const HIST_MEM_DEPTH_MAX_DEFAULT: usize = 10000; // Default maximum histogram memory depth

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum RecordingFormat {
    Csv,
    Json,
    Xlsx,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum RecordingMode {
    FixedInterval,
    Manual,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum TimestampFormat {
    Rfc3339,
    Unix,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Record {
    pub index: usize, // New field for measurement index
    #[serde(with = "chrono::serde::ts_seconds")]
    pub timestamp: DateTime<chrono::Utc>,
    pub unit: String,
    pub value: f64,
}

/// We derive Deserialize/Serialize so we can persist app state on shutdown.
#[derive(Serialize, Deserialize)]
#[serde(default)] // if we add new fields, give them default values when deserializing old state
pub struct MyApp {
    serial_port: String,
    baud_rate: u32,
    bits: u32,
    stop_bits: u32,
    parity: bool,
    mem_depth: usize,              // Persistent, adjustable via slider
    mem_depth_max: usize,          // Persistent, maximum for slider
    hist_mem_depth: usize,         // Persistent, histogram memory depth
    hist_mem_depth_max: usize,     // Persistent, maximum for histogram memory depth
    hist_collect_interval_ms: u64, // Persistent, histogram collection interval
    hist_collect_active: bool,     // Persistent, whether histogram collection is active
    connect_on_startup: bool,
    value_debug: bool,
    poll_interval_ms: u64,
    graph_update_interval_ms: u64, // Persistent, adjustable via slider in main GUI
    graph_update_interval_max: u64, // Persistent, maximum for graph update interval slider
    beeper_enabled: bool,          // Persistent beeper state
    cont_threshold: u32,           // Persistent continuity threshold (0–1000 ohms)
    diod_threshold: f32,           // Persistent diode threshold (0–3.0 volts)
    lock_remote: bool,             // Persistent, whether to lock meter in remote mode
    hold_enabled: bool,            // Persistent, whether hold mode is enabled
    curr_rate: usize,              // Persistent, current sampling rate index
    reverse_graph: bool,           // Persistent, whether to reverse graph direction
    graph_line_color: Color32,     // Persistent, color for graph line
    hist_bar_color: Color32,       // Persistent, color for histogram bars
    measurement_font_color: Color32, // Persistent, color for measurement box font
    box_background_color: Color32, // Persistent, background color for measurement, mode, and option boxes
    always_on_top: bool,           // Persistent, keep window above all others
    #[serde(skip)]
    recording_open: bool, // Do not persist, whether recording viewport is open
    recording_format: RecordingFormat, // Persistent, selected recording format
    recording_file_path: String,   // Persistent, target file path
    recording_mode: RecordingMode, // Persistent, recording mode
    recording_interval_ms: u64,    // Persistent, fixed interval duration
    recording_active: bool,        // Persistent, whether recording is active
    recording_timestamp_format: TimestampFormat, // Persistent, timestamp format
    #[serde(skip)]
    recording_data: Vec<Record>, // Do not persist recording data
    #[serde(skip)]
    recording_data_len: usize, // Do not persist, tracks length of recording_data for auto-scroll
    #[serde(skip)]
    curr_meter: String,
    #[serde(skip)]
    metermode: MeterMode,
    #[serde(skip)]
    confstring: String,
    #[serde(skip)]
    curr_meas: f64,
    #[serde(skip)]
    curr_precision: f64,   // Measurement precision in base unit (e.g. 1.0 = ±1 Ohm)
    #[serde(skip)]
    curr_unit: String,
    #[serde(skip)]
    issue_new_write: bool,
    #[serde(skip)]
    readbuf: [u8; 1024],
    #[serde(skip)]
    portlist: VecDeque<String>,
    #[serde(skip)]
    values: VecDeque<f64>,
    #[serde(skip)]
    hist_values: VecDeque<f64>, // Buffer for histogram data
    #[serde(skip)]
    poll: Poll,
    #[serde(skip)]
    events: Events,
    #[serde(skip)]
    serial: Option<SerialStream>,
    #[serde(skip)]
    device: Arc<Mutex<String>>, // Device IDN string (shared with serial task)
    #[serde(skip)]
    plugin: Arc<Mutex<Box<dyn DevicePlugin>>>, // Active device plugin (shared with serial task)
    #[serde(skip)]
    ports: Vec<SerialPortInfo>,
    #[serde(skip)]
    tempdir: Option<tempfile::TempDir>,
    #[serde(skip)]
    settings_open: bool,
    #[serde(skip)]
    is_init: bool,
    #[serde(skip)]
    curr_range: usize,
    #[serde(skip)]
    pending_mode_change: Option<MeterMode>, // Track mode being changed to, ignore range updates during transition
    #[serde(skip)]
    serial_rx: Option<mpsc::Receiver<Option<(f64, f64)>>>, // handle measurements (value, precision)
    #[serde(skip)]
    serial_tx: Option<mpsc::Sender<String>>, // channel for sending commands to serial task
    #[serde(skip)]
    shutdown_tx: Option<oneshot::Sender<()>>, // Signal to shutdown serial task
    #[serde(skip)]
    mode_rx: Option<mpsc::Receiver<MeterMode>>, // Channel for mode updates
    #[serde(skip)]
    range_rx: Option<mpsc::Receiver<(MeterMode, usize)>>, // Channel for (mode, range_index) updates
    #[serde(skip)]
    value_debug_shared: Arc<Mutex<bool>>, // Shared debug flag for live updates
    #[serde(skip)]
    poll_interval_shared: Arc<Mutex<u64>>, // Shared poll interval for live updates
    #[serde(skip)]
    hold_enabled_shared: Arc<Mutex<bool>>, // Shared hold flag for pausing measurements
    #[serde(skip)]
    graph_update_interval_shared: Arc<Mutex<u64>>, // Shared graph update interval
    #[serde(skip)]
    last_graph_update: f64, // Track last graph update time
    #[serde(skip)]
    last_hist_collect_time: f64, // Track last histogram collection time
    #[serde(skip)]
    connection_state: ConnectionState, // New field to track connection status
    #[serde(skip)]
    connection_error: Option<String>, // New field to store connection error message
    #[serde(skip)]
    meas_count: u32, // Track measurement cycles for periodic FUNC? polling
    #[serde(skip)]
    last_record_time: f64, // Track last recording time for fixed interval
    graph_config: graph::GraphConfig, // Graph configuration
    #[serde(skip)]
    plot_dock_state: DockState<ui::PlotTab>, // Dock state for plot tabs
    // Power supply state (SPM6103)
    #[serde(skip)]
    ps_output_on: bool,
    #[serde(skip)]
    pending_changes: Arc<Mutex<crate::plugins::PendingChanges>>,  // Shared pending GUI changes
    ps_voltage_set: f64,      // Persistent: user's desired voltage
    ps_current_set: f64,      // Persistent: user's desired current
    ps_ovp: f64,              // Persistent: overvoltage protection
    ps_ocp: f64,              // Persistent: overcurrent protection
    #[serde(skip)]
    ps_voltage_readback: f64, // Live readback from device
    #[serde(skip)]
    ps_current_readback: f64, // Live readback from device
    #[serde(skip)]
    ps_power_readback: f64,   // Live power readback from device
    #[serde(skip)]
    ps_rx: Option<mpsc::Receiver<PowerSupplyState>>, // Channel for PS state updates
    #[serde(skip)]
    ps_initial_sync_done: bool, // Whether initial PS state sync has been done
    #[serde(skip)]
    ps_voltage_input: String,  // Text input buffer for voltage
    #[serde(skip)]
    ps_current_input: String,  // Text input buffer for current
    #[serde(skip)]
    ps_ovp_input: String,      // Text input buffer for OVP
    #[serde(skip)]
    ps_ocp_input: String,      // Text input buffer for OCP
    #[serde(skip)]
    ps_input_has_focus: bool,  // True if any PS text input had focus last frame
    #[serde(skip)]
    ps_confirmed_non_ps: bool, // Set to true when connected device confirmed to have no PS
}

// Enum to track connection state
#[derive(PartialEq)]
enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
}

impl Default for MyApp {
    fn default() -> Self {
        Self {
            serial_port: "".to_owned(),
            baud_rate: 115200,
            bits: 8,
            stop_bits: 1,
            parity: false,
            mem_depth: MEM_DEPTH_DEFAULT,
            mem_depth_max: MEM_DEPTH_MAX_DEFAULT,
            hist_mem_depth: HIST_MEM_DEPTH_DEFAULT,
            hist_mem_depth_max: HIST_MEM_DEPTH_MAX_DEFAULT,
            hist_collect_interval_ms: 100,
            hist_collect_active: false,
            connect_on_startup: false,
            value_debug: false,
            curr_meter: "".to_owned(),
            metermode: MeterMode::Vdc,
            confstring: "".to_owned(),
            curr_meas: f64::NAN,
            curr_precision: 0.0001,
            curr_unit: "VDC".to_owned(),
            issue_new_write: false,
            readbuf: [0u8; 1024],
            portlist: VecDeque::with_capacity(11),
            values: VecDeque::with_capacity(MEM_DEPTH_DEFAULT + 1),
            hist_values: VecDeque::with_capacity(MEM_DEPTH_DEFAULT + 1),
            poll: Poll::new().unwrap(),
            events: Events::with_capacity(1),
            serial: None,
            device: Arc::new(Mutex::new("".to_owned())),
            plugin: Arc::new(Mutex::new(crate::plugins::default_plugin())),
            ports: vec![],
            tempdir: tempfile::Builder::new().prefix("rustymeter").tempdir().ok(),
            settings_open: false,
            is_init: false,
            curr_rate: 0,
            curr_range: 0,
            pending_mode_change: None,
            reverse_graph: false,
            graph_line_color: Color32::from_rgb(0, 255, 255),
            hist_bar_color: Color32::from_rgb(0, 255, 0),
            measurement_font_color: Color32::from_rgb(0, 255, 255),
            box_background_color: Color32::from_rgba_unmultiplied(0, 0, 0, 255),
            always_on_top: false,
            recording_open: false,
            recording_format: RecordingFormat::Csv,
            recording_file_path: "".to_owned(),
            recording_mode: RecordingMode::FixedInterval,
            recording_interval_ms: 1000,
            recording_active: false,
            recording_timestamp_format: TimestampFormat::Rfc3339,
            recording_data: vec![],
            recording_data_len: 0,
            serial_rx: None,
            serial_tx: None,
            shutdown_tx: None,
            mode_rx: None,
            range_rx: None,
            poll_interval_ms: 20,
            graph_update_interval_ms: 20,
            graph_update_interval_max: 1000,
            beeper_enabled: true,
            cont_threshold: 50,
            diod_threshold: 2.0,
            hold_enabled: false,
            lock_remote: true,
            value_debug_shared: Arc::new(Mutex::new(false)),
            poll_interval_shared: Arc::new(Mutex::new(20)),
            hold_enabled_shared: Arc::new(Mutex::new(false)),
            graph_update_interval_shared: Arc::new(Mutex::new(20)),
            last_graph_update: 0.0,
            last_hist_collect_time: 0.0,
            connection_state: ConnectionState::Disconnected,
            connection_error: None,
            meas_count: 0,
            last_record_time: 0.0,
            graph_config: graph::GraphConfig::default(),
            plot_dock_state: DockState::new(vec![]),
            // Power supply defaults
            ps_output_on: false,
            pending_changes: Arc::new(Mutex::new(crate::plugins::PendingChanges::default())),
            ps_voltage_set: 5.0,
            ps_current_set: 1.0,
            ps_ovp: 32.0,
            ps_ocp: 3.2,
            ps_voltage_readback: 0.0,
            ps_current_readback: 0.0,
            ps_power_readback: 0.0,
            ps_rx: None,
            ps_initial_sync_done: false,
            ps_voltage_input: "5.000".to_string(),
            ps_current_input: "1.000".to_string(),
            ps_ovp_input: "32.000".to_string(),
            ps_ocp_input: "3.200".to_string(),
            ps_input_has_focus: false,
            ps_confirmed_non_ps: false,
        }
    }
}

impl MyApp {
    /// Called once before the first frame.
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut fonts = FontDefinitions::default();

        fonts.font_data.insert(
            "B612Mono-Bold".to_owned(),
            Arc::new(FontData::from_static(include_bytes!(
                "../../assets/fonts/B612Mono-Bold.ttf"
            ))),
        );

        let mut newfam = BTreeMap::new();
        newfam.insert(
            FontFamily::Name("B612Mono-Bold".into()),
            vec!["B612Mono-Bold".to_owned()],
        );
        fonts.families.append(&mut newfam);

        cc.egui_ctx.set_fonts(fonts);

        if let Some(storage) = cc.storage {
            let app: MyApp = eframe::get_value(storage, eframe::APP_KEY).unwrap_or_default();
            *app.value_debug_shared.lock().unwrap() = app.value_debug;
            *app.poll_interval_shared.lock().unwrap() = app.poll_interval_ms;
            *app.graph_update_interval_shared.lock().unwrap() = app.graph_update_interval_ms;
            return app;
        }

        let app = Self::default();
        *app.value_debug_shared.lock().unwrap() = app.value_debug;
        *app.poll_interval_shared.lock().unwrap() = app.poll_interval_ms;
        *app.graph_update_interval_shared.lock().unwrap() = app.graph_update_interval_ms;
        app
    }

    fn spawn_graph_update_task(&mut self, ctx: Context) {
        let graph_update_interval_shared = self.graph_update_interval_shared.clone();
        let ctx = ctx.clone();

        tokio::spawn(async move {
            loop {
                let interval = *graph_update_interval_shared.lock().unwrap();
                ctx.request_repaint();
                tokio::time::sleep(Duration::from_millis(interval)).await;
            }
        });
    }

    /// Set meter mode — queries plugin for all SCPI commands.
    fn set_mode(&mut self, mode: MeterMode) {
        let plugin = self.plugin.lock().unwrap();
        let unit = plugin.mode_unit(mode).to_owned();
        let mode_cmd = plugin.mode_command(mode);
        let caps = plugin.capabilities().clone();
        drop(plugin);

        self.pending_mode_change = Some(mode);
        self.metermode = mode;
        self.curr_unit = unit;
        self.curr_range = 0; // Reset to AUTO/first range
        self.confstring = mode_cmd.clone();

        // Set pending mode change — serial task will send and retry until instrument confirms
        self.pending_changes.lock().unwrap().mode = Some((mode, mode_cmd, 5));

        // Send beeper/threshold for Diod and Cont modes (best-effort, via channel)
        if mode == MeterMode::Diod || mode == MeterMode::Cont {
            if let Some(tx) = self.serial_tx.clone() {
                let plugin = self.plugin.lock().unwrap();
                let beeper_cmd = if caps.has_beeper {
                    plugin.beeper_command(self.beeper_enabled)
                } else {
                    None
                };
                let threshold_cmd = if caps.has_threshold {
                    if mode == MeterMode::Cont {
                        plugin.cont_threshold_command(self.cont_threshold)
                    } else {
                        plugin.diod_threshold_command(self.diod_threshold)
                    }
                } else {
                    None
                };
                drop(plugin);

                let value_debug = self.value_debug;
                tokio::spawn(async move {
                    if let Some(cmd) = beeper_cmd {
                        if let Err(e) = tx.send(cmd).await {
                            if value_debug { println!("Failed to queue beeper command: {}", e); }
                        }
                    }
                    if let Some(cmd) = threshold_cmd {
                        if let Err(e) = tx.send(cmd).await {
                            if value_debug { println!("Failed to queue threshold command: {}", e); }
                        }
                    }
                });
            }
        }

        self.values = VecDeque::with_capacity(self.mem_depth);
        self.hist_values = VecDeque::with_capacity(self.hist_mem_depth);
    }

    // Method to handle disconnection
    fn disconnect(&mut self) {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }
        self.serial_tx = None;
        self.serial_rx = None;
        self.mode_rx = None;
        self.ps_rx = None;
        self.serial = None;
        self.connection_state = ConnectionState::Disconnected;
        self.connection_error = None;
        let mut device = self.device.lock().unwrap();
        *device = "".to_owned();
        // Reset plugin to default
        *self.plugin.lock().unwrap() = crate::plugins::default_plugin();
        self.curr_meas = f64::NAN;
        self.values.clear();
        self.hist_values.clear();
        self.meas_count = 0;
        self.ps_output_on = false;
        self.ps_voltage_readback = 0.0;
        self.ps_current_readback = 0.0;
        self.ps_power_readback = 0.0;
        self.ps_initial_sync_done = false;
        self.ps_confirmed_non_ps = false;
        *self.pending_changes.lock().unwrap() = crate::plugins::PendingChanges::default();
    }
}
