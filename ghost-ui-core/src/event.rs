//! `UiEvent` — the core's own input alphabet. The shell translates real OS
//! input (winit) and the effects of its own commands (clipboard reads, session
//! enumeration, pumped socket output, clock ticks) into these, and feeds them to
//! `update`. Pixel positions arrive here; the core converts to cells itself
//! against the layout it produced, so hit-testing stays pure and testable.

use crate::SessionId;
use crate::input::{Key, KeyAlternates, KeyEventKind, Mods};
use ghost_vt::protocol::{SessionEvent, SessionState};
use ghost_vt::session::SessionInfo;

/// A dead-but-remembered session, read back from its durable descriptor (see
/// the host's `ghost_vt::descriptor`): the identity and metadata a dead tile
/// shows, and the key a recreate is issued under.
#[derive(Clone, Debug, PartialEq)]
pub struct DeadSession {
    pub name: SessionId,
    /// The display name it had, empty if never renamed.
    pub display_name: String,
    /// The command it ran (empty means the user's `$SHELL`).
    pub command: Vec<String>,
    /// Its last known working directory (display form, `~`-abbreviated).
    pub cwd: Option<String>,
}

/// What a session subscription pushed: the one starting snapshot, or a delta
/// event ([`ghost_vt::client::Subscriber`]). The shell subscribes to every
/// session whose host serves it and fans the pushes out to each window.
#[derive(Clone, Debug)]
pub enum SessionPush {
    Snapshot(SessionState),
    Event(SessionEvent),
}

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
    /// A key transition: a press, an auto-repeat, or a release. `alts` carries the
    /// platform's alternate codepoints (for the kitty report-alternate-keys flag);
    /// it is `None` when unavailable or for non-text keys.
    Key {
        key: Key,
        mods: Mods,
        kind: KeyEventKind,
        alts: Option<KeyAlternates>,
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
    /// A driven session's transport dropped without the child exiting — a lost
    /// connection whose session may still be alive on the far side (a remote
    /// session over ssh). The tile enters a *reconnecting* hold (frozen, dimmed)
    /// instead of tearing down; the shell retries the attach and, on success,
    /// sends [`SessionReattached`](UiEvent::SessionReattached).
    SessionDisconnected {
        name: SessionId,
    },
    /// A reconnecting session's transport is back (the shell re-attached and the
    /// host is resyncing its screen): clear the reconnecting hold.
    SessionReattached {
        name: SessionId,
    },
    /// Reply to `Cmd::ListSessions`.
    SessionList(Vec<SessionInfo>),
    /// A state push from `name`'s subscription. State reaches the fleet the
    /// moment it changes; the periodic `SessionList` remains only as set
    /// discovery and a slow backstop.
    SessionPush {
        name: SessionId,
        push: SessionPush,
    },
    /// The session *set* may have changed (a session directory appeared or
    /// vanished, or a subscription ended): re-enumerate now rather than waiting
    /// for the floor tick.
    SessionsChanged,
    /// The authoritative group registry: loaded from disk at startup, or
    /// re-broadcast when another window saved a change.
    GroupsLoaded(Vec<crate::group::Group>),
    /// The dead-but-remembered sessions (group members with a durable
    /// descriptor but no live host), refreshed alongside every session list.
    /// The fleet keeps them as dead tiles offering a recreate.
    DeadSessions(Vec<DeadSession>),
    /// The shell has attached `SessionId` for this window (reply to
    /// `Cmd::SpawnSession` / `Cmd::TakeOver`): switch to its single view and
    /// take ownership. The window adopts the fleet tile's screen if it has one.
    AdoptSession(SessionId),
    /// Injected monotonic clock pulse, milliseconds since the shell started.
    /// The sole time source — the core never reads a wall-clock.
    Tick {
        now_ms: u64,
    },
}
