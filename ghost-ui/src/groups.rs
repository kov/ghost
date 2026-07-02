//! Persistence of user-defined session groups: a small TOML file in the data
//! dir (`$XDG_DATA_HOME/ghost/groups.toml`), loaded once at startup and
//! rewritten whole on every `Cmd::SaveGroups`.

use ghost_ui_core::Group;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The file's shape: repeated `[[group]]` tables.
#[derive(Default, Serialize, Deserialize)]
struct GroupsFile {
    #[serde(default)]
    group: Vec<Group>,
}

fn file_in(dir: &Path) -> PathBuf {
    dir.join("groups.toml")
}

/// Load the persisted groups from `dir`; a missing or malformed file is just
/// "no groups" (the next save rewrites it).
fn load_from(dir: &Path) -> Vec<Group> {
    let Ok(text) = std::fs::read_to_string(file_in(dir)) else {
        return Vec::new();
    };
    toml::from_str::<GroupsFile>(&text)
        .map(|f| f.group)
        .unwrap_or_default()
}

fn save_in(dir: &Path, groups: &[Group]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let text = toml::to_string_pretty(&GroupsFile {
        group: groups.to_vec(),
    })
    .map_err(std::io::Error::other)?;
    std::fs::write(file_in(dir), text)
}

/// The groups persisted in the data dir (empty if none were ever saved).
pub fn load() -> Vec<Group> {
    load_from(&ghost_vt::paths::data_dir())
}

/// Persist `groups` to the data dir; best-effort (a failure only costs
/// persistence across runs, so it's logged, not fatal).
pub fn save(groups: &[Group]) {
    if let Err(e) = save_in(&ghost_vt::paths::data_dir(), groups) {
        eprintln!("ghost: saving groups failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_round_trip_through_the_toml_file() {
        let dir = tempfile::tempdir().unwrap();
        let groups = vec![
            Group {
                name: "web".into(),
                color: 0,
                members: vec!["alpha".into(), "beta".into()],
            },
            Group {
                name: "infra".into(),
                color: 3,
                members: vec!["gamma".into()],
            },
        ];
        save_in(dir.path(), &groups).unwrap();
        assert_eq!(load_from(dir.path()), groups);
    }

    #[test]
    fn a_missing_or_malformed_file_loads_as_no_groups() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_from(dir.path()), Vec::new());
        std::fs::write(file_in(dir.path()), "not toml [").unwrap();
        assert_eq!(load_from(dir.path()), Vec::new());
    }
}
