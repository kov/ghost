//! End-to-end test for the attach-state marker a host advertises so other
//! processes (the GTK frontend) can tell "open elsewhere" from "detached".
//!
//! The host keeps a marker file `<session>/attached` present exactly while a
//! display client is attached; `session::list()` surfaces it as
//! `SessionInfo.attached`. Here we drive a real session through the `ghost`
//! binary and assert the marker tracks a headless client's attach and detach.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use ghost_vt::client::Client;
use ghost_vt::protocol::ClientMsg;

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

#[test]
fn host_marks_attached_while_a_display_client_is_connected() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "attach-marker";
    let _guard = KillOnDrop { xdg, name };

    // A real, detached-but-deferred session running `cat`.
    let out = ghost(xdg)
        .args(["new", name, "-d", "--", "cat"])
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

    let session_dir = xdg.join("run").join("ghost").join(name);
    let marker = session_dir.join("attached");

    // Nobody has completed the attach handshake yet: detached.
    assert!(
        !marker.exists(),
        "session marked attached before any client connected"
    );

    // Attach a headless client; the first Resize is the handshake that makes it
    // the display client. The host should then mark the session attached.
    let sock = session_dir.join("sock");
    let mut client = Client::connect_path(&sock).expect("headless client connect");
    client
        .send(&ClientMsg::Resize { cols: 80, rows: 24 })
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || marker.exists()),
        "host did not mark the session attached after the handshake"
    );

    // Detach by dropping the client (the socket closes). The host should notice
    // and clear the marker — the session keeps running, just unattached.
    drop(client);
    assert!(
        wait_until(Duration::from_secs(5), || !marker.exists()),
        "host did not clear the marker after the client detached"
    );
    assert!(
        ls(xdg).contains(name),
        "session should still be alive after detach"
    );
}
