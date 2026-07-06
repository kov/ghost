//! End-to-end test for `ghost __watch`, the pushed session-set stream that
//! replaces the fleet's poll: it emits the listing once, then again whenever the
//! session set changes.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use ghost_vt::client::Session;
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

fn sock(xdg: &Path, name: &str) -> PathBuf {
    xdg.join("run").join("ghost").join(name).join("sock")
}

/// A title change writes `<session>/meta` from the host's run loop — a write
/// *inside* the per-session subdir, with no process opening the runtime dir. It
/// is the cleanest artifact-proof probe of the push-on-change path: unlike a
/// rename (whose CLI incidentally `opendir`s the runtime dir and so wakes the
/// watch by side effect), nothing here pokes the watched directory, so a push can
/// only come from the watcher actually noticing the meta write.
#[test]
fn watch_pushes_on_title_change() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();

    // A session running a shell, so we can drive an OSC title change into it.
    ghost(xdg)
        .args(["new", "-d", "titled", "--", "sh"])
        .output()
        .unwrap();
    let _guard2 = KillOnDrop {
        xdg,
        name: "titled",
    };

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

    // Wait until the watcher has seen the session with its default (empty) title.
    assert!(
        wait_for(&rx, Duration::from_secs(5), |s| {
            s.iter().any(|i| i.name == "titled" && i.title.is_empty())
        }),
        "the session was not pushed to the watcher"
    );

    // Attach (connects to the socket directly — does NOT open the runtime dir) and
    // drive an OSC 2 title change through the shell. The host processes it and
    // rewrites `titled/meta`; the watcher must push the fresh title well under the
    // 30s heartbeat, proving it noticed the in-subdir meta write.
    let mut session = Session::attach_path(&sock(xdg, "titled"), "titled", 80, 24).expect("attach");
    session
        .set_read_timeout(Some(Duration::from_millis(25)))
        .unwrap();
    session
        .send_input(b"printf '\\033]2;HELLO-TITLE\\007'\n")
        .unwrap();

    assert!(
        wait_for(&rx, Duration::from_secs(5), |s| {
            s.iter()
                .any(|i| i.name == "titled" && i.title == "HELLO-TITLE")
        }),
        "the title change was not pushed to the watcher within the heartbeat window"
    );
}

/// Kill a session by name on drop, so a failing assertion still cleans up.
struct KillOnDrop<'a> {
    xdg: &'a Path,
    name: &'a str,
}
impl Drop for KillOnDrop<'_> {
    fn drop(&mut self) {
        let _ = ghost(self.xdg).args(["kill", self.name]).output();
    }
}
