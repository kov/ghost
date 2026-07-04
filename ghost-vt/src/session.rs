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
/// Serializable so `ghost ls --json` can emit a listing that the remote-fleet
/// initiator parses back over the ssh transport.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionInfo {
    pub name: String,
    pub pid: i32,
    /// Unix seconds at which the session was created, or `None` if unrecorded.
    pub created_at: Option<i64>,
    /// The current terminal title (OSC 0/2), empty if none has been set.
    pub title: String,
    /// The command the session runs (empty means the user's `$SHELL`).
    pub command: Vec<String>,
    /// Whether a display client is currently attached to the session, read from
    /// the host's `attached` marker file. Lets a front-end separate sessions held
    /// elsewhere from genuinely detached ones.
    pub attached: bool,
    /// Whether the session rang the terminal bell while unattached and has not
    /// been switched to since, read from the host's `bell` marker file. Lets a
    /// front-end highlight a session with an unseen notification.
    pub bell: bool,
    /// The user-chosen display name (`ghost rename`), empty if never renamed.
    /// A label only — `name` remains the session's immutable identity (its
    /// directory, socket, and recording never move). Show [`Self::display`],
    /// key on `name`.
    pub display_name: String,
    /// The child's working directory for display (home collapsed to `~`),
    /// read from the durable descriptor — so it refreshes on the host's
    /// checkpoint/detach cadence, not per keystroke. `None` when unknown.
    pub cwd: Option<String>,
    /// The session's terminal grid `(cols, rows)`, refreshed by the host when a
    /// display client resizes it. Lets a fleet shape a session's tile correctly
    /// before (or without) observing it. `None` when unrecorded (metadata
    /// written before the field existed).
    pub size: Option<(u16, u16)>,
    /// The session's remote connection, if it is an ssh/mosh session (from
    /// [`crate::connection`]); `None` for a local session. Lets a fleet mark
    /// remote sessions. Read from the host's `meta`.
    pub connection: Option<crate::connection::ConnectionSpec>,
}

impl SessionInfo {
    /// The name a human should see: the display name when one was set, else the
    /// immutable session name.
    pub fn display(&self) -> &str {
        if self.display_name.is_empty() {
            &self.name
        } else {
            &self.display_name
        }
    }
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
    let mut out = list_in(&paths::runtime_dir())?;
    // Enrich with the durable descriptor's cwd (display metadata, like the
    // title): done here, not in `list_in`, so the directory-scan logic stays
    // testable against a plain tempdir.
    for s in &mut out {
        s.cwd = crate::descriptor::read(&s.name)
            .and_then(|d| d.cwd)
            .map(|p| display_path(&p));
    }
    Ok(out)
}

/// A path for human display: the user's home collapsed to `~`.
pub fn display_path(p: &Path) -> String {
    match dirs::home_dir().and_then(|h| p.strip_prefix(h).ok().map(|r| r.to_path_buf())) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
        None => p.display().to_string(),
    }
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
                    attached: path.join("attached").exists(),
                    bell: path.join("bell").exists(),
                    display_name: meta.display_name,
                    cwd: None, // filled by [`list`] from the descriptor
                    size: Some(meta.size).filter(|&s| s != (0, 0)),
                    connection: meta.connection,
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

/// Whether `name` is usable as a session's *display name* — a human label shown
/// in the UI and stored in `meta`, never a path component. Far looser than
/// [`valid_name`]: spaces, punctuation, and non-ASCII (accents, emoji) are all
/// fine, since the display name never becomes a directory or socket. Only the
/// empty string, over-long labels, and control characters are rejected — the
/// last because they could corrupt the terminal, the `meta` file, or the
/// `\u{1f}` separator the fleet uses to namespace remote-session ids.
pub fn valid_display_name(name: &str) -> bool {
    !name.is_empty() && name.chars().count() <= 64 && !name.chars().any(char::is_control)
}

