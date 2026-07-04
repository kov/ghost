//! Persistence of the workspace snapshot: a small TOML file in the data dir
//! (`$XDG_DATA_HOME/ghost/windows.toml`) recording the windows open at the last
//! quit, so a bare `ghost` launch can recreate them. Kept current as windows
//! change and flushed by the shutdown funnel in `main.rs`; the companion to
//! `groups.rs`, which persists the group memberships these records reference.

use ghost_ui_core::WindowRecord;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The file's shape: repeated `[[window]]` tables.
#[derive(Default, Serialize, Deserialize)]
struct WindowsFile {
    #[serde(default)]
    window: Vec<WindowRecord>,
}

fn file_in(dir: &Path) -> PathBuf {
    dir.join("windows.toml")
}

/// Load the persisted workspace from `dir`; a missing or malformed file is just
/// "no windows" (the next save rewrites it).
fn load_from(dir: &Path) -> Vec<WindowRecord> {
    let Ok(text) = std::fs::read_to_string(file_in(dir)) else {
        return Vec::new();
    };
    toml::from_str::<WindowsFile>(&text)
        .map(|f| f.window)
        .unwrap_or_default()
}

fn save_in(dir: &Path, windows: &[WindowRecord]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let text = toml::to_string_pretty(&WindowsFile {
        window: windows.to_vec(),
    })
    .map_err(std::io::Error::other)?;
    std::fs::write(file_in(dir), text)
}

/// The workspace persisted in the data dir (empty if none was ever saved).
pub fn load() -> Vec<WindowRecord> {
    load_from(&ghost_vt::paths::data_dir())
}

/// Persist `windows` to the data dir; best-effort (a failure only costs restore
/// across runs, so it's logged, not fatal).
pub fn save(windows: &[WindowRecord]) {
    if let Err(e) = save_in(&ghost_vt::paths::data_dir(), windows) {
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
            foreground: (!fleet).then(|| "alpha".to_string()),
            attached: vec!["alpha".into()],
        }
    }

    #[test]
    fn the_workspace_round_trips_through_the_toml_file() {
        let dir = tempfile::tempdir().unwrap();
        let windows = vec![rec("win-1", 120, 40, false), rec("win-2", 80, 24, true)];
        save_in(dir.path(), &windows).unwrap();
        assert_eq!(load_from(dir.path()), windows);
    }

    #[test]
    fn a_missing_or_malformed_file_loads_as_no_windows() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_from(dir.path()), Vec::new());
        std::fs::write(file_in(dir.path()), "not toml [").unwrap();
        assert_eq!(load_from(dir.path()), Vec::new());
    }
}
