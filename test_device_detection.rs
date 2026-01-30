// Simple test program to verify device detection logic
mod multimeter;
use multimeter::DeviceType;

fn main() {
    // Test XDM1041 detection
    let xdm_response = "OWON,XDM1041,12345678,V4.3.0";
    let xdm_type = DeviceType::from_idn(xdm_response);
    println!("XDM1041 response: {}", xdm_response);
    println!("Detected type: {:?}", xdm_type);
    println!("Measurement command: {}", xdm_type.meas_cmd());
    println!();

    // Test SPM6103 detection
    let spm_response = "OWON,SPM6103,25281747,FV:V2.1.0";
    let spm_type = DeviceType::from_idn(spm_response);
    println!("SPM6103 response: {}", spm_response);
    println!("Detected type: {:?}", spm_type);
    println!("Measurement command: {}", spm_type.meas_cmd());
    println!();

    // Test unknown device
    let unknown_response = "OWON,UNKNOWN,12345678,V1.0.0";
    let unknown_type = DeviceType::from_idn(unknown_response);
    println!("Unknown response: {}", unknown_response);
    println!("Detected type: {:?}", unknown_type);
    println!("Measurement command: {}", unknown_type.meas_cmd());
    println!();

    // Test measurement value parsing for both formats
    println!("XDM1041 measurement format: +1.234");
    if let Ok(val) = "+1.234".parse::<f64>() {
        println!("Parsed value: {}", val);
    }
    println!();

    println!("SPM6103 measurement format: VOLT:DC +4.0000E-04");
    let spm_meas = "VOLT:DC +4.0000E-04";
    let parts: Vec<&str> = spm_meas.split_whitespace().collect();
    if parts.len() == 2 {
        if let Ok(val) = parts[1].parse::<f64>() {
            println!("Parsed value: {}", val);
            println!("Mode: {}", parts[0]);
        }
    }
}
