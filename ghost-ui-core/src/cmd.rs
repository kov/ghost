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
    /// Open a read-only observation of a session — a live fleet preview. The
    /// shell replies with `UiEvent::SessionPush`es (grid, state) and mirrored
    /// output as `UiEvent::SessionData`; it never resizes or steals the
    /// session.
    Observe(SessionId),
    /// Close a session's observation (its tile is gone, driven by this
    /// window now, or the fleet closed).
    Unobserve(SessionId),
    /// Kill a session and its process (the shell sends `ClientMsg::Kill`).
    Kill(SessionId),
    /// Rename a session (the shell sends `ClientMsg::Rename`).
    Rename {
        session: SessionId,
        name: String,
    },
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
    /// Upload a kitty-graphics image's pixels to the renderer, out of band and
    /// keyed by `id` (the pixels never travel through the `Scene`/`Frame`, which
    /// stay cheap to clone and compare). Sent once per image, before the `Redraw`
    /// that first draws it; the renderer caches it by `id`.
    UploadImage {
        id: u32,
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    /// Open a hyperlink (OSC 8, Ctrl+click) in the system handler. The URL's
    /// scheme has already been allowlisted by the model.
    OpenUrl(String),
    /// Set the window's pointer shape (hand over a Ctrl-hovered hyperlink).
    PointerIcon(PointerIcon),
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

/// The pointer shape a [`Cmd::PointerIcon`] asks the window to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerIcon {
    /// The platform's normal arrow / text cursor.
    Default,
    /// The link-hover hand.
    Pointer,
}
