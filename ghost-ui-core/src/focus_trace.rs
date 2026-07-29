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
