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
        .args(["new", name, "-d", "--", "sleep", "600"])
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

#[test]
fn rename_allows_spaces_in_the_display_name() {
    // The display name is a label, not a path component (the session keeps its
    // immutable id), so spaces and other human-friendly characters are fine.
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "rename-test";
    let _guard = Cleanup { xdg, name };

    ghost(xdg)
        .args(["new", name, "-d", "--", "sleep", "600"])
        .output()
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains(name)),
        "session `{name}` was not listed"
    );

    let out = ghost(xdg)
        .args(["rename", name, "prod deploy 🚀"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost rename` to a spaced label failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg)
            .contains("prod deploy 🚀")),
        "renamed label not shown by `ghost ls`: {}",
        ls(xdg)
    );
}

#[test]
fn ls_json_emits_a_parseable_listing() {
    // `ghost ls --json` feeds the remote-fleet initiator, so its output must
    // parse straight back into the SessionInfo the local lister produces.
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "json-test";
    let _guard = Cleanup { xdg, name };

    // An empty listing is a valid empty JSON array (no session yet).
    let out = ghost(xdg).args(["ls", "--json"]).output().unwrap();
    let empty: Vec<ghost_vt::session::SessionInfo> =
        serde_json::from_slice(&out.stdout).expect("empty --json parses");
    assert!(empty.is_empty(), "no sessions yet: {empty:?}");

    ghost(xdg)
        .args(["new", name, "-d", "--", "sleep", "600"])
        .output()
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains(name)),
        "session `{name}` was not listed"
    );

    let out = ghost(xdg).args(["ls", "--json"]).output().unwrap();
    let infos: Vec<ghost_vt::session::SessionInfo> =
        serde_json::from_slice(&out.stdout).expect("--json parses");
    let s = infos
        .iter()
        .find(|s| s.name == name)
        .expect("the session is in the JSON listing");
    assert_eq!(s.command, vec!["sleep".to_string(), "600".to_string()]);
    assert!(s.connection.is_none(), "a local session has no connection");
}
