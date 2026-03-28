//! Root helper process — launched once at app startup via pkexec.
//! Accepts commands over a Unix socket and performs all operations
//! that require root privileges (starting/stopping statime, installing
//! the ALSA plugin). The socket uses 0o666 permissions so the invoking
//! user can connect; the PID-scoped path makes it non-guessable.
//!
//! Protocol: newline-terminated text commands, tab-separated args.
//! Each command gets a single-line response: "OK ..." or "ERR ...".
//!
//! Commands:
//!   CHECK                        — health check
//!   INSTALL_PLUGIN\t<src>\t<dst> — copy inferno .so to system ALSA dir
//!   START_STATIME\t<bin>\t<cfg>  — spawn statime, responds with "OK pid=<n>"
//!   STOP_STATIME                 — terminate statime
//!   STATIME_STATUS               — "OK pid=<n>" or "OK pid=none"
//!   QUIT                         — clean up and exit

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;

pub fn run_helper(socket_path: &str) {
    // Bind to a temp path first, set permissions there, then rename atomically
    // to the final path. This closes the race window: the app's wait_for_path
    // only fires once the socket is at its final path with correct permissions.
    // We use 0o666 so the invoking user can connect without needing chown
    // (which requires knowing PKEXEC_UID, not reliably set by all polkit versions).
    let temp_path = format!("{socket_path}.setup");
    let _ = std::fs::remove_file(&temp_path);
    let _ = std::fs::remove_file(socket_path);

    let listener = match UnixListener::bind(&temp_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("idvs-helper: bind {temp_path}: {e}");
            return;
        }
    };

    // 0o666: any local user can connect. The socket path includes the app's
    // PID so it is not guessable by other users.
    let _ = std::fs::set_permissions(&temp_path, std::fs::Permissions::from_mode(0o666));

    // Atomic rename — app sees the socket only after permissions are set
    if let Err(e) = std::fs::rename(&temp_path, socket_path) {
        eprintln!("idvs-helper: rename to {socket_path}: {e}");
        let _ = std::fs::remove_file(&temp_path);
        return;
    }

    // Accept exactly one connection — the idvs GUI process
    match listener.accept() {
        Ok((stream, _)) => handle_connection(stream),
        Err(e) => eprintln!("idvs-helper: accept: {e}"),
    }

    let _ = std::fs::remove_file(socket_path);
}

fn handle_connection(stream: std::os::unix::net::UnixStream) {
    let mut writer = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    let reader = BufReader::new(stream);
    let mut statime: Option<Child> = None;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let trimmed = line.trim();
        let response = dispatch(trimmed, &mut statime);
        let quit = trimmed == "QUIT";
        let _ = writeln!(writer, "{response}");
        if quit {
            break;
        }
    }

    // Connection dropped or QUIT — terminate statime
    terminate_statime(&mut statime);
}

fn dispatch(cmd: &str, statime: &mut Option<Child>) -> String {
    let parts: Vec<&str> = cmd.splitn(3, '\t').collect();
    match parts.first().copied().unwrap_or("") {
        "CHECK" => "OK".to_string(),

        "INSTALL_PLUGIN" => {
            if parts.len() < 3 {
                return "ERR missing arguments".to_string();
            }
            match std::fs::copy(parts[1], parts[2]) {
                Ok(_) => "OK".to_string(),
                Err(e) => format!("ERR {e}"),
            }
        }

        "START_STATIME" => {
            if parts.len() < 3 {
                return "ERR missing arguments".to_string();
            }
            terminate_statime(statime);
            match Command::new(parts[1])
                .args(["-c", parts[2]])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => {
                    let pid = child.id();
                    *statime = Some(child);
                    format!("OK pid={pid}")
                }
                Err(e) => format!("ERR {e}"),
            }
        }

        "STOP_STATIME" => {
            terminate_statime(statime);
            "OK".to_string()
        }

        "STATIME_STATUS" => match statime {
            Some(ref mut child) => match child.try_wait() {
                Ok(Some(_)) => {
                    *statime = None;
                    "OK pid=none".to_string()
                }
                _ => format!("OK pid={}", child.id()),
            },
            None => "OK pid=none".to_string(),
        },

        "QUIT" => "OK".to_string(),

        _ => "ERR unknown command".to_string(),
    }
}

fn terminate_statime(statime: &mut Option<Child>) {
    if let Some(ref mut child) = *statime {
        let pid = Pid::from_raw(child.id() as i32);
        let _ = signal::kill(pid, Signal::SIGTERM);
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                _ => {}
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    *statime = None;
}
