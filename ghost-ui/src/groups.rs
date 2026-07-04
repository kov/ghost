//! Persistence of the session-group registry: a small TOML file in the data
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
/// "no groups" (the next save rewrites it). Records predating durable ids
/// (the manual-groups era) get distinct ids backfilled — no window claims
/// them, so they behave as closed groups.
fn load_from(dir: &Path) -> Vec<Group> {
    let Ok(text) = std::fs::read_to_string(file_in(dir)) else {
        return Vec::new();
    };
    let mut groups = toml::from_str::<GroupsFile>(&text)
        .map(|f| f.group)
        .unwrap_or_default();
    // A memberless group remembers nothing — prune it rather than render an
    // empty closed block forever.
    groups.retain(|g| !g.members.is_empty());
    for (i, g) in groups.iter_mut().enumerate() {
        if g.id.is_empty() {
            g.id = format!("legacy-{i}");
        }
    }
    groups
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
            // An ssh group: its connection must survive the nested TOML table.
            Group {
                id: "w1".into(),
                name: "web".into(),
                color: 0,
                members: vec!["alpha".into(), "beta".into()],
                connection: ghost_vt::connection::ConnectionSpec::parse_target("kov@box"),
            },
            Group {
                id: "w2".into(),
                name: "infra".into(),
                color: 3,
                members: vec!["gamma".into()],
                connection: None,
            },
        ];
        save_in(dir.path(), &groups).unwrap();
        assert_eq!(load_from(dir.path()), groups);
    }

    #[test]
    fn a_group_without_a_connection_loads_as_local() {
        // Existing group files predate the connection field: they must parse,
        // with the group defaulting to a plain local group.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            file_in(dir.path()),
            "[[group]]\nid = \"w1\"\nname = \"blue\"\ncolor = 0\nmembers = [\"alpha\"]\n",
        )
        .unwrap();
        let loaded = load_from(dir.path());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].connection, None);
    }

    #[test]
    fn a_missing_or_malformed_file_loads_as_no_groups() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_from(dir.path()), Vec::new());
        std::fs::write(file_in(dir.path()), "not toml [").unwrap();
        assert_eq!(load_from(dir.path()), Vec::new());
    }

    #[test]
    fn memberless_groups_are_pruned_at_load() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            file_in(dir.path()),
            "[[group]]\nid = \"w9\"\nname = \"blue\"\ncolor = 0\nmembers = []\n\n\
             [[group]]\nid = \"w2\"\nname = \"green\"\ncolor = 1\nmembers = [\"alpha\"]\n",
        )
        .unwrap();
        let loaded = load_from(dir.path());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "w2");
    }

    #[test]
    fn a_file_predating_group_ids_loads_with_backfilled_ids() {
        // Files written before groups carried ids (the manual-groups era)
        // get distinct ids backfilled: no window ever claims them, so they
        // behave as closed groups, but id-keyed lookups must not collide.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            file_in(dir.path()),
            "[[group]]\nname = \"web\"\ncolor = 1\nmembers = [\"alpha\"]\n\n\
             [[group]]\nname = \"infra\"\ncolor = 2\nmembers = [\"beta\"]\n",
        )
        .unwrap();
        let loaded = load_from(dir.path());
        assert_eq!(loaded.len(), 2);
        assert!(loaded.iter().all(|g| !g.id.is_empty()));
        assert_ne!(loaded[0].id, loaded[1].id);
        assert_eq!(loaded[0].name, "web");
        assert_eq!(loaded[0].members, vec!["alpha".to_string()]);
    }
}
