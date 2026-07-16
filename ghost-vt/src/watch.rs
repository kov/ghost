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

/// Whether a filesystem event should wake the emit loop. A bare read (`Access`)
/// must not: [`emit`] opens the runtime dir and every `<session>/meta` to build
/// the listing, and under the recursive watch those reads would feed straight
/// back in as events, spinning the loop forever. Only a mutation — a session dir
/// created/removed, or a `meta`/marker file written (a label/title change, an
/// attach/bell toggle) — changes what a listing reports, so only mutations wake.
fn wakes(kind: &notify::EventKind) -> bool {
    !matches!(kind, notify::EventKind::Access(_))
}

/// Stream the session listing to stdout: once now, then on every runtime-tree
/// mutation (coalesced, and only when the listing actually changed) and on a
/// [`HEARTBEAT`] tick, until the reader goes away.
pub fn run() -> io::Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    let mut last = listing_line()?;
    write_line(&mut out, &last)?;

    let dir = paths::runtime_dir();
    // The dir may not exist before the first session; create it so the watch binds.
    std::fs::create_dir_all(&dir).ok();

    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res
            && wakes(&ev.kind)
        {
            let _ = tx.send(());
        }
    })
    .map_err(io::Error::other)?;
    use notify::Watcher;
    // Recursive: a label or title change rewrites `<session>/meta`, a write one
    // level below the runtime dir that a non-recursive watch never sees — so a
    // rename/retitle would otherwise not propagate until the next heartbeat.
    watcher
        .watch(&dir, notify::RecursiveMode::Recursive)
        .map_err(io::Error::other)?;

    loop {
        match rx.recv_timeout(HEARTBEAT) {
            // A mutation: coalesce the burst, then emit the fresh listing — but
            // only if it actually changed, so a write that doesn't alter the
            // listing (or an already-coalesced burst) costs no push over the pipe.
            Ok(_) => {
                while rx.recv_timeout(COALESCE).is_ok() {}
                let line = listing_line()?;
                if line != last {
                    write_line(&mut out, &line)?;
                    last = line;
                }
            }
            // Keepalive: always re-emit, even unchanged — it refreshes the listing
            // and detects a closed pipe via the failing write.
            Err(mpsc::RecvTimeoutError::Timeout) => {
                last = listing_line()?;
                write_line(&mut out, &last)?;
            }
            // The watcher was dropped — cannot happen while `watcher` is held.
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

/// Parse one line of [`run`]'s output back into a session listing — the reader's
/// inverse of [`emit`], so the JSON shape stays owned here.
pub fn parse_listing(line: &str) -> serde_json::Result<Vec<session::SessionInfo>> {
    serde_json::from_str(line)
}

/// The current session listing as one line of JSON (the same shape as
/// `ghost ls --json`), without the trailing newline.
fn listing_line() -> io::Result<String> {
    let sessions = session::list().unwrap_or_default();
    serde_json::to_string(&sessions).map_err(io::Error::other)
}

/// Write one listing line, newline-terminated and flushed.
fn write_line(out: &mut impl Write, line: &str) -> io::Result<()> {
    out.write_all(line.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()
}

/// Write the current session listing as one line of JSON (the same shape as
/// `ghost ls --json`), newline-terminated and flushed.
pub fn emit(out: &mut impl Write) -> io::Result<()> {
    write_line(out, &listing_line()?)
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

    #[test]
    fn a_listing_from_a_host_predating_every_optional_field_still_parses() {
        // Cross-version forward-compat. A listing written by an OLDER host omits the
        // fields appended to SessionInfo since (created_at, cwd, size, connection),
        // so parsing it on a newer client must still succeed — every appended field
        // stays omittable (Option or #[serde(default)]). If a *non*-Option field is
        // ever appended this breaks, and the client's __watch reader would drop the
        // WHOLE remote fleet against any host predating the field, not just miss the
        // one value. Keep new listing fields omittable (or update this test knowing
        // it severs cross-version listings).
        let older_host_line = r#"[{
            "name": "s1", "pid": 42, "title": "", "command": ["bash"],
            "attached": false, "bell": false, "display_name": ""
        }]"#;
        let infos =
            parse_listing(older_host_line).expect("an older host's listing must still parse");
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].name, "s1");
        assert_eq!(infos[0].size, None);
        assert_eq!(infos[0].connection, None);
        assert_eq!(infos[0].created_at, None);
        assert_eq!(infos[0].cwd, None);
    }
}
