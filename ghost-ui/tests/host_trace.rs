//! The host half of the wire trace (`ghost-vt` session host).
//!
//! The GUI's trace ends at its own write to the session socket. For a remote
//! session the rest of the journey happens on another machine: the host receives
//! the frame, queues the bytes, and drains them into the child's PTY as the
//! non-blocking master accepts them. Nothing observed that drain — a keystroke
//! sitting in `pty_out` looked exactly like one the child had already read, and
//! the recording cannot tell them apart either (it records input at receipt).
//!
//! These drive the real `ghost` binary and a real `ghost_vt::client::Client`,
//! and assert on the host's own trace file: it is written where the HOST runs,
//! which for a remote session is not where the GUI writes.

use std::path::{Path, PathBuf};
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

fn socket(xdg: &Path, name: &str) -> PathBuf {
    xdg.join("run").join("ghost").join(name).join("sock")
}

/// Where the host writes its trace: its OWN data dir, not the client's.
fn trace_path(xdg: &Path) -> PathBuf {
    xdg.join("data")
        .join("ghost")
        .join("trace")
        .join("wire.log")
}

fn trace(xdg: &Path) -> String {
    std::fs::read_to_string(trace_path(xdg)).unwrap_or_default()
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

/// End the session through the CLI in its own XDG env — a host outlives its
/// client on purpose, and a leaked one holds an inotify instance.
struct ReapOnDrop<'a> {
    xdg: &'a Path,
    name: &'a str,
}

impl Drop for ReapOnDrop<'_> {
    fn drop(&mut self) {
        let _ = ghost(self.xdg).args(["kill", self.name]).output();
    }
}

/// Start a detached session running `cat` and attach a client to it.
fn session(xdg: &Path, name: &str) -> Client {
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
    let mut c = Client::connect_path(&socket(xdg, name)).expect("client connect");
    c.send(&ClientMsg::Resize { cols: 80, rows: 24 }).unwrap();
    c.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    c
}

/// A running host starts tracing when its client says so, and what it logs is
/// the segment nothing else covers: the bytes it received for the child, and
/// what the PTY write actually accepted.
#[test]
fn a_running_host_traces_its_pty_drain_when_the_client_asks() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "traced-host";
    let _guard = ReapOnDrop { xdg, name };
    let mut c = session(xdg, name);

    // The session was already running before anything asked for a trace — the
    // deployment that matters, where restarting the host is not on the table.
    c.send(&ClientMsg::Trace { on: true }).unwrap();
    c.send(&ClientMsg::Input(b"ping\n".to_vec())).unwrap();

    assert!(
        wait_until(Duration::from_secs(5), || {
            let log = trace(xdg);
            log.contains("host input") && log.contains("pty wrote")
        }),
        "the host must trace what it received and what its PTY took; \
         {} holds: {:?}",
        trace_path(xdg).display(),
        trace(xdg)
    );

    let log = trace(xdg);
    assert!(
        log.contains("ping^J"),
        "the payload is logged readably (ESC and C0 escaped): {log:?}"
    );
    assert!(
        log.contains(name),
        "every line names the session it concerns: {log:?}"
    );
}

/// ...and stops when the client says so. Tracing costs a file append per
/// keystroke and per mouse report on a session that may outlive the window that
/// armed it, so turning it off must reach the host too.
#[test]
fn the_host_stops_tracing_when_the_client_disarms_it() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "untraced-host";
    let _guard = ReapOnDrop { xdg, name };
    let mut c = session(xdg, name);

    c.send(&ClientMsg::Trace { on: true }).unwrap();
    c.send(&ClientMsg::Input(b"armed\n".to_vec())).unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || trace(xdg).contains("armed")),
        "precondition: tracing is on"
    );

    c.send(&ClientMsg::Trace { on: false }).unwrap();
    c.send(&ClientMsg::Input(b"quiet\n".to_vec())).unwrap();
    // Give the host every chance to log it: the assertion is an absence, so it
    // must not pass merely by being read too early. `armed` reaching the file is
    // the proof the loop ran; wait for the child to echo `quiet` back, which it
    // cannot do until the host wrote it — the very moment a trace line would
    // have been appended.
    let mut echoed = false;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if let Ok(Some(msgs)) = c.recv_ready()
            && msgs.iter().any(|m| {
                matches!(m, ghost_vt::protocol::ServerMsg::Output(b)
                    if String::from_utf8_lossy(b).contains("quiet"))
            })
        {
            echoed = true;
            break;
        }
    }
    assert!(echoed, "the child never echoed the disarmed input back");
    assert!(
        !trace(xdg).contains("quiet"),
        "input after the disarm must not be traced: {:?}",
        trace(xdg)
    );
}
