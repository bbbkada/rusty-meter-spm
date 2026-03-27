//! SPM plugin — supports all OWON SPM-series power supplies with integrated multimeter.
//!
//! Tested against **SPM6103**. Matched for any model whose name starts with "SPM".
//!
//! Key characteristics:
//! - `CONFigure:ALL?` for both measurement *and* function query (comma-separated)
//! - `FUNC:*` commands for mode switching
//! - `SENS:*:RANG` / `*:RANG:AUTO` for range control
//! - Hardware hold via `MULT:HOLD`
//! - Integrated power supply (OUTP, VOLT, CURR, MEAS:ALL?, etc.)
//! - **No** sampling rate control, beeper, or threshold settings

use crate::multimeter::MeterMode;
use super::{
    DeviceCapabilities, DevicePlugin, ParseResult, PowerSupplyLimits, RangeInfo,
    count_decimals,
};

// ─── SI-prefix helper (SPM-specific) ────────────────────────────

/// Extract SI prefix multiplier from a unit suffix string.
/// e.g. "mV" → 0.001, "kOhm" → 1000, "KOhm" → 1000, "nF" → 1e-9, "MOhm" → 1e6
fn extract_si_multiplier(unit_suffix: &str) -> f64 {
    if unit_suffix.is_empty() {
        return 1.0;
    }
    match unit_suffix.chars().next().unwrap() {
        'n' => 1e-9,
        'u' | 'μ' => 1e-6,
        'm' => 0.001,
        'k' | 'K' => 1e3,
        'M' => 1e6,
        'G' => 1e9,
        _ => 1.0,
    }
}

// ─── Internal range data ────────────────────────────────────────

struct InternalRangeInfo {
    scpi_prefix: &'static str,
    auto_on_cmd: Option<&'static str>,
    auto_off_cmd: Option<&'static str>,
    ranges: Vec<(&'static str, &'static str)>, // (display_name, scpi_value)
}

// ─── SpmPlugin ──────────────────────────────────────────────────

pub struct SpmPlugin {
    capabilities: DeviceCapabilities,
}

impl SpmPlugin {
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
                    MeterMode::Diod,
                    MeterMode::Cont,
                ],
                has_power_supply: true,
                power_supply_limits: Some(PowerSupplyLimits {
                    voltage_min: 0.0,
                    voltage_max: 60.0,
                    current_min: 0.0,
                    current_max: 3.2,
                    ovp_max: 63.0,
                    ocp_max: 3.5,
                }),
                rate_options: vec![], // SPM does not support rate control
                has_beeper: false,
                has_threshold: false,
                has_hold: true,
            },
        }
    }

    /// Internal range data with full SCPI details (not exposed to GUI).
    fn internal_range_info(&self, mode: MeterMode) -> Option<InternalRangeInfo> {
        match mode {
            MeterMode::Vdc => Some(InternalRangeInfo {
                scpi_prefix: "SENS:VOLT:DC:RANG ",
                auto_on_cmd: Some("VOLT:DC:RANG:AUTO 1\n"),
                auto_off_cmd: Some("VOLT:DC:RANG:AUTO 0\n"),
                ranges: vec![
                    ("auto", "AUTO"),
                    ("200mV", "200E-3"),
                    ("2V", "2"),
                    ("20V", "20"),
                    ("200V", "200"),
                    ("1000V", "1000"),
                ],
            }),
            MeterMode::Vac => Some(InternalRangeInfo {
                scpi_prefix: "SENS:VOLT:AC:RANG ",
                auto_on_cmd: Some("VOLT:AC:RANG:AUTO 1\n"),
                auto_off_cmd: Some("VOLT:AC:RANG:AUTO 0\n"),
                ranges: vec![
                    ("auto", "AUTO"),
                    ("200mV", "200E-3"),
                    ("2V", "2"),
                    ("20V", "20"),
                    ("200V", "200"),
                    ("750V", "750"),
                ],
            }),
            MeterMode::Adc => Some(InternalRangeInfo {
                scpi_prefix: "SENS:CURR:DC:RANG ",
                auto_on_cmd: None, // No auto for current on SPM
                auto_off_cmd: None,
                ranges: vec![
                    ("200mA", "200E-3"),
                    ("10A", "10"),
                ],
            }),
            MeterMode::Aac => Some(InternalRangeInfo {
                scpi_prefix: "SENS:CURR:AC:RANG ",
                auto_on_cmd: None,
                auto_off_cmd: None,
                ranges: vec![
                    ("200mA", "200E-3"),
                    ("10A", "10"),
                ],
            }),
            MeterMode::Res => Some(InternalRangeInfo {
                scpi_prefix: "SENS:RES:RANG ",
                auto_on_cmd: Some("RES:RANG:AUTO 1\n"),
                auto_off_cmd: Some("RES:RANG:AUTO 0\n"),
                ranges: vec![
                    ("auto", "AUTO"),
                    ("200Ohm", "200"),
                    ("2kOhm", "2E3"),
                    ("20kOhm", "20E3"),
                    ("200kOhm", "200E3"),
                    ("2MOhm", "2E6"),
                    ("20MOhm", "20E6"),
                    ("100MOhm", "100E6"),
                ],
            }),
            MeterMode::Cap => Some(InternalRangeInfo {
                scpi_prefix: "SENS:CAP:RANG ",
                auto_on_cmd: None,
                auto_off_cmd: None,
                ranges: vec![
                    ("auto", "AUTO"),
                ],
            }),
            _ => None, // Diod, Cont have no user-selectable ranges
        }
    }
}

