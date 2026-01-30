use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    time::Duration,
};

use mio::{Events, Interest, Poll, Token};
use tokio::sync::{mpsc, oneshot};

use crate::multimeter::{DeviceType, MeterMode, RateCmd, ScpiMode};

const SERIAL_TOKEN: Token = Token(0);

impl super::MyApp {
    pub fn spawn_serial_task(&mut self) {
        if self.serial.is_none() {
            return;
        }

        let (tx_data, rx_data) = mpsc::channel::<Option<f64>>(100); // Channel for measurements
        let (tx_cmd, mut rx_cmd) = mpsc::channel::<String>(100); // Channel for commands
        let (tx_mode, rx_mode) = mpsc::channel::<MeterMode>(10); // Channel for mode updates
        let (tx_range, rx_range) = mpsc::channel::<(MeterMode, usize)>(10); // Channel for (mode, range_index) updates
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>(); // Shutdown signal
        self.serial_rx = Some(rx_data);
        self.serial_tx = Some(tx_cmd.clone());
        self.mode_rx = Some(rx_mode);
        self.range_rx = Some(rx_range);
        self.shutdown_tx = Some(shutdown_tx);

        let mut serial = self.serial.take().unwrap();
        let value_debug_shared = self.value_debug_shared.clone();
        let poll_interval_shared = self.poll_interval_shared.clone();
        let device_shared = self.device.clone();
        let device_type_shared = self.device_type.clone(); // Clone device type Arc for use in task
        let lock_remote = self.lock_remote;
        let beeper_enabled = self.beeper_enabled;
        let cont_threshold = self.cont_threshold;
        let diod_threshold = self.diod_threshold;
        let curr_rate = self.curr_rate;
        let curr_mode = self.metermode;

        tokio::spawn(async move {
            let mut poll = Poll::new().unwrap();
            let mut events = Events::with_capacity(1);
            let mut readbuf = [0u8; 1024];
            let mut scpimode = ScpiMode::Idn;
            let mut command_queue: VecDeque<String> = VecDeque::new();
            let mut shutting_down = false;
            let mut drop_serial = false; // Flag to indicate when to drop serial
            let mut meas_count = 0; // Counter for measurement cycles
            let mut last_mode = curr_mode;
            let mut swap_diod_cont = false; // Default to no swap
            let mut expecting_range_response = false; // Flag to indicate we're waiting for range query response

            // Register serial port for readable and writable events
            poll.registry()
                .register(
                    &mut serial,
                    SERIAL_TOKEN,
                    Interest::READABLE | Interest::WRITABLE,
                )
                .unwrap();
            if *value_debug_shared.lock().unwrap() {
                println!("Serial port registered for READABLE and WRITABLE events");
            }

            // Initial commands
            command_queue.push_back("*IDN?\n".to_string());
            // Queue initial configuration commands (only if device supports them)
            // Note: At this point device type is unknown, so we skip device-specific commands
            // They will be sent later when switching modes or can be queried after *IDN?

            loop {
                tokio::select! {
                    _ = &mut shutdown_rx, if !shutting_down => {
                        // Shutdown signal received, queue shutdown commands and stop MEAS? polling
                        if *value_debug_shared.lock().unwrap() {
                            println!("Shutdown signal received, processing remaining queue: {:?}", command_queue);
                        }
                        shutting_down = true;
                        command_queue.push_back("SYST:LOC\n".to_string());
                        command_queue.push_back("*RST\n".to_string());
                        if *value_debug_shared.lock().unwrap() {
                            println!("Queued SYST:LOC and *RST for shutdown, queue: {:?}", command_queue);
                        }
                    }
                    _ = async {
                        let debug = *value_debug_shared.lock().unwrap();
                        let interval = *poll_interval_shared.lock().unwrap();

                        // Queue new commands from UI (always, even during shutdown)
                        // MULT:HOLD commands get priority (push_front), others go to back
                        while let Ok(cmd) = rx_cmd.try_recv() {
                            if debug {
                                println!("Queuing command from UI: {:?}", cmd);
                            }
                            // Give MULT:HOLD commands highest priority by pushing to front
                            if cmd.contains("MULT:HOLD") {
                                command_queue.push_front(cmd);
                                if debug {
                                    println!("MULT:HOLD command prioritized to front of queue");
                                }
                            } else {
                                command_queue.push_back(cmd);
                            }
                        }

                        // Poll for readable or writable events
                        match poll.poll(&mut events, Some(Duration::from_millis(interval))) {
                            Ok(()) => {
                                for event in events.iter() {
                                    // Handle writes
                                    if event.is_writable() && !command_queue.is_empty() {
                                        if let Some(cmd) = command_queue.front() {
                                            if debug {
                                                println!("Sending: {:?}", cmd);
                                            }
                                            match serial.write_all(cmd.as_bytes()) {
                                                Ok(()) => {
                                                    let cmd = command_queue.pop_front().unwrap();
                                                    if debug {
                                                        println!("Command sent: {:?}", cmd);
                                                    }
                                                    
                                                    // Set flag if this is a range query command
                                                    if cmd.contains("RANG?") || (cmd.contains("CONF:") && cmd.ends_with("?\n")) {
                                                        expecting_range_response = true;
                                                        if debug {
                                                            println!("Expecting range response for: {}", cmd);
                                                        }
                                                    }
                                                    
                                                    // Queue SYST:REM (if enabled) and measurement command after sending *IDN?
                                                    if cmd == "*IDN?\n" && !shutting_down {
                                                        if lock_remote {
                                                            command_queue.push_back("SYST:REM\n".to_string());
                                                            if debug {
                                                                println!("Queued SYST:REM after *IDN?");
                                                            }
                                                        }
                                                        // Use device-specific measurement command
                                                        let device_type = device_type_shared.lock().unwrap();
                                                        let meas_cmd = device_type.meas_cmd().to_string();
                                                        command_queue.push_back(meas_cmd.clone());
                                                        if debug {
                                                            println!(
                                                                "Queued {} after sending *IDN?, queue: {:?}",
                                                                meas_cmd, command_queue
                                                            );
                                                        }
                                                    }
                                                    // Set flag to drop serial after *RST is sent during shutdown
                                                    if shutting_down && cmd == "*RST\n" {
                                                        if debug {
                                                            println!("*RST sent, marking serial for shutdown");
                                                        }
                                                        drop_serial = true;
                                                    }
                                                }
                                                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                                                    if debug {
                                                        println!(
                                                            "Serial write would block for {:?}, waiting",
                                                            cmd
                                                        );
                                                    }
                                                    break;
                                                }
                                                Err(e) => {
                                                    if debug {
                                                        println!("Failed to send command {:?}: {}", cmd, e);
                                                    }
                                                    command_queue.pop_front();
                                                    break;
                                                }
                                            }
                                        }
                                    }

                                    // Handle reads
                                    if event.is_readable() {
                                        loop {
                                            match serial.read(&mut readbuf) {
                                                Ok(count) => {
                                                    let content =
                                                        String::from_utf8_lossy(&readbuf[..count]);
                                                    if debug {
                                                        println!("Received: {:?}", content);
                                                    }
                                                    if content.ends_with("\r\n") || content.ends_with("\n") {
                                                        let trimmed = content.trim_end();
                                                        if scpimode == ScpiMode::Idn {
                                                            let mut device = device_shared.lock().unwrap();
                                                            *device = trimmed.to_owned();
                                                            
                                                            // Detect device type from *IDN? response
                                                            let mut device_type = device_type_shared.lock().unwrap();
                                                            *device_type = DeviceType::from_idn(trimmed);
                                                            let supports_rate = device_type.supports_rate_control();
                                                            let supports_beeper = device_type.plugin().supports_beeper();
                                                            let supports_threshold = device_type.plugin().supports_threshold();
                                                            drop(device_type); // Release the lock
                                                            
                                                            scpimode = ScpiMode::Meas;
                                                            if debug {
                                                                println!(
                                                                    "Updated device string: {} (supports_rate: {}, supports_beeper: {}, supports_threshold: {})",
                                                                    trimmed, supports_rate, supports_beeper, supports_threshold
                                                                );
                                                            }
                                                            
                                                            // Queue initial configuration commands based on device support
                                                            if supports_rate {
                                                                command_queue.push_back(format!(
                                                                    "RATE {}\n",
                                                                    RateCmd::default().get_opt(curr_rate).1
                                                                ));
                                                            }
                                                            if supports_beeper {
                                                                if beeper_enabled {
                                                                    command_queue.push_back("SYST:BEEP:STATe ON\n".to_string());
                                                                } else {
                                                                    command_queue.push_back("SYST:BEEP:STATe OFF\n".to_string());
                                                                }
                                                            }
                                                            if supports_threshold {
                                                                command_queue.push_back(format!("CONT:THREshold {}\n", cont_threshold));
                                                                command_queue.push_back(format!("DIOD:THREshold {}\n", diod_threshold));
                                                            }
                                                            
                                                            // Parse *IDN? response to determine DIOD/CONT swap
                                                            // this is to circumvent a bug on OWON XDM 1041/1241 meters
                                                            let parts: Vec<&str> = trimmed.split(',').collect();
                                                            if parts.len() >= 4 && parts[0] == "OWON" && (parts[1] == "XDM1041" || parts[1] == "XDM1241") {
                                                                let fw_version = parts[3].trim_start_matches('V');
                                                                let version_parts: Vec<&str> = fw_version.split('.').collect();
                                                                if version_parts.len() >= 3 {
                                                                    if let Ok(major) = version_parts[0].parse::<u32>() {
                                                                        if let Ok(minor) = version_parts[1].parse::<u32>() {
                                                                            // Swap DIOD/CONT for firmware < 4.3.0
                                                                            swap_diod_cont = major < 4 || (major == 4 && minor < 3);
                                                                            if debug {
                                                                                println!(
                                                                                    "Firmware detected: V{}.{}.{}, swap_diod_cont: {}",
                                                                                    major, minor, version_parts[2], swap_diod_cont
                                                                                );
                                                                            }
                                                                        }
                                                                    }
                                                                }
                                                            }
                                                        } else if scpimode == ScpiMode::Meas {
                                                            // Check if this is a range response
                                                            if expecting_range_response {
                                                                // Skip CONFigure:ALL? format responses when waiting for range response
                                                                // SPM6103 sends both the measurement and the actual range value
                                                                if trimmed.contains(',') {
                                                                    // This is a CONFigure:ALL? response, not a range response - ignore it
                                                                    if debug {
                                                                        println!("Ignoring CONFigure:ALL? format response while waiting for range: '{}'", trimmed);
                                                                    }
                                                                } else {
                                                                    // This should be the actual range response (numeric or "AUTO")
                                                                    expecting_range_response = false;
                                                                    
                                                                    if debug {
                                                                        println!("Processing range response: '{}' for mode {:?}", trimmed, last_mode);
                                                                    }
                                                                    
                                                                    // Try to parse range response
                                                                    let range_idx = {
                                                                        let device_type = device_type_shared.lock().unwrap();
                                                                        let plugin = device_type.plugin();
                                                                        plugin.parse_range_response(trimmed, last_mode)
                                                                    }; // Lock released here
                                                                    
                                                                    if let Some(idx) = range_idx {
                                                                        let _ = tx_range.send((last_mode, idx)).await;
                                                                        if debug {
                                                                            println!("Parsed range response: index {} for mode {:?}", idx, last_mode);
                                                                        }
                                                                    } else {
                                                                        if debug {
                                                                            println!("Failed to parse range response: '{}'", trimmed);
                                                                        }
                                                                    }
                                                                    continue; // Skip normal measurement parsing
                                                                }
                                                            }
                                                            
                                                            // Use device plugin for parsing
                                                            let parse_result = {
                                                                let device_type = device_type_shared.lock().unwrap();
                                                                let plugin = device_type.plugin();
                                                                plugin.parse_measurement(trimmed, swap_diod_cont)
                                                            }; // Lock released here
                                                            
                                                            // Handle mode update if detected
                                                            if let Some(mode) = parse_result.mode {
                                                                if mode != last_mode {
                                                                    last_mode = mode;
                                                                    let _ = tx_mode.send(mode).await;
                                                                    if debug {
                                                                        println!("Detected mode: {:?}", mode);
                                                                    }
                                                                    
                                                                    // Queue beeper and threshold commands for DIOD/CONT modes if device supports them
                                                                    let device_type = device_type_shared.lock().unwrap();
                                                                    let supports_beeper = device_type.plugin().supports_beeper();
                                                                    let supports_threshold = device_type.plugin().supports_threshold();
                                                                    drop(device_type);
                                                                    
                                                                    if mode == MeterMode::Cont {
                                                                        if supports_beeper {
                                                                            if beeper_enabled {
                                                                                command_queue.push_back("SYST:BEEP:STATe ON\n".to_string());
                                                                            } else {
                                                                                command_queue.push_back("SYST:BEEP:STATe OFF\n".to_string());
                                                                            }
                                                                        }
                                                                        if supports_threshold {
                                                                            command_queue.push_back(format!("CONT:THREshold {}\n", cont_threshold));
                                                                        }
                                                                    } else if mode == MeterMode::Diod {
                                                                        if supports_beeper {
                                                                            if beeper_enabled {
                                                                                command_queue.push_back("SYST:BEEP:STATe ON\n".to_string());
                                                                            } else {
                                                                                command_queue.push_back("SYST:BEEP:STATe OFF\n".to_string());
                                                                            }
                                                                        }
                                                                        if supports_threshold {
                                                                            command_queue.push_back(format!("DIOD:THREshold {}\n", diod_threshold));
                                                                        }
                                                                    }
                                                                    
                                                                    // Queue CONFigure:ALL? to get the actual range after mode change
                                                                    // This ensures we get fresh range info for the new mode
                                                                    command_queue.push_back("CONFigure:ALL?\n".to_string());
                                                                    if debug {
                                                                        println!("Queued CONFigure:ALL? to refresh range after mode change");
                                                                    }
                                                                }
                                                            }
                                                            
                                                            // Handle measurement value if detected
                                                            if let Some(meas) = parse_result.measurement {
                                                                let _ = tx_data.send(Some(meas)).await;
                                                                if debug {
                                                                    println!("Sent measurement: {} from {}", meas, trimmed);
                                                                }
                                                                meas_count += 1;
                                                            }
                                                            
                                                            // Handle range index from CONFigure:ALL? response
                                                            // Only send if we got a range_index (instruments reports actual range)
                                                            if let Some(range_idx) = parse_result.range_index {
                                                                let _ = tx_range.send((last_mode, range_idx)).await;
                                                                if debug {
                                                                    println!("Extracted range index {} for mode {:?} from CONFigure:ALL? response", range_idx, last_mode);
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                                                    break;
                                                }
                                                Err(e) => {
                                                    if debug {
                                                        println!("Serial read error: {}", e);
                                                    }
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                if debug {
                                    println!("Poll error: {}", e);
                                }
                            }
                        }

                        // Queue measurement or function query for continuous polling in Meas mode if queue is empty, only if not shutting down
                        // Continue queuing even when hold is enabled to keep poll loop active (graph pausing is handled in UI)
                        if !shutting_down && scpimode == ScpiMode::Meas && command_queue.is_empty() {
                            if meas_count >= 10 {
                                // Use device-specific function query command
                                let device_type = device_type_shared.lock().unwrap();
                                let func_cmd = device_type.func_cmd().to_string();
                                command_queue.push_back(func_cmd.clone());
                                meas_count = 0;
                            } else {
                                // Use device-specific measurement command
                                let device_type = device_type_shared.lock().unwrap();
                                let meas_cmd = device_type.meas_cmd().to_string();
                                command_queue.push_back(meas_cmd.clone());
                            }
                        }

                        tokio::time::sleep(Duration::from_millis(interval)).await;
                    } => {}
                }

                // Exit the loop if we're shutting down and serial should be dropped
                if shutting_down && drop_serial {
                    break;
                }
            }

            // Cleanup after exiting the loop
            if *value_debug_shared.lock().unwrap() {
                println!("Cleaning up serial task");
            }
            let _ = poll.registry().deregister(&mut serial);
            drop(serial); // Explicitly drop the serial port
        });
    }
}
