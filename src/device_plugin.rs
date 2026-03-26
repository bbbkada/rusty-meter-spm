use crate::multimeter::MeterMode;

/// Count the number of decimal places in a numeric string
fn count_decimals(s: &str) -> usize {
    // Strip leading sign
    let s = s.trim_start_matches(|c: char| c == '+' || c == '-');
    if let Some(dot_pos) = s.find('.') {
        // Count digits after the dot (stop at non-digit)
        s[dot_pos + 1..].chars().take_while(|c| c.is_ascii_digit()).count()
    } else {
        0
    }
}

/// Extract SI prefix multiplier from a unit suffix string.
/// e.g. "mV" -> 0.001, "kOhm" -> 1000, "KOhm" -> 1000, "nF" -> 1e-9, "V" -> 1.0, "MOhm" -> 1e6
fn extract_si_multiplier(unit_suffix: &str) -> f64 {
    if unit_suffix.is_empty() {
        return 1.0;
    }
    // Check first character for SI prefix
    match unit_suffix.chars().next().unwrap() {
        'n' => 1e-9,       // nano: nF
        'u' | 'μ' => 1e-6, // micro: uF, μF
        'm' => {
            // 'm' could be milli (mV, mA) or start of unit (MOhm is uppercase M)
            // Since unit_suffix comes after numeric part, "mV" = milli, "mA" = milli
            0.001
        }
        'k' | 'K' => 1e3,  // kilo: kOhm, KOhm — SPM6103 uses uppercase K (2KΩ, 20KΩ)
        'M' => {
            // Mega: MOhm (but not "mV" which starts lowercase)
            1e6
        }
        'G' => 1e9,        // Giga: GOhm, GHz
        _ => 1.0,          // No prefix, just base unit (V, A, Ohm, F, Hz, etc.)
    }
}

/// Result of parsing a device response
#[derive(Debug)]
pub struct ParseResult {
    pub measurement: Option<f64>,
    pub mode: Option<MeterMode>,
    pub range_index: Option<usize>, // Index into RangeInfo.ranges for the current mode
    pub decimals: Option<usize>,    // Number of decimal places in the raw measurement string
}

/// Range information for a specific mode
#[derive(Clone, Debug)]
pub struct RangeInfo {
    pub scpi_prefix: &'static str,
    pub ranges: Vec<(&'static str, &'static str)>, // (display_name, scpi_value)
    pub auto_on_cmd: Option<&'static str>,   // SCPI command to enable auto range (None = use scpi_prefix + "AUTO")
    pub auto_off_cmd: Option<&'static str>,  // SCPI command to disable auto range before setting fixed range
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

/// Pending GUI changes awaiting communication with the instrument.
/// Each field is Option<T>: None = no pending change, Some(v) = user wants this value.
/// Shared between UI thread and serial task via Arc<Mutex<>>.
///
/// Rules:
/// 1. GUI sets fields freely (latest value wins, no queuing)
/// 2. Serial task reads & clears at the start of each PS poll cycle, sends commands
/// 3. UI receiver skips instrument readback for any field that still has a pending change
/// 4. Fields are cleared by serial task ONLY after the command has been sent AND the
///    response cycle completes (so the next readback reflects the new value)
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PendingChanges {
    pub output_on: Option<bool>,
    pub voltage_set: Option<f64>,
    pub current_set: Option<f64>,
    pub ovp: Option<f64>,
    pub ocp: Option<f64>,
    pub range: Option<(usize, String)>,  // (range_index, full SCPI command string to send)
    pub mode: Option<(MeterMode, String, u32)>,  // (target mode, SCPI command, retries remaining)
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
    pub voltage_readback: f64, // Actual measured output voltage (MEAS:VOLT?)
    pub current_readback: f64, // Actual measured output current (MEAS:CURR?)
    pub power_readback: f64,   // Actual measured output power (MEAS:POW?)
    pub includes_settings: bool, // True only for full polls that queried VOLT?/CURR?/etc.
}

/// Trait for device-specific behavior
/// Each supported device implements this trait with its own parsing and command logic
pub trait DevicePlugin: Send + Sync {
    /// Parse a measurement response from the device
    /// Returns Some(value) if parsing succeeds, None if this response isn't a measurement
    fn parse_measurement(&self, response: &str, swap_diod_cont: bool) -> ParseResult;
    
