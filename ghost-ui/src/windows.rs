//! Persistence of the workspace snapshot: a small TOML file in the data dir
//! (`$XDG_DATA_HOME/ghost/windows.toml`) recording the windows open at the last
//! quit, so a bare `ghost` launch can recreate them. Kept current as windows
//! change and flushed by the shutdown funnel in `main.rs`; the companion to
//! `groups.rs`, which persists the group memberships these records reference.

use ghost_ui_core::WindowRecord;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The persisted workspace: a `[sessions]` table followed by repeated
/// `[[window]]` tables.
///
/// `sessions` comes first because TOML puts every plain key before the first
/// table header, and an array-of-tables opened above it would swallow the
/// table's keys.
#[derive(Default, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Workspace {
    /// The compositor session this process's windows are registered under, per
    /// desktop. Keyed by `$XDG_CURRENT_DESKTOP` because the identifier means
    /// nothing to a different compositor — a machine that switches desktops must
    /// not hand one desktop's id to another and have its windows come back
    /// wrong, or not at all.
    ///
    /// Empty where the compositor doesn't speak `xdg_session_management_v1`,
    /// which is most of them.
    #[serde(default)]
    pub sessions: BTreeMap<String, String>,

    /// The windows open at the last quit.
    #[serde(default, rename = "window")]
    pub windows: Vec<WindowRecord>,
}

fn file_in(dir: &Path) -> PathBuf {
    dir.join("windows.toml")
}

/// Load the persisted workspace from `dir`; a missing or malformed file is just
/// "no windows" (the next save rewrites it).
fn load_from(dir: &Path) -> Workspace {
    let Ok(text) = std::fs::read_to_string(file_in(dir)) else {
        return Workspace::default();
    };
    toml::from_str::<Workspace>(&text).unwrap_or_default()
}

fn save_in(dir: &Path, workspace: &Workspace) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let text = toml::to_string_pretty(workspace).map_err(std::io::Error::other)?;
    std::fs::write(file_in(dir), text)
}

/// The workspace persisted in the data dir (empty if none was ever saved).
pub fn load() -> Workspace {
    load_from(&ghost_vt::paths::data_dir())
}

/// Persist `workspace` to the data dir; best-effort (a failure only costs
/// restore across runs, so it's logged, not fatal).
pub fn save(workspace: &Workspace) {
    if let Err(e) = save_in(&ghost_vt::paths::data_dir(), workspace) {
        eprintln!("ghost: saving workspace failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(group_id: &str, cols: u16, rows: u16, fleet: bool) -> WindowRecord {
        WindowRecord {
            group_id: group_id.into(),
            cols,
            rows,
            fleet,
            foreground: (!fleet).then(|| "alpha".into()),
            attached: vec!["alpha".into()],
        }
    }

    /// A `windows.toml` written before session ids were typed loads into typed
    /// ids and is written back byte for byte.
    #[test]
    fn a_workspace_with_a_remote_session_loads_typed_and_saves_unchanged() {
        let text = "[sessions]\n\n\
                    [[window]]\n\
                    group_id = \"w1\"\n\
                    cols = 80\n\
                    rows = 24\n\
                    fleet = false\n\
                    foreground = \"kov@box\\u001Fwork\"\n\
                    attached = [\n    \"alpha\",\n    \"kov@box\\u001Fwork\",\n]\n";
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(file_in(dir.path()), text).unwrap();

        let workspace = load_from(dir.path());
        let remote = ghost_ui_core::SessionId::remote("kov@box", "work");
        assert_eq!(workspace.windows[0].foreground, Some(remote.clone()));
        assert_eq!(
            workspace.windows[0].attached,
            vec![ghost_ui_core::SessionId::local("alpha"), remote]
        );
        save_in(dir.path(), &workspace).unwrap();
        assert_eq!(std::fs::read_to_string(file_in(dir.path())).unwrap(), text);
    }

    #[test]
    fn the_workspace_round_trips_through_the_toml_file() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace {
            windows: vec![rec("win-1", 120, 40, false), rec("win-2", 80, 24, true)],
            ..Workspace::default()
        };
        save_in(dir.path(), &workspace).unwrap();
        assert_eq!(load_from(dir.path()), workspace);
    }

    #[test]
    fn the_compositor_session_ids_round_trip_alongside_the_windows() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = Workspace {
            sessions: BTreeMap::from([
                ("GNOME".to_string(), "d3adb33f".to_string()),
                ("synoik".to_string(), "c0ffee".to_string()),
            ]),
            windows: vec![rec("win-1", 120, 40, false)],
        };
        save_in(dir.path(), &workspace).unwrap();
        assert_eq!(load_from(dir.path()), workspace);
    }

    #[test]
    fn a_workspace_written_before_session_ids_existed_still_loads() {
        // The file predates the `[sessions]` table; its windows must survive.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            file_in(dir.path()),
            "[[window]]\ngroup_id = \"win-1\"\ncols = 80\nrows = 24\nfleet = false\n",
        )
        .unwrap();
        let loaded = load_from(dir.path());
        assert_eq!(loaded.windows.len(), 1);
        assert!(loaded.sessions.is_empty());
    }

    #[test]
    fn a_missing_or_malformed_file_loads_as_no_windows() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_from(dir.path()), Workspace::default());
        std::fs::write(file_in(dir.path()), "not toml [").unwrap();
        assert_eq!(load_from(dir.path()), Workspace::default());
    }
}
