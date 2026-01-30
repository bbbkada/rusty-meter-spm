use phf::{OrderedMap, phf_ordered_map};
use crate::device_plugin::{DevicePlugin, Xdm1041Plugin, Spm6103Plugin};

/// Device types supported by the application
#[derive(PartialEq, Clone, Copy, Debug)]
pub enum DeviceType {
    /// OWON XDM1041/XDM1241 Benchtop Multimeter
    OwonXdm1041,
    /// OWON SPM6103 Power Supply with integrated Multimeter
    OwonSpm6103,
    /// Unknown/unsupported device (fallback to XDM1041 commands)
    Unknown,
}

impl DeviceType {
    /// Detect device type from *IDN? response
    /// Example responses:
    /// - "OWON,XDM1041,12345678,V1.2.3"
    /// - "OWON,SPM6103,25281747,FV:V2.1.0"
    /// Supports wildcard matching: XDM* defaults to XDM1041, SPM* defaults to SPM6103
    pub fn from_idn(idn: &str) -> Self {
        let parts: Vec<&str> = idn.split(',').collect();
        if parts.len() >= 2 && parts[0] == "OWON" {
            let model = parts[1];
            match model {
                "XDM1041" | "XDM1241" => DeviceType::OwonXdm1041,
                "SPM6103" => DeviceType::OwonSpm6103,
                _ => {
                    // Wildcard matching
                    if model.starts_with("XDM") {
                        DeviceType::OwonXdm1041
                    } else if model.starts_with("SPM") {
                        DeviceType::OwonSpm6103
                    } else {
                        DeviceType::Unknown
                    }
                }
            }
        } else {
            DeviceType::Unknown
        }
    }

    /// Get the plugin implementation for this device type
    pub fn plugin(&self) -> Box<dyn DevicePlugin> {
        match self {
            DeviceType::OwonXdm1041 | DeviceType::Unknown => Box::new(Xdm1041Plugin),
            DeviceType::OwonSpm6103 => Box::new(Spm6103Plugin),
        }
    }

    /// Get the measurement query command for this device
    pub fn meas_cmd(&self) -> &'static str {
        match self {
            DeviceType::OwonXdm1041 | DeviceType::Unknown => "MEAS?\n",
            DeviceType::OwonSpm6103 => "CONFigure:ALL?\n",
        }
    }

    /// Get the function query command for this device
    pub fn func_cmd(&self) -> &'static str {
        match self {
            DeviceType::OwonXdm1041 | DeviceType::Unknown => "FUNC?\n",
            DeviceType::OwonSpm6103 => "FUNC?\n",
        }
    }

    /// Get the command to set a specific meter mode
    /// Returns the appropriate command string for the device type
    pub fn mode_cmd(&self, mode: MeterMode) -> String {
        self.plugin().mode_command(mode)
    }
    
    /// Check if this device supports a specific mode
    pub fn supports_mode(&self, mode: MeterMode) -> bool {
        self.plugin().supports_mode(mode)
    }
    
    /// Check if this device supports sampling rate control
    pub fn supports_rate_control(&self) -> bool {
        self.plugin().supports_rate_control()
    }
}

/// A trait that must be implemented for all SCPI command structs.
/// Gets passed the struct instance itself and the selected option name
/// and must return a complete SCPI command string (including newline)
/// that can be sent via serial or LXI to the target device.
pub trait GenScpi {
    fn gen_scpi(&self, opt_name: &str) -> String;
}

#[derive(PartialEq, Clone, Copy, Debug)]
pub enum ScpiMode {
    Idn,
    Meas,
}

#[derive(PartialEq, Eq, Hash, Clone, Copy, Debug)]
pub enum MeterMode {
    Vdc,
    Vac,
    Adc,
    Aac,
    Res,
    Cap,
    Freq,
    Per,
    Diod,
    Cont,
    Temp,
}

pub struct RateCmd {
    scpi: &'static str,
    pub opts: OrderedMap<&'static str, &'static str>,
}

impl Default for RateCmd {
    // this corresponds to OWON XDM1041
    fn default() -> Self {
        Self {
            scpi: "RATE ",
            opts: phf_ordered_map! {
                "Slow" => "S",
                "Medium" => "M",
                "Fast" => "F",
            },
        }
    }
}

impl GenScpi for RateCmd {
    fn gen_scpi(&self, opt_name: &str) -> String {
        format!("{}{}\n", self.scpi, self.opts[opt_name])
    }
}

impl RateCmd {
    pub fn get_opt(&self, index: usize) -> (&'static str, &'static str) {
        let (key, value) = self.opts.index(index).unwrap();
        (*key, *value)
    }

    pub fn len(&self) -> usize {
        self.opts.len()
    }
}
