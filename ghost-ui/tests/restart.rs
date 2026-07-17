//! Restarting a live session's host under the current binary, keeping its screen.
//!
//! When a remote host predates the staged ghost, its sessions keep running at the
//! old protocol level. `ghost __restart <name>` brings one up to the current
//! binary: it ends the running host with a graceful SIGTERM (a logout-equivalent
//! that LEAVES the recording, unlike `ghost kill` which discards it), waits for it
//! to exit, then respawns the session seeded from that recording with the child
//! deferred to the next attach. The running *program* is lost (a re-exec upgrade
//! would preserve it); the visible screen and scrollback survive, and the session
//! comes back under a fresh host process.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use ghost_vt::client::Session;
use ghost_vt::screen::Screen;

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

struct KillOnDrop<'a> {
    xdg: &'a Path,
    name: &'a str,
}

impl Drop for KillOnDrop<'_> {
    fn drop(&mut self) {
        let _ = ghost(self.xdg).args(["kill", self.name]).output();
    }
}

fn sock(xdg: &Path, name: &str) -> PathBuf {
    xdg.join("run").join("ghost").join(name).join("sock")
}

fn host_pid(xdg: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(xdg.join("run").join("ghost").join(name).join("pid"))
        .ok()
        .map(|s| s.trim().to_string())
}

/// Attach and pump until `needle` renders (or a short deadline passes).
fn wait_for_screen(session: &mut Session, screen: &mut Screen, needle: &str) -> bool {
    wait_until(Duration::from_secs(5), || {
        if let Ok(p) = session.pump() {
            screen.feed(&p.output);
        }
        screen.text().join("\n").contains(needle)
    })
}

#[test]
fn restart_keeps_the_screen_and_brings_the_session_back_under_a_new_host() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();

    // A live session with a distinctive marker on its screen.
    let out = ghost(xdg)
        .args([
            "new",
            "restartme",
            "-d",
            "--",
            "sh",
            "-c",
            "echo BEFORE-RESTART; sleep 30",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _guard = KillOnDrop {
        xdg,
        name: "restartme",
    };
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains("restartme")),
        "session not listed"
    );

    // Attach and confirm the marker reached the host (so it's in the recording a
    // restart will seed from); note the original host pid.
    {
        let mut s = Session::attach_path(&sock(xdg, "restartme"), "restartme", 80, 24)
            .expect("attach first life");
        s.set_read_timeout(Some(Duration::from_millis(25))).unwrap();
        let mut screen = Screen::new(80, 24, 100);
        assert!(
            wait_for_screen(&mut s, &mut screen, "BEFORE-RESTART"),
            "the first life's output never arrived; saw:\n{}",
            screen.text().join("\n")
        );
    }
    let before_pid = host_pid(xdg, "restartme").expect("original host pid");

    // Restart the session's host under the current binary.
    let out = ghost(xdg)
        .args(["__restart", "restartme"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost __restart` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // It comes back listed under a DIFFERENT host process…
    assert!(
        wait_until(Duration::from_secs(5), || {
            ls(xdg).contains("restartme")
                && host_pid(xdg, "restartme").is_some_and(|p| p != before_pid)
        }),
        "the session did not come back under a new host after restart"
    );

    // …and its screen SURVIVED — attaching resyncs the pre-restart marker (seeded
    // from the recording), not a blank screen.
    let mut s =
        Session::attach_path(&sock(xdg, "restartme"), "restartme", 80, 24).expect("attach restart");
    s.set_read_timeout(Some(Duration::from_millis(25))).unwrap();
    let mut screen = Screen::new(80, 24, 100);
    assert!(
        wait_for_screen(&mut s, &mut screen, "BEFORE-RESTART"),
        "the pre-restart screen did not survive the restart; saw:\n{}",
        screen.text().join("\n")
    );
}
