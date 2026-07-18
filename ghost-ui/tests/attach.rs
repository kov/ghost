//! End-to-end tests for `ghost attach`: the transparent-pipe client and the
//! host's output forwarding.
//!
//! Each test drives the real `ghost attach` process over a PTY and inspects what
//! it prints by feeding the bytes into a `vt` terminal emulator — so assertions
//! are about the resulting screen, not raw bytes. Timing is read-until-predicate
//! with a timeout, never fixed sleeps.

use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ghost_vt::Vt;
use pty_process::Size;
use pty_process::blocking::{Command as PtyCommand, Pty, open};

const GHOST: &str = env!("CARGO_BIN_EXE_ghost");

fn ghost(xdg: &Path) -> Command {
    let mut c = Command::new(GHOST);
    set_xdg(&mut c, xdg);
    c
}

/// Point a command's XDG dirs at isolated subdirectories of the test's tempdir.
/// Runtime and data must be *distinct* roots, as they are in production: a
/// session is now a directory under the runtime root, and `ghost ls` treats
/// every directory there as a session — so the recordings directory must not
/// share that root. (Recording is on by default, so this also keeps the suite
/// from writing into the real `$XDG_DATA_HOME`.)
fn set_xdg(c: &mut Command, xdg: &Path) {
    c.env("XDG_RUNTIME_DIR", xdg.join("run"));
    c.env("XDG_DATA_HOME", xdg.join("data"));
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

/// A `ghost attach` process driven over a PTY, with a background reader that
/// feeds everything the client prints into a `vt` emulator we can inspect.
struct Attached {
    pty: Arc<Pty>,
    vt: Arc<Mutex<Vt>>,
    child: std::process::Child,
}

impl Attached {
    /// Drive `ghost attach <name>` over a PTY.
    fn new(xdg: &Path, name: &str, cols: u16, rows: u16) -> Self {
        Self::spawn(xdg, &["attach", name], cols, rows)
    }

    /// Drive `ghost new <name> -- <command…>` over a PTY, exercising the default
    /// auto-attach: `new` starts the session and attaches us to it in one step.
    fn new_session(xdg: &Path, name: &str, command: &[&str], cols: u16, rows: u16) -> Self {
        let mut args = vec!["new", name, "--"];
        args.extend_from_slice(command);
        Self::spawn(xdg, &args, cols, rows)
    }

    /// Run `ghost <args…>` as the foreground process of a fresh PTY, feeding
    /// everything it prints into a `vt` we can inspect.
    fn spawn(xdg: &Path, args: &[&str], cols: u16, rows: u16) -> Self {
        let (pty, pts) = open().expect("open pty");
        pty.resize(Size::new(rows, cols)).expect("resize pty");
        // Disable the slave's line-discipline echo so the test measures the
        // client's forwarding, not the kernel's local echo. (The real client
        // puts this terminal into raw mode too.)
        {
            use rustix::termios::{OptionalActions, tcgetattr, tcsetattr};
            let mut t = tcgetattr(&pts).expect("tcgetattr pts");
            t.make_raw();
            tcsetattr(&pts, OptionalActions::Now, &t).expect("tcsetattr pts");
        }
        let child = PtyCommand::new(GHOST)
            .args(args)
            .env("XDG_RUNTIME_DIR", xdg.join("run"))
            .env("XDG_DATA_HOME", xdg.join("data"))
            .spawn(pts)
            .expect("spawn ghost");

        let pty = Arc::new(pty);
        let vt = Arc::new(Mutex::new(Vt::new(cols as usize, rows as usize)));
        let reader_pty = Arc::clone(&pty);
        let reader_vt = Arc::clone(&vt);
        std::thread::spawn(move || {
            let mut r: &Pty = &reader_pty;
            let mut buf = [0u8; 4096];
            loop {
                match r.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let chunk = String::from_utf8_lossy(&buf[..n]);
                        reader_vt.lock().unwrap().feed_str(&chunk);
                    }
                }
            }
        });

        Attached { pty, vt, child }
    }

    fn send(&self, bytes: &[u8]) {
        let mut w: &Pty = &self.pty;
        w.write_all(bytes).expect("write to pty");
    }

    fn screen(&self) -> Vec<String> {
        self.vt.lock().unwrap().text()
    }

    /// The cursor's current position as `(row, col)`, both 0-based.
    fn cursor(&self) -> (usize, usize) {
        let c = self.vt.lock().unwrap().cursor();
        (c.row, c.col)
    }

    /// Wait until `pred` holds, or time out.
    fn wait_until(&self, timeout: Duration, mut pred: impl FnMut(&Self) -> bool) -> bool {
        let start = Instant::now();
        loop {
            if pred(self) {
                return true;
            }
            if start.elapsed() >= timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Wait until the visible screen satisfies `pred`, or time out.
    fn wait_for_screen(&self, timeout: Duration, mut pred: impl FnMut(&[String]) -> bool) -> bool {
        let start = Instant::now();
        loop {
            if pred(&self.screen()) {
                return true;
            }
            if start.elapsed() >= timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn screen_contains(needle: &str) -> impl Fn(&[String]) -> bool + '_ {
    move |lines| lines.iter().any(|l| l.contains(needle))
}

#[test]
fn attach_streams_session_output() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "attach-echo";
    let _guard = KillOnDrop { xdg, name };

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

    let term = Attached::new(xdg, name, 80, 24);

    // `cat` echoes its input back; typing a unique line should surface it on the
    // attached terminal's screen.
    term.send(b"ghost-echo-7r\n");

    assert!(
        term.wait_for_screen(Duration::from_secs(5), screen_contains("ghost-echo-7r")),
        "typed text never reached the screen; got: {:?}",
        term.screen()
    );
}

impl Attached {
    /// Resize the underlying terminal, as a windowing system would when the
    /// user resizes the window. Mirrors the change into our `vt` so the parsed
    /// screen matches a real terminal, and delivers SIGWINCH to the attached
    /// client (the foreground process of this PTY).
    fn resize(&self, cols: u16, rows: u16) {
        self.pty.resize(Size::new(rows, cols)).expect("resize pty");
        self.vt.lock().unwrap().resize(cols as usize, rows as usize);
    }

    /// Wait for the client process to exit, returning whether it did in time.
    fn wait_exit(&mut self, timeout: Duration) -> bool {
        let start = Instant::now();
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => return true,
                Ok(None) => {}
            }
            if start.elapsed() >= timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

#[test]
fn detach_keeps_session_alive_then_reattach() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "attach-detach";
    let _guard = KillOnDrop { xdg, name };

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

    let mut term = Attached::new(xdg, name, 80, 24);
    term.send(b"first-marker\n");
    assert!(
        term.wait_for_screen(Duration::from_secs(5), screen_contains("first-marker")),
        "first marker never appeared; got: {:?}",
        term.screen()
    );

    // Detach with the default trigger: Ctrl-\ then 'd'.
    term.send(b"\x1cd");
    assert!(
        term.wait_exit(Duration::from_secs(5)),
        "client did not exit after detach"
    );
    drop(term);

    // The session must still be alive after the client detaches.
    assert!(
        ls(xdg).contains(name),
        "session died on detach (it should survive)"
    );

    // Reattaching to the same session works and new output flows.
    let term2 = Attached::new(xdg, name, 80, 24);
    term2.send(b"second-marker\n");
    assert!(
        term2.wait_for_screen(Duration::from_secs(5), screen_contains("second-marker")),
        "reattached session did not echo; got: {:?}",
        term2.screen()
    );
}

/// A local `ghost attach` must SURVIVE a host self-upgrade. The host re-execs in
/// place — same pid, same liveness lock, same listener socket, same child — so
/// the attach's connection drops but the session is NOT over. The client must
/// reconnect by name (the lock is still held) and stay interactive, not print
/// "session closed" and exit.
#[test]
fn attach_survives_a_host_self_upgrade() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "attach-upgrade";
    let _guard = KillOnDrop { xdg, name };

    let out = ghost(xdg)
        .args([
            "new",
            name,
            "-d",
            "--",
            "sh",
            "-c",
            "echo READYtag; exec cat",
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

    let mut term = Attached::new(xdg, name, 80, 24);
    assert!(
        term.wait_for_screen(Duration::from_secs(5), screen_contains("READYtag")),
        "session not attached; got: {:?}",
        term.screen()
    );

    // Upgrade the host in place (to itself — proving the mechanism needs no newer
    // binary). Delivered over a separate control connection; the attached client
    // is the display client whose connection the exec drops.
    let out = ghost(xdg).args(["__upgrade", name]).output().unwrap();
    assert!(
        out.status.success(),
        "`ghost __upgrade` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The client must NOT have exited: it reconnected across the re-exec.
    assert!(
        !term.wait_exit(Duration::from_secs(2)),
        "the attach exited on the self-upgrade instead of reconnecting"
    );

    // …and it is still a live pipe to the SAME adopted child: typed input still
    // echoes back through `cat`.
    term.send(b"AFTER-UPGRADE\n");
    assert!(
        term.wait_for_screen(Duration::from_secs(5), screen_contains("AFTER-UPGRADE")),
        "reconnected attach did not echo input after the upgrade; got: {:?}",
        term.screen()
    );
}

/// Two local attaches to the same session must not FIGHT over the display. When
/// a second `ghost attach` takes over, the first is told it was superseded and
/// exits cleanly — it must NOT mistake that drop for a self-upgrade re-exec and
/// reconnect, which would steal the display straight back, forever. (Regression
/// guard for the reconnect-across-upgrade feature.)
#[test]
fn a_second_attach_supersedes_the_first_without_a_takeover_war() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "attach-takeover";
    let _guard = KillOnDrop { xdg, name };

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

    // First attach: confirm it is the live interactive display client.
    let mut first = Attached::new(xdg, name, 80, 24);
    first.send(b"first-live\n");
    assert!(
        first.wait_for_screen(Duration::from_secs(5), screen_contains("first-live")),
        "first attach not interactive; got: {:?}",
        first.screen()
    );

    // Second attach takes over the display.
    let second = Attached::new(xdg, name, 80, 24);

    // The FIRST attach must exit (it was superseded) rather than spinning to
    // reclaim the display.
    assert!(
        first.wait_exit(Duration::from_secs(5)),
        "the superseded first attach did not exit — it is fighting for the display"
    );

    // The second attach owns the session and is interactive.
    second.send(b"second-live\n");
    assert!(
        second.wait_for_screen(Duration::from_secs(5), screen_contains("second-live")),
        "second attach not interactive after take-over; got: {:?}",
        second.screen()
    );
}

/// Taking over the display from a STALLED client must not wedge the host. The
/// outgoing client is told it was superseded, but if it has frozen (a stopped
/// process / Ctrl-S — the usual reason the user is attaching from elsewhere) and
/// its socket buffers are full, that farewell must be best-effort and
/// non-blocking: a blocking flush would block the whole host inside the write,
/// unrecoverably (SIGTERM can't interrupt it). We assert the host stays
/// responsive by requiring `ghost kill` to still succeed after such a take-over.
#[test]
fn a_takeover_from_a_stalled_client_does_not_wedge_the_host() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "wedge";
    let _guard = KillOnDrop { xdg, name };

    // A child that floods output, so a stopped display client's send buffers
    // fill quickly.
    let out = ghost(xdg)
        .args([
            "new",
            name,
            "-d",
            "--",
            "sh",
            "-c",
            "while :; do echo flood; done",
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

    // First attach receives the flood, then we FREEZE it (stop the process): it
    // stops draining its socket, so the host's send buffers to it fill up.
    let first = Attached::new(xdg, name, 80, 24);
    assert!(
        first.wait_for_screen(Duration::from_secs(5), screen_contains("flood")),
        "first attach never saw output; got: {:?}",
        first.screen()
    );
    let first_pid = first.child.id().to_string();
    signal_pid(&first_pid, "STOP");
    // Let the flood fill the stalled client's outbuf + kernel send buffer.
    std::thread::sleep(Duration::from_millis(800));

    // Second attach takes over the display from the stalled first.
    let _second = Attached::new(xdg, name, 80, 24);

    // The crux: the host is NOT wedged in a blocking send — `ghost kill` (a
    // SIGTERM) still lands and tears the session down.
    let out = ghost(xdg).args(["kill", name]).output().unwrap();
    signal_pid(&first_pid, "CONT"); // let the frozen attach resume so it can exit
    assert!(
        out.status.success(),
        "`ghost kill` failed after a stalled-client take-over — the host is wedged: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wait_until(Duration::from_secs(5), || !ls(xdg).contains(name)),
        "session not gone after kill"
    );
}

/// A take-over from a STALLED, FLOODING client must not INVERT: the newcomer keeps
/// the display and the stalled incumbent yields. When the incumbent's socket
/// buffers are full its `Superseded` farewell is dropped (best-effort,
/// non-blocking — see the wedge test), so the incumbent sees only a bare EOF,
/// indistinguishable BY LIVENESS ALONE from a self-upgrade re-exec. It must not
/// reconnect on that: the host publishes an exec-generation marker that only a
/// real re-exec bumps, so a generation unchanged since attach means "you were
/// taken over — yield", not "re-exec — reconnect". Without the generation check
/// the stalled incumbent reconnects and steals the display straight back,
/// inverting the take-over (the flood-inversion regression guard).
#[test]
fn a_flooded_takeover_does_not_invert_when_the_farewell_is_dropped() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "invert";
    let _guard = KillOnDrop { xdg, name };

    // Same flood as the wedge test, so a stopped client's send buffers fill fast.
    let out = ghost(xdg)
        .args([
            "new",
            name,
            "-d",
            "--",
            "sh",
            "-c",
            "while :; do echo flood; done",
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

    // Incumbent attaches, receives the flood, then FREEZES: its socket send
    // buffers fill, so the farewell the host later pushes will WouldBlock and be
    // dropped — the incumbent will see a bare EOF, not a `Superseded`.
    let mut first = Attached::new(xdg, name, 80, 24);
    assert!(
        first.wait_for_screen(Duration::from_secs(5), screen_contains("flood")),
        "incumbent never saw output; got: {:?}",
        first.screen()
    );
    let first_pid = first.child.id().to_string();
    signal_pid(&first_pid, "STOP");
    std::thread::sleep(Duration::from_millis(800));

    // Newcomer takes over the display and becomes the live client.
    let mut second = Attached::new(xdg, name, 80, 24);
    assert!(
        second.wait_for_screen(Duration::from_secs(5), screen_contains("flood")),
        "newcomer never became the display client; got: {:?}",
        second.screen()
    );

    // Resume the incumbent so it can act on its bare EOF.
    signal_pid(&first_pid, "CONT");

    // The crux: the incumbent must YIELD (exit), NOT reconnect and steal the
    // display back — the generation it attached at is unchanged, so this is a
    // take-over, not a re-exec.
    assert!(
        first.wait_exit(Duration::from_secs(5)),
        "the flooded incumbent did not exit — it reconnected and inverted the take-over"
    );
    // …and the newcomer still owns the session (it did not lose the display to a
    // reconnecting incumbent).
    assert!(
        !second.wait_exit(Duration::from_secs(1)),
        "the newcomer lost the display to the reconnecting incumbent"
    );
}

/// Send signal `sig` to `pid` via `kill(1)`.
fn signal_pid(pid: &str, sig: &str) {
    let _ = Command::new("kill")
        .args([format!("-{sig}").as_str(), pid])
        .status();
}

#[test]
fn resize_propagates_to_session_child() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "attach-resize";
    let _guard = KillOnDrop { xdg, name };

    // A child that continuously reports its terminal size (`stty size` prints
    // "rows cols"). After the host resizes its PTY the new dimensions show up
    // on the next iteration — no dependence on signal-delivery timing.
    let out = ghost(xdg)
        .args([
            "new",
            name,
            "-d",
            "--",
            "sh",
            "-c",
            "while :; do stty size; sleep 0.2; done",
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

    let term = Attached::new(xdg, name, 80, 24);
    // The child first reports the initial 24x80 size.
    assert!(
        term.wait_for_screen(Duration::from_secs(5), screen_contains("24 80")),
        "initial size never reported; got: {:?}",
        term.screen()
    );

    // Resizing the terminal must reach the child through the
    // client -> host -> PTY resize path.
    term.resize(100, 30);
    assert!(
        term.wait_for_screen(Duration::from_secs(5), screen_contains("30 100")),
        "resized size never reached the child; got: {:?}",
        term.screen()
    );
}

#[test]
fn reattach_replays_screen_state() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "attach-replay";
    let _guard = KillOnDrop { xdg, name };

    // The session prints a marker exactly once, then keeps running (`cat`).
    let out = ghost(xdg)
        .args([
            "new",
            name,
            "-d",
            "--",
            "sh",
            "-c",
            "echo PERSISTENT-MARKER; exec cat",
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

    // First attach sees the marker live.
    let mut term = Attached::new(xdg, name, 80, 24);
    assert!(
        term.wait_for_screen(Duration::from_secs(5), screen_contains("PERSISTENT-MARKER")),
        "marker never appeared on first attach; got: {:?}",
        term.screen()
    );

    // Detach and reconnect. The marker was printed once, long ago, so it can
    // only reappear if the host replays its authoritative screen state on
    // attach — the live stream never repeats it.
    term.send(b"\x1cd");
    assert!(
        term.wait_exit(Duration::from_secs(5)),
        "client did not exit after detach"
    );
    drop(term);

    let term2 = Attached::new(xdg, name, 80, 24);
    assert!(
        term2.wait_for_screen(Duration::from_secs(5), screen_contains("PERSISTENT-MARKER")),
        "reattach did not replay screen state; got: {:?}",
        term2.screen()
    );
}

#[test]
fn reattach_replays_scrollback() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "attach-scrollback";
    let _guard = KillOnDrop { xdg, name };

    // Print a distinctive first line, then enough lines to push it well past
    // the 24-row viewport into scrollback, then idle.
    let out = ghost(xdg)
        .args([
            "new",
            name,
            "-d",
            "--",
            "sh",
            "-c",
            "echo FIRST-LINE-MARKER; for i in $(seq 1 50); do echo line-$i; done; exec cat",
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

    // First attach: wait for the last line so we know all output was produced.
    let mut term = Attached::new(xdg, name, 80, 24);
    assert!(
        term.wait_for_screen(Duration::from_secs(5), screen_contains("line-50")),
        "session output never appeared; got: {:?}",
        term.screen()
    );
    term.send(b"\x1cd");
    assert!(
        term.wait_exit(Duration::from_secs(5)),
        "client did not detach"
    );
    drop(term);

    // The first line scrolled off the viewport long ago; it can only reappear
    // if the host replays bounded scrollback on attach.
    let term2 = Attached::new(xdg, name, 80, 24);
    assert!(
        term2.wait_for_screen(Duration::from_secs(5), screen_contains("FIRST-LINE-MARKER")),
        "reattach did not replay scrollback; got: {:?}",
        term2.screen()
    );
}

#[test]
fn session_starts_in_launch_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    // A distinct directory to launch the session from; the session's shell
    // should start here, the way tmux/dtach/screen inherit the launch cwd —
    // not in `/` (where the daemon itself chdir's for daemon hygiene).
    let workdir = tempfile::tempdir().unwrap();
    let expected = workdir.path().canonicalize().unwrap();
    let name = "attach-cwd";
    let _guard = KillOnDrop { xdg, name };

    // `pwd -P` prints the physical cwd once (replayed from screen state on
    // attach), then the session idles on `cat`.
    let out = ghost(xdg)
        .current_dir(workdir.path())
        .args(["new", name, "-d", "--", "sh", "-c", "pwd -P; exec cat"])
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

    let term = Attached::new(xdg, name, 80, 24);
    let needle = expected.to_str().unwrap().to_owned();
    assert!(
        term.wait_for_screen(Duration::from_secs(5), screen_contains(&needle)),
        "session did not start in the launch directory ({needle:?}); got: {:?}",
        term.screen()
    );
}

#[test]
fn new_auto_attaches() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "new-autoattach";
    let _guard = KillOnDrop { xdg, name };

    // `ghost new` with no `-d` should attach us straight to the new session:
    // the marker the command prints appears with no separate `ghost attach`.
    let term = Attached::new_session(
        xdg,
        name,
        &["sh", "-c", "echo AUTO-ATTACH-MARKER; exec cat"],
        80,
        24,
    );
    assert!(
        term.wait_for_screen(
            Duration::from_secs(5),
            screen_contains("AUTO-ATTACH-MARKER")
        ),
        "auto-attach did not show session output; got: {:?}",
        term.screen()
    );

    // It is a real backgrounded session (listed by `ls`) and a live pipe: typed
    // input echoes back through `cat`.
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains(name)),
        "auto-attached session was not listed"
    );
    term.send(b"live-input\n");
    assert!(
        term.wait_for_screen(Duration::from_secs(5), screen_contains("live-input")),
        "auto-attached session did not echo input; got: {:?}",
        term.screen()
    );
}

#[test]
fn new_detached_does_not_attach() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "new-detached";
    let _guard = KillOnDrop { xdg, name };

    // `-d` keeps the old behaviour: return immediately with a confirmation and
    // leave the session running in the background, unattached.
    let out = ghost(xdg)
        .args([
            "new",
            name,
            "-d",
            "--",
            "sh",
            "-c",
            "echo DETACHED; exec cat",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new -d` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("started session"),
        "expected a confirmation message; got: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains(name)),
        "detached session was not listed"
    );

    // Attaching afterwards works and replays the output the session produced
    // while nobody was attached.
    let term = Attached::new(xdg, name, 80, 24);
    assert!(
        term.wait_for_screen(Duration::from_secs(5), screen_contains("DETACHED")),
        "could not attach to the detached session; got: {:?}",
        term.screen()
    );
}

#[test]
fn resync_uses_the_attaching_clients_size() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "attach-size";
    let _guard = KillOnDrop { xdg, name };

    // Default 80-wide session: print 50 X's with no newline (cursor ends at
    // column 50, comfortably within 80 columns), then idle.
    let fifty_x = "X".repeat(50);
    let out = ghost(xdg)
        .args([
            "new",
            name,
            "-d",
            "--",
            "sh",
            "-c",
            &format!("printf '{fifty_x}'; exec cat"),
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

    // Barrier: the reflow assertion below only tests the resync when the host's
    // screen already holds the X's — otherwise they arrive as live output after
    // the narrow attach and wrap naturally (cursor row 1), which is correct but
    // not what this test is about. `ls` listing the session doesn't imply the
    // child has printed yet, so attach at the session's own size and wait for
    // the content: the host feeds its screen before forwarding, so seeing the
    // X's here means the host screen has them.
    {
        let warm = Attached::new(xdg, name, 80, 24);
        assert!(
            warm.wait_for_screen(Duration::from_secs(5), screen_contains(&fifty_x)),
            "session content never appeared; got: {:?}",
            warm.screen()
        );
    } // drop: disconnect so the narrow attach below is the sole client

    // Attach at 40 columns — narrower than the session. If the host repaints at
    // the client's size, the 50 X's wrap at column 40 and the cursor settles at
    // column 10 on the second visual row's worth of cells; if it repaints at the
    // stale 80-column size, the absolute cursor move clamps to the last column
    // (39). The wrapped text reads the same either way, so we check the cursor
    // column. (avt keeps the cursor on row 0 across reflow, so only the column
    // distinguishes the two.)
    let term = Attached::new(xdg, name, 40, 24);
    assert!(
        term.wait_for_screen(Duration::from_secs(5), screen_contains("XXXXXXXXXX")),
        "session content never replayed; got: {:?}",
        term.screen()
    );
    assert!(
        term.wait_until(Duration::from_secs(5), |t| t.cursor() == (0, 10)),
        "resync was not laid out at the client's width (cursor at {:?}, expected (0, 10)); screen: {:?}",
        term.cursor(),
        term.screen()
    );
}

#[test]
fn cli_rename_changes_session_name() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let old = "rename-old";
    let new = "rename-fresh";
    // Kill under whichever name the session ends up holding.
    let _g_old = KillOnDrop { xdg, name: old };
    let _g_new = KillOnDrop { xdg, name: new };

    let out = ghost(xdg)
        .args(["new", old, "-d", "--", "sh", "-c", "echo HItag; exec cat"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains(old)),
        "session not listed under old name"
    );

    let out = ghost(xdg).args(["rename", old, new]).output().unwrap();
    assert!(
        out.status.success(),
        "`ghost rename` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // `ls` now shows the new name and not the old one.
    assert!(
        wait_until(Duration::from_secs(5), || {
            let l = ls(xdg);
            l.contains(new) && !l.contains(old)
        }),
        "rename not reflected in `ls`: {}",
        ls(xdg)
    );

    // The renamed session is still alive: attaching by the new name replays the
    // output it produced before the rename.
    let term = Attached::new(xdg, new, 80, 24);
    assert!(
        term.wait_for_screen(Duration::from_secs(5), screen_contains("HItag")),
        "renamed session not reachable; got: {:?}",
        term.screen()
    );
}

#[test]
fn cli_rename_does_not_disturb_attached_client() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let old = "busy-old";
    let new = "busy-fresh";
    let _g_old = KillOnDrop { xdg, name: old };
    let _g_new = KillOnDrop { xdg, name: new };

    // Auto-attach to a live session.
    let term = Attached::new_session(xdg, old, &["sh", "-c", "echo READYtag; exec cat"], 80, 24);
    assert!(
        term.wait_for_screen(Duration::from_secs(5), screen_contains("READYtag")),
        "session not attached; got: {:?}",
        term.screen()
    );

    // Rename from outside, via the CLI, while the client stays attached.
    let out = ghost(xdg).args(["rename", old, new]).output().unwrap();
    assert!(
        out.status.success(),
        "`ghost rename` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains(new)),
        "rename not reflected in `ls`: {}",
        ls(xdg)
    );

    // The control connection must NOT have bumped the attached client: it is
    // still live and interactive, so typed input still echoes back.
    term.send(b"still-here\n");
    assert!(
        term.wait_for_screen(Duration::from_secs(5), screen_contains("still-here")),
        "attached client was disturbed by the rename; got: {:?}",
        term.screen()
    );
}

#[test]
fn rename_is_refused_for_a_host_predating_label_renames() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let old = "elder-old";
    let new = "elder-fresh";
    let _g = KillOnDrop { xdg, name: old };

    let out = ghost(xdg)
        .args(["new", old, "-d", "--", "sh", "-c", "exec cat"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains(old)),
        "session not listed"
    );

    // Masquerade as a host built before label renames (proto level 1, the Theme
    // era): such a host would MOVE the session directory on Rename — the exact
    // file churn that detaches clients and strands stale ids in windows. The
    // client must refuse rather than trigger it. (Level 0 — no marker at all —
    // is the same refusal path.)
    let dir = xdg.join("run/ghost").join(old);
    std::fs::write(dir.join("proto"), "1").unwrap();

    let out = ghost(xdg).args(["rename", old, new]).output().unwrap();
    assert!(
        !out.status.success(),
        "renaming an old host must be refused, got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("restart"),
        "the refusal should tell the user to restart the session: {err}"
    );
    // Nothing changed: same name listed, no directory for the new name.
    assert!(
        ls(xdg).contains(old),
        "the session kept its name: {}",
        ls(xdg)
    );
    assert!(
        !xdg.join("run/ghost").join(new).exists(),
        "no rename side effects on disk"
    );
}

#[test]
fn rename_moves_no_files_and_keeps_the_attach_marker() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let old = "steady-old";
    let new = "steady-fresh";
    let _g_old = KillOnDrop { xdg, name: old };
    let _g_new = KillOnDrop { xdg, name: new };

    let term = Attached::new_session(xdg, old, &["sh", "-c", "echo READYtag; exec cat"], 80, 24);
    assert!(
        term.wait_for_screen(Duration::from_secs(5), screen_contains("READYtag")),
        "session not attached; got: {:?}",
        term.screen()
    );

    // The rename is a display-name change only: the session's identity — and with
    // it the socket, pid file, and attached marker — must stay exactly where they
    // are, so nothing about the attachment can be disturbed.
    let dir = xdg.join("run/ghost").join(old);
    assert!(
        dir.join("sock").exists(),
        "socket missing before the rename"
    );
    assert!(
        wait_until(Duration::from_secs(5), || dir.join("attached").exists()),
        "attached marker missing before the rename"
    );

    let out = ghost(xdg).args(["rename", old, new]).output().unwrap();
    assert!(
        out.status.success(),
        "`ghost rename` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains(new)),
        "rename not reflected in `ls`: {}",
        ls(xdg)
    );

    assert!(
        dir.join("sock").exists(),
        "rename moved the session's socket"
    );
    assert!(
        dir.join("attached").exists(),
        "rename dropped the attached marker"
    );
    assert!(
        !xdg.join("run/ghost").join(new).exists(),
        "rename created a directory for the display name"
    );
}

#[test]
fn rename_keeps_a_detached_session_detached() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let old = "calm-old";
    let new = "calm-fresh";
    let _g_old = KillOnDrop { xdg, name: old };
    let _g_new = KillOnDrop { xdg, name: new };

    let out = ghost(xdg)
        .args(["new", old, "-d", "--", "sh", "-c", "echo HItag; exec cat"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains(old)),
        "session not listed"
    );
    let dir = xdg.join("run/ghost").join(old);
    assert!(
        !dir.join("attached").exists(),
        "a detached session must carry no attached marker"
    );

    let out = ghost(xdg).args(["rename", old, new]).output().unwrap();
    assert!(
        out.status.success(),
        "`ghost rename` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains(new)),
        "rename not reflected in `ls`: {}",
        ls(xdg)
    );

    // Renaming must not attach: the session stays detached, and is still
    // reachable (by its display name) afterwards.
    assert!(
        !dir.join("attached").exists(),
        "rename attached a detached session"
    );
    let term = Attached::new(xdg, new, 80, 24);
    assert!(
        term.wait_for_screen(Duration::from_secs(5), screen_contains("HItag")),
        "renamed session not reachable by display name; got: {:?}",
        term.screen()
    );
}

#[test]
fn rename_prompt_renames_attached_session() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let old = "prompt-old";
    let new = "prompt-fresh";
    let _g_old = KillOnDrop { xdg, name: old };
    let _g_new = KillOnDrop { xdg, name: new };

    let term = Attached::new_session(xdg, old, &["sh", "-c", "echo READYtag; exec cat"], 80, 24);
    assert!(
        term.wait_for_screen(Duration::from_secs(5), screen_contains("READYtag")),
        "session not attached; got: {:?}",
        term.screen()
    );

    // Open the rename prompt with the default trigger: Ctrl-\ then 'r'.
    term.send(b"\x1cr");
    assert!(
        term.wait_for_screen(Duration::from_secs(5), screen_contains("rename session to")),
        "rename prompt did not appear; got: {:?}",
        term.screen()
    );

    // Type the new name and confirm with Enter.
    term.send(new.as_bytes());
    term.send(b"\r");

    assert!(
        wait_until(Duration::from_secs(5), || {
            let l = ls(xdg);
            l.contains(new) && !l.contains(old)
        }),
        "prompt rename not reflected in `ls`: {}",
        ls(xdg)
    );

    // Session survives the rename and stays interactive under the new name.
    term.send(b"post-rename\n");
    assert!(
        term.wait_for_screen(Duration::from_secs(5), screen_contains("post-rename")),
        "session not interactive after prompt rename; got: {:?}",
        term.screen()
    );
}
