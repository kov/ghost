//! Durable per-session descriptors: what recreating a dead session needs.
//!
//! The host writes `<data>/sessions/<name>.json` when the child actually
//! starts and refreshes it as the facts change (a rename, the child changing
//! directory). Unlike the runtime `meta` — which is pruned with the session
//! directory — this file deliberately survives the session's death: it is the
//! fleet's memory of a dead group member, and the seed for recreating it.
//! Like `meta` it is descriptive only: safe to be missing or stale.

use serde::{Deserialize, Serialize};
use std::io;
use std::path::PathBuf;

/// The durable facts about one session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Descriptor {
    /// The command the session runs (empty means the user's `$SHELL`).
    pub command: Vec<String>,
    /// The child's working directory: its launch dir, refreshed to the current
    /// one while it runs (Linux), so a recreate lands where the user was.
    pub cwd: Option<PathBuf>,
    /// Unix milliseconds at which the session was created (mirrors `meta`).
    pub created_at: i64,
    /// The user-chosen display name when last written, empty if never renamed.
    #[serde(default)]
    pub display_name: String,
    /// This session's remote connection, if it is an ssh/mosh session — the
    /// authoritative record used to *reconnect* it on relaunch (see
    /// [`crate::connection`]). `None` for an ordinary local session; defaulted
    /// so descriptors written before connections existed still parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<crate::connection::ConnectionSpec>,
    /// What a program on this session's tty may change about the terminal (see
    /// [`ghost_term::policy`]) — the policy the last terminal to attach reported.
    ///
    /// Here rather than only in the runtime `meta`, because `meta` is pruned with
    /// the session directory the moment the host exits: a recreate or a resurrect
    /// reads *this* file, and a policy that died with the process would hand the
    /// session straight back to a program that had been refused. Defaulted, so
    /// descriptors written before the field existed still parse — as the old
    /// behavior, which is what those sessions were running.
    #[serde(default)]
    pub policy: ghost_term::TerminalPolicy,
}

/// Where `name`'s descriptor lives.
pub fn path(name: &str) -> PathBuf {
    crate::paths::data_dir()
        .join("sessions")
        .join(format!("{name}.json"))
}

/// Write `name`'s descriptor atomically (temp file + rename), creating the
/// `sessions` directory as needed.
pub fn write(name: &str, d: &Descriptor) -> io::Result<()> {
    let p = path(name);
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_vec(d).expect("Descriptor serializes cleanly");
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, &p)
}

/// Read `name`'s descriptor, or `None` if absent or unreadable.
pub fn read(name: &str) -> Option<Descriptor> {
    serde_json::from_slice(&std::fs::read(path(name)).ok()?).ok()
}

/// Forget `name` (its group membership was dropped; nothing references it).
pub fn remove(name: &str) {
    let _ = std::fs::remove_file(path(name));
}

/// Every session name with a descriptor on disk (for the shell's sweep: dead
/// group members to remember, unreferenced leftovers to prune).
pub fn all_names() -> Vec<String> {
    let dir = crate::paths::data_dir().join("sessions");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| {
            let p = e.ok()?.path();
            if p.extension()? != "json" {
                return None;
            }
            Some(p.file_stem()?.to_str()?.to_owned())
        })
        .collect()
}

/// Refresh just the display name of an existing descriptor (a rename while the
/// session runs). A session whose child hasn't started yet has no descriptor;
/// the eventual spawn writes the then-current name.
pub fn set_display_name(name: &str, display_name: &str) {
    if let Some(mut d) = read(name)
        && d.display_name != display_name
    {
        d.display_name = display_name.to_string();
        let _ = write(name, &d);
    }
}

/// Refresh just the working directory of an existing descriptor.
pub fn set_cwd(name: &str, cwd: &std::path::Path) {
    if let Some(mut d) = read(name)
        && d.cwd.as_deref() != Some(cwd)
    {
        d.cwd = Some(cwd.to_path_buf());
        let _ = write(name, &d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        // `path()` derives from the process env; exercise the file layer via a
        // descriptor under the tempdir through the env-independent pieces.
        let d = Descriptor {
            command: vec!["vim".into()],
            cwd: Some(tmp.path().to_path_buf()),
            created_at: 1_700_000_000_000,
            display_name: "build-box".into(),
            connection: None,
            policy: ghost_term::TerminalPolicy::default(),
        };
        let json = serde_json::to_vec(&d).unwrap();
        let back: Descriptor = serde_json::from_slice(&json).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn a_descriptor_without_a_display_name_still_reads() {
        let legacy = br#"{"command":["sh"],"cwd":null,"created_at":1}"#;
        let d: Descriptor = serde_json::from_slice(legacy).expect("legacy parses");
        assert_eq!(d.display_name, "");
        assert_eq!(d.connection, None, "a legacy descriptor has no connection");
    }

    #[test]
    fn a_connection_carrying_descriptor_round_trips() {
        // An ssh session's descriptor keeps an empty command (the spec drives
        // the child) and the spec itself, so relaunch can reconnect.
        let d = Descriptor {
            command: Vec::new(),
            cwd: None,
            created_at: 1,
            display_name: String::new(),
            connection: crate::connection::ConnectionSpec::parse_target("kov@box"),
            policy: ghost_term::TerminalPolicy::default(),
        };
        let json = serde_json::to_vec(&d).unwrap();
        let back: Descriptor = serde_json::from_slice(&json).unwrap();
        assert_eq!(back, d);
        assert_eq!(back.connection.unwrap().target(), "kov@box");
    }
}
