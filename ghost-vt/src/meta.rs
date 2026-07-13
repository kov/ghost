//! Per-session metadata for discovery.
//!
//! The host writes `<session>/meta` (JSON) with the session's creation time,
//! command, and current terminal title; [`crate::session::list`] reads it so a
//! GUI can identify sessions it isn't attached to. It's intentionally *not* the
//! liveness signal (that's the lock file) — just descriptive data, safe to be
//! missing or briefly stale.

use serde::{Deserialize, Serialize};
use std::io;
use std::path::Path;

/// Descriptive metadata for one session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    /// Unix milliseconds at which the session was created. The fleet's spatial
    /// sort key, so the resolution is sub-second to order same-second sessions.
    pub created_at: i64,
    /// The command the session runs (empty means the user's `$SHELL`).
    pub command: Vec<String>,
    /// The current terminal title (OSC 0/2), empty if none has been set.
    pub title: String,
    /// The user-chosen display name (`ghost rename`), empty if never renamed.
    /// Purely a label: the session's *identity* — its directory, socket, and
    /// recording — is the immutable spawn-time name, so renaming moves no files
    /// and never disturbs attached clients. `default` keeps metadata written
    /// before this field existed parseable.
    #[serde(default)]
    pub display_name: String,
    /// The session's terminal grid `(cols, rows)`, refreshed whenever a display
    /// client resizes it. Discovery hands it to the fleet so a never-observed
    /// session's tile is born with its real aspect — the grid must not reshuffle
    /// when the observer's first snapshot lands. `(0, 0)` (the pre-field
    /// default) means unrecorded.
    #[serde(default)]
    pub size: (u16, u16),
    /// This session's remote connection, if it is an ssh/mosh session (see
    /// [`crate::connection`]). Carried here so the spawn copies it into the
    /// durable descriptor and discovery (`session::list`) can surface it; `None`
    /// for a local session, defaulted so pre-connection metadata still parses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<crate::connection::ConnectionSpec>,
    /// What a program on this session's tty may change about the terminal (see
    /// [`ghost_term::policy`]) — the policy the last terminal to attach reported.
    ///
    /// Kept here because a session outlives every terminal that shows it: detached,
    /// there is nobody to ask, so the host goes on enforcing what it was last told,
    /// and a recreate or a resurrect gets it back rather than silently reverting to
    /// permissive. Defaulted, so metadata written before the field existed still
    /// parses — as the old behavior, which is what those sessions were running.
    #[serde(default)]
    pub policy: ghost_term::TerminalPolicy,
}

/// Write `meta` to `path` atomically (write a sibling temp file, then rename),
/// so a concurrent [`read`] never sees a half-written file.
pub fn write(path: &Path, meta: &Meta) -> io::Result<()> {
    let json = serde_json::to_vec(meta).expect("Meta serializes cleanly");
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, path)
}

/// Read a session's metadata, or `None` if it's absent or unreadable.
pub fn read(path: &Path) -> Option<Meta> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("meta");
        let meta = Meta {
            created_at: 1_700_000_000_000,
            command: vec!["vim".into(), "main.rs".into()],
            title: "vim · main.rs".into(),
            display_name: "build box".into(),
            size: (120, 60),
            connection: None,
            policy: ghost_term::TerminalPolicy::default(),
        };
        write(&path, &meta).unwrap();
        assert_eq!(read(&path), Some(meta));
    }

    #[test]
    fn meta_without_a_display_name_still_reads() {
        // Metadata written before display names existed must keep parsing, with
        // the display name defaulting to unset (the session shows its id).
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("meta");
        std::fs::write(&path, br#"{"created_at":1,"command":["sh"],"title":"t"}"#).unwrap();
        let meta = read(&path).expect("legacy meta parses");
        assert_eq!(meta.display_name, "");
        assert_eq!(meta.size, (0, 0), "pre-size metadata reads as unknown");
        assert_eq!(
            meta.policy,
            ghost_term::TerminalPolicy::allow_all(),
            "a session from before the policy existed keeps the behavior it had"
        );
    }

    #[test]
    fn read_missing_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read(&tmp.path().join("absent")), None);
    }
}
