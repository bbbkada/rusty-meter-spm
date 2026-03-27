//! Default plugin — baseline multimeter behaviour (XDM1041-like).
//!
//! Used as a fallback when no device-specific plugin matches the model name.
//! Implements the command set known from OWON XDM1041/XDM1241 bench multimeters.
//!
//! Key characteristics:
//! - `MEAS?` for measurements, `FUNC?` for function query
//! - `CONF:*` commands for mode and range
//! - `RATE S/M/F` for sampling rate
//! - Beeper and threshold control
//! - **No** power supply, **no** hardware hold

use crate::multimeter::MeterMode;
use super::{
    DeviceCapabilities, DevicePlugin, ParseResult, RangeInfo,
    count_decimals,
};

// ─── Internal range data ────────────────────────────────────────

struct InternalRangeInfo {
    scpi_prefix: &'static str,
    ranges: Vec<(&'static str, &'static str)>, // (display_name, scpi_value)
}

// ─── DefaultPlugin ──────────────────────────────────────────────

pub struct DefaultPlugin {
    capabilities: DeviceCapabilities,
    /// XDM1041 firmware < 4.3 swaps DIOD and CONT in FUNC? responses
    swap_diod_cont: bool,
}

impl DefaultPlugin {
    pub fn new() -> Self {
        Self {
            capabilities: DeviceCapabilities {
                supported_modes: vec![
                    MeterMode::Vdc,
                    MeterMode::Vac,
                    MeterMode::Adc,
                    MeterMode::Aac,
                    MeterMode::Res,
                    MeterMode::Cap,
                    MeterMode::Freq,
                    MeterMode::Per,
                    MeterMode::Diod,
                    MeterMode::Cont,
                    MeterMode::Temp,
                ],
                has_power_supply: false,
                power_supply_limits: None,
                rate_options: vec!["Slow", "Medium", "Fast"],
                has_beeper: true,
                has_threshold: true,
                has_hold: false,
            },
            swap_diod_cont: false,
        }
    }

    /// Internal range data with SCPI details.
    fn internal_range_info(&self, mode: MeterMode) -> Option<InternalRangeInfo> {
        match mode {
            MeterMode::Vdc => Some(InternalRangeInfo {
                scpi_prefix: "CONF:VOLT:DC ",
                ranges: vec![
                    ("auto", "AUTO"),
                    ("50mV", "50E-3"),
                    ("500mV", "500E-3"),
                    ("5V", "5"),
                    ("50V", "50"),
                    ("500V", "500"),
                    ("1000V", "1000"),
                ],
            }),
            MeterMode::Vac => Some(InternalRangeInfo {
                scpi_prefix: "CONF:VOLT:AC ",
                ranges: vec![
                    ("auto", "AUTO"),
                    ("500mV", "500E-3"),
                    ("5V", "5"),
                    ("50V", "50"),
                    ("500V", "500"),
                    ("750V", "750"),
                ],
            }),
            MeterMode::Adc => Some(InternalRangeInfo {
                scpi_prefix: "CONF:CURR:DC ",
                ranges: vec![
                    ("500uA", "500E-6"),
                    ("5mA", "5E-3"),
                    ("50mA", "50E-3"),
                    ("500mA", "500E-3"),
                    ("5A", "5"),
                    ("10A", "10"),
                ],
            }),
            MeterMode::Aac => Some(InternalRangeInfo {
                scpi_prefix: "CONF:CURR:AC ",
                ranges: vec![
                    ("500uA", "500E-6"),
                    ("5mA", "5E-3"),
                    ("50mA", "50E-3"),
                    ("500mA", "500E-3"),
                    ("5A", "5"),
                    ("10A", "10"),
                ],
            }),
            MeterMode::Res => Some(InternalRangeInfo {
                scpi_prefix: "CONF:RES ",
                ranges: vec![
                    ("auto", "AUTO"),
                    ("500Ohm", "500"),
                    ("5kOhm", "5E3"),
                    ("50kOhm", "50E3"),
                    ("500kOhm", "500E3"),
                    ("5MOhm", "5E6"),
                    ("50MOhm", "50E6"),
                ],
            }),
            MeterMode::Cap => Some(InternalRangeInfo {
                scpi_prefix: "CONF:CAP ",
                ranges: vec![
                    ("auto", "AUTO"),
                    ("50nF", "50E-9"),
                    ("500nF", "500E-9"),
                    ("5uF", "5E-6"),
                    ("50uF", "50E-6"),
                    ("500uF", "500E-6"),
                    ("5mF", "5E-3"),
                    ("50mF", "50E-3"),
                ],
            }),
            MeterMode::Temp => Some(InternalRangeInfo {
                scpi_prefix: "CONF:TEMP:RTD ",
                ranges: vec![
                    ("PT100", "PT100"),
                    ("K-type (KITS90)", "KITS90"),
                ],
            }),
            _ => None, // Freq, Per, Diod, Cont have no user-selectable ranges
        }
    }
}

