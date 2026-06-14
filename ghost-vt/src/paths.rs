//! Filesystem locations for sessions.
//!
//! Per-session sockets and pidfiles live under `$XDG_RUNTIME_DIR/ghost`
//! (falling back to `/tmp/ghost-<uid>` when the variable is unset). That
//! directory is ephemeral by design — wiped on reboot — which doubles as
//! free stale-socket cleanup, since a session never outlives its host's kernel.

use std::path::PathBuf;

/// The directory holding per-session sockets and pidfiles.
pub fn runtime_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let uid = unsafe { libc::getuid() };
            std::env::temp_dir().join(format!("ghost-{uid}"))
        });
    base.join("ghost")
}

/// Create the runtime directory if needed and return it.
pub fn ensure_runtime_dir() -> std::io::Result<PathBuf> {
    let dir = runtime_dir();
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Path of the control socket for the named session.
pub fn socket_path(name: &str) -> PathBuf {
    runtime_dir().join(format!("{name}.sock"))
}

/// Path of the pidfile for the named session.
pub fn pid_path(name: &str) -> PathBuf {
    runtime_dir().join(format!("{name}.pid"))
}
