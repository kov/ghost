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
        let entry = entry?;
        // Each session is a directory `<runtime>/<name>/` holding sock + pid.
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
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

/// Whether `name` is usable as a session name. A name becomes a directory and a
/// socket filename, so it must be a single, safe path component: non-empty, not
/// over-long, no separators or `.`/`..`, and restricted to an unambiguous set of
/// characters.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
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
        libc::kill(pid, 0) == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
}

fn prune(name: &str) {
    // The whole session directory (sock + pid) goes at once.
    let _ = std::fs::remove_dir_all(paths::session_dir(name));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_name_accepts_safe_names_and_rejects_unsafe() {
        for ok in ["work", "ghost-1234", "my_session.2", "a", &"x".repeat(64)] {
            assert!(valid_name(ok), "{ok:?} should be valid");
        }
        for bad in [
            "",              // empty
            ".",             // current dir
            "..",            // parent dir
            "a/b",           // path separator
            "with space",    // whitespace
            "tab\t",         // control char
            "emoji😀",       // non-ascii
            &"x".repeat(65), // too long
        ] {
            assert!(!valid_name(bad), "{bad:?} should be rejected");
        }
    }
}
