//! Filesystem locations for sessions.
//!
//! Each session owns a directory `<runtime>/ghost/<name>/` holding its `sock`,
//! `pid`, and `lock`. The runtime base is `$XDG_RUNTIME_DIR` when set (Linux's
//! per-user tmpfs, wiped at logout); otherwise it is a durable per-user dir
//! chosen by [`runtime_dir`] — deliberately **not** the temp dir, which macOS
//! reaps every few days and would delete a live session's files out from under
//! its still-running host. Correctness therefore can't lean on the tree being
//! wiped: stale leftovers (a crashed host, or entries that outlive a reboot) are
//! pruned by the liveness check in [`crate::session::list`]. Grouping the
//! per-session files in one directory also makes renaming a session a single
//! atomic `rename(2)` of that directory.

use std::path::PathBuf;

/// The directory holding the per-session subdirectories.
pub fn runtime_dir() -> PathBuf {
    resolve_runtime_dir(
        std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from),
        dirs::state_dir(),
        dirs::data_local_dir(),
    )
}

/// Resolve the runtime base, kept pure so it can be unit-tested without poking
/// process env. An explicit `XDG_RUNTIME_DIR` wins everywhere (the Linux
/// standard and the test suite's isolation knob). With none set — the normal
/// case on macOS, where the variable is never present — fall back to a durable
/// per-user base the OS does **not** periodically reap, never the temp dir:
/// macOS's `dirhelper` sweeps `$TMPDIR` (`/var/folders/.../T`) every ~3 days,
/// which would delete a live session's `sock`/`pid`/`lock` out from under the
/// running host and hide it from discovery. `dirs` maps these to the right home
/// per platform — state dir (`~/.local/state`) on Linux, or data-local
/// (`~/Library/Application Support`, `%LOCALAPPDATA%`) where there is no state
/// dir. Stale entries are pruned by liveness (see [`crate::session::list`]), not
/// by relying on the directory being wiped.
fn resolve_runtime_dir(
    xdg_runtime: Option<PathBuf>,
    state: Option<PathBuf>,
    data_local: Option<PathBuf>,
) -> PathBuf {
    xdg_runtime
        .or(state)
        .or(data_local)
        .unwrap_or_else(std::env::temp_dir)
        .join("ghost")
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn honors_explicit_xdg_runtime_dir() {
        // An explicit XDG_RUNTIME_DIR wins on every platform: it is the Linux
        // standard (a per-user tmpfs wiped at logout) and the test suite's
        // isolation knob. Resolution must not depend on the durable fallbacks.
        let got = resolve_runtime_dir(
            Some(PathBuf::from("/run/user/501")),
            Some(PathBuf::from("/home/u/.local/state")),
            Some(PathBuf::from("/home/u/.local/share")),
        );
        assert_eq!(got, PathBuf::from("/run/user/501/ghost"));
    }

    #[test]
    fn falls_back_to_durable_base_not_reapable_temp() {
        // Regression: on macOS XDG_RUNTIME_DIR is unset, and the old fallback
        // used $TMPDIR (`/var/folders/.../T`), which macOS's dirhelper reaps
        // every ~3 days — silently deleting a live session's sock/pid/lock out
        // from under the running host, so `ghost ls` (and the GUI) report
        // nothing. The fallback must be a durable per-user base instead. With no
        // state dir (macOS), the data-local base (`~/Library/Application
        // Support`) is used.
        let app_support = PathBuf::from("/Users/u/Library/Application Support");
        let got = resolve_runtime_dir(None, None, Some(app_support.clone()));
        assert_eq!(got, app_support.join("ghost"));
        assert!(
            !got.starts_with(std::env::temp_dir()),
            "runtime dir {got:?} must not live under the reapable temp dir"
        );
    }

    #[test]
    fn prefers_state_over_data_local_without_xdg_runtime() {
        // Linux without XDG_RUNTIME_DIR set: the state dir (`~/.local/state`) is
        // the right durable home, preferred over the data-local share dir.
        let got = resolve_runtime_dir(
            None,
            Some(PathBuf::from("/home/u/.local/state")),
            Some(PathBuf::from("/home/u/.local/share")),
        );
        assert_eq!(got, PathBuf::from("/home/u/.local/state/ghost"));
    }
}
