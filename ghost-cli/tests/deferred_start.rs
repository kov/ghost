//! E2E tests for deferred child start.
//!
//! A session that will be attached to (the default `ghost new`, or an explicit
//! `-d --defer`) must NOT spawn its child until a client completes the attach
//! handshake — the child's startup terminal queries are then answered by a real
//! display client instead of being lost. A plain detached session (`-d`) keeps
//! starting its child eagerly, since there is no terminal to uphold that
//! contract anyway.
//!
//! `--defer` is a hidden flag (an implementation detail, not in `--help`): it
//! creates a *deferred but unattached* session, the primitive a GUI front-end
//! creates and then attaches to, and the only way to observe deferral over the
//! binary (the default path auto-attaches instantly, hiding the gap).

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use ghost_vt::client::Client;
use ghost_vt::protocol::ClientMsg;

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

/// Kills a session on drop so a failed test never leaks a daemon.
struct KillOnDrop<'a> {
    xdg: &'a Path,
    name: &'a str,
}

impl Drop for KillOnDrop<'_> {
    fn drop(&mut self) {
        let _ = ghost(self.xdg).args(["kill", self.name]).output();
    }
}

#[test]
fn deferred_session_starts_child_only_on_attach() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "defer-start";
    let _guard = KillOnDrop { xdg, name };

    // The child touches a sentinel the instant it runs, then idles on `cat`.
    let sentinel = xdg.join("deferred-started");
    let cmd = format!("touch '{}'; exec cat", sentinel.display());

    // `-d --defer`: detached (no auto-attach) but deferred — the child must not
    // run until a client completes the handshake.
    let out = ghost(xdg)
        .args(["new", name, "-d", "--defer", "--", "sh", "-c", &cmd])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new -d --defer` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains(name)),
        "deferred session not listed"
    );

    // No client has attached, so the child must not have started. Bounded
    // negative check (cf. record.rs's no-record test).
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !sentinel.exists(),
        "child ran before any client attached — deferral failed"
    );

    // Completing the attach handshake (the first Resize) must spawn the child.
    let sock = xdg.join("run").join("ghost").join(name).join("sock");
    let mut client = Client::connect_path(&sock).expect("attach to deferred session");
    client
        .send(&ClientMsg::Resize { cols: 80, rows: 24 })
        .unwrap();

    assert!(
        wait_until(Duration::from_secs(5), || sentinel.exists()),
        "child did not start after the attach handshake"
    );
}

#[test]
fn detached_session_starts_child_eagerly() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "eager-start";
    let _guard = KillOnDrop { xdg, name };

    let sentinel = xdg.join("eager-started");
    let cmd = format!("touch '{}'; exec cat", sentinel.display());

    // Plain `-d`: detached and eager. The child runs immediately, with no client
    // ever attaching.
    let out = ghost(xdg)
        .args(["new", name, "-d", "--", "sh", "-c", &cmd])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new -d` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        wait_until(Duration::from_secs(5), || sentinel.exists()),
        "detached child did not start eagerly"
    );
}