impl DevicePlugin for DefaultPlugin {
    fn name(&self) -> &str { "Default" }

    fn capabilities(&self) -> &DeviceCapabilities { &self.capabilities }

    fn configure_from_idn(&mut self, idn: &str) {
        // Detect XDM1041/XDM1241 firmware version for DIOD/CONT swap
        let parts: Vec<&str> = idn.split(',').collect();
        if parts.len() >= 4 && parts[0] == "OWON"
            && (parts[1] == "XDM1041" || parts[1] == "XDM1241")
        {
            let fw_version = parts[3].trim_start_matches('V');
            let vparts: Vec<&str> = fw_version.split('.').collect();
            if vparts.len() >= 2 {
                if let (Ok(major), Ok(minor)) = (vparts[0].parse::<u32>(), vparts[1].parse::<u32>()) {
                    self.swap_diod_cont = major < 4 || (major == 4 && minor < 3);
                }
            }
        }
    }

    // ─── SCPI Command Generation ────────────────────────────────

    fn measurement_command(&self) -> &str { "MEAS?\n" }
    fn function_query_command(&self) -> &str { "FUNC?\n" }

    fn mode_command(&self, mode: MeterMode) -> String {
        match mode {
            MeterMode::Vdc  => "CONF:VOLT:DC AUTO\n".to_string(),
            MeterMode::Vac  => "CONF:VOLT:AC AUTO\n".to_string(),
            MeterMode::Adc  => "FUNC:CURR:DC\n".to_string(),
            MeterMode::Aac  => "FUNC:CURR:AC\n".to_string(),
            MeterMode::Res  => "CONF:RES AUTO\n".to_string(),
            MeterMode::Cap  => "CONF:CAP AUTO\n".to_string(),
            MeterMode::Freq => "CONF:FREQ\n".to_string(),
            MeterMode::Per  => "CONF:PER\n".to_string(),
            MeterMode::Diod => "CONF:DIOD\n".to_string(),
            MeterMode::Cont => "CONF:CONT\n".to_string(),
            MeterMode::Temp => "CONF:TEMP:RTD PT100\n".to_string(),
        }
    }

    fn rate_command(&self, index: usize) -> Option<String> {
        let val = match index {
            0 => "S",
            1 => "M",
            2 => "F",
            _ => return None,
        };
        Some(format!("RATE {}\n", val))
    }

    fn range_command(&self, mode: MeterMode, range_index: usize) -> Option<String> {
        let info = self.internal_range_info(mode)?;
        let (_name, scpi_val) = info.ranges.get(range_index)?;
        // XDM1041: no separate auto on/off commands — uses "CONF:VOLT:DC AUTO"
        Some(format!("{}{}\n", info.scpi_prefix, scpi_val))
    }

    fn beeper_command(&self, on: bool) -> Option<String> {
        if on {
            Some("SYST:BEEP:STATe ON\n".to_string())
        } else {
            Some("SYST:BEEP:STATe OFF\n".to_string())
        }
    }

