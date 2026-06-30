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
        };
        write(&path, &meta).unwrap();
        assert_eq!(read(&path), Some(meta));
    }

    #[test]
    fn read_missing_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read(&tmp.path().join("absent")), None);
    }
}
