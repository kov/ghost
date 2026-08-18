//! A trace of everything ghost sends a child, for catching the "Claude's
//! question stops accepting input" class of bug in the act.
//!
//! Every byte that leaves for a program — keystrokes, pasted text, mouse
//! reports, query replies, the DEC ?1004 focus conversation, graphics
//! acknowledgements — appends one timestamped line, alongside the terminal
//! state ghost believed it was in when it sent them and what the write actually
//! did. A session recording already carries the bytes; this carries the *why*,
//! and it carries it on the machine running the GUI, which for a remote session
//! is not the machine holding the recording.
//!
//! Armed two ways. `[diagnostics] wire_trace = true` in `ui.toml` is the one a
//! user reaches for: it survives however the GUI was started (a desktop file has
//! nobody to hand an env var to), it takes effect on the next config reload, and
//! it writes to [`crate::paths::wire_trace_path`] — a path ghost picks, so
//! there is nothing to remember. `GHOST_FOCUS_TRACE=/path/to/file` still names a
//! destination directly and wins over the config, for tests and one-off runs.
//! Disarmed (the normal case), every hook costs one relaxed atomic load and one
//! environment lookup, and returns before rendering anything.
//!
//! The file is opened per event, so there is no shared state to initialize (or to
//! latch a stale read), it works however the process was launched, and concurrent
//! writers interleave safely — each event is one `write_all` of a whole line in
//! append mode. It is rolled at [`MAX_BYTES`]: a program holding any-motion mouse
//! tracking produces a report per cell the pointer crosses, so an armed trace left
//! on for a week must not be able to fill the disk.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const VAR: &str = "GHOST_FOCUS_TRACE";

/// Roll the log once it passes this, keeping one previous generation.
pub const MAX_BYTES: u64 = 8 * 1024 * 1024;

/// The config-armed destination. `None` = off; the env var overrides either way.
static CONFIGURED: Mutex<Option<PathBuf>> = Mutex::new(None);
/// A lock-free mirror of "some destination is set", so the disarmed case — every
/// keystroke, every pointer motion — costs one relaxed load rather than a mutex.
static ARMED: AtomicBool = AtomicBool::new(false);
/// Mouse reports sent since this process started. Carried on each mouse line so a
/// flood is countable straight out of the log (and so reports generated but never
/// written are visible as a gap).
static MOUSE_REPORTS: AtomicU64 = AtomicU64::new(0);

/// Arm or disarm the config-driven trace. Called at startup and on every config
/// reload, so editing `ui.toml` turns it on under a running GUI.
pub fn set_enabled(on: bool) {
    let dest = on.then(crate::paths::wire_trace_path);
    if let Ok(mut c) = CONFIGURED.lock() {
        *c = dest;
        ARMED.store(c.is_some(), Ordering::Relaxed);
    }
}

/// Whether tracing is on — for callers that must do extra work (rendering bytes,
/// reading the terminal's mode state) before they have anything to log.
pub fn enabled() -> bool {
    ARMED.load(Ordering::Relaxed) || std::env::var_os(VAR).is_some()
}

/// Where lines go, if anywhere. An explicit `GHOST_FOCUS_TRACE` wins: a run that
/// names its own file means it, and a test must never be redirected into the
/// user's log by a config it did not write.
fn destination() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(VAR) {
        return Some(PathBuf::from(p));
    }
    if !ARMED.load(Ordering::Relaxed) {
        return None;
    }
    CONFIGURED.lock().ok()?.clone()
}

/// The focus report an input payload carries, if any — `"I"`/`"O"` for the wire
/// log. Reports are sent alone or alongside query replies, so scan rather than
/// compare.
pub fn report_in(bytes: &[u8]) -> Option<&'static str> {
    if bytes.windows(3).any(|w| w == b"\x1b[I") {
        Some("I")
    } else if bytes.windows(3).any(|w| w == b"\x1b[O") {
        Some("O")
    } else {
        None
    }
}

