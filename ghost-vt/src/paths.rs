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

/// Path of the liveness lock for the named session. The host holds an exclusive
/// `flock` on this file for its whole life; session discovery reads liveness from
/// whether the lock can be taken (see [`crate::session::list`]).
pub fn lock_path(name: &str) -> PathBuf {
    session_dir(name).join("lock")
}

/// Path of the metadata file for the named session (creation time, command, and
/// live title), written by the host and read by discovery for the GUI sidebar.
pub fn meta_path(name: &str) -> PathBuf {
    session_dir(name).join("meta")
}

/// Path of the attach marker for the named session. The host keeps this file
/// present exactly while a display client is attached; its presence is how
/// discovery reports [`crate::session::SessionInfo::attached`] so a front-end can
/// tell "open elsewhere" from "detached".
pub fn attached_path(name: &str) -> PathBuf {
    session_dir(name).join("attached")
}

/// Path of the bell marker for the named session. The host creates this file when
/// the child rings the terminal bell (BEL) while no display client is attached,
/// and removes it when a client attaches; its presence is how discovery reports
/// [`crate::session::SessionInfo::bell`] so a front-end can highlight a session
/// with an unseen notification.
pub fn bell_path(name: &str) -> PathBuf {
    session_dir(name).join("bell")
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

/// The durable config directory (`$XDG_CONFIG_HOME/ghost`, falling back to
/// `~/.config/ghost`). Holds user-editable settings (e.g. the GUI frontend's
/// `gtk.toml`); the core/CLI do not require anything here.
pub fn config_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            home.join(".config")
        });
    base.join("ghost")
}

/// Path of the recording for the named session.
pub fn recording_path(name: &str) -> PathBuf {
    data_dir()
        .join("recordings")
        .join(format!("{name}.ghostrec"))
}
