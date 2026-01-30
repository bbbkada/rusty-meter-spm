use crate::multimeter::MeterMode;

/// Result of parsing a device response
#[derive(Debug)]
pub struct ParseResult {
    pub measurement: Option<f64>,
    pub mode: Option<MeterMode>,
    pub range_index: Option<usize>, // Index into RangeInfo.ranges for the current mode
}

/// Range information for a specific mode
#[derive(Clone, Debug)]
pub struct RangeInfo {
    pub scpi_prefix: &'static str,
    pub ranges: Vec<(&'static str, &'static str)>, // (display_name, scpi_value)
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
            // Extract mode string (before space or full string)
            let mode_str = unquoted.split_whitespace().next().unwrap_or(unquoted);
            
            let mode = match mode_str {
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
                    // Also try matching "VOLT AC" explicitly for XDM1041
                    if unquoted == "VOLT AC" {
                        MeterMode::Vac
                    } else if unquoted == "CURR AC" {
                        MeterMode::Aac
                    } else {
                        return ParseResult { measurement: None, mode: None, range_index: None };
                    }
                }
            };
            
            return ParseResult { measurement: None, mode: Some(mode), range_index: None };
        }
        
        // Try parsing direct measurement value (format: "+1.234")
        if let Ok(meas) = trimmed.parse::<f64>() {
            return ParseResult { measurement: Some(meas), mode: None, range_index: None };
        }
        
        ParseResult { measurement: None, mode: None, range_index: None }
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
}

/// Plugin for OWON SPM6103 power supply with integrated multimeter
pub struct Spm6103Plugin;

impl DevicePlugin for Spm6103Plugin {
    fn parse_measurement(&self, response: &str, swap_diod_cont: bool) -> ParseResult {
        let trimmed = response.trim();
        
        // SPM6103 uses comma-separated format
        if !trimmed.contains(',') {
            return ParseResult { measurement: None, mode: None, range_index: None };
        }
        
        // CONFigure:ALL? format: "VOLT:DC,+0.0011V,AUTO,2V"
        // or "RES,+000.26Ohm,AUTO,200Ohm" or "RES,OL,AUTO,100M Ohm"
        // or "DIOD,Open,Manual,"
        // Format is "TYPE,VALUE,STATUS,RANGE"
        let parts: Vec<&str> = trimmed.split(',').collect();
        if parts.len() < 2 {
            return ParseResult { measurement: None, mode: None, range_index: None };
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
            // Remove all trailing alphabetic characters (units like V, A, mA, Ohm, etc.)
            let numeric_part = value_str.trim_end_matches(|c: char| c.is_alphabetic());
            numeric_part.parse::<f64>().ok()
        };
        
        // Extract range - check STATUS field (3rd) first for AUTO/Manual
        let range_index = if parts.len() >= 3 && detected_mode.is_some() {
            let status = parts[2].trim();
            
            // If status is AUTO, range index should be 0 (auto)
            if status.eq_ignore_ascii_case("AUTO") {
                Some(0)
            } else if parts.len() >= 4 {
                // Manual mode - parse the actual range from 4th field
                let range_str = parts[3].trim();
                if !range_str.is_empty() {
                    let idx = self.parse_range_response(range_str, detected_mode.unwrap());
                    if idx.is_some() {
                        println!("DEBUG: parse_measurement extracted range '{}' -> index {:?}", range_str, idx);
                    } else {
                        println!("DEBUG: parse_measurement failed to parse range '{}'", range_str);
                    }
                    idx
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        
        ParseResult { measurement, mode: detected_mode, range_index }
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
                ranges: vec![
                    ("200mA", "200E-3"),
                    ("10A", "10"),
                ],
            }),
            MeterMode::Aac => Some(RangeInfo {
                scpi_prefix: "SENS:CURR:AC:RANG ",
                ranges: vec![
                    ("200mA", "200E-3"),
                    ("10A", "10"),
                ],
            }),
            MeterMode::Res => Some(RangeInfo {
                scpi_prefix: "SENS:RES:RANG ",
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
                ranges: vec![
                    ("auto", "AUTO"),
                    ("2nF", "2E-9"),
                    ("20nF", "20E-9"),
                    ("200nF", "200E-9"),
                    ("2uF", "2E-6"),
                    ("20uF", "20E-6"),
                    ("200uF", "200E-6"),
                    ("2mF", "2E-3"),
                    ("20mF", "20E-3"),
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
            // Check for AUTO first
            if trimmed.eq_ignore_ascii_case("AUTO") {
                return Some(0); // Auto is always index 0
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
}
