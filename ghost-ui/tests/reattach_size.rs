//! End-to-end: a GUI client must complete its attach handshake at the size it
//! will actually render at, not a provisional default.
//!
//! The host lays out its resync (the repaint a reattaching client receives) at
//! the size of the first resize it sees — the handshake. A maximized GUI window
//! that handshakes at a fixed 80×24 makes the host reflow a full-size screen down
//! to 80×24 and pin the cursor to *that* smaller bottom row; a later resize back
//! up to the real size can't recover it, so the next byte of output lands
//! mid-screen — the "output cut off, prompt and cursor at the cut-off point" bug.
//!
//! These tests drive the **real** session host (`ghost new`), the **real**
//! `Session` client, and the **real** `TerminalModel` the windowed shell feeds —
//! the same spawn → attach → resize → pump → feed path the GUI runs — and assert
//! on the model's resulting screen. They reproduce both the broken (provisional
//! handshake) and correct (real-size handshake) reattach so the fix is pinned.

use std::process::Command;
use std::time::{Duration, Instant};

use ghost_render::CellMetrics;
use ghost_ui_core::{TerminalModel, UiEvent};
use ghost_vt::client::Session;

const GHOST: &str = env!("CARGO_BIN_EXE_ghost");

// Arbitrary positive metrics: these tests size the model in cells directly, so
// the per-pixel values never enter the picture.
const METRICS: CellMetrics = CellMetrics {
    advance: 8.0,
    line_height: 16.0,
};

// A "maximized" window, comfortably larger than the legacy 80×24 default.
const COLS: u16 = 100;
const ROWS: u16 = 40;

struct Xdg {
    _tmp: tempfile::TempDir,
    run: std::path::PathBuf,
    data: std::path::PathBuf,
}

