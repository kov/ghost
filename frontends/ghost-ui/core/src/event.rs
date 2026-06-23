//! `UiEvent` — the core's own input alphabet. The shell translates real OS
//! input (winit) and the effects of its own commands (clipboard reads, session
//! enumeration, pumped socket output, clock ticks) into these, and feeds them to
//! `update`. Pixel positions arrive here; the core converts to cells itself
//! against the layout it produced, so hit-testing stays pure and testable.

use crate::SessionId;
use crate::input::{Key, Mods};
use ghost_vt::session::SessionInfo;

/// A pointer position in physical pixels (origin top-left).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointPx {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerButton {
    Left,
    Middle,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerPhase {
    Press,
    Release,
    Motion,
    Wheel,
}

/// Everything that can drive the UI core. Input from the user, plus the replies
/// to the core's own read-requests (see [`Cmd`](crate::Cmd)) and the injected
/// clock — so `update` never touches the world directly.
#[derive(Clone, Debug)]
pub enum UiEvent {
    /// A key transition. `pressed` is false on release.
    Key {
        key: Key,
        mods: Mods,
        pressed: bool,
    },
    /// Committed text (IME commit, or text the shell pasted in).
    Text(String),
    /// In-progress IME composition (the preedit string); empty ends/cancels it.
    /// While a non-empty preedit is active the terminal suppresses raw key input
    /// so the keystrokes driving composition aren't also sent to the child.
    Preedit(String),
    /// Set the absolute font zoom (e.g. from persisted config); the model clamps
    /// it to its bounds and re-grids. Relative steps come via `Key` shortcuts.
    SetZoom(f32),
    Pointer {
        phase: PointerPhase,
        button: Option<PointerButton>,
        pos: PointPx,
        mods: Mods,
        wheel_dy: f64,
        /// Click count for a `Press` (1 = single, 2 = double, 3 = triple); 1 for
        /// other phases. Drives word/line selection.
        clicks: u8,
    },
    Focus(bool),
    Resize {
        w_px: u32,
        h_px: u32,
        scale: f64,
    },
    /// Reply to `Cmd::ReadClipboard` (None if the clipboard was empty/unreadable).
    ClipboardText(Option<String>),
    /// Output the shell pumped off a session socket.
    SessionData {
        name: SessionId,
        bytes: Vec<u8>,
        ended: bool,
    },
    /// Reply to `Cmd::ListSessions`.
    SessionList(Vec<SessionInfo>),
    /// Injected monotonic clock pulse, milliseconds since the shell started.
    /// The sole time source — the core never reads a wall-clock.
    Tick {
        now_ms: u64,
    },
}
