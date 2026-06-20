//! End-to-end test for the bell-notification marker a host advertises so a
//! front-end (the GTK frontend) can highlight sessions that rang the terminal
//! bell while nobody was attached to witness it.
//!
//! The host keeps a marker file `<session>/bell` present once a *ground-state*
//! BEL (0x07) is seen while no display client is attached, and clears it when a
//! client attaches (you've now switched to it). `session::list()` surfaces it as
//! `SessionInfo.bell`. Here we drive a real session through the `ghost` binary:
//! an eager, detached child rings the bell with nobody attached, and we assert
//! the marker appears and then clears on attach.

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
fn host_marks_bell_for_an_unattached_session_and_clears_it_on_attach() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "bell-test";
    let _guard = KillOnDrop { xdg, name };

    // A real, *eager* (child starts immediately) detached session that rings the
    // bell once at startup with nobody attached, then idles so it stays listed.
    // `-d` without `--defer` starts the child eagerly (see `ghost new --help`).
    let out = ghost(xdg)
        .args([
            "new",
            name,
            "-d",
            "--",
            "sh",
            "-c",
            "printf '\\a'; sleep 60",
        ])
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
    let marker = session_dir.join("bell");

    // The child rang the bell while detached: the host should mark it so a
    // front-end can highlight the session as having an unseen notification.
    assert!(
        wait_until(Duration::from_secs(5), || marker.exists()),
        "host did not mark the session as having rung the bell"
    );

    // Attach a headless client; the first Resize is the handshake. Attaching is
    // "switching to" the session, so the host should clear the bell marker.
    let sock = session_dir.join("sock");
    let mut client = Client::connect_path(&sock).expect("headless client connect");
    client
        .send(&ClientMsg::Resize { cols: 80, rows: 24 })
        .unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || !marker.exists()),
        "host did not clear the bell marker after a client attached"
    );

    drop(client);
}
