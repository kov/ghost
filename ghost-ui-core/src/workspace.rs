//! The workspace snapshot: the set of windows open at the last quit, captured
//! so a bare `ghost` launch can recreate them. Each window is remembered by its
//! group identity (so the reopened window reclaims the same group rather than
//! forking a fresh one), the grid it was sized to, its view mode, and the
//! sessions it drove. The shell persists this alongside the group registry (see
//! `ghost-ui/src/windows.rs`) and keeps it current as windows change.

use crate::SessionId;
use crate::group::GroupId;
use serde::{Deserialize, Serialize};

/// One window's restorable state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowRecord {
    /// The window's group identity, reclaimed on restore so membership and the
    /// fleet's cross-window bucketing continue rather than forking a new group.
    pub group_id: GroupId,
    /// The terminal grid the window was sized to; a restored window opens to fit
    /// it (the same path a fresh window uses for its configured columns/rows).
    pub cols: u16,
    pub rows: u16,
    /// Whether the window was showing the fleet overview rather than a single
    /// terminal.
    pub fleet: bool,
    /// The session shown in the single view, restored as the foreground.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground: Option<SessionId>,
    /// The sessions this window drove (its attached set). On restore the live
    /// ones are reattached and the dead ones relaunched; other members of the
    /// group stay cold in its block.
    #[serde(default)]
    pub attached: Vec<SessionId>,
}

/// The windows open at the last quit, in a stable order. (Round-trips through
/// the TOML file the shell writes; that path is tested in `ghost-ui`'s
/// `windows.rs`, which owns the serialization format.)
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    pub windows: Vec<WindowRecord>,
}