impl Xdg {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let run = tmp.path().join("run");
        let data = tmp.path().join("data");
        std::fs::create_dir_all(&run).unwrap();
        Xdg {
            _tmp: tmp,
            run,
            data,
        }
    }

    fn ghost(&self) -> Command {
        let mut c = Command::new(GHOST);
        c.env("XDG_RUNTIME_DIR", &self.run)
            .env("XDG_DATA_HOME", &self.data);
        c
    }

    fn sock(&self, name: &str) -> std::path::PathBuf {
        self.run.join("ghost").join(name).join("sock")
    }
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
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Spawn a detached session that fills well past one screen with distinct,
/// numbered lines, ends at a prompt-like marker, then idles on `cat`.
fn spawn_filled(xdg: &Xdg, name: &str) {
    let out = xdg
        .ghost()
        .args([
            "new",
            name,
            "-d",
            "--",
            "sh",
            "-c",
            "for i in $(seq 1 100); do printf 'LINE-%d-padpadpadpadpad\\n' \"$i\"; done; \
             printf 'PROMPT$ '; exec cat",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wait_until(Duration::from_secs(5), || xdg.sock(name).exists()),
        "session socket never appeared"
    );
}

/// A GUI client: a real `Session` plus the real `TerminalModel` the shell feeds.
struct GuiClient {
    name: String,
    session: Session,
    model: TerminalModel,
}

impl GuiClient {
    /// Attach as the windowed shell does: handshake at `handshake`, render at
    /// `model_size`. When they differ (the bug), the model also sends its own
    /// real resize afterwards — exactly the shell's `attach` then `Cmd::Resize`.
    /// Takes the socket path explicitly so parallel tests don't share env state.
    fn attach(
        sock: &std::path::Path,
        name: &str,
        handshake: (u16, u16),
        model_size: (u16, u16),
    ) -> Self {
        let mut session = Session::attach_deferred_path(sock, name).unwrap();
        session
            .set_read_timeout(Some(Duration::from_millis(1)))
            .unwrap();
        session.resize(handshake.0, handshake.1).unwrap();
        if handshake != model_size {
            session.resize(model_size.0, model_size.1).unwrap();
        }
        GuiClient {
            name: name.to_string(),
            session,
            model: TerminalModel::new(name.to_string(), model_size.0, model_size.1, METRICS),
        }
    }

    /// Drain ready output once and feed it into the model, as the shell's loop does.
    fn pump_once(&mut self) {
        let mut bytes = Vec::new();
        let mut ended = false;
        for _ in 0..64 {
            match self.session.pump() {
                Ok(p) => {
                    let empty = p.output.is_empty();
                    bytes.extend_from_slice(&p.output);
                    ended |= p.ended;
                    if p.ended || empty {
                        break;
                    }
                }
                Err(_) => {
                    ended = true;
                    break;
                }
            }
        }
        if !bytes.is_empty() || ended {
            self.model.update(UiEvent::SessionData {
                name: self.name.clone(),
                bytes,
                ended,
            });
        }
    }

    /// Pump until the model's screen satisfies `pred`, or time out.
    fn pump_until(&mut self, timeout: Duration, mut pred: impl FnMut(&Self) -> bool) -> bool {
        let start = Instant::now();
        loop {
            self.pump_once();
            if pred(self) {
                return true;
            }
            if start.elapsed() >= timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// The visible viewport as trimmed text rows.
    fn viewport(&self) -> Vec<String> {
        self.model
            .screen()
            .vt()
            .view()
            .map(|l| l.text().trim_end().to_string())
            .collect()
    }

    /// Cursor row, 1-based (1 == top, `rows` == bottom).
    fn cursor_row(&self) -> u16 {
        self.model.screen().cursor().1
    }

    fn send(&mut self, bytes: &[u8]) {
        self.session.send_input(bytes).unwrap();
    }
}

fn has_line(view: &[String], needle: &str) -> bool {
    view.iter().any(|l| l.contains(needle))
}

#[test]
fn reattach_at_window_size_keeps_the_cursor_at_the_bottom() {
    let xdg = Xdg::new();
    let name = format!("reattach-ok-{}", std::process::id());
    spawn_filled(&xdg, &name);
    let sock = xdg.sock(&name);

    // First attach at the real window size — the canonical full-screen state.
    let mut first = GuiClient::attach(&sock, &name, (COLS, ROWS), (COLS, ROWS));
    assert!(
        first.pump_until(Duration::from_secs(5), |c| {
            has_line(&c.viewport(), "LINE-100") && has_line(&c.viewport(), "PROMPT$")
        }),
        "session content never streamed in; got: {:?}",
        first.viewport()
    );
    let ground_truth = first.viewport();
    assert_eq!(
        first.cursor_row(),
        ROWS,
        "baseline cursor should sit on the bottom row"
    );
    drop(first);

    // Reattach the way the fixed shell does: one handshake at the real size.
    let mut again = GuiClient::attach(&sock, &name, (COLS, ROWS), (COLS, ROWS));
    assert!(
        again.pump_until(Duration::from_secs(5), |c| has_line(
            &c.viewport(),
            "LINE-100"
        )),
        "reattach never replayed the screen; got: {:?}",
        again.viewport()
    );

    // The replayed screen matches, and — the crux — the cursor is restored to the
    // real bottom row, not a stale smaller one.
    assert_eq!(
        again.viewport(),
        ground_truth,
        "reattached viewport differs from the original"
    );
    assert_eq!(
        again.cursor_row(),
        ROWS,
        "reattach must restore the cursor to the bottom row"
    );

    // The next output therefore appends at the bottom and leaves the body intact.
    again.send(b"AFTER-REATTACH\n");
    assert!(
        again.pump_until(Duration::from_secs(5), |c| has_line(
            &c.viewport(),
            "AFTER-REATTACH"
        )),
        "new output never echoed; got: {:?}",
        again.viewport()
    );
    assert!(
        has_line(&again.viewport(), "LINE-99-padpadpadpadpad"),
        "new output clobbered the body — LINE-99 went missing: {:?}",
        again.viewport()
    );
}

/// Characterizes the failure the fix prevents: a provisional 80×24 handshake on a
/// 100×40 window leaves the cursor stranded at row 24, so the next output corrupts
/// the middle of the screen. This is the host behaving correctly for the size it
/// was *told* — which is exactly why the shell must hand it the real size.
#[test]
fn provisional_handshake_strands_the_cursor_mid_screen() {
    let xdg = Xdg::new();
    let name = format!("reattach-bug-{}", std::process::id());
    spawn_filled(&xdg, &name);
    let sock = xdg.sock(&name);

    // Establish the full-size state.
    let mut first = GuiClient::attach(&sock, &name, (COLS, ROWS), (COLS, ROWS));
    assert!(
        first.pump_until(Duration::from_secs(5), |c| has_line(
            &c.viewport(),
            "PROMPT$"
        )),
        "session content never streamed in; got: {:?}",
        first.viewport()
    );
    drop(first);

    // Reattach the old (broken) way: handshake at 80×24, then resize up to 100×40.
    let mut again = GuiClient::attach(&sock, &name, (80, 24), (COLS, ROWS));
    assert!(
        again.pump_until(Duration::from_secs(5), |c| has_line(
            &c.viewport(),
            "PROMPT$"
        )),
        "reattach never replayed; got: {:?}",
        again.viewport()
    );

    // The resync was laid out at 24 rows, so the cursor is pinned to row 24, not
    // the real bottom row 40.
    assert_eq!(
        again.cursor_row(),
        24,
        "the provisional handshake should strand the cursor at the 24-row bottom"
    );

    // Proof of the visible corruption: the next output lands at the stranded
    // cursor row, mangling that body line instead of appending below the prompt.
    let before = again.viewport();
    let clobber_row = again.cursor_row() as usize; // 1-based
    let victim = before[clobber_row - 1].clone();
    assert!(
        victim.starts_with("LINE-") && victim.ends_with("padpadpadpadpad"),
        "expected an intact body line under the stranded cursor, got {victim:?}"
    );
    again.send(b"CORRUPT\n");
    assert!(
        again.pump_until(Duration::from_secs(5), |c| has_line(
            &c.viewport(),
            "CORRUPT"
        )),
        "marker never echoed; got: {:?}",
        again.viewport()
    );
    assert!(
        !has_line(&again.viewport(), &victim),
        "expected the next output to corrupt the mid-screen line {victim:?}, but the body was intact: {:?}",
        again.viewport()
    );
}
