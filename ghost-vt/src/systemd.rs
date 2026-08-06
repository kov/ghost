//! Surviving a logout on a systemd desktop.
//!
//! A host is daemonized — double-fork, `setsid`, reparented to init — and none
//! of that saves it. On a systemd desktop a process is killed by its *cgroup*,
//! not its parentage: the host is born inside the launching app's scope, e.g.
//!
//! ```text
//! /user.slice/user-N.slice/user@N.service/app.slice/app-…ghost….scope
//! ```
//!
//! and those app scopes are `PartOf=graphical-session.target`. Logging out stops
//! that target, which stops the scope, which SIGTERMs everything inside it. No
//! amount of forking escapes a cgroup.
//!
//! Two things are needed, and neither requires privilege:
//!
//! 1. [`escape_graphical_session`] — the host asks the user manager to move it
//!    into a transient scope of its own under `background.slice`, which nothing
//!    binds to the graphical session. Only the host itself can do this: the
//!    double-fork means the launcher never learns its pid.
//! 2. [`ensure_linger`] — without lingering, `user@N.service` is torn down when
//!    the user's last session ends, taking `background.slice` with it. Enabling
//!    it for one's own account is `implicit active: yes` in systemd's shipped
//!    polkit policy, so this needs no password and no admin.
//!
//! Everything here is best-effort and silent on failure: no systemd (a
//! container, a remote host, a non-systemd distro), no `busctl`, or a locked-down
//! policy just leaves hosts behaving exactly as they did before — they still
//! outlive their client, they just don't outlive the login.

#[cfg(target_os = "linux")]
use std::process::Command;

/// Move this process into a transient scope under `background.slice`, out of
/// whatever app scope it inherited from the process that spawned it.
///
/// Called by the host, in the host: the launcher double-forks, so it never
/// learns the pid to move. Best-effort — a failure only means the host keeps the
/// pre-existing behaviour of dying with the graphical session.
#[cfg(target_os = "linux")]
pub fn escape_graphical_session(session_name: &str) {
    let pid = std::process::id();
    // Session names are validated to `[A-Za-z0-9._-]` (`session::valid_name`),
    // all legal in a unit name; cap the length so the unit name stays well under
    // systemd's limit. The pid keeps it unique against a same-named session
    // whose scope has not been collected yet.
    let stem: String = session_name.chars().take(64).collect();
    let unit = format!("ghost-{stem}-{pid}.scope");

    let _ = run_quiet(Command::new("busctl").args([
        "--user",
        "call",
        "org.freedesktop.systemd1",
        "/org/freedesktop/systemd1",
        "org.freedesktop.systemd1.Manager",
        "StartTransientUnit",
        "ssa(sv)a(sa(sv))",
        &unit,
        "fail",
        // Properties: adopt us, park us outside the graphical session, and let
        // systemd garbage-collect the unit once we exit.
        "3",
        "PIDs",
        "au",
        "1",
        &pid.to_string(),
        "Slice",
        "s",
        "background.slice",
        "CollectMode",
        "s",
        "inactive-or-failed",
        // No auxiliary units.
        "0",
    ]));
}

/// Enable systemd lingering for this user, so `user@N.service` — and with it the
/// `background.slice` our hosts live in — survives the last logout.
///
/// Called by the launcher rather than the host: it is a once-per-user decision
/// about persistent system state, and only the launcher has a stderr anyone
/// reads (the host's is `/dev/null`). Best-effort and idempotent.
#[cfg(target_os = "linux")]
pub fn ensure_linger() {
    // Lingering can only change under us if the user turns it off deliberately,
    // and re-answering costs a `loginctl` fork on the spawn path — so ask once
    // per process, not once per session.
    static ASKED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if ASKED.set(()).is_err() {
        return;
    }
    // SAFETY: `getuid` is always safe; it cannot fail and touches no memory.
    let uid = unsafe { libc::getuid() };
    if !on_login_runtime_dir(std::env::var_os("XDG_RUNTIME_DIR").as_deref(), uid) {
        // A redirected runtime dir means we are not driving this login's session
        // (a test harness, a sandbox). Lingering is persistent, user-wide state —
        // don't touch it on behalf of a session that isn't the user's real one.
        return;
    }
    let uid = uid.to_string();

    let already =
        run_quiet(Command::new("loginctl").args(["show-user", &uid, "--value", "-p", "Linger"]));
    // Unknown state (no loginctl, no such user record) is not "off" — refuse to
    // guess rather than flip system state on a machine we failed to read.
    if !already.is_ok_and(|out| out.trim() == "no") {
        return;
    }

    if run_quiet(Command::new("loginctl").args(["enable-linger", &uid])).is_ok() {
        eprintln!(
            "ghost: enabled systemd lingering so sessions survive logout \
             (undo with `loginctl disable-linger`)"
        );
    }
}

/// Is `runtime_dir` the runtime directory systemd made for this login, rather
/// than one a harness redirected us to?
#[cfg(target_os = "linux")]
fn on_login_runtime_dir(runtime_dir: Option<&std::ffi::OsStr>, uid: u32) -> bool {
    runtime_dir.is_some_and(|dir| dir == std::ffi::OsStr::new(&format!("/run/user/{uid}")))
}

/// Run a helper to completion, discarding its stderr, and return its stdout on
/// success. `Err` for "couldn't run it" and "it failed" alike — every caller
/// here treats both the same way.
#[cfg(target_os = "linux")]
fn run_quiet(cmd: &mut Command) -> std::io::Result<String> {
    // Close every inherited descriptor above stdio in the child. The listener and
    // the liveness lock are deliberately left non-CLOEXEC for the host's whole
    // life (a self-upgrade re-hands them across its `execv`), and a helper that
    // hung while holding a dup of the lock would keep a dead session looking
    // alive — `session::list` prunes exactly when that flock is free.
    // SAFETY: `close_range` is async-signal-safe, and the host is single-threaded
    // when this runs.
    unsafe {
        std::os::unix::process::CommandExt::pre_exec(cmd, || {
            libc::close_range(3, libc::c_uint::MAX, 0);
            Ok(())
        });
    }
    let out = cmd.stderr(std::process::Stdio::null()).output()?;
    if !out.status.success() {
        return Err(std::io::Error::other("helper exited non-zero"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Nothing to escape: macOS logout tears sessions down by other means, and no
/// other platform we build for has cgroups.
#[cfg(not(target_os = "linux"))]
pub fn escape_graphical_session(_session_name: &str) {}

/// No systemd, no lingering.
#[cfg(not(target_os = "linux"))]
pub fn ensure_linger() {}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn the_login_runtime_dir_is_the_one_systemd_made_for_this_uid() {
        assert!(on_login_runtime_dir(
            Some(OsStr::new("/run/user/1000")),
            1000
        ));
    }

    #[test]
    fn a_redirected_runtime_dir_is_not_this_login() {
        // The E2E suite points XDG_RUNTIME_DIR at a tempdir; enabling lingering
        // from there would flip persistent user state during a test run.
        assert!(!on_login_runtime_dir(
            Some(OsStr::new("/tmp/.tmpXYZ/run")),
            1000
        ));
        // Another user's runtime dir is not ours either.
        assert!(!on_login_runtime_dir(
            Some(OsStr::new("/run/user/1001")),
            1000
        ));
        assert!(!on_login_runtime_dir(None, 1000));
    }
}
