//! Session discovery and lifecycle: enumerate live sessions (pruning dead ones)
//! and kill a session by name.
//!
//! Liveness is determined from the per-session pidfile (`kill(pid, 0)`), and
//! stale socket/pid files left by a crashed host are removed on sight.

use crate::paths;
use std::io;

/// A live session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub name: String,
    pub pid: i32,
}

/// List live sessions, pruning stale files for hosts that are gone.
pub fn list() -> io::Result<Vec<SessionInfo>> {
    let dir = paths::runtime_dir();
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("sock") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()).map(str::to_owned) else {
            continue;
        };
        match read_pid(&name) {
            Some(pid) if pid_alive(pid) => out.push(SessionInfo { name, pid }),
            _ => prune(&name),
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Kill the named session's host (and thereby its child). Returns `false` if no
/// live session by that name exists.
pub fn kill_session(name: &str) -> io::Result<bool> {
    let pid = match read_pid(name) {
        Some(pid) if pid_alive(pid) => pid,
        _ => {
            prune(name);
            return Ok(false);
        }
    };
    unsafe { libc::kill(pid, libc::SIGTERM) };
    // Wait for the host to exit and clean up after itself, then prune as a
    // backstop. Polling kill(pid, 0) is the standard way to wait on a process
    // that is not our child.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while pid_alive(pid) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    prune(name);
    Ok(true)
}

fn read_pid(name: &str) -> Option<i32> {
    std::fs::read_to_string(paths::pid_path(name))
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn pid_alive(pid: i32) -> bool {
    // Signal 0 performs error checking without sending a signal. EPERM means the
    // process exists but we may not signal it — still "alive" for our purposes.
    unsafe {
        libc::kill(pid, 0) == 0
            || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

fn prune(name: &str) {
    let _ = std::fs::remove_file(paths::socket_path(name));
    let _ = std::fs::remove_file(paths::pid_path(name));
}