impl DevicePlugin for SpmPlugin {
    fn name(&self) -> &str { "SPM" }

    fn capabilities(&self) -> &DeviceCapabilities { &self.capabilities }

    // ─── SCPI Command Generation ────────────────────────────────

    fn measurement_command(&self) -> &str { "CONFigure:ALL?\n" }
    fn function_query_command(&self) -> &str { "CONFigure:ALL?\n" }

    fn mode_command(&self, mode: MeterMode) -> String {
        match mode {
            MeterMode::Vdc  => "FUNC:VOLT:DC\n".to_string(),
            MeterMode::Vac  => "FUNC:VOLT:AC\n".to_string(),
            MeterMode::Adc  => "FUNC:CURR:DC\n".to_string(),
            MeterMode::Aac  => "FUNC:CURR:AC\n".to_string(),
            MeterMode::Res  => "FUNC:RES\n".to_string(),
            MeterMode::Cap  => "FUNC:CAP\n".to_string(),
            MeterMode::Freq => "FUNC:FREQ\n".to_string(),
            MeterMode::Per  => "FUNC:PER\n".to_string(),
            MeterMode::Diod => "FUNC:DIOD\n".to_string(),
            MeterMode::Cont => "FUNC:CONT\n".to_string(),
            MeterMode::Temp => "FUNC:TEMP\n".to_string(),
        }
    }

    fn range_command(&self, mode: MeterMode, range_index: usize) -> Option<String> {
        let info = self.internal_range_info(mode)?;
        let (name, scpi_val) = info.ranges.get(range_index)?;
        if *name == "auto" {
            if let Some(cmd) = info.auto_on_cmd {
                Some(cmd.to_string())
            } else {
                Some(format!("{}AUTO\n", info.scpi_prefix))
            }
        } else {
            let mut cmds = String::new();
            if let Some(auto_off) = info.auto_off_cmd {
                cmds.push_str(auto_off);
            }
            cmds.push_str(&format!("{}{}\n", info.scpi_prefix, scpi_val));
            Some(cmds)
        }
    }

    fn hold_command(&self, on: bool) -> Option<String> {
        if on {
            Some("MULT:HOLD ON\n".to_string())
        } else {
            Some("MULT:HOLD OFF\n".to_string())
        }
    }

    // ─── Power Supply Commands ──────────────────────────────────

    fn ps_output_command(&self, on: bool) -> Option<String> {
        Some(if on { "OUTP ON\n".to_string() } else { "OUTP OFF\n".to_string() })
    }

    fn ps_query_output(&self) -> Option<&str> { Some("OUTP?\n") }

    fn ps_set_voltage(&self, v: f64) -> Option<String> {
        Some(format!("VOLT {:.3}\n", v))
    }

    fn ps_set_current(&self, a: f64) -> Option<String> {
        Some(format!("CURR {:.3}\n", a))
    }

    fn ps_set_ovp(&self, v: f64) -> Option<String> {
        Some(format!("VOLT:LIM {:.3}\n", v))
    }

    fn ps_set_ocp(&self, a: f64) -> Option<String> {
        Some(format!("CURR:LIM {:.3}\n", a))
    }

    fn ps_query_voltage(&self)  -> Option<&str> { Some("VOLT?\n") }
    fn ps_query_current(&self)  -> Option<&str> { Some("CURR?\n") }
    fn ps_query_ovp(&self)      -> Option<&str> { Some("VOLT:LIM?\n") }
    fn ps_query_ocp(&self)      -> Option<&str> { Some("CURR:LIM?\n") }
    fn ps_query_meas_all(&self) -> Option<&str> { Some("MEAS:ALL?\n") }

    // ─── Response Parsing ───────────────────────────────────────

