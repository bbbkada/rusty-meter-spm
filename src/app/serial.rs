use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    time::Duration,
};

use mio::{Events, Interest, Poll, Token};
use tokio::sync::{mpsc, oneshot};

use crate::multimeter::{DeviceType, MeterMode, RateCmd, ScpiMode};
use crate::device_plugin::PowerSupplyState;

const SERIAL_TOKEN: Token = Token(0);

/// Power supply query phase: tracks which PS query we're waiting for a response to.
/// Only ONE query is in-flight at a time. The response handler advances the phase
/// and queues the next query.
#[derive(Debug, Clone, Copy, PartialEq)]
enum PsPhase {
    Idle,
    WaitOutp,
    WaitVolt,
    WaitCurr,
    WaitVoltLim,
    WaitCurrLim,
    WaitMeasAll,
    WaitMeasAllFast, // Fast poll: only OUTP? + MEAS:ALL? (skip settings)
}

impl super::MyApp {
    pub fn spawn_serial_task(&mut self) {
        if self.serial.is_none() {
            return;
        }

        let (tx_data, rx_data) = mpsc::channel::<Option<(f64, usize)>>(100);
        let (tx_cmd, mut rx_cmd) = mpsc::channel::<String>(100);
        let (tx_mode, rx_mode) = mpsc::channel::<MeterMode>(10);
        let (tx_range, rx_range) = mpsc::channel::<(MeterMode, usize)>(10);
        let (tx_ps, rx_ps) = mpsc::channel::<PowerSupplyState>(10);
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
        self.serial_rx = Some(rx_data);
        self.serial_tx = Some(tx_cmd.clone());
        self.mode_rx = Some(rx_mode);
        self.range_rx = Some(rx_range);
        self.ps_rx = Some(rx_ps);
        self.shutdown_tx = Some(shutdown_tx);

        let mut serial = self.serial.take().unwrap();
        let value_debug_shared = self.value_debug_shared.clone();
        let poll_interval_shared = self.poll_interval_shared.clone();
        let device_shared = self.device.clone();
        let device_type_shared = self.device_type.clone();
        let lock_remote = self.lock_remote;
        let beeper_enabled = self.beeper_enabled;
        let pending_changes = self.pending_changes.clone();
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
            let mut drop_serial = false;
            let mut meas_count: u32 = 0;
            let mut last_mode = curr_mode;
            let mut swap_diod_cont = false;
            let mut expecting_range_response = false;

            // Power supply state
            let mut ps_phase = PsPhase::Idle;
            let mut ps_state = PowerSupplyState::default();
            let mut ps_poll_counter: u32 = 0;
            let mut ps_full_poll_counter: u32 = 0; // Full poll (all settings) every N fast polls
            let mut ps_timeout: u32 = 0;
            let mut has_power_supply = false;

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

            command_queue.push_back("*IDN?\n".to_string());
            if *value_debug_shared.lock().unwrap() {
                println!("[SERIAL] Task started, *IDN? queued");
            }

            loop {
                tokio::select! {
                    _ = &mut shutdown_rx, if !shutting_down => {
                        if *value_debug_shared.lock().unwrap() {
                            println!("Shutdown signal received, processing remaining queue: {:?}", command_queue);
                        }
                        shutting_down = true;
                        ps_phase = PsPhase::Idle;
                        if has_power_supply {
                            command_queue.push_back("OUTP OFF\n".to_string());
                        }
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
                        while let Ok(cmd) = rx_cmd.try_recv() {
                            if debug {
                                println!("Queuing command from UI: {:?}", cmd);
                            }
                            if cmd.contains("MULT:HOLD") {
                                command_queue.push_front(cmd);
                                if debug {
                                    println!("MULT:HOLD command prioritized to front of queue");
                                }
                            } else {
                                command_queue.push_back(cmd);
                            }
                        }

                        // Try to send queued commands EVERY iteration
                        // (don't rely on edge-triggered WRITABLE events from mio)
                        if !command_queue.is_empty() && debug {
                            println!("[SERIAL] Queue depth: {} commands: {:?}", command_queue.len(),
                                command_queue.iter().take(5).collect::<Vec<_>>());
                        }
                        while !command_queue.is_empty() {
                            if let Some(cmd) = command_queue.front() {
                                if debug {
                                    println!("Sending: {:?}", cmd);
                                }
                                match serial.write_all(cmd.as_bytes()) {
                                    Ok(()) => {
                                        let cmd = command_queue.pop_front().unwrap();
                                        if debug { println!("[SERIAL] SENT: {:?} (queue_len={})", cmd, command_queue.len()); }

                                        // Set flag if this is a range query command
                                        if cmd.contains("RANG?") || (cmd.contains("CONF:") && cmd.ends_with("?\n")) {
                                            expecting_range_response = true;
                                            if debug {
                                                println!("Expecting range response for: {}", cmd);
                                            }
                                        }

                                        // Queue SYST:REM after sending *IDN?
                                        if cmd == "*IDN?\n" && !shutting_down {
                                            if lock_remote {
                                                command_queue.push_back("SYST:REM\n".to_string());
                                                if debug {
                                                    println!("Queued SYST:REM after *IDN?");
                                                }
                                            }
                                        }
                                        // Set flag to drop serial after *RST is sent during shutdown
                                        if shutting_down && cmd == "*RST\n" {
                                            if debug {
                                                println!("*RST sent, marking serial for shutdown");
                                            }
                                            drop_serial = true;
                                        }

                                        // Only send ONE query command at a time (commands ending with ?)
                                        // to maintain request/response ordering. Non-query commands
                                        // (SET commands like OUTP ON, VOLT 5, SYST:REM) can be sent
                                        // back-to-back since they don't generate responses.
                                        if cmd.trim_end().ends_with('?') {
                                            break; // Wait for response before sending next query
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

                        // Poll for readable events
                        match poll.poll(&mut events, Some(Duration::from_millis(interval))) {
                            Ok(()) => {
                                for event in events.iter() {
                                    // Handle reads
                                    if event.is_readable() {
                                        loop {
                                            match serial.read(&mut readbuf) {
                                                Ok(count) => {
                                                    let content =
                                                        String::from_utf8_lossy(&readbuf[..count]);
                                                    if debug { println!("[SERIAL] RECV[{}]: {:?}", count, content); }
                                                    // Split by newlines and process each complete line
                                                    for raw_line in content.split('\n') {
                                                        let trimmed = raw_line.trim();
                                                        if trimmed.is_empty() {
                                                            continue;
                                                        }
                                                        // Skip ERR responses (e.g. from unsupported commands)
                                                        if trimmed == "ERR" {
                                                            if debug { println!("[SERIAL] Skipping ERR response"); }
                                                            continue;
                                                        }
                                                        if debug { println!("[SERIAL] PROCESSING: {:?} (scpimode={:?}, ps_phase={:?})", trimmed, scpimode, ps_phase); }
                                                        if scpimode == ScpiMode::Idn {
                                                            let mut device = device_shared.lock().unwrap();
                                                            *device = trimmed.to_owned();

                                                            // Detect device type from *IDN? response
                                                            let mut device_type = device_type_shared.lock().unwrap();
                                                            *device_type = DeviceType::from_idn(trimmed);
                                                            let supports_rate = device_type.supports_rate_control();
                                                            let supports_beeper = device_type.plugin().supports_beeper();
                                                            let supports_threshold = device_type.plugin().supports_threshold();
                                                            has_power_supply = device_type.plugin().supports_power_supply();
                                                            drop(device_type);

                                                            scpimode = ScpiMode::Meas;
                                                            if debug { println!("[SERIAL] IDN detected: {} has_ps={}", trimmed, has_power_supply); }
                                                            if debug {
                                                                println!(
                                                                    "Updated device string: {} (supports_rate: {}, supports_beeper: {}, supports_threshold: {}, has_ps: {})",
                                                                    trimmed, supports_rate, supports_beeper, supports_threshold, has_power_supply
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

                                                            // Start initial PS query (one at a time)
                                                            if has_power_supply {
                                                                ps_phase = PsPhase::WaitOutp;
                                                                ps_timeout = 0;
                                                                command_queue.push_back("OUTP?\n".to_string());
                                                                if debug {
                                                                    println!("Starting initial PS query sequence");
                                                                }
                                                            }

                                                            // Parse *IDN? response to determine DIOD/CONT swap
                                                            let parts: Vec<&str> = trimmed.split(',').collect();
                                                            if parts.len() >= 4 && parts[0] == "OWON" && (parts[1] == "XDM1041" || parts[1] == "XDM1241") {
                                                                let fw_version = parts[3].trim_start_matches('V');
                                                                let version_parts: Vec<&str> = fw_version.split('.').collect();
                                                                if version_parts.len() >= 3 {
                                                                    if let Ok(major) = version_parts[0].parse::<u32>() {
                                                                        if let Ok(minor) = version_parts[1].parse::<u32>() {
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
                                                            // === PS response handling ===
                                                            // Only active when we're waiting for a specific PS response.
                                                            // For most phases: PS responses are simple values (no commas, no colons).
                                                            // For WaitMeasAll: MEAS:ALL? returns comma-separated values like "0.000,0.000"
                                                            // so we need to allow commas through for that phase.
                                                            let is_ps_candidate = ps_phase != PsPhase::Idle && (
                                                                ps_phase == PsPhase::WaitMeasAll || ps_phase == PsPhase::WaitMeasAllFast ||
                                                                (!trimmed.contains(',') && !trimmed.contains(':'))
                                                            );
                                                            if is_ps_candidate {
                                                                if debug { println!("[SERIAL] PS candidate: {:?} for phase {:?}", trimmed, ps_phase); }
                                                                let consumed = match ps_phase {
                                                                    PsPhase::WaitOutp => {
                                                                        if trimmed == "0" || trimmed == "1" || trimmed.eq_ignore_ascii_case("ON") || trimmed.eq_ignore_ascii_case("OFF") {
                                                                            ps_state.output_on = trimmed == "1" || trimmed.eq_ignore_ascii_case("ON");
                                                                            if debug { println!("PS OUTP? = {} -> output_on={}", trimmed, ps_state.output_on); }
                                                                            // Check if this is a fast poll (skip settings) or full poll
                                                                            if ps_full_poll_counter > 0 {
                                                                                // Fast poll: skip settings, go straight to MEAS:ALL?
                                                                                ps_phase = PsPhase::WaitMeasAllFast;
                                                                                command_queue.push_back("MEAS:ALL?\n".to_string());
                                                                            } else {
                                                                                // Full poll: query all settings
                                                                                ps_phase = PsPhase::WaitVolt;
                                                                                command_queue.push_back("VOLT?\n".to_string());
                                                                            }
                                                                            ps_timeout = 0;
                                                                            true
                                                                        } else { false }
                                                                    }
                                                                    PsPhase::WaitVolt => {
                                                                        if let Ok(v) = trimmed.parse::<f64>() {
                                                                            ps_state.voltage_set = v;
                                                                            if debug { println!("PS VOLT? = {}", v); }
                                                                            ps_phase = PsPhase::WaitCurr;
                                                                            command_queue.push_back("CURR?\n".to_string());
                                                                            ps_timeout = 0;
                                                                            true
                                                                        } else { false }
                                                                    }
                                                                    PsPhase::WaitCurr => {
                                                                        if let Ok(v) = trimmed.parse::<f64>() {
                                                                            ps_state.current_set = v;
                                                                            if debug { println!("PS CURR? = {}", v); }
                                                                            ps_phase = PsPhase::WaitVoltLim;
                                                                            command_queue.push_back("VOLT:LIM?\n".to_string());
                                                                            ps_timeout = 0;
                                                                            true
                                                                        } else { false }
                                                                    }
                                                                    PsPhase::WaitVoltLim => {
                                                                        if let Ok(v) = trimmed.parse::<f64>() {
                                                                            ps_state.ovp = v;
                                                                            if debug { println!("PS VOLT:LIM? = {}", v); }
                                                                            ps_phase = PsPhase::WaitCurrLim;
                                                                            command_queue.push_back("CURR:LIM?\n".to_string());
                                                                            ps_timeout = 0;
                                                                            true
                                                                        } else { false }
                                                                    }
                                                                    PsPhase::WaitCurrLim => {
                                                                        if let Ok(v) = trimmed.parse::<f64>() {
                                                                            ps_state.ocp = v;
                                                                            if debug { println!("PS CURR:LIM? = {}", v); }
                                                                            // Now query actual output measurement
                                                                            ps_phase = PsPhase::WaitMeasAll;
                                                                            command_queue.push_back("MEAS:ALL?\n".to_string());
                                                                            ps_timeout = 0;
                                                                            true
                                                                        } else { false }
                                                                    }
                                                                    PsPhase::WaitMeasAll => {
                                                                        // MEAS:ALL? returns comma-separated: "V,I" or "V,I,P"
                                                                        // or space-separated: "V I P" depending on firmware
                                                                        let parts: Vec<&str> = if trimmed.contains(',') {
                                                                            trimmed.split(',').collect()
                                                                        } else {
                                                                            trimmed.split_whitespace().collect()
                                                                        };
                                                                        if parts.len() >= 2 {
                                                                            if let (Ok(v), Ok(i)) = (parts[0].parse::<f64>(), parts[1].parse::<f64>()) {
                                                                                ps_state.voltage_readback = v;
                                                                                ps_state.current_readback = i;
                                                                                if parts.len() >= 3 {
                                                                                    if let Ok(p) = parts[2].parse::<f64>() {
                                                                                        ps_state.power_readback = p;
                                                                                    }
                                                                                } else {
                                                                                    // Only V,I — calculate power
                                                                                    ps_state.power_readback = v * i;
                                                                                }
                                                                                if debug { println!("[SERIAL] PS MEAS:ALL? = V:{} A:{} W:{}", v, i, ps_state.power_readback); }
                                                                                // All queries done, send complete state
                                                                                ps_state.includes_settings = true;
                                                                                let _ = tx_ps.send(ps_state.clone()).await;
                                                                                if debug { println!("[SERIAL] PS state sent to UI: out={} V_set={} I_set={} OVP={} OCP={} V_meas={} I_meas={} W={}",
                                                                                    ps_state.output_on, ps_state.voltage_set, ps_state.current_set,
                                                                                    ps_state.ovp, ps_state.ocp,
                                                                                    ps_state.voltage_readback, ps_state.current_readback, ps_state.power_readback); }
                                                                                ps_phase = PsPhase::Idle;
                                                                                ps_timeout = 0;
                                                                                true
                                                                            } else { false }
                                                                        } else { false }
                                                                    }
                                                                    PsPhase::WaitMeasAllFast => {
                                                                        // Fast poll: same MEAS:ALL? parsing
                                                                        let parts: Vec<&str> = if trimmed.contains(',') {
                                                                            trimmed.split(',').collect()
                                                                        } else {
                                                                            trimmed.split_whitespace().collect()
                                                                        };
                                                                        if parts.len() >= 2 {
                                                                            if let (Ok(v), Ok(i)) = (parts[0].parse::<f64>(), parts[1].parse::<f64>()) {
                                                                                ps_state.voltage_readback = v;
                                                                                ps_state.current_readback = i;
                                                                                if parts.len() >= 3 {
                                                                                    if let Ok(p) = parts[2].parse::<f64>() {
                                                                                        ps_state.power_readback = p;
                                                                                    }
                                                                                } else {
                                                                                    ps_state.power_readback = v * i;
                                                                                }
                                                                                if debug { println!("[SERIAL] PS MEAS:ALL? (fast) = V:{} A:{} W:{}", v, i, ps_state.power_readback); }
                                                                                ps_state.includes_settings = false;
                                                                                let _ = tx_ps.send(ps_state.clone()).await;
                                                                                ps_phase = PsPhase::Idle;
                                                                                ps_timeout = 0;
                                                                                true
                                                                            } else { false }
                                                                        } else { false }
                                                                    }
                                                                    PsPhase::Idle => false,
                                                                };
                                                                if consumed {
                                                                    continue; // Skip normal measurement parsing
                                                                }
                                                                // Not a valid PS response, fall through to normal parsing
                                                                if debug {
                                                                    println!("PS: '{}' not valid for {:?}, parsing normally", trimmed, ps_phase);
                                                                }
                                                            }

                                                            // === Range response handling ===
                                                            if expecting_range_response {
                                                                if trimmed.contains(',') {
                                                                    if debug {
                                                                        println!("Ignoring CONFigure:ALL? format response while waiting for range: '{}'", trimmed);
                                                                    }
                                                                } else {
                                                                    expecting_range_response = false;

                                                                    if debug {
                                                                        println!("Processing range response: '{}' for mode {:?}", trimmed, last_mode);
                                                                    }

                                                                    let range_idx = {
                                                                        let device_type = device_type_shared.lock().unwrap();
                                                                        let plugin = device_type.plugin();
                                                                        plugin.parse_range_response(trimmed, last_mode)
                                                                    };

                                                                    if let Some(idx) = range_idx {
                                                                        let _ = tx_range.send((last_mode, idx)).await;
                                                                        if debug {
                                                                            println!("Parsed range response: index {} for mode {:?}", idx, last_mode);
                                                                        }
                                                                    } else if debug {
                                                                        println!("Failed to parse range response: '{}'", trimmed);
                                                                    }
                                                                    continue; // Skip normal measurement parsing
                                                                }
                                                            }

                                                            // === Normal measurement parsing ===
                                                            let parse_result = {
                                                                let device_type = device_type_shared.lock().unwrap();
                                                                let plugin = device_type.plugin();
                                                                plugin.parse_measurement(trimmed, swap_diod_cont)
                                                            };

                                                            // Handle mode update if detected
                                                            if let Some(mode) = parse_result.mode {
                                                                if mode != last_mode {
                                                                    last_mode = mode;
                                                                    let _ = tx_mode.send(mode).await;
                                                                    if debug {
                                                                        println!("Detected mode: {:?}", mode);
                                                                    }

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

                                                                    command_queue.push_back("CONFigure:ALL?\n".to_string());
                                                                    if debug {
                                                                        println!("Queued CONFigure:ALL? to refresh range after mode change");
                                                                    }
                                                                }
                                                            }

                                                            // Handle measurement value if detected
                                                            if let Some(meas) = parse_result.measurement {
                                                                let decimals = parse_result.decimals.unwrap_or(4);
                                                                let _ = tx_data.send(Some((meas, decimals))).await;
                                                                if debug {
                                                                    println!("Sent measurement: {} ({}dp) from {}", meas, decimals, trimmed);
                                                                }
                                                                meas_count += 1;
                                                            }

                                                            // Handle range index from CONFigure:ALL? response
                                                            if let Some(range_idx) = parse_result.range_index {
                                                                let _ = tx_range.send((last_mode, range_idx)).await;
                                                                if debug {
                                                                    println!("Extracted range index {} for mode {:?} from CONFigure:ALL? response", range_idx, last_mode);
                                                                }
                                                            }
                                                        }
                                                    } // end for raw_line
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

                        // PS timeout: if waiting too long for PS response, abort and resume normal operation
                        if ps_phase != PsPhase::Idle {
                            ps_timeout += 1;
                            if ps_timeout > 50 {
                                if debug {
                                    println!("PS query timeout (phase={:?}), resetting to Idle", ps_phase);
                                }
                                ps_phase = PsPhase::Idle;
                                ps_timeout = 0;
                            }
                        }

                        // Queue measurement or PS query when idle.
                        // CRITICAL: Don't queue measurement commands while PS sequence
                        // is active — responses would interleave and corrupt the PS state machine.
                        if !shutting_down && scpimode == ScpiMode::Meas && command_queue.is_empty() {
                            if ps_phase != PsPhase::Idle {
                                if debug { println!("[SERIAL] Idle: PS active (phase={:?}), waiting", ps_phase); }
                            } else {
                                // Send pending mode command before measurement query.
                                // Re-sent every cycle until UI confirms mode change via readback,
                                // but limited to avoid flooding the instrument.
                                {
                                    let mut changes = pending_changes.lock().unwrap();
                                    if let Some((target_mode, ref cmd, retries)) = changes.mode.clone() {
                                        if retries > 0 {
                                            command_queue.push_back(cmd.clone());
                                            changes.mode = Some((target_mode, cmd.clone(), retries - 1));
                                            if debug { println!("Pending mode cmd (retries left={})", retries - 1); }
                                        }
                                    }
                                }
                                
                                // Normal operation: queue measurement or function query
                                if meas_count >= 10 {
                                    let device_type = device_type_shared.lock().unwrap();
                                    let func_cmd = device_type.func_cmd().to_string();
                                    command_queue.push_back(func_cmd.clone());
                                    meas_count = 0;
                                } else {
                                    let device_type = device_type_shared.lock().unwrap();
                                    let meas_cmd = device_type.meas_cmd().to_string();
                                    command_queue.push_back(meas_cmd.clone());
                                }

                                // Periodically start a PS query sequence
                                if has_power_supply {
                                    ps_poll_counter += 1;
                                    if ps_poll_counter >= 5 {
                                        ps_poll_counter = 0;
                                        ps_phase = PsPhase::WaitOutp;
                                        ps_timeout = 0;
                                        // Send ALL pending GUI changes every poll cycle.
                                        // SET commands are idempotent — safe to re-send until
                                        // the UI clears the pending field upon readback confirmation.
                                        // This guarantees delivery even if a command is lost.
                                        let changes = pending_changes.lock().unwrap().clone();
                                        if let Some(on) = changes.output_on {
                                            let cmd = if on { "OUTP ON\n" } else { "OUTP OFF\n" };
                                            command_queue.push_back(cmd.to_string());
                                            if debug { println!("PS pending: OUTP {}", if on { "ON" } else { "OFF" }); }
                                        }
                                        if let Some(v) = changes.voltage_set {
                                            command_queue.push_back(format!("VOLT {:.3}\n", v));
                                            if debug { println!("PS pending: VOLT {:.3}", v); }
                                        }
                                        if let Some(v) = changes.current_set {
                                            command_queue.push_back(format!("CURR {:.3}\n", v));
                                            if debug { println!("PS pending: CURR {:.3}", v); }
                                        }
                                        if let Some(v) = changes.ovp {
                                            command_queue.push_back(format!("VOLT:LIM {:.3}\n", v));
                                            if debug { println!("PS pending: VOLT:LIM {:.3}", v); }
                                        }
                                        if let Some(v) = changes.ocp {
                                            command_queue.push_back(format!("CURR:LIM {:.3}\n", v));
                                            if debug { println!("PS pending: CURR:LIM {:.3}", v); }
                                        }
                                        if let Some((_idx, ref cmd)) = changes.range {
                                            for line in cmd.lines() {
                                                let line = line.trim();
                                                if !line.is_empty() {
                                                    command_queue.push_back(format!("{}\n", line));
                                                }
                                            }
                                            if debug { println!("PS pending: range cmd {}", cmd.trim()); }
                                        }
                                        command_queue.push_back("OUTP?\n".to_string());
                                        // Full poll (all settings) every 10th PS poll
                                        ps_full_poll_counter += 1;
                                        if ps_full_poll_counter >= 10 {
                                            ps_full_poll_counter = 0;
                                        }
                                        if debug {
                                            println!("Starting PS query (full={})", ps_full_poll_counter == 0);
                                        }
                                    }
                                }
                            }
                        }

                        // Sleep only a short time to yield, poll() already provides the main wait
                        tokio::time::sleep(Duration::from_millis(1)).await;
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
            drop(serial);
        });
    }
}