    /// Generate the command to set a specific meter mode
    fn mode_command(&self, mode: MeterMode) -> String;
    
    /// Check if a specific mode is supported by this device
    fn supports_mode(&self, mode: MeterMode) -> bool;
    
    /// Check if this device supports sampling rate control
    fn supports_rate_control(&self) -> bool;
    
    /// Get range information for a specific mode (if supported)
    fn range_info(&self, mode: MeterMode) -> Option<RangeInfo>;
    
    /// Check if this device supports beeper control (SYST:BEEP:STATe)
    fn supports_beeper(&self) -> bool;
    
    /// Check if this device supports threshold settings (CONT:THREshold, DIOD:THREshold)
    fn supports_threshold(&self) -> bool;
    
    /// Check if this device supports hardware hold (MULT:HOLD command)
    fn supports_hold(&self) -> bool;
    
    /// Generate the query command to read current range setting for a mode
    fn range_query_command(&self, mode: MeterMode) -> Option<String>;
    
    /// Parse range query response and return the matching range index
    /// Returns None if response cannot be parsed or doesn't match any known range
    fn parse_range_response(&self, response: &str, mode: MeterMode) -> Option<usize>;
    
    /// Check if this device has an integrated power supply
    fn supports_power_supply(&self) -> bool;
    
    /// Get power supply limits (voltage/current min/max, OVP/OCP max)
    fn power_supply_limits(&self) -> Option<PowerSupplyLimits>;
}

/// Plugin for OWON XDM1041/XDM1241 multimeters
pub struct Xdm1041Plugin;

impl DevicePlugin for Xdm1041Plugin {
    fn parse_measurement(&self, response: &str, swap_diod_cont: bool) -> ParseResult {
        let trimmed = response.trim();
        
        // Handle quoted function responses from FUNC?
        let unquoted = trimmed.trim_matches('"');
        if unquoted.starts_with("VOLT") || unquoted.starts_with("CURR") ||
           unquoted == "FREQ" || unquoted == "PER" ||
           unquoted == "CAP" || unquoted == "CONT" ||
           unquoted == "DIOD" || unquoted == "RES" ||
           unquoted == "TEMP"
        {
            // Check for space-separated AC/DC modes FIRST (e.g. "VOLT AC", "CURR AC").
            // XDM1041 FUNC? can return "VOLT AC" (space-separated). Splitting by
            // whitespace would yield "VOLT" which incorrectly maps to VDC.
            let mode = if unquoted == "VOLT AC" || unquoted.starts_with("VOLT AC ") {
                MeterMode::Vac
            } else if unquoted == "CURR AC" || unquoted.starts_with("CURR AC ") {
                MeterMode::Aac
            } else {
                // Extract mode string (before space or full string)
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
                    // Handle DIOD/CONT based on firmware version
                    "DIOD" => if swap_diod_cont { MeterMode::Cont } else { MeterMode::Diod },
                    "CONT" => if swap_diod_cont { MeterMode::Diod } else { MeterMode::Cont },
                    _ => {
                        return ParseResult { measurement: None, mode: None, range_index: None, decimals: None };
                    }
                }
            };
            
            return ParseResult { measurement: None, mode: Some(mode), range_index: None, decimals: None };
        }
        
        // Try parsing direct measurement value (format: "+1.234")
        if let Ok(meas) = trimmed.parse::<f64>() {
            let decimals = count_decimals(trimmed);
            return ParseResult { measurement: Some(meas), mode: None, range_index: None, decimals: Some(decimals) };
        }
        
        ParseResult { measurement: None, mode: None, range_index: None, decimals: None }
    }
    
