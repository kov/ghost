//! End-to-end tests for the subscription surface: a client that sends
//! `Subscribe` is a state observer, not a display client. The host answers
//! with one `Snapshot` of the session's mutable state and — crucially — does
//! not treat the subscriber as an attach: no `attached` marker appears, and
//! an unseen-bell marker is not cleared by someone merely watching.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use ghost_vt::client::{Client, Subscriber};
use ghost_vt::protocol::{
    AttachInfo, ClientMsg, PROTO_SUBSCRIBE, ServerMsg, SessionEvent, SessionState,
};

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

/// Pump the connection, appending every pushed event to `seen`, until `pred`
/// is satisfied (or time runs out — the assertion then shows what arrived).
fn recv_events_until(
    client: &mut Client,
    seen: &mut Vec<SessionEvent>,
    timeout: Duration,
    mut pred: impl FnMut(&[SessionEvent]) -> bool,
) {
    client
        .set_read_timeout(Some(Duration::from_millis(25)))
        .unwrap();
    let start = Instant::now();
    while start.elapsed() < timeout {
        if pred(seen) {
            return;
        }
        let msgs = match client.recv_ready() {
            Ok(Some(msgs)) => msgs,
            Ok(None) => break,  // EOF
            Err(_) => continue, // read timeout — keep waiting
        };
        for msg in msgs {
            if let ServerMsg::Event(e) = msg {
                seen.push(e);
            }
        }
    }
    assert!(pred(seen), "expected event did not arrive; got {seen:?}");
}

#[test]
fn the_subscriber_api_delivers_the_snapshot_then_events_then_eof() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "subscriber-api-test";
    let _guard = KillOnDrop { xdg, name };

    let session_dir = spawn_session(xdg, name, "sleep 60");
    let sock = session_dir.join("sock");

    // The typed observer wrapper: connects, verifies the host serves
    // subscriptions, and subscribes in one step. Pumps never block.
    let mut sub = Subscriber::connect_path(&sock).expect("subscriber connect");

    // First pump(s) deliver the snapshot.
    let mut snapshot = None;
    assert!(
        wait_until(Duration::from_secs(5), || {
            let p = sub.pump().unwrap();
            snapshot = snapshot.take().or(p.snapshot);
            snapshot.is_some()
        }),
        "no snapshot delivered"
    );
    assert_eq!(snapshot.unwrap().attached, None);

    // A display client attaching arrives as a pushed event.
    let mut display = Client::connect_path(&sock).expect("display connect");
    display
        .send(&ClientMsg::Resize { cols: 80, rows: 24 })
        .unwrap();
    let mut events = Vec::new();
    assert!(
        wait_until(Duration::from_secs(5), || {
            events.extend(sub.pump().unwrap().events);
            events.contains(&SessionEvent::Attached(AttachInfo { client: None }))
        }),
        "no Attached event; got {events:?}"
    );

    // Killing the session ends the subscription: pump reports it ended.
    drop(display);
    let out = ghost(xdg).args(["kill", name]).output().unwrap();
    assert!(out.status.success());
    assert!(
        wait_until(Duration::from_secs(5), || sub.pump().unwrap().ended),
        "subscription did not observe the host's death as EOF"
    );
}

#[test]
fn a_subscriber_is_pushed_state_events_as_the_session_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "events-test";
    let _guard = KillOnDrop { xdg, name };

    // The child waits for a line of input, then rings the bell and sets the
    // terminal title — observable state changes we trigger on demand.
    let session_dir = spawn_session(
        xdg,
        name,
        "read line; printf '\\a'; printf '\\033]2;hello\\007'; sleep 60",
    );
    let sock = session_dir.join("sock");

    // Subscribe first, so every later change is a delta on the snapshot.
    let mut sub = Client::connect_path(&sock).expect("subscriber connect");
    sub.send(&ClientMsg::Subscribe).unwrap();
    let state = recv_snapshot(&mut sub, Duration::from_secs(5)).expect("snapshot");
    assert_eq!(state.attached, None);
    let mut seen = Vec::new();

    // An identified display client attaches -> Attached(window-1).
    let mut display = Client::connect_path(&sock).expect("display connect");
    display
        .send(&ClientMsg::Hello {
            client: "window-1".to_string(),
        })
        .unwrap();
    display
        .send(&ClientMsg::Resize { cols: 80, rows: 24 })
        .unwrap();
    let attached = SessionEvent::Attached(AttachInfo {
        client: Some("window-1".to_string()),
    });
    recv_events_until(&mut sub, &mut seen, Duration::from_secs(5), |seen| {
        seen.contains(&attached)
    });

    // Waking the child rings the bell and sets the title. The bell rings while
    // a display client is attached: the live event fires anyway (that is the
    // point of the push), while the unseen-bell marker stays clear.
    display.send(&ClientMsg::Input(b"\n".to_vec())).unwrap();
    recv_events_until(&mut sub, &mut seen, Duration::from_secs(5), |seen| {
        seen.contains(&SessionEvent::Bell)
            && seen.contains(&SessionEvent::TitleChanged("hello".to_string()))
            && seen.contains(&SessionEvent::Activity)
    });
    assert!(
        !session_dir.join("bell").exists(),
        "a bell witnessed by an attached client must not be marked unseen"
    );

    // Renaming the session -> Renamed.
    display
        .send(&ClientMsg::Rename("otter".to_string()))
        .unwrap();
    recv_events_until(&mut sub, &mut seen, Duration::from_secs(5), |seen| {
        seen.contains(&SessionEvent::Renamed("otter".to_string()))
    });

    // Dropping the display client -> Detached.
    drop(display);
    recv_events_until(&mut sub, &mut seen, Duration::from_secs(5), |seen| {
        seen.contains(&SessionEvent::Detached)
    });
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
