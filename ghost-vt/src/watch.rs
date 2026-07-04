//! The `ghost __watch` session-set stream — the pushed replacement for the
//! fleet's poll.
//!
//! Run on a machine hosting sessions, it writes the current listing as one line
//! of JSON, then re-writes it whenever the runtime dir changes (a session
//! created, killed, or renamed). A client — local, or tunnelled over the ssh
//! transport as `ssh host -- ghost __watch` — learns of changes at once instead
//! of re-listing on a timer. A slow heartbeat re-emits too, which both refreshes
//! the listing and surfaces a gone client (the write fails once its pipe closes).

use crate::{paths, session};
use std::io::{self, Write};
use std::sync::mpsc;
use std::time::Duration;

/// Re-emit at least this often even with no filesystem change: a keepalive that
/// also surfaces a gone reader (a write to its closed pipe fails, ending the loop).
const HEARTBEAT: Duration = Duration::from_secs(30);

/// Coalesce a burst of filesystem events (a session spawn touches several files)
/// into a single listing.
const COALESCE: Duration = Duration::from_millis(50);

/// Stream the session listing to stdout: once now, then on every runtime-dir
/// change (coalesced) and on a [`HEARTBEAT`] tick, until the reader goes away.
pub fn run() -> io::Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    emit(&mut out)?;

    let dir = paths::runtime_dir();
    // The dir may not exist before the first session; create it so the watch binds.
    std::fs::create_dir_all(&dir).ok();

    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let _ = tx.send(res.is_ok());
    })
    .map_err(io::Error::other)?;
    use notify::Watcher;
    watcher
        .watch(&dir, notify::RecursiveMode::NonRecursive)
        .map_err(io::Error::other)?;

    loop {
        match rx.recv_timeout(HEARTBEAT) {
            // A change: coalesce the burst, then emit the fresh listing once.
            Ok(_) => {
                while rx.recv_timeout(COALESCE).is_ok() {}
                emit(&mut out)?;
            }
            // Keepalive: re-emit (and detect a closed pipe via the failing write).
            Err(mpsc::RecvTimeoutError::Timeout) => emit(&mut out)?,
            // The watcher was dropped — cannot happen while `watcher` is held.
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

/// Write the current session listing as one line of JSON (the same shape as
/// `ghost ls --json`), newline-terminated and flushed.
pub fn emit(out: &mut impl Write) -> io::Result<()> {
    let sessions = session::list().unwrap_or_default();
    let line = serde_json::to_string(&sessions).map_err(io::Error::other)?;
    out.write_all(line.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emit_writes_one_newline_terminated_json_listing() {
        let mut buf = Vec::new();
        emit(&mut buf).unwrap();
        assert_eq!(buf.last(), Some(&b'\n'), "the listing is one line");
        let line = std::str::from_utf8(&buf).unwrap().trim_end();
        // Parses back into the same shape `ghost ls --json` / the poller consume.
        let _: Vec<session::SessionInfo> =
            serde_json::from_str(line).expect("emitted listing parses");
    }
}
