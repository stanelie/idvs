use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::network::interface_ip;

macro_rules! emit_log {
    ($tx:expr, $($arg:tt)*) => {{
        let msg = format!($($arg)*);
        let _ = $tx.send(WorkerEvent::Log(msg));
    }};
}

/// Commands sent from the UI to the worker
pub enum WorkerCmd {
    Start(Config),
    Stop,
}

/// Events emitted by the worker to the UI
pub enum WorkerEvent {
    Log(String),
    StateChanged(WorkerState),
    PtpStatus(PtpStatus),
}

#[derive(Clone, Debug, PartialEq)]
#[allow(dead_code)]
pub enum WorkerState {
    Idle,
    StartingStatime,
    WaitingForClock,
    ConfiguringAlsa,
    AddingPipeWireNode,
    Running { statime_pid: u32 },
    Stopping,
    Error(String),
}

#[derive(Clone, Debug, Default)]
pub struct PtpStatus {
    /// Offset from PTP master in nanoseconds
    pub offset_ns: f64,
    /// Mean network delay in nanoseconds
    pub delay_ns: f64,
    /// True when we have received at least one status update
    pub has_data: bool,
    /// True when offset is small enough to consider synced
    pub synced: bool,
}

// ---------------------------------------------------------------------------
// Root helper connection
// ---------------------------------------------------------------------------

struct HelperConn {
    _process: Child,
    writer: UnixStream,
    reader: BufReader<UnixStream>,
}

impl HelperConn {
    /// Send a single-line command and return the single-line response.
    fn send(&mut self, cmd: &str) -> Result<String, String> {
        writeln!(self.writer, "{cmd}").map_err(|e| format!("helper write: {e}"))?;
        let mut line = String::new();
        self.reader
            .read_line(&mut line)
            .map_err(|e| format!("helper read: {e}"))?;
        Ok(line.trim().to_string())
    }
}

// ---------------------------------------------------------------------------
// Worker entry point
// ---------------------------------------------------------------------------

/// Run the worker in a thread. Returns a (cmd_tx, event_rx) pair.
pub fn spawn_worker() -> (Sender<WorkerCmd>, Receiver<WorkerEvent>) {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<WorkerCmd>();
    let (event_tx, event_rx) = std::sync::mpsc::channel::<WorkerEvent>();
    std::thread::spawn(move || worker_main(cmd_rx, event_tx));
    (cmd_tx, event_rx)
}

