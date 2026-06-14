//! End-to-end lifecycle tests driving the real `ghost` binary.
//!
//! Each test runs against an isolated `XDG_RUNTIME_DIR` (a tempdir), so they are
//! parallel-safe and never touch the user's real sessions. Timing uses
//! read-until-predicate with a timeout, never fixed sleeps.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

fn ghost(xdg: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_ghost"));
    c.env("XDG_RUNTIME_DIR", xdg);
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
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Kills a session on drop, so a failed assertion never leaks a daemon.
struct Cleanup<'a> {
    xdg: &'a Path,
    name: &'a str,
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        let _ = ghost(self.xdg).args(["kill", self.name]).output();
    }
}

#[test]
fn session_lifecycle_new_ls_kill() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "tracer-test";
    let _guard = Cleanup { xdg, name };

    let out = ghost(xdg)
        .args(["new", name, "--", "sleep", "600"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains(name)),
        "session `{name}` was not listed by `ghost ls`"
    );

    let out = ghost(xdg).args(["kill", name]).output().unwrap();
    assert!(
        out.status.success(),
        "`ghost kill` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        wait_until(Duration::from_secs(5), || !ls(xdg).contains(name)),
        "session `{name}` still listed after `ghost kill`"
    );
}
