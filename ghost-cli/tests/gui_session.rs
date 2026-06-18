//! E2E tests for the front-end session model (`ghost_vt::client::Session`).
//!
//! `Session` is the event-loop-friendly attach layer a GUI drives: attach +
//! handshake, `pump` ready output as bytes, `send_input`/`resize`, and detach by
//! dropping (the session stays alive). We exercise exactly that flow headlessly,
//! against real sessions created through the `ghost` binary.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use ghost_vt::client::Session;

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

fn sock(xdg: &Path, name: &str) -> PathBuf {
    xdg.join("run").join("ghost").join(name).join("sock")
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

struct KillOnDrop<'a> {
    xdg: &'a Path,
    name: &'a str,
}

impl Drop for KillOnDrop<'_> {
    fn drop(&mut self) {
        let _ = ghost(self.xdg).args(["kill", self.name]).output();
    }
}

/// Pump until the accumulated rendered output contains `needle` or the session
/// ends or the timeout elapses. Returns the accumulated text and whether the
/// session ended.
fn pump_for(session: &mut Session, needle: &str, timeout: Duration) -> (String, bool) {
    let start = Instant::now();
    let mut acc = String::new();
    let mut ended = false;
    while start.elapsed() < timeout {
        let pumped = session.pump().expect("pump");
        acc.push_str(&String::from_utf8_lossy(&pumped.output));
        ended |= pumped.ended;
        if ended || acc.contains(needle) {
            break;
        }
    }
    (acc, ended)
}

#[test]
fn attach_pumps_output_then_detach_keeps_session_alive() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "gui-cat";
    let _guard = KillOnDrop { xdg, name };

    // Deferred, unattached — a GUI tab's session-creation primitive at the binary
    // level (the GUI itself spawns in-process; the wire behaviour is identical).
    let out = ghost(xdg)
        .args(["new", name, "-d", "--defer", "--", "cat"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains(name)),
        "session not listed"
    );

    {
        let mut s = Session::attach_path(&sock(xdg, name), name, 80, 24).expect("attach");
        s.set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        assert_eq!(s.name(), name);
        // The handshake started the deferred `cat`; it echoes our input back.
        s.send_input(b"gui-marker\n").unwrap();
        let (acc, ended) = pump_for(&mut s, "gui-marker", Duration::from_secs(5));
        assert!(!ended, "session ended unexpectedly; got {acc:?}");
        assert!(
            acc.contains("gui-marker"),
            "input was not echoed; got {acc:?}"
        );
        // `s` drops here → detach (the connection closes; the session lives on).
    }

    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains(name)),
        "session did not stay alive after detach-on-drop"
    );

    // Reattaching and driving again works.
    let mut s = Session::attach_path(&sock(xdg, name), name, 80, 24).expect("reattach");
    s.set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    s.send_input(b"again\n").unwrap();
    let (acc, _) = pump_for(&mut s, "again", Duration::from_secs(5));
    assert!(acc.contains("again"), "reattach did not echo; got {acc:?}");
}

#[test]
fn pump_reports_end_when_child_exits() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "gui-bye";
    let _guard = KillOnDrop { xdg, name };

    let out = ghost(xdg)
        .args(["new", name, "-d", "--defer", "--", "sh", "-c", "echo bye"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains(name)),
        "session not listed"
    );

    let mut s = Session::attach_path(&sock(xdg, name), name, 80, 24).expect("attach");
    s.set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    // The handshake starts the child, which prints `bye` and exits.
    let (acc, ended) = pump_for(&mut s, "\u{0}never\u{0}", Duration::from_secs(5));
    assert!(ended, "pump never reported the session end; got {acc:?}");
    assert!(
        acc.contains("bye"),
        "did not render child output before the end; got {acc:?}"
    );
}
