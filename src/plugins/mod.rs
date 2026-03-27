//! Device plugin system for RustyMeter.
//!
//! Each supported device family implements the [`DevicePlugin`] trait, which provides:
//! - All SCPI command generation (no SCPI knowledge in GUI code)
//! - Response parsing
//! - Device capabilities (what controls to show in GUI)
//!
//! Plugin resolution uses hierarchical model-name matching:
//! e.g., for model "SPM6103": try SPM6103 → SPM610 → SPM61 → SPM6 → SPM → Default

mod default;
mod spm;

pub use default::DefaultPlugin;
pub use spm::SpmPlugin;

use crate::multimeter::MeterMode;

// ─── Core Types ─────────────────────────────────────────────────

/// Result of parsing a device response
#[derive(Debug)]
pub struct ParseResult {
    pub measurement: Option<f64>,
    pub mode: Option<MeterMode>,
    pub range_index: Option<usize>,
    pub precision: Option<f64>,
}

/// Range information for a specific mode — used by GUI to populate dropdowns.
/// The plugin's `range_command(mode, index)` generates the actual SCPI.
#[derive(Clone, Debug)]
pub struct RangeInfo {
    /// Display names for each range option (e.g., \["auto", "200mV", "2V", …\])
    pub ranges: Vec<&'static str>,
}

/// Power supply limits for a device with integrated PSU
#[derive(Clone, Debug)]
pub struct PowerSupplyLimits {
    pub voltage_min: f64,
    pub voltage_max: f64,
    pub current_min: f64,
    pub current_max: f64,
    pub ovp_max: f64,
    pub ocp_max: f64,
}

/// Describes what a device can do — GUI queries this to decide what controls to show.
#[derive(Clone, Debug)]
pub struct DeviceCapabilities {
    /// Meter modes supported by this device
    pub supported_modes: Vec<MeterMode>,
    /// Whether this device has an integrated power supply
    pub has_power_supply: bool,
    /// Power supply limits (voltage/current min/max, OVP/OCP max)
    pub power_supply_limits: Option<PowerSupplyLimits>,
    /// Sampling rate display names (e.g., \["Slow", "Medium", "Fast"\]).
    /// Empty vec means device does not support rate control.
    pub rate_options: Vec<&'static str>,
    /// Whether this device supports beeper control
    pub has_beeper: bool,
    /// Whether this device supports threshold settings (continuity/diode)
    pub has_threshold: bool,
    /// Whether this device supports hardware hold
    pub has_hold: bool,
}

/// Pending GUI changes awaiting communication with the instrument.
/// Each field is `Option<T>`: `None` = no pending change, `Some(v)` = user wants this value.
/// Shared between UI thread and serial task via `Arc<Mutex<>>`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PendingChanges {
    pub output_on: Option<bool>,
    pub voltage_set: Option<f64>,
    pub current_set: Option<f64>,
    pub ovp: Option<f64>,
    pub ocp: Option<f64>,
    pub range: Option<(usize, String)>,              // (range_index, full SCPI command string)
    pub mode: Option<(MeterMode, String, u32)>,      // (target mode, SCPI command, retries remaining)
}

impl PendingChanges {
    pub fn has_any(&self) -> bool {
        self.output_on.is_some()
            || self.voltage_set.is_some()
            || self.current_set.is_some()
            || self.ovp.is_some()
            || self.ocp.is_some()
            || self.range.is_some()
            || self.mode.is_some()
    }
}

/// Power supply readback state received from device
#[derive(Clone, Debug, Default)]
pub struct PowerSupplyState {
    pub output_on: bool,
    pub voltage_set: f64,
    pub current_set: f64,
    pub ovp: f64,
    pub ocp: f64,
    pub voltage_readback: f64,
    pub current_readback: f64,
    pub power_readback: f64,
    pub includes_settings: bool,
}

// ─── Shared Helpers ─────────────────────────────────────────────

/// Count the number of decimal places in a numeric string.
pub(crate) fn count_decimals(s: &str) -> usize {
    let s = s.trim_start_matches(|c: char| c == '+' || c == '-');
    if let Some(dot_pos) = s.find('.') {
        s[dot_pos + 1..].chars().take_while(|c| c.is_ascii_digit()).count()
    } else {
        0
    }
}

