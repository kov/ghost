//! End-to-end test for `ghost __watch`, the pushed session-set stream that
//! replaces the fleet's poll: it emits the listing once, then again whenever the
//! session set changes.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use ghost_vt::session::SessionInfo;

fn ghost(xdg: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_ghost"));
    c.env("XDG_RUNTIME_DIR", xdg.join("run"));
    c.env("XDG_DATA_HOME", xdg.join("data"));
    c
}

/// Kill a spawned child on drop.
struct KillChild(Child);
impl Drop for KillChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Read JSON listings off `rx` until one satisfies `pred` or the deadline passes.
fn wait_for(
    rx: &Receiver<String>,
    timeout: Duration,
    mut pred: impl FnMut(&[SessionInfo]) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        match rx.recv_timeout(left) {
            Ok(line) => {
                if let Ok(sessions) = serde_json::from_str::<Vec<SessionInfo>>(&line)
                    && pred(&sessions)
                {
                    return true;
                }
            }
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return false,
        }
    }
}

#[test]
fn watch_streams_the_listing_and_pushes_on_change() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();

    // Start the watcher; a reader thread hands each JSON line to the test.
    let mut child = ghost(xdg)
        .arg("__watch")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let _guard = KillChild(child);
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(l) = line else { break };
            if tx.send(l).is_err() {
                break;
            }
        }
    });

    // The initial listing arrives immediately, and is empty (no sessions yet).
    assert!(
        wait_for(&rx, Duration::from_secs(5), |s| s.is_empty()),
        "no initial (empty) listing was pushed"
    );

    // Creating a session pushes a fresh listing that includes it — no polling.
    ghost(xdg)
        .args(["new", "-d", "watch-test", "--", "sleep", "600"])
        .output()
        .unwrap();
    let saw_it = wait_for(&rx, Duration::from_secs(5), |s| {
        s.iter().any(|i| i.name == "watch-test")
    });
    let _ = ghost(xdg).args(["kill", "watch-test"]).output();
    assert!(saw_it, "the new session was not pushed to the watcher");
}
