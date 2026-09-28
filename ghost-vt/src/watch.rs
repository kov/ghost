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
    let (_watcher, changes) = watch_set()?;
    let mut failed = None;
    changes.stream(|sessions| {
        let line = serde_json::to_string(sessions).map_err(io::Error::other);
        match line.and_then(|l| write_line(&mut out, &l)) {
            Ok(()) => true,
            Err(e) => {
                failed = Some(e);
                false
            }
        }
    });
    failed.map_or(Ok(()), Err)
}

/// Mutations of this machine's session set, from a watch taken by [`watch_set`].
/// The stream ends when the watcher returned alongside it is dropped.
pub struct SetChanges(mpsc::Receiver<()>);

/// Watch this machine's session set: the runtime tree (recursively) and the
/// session descriptors. Keep the returned watcher alive for as long as the
/// [`SetChanges`] should flow; dropping it ends [`SetChanges::stream`].
pub fn watch_set() -> io::Result<(notify::RecommendedWatcher, SetChanges)> {
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
    // The descriptors too, in the *data* dir — a separate tree. Almost everything
    // a listing reports is a write under the runtime dir, but the working
    // directory is not: a `cd` rewrites only `<data>/sessions/<name>.json`, where
    // nothing was watching. That made the one field a branched session inherits
    // the one field that arrived on the 30s heartbeat, so a new session opened in
    // the directory its sibling had left half a minute ago.
    //
    // Best-effort: a host that has never written a descriptor has no such
    // directory, and failing to watch it must not cost the listing its stream.
    let descriptors = paths::data_dir().join("sessions");
    if std::fs::create_dir_all(&descriptors).is_ok() {
        let _ = watcher.watch(&descriptors, notify::RecursiveMode::NonRecursive);
    }
    Ok((watcher, SetChanges(rx)))
}

impl SetChanges {
    /// Hand `emit` the current listing now, then again on every mutation
    /// (coalesced, and only when the listing actually changed) and on every
    /// [`HEARTBEAT`] tick. Returns once `emit` answers `false` or the watcher is
    /// dropped.
    pub fn stream(self, mut emit: impl FnMut(&[session::SessionInfo]) -> bool) {
        let rx = self.0;
        // The first listing is taken *after* the watch is registered, never
        // before. A session is only listable once its host has written its pid,
        // which lands some milliseconds after the spawn command that forked it
        // returned — so a session coming up right now becomes visible at an
        // instant we do not control. Snapshot first and that instant can fall
        // between the snapshot and the registration: the listing misses it and
        // no event is pending, so it stays unreported until the heartbeat, half a
        // minute later. This way round the change is either already in the
        // snapshot or waiting in `rx`; the worst case is a redundant wake, which
        // the `sessions != last` check below absorbs.
        let mut last = listing();
        if !emit(&last) {
            return;
        }
        loop {
            match rx.recv_timeout(HEARTBEAT) {
                // A mutation: coalesce the burst, then emit the fresh listing —
                // but only if it actually changed, so a write that doesn't alter
                // the listing (or an already-coalesced burst) costs no push.
                Ok(()) => {
                    while rx.recv_timeout(COALESCE).is_ok() {}
                    let sessions = listing();
                    if sessions != last {
                        last = sessions;
                        if !emit(&last) {
                            return;
                        }
                    }
                }
                // Keepalive: always re-emit, even unchanged — it refreshes the
                // listing and lets a pipe-backed `emit` notice a gone reader.
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    last = listing();
                    if !emit(&last) {
                        return;
                    }
                }
                // The watcher was dropped.
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

/// Parse one line of [`run`]'s output back into a session listing — the reader's
/// inverse of [`emit`], so the JSON shape stays owned here.
pub fn parse_listing(line: &str) -> serde_json::Result<Vec<session::SessionInfo>> {
    serde_json::from_str(line)
}

/// The current session listing.
fn listing() -> Vec<session::SessionInfo> {
    session::list().unwrap_or_default()
}

/// The current session listing as one line of JSON (the same shape as
/// `ghost ls --json`), without the trailing newline.
fn listing_line() -> io::Result<String> {
    serde_json::to_string(&listing()).map_err(io::Error::other)
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

    /// Building a listing READS the runtime tree (opendir/readdir, every `meta`),
    /// which raises Access events under inotify. Counting those as changes would
    /// turn one listing into a self-sustaining loop of re-listing.
    #[test]
    fn a_read_never_wakes_the_stream_but_a_mutation_does() {
        use notify::event::{AccessKind, AccessMode, CreateKind, EventKind, ModifyKind};
        assert!(!wakes(&EventKind::Access(AccessKind::Read)));
        assert!(!wakes(&EventKind::Access(AccessKind::Close(
            AccessMode::Write
        ))));
        assert!(wakes(&EventKind::Create(CreateKind::Folder)));
        assert!(wakes(&EventKind::Modify(ModifyKind::Any)));
    }

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