// ─── DevicePlugin Trait ─────────────────────────────────────────

/// Trait for device-specific behavior.
///
/// **All** SCPI command knowledge lives in the plugin implementation.
/// GUI queries [`capabilities()`](DevicePlugin::capabilities) to decide what controls to show.
pub trait DevicePlugin: Send + Sync {
    /// Human-readable plugin name
    fn name(&self) -> &str;

    /// What this device can do
    fn capabilities(&self) -> &DeviceCapabilities;

    /// Configure plugin from `*IDN?` response (e.g., detect firmware version).
    /// Called once by the registry after the plugin is created.
    fn configure_from_idn(&mut self, _idn: &str) {}

    // ─── SCPI Command Generation ────────────────────────────────

    /// Identify command (default: `*IDN?`)
    fn identify_command(&self) -> &str { "*IDN?\n" }

    /// Reset instrument
    fn reset_command(&self) -> &str { "*RST\n" }

    /// Enter remote mode
    fn remote_command(&self) -> Option<&str> { Some("SYST:REM\n") }

    /// Return to local mode
    fn local_command(&self) -> Option<&str> { Some("SYST:LOC\n") }

    /// Query for a measurement reading
    fn measurement_command(&self) -> &str;

    /// Query for current function/mode
    fn function_query_command(&self) -> &str;

    /// Command to switch to a specific meter mode
    fn mode_command(&self, mode: MeterMode) -> String;

    /// Command to set sampling rate by index into `capabilities().rate_options`.
    /// Returns `None` if device doesn't support rate control.
    fn rate_command(&self, _index: usize) -> Option<String> { None }

    /// Command(s) to set a specific range. May contain multiple lines
    /// (e.g. auto-off + range set). Returns `None` if mode has no ranges.
    fn range_command(&self, mode: MeterMode, range_index: usize) -> Option<String>;

    /// Command to enable/disable hardware hold
    fn hold_command(&self, _on: bool) -> Option<String> { None }

    /// Command to enable/disable beeper
    fn beeper_command(&self, _on: bool) -> Option<String> { None }

    /// Command to set continuity threshold (ohms)
    fn cont_threshold_command(&self, _ohms: u32) -> Option<String> { None }

    /// Command to set diode threshold (volts)
    fn diod_threshold_command(&self, _volts: f32) -> Option<String> { None }

    // ─── Power Supply Commands ──────────────────────────────────

    fn ps_output_command(&self, _on: bool) -> Option<String> { None }
    fn ps_query_output(&self) -> Option<&str> { None }
    fn ps_set_voltage(&self, _v: f64) -> Option<String> { None }
    fn ps_set_current(&self, _a: f64) -> Option<String> { None }
    fn ps_set_ovp(&self, _v: f64) -> Option<String> { None }
    fn ps_set_ocp(&self, _a: f64) -> Option<String> { None }
    fn ps_query_voltage(&self) -> Option<&str> { None }
    fn ps_query_current(&self) -> Option<&str> { None }
    fn ps_query_ovp(&self) -> Option<&str> { None }
    fn ps_query_ocp(&self) -> Option<&str> { None }
    fn ps_query_meas_all(&self) -> Option<&str> { None }

    // ─── Response Parsing ───────────────────────────────────────

    /// Parse a measurement/function response from the device
    fn parse_measurement(&self, response: &str) -> ParseResult;

    /// Parse output state response (`ON`/`OFF`, `0`/`1`)
    fn parse_output_state(&self, response: &str) -> Option<bool> {
        let t = response.trim();
        if t == "1" || t.eq_ignore_ascii_case("ON") {
            Some(true)
        } else if t == "0" || t.eq_ignore_ascii_case("OFF") {
            Some(false)
        } else {
            None
        }
    }

    /// Parse a numeric PS value response
    fn parse_ps_value(&self, response: &str) -> Option<f64> {
        response.trim().parse::<f64>().ok()
    }