/// Bytes as a terminal-log reader expects to see them: ESC as `^[`, the other C0
/// controls as `^X`, anything not printable ASCII as `\xNN`. Lossless enough to
/// retype, and — unlike raw bytes — it cannot reprogram the terminal of whoever
/// `cat`s the log.
pub fn escape(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() + 8);
    for &b in bytes {
        match b {
            0x1b => out.push_str("^["),
            0x00..=0x1f => {
                out.push('^');
                out.push((b + b'@') as char);
            }
            0x20..=0x7e => out.push(b as char),
            _ => out.push_str(&format!("\\x{b:02x}")),
        }
    }
    out
}

/// Count one mouse report and return its running total, for the `#n` on a mouse
/// line. Only called while tracing.
pub fn count_mouse_report() -> u64 {
    MOUSE_REPORTS.fetch_add(1, Ordering::Relaxed) + 1
}

/// Append one `<unix-ms> <hh:mm:ss.mmm>Z [<pid>] <session> <event>` line.
/// `session` is the id the event concerns, or `"*"` for app-wide events. The
/// clock is UTC; the pid separates two ghosts sharing the file.
pub fn log(session: &str, event: std::fmt::Arguments<'_>) {
    let Some(path) = destination() else {
        return;
    };
    roll_if_full(&path);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let line = format!(
        "{ms} {:02}:{:02}:{:02}.{:03}Z [{}] {session} {event}\n",
        ms / 3_600_000 % 24,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000,
        std::process::id(),
    );
    let _ = file.write_all(line.as_bytes());
}

/// Roll the log to `<name>.1` once it passes [`MAX_BYTES`], keeping one previous
/// generation. Best-effort: a rename losing a race with another writer costs at
/// most a few lines, which is why this is not worth a lock.
fn roll_if_full(path: &std::path::Path) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() < MAX_BYTES {
        return;
    }
    let mut prev = path.as_os_str().to_os_string();
    prev.push(".1");
    let _ = std::fs::rename(path, prev);
}

/// Test-only: run `f` with the trace pointed at a fresh file, and return every line
/// it wrote. Serialized process-wide — the destination is an env var, so two
/// capturing tests running at once would write into each other's file (and each
/// other's assertions).
///
/// Public because the crates whose tracing this covers are OTHER crates, and a
/// `#[cfg(test)]` item is invisible across a crate boundary.
#[doc(hidden)]
pub fn capture(f: impl FnOnce()) -> String {
    use std::sync::atomic::AtomicU32;
    static LOCK: Mutex<()> = Mutex::new(());
    static NTH: AtomicU32 = AtomicU32::new(0);
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = std::env::temp_dir().join(format!(
        "ghost-wire-trace-{}-{}.log",
        std::process::id(),
        NTH.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    // SAFETY: the var is process-global, but every test that sets or reads it holds
    // the lock above, and `log` re-reads it per event (no latched state to poison).
    unsafe { std::env::set_var(VAR, &path) };
    f();
    unsafe { std::env::remove_var(VAR) };
    let out = std::fs::read_to_string(&path).unwrap_or_default();
    let _ = std::fs::remove_file(&path);
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn focus_reports_are_spotted_inside_input_payloads() {
        // A report alone, and one riding along with other input (the ingest can
        // batch a query reply and the rising-edge report into one payload).
        assert_eq!(super::report_in(b"\x1b[I"), Some("I"));
        assert_eq!(super::report_in(b"\x1b[0n\x1b[O"), Some("O"));
        // Ordinary keys — including a plain CSI arrow — are not reports.
        assert_eq!(super::report_in(b"hello"), None);
        assert_eq!(super::report_in(b"\x1b[A"), None);
    }

    #[test]
    fn bytes_are_logged_readably_and_inertly() {
        // What a mouse report and a keystroke look like in the log.
        assert_eq!(super::escape(b"\x1b[<35;12;5M"), "^[[<35;12;5M");
        assert_eq!(super::escape(b"hi\r"), "hi^M");
        // A raw ESC in the log would reprogram the terminal reading it; UTF-8 and
        // any other high byte survive as an escape rather than as itself.
        assert_eq!(super::escape("é".as_bytes()), "\\xc3\\xa9");
    }
}
