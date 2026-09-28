//! Persistence of the session groups' attributes — name, color, connection — in a
//! small TOML file in the data dir (`$XDG_DATA_HOME/ghost/groups.toml`), loaded
//! once at startup and rewritten whole on every change. Membership is not kept
//! here: each session's host keeps its group. A file from before that still
//! lists members, and they load, for the one-time migration to the hosts.

use ghost_ui_core::Group;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The file's shape: repeated `[[group]]` tables.
#[derive(Default, Serialize, Deserialize)]
struct GroupsFile {
    #[serde(default)]
    group: Vec<Record>,
}

/// One group in the file: [`Group`] without its members, which are only read (from
/// a file predating host-kept membership), never written.
#[derive(Serialize, Deserialize)]
struct Record {
    #[serde(default)]
    id: ghost_ui_core::group::GroupId,
    name: String,
    color: u8,
    #[serde(default, skip_serializing)]
    members: Vec<ghost_ui_core::SessionId>,
    /// Last, so TOML emits its nested table after the scalar fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    connection: Option<ghost_vt::connection::ConnectionSpec>,
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
    let mut groups: Vec<Group> = toml::from_str::<GroupsFile>(&text)
        .map(|f| f.group)
        .unwrap_or_default()
        .into_iter()
        .map(|r| Group {
            id: r.id,
            name: r.name,
            color: r.color,
            members: r.members,
            connection: r.connection,
        })
        .collect();
    for (i, g) in groups.iter_mut().enumerate() {
        if g.id.is_empty() {
            g.id = format!("legacy-{i}");
        }
    }
    groups
}

fn save_in(dir: &Path, groups: &[Group]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let group = groups
        .iter()
        .map(|g| Record {
            id: g.id.clone(),
            name: g.name.clone(),
            color: g.color,
            members: Vec::new(),
            connection: g.connection.clone(),
        })
        .collect();
    let text = toml::to_string_pretty(&GroupsFile { group }).map_err(std::io::Error::other)?;
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

    /// A `groups.toml` from before membership moved to the hosts — a remote member
    /// stored as its `<target>␟<name>` composite — loads into typed ids, for the
    /// one-time migration that tells each member's host its group.
    #[test]
    fn a_groups_file_with_a_remote_member_loads_typed() {
        let text = "[[group]]\n\
                    id = \"w1\"\n\
                    name = \"blue\"\n\
                    color = 0\n\
                    members = [\n    \"alpha\",\n    \"kov@box\\u001Fwork\",\n]\n";
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(file_in(dir.path()), text).unwrap();

        let groups = load_from(dir.path());
        assert_eq!(
            groups[0].members,
            vec![
                ghost_ui_core::SessionId::local("alpha"),
                ghost_ui_core::SessionId::remote("kov@box", "work"),
            ]
        );
    }

    #[test]
    fn group_attributes_round_trip_through_the_toml_file_without_members() {
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
        let text = std::fs::read_to_string(file_in(dir.path())).unwrap();
        assert!(
            !text.contains("members"),
            "membership lives on the hosts, not in the file:\n{text}"
        );
        let attributes: Vec<Group> = groups
            .into_iter()
            .map(|g| Group {
                members: Vec::new(),
                ..g
            })
            .collect();
        assert_eq!(load_from(dir.path()), attributes);
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
    fn an_attributes_only_file_loads_every_group() {
        // A group's members come from its sessions' hosts, so a group the file
        // names without members is not stale: its attributes are what it keeps.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            file_in(dir.path()),
            "[[group]]\nid = \"w9\"\nname = \"blue\"\ncolor = 0\n\n\
             [[group]]\nid = \"w2\"\nname = \"green\"\ncolor = 1\n",
        )
        .unwrap();
        let loaded = load_from(dir.path());
        let ids: Vec<&str> = loaded.iter().map(|g| g.id.as_str()).collect();
        assert_eq!(ids, vec!["w9", "w2"]);
        assert!(loaded.iter().all(|g| g.members.is_empty()));
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
        assert_eq!(
            loaded[0].members,
            vec![ghost_ui_core::SessionId::local("alpha")]
        );
    }
}