    /// Parse `MEAS:ALL?` response → `(voltage, current, power)`
    fn parse_ps_meas_all(&self, response: &str) -> Option<(f64, f64, f64)> {
        let trimmed = response.trim();
        let parts: Vec<&str> = if trimmed.contains(',') {
            trimmed.split(',').collect()
        } else {
            trimmed.split_whitespace().collect()
        };
        if parts.len() >= 2 {
            if let (Ok(v), Ok(i)) = (parts[0].parse::<f64>(), parts[1].parse::<f64>()) {
                let p = if parts.len() >= 3 {
                    parts[2].parse::<f64>().unwrap_or(v * i)
                } else {
                    v * i
                };
                return Some((v, i, p));
            }
        }
        None
    }

    /// Parse a range query response and return the matching range index
    fn parse_range_response(&self, response: &str, mode: MeterMode) -> Option<usize>;

    /// Get range info for a specific mode (display names for GUI dropdown)
    fn range_info(&self, mode: MeterMode) -> Option<RangeInfo>;

    /// Get the unit string for a mode
    fn mode_unit(&self, mode: MeterMode) -> &str {
        match mode {
            MeterMode::Vdc => "VDC",
            MeterMode::Vac => "VAC",
            MeterMode::Adc => "ADC",
            MeterMode::Aac => "AAC",
            MeterMode::Res => "Ohm",
            MeterMode::Cap => "F",
            MeterMode::Freq => "Hz",
            MeterMode::Per => "s",
            MeterMode::Diod => "V",
            MeterMode::Cont => "Ohm",
            MeterMode::Temp => "°C",
        }
    }

    /// Get the display label for a mode button
    fn mode_label(&self, mode: MeterMode) -> &str {
        match mode {
            MeterMode::Vdc => "VDC",
            MeterMode::Vac => "VAC",
            MeterMode::Adc => "ADC",
            MeterMode::Aac => "AAC",
            MeterMode::Res => "Ohm",
            MeterMode::Cap => "C",
            MeterMode::Freq => "Freq",
            MeterMode::Per => "Period",
            MeterMode::Diod => "Diode",
            MeterMode::Cont => "Cont",
            MeterMode::Temp => "Temp",
        }
    }
}

// ─── Plugin Registry ────────────────────────────────────────────

/// Extract model name from `*IDN?` response.
/// `"OWON,SPM6103,25281747,FV:V2.1.0"` → `"SPM6103"`
fn extract_model(idn: &str) -> String {
    let parts: Vec<&str> = idn.split(',').collect();
    if parts.len() >= 2 {
        parts[1].trim().to_string()
    } else {
        String::new()
    }
}

struct PluginRegistryEntry {
    prefix: &'static str,
    factory: fn() -> Box<dyn DevicePlugin>,
}

/// All registered plugins. Add new device families here.
/// Plugins with longer prefixes are more specific and win over shorter ones.
fn plugin_registry() -> Vec<PluginRegistryEntry> {
    vec![
        PluginRegistryEntry { prefix: "SPM", factory: || Box::new(SpmPlugin::new()) },
        // Future plugins:
        // PluginRegistryEntry { prefix: "SPM6103", factory: || Box::new(Spm6103SpecificPlugin::new()) },
        // PluginRegistryEntry { prefix: "XDM", factory: || Box::new(XdmPlugin::new()) },
    ]
}

/// Resolve the best matching plugin for a device based on its `*IDN?` response.
///
/// # Matching algorithm
/// 1. Extract model name from IDN (e.g., `"SPM6103"`)
/// 2. Try progressively shorter prefixes: `SPM6103`, `SPM610`, `SPM61`, `SPM6`, `SPM`
/// 3. First registry match wins (most specific prefix)
/// 4. If no match, return [`DefaultPlugin`]
pub fn resolve_plugin(idn: &str) -> Box<dyn DevicePlugin> {
    let model = extract_model(idn);
    let registry = plugin_registry();

    let mut test_prefix = model.clone();
    while !test_prefix.is_empty() {
        for entry in &registry {
            if entry.prefix.eq_ignore_ascii_case(&test_prefix) {
                let mut plugin = (entry.factory)();
                plugin.configure_from_idn(idn);
                return plugin;
            }
        }
        test_prefix.pop();
    }

    let mut plugin: Box<dyn DevicePlugin> = Box::new(DefaultPlugin::new());
    plugin.configure_from_idn(idn);
    plugin
}

/// Create a default plugin (used before device is identified)
pub fn default_plugin() -> Box<dyn DevicePlugin> {
    Box::new(DefaultPlugin::new())
}
