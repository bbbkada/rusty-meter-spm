use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    time::Duration,
};

use mio::{Events, Interest, Poll, Token};
use tokio::sync::{mpsc, oneshot};

use crate::multimeter::MeterMode;
use crate::plugins::PowerSupplyState;

const SERIAL_TOKEN: Token = Token(0);

/// Serial-task internal state: are we waiting for *IDN? or doing measurement polling?
#[derive(PartialEq, Clone, Copy, Debug)]
enum ScpiMode {
    Idn,
    Meas,
}

/// Power supply query phase: tracks which PS query we're waiting for.
#[derive(Debug, Clone, Copy, PartialEq)]
enum PsPhase {
    Idle,
    WaitOutp,
    WaitVolt,
    WaitCurr,
    WaitVoltLim,
    WaitCurrLim,
    WaitMeasAll,
    WaitMeasAllFast,
}

impl super::MyApp {
    pub fn spawn_serial_task(&mut self) {
        if self.serial.is_none() {
            return;
        }

        let (tx_data, rx_data) = mpsc::channel::<Option<(f64, f64)>>(100);
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
        let plugin_shared = self.plugin.clone();
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
            let mut expecting_range_response = false;

            // Power supply state
            let mut ps_phase = PsPhase::Idle;
            let mut ps_state = PowerSupplyState::default();
            let mut ps_poll_counter: u32 = 0;
            let mut ps_full_poll_counter: u32 = 0;
            let mut ps_timeout: u32 = 0;
            let mut has_power_supply = false;

            poll.registry()
                .register(
                    &mut serial,
                    SERIAL_TOKEN,
                    Interest::READABLE | Interest::WRITABLE,
                )
                .unwrap();

            command_queue.push_back("*IDN?\n".to_string());
            if *value_debug_shared.lock().unwrap() {
                println!("[SERIAL] Task started, *IDN? queued");
            }

            loop {
                tokio::select! {
                    _ = &mut shutdown_rx, if !shutting_down => {
                        let debug = *value_debug_shared.lock().unwrap();
                        if debug {
                            println!("Shutdown signal received, processing remaining queue: {:?}", command_queue);
                        }
                        shutting_down = true;
                        ps_phase = PsPhase::Idle;
                        if has_power_supply {
                            let plugin = plugin_shared.lock().unwrap();
                            if let Some(cmd) = plugin.ps_output_command(false) {
                                command_queue.push_back(cmd);
                            }
                        }
                        {
                            let plugin = plugin_shared.lock().unwrap();
                            if let Some(cmd) = plugin.local_command() {
                                command_queue.push_back(cmd.to_string());
                            }
                            command_queue.push_back(plugin.reset_command().to_string());
                        }
                    }
                    _ = async {
                        let debug = *value_debug_shared.lock().unwrap();
                        let interval = *poll_interval_shared.lock().unwrap();

                        // Queue new commands from UI
                        while let Ok(cmd) = rx_cmd.try_recv() {
                            if debug {
                                println!("Queuing command from UI: {:?}", cmd);
                            }
                            if cmd.contains("MULT:HOLD") {
                                command_queue.push_front(cmd);
                            } else {
                                command_queue.push_back(cmd);
                            }
                        }

                        // Send queued commands
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

                                        // Set flag if this is a range query
                                        if cmd.contains("RANG?") || (cmd.contains("CONF:") && cmd.ends_with("?\n")) {
                                            expecting_range_response = true;
                                        }

                                        // Queue SYST:REM after *IDN?
                                        if cmd == "*IDN?\n" && !shutting_down {
                                            if lock_remote {
                                                let plugin = plugin_shared.lock().unwrap();
                                                if let Some(rem) = plugin.remote_command() {
                                                    command_queue.push_back(rem.to_string());
                                                }
                                            }
                                        }

                                        // Mark serial for drop after reset during shutdown
                                        if shutting_down && cmd.trim() == "*RST" {
                                            drop_serial = true;
                                        }

                                        // Only send ONE query at a time
                                        if cmd.trim_end().ends_with('?') {
                                            break;
                                        }
                                    }
                                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
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
                                    if event.is_readable() {
                                        loop {
                                            match serial.read(&mut readbuf) {
                                                Ok(count) => {
                                                    let content =
                                                        String::from_utf8_lossy(&readbuf[..count]);
                                                    if debug { println!("[SERIAL] RECV[{}]: {:?}", count, content); }

                                                    for raw_line in content.split('\n') {
                                                        let trimmed = raw_line.trim();
                                                        if trimmed.is_empty() {
                                                            continue;
                                                        }
                                                        if trimmed == "ERR" {
                                                            if debug { println!("[SERIAL] Skipping ERR response"); }
                                                            continue;
                                                        }
                                                        if debug { println!("[SERIAL] PROCESSING: {:?} (scpimode={:?}, ps_phase={:?})", trimmed, scpimode, ps_phase); }

                                                        if scpimode == ScpiMode::Idn {
                                                            // ── IDN Response ──
                                                            let mut device = device_shared.lock().unwrap();
                                                            *device = trimmed.to_owned();
                                                            drop(device);

                                                            // Resolve plugin from IDN
                                                            let resolved = crate::plugins::resolve_plugin(trimmed);
                                                            let caps = resolved.capabilities().clone();
                                                            *plugin_shared.lock().unwrap() = resolved;
                                                            has_power_supply = caps.has_power_supply;

                                                            scpimode = ScpiMode::Meas;
                                                            if debug { println!("[SERIAL] IDN detected: {} has_ps={}", trimmed, has_power_supply); }

                                                            // Queue initial configuration based on capabilities
                                                            {
                                                                let plugin = plugin_shared.lock().unwrap();

                                                                // Sampling rate
                                                                if !caps.rate_options.is_empty() {
                                                                    if let Some(cmd) = plugin.rate_command(curr_rate) {
                                                                        command_queue.push_back(cmd);
                                                                    }
                                                                }

                                                                // Beeper
                                                                if caps.has_beeper {
                                                                    if let Some(cmd) = plugin.beeper_command(beeper_enabled) {
                                                                        command_queue.push_back(cmd);
                                                                    }
                                                                }

                                                                // Thresholds
                                                                if caps.has_threshold {
                                                                    if let Some(cmd) = plugin.cont_threshold_command(cont_threshold) {
                                                                        command_queue.push_back(cmd);
                                                                    }
                                                                    if let Some(cmd) = plugin.diod_threshold_command(diod_threshold) {
                                                                        command_queue.push_back(cmd);
                                                                    }
                                                                }
                                                            }

                                                            // Start initial PS query sequence
                                                            if has_power_supply {
                                                                let plugin = plugin_shared.lock().unwrap();
                                                                if let Some(cmd) = plugin.ps_query_output() {
                                                                    ps_phase = PsPhase::WaitOutp;
                                                                    ps_timeout = 0;
                                                                    command_queue.push_back(cmd.to_string());
                                                                }
                                                            }
                                                        } else if scpimode == ScpiMode::Meas {
                                                            // ── PS Response Handling ──
                                                            let is_ps_candidate = ps_phase != PsPhase::Idle && (
                                                                ps_phase == PsPhase::WaitMeasAll || ps_phase == PsPhase::WaitMeasAllFast ||
                                                                (!trimmed.contains(',') && !trimmed.contains(':'))
                                                            );
                                                            if is_ps_candidate {
                                                                if debug { println!("[SERIAL] PS candidate: {:?} for phase {:?}", trimmed, ps_phase); }
                                                                // Parse PS response under lock, then drop lock before any .await
                                                                let (consumed, should_send_ps) = {
                                                                    let plugin = plugin_shared.lock().unwrap();
                                                                    match ps_phase {
                                                                    PsPhase::WaitOutp => {
                                                                        if let Some(on) = plugin.parse_output_state(trimmed) {
                                                                            ps_state.output_on = on;
                                                                            if debug { println!("PS OUTP? = {} -> output_on={}", trimmed, on); }
                                                                            if ps_full_poll_counter > 0 {
                                                                                if let Some(cmd) = plugin.ps_query_meas_all() {
                                                                                    ps_phase = PsPhase::WaitMeasAllFast;
                                                                                    command_queue.push_back(cmd.to_string());
                                                                                }
                                                                            } else {
                                                                                if let Some(cmd) = plugin.ps_query_voltage() {
                                                                                    ps_phase = PsPhase::WaitVolt;
                                                                                    command_queue.push_back(cmd.to_string());
                                                                                }
                                                                            }
                                                                            ps_timeout = 0;
                                                                            (true, false)
                                                                        } else { (false, false) }
                                                                    }
                                                                    PsPhase::WaitVolt => {
                                                                        if let Some(v) = plugin.parse_ps_value(trimmed) {
                                                                            ps_state.voltage_set = v;
                                                                            if let Some(cmd) = plugin.ps_query_current() {
                                                                                ps_phase = PsPhase::WaitCurr;
                                                                                command_queue.push_back(cmd.to_string());
                                                                            }
                                                                            ps_timeout = 0;
                                                                            (true, false)
                                                                        } else { (false, false) }
                                                                    }
                                                                    PsPhase::WaitCurr => {
                                                                        if let Some(v) = plugin.parse_ps_value(trimmed) {
                                                                            ps_state.current_set = v;
                                                                            if let Some(cmd) = plugin.ps_query_ovp() {
                                                                                ps_phase = PsPhase::WaitVoltLim;
                                                                                command_queue.push_back(cmd.to_string());
                                                                            }
                                                                            ps_timeout = 0;
                                                                            (true, false)
                                                                        } else { (false, false) }
                                                                    }
                                                                    PsPhase::WaitVoltLim => {
                                                                        if let Some(v) = plugin.parse_ps_value(trimmed) {
                                                                            ps_state.ovp = v;
                                                                            if let Some(cmd) = plugin.ps_query_ocp() {
                                                                                ps_phase = PsPhase::WaitCurrLim;
                                                                                command_queue.push_back(cmd.to_string());
                                                                            }
                                                                            ps_timeout = 0;
                                                                            (true, false)
                                                                        } else { (false, false) }
                                                                    }
                                                                    PsPhase::WaitCurrLim => {
                                                                        if let Some(v) = plugin.parse_ps_value(trimmed) {
                                                                            ps_state.ocp = v;
                                                                            if let Some(cmd) = plugin.ps_query_meas_all() {
                                                                                ps_phase = PsPhase::WaitMeasAll;
                                                                                command_queue.push_back(cmd.to_string());
                                                                            }
                                                                            ps_timeout = 0;
                                                                            (true, false)
                                                                        } else { (false, false) }
                                                                    }
                                                                    PsPhase::WaitMeasAll => {
                                                                        if let Some((v, i, p)) = plugin.parse_ps_meas_all(trimmed) {
                                                                            ps_state.voltage_readback = v;
                                                                            ps_state.current_readback = i;
                                                                            ps_state.power_readback = p;
                                                                            ps_state.includes_settings = true;
                                                                            ps_phase = PsPhase::Idle;
                                                                            ps_timeout = 0;
                                                                            (true, true) // need to send ps_state
                                                                        } else { (false, false) }
                                                                    }
                                                                    PsPhase::WaitMeasAllFast => {
                                                                        if let Some((v, i, p)) = plugin.parse_ps_meas_all(trimmed) {
                                                                            ps_state.voltage_readback = v;
                                                                            ps_state.current_readback = i;
                                                                            ps_state.power_readback = p;
                                                                            ps_state.includes_settings = false;
                                                                            ps_phase = PsPhase::Idle;
                                                                            ps_timeout = 0;
                                                                            (true, true) // need to send ps_state
                                                                        } else { (false, false) }
                                                                    }
                                                                    PsPhase::Idle => (false, false),
                                                                    }
                                                                }; // plugin lock dropped here
                                                                // Send PS state outside the lock
                                                                if should_send_ps {
                                                                    let _ = tx_ps.send(ps_state.clone()).await;
                                                                }
                                                                if consumed {
                                                                    continue;
                                                                }
                                                                if debug {
                                                                    println!("PS: '{}' not valid for {:?}, parsing normally", trimmed, ps_phase);
                                                                }
                                                            }

                                                            // ── Range Response Handling ──
                                                            if expecting_range_response {
                                                                if trimmed.contains(',') {
                                                                    if debug {
                                                                        println!("Ignoring CONFigure:ALL? format while waiting for range: '{}'", trimmed);
                                                                    }
                                                                } else {
                                                                    expecting_range_response = false;
                                                                    let range_idx = {
                                                                        let plugin = plugin_shared.lock().unwrap();
                                                                        plugin.parse_range_response(trimmed, last_mode)
                                                                    };
                                                                    if let Some(idx) = range_idx {
                                                                        let _ = tx_range.send((last_mode, idx)).await;
                                                                        if debug {
                                                                            println!("Parsed range response: index {} for mode {:?}", idx, last_mode);
                                                                        }
                                                                    }
                                                                    continue;
                                                                }
                                                            }

                                                            // ── Normal Measurement Parsing ──
                                                            let parse_result = {
                                                                let plugin = plugin_shared.lock().unwrap();
                                                                plugin.parse_measurement(trimmed)
                                                            };

                                                            // Handle mode update
                                                            if let Some(mode) = parse_result.mode {
                                                                // Clear pending retries when instrument confirms target mode
                                                                {
                                                                    let mut changes = pending_changes.lock().unwrap();
                                                                    if let Some((target, _, _)) = &changes.mode {
                                                                        if *target == mode {
                                                                            if debug { println!("Instrument confirmed target mode {:?}, clearing pending retries", mode); }
                                                                            changes.mode = None;
                                                                        }
                                                                    }
                                                                }
                                                                if mode != last_mode {
                                                                    last_mode = mode;
                                                                    let _ = tx_mode.send(mode).await;
                                                                    if debug {
                                                                        println!("Detected mode: {:?}", mode);
                                                                    }

                                                                    // Mode-specific init
                                                                    let plugin = plugin_shared.lock().unwrap();
                                                                    let caps = plugin.capabilities();
                                                                    if mode == MeterMode::Cont || mode == MeterMode::Diod {
                                                                        if caps.has_beeper {
                                                                            if let Some(cmd) = plugin.beeper_command(beeper_enabled) {
                                                                                command_queue.push_back(cmd);
                                                                            }
                                                                        }
                                                                        if caps.has_threshold {
                                                                            if mode == MeterMode::Cont {
                                                                                if let Some(cmd) = plugin.cont_threshold_command(cont_threshold) {
                                                                                    command_queue.push_back(cmd);
                                                                                }
                                                                            } else {
                                                                                if let Some(cmd) = plugin.diod_threshold_command(diod_threshold) {
                                                                                    command_queue.push_back(cmd);
                                                                                }
                                                                            }
                                                                        }
                                                                    }
                                                                    // Queue function query to refresh range after mode change
                                                                    command_queue.push_back(plugin.function_query_command().to_string());
                                                                }
                                                            }

                                                            // Handle measurement value
                                                            if let Some(meas) = parse_result.measurement {
                                                                let precision = parse_result.precision.unwrap_or(0.0001);
                                                                let _ = tx_data.send(Some((meas, precision))).await;
                                                                meas_count += 1;
                                                            }

                                                            // Handle range index from CONFigure:ALL?
                                                            if let Some(range_idx) = parse_result.range_index {
                                                                let _ = tx_range.send((last_mode, range_idx)).await;
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

                        // PS timeout
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

                        // Queue measurement or PS query when idle
                        if !shutting_down && scpimode == ScpiMode::Meas && command_queue.is_empty() {
                            if ps_phase != PsPhase::Idle {
                                if debug { println!("[SERIAL] Idle: PS active (phase={:?}), waiting", ps_phase); }
                            } else {
                                // Send pending mode command
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

                                // Send pending range change (works for all devices)
                                {
                                    let changes = pending_changes.lock().unwrap();
                                    if let Some((_idx, ref cmd)) = changes.range {
                                        for line in cmd.lines() {
                                            let line = line.trim();
                                            if !line.is_empty() {
                                                command_queue.push_back(format!("{}\n", line));
                                            }
                                        }
                                        if debug { println!("Pending range cmd sent"); }
                                    }
                                }

                                // Queue measurement or function query
                                {
                                    let plugin = plugin_shared.lock().unwrap();
                                    if meas_count >= 10 {
                                        command_queue.push_back(plugin.function_query_command().to_string());
                                        meas_count = 0;
                                    } else {
                                        command_queue.push_back(plugin.measurement_command().to_string());
                                    }
                                }

                                // Periodically start PS query sequence
                                if has_power_supply {
                                    ps_poll_counter += 1;
                                    if ps_poll_counter >= 5 {
                                        ps_poll_counter = 0;
                                        ps_phase = PsPhase::WaitOutp;
                                        ps_timeout = 0;

                                        // Send ALL pending GUI changes
                                        let changes = pending_changes.lock().unwrap().clone();
                                        let plugin = plugin_shared.lock().unwrap();

                                        if let Some(on) = changes.output_on {
                                            if let Some(cmd) = plugin.ps_output_command(on) {
                                                command_queue.push_back(cmd);
                                            }
                                        }
                                        if let Some(v) = changes.voltage_set {
                                            if let Some(cmd) = plugin.ps_set_voltage(v) {
                                                command_queue.push_back(cmd);
                                            }
                                        }
                                        if let Some(v) = changes.current_set {
                                            if let Some(cmd) = plugin.ps_set_current(v) {
                                                command_queue.push_back(cmd);
                                            }
                                        }
                                        if let Some(v) = changes.ovp {
                                            if let Some(cmd) = plugin.ps_set_ovp(v) {
                                                command_queue.push_back(cmd);
                                            }
                                        }
                                        if let Some(v) = changes.ocp {
                                            if let Some(cmd) = plugin.ps_set_ocp(v) {
                                                command_queue.push_back(cmd);
                                            }
                                        }
                                        drop(plugin); // release before PS query

                                        let plugin = plugin_shared.lock().unwrap();
                                        if let Some(cmd) = plugin.ps_query_output() {
                                            command_queue.push_back(cmd.to_string());
                                        }
                                        drop(plugin);

                                        // Full poll every 10th PS poll
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

                        tokio::time::sleep(Duration::from_millis(1)).await;
                    } => {}
                }

                if shutting_down && drop_serial {
                    break;
                }
            }

            if *value_debug_shared.lock().unwrap() {
                println!("Cleaning up serial task");
            }
            let _ = poll.registry().deregister(&mut serial);
            drop(serial);
        });
    }
}
