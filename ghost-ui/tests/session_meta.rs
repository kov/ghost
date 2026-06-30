//! The host records per-session metadata (creation time, command, live title)
//! that discovery surfaces — what the GUI sidebar uses to identify sessions.
//! Here we drive it end to end: a session sets its window title, and `ghost ls`
//! reports it.

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
fn ls_reports_the_session_title() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "titled";
    let _guard = KillOnDrop { xdg, name };

    // An eager (`-d`) session whose program sets the window title via OSC 2, then
    // idles. The host parses the OSC off the PTY and records the title, with no
    // client attached.
    let out = ghost(xdg)
        .args([
            "new",
            name,
            "-d",
            "--",
            "sh",
            "-c",
            r"printf '\033]2;MY-TITLE\007'; exec sleep 600",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains("MY-TITLE")),
        "`ghost ls` never reported the session title; got: {:?}",
        ls(xdg)
    );
}