    fn parse_measurement(&self, response: &str) -> ParseResult {
        let trimmed = response.trim();

        // SPM6103 uses comma-separated format from CONFigure:ALL?
        if !trimmed.contains(',') {
            return ParseResult { measurement: None, mode: None, range_index: None, precision: None };
        }

        // Format: "TYPE,VALUE,STATUS,RANGE"
        // e.g. "VOLT:DC,+0.0011V,AUTO,2V"  or  "RES,OL,AUTO,100M Ohm"
        let parts: Vec<&str> = trimmed.split(',').collect();
        if parts.len() < 2 {
            return ParseResult { measurement: None, mode: None, range_index: None, precision: None };
        }

        // ── Detect mode from TYPE field ──
        let mode_type = parts[0].trim();
        let detected_mode = match mode_type {
            "VOLT:DC" | "VOLT" => Some(MeterMode::Vdc),
            "VOLT:AC"          => Some(MeterMode::Vac),
            "CURR:DC" | "CURR" => Some(MeterMode::Adc),
            "CURR:AC"          => Some(MeterMode::Aac),
            "RES"              => Some(MeterMode::Res),
            "CAP"              => Some(MeterMode::Cap),
            "FREQ"             => Some(MeterMode::Freq),
            "PER"              => Some(MeterMode::Per),
            "DIOD"             => Some(MeterMode::Diod),
            "CONT"             => Some(MeterMode::Cont),
            "TEMP"             => Some(MeterMode::Temp),
            _                  => None,
        };

        // ── Extract numeric value ──
        let value_str = parts[1].trim();
        let (measurement, precision) = if value_str == "OL" || value_str.starts_with("OL") || value_str == "Open" {
            (Some(1e9), None)
        } else {
            // Strip spaces: "+0.0007 KOhm" → "+0.0007KOhm"
            let value_clean: String = value_str.chars().filter(|c| *c != ' ').collect();
            let numeric_part = value_clean.trim_end_matches(|c: char| c.is_alphabetic());
            let unit_suffix = &value_clean[numeric_part.len()..];
            let multiplier = extract_si_multiplier(unit_suffix);
            let meas = numeric_part.parse::<f64>().ok().map(|v| v * multiplier);
            let raw = count_decimals(numeric_part);
            let prec = Some(10_f64.powi(-(raw as i32)) * multiplier);
            (meas, prec)
        };

        // ── Extract range index ──
        let range_index = if parts.len() >= 3 && detected_mode.is_some() {
            let status = parts[2].trim();
            let mode = detected_mode.unwrap();

            let has_auto = self.range_info(mode)
                .map(|ri| ri.ranges.first() == Some(&"auto"))
                .unwrap_or(false);

            if status.eq_ignore_ascii_case("AUTO") && has_auto {
                Some(0)
            } else if parts.len() >= 4 {
                let range_str = parts[3].trim();
                if !range_str.is_empty() {
                    self.parse_range_response(range_str, mode)
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        ParseResult { measurement, mode: detected_mode, range_index, precision }
    }

    fn parse_range_response(&self, response: &str, mode: MeterMode) -> Option<usize> {
        let trimmed = response.trim();
        let info = self.internal_range_info(mode)?;

        // Check for AUTO
        if trimmed.eq_ignore_ascii_case("AUTO") {
            if info.ranges.first().map(|(n, _)| *n == "auto").unwrap_or(false) {
                return Some(0);
            }
        }

        // Normalize: remove spaces and lowercase
        let normalized = trimmed.replace(' ', "").to_lowercase();

        // Try matching against display names
        for (idx, (display, _)) in info.ranges.iter().enumerate() {
            if normalized == display.to_lowercase() {
                return Some(idx);
            }
        }

        // Parse numeric part with SI prefix handling
        let numeric_part = normalized.trim_end_matches(|c: char| c.is_alphabetic());
        if let Some(multiplier_char) = normalized.chars().rev()
            .find(|c| c.is_alphabetic() && (*c == 'k' || *c == 'm' || *c == 'n' || *c == 'u'))
        {
            let base_num = numeric_part.trim_end_matches(|c: char| !c.is_numeric() && c != '.' && c != '-' && c != '+');
            if let Ok(mut val) = base_num.parse::<f64>() {
                val *= match multiplier_char {
                    'k' => 1000.0,
                    'M' | 'm' if normalized.contains("ohm") || normalized.contains('v') || normalized.contains('a') => {
                        if val < 10.0 { 1_000_000.0 } else { 0.001 }
                    },
                    'm' => 0.001,
                    'u' => 0.000_001,
                    'n' => 0.000_000_001,
                    _ => 1.0,
                };
                for (idx, (_, scpi_val)) in info.ranges.iter().enumerate() {
                    if let Ok(expected) = scpi_val.parse::<f64>() {
                        if (val - expected).abs() < expected.abs() * 0.01 {
                            return Some(idx);
                        }
                    }
                }
            }
        } else if let Ok(val) = numeric_part.parse::<f64>() {
            for (idx, (_, scpi_val)) in info.ranges.iter().enumerate() {
                if let Ok(expected) = scpi_val.parse::<f64>() {
                    if (val - expected).abs() < 0.1 {
                        return Some(idx);
                    }
                }
            }
        }

        // Exact string match as fallback
        for (idx, (_, scpi_val)) in info.ranges.iter().enumerate() {
            if scpi_val.eq_ignore_ascii_case(trimmed) {
                return Some(idx);
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
