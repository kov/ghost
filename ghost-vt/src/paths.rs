//! Filesystem locations for sessions.
//!
//! Each session owns a directory `$XDG_RUNTIME_DIR/ghost/<name>/` holding its
//! `sock` and `pid` (the base falls back to `/tmp/ghost-<uid>` when the variable
//! is unset). That tree is ephemeral by design — wiped on reboot — which doubles
//! as free stale-socket cleanup, since a session never outlives its host's
//! kernel. Grouping the per-session files in one directory also makes renaming a
//! session a single atomic `rename(2)` of that directory.

use std::path::PathBuf;

/// The directory holding the per-session subdirectories.
pub fn runtime_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let uid = unsafe { libc::getuid() };
            std::env::temp_dir().join(format!("ghost-{uid}"))
        });
    base.join("ghost")
}

/// The directory holding one session's `sock` and `pid`.
pub fn session_dir(name: &str) -> PathBuf {
    runtime_dir().join(name)
}

/// Create the session's directory (and the runtime root) if needed; return it.
pub fn ensure_session_dir(name: &str) -> std::io::Result<PathBuf> {
    let dir = session_dir(name);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Path of the control socket for the named session.
pub fn socket_path(name: &str) -> PathBuf {
    session_dir(name).join("sock")
}

/// Path of the pidfile for the named session.
pub fn pid_path(name: &str) -> PathBuf {
    session_dir(name).join("pid")
}

/// The durable data directory (`$XDG_DATA_HOME/ghost`, falling back to
/// `~/.local/share/ghost`). Unlike [`runtime_dir`], this survives reboot — it
/// holds recordings, which are archival.
pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            home.join(".local").join("share")
        });
    base.join("ghost")
}

/// Path of the recording for the named session.
pub fn recording_path(name: &str) -> PathBuf {
    data_dir()
        .join("recordings")
        .join(format!("{name}.ghostrec"))
}
