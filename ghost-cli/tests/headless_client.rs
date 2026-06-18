//! End-to-end test for the headless client API (`ghost_vt::client::Client`):
//! the programmatic attach path a GUI front-end uses instead of the terminal
//! pipe. Drives a real session through the `ghost` binary, then connects a
//! headless client straight to its socket and round-trips input -> output.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use ghost_vt::client::Client;
use ghost_vt::protocol::{ClientMsg, ServerMsg};

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
fn headless_client_round_trips_input_and_output() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "headless-cat";
    let _guard = KillOnDrop { xdg, name };

    // A real session running `cat` (echoes its stdin), created via the binary.
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

    // Connect a headless client directly to the session socket — no PTY, no
    // `ghost attach` process. This is the path a GUI front-end takes.
    let sock = xdg.join("run").join("ghost").join(name).join("sock");
    let mut client = Client::connect_path(&sock).expect("headless client connect");

    // The attach handshake is the first Resize: it triggers the resync and turns
    // on live output. Then send input that `cat` will echo straight back.
    client
        .send(&ClientMsg::Resize { cols: 80, rows: 24 })
        .unwrap();
    client
        .send(&ClientMsg::Input(b"headless-marker\n".to_vec()))
        .unwrap();

    // Drain server output until the marker echoes back (read-until-predicate,
    // bounded by a timeout — never a fixed sleep).
    client
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let mut acc = String::new();
    let start = Instant::now();
    let mut got = false;
    while start.elapsed() < Duration::from_secs(5) {
        match client.recv_ready().expect("recv from session") {
            None => break, // server closed the connection
            Some(msgs) => {
                for m in msgs {
                    if let ServerMsg::Output(bytes) = m {
                        acc.push_str(&String::from_utf8_lossy(&bytes));
                    }
                }
            }
        }
        if acc.contains("headless-marker") {
            got = true;
            break;
        }
    }
    assert!(
        got,
        "marker never echoed back to the headless client; got: {acc:?}"
    );
}
