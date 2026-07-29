//! An env-gated trace of the focus-reporting conversation, for catching the
//! "Claude's question stops accepting input" class of bug in the act: set
//! `GHOST_FOCUS_TRACE=/path/to/file` and every focus-related event — DEC ?1004
//! mode flips, window focus events (including ones MUTED because the mode is
//! off), rising-edge reports, promotion reasserts, what actually went over the
//! wire, and remote transport drops/reattaches — appends one timestamped line.
//! Unset (the normal case), every hook is a single env lookup that returns.
//!
//! The file is opened per event, so there is no shared state to initialize (or
//! to latch a stale env read), it works however the process was launched, and
//! concurrent writers interleave safely — each event is one `write_all` of a
//! whole line in append mode.

use std::io::Write;

const VAR: &str = "GHOST_FOCUS_TRACE";

/// Whether tracing is on — for callers that must do extra work (scanning an
/// input payload) before they have anything to log.
pub fn enabled() -> bool {
    std::env::var_os(VAR).is_some()
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

/// Append one `<unix-ms> <hh:mm:ss.mmm>Z <session> <event>` line. `session` is
/// the id the event concerns, or `"*"` for app-wide events. The clock is UTC.
pub fn log(session: &str, event: std::fmt::Arguments<'_>) {
    let Some(path) = std::env::var_os(VAR) else {
        return;
    };
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let line = format!(
        "{ms} {:02}:{:02}:{:02}.{:03}Z {session} {event}\n",
        ms / 3_600_000 % 24,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000,
    );
    let _ = file.write_all(line.as_bytes());
}

/// Test-only: run `f` with the trace pointed at a fresh file, and return every line
/// it wrote. Serialized process-wide — the destination is an env var, so two
/// capturing tests running at once would write into each other's file (and each
/// other's assertions).
#[cfg(test)]
pub(crate) fn capture(f: impl FnOnce()) -> String {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, Ordering};
    static LOCK: Mutex<()> = Mutex::new(());
    static NTH: AtomicU32 = AtomicU32::new(0);
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = std::env::temp_dir().join(format!(
        "ghost-focus-trace-{}-{}.log",
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
}