/// Kill the named session's host (and thereby its child). Returns `false` if no
/// live session by that name exists.
///
/// A kill is the explicit throw-away verb: it also discards the session's
/// durable traces, so nothing offers to resurrect it. The host cannot do this
/// itself — the SIGTERM it receives here is indistinguishable from a logout's,
/// which must leave the session resurrectable — so the killer cleans up.
pub fn kill_session(name: &str) -> io::Result<bool> {
    let pid = match read_pid(name) {
        Some(pid) if pid_alive(pid) => pid,
        _ => {
            // Killing an already-dead session is how it gets forgotten.
            prune(name);
            discard(name);
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
    discard(name);
    Ok(true)
}

/// Discard `name`'s durable traces — the descriptor and the recording at its
/// default path — making the session unresurrectable and unremembered. Used
/// by every explicit end (kill, the child's own exit); an unclean death
/// (logout, reboot, crash) never reaches this.
pub fn discard(name: &str) {
    crate::descriptor::remove(name);
    let _ = std::fs::remove_file(crate::paths::recording_path(name));
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
    fn display_prefers_the_display_name_and_falls_back_to_the_id() {
        let mut s = SessionInfo {
            name: "sess-1".into(),
            pid: 1,
            created_at: None,
            title: String::new(),
            command: vec![],
            attached: false,
            bell: false,
            display_name: String::new(),
            cwd: None,
            size: None,
            connection: None,
        };
        assert_eq!(s.display(), "sess-1", "unset display falls back to the id");
        s.display_name = "build box".into();
        assert_eq!(s.display(), "build box");
    }

    #[test]
    fn session_info_round_trips_through_json() {
        // `ghost ls --json` serializes a listing; the remote-fleet initiator parses
        // it back, so every field must survive the round trip verbatim.
        let s = SessionInfo {
            name: "sess-1".into(),
            pid: 4321,
            created_at: Some(1_700_000_000),
            title: "vim".into(),
            command: vec!["vim".into(), "src/main.rs".into()],
            attached: true,
            bell: false,
            display_name: "editor".into(),
            cwd: Some("~/proj".into()),
            size: Some((120, 40)),
            connection: crate::connection::ConnectionSpec::parse_target("kov@box"),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: SessionInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn list_in_reports_the_display_name_from_meta() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let dir = root.join("sess-1");
        std::fs::create_dir_all(&dir).unwrap();
        let lock = std::fs::File::create(dir.join("lock")).unwrap();
        flock(&lock, FlockOperation::NonBlockingLockExclusive).unwrap();
        std::fs::write(dir.join("pid"), std::process::id().to_string()).unwrap();
        crate::meta::write(
            &dir.join("meta"),
            &crate::meta::Meta {
                created_at: 1,
                command: vec![],
                title: String::new(),
                display_name: "build box".into(),
                size: (120, 60),
                connection: None,
            },
        )
        .unwrap();

        let sessions = list_in(root).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0].display_name, "build box",
            "the display name travels from meta into the listing"
        );
        assert_eq!(
            sessions[0].size,
            Some((120, 60)),
            "the grid size travels from meta into the listing"
        );
        assert_eq!(
            sessions[0].connection, None,
            "a local session lists no connection"
        );
        drop(lock);
    }

    #[test]
    fn list_in_reports_an_ssh_sessions_connection() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let dir = root.join("sess-ssh");
        std::fs::create_dir_all(&dir).unwrap();
        let lock = std::fs::File::create(dir.join("lock")).unwrap();
        flock(&lock, FlockOperation::NonBlockingLockExclusive).unwrap();
        std::fs::write(dir.join("pid"), std::process::id().to_string()).unwrap();
        crate::meta::write(
            &dir.join("meta"),
            &crate::meta::Meta {
                created_at: 1,
                connection: crate::connection::ConnectionSpec::parse_target("kov@box"),
                ..Default::default()
            },
        )
        .unwrap();

        let sessions = list_in(root).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0].connection.as_ref().map(|c| c.target()),
            Some("kov@box".to_string()),
            "the connection travels from meta into the listing"
        );
        drop(lock);
    }

    #[test]
    fn list_in_reports_an_unrecorded_size_as_unknown() {
        // Metadata written before the size field existed reads as (0, 0); the
        // listing must surface that as "unknown", not a degenerate zero grid.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let dir = root.join("sess-1");
        std::fs::create_dir_all(&dir).unwrap();
        let lock = std::fs::File::create(dir.join("lock")).unwrap();
        flock(&lock, FlockOperation::NonBlockingLockExclusive).unwrap();
        std::fs::write(dir.join("pid"), std::process::id().to_string()).unwrap();
        std::fs::write(
            dir.join("meta"),
            br#"{"created_at":1,"command":[],"title":""}"#,
        )
        .unwrap();

        let sessions = list_in(root).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].size, None);
        drop(lock);
    }

    #[test]
    fn list_in_reports_attached_state_from_marker_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // A fully-up session (lock held + pid written) so it is listable.
        let mk_live = |name: &str| {
            let d = root.join(name);
            std::fs::create_dir_all(&d).unwrap();
            let lock = std::fs::File::create(d.join("lock")).unwrap();
            flock(&lock, FlockOperation::NonBlockingLockExclusive).unwrap();
            std::fs::write(d.join("pid"), std::process::id().to_string()).unwrap();
            (d, lock)
        };

        // Presence of the `attached` marker is the whole signal: the host writes
        // it while a display client is attached and removes it on detach.
        let (attached, attached_lock) = mk_live("attached");
        std::fs::write(attached.join("attached"), "").unwrap();
        let (_detached, detached_lock) = mk_live("detached");

        let sessions = list_in(root).unwrap();
        let by_name = |n: &str| sessions.iter().find(|s| s.name == n).unwrap();
        assert!(by_name("attached").attached, "marker present -> attached");
        assert!(!by_name("detached").attached, "no marker -> detached");

        // Hold the lock fds until the assertions are done.
        drop(attached_lock);
        drop(detached_lock);
    }

    #[test]
    fn list_in_reports_bell_state_from_marker_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let mk_live = |name: &str| {
            let d = root.join(name);
            std::fs::create_dir_all(&d).unwrap();
            let lock = std::fs::File::create(d.join("lock")).unwrap();
            flock(&lock, FlockOperation::NonBlockingLockExclusive).unwrap();
            std::fs::write(d.join("pid"), std::process::id().to_string()).unwrap();
            (d, lock)
        };

        // Presence of the `bell` marker is the whole signal: the host writes it
        // when the child rings the bell unattached and removes it on attach.
        let (rang, rang_lock) = mk_live("rang");
        std::fs::write(rang.join("bell"), "").unwrap();
        let (_quiet, quiet_lock) = mk_live("quiet");

        let sessions = list_in(root).unwrap();
        let by_name = |n: &str| sessions.iter().find(|s| s.name == n).unwrap();
        assert!(by_name("rang").bell, "marker present -> bell");
        assert!(!by_name("quiet").bell, "no marker -> no bell");

        // Hold the lock fds until the assertions are done.
        drop(rang_lock);
        drop(quiet_lock);
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

    #[test]
    fn valid_display_name_allows_spaces_and_unicode_but_not_control_or_empty() {
        // A display name is a label, not a path component: it may hold spaces,
        // punctuation, and non-ASCII.
        for ok in ["test space", "My Session!", "café ☕", "a", &"x".repeat(64)] {
            assert!(valid_display_name(ok), "{ok:?} should be a valid label");
        }
        for bad in [
            "",              // empty
            "tab\there",     // control char
            "new\nline",     // newline
            "unit\u{1f}sep", // the remote-id separator (a control char)
            &"x".repeat(65), // too long
        ] {
            assert!(!valid_display_name(bad), "{bad:?} should be rejected");
        }
    }
}
