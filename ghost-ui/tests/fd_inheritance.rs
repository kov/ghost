//! E2E test that a session's private descriptors stop at the host.
//!
//! The host reclaims its listening socket and its liveness lock from argv with
//! `FD_CLOEXEC` cleared — that is how they cross the spawn `execv` — and if they
//! stay that way, every program the user runs in the session inherits both.
//!
//! The lock is the damaging one. `session::list` treats a *free* flock as proof
//! the host is gone, and an flock lives on the open file description, so a dup
//! held by anything the user backgrounded keeps the session's lock held after
//! the host itself is gone. The session then never prunes: it lists forever, and
//! because that same flock is the atomic "already exists" check in `spawn`, the
//! name can never be reused — a wedge that outlives the crash that caused it and
//! points at nothing the user can see.

#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

const GHOST: &str = env!("CARGO_BIN_EXE_ghost");

fn ghost(xdg: &Path) -> Command {
    let mut c = Command::new(GHOST);
    c.env("XDG_RUNTIME_DIR", xdg.join("run"));
    c.env("XDG_DATA_HOME", xdg.join("data"));
    c
}

fn ls(xdg: &Path) -> String {
    let out = ghost(xdg).arg("ls").output().expect("run `ghost ls`");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn wait_until(timeout: Duration, mut pred: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    loop {
        if pred() {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn read_pid(path: &Path) -> Option<i32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// SIGKILL a pid we captured ourselves — never one discovered by matching.
fn kill9(pid: i32) {
    let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
}

#[test]
fn a_process_the_session_started_cannot_wedge_the_session_name() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "leaky";
    let leaked_pid_file = xdg.join("leaked.pid");

    // A session whose child leaves something long-lived behind — an ordinary
    // `nohup make &`, a dev server, anything meant to outlive the terminal.
    // `nohup` matters: a plain background job shares the pty's process group and
    // is SIGHUP'd the moment the master closes, so it would take the inherited
    // descriptors to the grave with it and hide the bug. It records its own pid
    // so we can reap exactly it.
    let script = format!(
        "nohup sleep 600 >/dev/null 2>&1 & echo $! > {}; exec cat",
        leaked_pid_file.display()
    );
    let out = ghost(xdg)
        .args(["new", name, "-d", "--", "sh", "-c", &script])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let host_pid_file = xdg.join("run").join("ghost").join(name).join("pid");
    assert!(
        wait_until(Duration::from_secs(5), || read_pid(&host_pid_file)
            .is_some()
            && read_pid(&leaked_pid_file).is_some()),
        "session never started its child"
    );
    let host_pid = read_pid(&host_pid_file).expect("host pidfile");
    let leaked_pid = read_pid(&leaked_pid_file).expect("backgrounded child pidfile");

    // Crash the host outright. SIGKILL, not `ghost kill`: a graceful exit removes
    // the session directory on its way out and would hide the stale lock behind
    // that cleanup. What must hold is the invariant a crash relies on — the
    // kernel frees the flock when the host dies, and nothing else holds it.
    kill9(host_pid);

    let listed = wait_until(Duration::from_secs(5), || !ls(xdg).contains(name));
    kill9(leaked_pid);
    assert!(
        listed,
        "a dead session still lists: something it started inherited its liveness \
         lock, so the name can never be pruned or reused"
    );

    // And the name is genuinely free again — that same flock is `spawn`'s atomic
    // "already exists" check.
    let out = ghost(xdg)
        .args(["new", name, "-d", "--", "sleep", "600"])
        .output()
        .unwrap();
    let reused = out.status.success();
    let _ = ghost(xdg).args(["kill", name]).output();
    assert!(
        reused,
        "the crashed session's name could not be reused: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
