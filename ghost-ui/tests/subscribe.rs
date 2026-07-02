//! End-to-end tests for the subscription surface: a client that sends
//! `Subscribe` is a state observer, not a display client. The host answers
//! with one `Snapshot` of the session's mutable state and — crucially — does
//! not treat the subscriber as an attach: no `attached` marker appears, and
//! an unseen-bell marker is not cleared by someone merely watching.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use ghost_vt::client::Client;
use ghost_vt::protocol::{AttachInfo, ClientMsg, PROTO_SUBSCRIBE, ServerMsg, SessionState};

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

/// Spawn a real, eager, detached session running `script` and wait for it to
/// be listed. Returns its session dir.
fn spawn_session(xdg: &Path, name: &str, script: &str) -> std::path::PathBuf {
    let out = ghost(xdg)
        .args(["new", name, "-d", "--", "sh", "-c", script])
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
    xdg.join("run").join("ghost").join(name)
}

/// Pump the connection until a `Snapshot` arrives (or time runs out).
fn recv_snapshot(client: &mut Client, timeout: Duration) -> Option<SessionState> {
    client
        .set_read_timeout(Some(Duration::from_millis(25)))
        .unwrap();
    let start = Instant::now();
    while start.elapsed() < timeout {
        let msgs = match client.recv_ready() {
            Ok(Some(msgs)) => msgs,
            Ok(None) => return None, // EOF: the host dropped us
            Err(_) => continue,      // read timeout — keep waiting
        };
        for msg in msgs {
            if let ServerMsg::Snapshot(state) = msg {
                return Some(state);
            }
        }
    }
    None
}

#[test]
fn a_subscriber_gets_a_snapshot_without_becoming_the_display_client() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "subscribe-test";
    let _guard = KillOnDrop { xdg, name };

    // The child rings the bell at startup with nobody attached, so the session
    // carries an unseen-bell notification the snapshot must report.
    let session_dir = spawn_session(xdg, name, "printf '\\a'; sleep 60");
    let bell_marker = session_dir.join("bell");
    assert!(
        wait_until(Duration::from_secs(5), || bell_marker.exists()),
        "unattached bell was not marked"
    );

    let sock = session_dir.join("sock");
    let mut sub = Client::connect_path(&sock).expect("subscriber connect");
    assert!(
        sub.proto() >= PROTO_SUBSCRIBE,
        "host must advertise the subscribe level it serves (got {})",
        sub.proto()
    );
    sub.send(&ClientMsg::Subscribe).unwrap();

    let state = recv_snapshot(&mut sub, Duration::from_secs(5))
        .expect("host answered the subscription with a snapshot");
    assert_eq!(state.attached, None, "nobody is attached");
    assert!(state.bell, "the unseen bell is part of the snapshot");

    // A subscriber is NOT a display client: watching must not mark the session
    // attached, and must not count as "seeing" the bell.
    assert!(
        !session_dir.join("attached").exists(),
        "subscribing must not set the attached marker"
    );
    assert!(
        bell_marker.exists(),
        "subscribing must not clear the bell marker"
    );
}

#[test]
fn the_snapshot_reports_the_identified_display_client() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "subscribe-attached-test";
    let _guard = KillOnDrop { xdg, name };

    let session_dir = spawn_session(xdg, name, "sleep 60");
    let sock = session_dir.join("sock");

    // A display client that identifies itself, then completes the attach
    // handshake (the first Resize).
    let mut display = Client::connect_path(&sock).expect("display connect");
    display
        .send(&ClientMsg::Hello {
            client: "window-1".to_string(),
        })
        .unwrap();
    display
        .send(&ClientMsg::Resize { cols: 80, rows: 24 })
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || session_dir
            .join("attached")
            .exists()),
        "display client did not attach"
    );

    let mut sub = Client::connect_path(&sock).expect("subscriber connect");
    sub.send(&ClientMsg::Subscribe).unwrap();
    let state = recv_snapshot(&mut sub, Duration::from_secs(5))
        .expect("host answered the subscription with a snapshot");
    assert_eq!(
        state.attached,
        Some(AttachInfo {
            client: Some("window-1".to_string()),
        }),
        "the snapshot names the identified display client"
    );
    assert!(!state.bell);

    drop(display);
}
