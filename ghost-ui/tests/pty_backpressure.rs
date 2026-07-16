//! End-to-end tests for the host's PTY *write* path (`ghost-vt` session host).
//!
//! The host is a single-threaded poll loop that owns the child's PTY master.
//! Input from the display client (and, while detached, query/graphics replies)
//! is written *into* the child. If that write blocks — a child that has stopped
//! reading its stdin, e.g. because it is itself blocked writing a full stdout —
//! a blocking write wedges the whole loop: the host stops reading PTY output,
//! the child's stdout fills, and the two deadlock. Worse, a host stuck in a
//! blocking `write()` has SIGTERM masked (signalfd), so `ghost kill` can't even
//! reap it.
//!
//! These drive the real `ghost` binary and a real `ghost_vt::client::Client`
//! (the path a GUI front-end takes), and assert only observable behaviour: a
//! flooded session must not stop the host serving other clients, and input
//! queued while the child was not reading must reach the child intact once it
//! reads again.

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

/// Reap the session on drop. A host wedged in a blocking PTY write ignores the
/// SIGTERM `ghost kill` sends (it is masked behind the signalfd), so first
/// SIGKILL the captured pid directly — the only thing that frees a wedged host —
/// then run `ghost kill` to clean up the session directory. Killing a specific,
/// captured pid (never a pattern) keeps this from touching unrelated processes.
struct ReapOnDrop<'a> {
    xdg: &'a Path,
    name: &'a str,
}

impl Drop for ReapOnDrop<'_> {
    fn drop(&mut self) {
        let pid_file = self
            .xdg
            .join("run")
            .join("ghost")
            .join(self.name)
            .join("pid");
        if let Ok(s) = std::fs::read_to_string(&pid_file)
            && let Ok(pid) = s.trim().parse::<i32>()
        {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
        }
        let _ = ghost(self.xdg).args(["kill", self.name]).output();
    }
}

fn socket(xdg: &Path, name: &str) -> std::path::PathBuf {
    xdg.join("run").join("ghost").join(name).join("sock")
}

/// A flooded child (fills stdout, never reads stdin) must not wedge the host:
/// a second client attaching afterwards must still get its resync. Pre-fix the
/// host blocks inside the input write and never returns to its poll loop, so the
/// second client's attach is never serviced.
#[test]
fn a_flooded_child_does_not_wedge_the_host_for_other_clients() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "flood";
    let _guard = ReapOnDrop { xdg, name };

    // `yes` floods stdout forever and never reads its stdin — the classic wedge.
    let out = ghost(xdg)
        .args(["new", name, "-d", "--", "yes"])
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

    let sock = socket(xdg, name);
    let mut a = Client::connect_path(&sock).expect("client A connect");
    a.send(&ClientMsg::Resize { cols: 80, rows: 24 }).unwrap();
    a.set_read_timeout(Some(Duration::from_millis(50))).unwrap();

    // Confirm the flood is actually flowing to an attached client first.
    let mut saw_output = false;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if let Ok(Some(msgs)) = a.recv_ready()
            && msgs
                .iter()
                .any(|m| matches!(m, ServerMsg::Output(b) if b.contains(&b'y')))
        {
            saw_output = true;
            break;
        }
    }
    assert!(saw_output, "attached client never saw the flood's output");

    // Flood the child's stdin far past the PTY input buffer, so the host's write
    // to the child would block. The bytes must be newline-terminated lines: the
    // child's tty is in canonical mode, where a *line* with no newline just fills
    // the edit buffer and the excess is discarded (never blocking the writer),
    // while completed lines fill the line-discipline read buffer and *do* block
    // the master write once it is full. Non-blocking + pump: `send` buffers and
    // best-effort flushes, so keep flushing to push bytes until the host wedges.
    let mut chunk = Vec::new();
    while chunk.len() < 64 * 1024 {
        chunk.extend_from_slice(b"flood-line-padding-to-about-sixty-four-bytes-per-line--\n");
    }
    a.set_nonblocking(true).unwrap();
    for _ in 0..8 {
        a.send(&ClientMsg::Input(chunk.clone())).unwrap();
    }

    // Wait for the flood's output to STALL — no Output frame for `STALL` — which
    // is the observable wedge. Pre-fix this always happens (the host is stuck in
    // the input write); post-fix it never does and we fall through at `DEADLINE`
    // (the second client still resyncs, so the deadline only costs time, never
    // correctness — it cannot make the test flake).
    let stall = Duration::from_millis(750);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_output = Instant::now();
    loop {
        let _ = a.flush_pending();
        if let Ok(Some(msgs)) = a.recv_ready()
            && msgs.iter().any(|m| matches!(m, ServerMsg::Output(_)))
        {
            last_output = Instant::now();
        }
        if last_output.elapsed() >= stall || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    // The real assertion: a *fresh* client can still attach and get its resync.
    // A wedged host never returns to its poll loop to accept/service it.
    let mut b = Client::connect_path(&sock).expect("client B connect");
    b.send(&ClientMsg::Resize { cols: 80, rows: 24 }).unwrap();
    b.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    let mut b_resynced = false;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if let Ok(Some(msgs)) = b.recv_ready()
            && msgs.iter().any(|m| matches!(m, ServerMsg::Output(_)))
        {
            b_resynced = true;
            break;
        }
    }
    assert!(
        b_resynced,
        "second client never resynced — the flooded child wedged the host"
    );
}

