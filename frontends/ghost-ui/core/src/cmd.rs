//! `Cmd` — the effects the core returns as data. The shell is the sole
//! interpreter: it performs the I/O and, for reads, feeds the answer back as a
//! [`UiEvent`](crate::UiEvent). The core itself never touches sockets, the
//! clipboard, or the clock — which is exactly what makes its behavior assertable
//! by inspecting the returned `Vec<Cmd>`.
//!
//! Every variant is `Clone + PartialEq + Debug`, so a test asserts the precise
//! effects of an event with `assert_eq!`.

use crate::SessionId;

#[derive(Clone, Debug, PartialEq)]
pub enum Cmd {
    /// Write already-encoded bytes to a session's PTY (keys/paste/mouse/replies).
    SendInput {
        session: SessionId,
        bytes: Vec<u8>,
    },
    /// Resize a session's grid.
    Resize {
        session: SessionId,
        cols: u16,
        rows: u16,
    },
    /// Read the system clipboard; the shell replies `UiEvent::ClipboardText`.
    ReadClipboard,
    /// Write text to the system clipboard.
    WriteClipboard(String),
    /// Read the primary selection (middle-click paste); the shell replies
    /// `UiEvent::ClipboardText`. A no-op on platforms without a primary selection.
    ReadPrimary,
    /// Write text to the primary selection (set whenever text is selected).
    WritePrimary(String),
    /// Enumerate sessions; the shell replies `UiEvent::SessionList`.
    ListSessions,
    /// Open / close a session socket (e.g. for a fleet tile preview).
    Attach(SessionId),
    Detach(SessionId),
    /// Spawn a new session (take-over / new window).
    Spawn {
        name: SessionId,
        command: Vec<String>,
    },
    /// Open a new window. The shell creates it in the fleet overview (it starts
    /// owning no session); the user spawns or takes one over from there.
    NewWindow,
    /// Close the window this command came from. The shell detaches the window's
    /// sessions (they keep running) — the "close = detach" default.
    CloseWindow,
    /// Spawn a fresh session and adopt it into this window: the shell picks the
    /// name, spawns + attaches it, then replies `UiEvent::AdoptSession` so the
    /// window switches to its single view.
    SpawnSession,
    /// Take over an existing session into this window: the shell attaches it
    /// (stealing the display if another window held it) and replies
    /// `UiEvent::AdoptSession` so the window switches to its single view.
    TakeOver(SessionId),
    /// Repaint the window.
    Redraw,
    /// Set the window title.
    SetTitle(String),
    /// Ask for a future `UiEvent::Tick` after the given delay.
    ScheduleTick {
        after_ms: u64,
    },
    /// Exit the application.
    Quit,
}