fn worker_main(cmd_rx: Receiver<WorkerCmd>, event_tx: Sender<WorkerEvent>) {
    let socket_path = format!("/tmp/.idvs-helper-{}.sock", std::process::id());

    emit_log!(
        event_tx,
        "Launching root helper — an authentication dialog will appear..."
    );
    let mut helper = spawn_helper(&socket_path, &event_tx);
    if helper.is_none() {
        emit_log!(
            event_tx,
            "Warning: root helper unavailable — Start will fail."
        );
    }

    let mut statime_pid: Option<u32> = None;
    // Tracks whether we wrote the PipeWire dante config so we know to clean it up.
    let mut pw_config_written = false;
    let mut last_ptp_poll = Instant::now() - Duration::from_secs(10);
    let mut last_status_poll = Instant::now() - Duration::from_secs(10);
    let mut observation_path: Option<String> = None;

    loop {
        match cmd_rx.try_recv() {
            Ok(WorkerCmd::Start(config)) => {
                let _ = event_tx.send(WorkerEvent::StateChanged(WorkerState::StartingStatime));
                match do_start(&config, &event_tx, &mut helper) {
                    Ok((pid, obs_path, pw_written)) => {
                        statime_pid = Some(pid);
                        observation_path = Some(obs_path);
                        pw_config_written = pw_written;
                        let _ = event_tx.send(WorkerEvent::StateChanged(
                            WorkerState::Running { statime_pid: pid },
                        ));
                    }
                    Err(e) => {
                        let _ = event_tx
                            .send(WorkerEvent::StateChanged(WorkerState::Error(e.clone())));
                        let _ = event_tx.send(WorkerEvent::Log(format!("[error] {e}")));
                    }
                }
            }
            Ok(WorkerCmd::Stop) => {
                let _ = event_tx.send(WorkerEvent::StateChanged(WorkerState::Stopping));
                do_stop(pw_config_written, &event_tx, &mut helper);
                statime_pid = None;
                pw_config_written = false;
                observation_path = None;
                let _ = event_tx.send(WorkerEvent::StateChanged(WorkerState::Idle));
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
        }

        // Periodically check if statime is still alive via the helper
        if statime_pid.is_some() && last_status_poll.elapsed() >= Duration::from_secs(5) {
            if let Some(ref mut h) = helper {
                if let Ok(resp) = h.send("STATIME_STATUS") {
                    if resp == "OK pid=none" {
                        emit_log!(event_tx, "[statime] Process exited unexpectedly");
                        let _ = event_tx.send(WorkerEvent::StateChanged(WorkerState::Error(
                            "statime exited unexpectedly".to_string(),
                        )));
                        statime_pid = None;
                        observation_path = None;
                    }
                }
            }
            last_status_poll = Instant::now();
        }

        // Poll PTP status periodically
        if statime_pid.is_some() && last_ptp_poll.elapsed() >= Duration::from_secs(1) {
            if let Some(ref obs_path) = observation_path {
                if let Some(status) = poll_ptp_status(obs_path) {
                    let _ = event_tx.send(WorkerEvent::PtpStatus(status));
                }
            }
            last_ptp_poll = Instant::now();
        }

        std::thread::sleep(Duration::from_millis(100));
    }

    if let Some(ref mut h) = helper {
        let _ = h.send("QUIT");
    }
}

// ---------------------------------------------------------------------------
// Helper spawn / connect
// ---------------------------------------------------------------------------

fn spawn_helper(socket_path: &str, event_tx: &Sender<WorkerEvent>) -> Option<HelperConn> {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            emit_log!(event_tx, "[helper] Cannot locate own binary: {e}");
            return None;
        }
    };

    let child = match Command::new("pkexec")
        .args([exe.to_string_lossy().as_ref(), "--helper", socket_path])
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            emit_log!(
                event_tx,
                "[helper] pkexec launch failed: {e} — is polkit installed?"
            );
            return None;
        }
    };

    if !wait_for_path(socket_path, Duration::from_secs(30)) {
        emit_log!(
            event_tx,
            "[helper] Timed out waiting for helper socket — authentication may have been denied."
        );
        return None;
    }

    let stream = match UnixStream::connect(socket_path) {
        Ok(s) => s,
        Err(e) => {
            emit_log!(event_tx, "[helper] Connect failed: {e}");
            return None;
        }
    };

    let writer = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            emit_log!(event_tx, "[helper] Stream clone failed: {e}");
            return None;
        }
    };

    let mut conn = HelperConn {
        _process: child,
        writer,
        reader: BufReader::new(stream),
    };

    match conn.send("CHECK") {
        Ok(resp) if resp == "OK" => {
            emit_log!(event_tx, "Root helper ready — no further authentication needed.");
            Some(conn)
        }
        Ok(resp) => {
            emit_log!(event_tx, "[helper] Unexpected CHECK response: {resp}");
            None
        }
        Err(e) => {
            emit_log!(event_tx, "[helper] CHECK failed: {e}");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Start / stop logic
// ---------------------------------------------------------------------------

/// Returns (statime_pid, observation_socket_path, pw_config_written).
fn do_start(
    config: &Config,
    event_tx: &Sender<WorkerEvent>,
    helper: &mut Option<HelperConn>,
) -> Result<(u32, String, bool), String> {
    // --- Validate prerequisites ---
    if config.interface.is_empty() {
        return Err("No network interface selected".to_string());
    }
    if !config.statime_bin.exists() {
        return Err(format!(
            "statime binary not found: {}",
            config.statime_bin.display()
        ));
    }
    if !config.inferno_so.exists() {
        return Err(format!(
            "Inferno ALSA plugin not found: {}",
            config.inferno_so.display()
        ));
    }

    let bind_ip = match interface_ip(&config.interface) {
        Some(ip) => ip.to_string(),
        None => {
            return Err(format!(
                "Could not find IPv4 address for interface '{}'",
                config.interface
            ))
        }
    };

    // --- Write statime config ---
    let statime_cfg_path = "/tmp/idvs-statime.toml";
    std::fs::write(statime_cfg_path, config.statime_config())
        .map_err(|e| format!("Failed to write statime config: {e}"))?;
    emit_log!(event_tx, "Wrote statime config to {statime_cfg_path}");

    // --- Remove stale clock/observation sockets ---
    for path in [config.clock_path.as_str(), config.observation_path.as_str()] {
        if Path::new(path).exists() {
            let _ = std::fs::remove_file(path);
        }
    }

    // --- Install ALSA plugin .so via helper ---
    install_alsa_plugin_via_helper(config, event_tx, helper);

    // --- Launch statime via helper ---
    emit_log!(
        event_tx,
        "Starting statime on interface '{}'...",
        config.interface
    );
    let statime_pid = match helper {
        Some(ref mut h) => {
            let cmd = format!(
                "START_STATIME\t{}\t{statime_cfg_path}",
                config.statime_bin.display()
            );
            match h.send(&cmd) {
                Ok(resp) if resp.starts_with("OK pid=") => resp["OK pid=".len()..]
                    .parse::<u32>()
                    .map_err(|_| format!("Bad PID in helper response: {resp}"))?,
                Ok(resp) => return Err(format!("Helper failed to start statime: {resp}")),
                Err(e) => return Err(format!("Helper unreachable: {e}")),
            }
        }
        None => return Err("Root helper is not running — cannot start statime.".to_string()),
    };
    emit_log!(event_tx, "statime started (PID {statime_pid})");

    // --- Wait for usrvclock socket (up to 15 seconds) ---
    emit_log!(
        event_tx,
        "Waiting for PTP clock socket at '{}'...",
        config.clock_path
    );
    if !wait_for_path(&config.clock_path, Duration::from_secs(15)) {
        return Err(format!(
            "Timed out waiting for clock socket '{}'. Is statime running?",
            config.clock_path
        ));
    }
    emit_log!(event_tx, "PTP clock socket ready");

    // --- Write ALSA configuration to ~/.asoundrc ---
    emit_log!(event_tx, "Writing ALSA configuration...");
    let alsa_cfg = config.alsa_config(&bind_ip);
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let asoundrc_path = format!("{home}/.asoundrc");
    std::fs::write(&asoundrc_path, &alsa_cfg)
        .map_err(|e| format!("Failed to write ~/.asoundrc: {e}"))?;
    emit_log!(event_tx, "ALSA config written to {asoundrc_path}");

    // --- Set up PipeWire (clock override + dante sink/source nodes) ---
    // Write both the clock syscall drop-in and the dante node config, then do
    // a single PipeWire restart so both take effect at once.
    let pw_config_written = if config.use_pipewire {
        setup_pipewire(config, event_tx)
    } else {
        false
    };

    emit_log!(event_tx, "--- Startup complete ---");
    emit_log!(
        event_tx,
        "ALSA device: dante (or dante_plug for format conversion)"
    );
    if pw_config_written {
        emit_log!(
            event_tx,
            "PipeWire nodes: '{}-sink' / '{}-source' visible in audio mixer",
            config.device_name,
            config.device_name,
        );
    }

    Ok((statime_pid, config.observation_path.clone(), pw_config_written))
}

fn do_stop(
    pw_config_written: bool,
    event_tx: &Sender<WorkerEvent>,
    helper: &mut Option<HelperConn>,
) {
    // Stop statime via helper (sends SIGTERM → SIGKILL)
    emit_log!(event_tx, "Stopping statime...");
    if let Some(ref mut h) = helper {
        match h.send("STOP_STATIME") {
            Ok(_) => emit_log!(event_tx, "statime stopped"),
            Err(e) => emit_log!(event_tx, "Warning: STOP_STATIME failed: {e}"),
        }
    } else {
        emit_log!(
            event_tx,
            "Warning: root helper not running — statime may still be running."
        );
    }

    // Remove PipeWire dante config and restart PipeWire to drop the nodes
    if pw_config_written {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        let conf_path = pipewire_dante_conf_path(&home);
        if std::fs::remove_file(&conf_path).is_ok() {
            emit_log!(event_tx, "Removed PipeWire Dante config, restarting PipeWire...");
            let _ = Command::new("systemctl")
                .args(["--user", "daemon-reload"])
                .status();
            let _ = Command::new("systemctl")
                .args(["--user", "restart", "pipewire"])
                .status();
            emit_log!(event_tx, "PipeWire restarted");
        }
    }
}

// ---------------------------------------------------------------------------
// PipeWire setup
// ---------------------------------------------------------------------------

/// Write the clock-syscall drop-in (if missing) and the dante sink/source
/// node config, then restart PipeWire once so both take effect.
///
/// Returns true if the dante config was successfully written (used to decide
/// whether to remove it on stop).
fn setup_pipewire(config: &Config, event_tx: &Sender<WorkerEvent>) -> bool {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let mut need_restart = false;

    // Clock syscall override (only written once, stays forever)
    let clock_override_dir = format!("{home}/.config/systemd/user/pipewire.service.d");
    let clock_override_file = format!("{clock_override_dir}/override.conf");
    if !Path::new(&clock_override_file).exists() {
        let _ = std::fs::create_dir_all(&clock_override_dir);
        let content = "[Service]\nSystemCallFilter=@clock\n";
        if std::fs::write(&clock_override_file, content).is_ok() {
            emit_log!(event_tx, "Installed PipeWire clock syscall override");
            need_restart = true;
        } else {
            emit_log!(event_tx, "Warning: could not write PipeWire clock override");
            emit_log!(
                event_tx,
                "  Run manually: systemctl --user daemon-reload && systemctl --user restart pipewire"
            );
        }
    }

    // Dante sink + source node config.
    // Uses api.alsa.pcm.sink / api.alsa.pcm.source (real PipeWire factories)
    // with node.always-process=true so PipeWire keeps the ALSA device open
    // continuously — this is what keeps the Dante device visible on the network.
    let conf_dir = format!("{home}/.config/pipewire/pipewire.conf.d");
    let conf_path = pipewire_dante_conf_path(&home);
    let conf_content = pipewire_dante_config(config);

    let _ = std::fs::create_dir_all(&conf_dir);
    match std::fs::write(&conf_path, &conf_content) {
        Ok(_) => {
            emit_log!(event_tx, "Wrote PipeWire Dante node config to {conf_path}");
            need_restart = true;
        }
        Err(e) => {
            emit_log!(event_tx, "Warning: could not write PipeWire Dante config: {e}");
            emit_log!(
                event_tx,
                "  PipeWire nodes will not be created; Dante device may not appear in audio router"
            );
            // Return false — caller shouldn't try to clean up a file that wasn't written
            if need_restart {
                // Still restart for the clock override
                do_pipewire_restart(event_tx);
            }
            return false;
        }
    }

    if need_restart {
        do_pipewire_restart(event_tx);
        // Give PipeWire a moment to come up and load the new config
        std::thread::sleep(Duration::from_secs(2));
    }

    true
}

fn do_pipewire_restart(event_tx: &Sender<WorkerEvent>) {
    emit_log!(event_tx, "Restarting PipeWire to apply config...");
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    let _ = Command::new("systemctl")
        .args(["--user", "restart", "pipewire"])
        .status();
    emit_log!(event_tx, "PipeWire restarted");
}

fn pipewire_dante_conf_path(home: &str) -> String {
    format!("{home}/.config/pipewire/pipewire.conf.d/idvs-dante.conf")
}

/// Generate PipeWire config that creates persistent sink + source nodes backed
/// by the 'dante' ALSA device.  node.always-process=true keeps the ALSA
/// device open at all times, which is what keeps the Dante device on the
/// network even when no app is actively playing/recording.
fn pipewire_dante_config(config: &Config) -> String {
    format!(
        r#"context.objects = [
  {{ factory = adapter
    args = {{
      factory.name                    = api.alsa.pcm.sink
      node.name                       = "{name}-sink"
      node.description                = "{name} (Dante Playback)"
      media.class                     = Audio/Sink
      api.alsa.path                   = dante
      api.alsa.headroom               = 0
      audio.channels                  = {tx}
      audio.rate                      = {rate}
      object.linger                   = true
      node.always-process             = true
      session.suspend-timeout-seconds = 0
    }}
  }}
  {{ factory = adapter
    args = {{
      factory.name                    = api.alsa.pcm.source
      node.name                       = "{name}-source"
      node.description                = "{name} (Dante Capture)"
      media.class                     = Audio/Source
      api.alsa.path                   = dante
      api.alsa.headroom               = 0
      audio.channels                  = {rx}
      audio.rate                      = {rate}
      object.linger                   = true
      node.always-process             = true
      session.suspend-timeout-seconds = 0
    }}
  }}
]
"#,
        name = config.device_name,
        tx = config.tx_channels,
        rx = config.rx_channels,
        rate = config.sample_rate,
    )
}

// ---------------------------------------------------------------------------
// ALSA plugin installation
// ---------------------------------------------------------------------------

fn install_alsa_plugin_via_helper(
    config: &Config,
    event_tx: &Sender<WorkerEvent>,
    helper: &mut Option<HelperConn>,
) {
    let so_src = &config.inferno_so;

    if !so_src.exists() {
        emit_log!(
            event_tx,
            "Warning: inferno plugin not found at '{}' — skipping ALSA plugin install.",
            so_src.display()
        );
        emit_log!(event_tx, "  Set the correct path in Advanced settings.");
        return;
    }

    let alsa_lib_dirs = [
        "/usr/lib/x86_64-linux-gnu/alsa-lib",
        "/usr/lib/aarch64-linux-gnu/alsa-lib",
        "/usr/lib/arm-linux-gnueabihf/alsa-lib",
        "/usr/lib64/alsa-lib",
        "/usr/lib/alsa-lib",
    ];

    let alsa_lib_dir = match alsa_lib_dirs.iter().find(|d| Path::new(d).is_dir()) {
        Some(d) => d,
        None => {
            emit_log!(
                event_tx,
                "Warning: could not find system ALSA lib dir — inferno plugin not installed system-wide."
            );
            return;
        }
    };

    let so_dst = format!("{alsa_lib_dir}/libasound_module_pcm_inferno.so");

    // Skip if already up to date (size check as quick heuristic)
    if Path::new(&so_dst).exists() {
        let src_size = std::fs::metadata(so_src).map(|m| m.len()).unwrap_or(0);
        let dst_size = std::fs::metadata(&so_dst).map(|m| m.len()).unwrap_or(1);
        if src_size == dst_size {
            emit_log!(event_tx, "ALSA plugin already installed at {so_dst}");
            return;
        }
        emit_log!(event_tx, "Updating ALSA plugin at {so_dst}...");
    } else {
        emit_log!(event_tx, "Installing ALSA plugin to {so_dst}...");
    }

    match helper {
        Some(ref mut h) => {
            let cmd = format!("INSTALL_PLUGIN\t{}\t{so_dst}", so_src.display());
            match h.send(&cmd) {
                Ok(resp) if resp == "OK" => {
                    emit_log!(event_tx, "ALSA plugin installed successfully")
                }
                Ok(resp) => emit_log!(
                    event_tx,
                    "Warning: ALSA plugin install failed: {resp} — device may not appear in all apps"
                ),
                Err(e) => emit_log!(
                    event_tx,
                    "Warning: ALSA plugin install failed: {e} — device may not appear in all apps"
                ),
            }
        }
        None => emit_log!(
            event_tx,
            "Warning: root helper not running — ALSA plugin not installed system-wide."
        ),
    }
}

// ---------------------------------------------------------------------------
// PTP observation socket polling
// ---------------------------------------------------------------------------

fn poll_ptp_status(socket_path: &str) -> Option<PtpStatus> {
    use std::io::Read;
    let mut stream = UnixStream::connect(socket_path).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .ok()?;

    let mut data = Vec::new();
    stream.read_to_end(&mut data).ok()?;

    let json: serde_json::Value = serde_json::from_slice(&data).ok()?;
    let current_ds = json.pointer("/instance/current_ds")?;

    let offset_bits = current_ds.get("offset_from_master")?.as_i64()?;
    let delay_bits = current_ds.get("mean_delay")?.as_i64()?;
    let steps_removed = current_ds
        .get("steps_removed")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u16;

    let offset_ns = offset_bits as f64 / 4_294_967_296.0;
    let delay_ns = delay_bits as f64 / 4_294_967_296.0;
    let synced = offset_ns.abs() < 500_000.0 && steps_removed > 0;

    Some(PtpStatus {
        offset_ns,
        delay_ns,
        has_data: true,
        synced,
    })
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

fn wait_for_path(path: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if Path::new(path).exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    false
}