/// Input queued while the child was not reading must reach the child intact,
/// in order, once it reads again — the drain half of the fix. The child ignores
/// stdin for a few seconds (so the host must hold the input) then execs `cat`,
/// which echoes it all back. Green pre-fix too (a blocking write just stalls
/// during the sleep); its job is to catch a *broken* drain — lost, reordered, or
/// never-resumed bytes — in the queued path.
#[test]
fn input_queued_while_the_child_slept_drains_intact_when_it_reads() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "drain";
    let _guard = ReapOnDrop { xdg, name };

    let out = ghost(xdg)
        .args(["new", name, "-d", "--", "sh", "-c", "sleep 3; exec cat"])
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

    let sock = socket(xdg, name);
    let mut c = Client::connect_path(&sock).expect("client connect");
    c.send(&ClientMsg::Resize { cols: 80, rows: 24 }).unwrap();
    c.set_read_timeout(Some(Duration::from_millis(50))).unwrap();

    // Well over the PTY input buffer, so the host must queue it while `sh` sleeps.
    // Short lines (< the canonical-mode line limit) with a unique final marker,
    // so `cat` echoes every line back and the marker proves the tail arrived.
    let mut payload = Vec::new();
    for i in 0..1200 {
        payload.extend_from_slice(
            format!("line-{i:05}-padding-to-about-sixty-chars-so-lines-stay\n").as_bytes(),
        );
    }
    payload.extend_from_slice(b"BACKPRESSURE-DRAIN-MARKER\n");
    c.set_nonblocking(true).unwrap();
    c.send(&ClientMsg::Input(payload)).unwrap();

    // Pump: flush the outbuf toward the host and drain echoed output until the
    // marker comes back (or a generous deadline — the child sleeps ~3s first).
    let mut acc = String::new();
    let mut got = false;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(20) {
        let _ = c.flush_pending();
        if let Ok(Some(msgs)) = c.recv_ready() {
            for m in msgs {
                if let ServerMsg::Output(bytes) = m {
                    acc.push_str(&String::from_utf8_lossy(&bytes));
                }
            }
        }
        if acc.contains("BACKPRESSURE-DRAIN-MARKER") {
            got = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(got, "queued input never drained back through the child");

    // The marker (the last line) arriving proves progress resumed, but not that
    // the drain delivered every byte: a partial-write bookkeeping slip could drop
    // or corrupt a line in the middle and still let the tail through. Echo is
    // in-order, so by the time the marker echoes, every earlier line must have
    // echoed too — assert each one survived, which is what "intact" means.
    let missing: Vec<usize> = (0..1200)
        .filter(|i| !acc.contains(&format!("line-{i:05}-")))
        .collect();
    assert!(
        missing.is_empty(),
        "queued input arrived corrupted — {} of 1200 lines missing from the drained echo (first few: {:?})",
        missing.len(),
        &missing[..missing.len().min(5)]
    );
}
