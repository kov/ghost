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

fn new_session(xdg: &Path, name: &str) {
    let out = ghost(xdg)
        .args(["new", name, "-d", "--", "sleep", "600"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new {name}` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains(name)),
        "session `{name}` not listed after `ghost new`"
    );
}

#[test]
fn kill_multi_two_live() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let a = "kill-multi-a";
    let b = "kill-multi-b";
    let _ga = Cleanup { xdg, name: a };
    let _gb = Cleanup { xdg, name: b };

    new_session(xdg, a);
    new_session(xdg, b);

    let out = ghost(xdg).args(["kill", a, b]).output().unwrap();
    assert!(
        out.status.success(),
        "`ghost kill {a} {b}` failed (exit {:?}): {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&format!("killed session '{a}'")),
        "expected 'killed session {a}' in stdout: {stdout}"
    );
    assert!(
        stdout.contains(&format!("killed session '{b}'")),
        "expected 'killed session {b}' in stdout: {stdout}"
    );
    assert!(
        wait_until(Duration::from_secs(5), || !ls(xdg).contains(a)),
        "session `{a}` still listed after kill"
    );
    assert!(
        wait_until(Duration::from_secs(5), || !ls(xdg).contains(b)),
        "session `{b}` still listed after kill"
    );
}

#[test]
fn kill_partial_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let live = "kill-partial-live";
    let bystander = "kill-partial-bystander";
    let _gl = Cleanup { xdg, name: live };
    let _gc = Cleanup {
        xdg,
        name: bystander,
    };

    new_session(xdg, live);
    new_session(xdg, bystander);

    let out = ghost(xdg)
        .args(["kill", live, "ghostname"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected exit 1 for partial failure"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains(&format!("killed session '{live}'")),
        "expected killed line for {live} in stdout: {stdout}"
    );
    assert!(
        stderr.contains("ghost: no such session 'ghostname'"),
        "expected no-such-session error in stderr: {stderr}"
    );
    assert!(
        wait_until(Duration::from_secs(5), || !ls(xdg).contains(live)),
        "session `{live}` should be gone"
    );
    assert!(
        ls(xdg).contains(bystander),
        "bystander `{bystander}` should still be running"
    );
}

#[test]
fn kill_all_and_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let a = "kill-all-a";
    let b = "kill-all-b";
    let _ga = Cleanup { xdg, name: a };
    let _gb = Cleanup { xdg, name: b };

    new_session(xdg, a);
    new_session(xdg, b);

    let out = ghost(xdg).args(["kill", "--all"]).output().unwrap();
    assert!(
        out.status.success(),
        "`ghost kill --all` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wait_until(Duration::from_secs(5), || !ls(xdg).contains(a)
            && !ls(xdg).contains(b)),
        "sessions should be gone after `ghost kill --all`"
    );

    // Second --all on an empty list: benign, specific message, exit 0.
    let out2 = ghost(xdg).args(["kill", "--all"]).output().unwrap();
    assert!(
        out2.status.success(),
        "`ghost kill --all` on empty list should exit 0"
    );
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert!(
        stdout2.contains("no sessions to kill"),
        "expected 'no sessions to kill', got: {stdout2}"
    );
}

#[test]
fn kill_dedup() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "kill-dedup";
    let _g = Cleanup { xdg, name };

    new_session(xdg, name);

    let out = ghost(xdg).args(["kill", name, name]).output().unwrap();
    assert!(
        out.status.success(),
        "`ghost kill {name} {name}` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let killed_count = stdout
        .lines()
        .filter(|l| l.contains(&format!("killed session '{name}'")))
        .count();
    assert_eq!(
        killed_count, 1,
        "session should be reported killed once; got:\n{stdout}"
    );
}

#[test]
fn kill_self_last() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let self_name = "kill-self";
    let other_name = "kill-other";
    let _gs = Cleanup {
        xdg,
        name: self_name,
    };
    let _go = Cleanup {
        xdg,
        name: other_name,
    };

    new_session(xdg, self_name);
    new_session(xdg, other_name);

    // Pretend we are running inside self_name by injecting the env var.
    let out = ghost(xdg)
        .env("GHOST_SESSION_ID", self_name)
        .args(["kill", "--all"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost kill --all` with GHOST_SESSION_ID failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    // self_name must appear AFTER other_name in the output.
    let self_pos = stdout.find(&format!("killed session '{self_name}'"));
    let other_pos = stdout.find(&format!("killed session '{other_name}'"));
    assert!(
        self_pos.is_some() && other_pos.is_some(),
        "both kill lines must appear; got:\n{stdout}"
    );
    assert!(
        other_pos.unwrap() < self_pos.unwrap(),
        "self session must be killed last; got:\n{stdout}"
    );
    assert!(
        wait_until(Duration::from_secs(5), || !ls(xdg).contains(self_name)
            && !ls(xdg).contains(other_name)),
        "both sessions should be gone"
    );
}

#[test]
fn new_refuses_an_unsafe_session_id() {
    // Unlike a display name, the session *id* becomes a directory and socket, so
    // it must stay a safe path component. `ghost new` refuses an unsafe one and
    // creates nothing.
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();

    let out = ghost(xdg)
        .args(["new", "bad name", "-d", "--", "true"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "an unsafe id must be refused");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("not a valid session name"),
        "expected a helpful error, got: {stderr}"
    );
    assert!(
        !ls(xdg).contains("bad name"),
        "no session should have been created: {}",
        ls(xdg)
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
