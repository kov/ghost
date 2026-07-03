//! Session groups: color-coded collections treated as a unit in the fleet.
//! A group is born automatically for each window and means "the sessions
//! attached to this window"; it is persisted by the shell in the data dir so
//! it survives the window closing. Membership is maintained by the models as
//! sessions attach and detach, not curated by hand.

use crate::SessionId;
use serde::{Deserialize, Serialize};

/// A group's durable identity, minted by the shell when its window is
/// created. Empty on records predating ids (the manual-groups era), which no
/// window ever claims.
pub type GroupId = String;

/// Accent colors assigned to groups round-robin at creation, referenced by
/// index so a future restyle recolors existing groups.
pub const GROUP_PALETTE: [[f32; 4]; 6] = [
    [0.36, 0.65, 0.95, 1.0], // blue
    [0.55, 0.80, 0.45, 1.0], // green
    [0.90, 0.60, 0.30, 1.0], // orange
    [0.75, 0.55, 0.90, 1.0], // purple
    [0.90, 0.45, 0.55, 1.0], // rose
    [0.45, 0.80, 0.80, 1.0], // teal
];

/// The palette colors' names, matching [`GROUP_PALETTE`] index for index: an
/// automatic group is born carrying its color's name until the user renames
/// it.
pub const GROUP_COLOR_NAMES: [&str; 6] = ["blue", "green", "orange", "purple", "rose", "teal"];

/// A color-coded collection of sessions: one window's attached set, live or
/// remembered (see the module docs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    /// Durable identity binding the group to the window carrying it (see
    /// [`GroupId`]).
    #[serde(default)]
    pub id: GroupId,
    pub name: String,
    /// Index into [`GROUP_PALETTE`] (wrapped at use).
    pub color: u8,
    /// Member session ids (immutable spawn-time names). The set is what
    /// matters; display order is the fleet's stable spatial order.
    pub members: Vec<SessionId>,
}

/// The window-group id embedded in a display client's self-reported identity
/// (`ghost-ui:<group-id>`, sent in the attach hello), if it carries one.
/// Identities from other kinds of clients — or from ghost-ui builds
/// predating window groups, whose suffix is a bare pid — simply name no
/// known group and bucket as generic "attached elsewhere".
pub fn holder_group(client: &str) -> Option<GroupId> {
    client.strip_prefix("ghost-ui:").map(str::to_string)
}

/// A window's identity string for the attach hello, embedding its group id
/// so other windows' fleets can bucket the session under its block.
pub fn window_identity(group_id: &str) -> String {
    format!("ghost-ui:{group_id}")
}

impl Group {
    /// A window's newborn group: no members yet, named after its color.
    pub fn auto(id: GroupId, color: u8) -> Self {
        Group {
            id,
            name: GROUP_COLOR_NAMES[color as usize % GROUP_COLOR_NAMES.len()].to_string(),
            color,
            members: Vec::new(),
        }
    }

    /// The group's accent color.
    pub fn rgba(&self) -> [f32; 4] {
        GROUP_PALETTE[self.color as usize % GROUP_PALETTE.len()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_identity_round_trips_its_group_id() {
        let id = window_identity("win-4321-2");
        assert_eq!(holder_group(&id), Some("win-4321-2".to_string()));
        // Foreign identities name no group.
        assert_eq!(holder_group("weird-client"), None);
    }

    #[test]
    fn an_automatic_group_is_named_after_its_color() {
        let g = Group::auto("win-1".into(), 2);
        assert_eq!(g.id, "win-1");
        assert_eq!(g.name, "orange");
        assert_eq!(g.rgba(), GROUP_PALETTE[2]);
        assert!(g.members.is_empty());
        // The color index wraps like rgba() does.
        assert_eq!(Group::auto("win-2".into(), 7).name, "green");
    }
}
