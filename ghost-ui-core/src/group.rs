//! User-defined session groups: named, color-coded collections treated as a
//! unit in the fleet. A group is fleet-side state (persisted by the shell in
//! the data dir), orthogonal to the attach-state sections: grouped sessions
//! render together in their group's block regardless of who drives them.

use crate::SessionId;
use serde::{Deserialize, Serialize};

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

/// A named, color-coded collection of sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    pub name: String,
    /// Index into [`GROUP_PALETTE`] (wrapped at use).
    pub color: u8,
    /// Member session ids (immutable spawn-time names), in display order.
    pub members: Vec<SessionId>,
}

impl Group {
    /// The group's accent color.
    pub fn rgba(&self) -> [f32; 4] {
        GROUP_PALETTE[self.color as usize % GROUP_PALETTE.len()]
    }
}
