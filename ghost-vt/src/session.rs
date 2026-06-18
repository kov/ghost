//! Session discovery and lifecycle: enumerate live sessions (pruning dead ones)
//! and kill a session by name.
//!
//! Liveness is determined by the per-session lock file: the host holds an
//! exclusive `flock` on `<session>/lock` for its whole life (acquired before it
//! daemonizes, so it covers startup too), and the kernel releases it when the
//! host exits or crashes. Discovery reads liveness from whether that lock can be
//! taken — no timing or pid-liveness guessing — and prunes only directories whose
//! lock is free (their host is gone).

use crate::paths;
use rustix::fs::{FlockOperation, flock};
use std::io;
use std::path::Path;

/// A live session, with the descriptive metadata a GUI uses to identify it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub name: String,
    pub pid: i32,
    /// Unix seconds at which the session was created, or `None` if unrecorded.
    pub created_at: Option<i64>,
    /// The current terminal title (OSC 0/2), empty if none has been set.
    pub title: String,
    /// The command the session runs (empty means the user's `$SHELL`).
    pub command: Vec<String>,
}

/// Liveness of a session directory, read from its lock file.
#[derive(Debug, PartialEq, Eq)]
enum HostState {
    /// A host holds the lock and has written its pid: a fully-up session.
    Live(i32),
    /// A host holds the lock but hasn't written its pid yet (still starting), or
    /// the lock file isn't there yet (directory mid-creation). Keep it, but it is
    /// not listable until the pid appears.
    Starting,
    /// No host holds the lock: a crash/exit leftover to prune.
    Dead,
}

/// List live sessions, pruning directories whose host is gone.
pub fn list() -> io::Result<Vec<SessionInfo>> {
    list_in(&paths::runtime_dir())
}

/// [`list`], but over an explicit runtime directory (so it can be tested against
/// a tempdir rather than the process's real XDG location).
fn list_in(runtime_dir: &Path) -> io::Result<Vec<SessionInfo>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(runtime_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        // Each session is a directory `<runtime>/<name>/` holding sock + lock + pid.
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let path = entry.path();
        match host_state(&path) {
            HostState::Live(pid) => {
                let meta = crate::meta::read(&path.join("meta")).unwrap_or_default();
                out.push(SessionInfo {
                    name,
                    pid,
                    created_at: Some(meta.created_at).filter(|&t| t != 0),
                    title: meta.title,
                    command: meta.command,
                });
            }
            HostState::Starting => {} // keep, but not yet listable
            HostState::Dead => {
                let _ = std::fs::remove_dir_all(&path);
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Read a session directory's liveness from its lock file. Trying to take the
/// lock non-blocking tells us whether a host still holds it: failure (would
/// block) means a host is alive, success means the lock is free and the session
/// is dead. The lock we may briefly acquire here is released as the file handle
/// drops at the end of this function.
fn host_state(session_dir: &Path) -> HostState {
    let lock = match std::fs::File::open(session_dir.join("lock")) {
        Ok(f) => f,
        // No lock file yet: a host is mid-creation (it makes the dir, then the
        // lock). Don't prune — that would race the starting host.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return HostState::Starting,
        // Any other error: be conservative and leave it alone.
        Err(_) => return HostState::Starting,
    };
    match flock(&lock, FlockOperation::NonBlockingLockExclusive) {
        // We took the lock: no host holds it -> dead.
        Ok(()) => HostState::Dead,
        // Held by a live host. List it once its pid is on disk.
        Err(e) if e == rustix::io::Errno::WOULDBLOCK => match read_pid_at(session_dir) {
            Some(pid) => HostState::Live(pid),
            None => HostState::Starting,
        },
        // Odd error testing the lock: don't prune.
        Err(_) => HostState::Starting,
    }
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
    read_pid_at(&paths::session_dir(name))
}

/// Read the pid from a session directory's pidfile.
fn read_pid_at(session_dir: &Path) -> Option<i32> {
    std::fs::read_to_string(session_dir.join("pid"))
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
    // The whole session directory (sock + lock + pid) goes at once.
    let _ = std::fs::remove_dir_all(paths::session_dir(name));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_uses_the_lock_to_keep_live_and_prune_dead_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mk = |name: &str| {
            let d = root.join(name);
            std::fs::create_dir_all(&d).unwrap();
            d
        };

        // Live: lock held + pid written -> listed, kept.
        let live = mk("live");
        let live_lock = std::fs::File::create(live.join("lock")).unwrap();
        flock(&live_lock, FlockOperation::NonBlockingLockExclusive).unwrap();
        std::fs::write(live.join("pid"), std::process::id().to_string()).unwrap();

        // Starting: lock held, no pid yet -> kept, but not listed.
        let starting = mk("starting");
        let starting_lock = std::fs::File::create(starting.join("lock")).unwrap();
        flock(&starting_lock, FlockOperation::NonBlockingLockExclusive).unwrap();

        // Dead: lock file present but unheld -> pruned (a stale pid must not save it).
        let dead = mk("dead");
        std::fs::File::create(dead.join("lock")).unwrap();
        std::fs::write(dead.join("pid"), "2147483646").unwrap();

        // Mid-creation: directory exists, no lock file yet -> kept.
        let creating = mk("creating");

        let names: Vec<String> = list_in(root).unwrap().into_iter().map(|s| s.name).collect();

        assert_eq!(names, vec!["live"], "only the fully-up session is listed");
        assert!(live.exists(), "live session was pruned");
        assert!(starting.exists(), "still-starting session was pruned");
        assert!(creating.exists(), "mid-creation session was pruned");
        assert!(!dead.exists(), "dead session was not pruned");

        // Hold the lock fds until the assertions are done.
        drop(live_lock);
        drop(starting_lock);
    }

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