    fn mode_command(&self, mode: MeterMode) -> String {
        // XDM1041 uses CONF:* commands with AUTO
        match mode {
            MeterMode::Vdc => "CONF:VOLT:DC AUTO\n".to_string(),
            MeterMode::Vac => "CONF:VOLT:AC AUTO\n".to_string(),
            MeterMode::Adc => "FUNC:CURR:DC\n".to_string(),
            MeterMode::Aac => "FUNC:CURR:AC\n".to_string(),
            MeterMode::Res => "CONF:RES AUTO\n".to_string(),
            MeterMode::Cap => "CONF:CAP AUTO\n".to_string(),
            MeterMode::Freq => "CONF:FREQ\n".to_string(),
            MeterMode::Per => "CONF:PER\n".to_string(),
            MeterMode::Diod => "CONF:DIOD\n".to_string(),
            MeterMode::Cont => "CONF:CONT\n".to_string(),
            MeterMode::Temp => "CONF:TEMP:RTD PT100\n".to_string(),
        }
    }
    
    fn supports_mode(&self, _mode: MeterMode) -> bool {
        // XDM1041 supports all modes
        true
    }
    
    fn supports_rate_control(&self) -> bool {
        // XDM1041 supports rate control (RATE command)
        true
    }
    
    fn range_info(&self, mode: MeterMode) -> Option<RangeInfo> {
        match mode {
            MeterMode::Vdc => Some(RangeInfo {
                scpi_prefix: "CONF:VOLT:DC ",
                auto_on_cmd: None,  // XDM1041: "CONF:VOLT:DC AUTO" works
                auto_off_cmd: None,
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
            MeterMode::Vac => Some(RangeInfo {
                scpi_prefix: "CONF:VOLT:AC ",
                auto_on_cmd: None,
                auto_off_cmd: None,
                ranges: vec![
                    ("auto", "AUTO"),
                    ("500mV", "500E-3"),
                    ("5V", "5"),
                    ("50V", "50"),
                    ("500V", "500"),
                    ("750V", "750"),
                ],
            }),
            MeterMode::Adc => Some(RangeInfo {
                scpi_prefix: "CONF:CURR:DC ",
                auto_on_cmd: None,
                auto_off_cmd: None,
                ranges: vec![
                    ("500uA", "500E-6"),
                    ("5mA", "5E-3"),
                    ("50mA", "50E-3"),
                    ("500mA", "500E-3"),
                    ("5A", "5"),
                    ("10A", "10"),
                ],
            }),
            MeterMode::Aac => Some(RangeInfo {
                scpi_prefix: "CONF:CURR:AC ",
                auto_on_cmd: None,
                auto_off_cmd: None,
                ranges: vec![
                    ("500uA", "500E-6"),
                    ("5mA", "5E-3"),
                    ("50mA", "50E-3"),
                    ("500mA", "500E-3"),
                    ("5A", "5"),
                    ("10A", "10"),
                ],
            }),
            MeterMode::Res => Some(RangeInfo {
                scpi_prefix: "CONF:RES ",
                auto_on_cmd: None,
                auto_off_cmd: None,
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
            MeterMode::Cap => Some(RangeInfo {
                scpi_prefix: "CONF:CAP ",
                auto_on_cmd: None,
                auto_off_cmd: None,
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
            MeterMode::Temp => Some(RangeInfo {
                scpi_prefix: "CONF:TEMP:RTD ",
                auto_on_cmd: None,
                auto_off_cmd: None,
                ranges: vec![
                    ("PT100", "PT100"),
                    ("K-type (KITS90)", "KITS90"),
                ],
            }),
            _ => None,
        }
    }
    
    fn supports_beeper(&self) -> bool {
        // XDM1041 supports beeper control via SYST:BEEP:STATe
        true
    }
    
    fn supports_threshold(&self) -> bool {
        // XDM1041 supports threshold settings via CONT:THREshold and DIOD:THREshold
        true
    }
    
    fn supports_hold(&self) -> bool {
        // XDM1041 does not support hardware hold
        false
    }
    
    fn range_query_command(&self, mode: MeterMode) -> Option<String> {
        // XDM1041 uses CONF:*? to query configuration
        match mode {
            MeterMode::Vdc => Some("CONF:VOLT:DC?\n".to_string()),
            MeterMode::Vac => Some("CONF:VOLT:AC?\n".to_string()),
            MeterMode::Adc => Some("CONF:CURR:DC?\n".to_string()),
            MeterMode::Aac => Some("CONF:CURR:AC?\n".to_string()),
            MeterMode::Res => Some("CONF:RES?\n".to_string()),
            MeterMode::Cap => Some("CONF:CAP?\n".to_string()),
            MeterMode::Temp => Some("CONF:TEMP?\n".to_string()),
            _ => None, // Freq, Per, Diod, Cont don't have ranges
        }
    }
    
    fn parse_range_response(&self, response: &str, mode: MeterMode) -> Option<usize> {
        // XDM1041 returns quoted responses like "VOLT:DC 5" or "VOLT:DC AUTO"
        // SPM6103 returns "200 Ohm" or "1000V" directly from CONFigure:ALL? 4th field
        let trimmed = response.trim().trim_matches('"');
        
        // Normalize by removing spaces for matching
        let normalized_response = trimmed.replace(" ", "");
        
        println!("DEBUG parse_range_response: input='{}', normalized='{}', mode={:?}", 
                 trimmed, normalized_response, mode);
        
        // Get range info for this mode
        if let Some(range_info) = self.range_info(mode) {
            println!("DEBUG: Checking {} ranges for {:?}", range_info.ranges.len(), mode);
            
            // First try matching against SCPI value
            for (idx, (display, scpi_val)) in range_info.ranges.iter().enumerate() {
                if scpi_val.eq_ignore_ascii_case(&normalized_response) {
                    println!("DEBUG: Matched SCPI value '{}' at index {}", scpi_val, idx);
                    return Some(idx);
                }
            }
            
            // If no match, try matching against display name
            for (idx, (display, scpi_val)) in range_info.ranges.iter().enumerate() {
                let normalized_display = display.replace(" ", "");
                if normalized_display.eq_ignore_ascii_case(&normalized_response) {
                    println!("DEBUG: Matched display '{}' (normalized '{}') at index {}", display, normalized_display, idx);
                    return Some(idx);
                }
            }
            
            // For XDM1041: Extract the range value (last part after space) and try again
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if parts.len() > 1 {
                let range_val = parts.last()?;
                
                // Try matching just the value part
                for (idx, (_display, scpi_val)) in range_info.ranges.iter().enumerate() {
                    if scpi_val.eq_ignore_ascii_case(range_val) {
                        println!("DEBUG: Matched XDM-style range_val '{}' at index {}", range_val, idx);
                        return Some(idx);
                    }
                }
            }
            
            println!("DEBUG: No match found for '{}'", normalized_response);
        } else {
            println!("DEBUG: No range_info for mode {:?}", mode);
        }
        
        None
    }
    
    fn supports_power_supply(&self) -> bool {
        false
    }
    
    fn power_supply_limits(&self) -> Option<PowerSupplyLimits> {
        None
    }
}

/// Plugin for OWON SPM6103 power supply with integrated multimeter
pub struct Spm6103Plugin;

impl DevicePlugin for Spm6103Plugin {
    fn parse_measurement(&self, response: &str, swap_diod_cont: bool) -> ParseResult {
        let trimmed = response.trim();
        
        // SPM6103 uses comma-separated format
        if !trimmed.contains(',') {
            return ParseResult { measurement: None, mode: None, range_index: None, decimals: None };
        }
        
        // CONFigure:ALL? format: "VOLT:DC,+0.0011V,AUTO,2V"
        // or "RES,+000.26Ohm,AUTO,200Ohm" or "RES,OL,AUTO,100M Ohm"
        // or "DIOD,Open,Manual,"
        // Format is "TYPE,VALUE,STATUS,RANGE"
        let parts: Vec<&str> = trimmed.split(',').collect();
        if parts.len() < 2 {
            return ParseResult { measurement: None, mode: None, range_index: None, decimals: None };
        }
        
        // Detect mode from the TYPE field
        let mode_type = parts[0].trim();
        let detected_mode = match mode_type {
            "VOLT:DC" | "VOLT" => Some(MeterMode::Vdc),
            "VOLT:AC" => Some(MeterMode::Vac),
            "CURR:DC" | "CURR" => Some(MeterMode::Adc),
            "CURR:AC" => Some(MeterMode::Aac),
            "RES" => Some(MeterMode::Res),
            "CAP" => Some(MeterMode::Cap),
            "FREQ" => Some(MeterMode::Freq),
            "PER" => Some(MeterMode::Per),
            "DIOD" => Some(if swap_diod_cont { MeterMode::Cont } else { MeterMode::Diod }),
            "CONT" => Some(if swap_diod_cont { MeterMode::Diod } else { MeterMode::Cont }),
            "TEMP" => Some(MeterMode::Temp),
            _ => None,
        };
        
        // Extract numeric value
        let value_str = parts[1].trim();
        
        // Handle special values
        let measurement = if value_str == "OL" || value_str.starts_with("OL") || value_str == "Open" {
            // Overload/Open Load
            Some(1e9)
        } else {
            // Strip spaces so formats like "+0.0007 KOhm" or "+0.0007K Ohm" become "+0.0007KOhm"
            let value_clean: String = value_str.chars().filter(|c| *c != ' ').collect();
            // Extract unit suffix and apply SI prefix to convert to base unit.
            // e.g. "+20.44mV" -> numeric=20.44, unit="mV", multiplier=0.001 -> 0.02044 V
            // This ensures graphs and recordings always use base units.
            let numeric_part = value_clean.trim_end_matches(|c: char| c.is_alphabetic());
            let unit_suffix = &value_clean[numeric_part.len()..];
            let multiplier = extract_si_multiplier(unit_suffix);
            numeric_part.parse::<f64>().ok().map(|v| v * multiplier)
        };
        
        // Count decimals, adjusting for SI prefix conversion.
        // e.g. "+20.44mV" has 2 raw decimals, but after milli conversion (0.02044) needs 5 decimals to preserve precision.
        let decimals = {
            let value_clean: String = value_str.chars().filter(|c| *c != ' ').collect();
            let numeric_part = value_clean.trim_end_matches(|c: char| c.is_alphabetic());
            let unit_suffix = &value_clean[numeric_part.len()..];
            let raw = count_decimals(numeric_part);
            let si_extra = match extract_si_multiplier(unit_suffix) {
                m if m <= 1e-9 + f64::EPSILON && m >= 1e-9 - f64::EPSILON => 9,  // nano
                m if m <= 1e-6 + f64::EPSILON && m >= 1e-6 - f64::EPSILON => 6,  // micro
                m if (m - 0.001).abs() < f64::EPSILON => 3,                       // milli
                m if (m - 1000.0).abs() < f64::EPSILON => 0,                      // kilo (fewer decimals ok)
                m if (m - 1e6).abs() < f64::EPSILON => 0,                         // mega
                _ => 0,                                                           // no prefix
            };
            Some(raw + si_extra)
        };
        
        // Extract range - check STATUS field (3rd) first for AUTO/Manual
        let range_index = if parts.len() >= 3 && detected_mode.is_some() {
            let status = parts[2].trim();
            let mode = detected_mode.unwrap();
            
            // Check if this mode has an "auto" entry in its range list
            let has_auto = self.range_info(mode)
                .map(|ri| ri.ranges.first().map(|(name, _)| *name == "auto").unwrap_or(false))
                .unwrap_or(false);
            
            if status.eq_ignore_ascii_case("AUTO") && has_auto {
                // Auto mode and mode supports it — index 0 is auto
                Some(0)
            } else if parts.len() >= 4 {
                // Manual mode, or auto mode without auto entry — parse range value
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
        
        ParseResult { measurement, mode: detected_mode, range_index, decimals }
    }
    
    fn mode_command(&self, mode: MeterMode) -> String {
        // SPM6103 uses FUNC:* commands
        match mode {
            MeterMode::Vdc => "FUNC:VOLT:DC\n".to_string(),
            MeterMode::Vac => "FUNC:VOLT:AC\n".to_string(),
            MeterMode::Adc => "FUNC:CURR:DC\n".to_string(),
            MeterMode::Aac => "FUNC:CURR:AC\n".to_string(),
            MeterMode::Res => "FUNC:RES\n".to_string(),
            MeterMode::Cap => "FUNC:CAP\n".to_string(),
            MeterMode::Freq => "FUNC:FREQ\n".to_string(),
            MeterMode::Per => "FUNC:PER\n".to_string(),
            MeterMode::Diod => "FUNC:DIOD\n".to_string(),
            MeterMode::Cont => "FUNC:CONT\n".to_string(),
            MeterMode::Temp => "FUNC:TEMP\n".to_string(),
        }
    }
    
    fn supports_mode(&self, mode: MeterMode) -> bool {
        // SPM6103 does not support Frequency, Period and Temperature modes
        !matches!(mode, MeterMode::Freq | MeterMode::Per | MeterMode::Temp)
    }
    
    fn supports_rate_control(&self) -> bool {
        // SPM6103 does not support rate control
        false
    }
    
    fn range_info(&self, mode: MeterMode) -> Option<RangeInfo> {
        match mode {
            MeterMode::Vdc => Some(RangeInfo {
                scpi_prefix: "SENS:VOLT:DC:RANG ",
                auto_on_cmd: Some("VOLT:DC:RANG:AUTO ON\n"),
                auto_off_cmd: Some("VOLT:DC:RANG:AUTO OFF\n"),
                ranges: vec![
                    ("auto", "AUTO"),
                    ("200mV", "200E-3"),
                    ("2V", "2"),
                    ("20V", "20"),
                    ("200V", "200"),
                    ("1000V", "1000"),
                ],
            }),
            MeterMode::Vac => Some(RangeInfo {
                scpi_prefix: "SENS:VOLT:AC:RANG ",
                auto_on_cmd: Some("VOLT:AC:RANG:AUTO ON\n"),
                auto_off_cmd: Some("VOLT:AC:RANG:AUTO OFF\n"),
                ranges: vec![
                    ("auto", "AUTO"),
                    ("200mV", "200E-3"),
                    ("2V", "2"),
                    ("20V", "20"),
                    ("200V", "200"),
                    ("750V", "750"),
                ],
            }),
            MeterMode::Adc => Some(RangeInfo {
                scpi_prefix: "SENS:CURR:DC:RANG ",
                auto_on_cmd: None,  // No auto for current on SPM6103
                auto_off_cmd: None,
                ranges: vec![
                    ("200mA", "200E-3"),
                    ("10A", "10"),
                ],
            }),
            MeterMode::Aac => Some(RangeInfo {
                scpi_prefix: "SENS:CURR:AC:RANG ",
                auto_on_cmd: None,
                auto_off_cmd: None,
                ranges: vec![
                    ("200mA", "200E-3"),
                    ("10A", "10"),
                ],
            }),
            MeterMode::Res => Some(RangeInfo {
                scpi_prefix: "SENS:RES:RANG ",
                auto_on_cmd: Some("RES:RANG:AUTO ON\n"),
                auto_off_cmd: Some("RES:RANG:AUTO OFF\n"),
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
            MeterMode::Cap => Some(RangeInfo {
                scpi_prefix: "SENS:CAP:RANG ",
                auto_on_cmd: None,
                auto_off_cmd: None,
                ranges: vec![
                    ("auto", "AUTO"),
                ],
            }),
            _ => None,
        }
    }
    
    fn supports_beeper(&self) -> bool {
        // SPM6103 does NOT support beeper control
        false
    }
    
    fn supports_threshold(&self) -> bool {
        // SPM6103 does NOT support threshold settings
        false
    }
    
    fn supports_hold(&self) -> bool {
        // SPM6103 supports hardware hold (MULT:HOLD command)
        true
    }
    
    fn range_query_command(&self, mode: MeterMode) -> Option<String> {
        // SPM6103 uses SENS:*:RANG? to query range
        match mode {
            MeterMode::Vdc => Some("VOLT:DC:RANG?\n".to_string()),
            MeterMode::Vac => Some("VOLT:AC:RANG?\n".to_string()),
            MeterMode::Adc => Some("CURR:DC:RANG?\n".to_string()),
            MeterMode::Aac => Some("CURR:AC:RANG?\n".to_string()),
            MeterMode::Res => Some("RES:RANG?\n".to_string()),
            MeterMode::Cap => Some("CAP:RANG?\n".to_string()),
            _ => None, // Freq, Per, Diod, Cont, Temp don't have ranges or not supported
        }
    }
    
    fn parse_range_response(&self, response: &str, mode: MeterMode) -> Option<usize> {
        // SPM6103 returns values like "+2.00000000E+00", "AUTO", "200 Ohm", "20k Ohm", "2M Ohm"
        let trimmed = response.trim();
        
        // Get range info for this mode
        if let Some(range_info) = self.range_info(mode) {
            // Check for AUTO — only return index 0 if the mode actually has "auto" in its range list
            if trimmed.eq_ignore_ascii_case("AUTO") {
                if range_info.ranges.first().map(|(name, _)| *name == "auto").unwrap_or(false) {
                    return Some(0);
                }
                // Mode has no auto entry — fall through to try matching as range value
            }
            
            // Normalize response: remove spaces and convert to lowercase for matching
            // "20k Ohm" -> "20kohm", "2 V" -> "2v"
            let normalized = trimmed.replace(" ", "").to_lowercase();
            
            // Try matching against display names (e.g., "200Ohm", "2kOhm", "20kOhm")
            for (idx, (display, _scpi_val)) in range_info.ranges.iter().enumerate() {
                let display_normalized = display.to_lowercase();
                if normalized == display_normalized {
                    return Some(idx);
                }
            }
            
            // Parse numeric part with SI prefix handling
            // Extract number and multiplier: "20k" -> 20 * 1000, "2M" -> 2 * 1000000
            let numeric_part = normalized
                .trim_end_matches(|c: char| c.is_alphabetic());
            
            if let Some(multiplier_char) = normalized.chars().rev()
                .find(|c| c.is_alphabetic() && (*c == 'k' || *c == 'm' || *c == 'n' || *c == 'u')) {
                
                let base_num = numeric_part.trim_end_matches(|c: char| !c.is_numeric() && c != '.' && c != '-' && c != '+');
                
                if let Ok(mut val) = base_num.parse::<f64>() {
                    // Apply SI multiplier
                    val *= match multiplier_char {
                        'k' => 1000.0,
                        'M' | 'm' if normalized.contains("ohm") || normalized.contains("v") || normalized.contains("a") => {
                            // Check context: "mV" is milli, "MOhm" is mega
                            if val < 10.0 { 1_000_000.0 } else { 0.001 }
                        },
                        'm' => 0.001,
                        'u' => 0.000_001,
                        'n' => 0.000_000_001,
                        _ => 1.0,
                    };
                    
                    // Find matching range by value comparison
                    for (idx, (_display, scpi_val)) in range_info.ranges.iter().enumerate() {
                        if let Ok(expected_val) = scpi_val.parse::<f64>() {
                            if (val - expected_val).abs() < expected_val * 0.01 {
                                return Some(idx);
                            }
                        }
                    }
                }
            } else if let Ok(val) = numeric_part.parse::<f64>() {
                // No SI prefix, direct comparison
                for (idx, (_display, scpi_val)) in range_info.ranges.iter().enumerate() {
                    if let Ok(expected_val) = scpi_val.parse::<f64>() {
                        if (val - expected_val).abs() < 0.1 {
                            return Some(idx);
                        }
                    }
                }
            }
            
            // Try exact string match as fallback
            for (idx, (_display, scpi_val)) in range_info.ranges.iter().enumerate() {
                if scpi_val.eq_ignore_ascii_case(trimmed) {
                    return Some(idx);
                }
            }
        }
        
        None
    }
    
    fn supports_power_supply(&self) -> bool {
        true
    }
    
    fn power_supply_limits(&self) -> Option<PowerSupplyLimits> {
        Some(PowerSupplyLimits {
            voltage_min: 0.0,
            voltage_max: 60.0,   // SPM6103: 0-60V
            current_min: 0.0,
            current_max: 3.2,    // SPM6103: 0-3.2A (approximation, depends on model)
            ovp_max: 63.0,
            ocp_max: 3.5,
        })
    }
}