    fn cont_threshold_command(&self, ohms: u32) -> Option<String> {
        Some(format!("CONT:THREshold {}\n", ohms))
    }

    fn diod_threshold_command(&self, volts: f32) -> Option<String> {
        Some(format!("DIOD:THREshold {}\n", volts))
    }

    // ─── Response Parsing ───────────────────────────────────────

    fn parse_measurement(&self, response: &str) -> ParseResult {
        let trimmed = response.trim();

        // Handle quoted function responses from FUNC?
        let unquoted = trimmed.trim_matches('"');
        if unquoted.starts_with("VOLT") || unquoted.starts_with("CURR")
            || unquoted == "FREQ" || unquoted == "PER"
            || unquoted == "CAP" || unquoted == "CONT"
            || unquoted == "DIOD" || unquoted == "RES"
            || unquoted == "TEMP"
        {
            // Check for space-separated AC/DC modes FIRST (e.g. "VOLT AC")
            let mode = if unquoted == "VOLT AC" || unquoted.starts_with("VOLT AC ") {
                MeterMode::Vac
            } else if unquoted == "CURR AC" || unquoted.starts_with("CURR AC ") {
                MeterMode::Aac
            } else {
                let mode_str = unquoted.split_whitespace().next().unwrap_or(unquoted);
                match mode_str {
                    "VOLT" | "VOLT:DC" => MeterMode::Vdc,
                    "VOLT:AC" => MeterMode::Vac,
                    "CURR" | "CURR:DC" => MeterMode::Adc,
                    "CURR:AC" => MeterMode::Aac,
                    "RES" => MeterMode::Res,
                    "CAP" => MeterMode::Cap,
                    "FREQ" => MeterMode::Freq,
                    "PER" => MeterMode::Per,
                    "TEMP" => MeterMode::Temp,
                    "DIOD" => if self.swap_diod_cont { MeterMode::Cont } else { MeterMode::Diod },
                    "CONT" => if self.swap_diod_cont { MeterMode::Diod } else { MeterMode::Cont },
                    _ => return ParseResult { measurement: None, mode: None, range_index: None, precision: None },
                }
            };
            return ParseResult { measurement: None, mode: Some(mode), range_index: None, precision: None };
        }

        // Try parsing direct measurement value (format: "+1.234")
        if let Ok(meas) = trimmed.parse::<f64>() {
            let raw = count_decimals(trimmed);
            let precision = 10_f64.powi(-(raw as i32));
            return ParseResult {
                measurement: Some(meas),
                mode: None,
                range_index: None,
                precision: Some(precision),
            };
        }

        ParseResult { measurement: None, mode: None, range_index: None, precision: None }
    }

    fn parse_range_response(&self, response: &str, mode: MeterMode) -> Option<usize> {
        let trimmed = response.trim().trim_matches('"');
        let normalized = trimmed.replace(' ', "");
        let info = self.internal_range_info(mode)?;

        // Try matching against SCPI value
        for (idx, (_, scpi_val)) in info.ranges.iter().enumerate() {
            if scpi_val.eq_ignore_ascii_case(&normalized) {
                return Some(idx);
            }
        }

        // Try matching against display name
        for (idx, (display, _)) in info.ranges.iter().enumerate() {
            if display.replace(' ', "").eq_ignore_ascii_case(&normalized) {
                return Some(idx);
            }
        }

        // XDM1041: extract range value (last part after space)
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        if parts.len() > 1 {
            let range_val = parts.last()?;
            for (idx, (_, scpi_val)) in info.ranges.iter().enumerate() {
                if scpi_val.eq_ignore_ascii_case(range_val) {
                    return Some(idx);
                }
            }
        }

        None
    }

    fn range_info(&self, mode: MeterMode) -> Option<RangeInfo> {
        let info = self.internal_range_info(mode)?;
        Some(RangeInfo {
            ranges: info.ranges.iter().map(|(name, _)| *name).collect(),
        })
    }
}
