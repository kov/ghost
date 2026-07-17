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
    /// Ask the window manager to resize *the window* to this inner size (physical
    /// px) — a program resized the grid from within its output (DECCOLM 80↔132),
    /// and the window has to follow the grid rather than the other way round. The
    /// request may be clamped or refused; whatever size the window ends up
    /// reporting comes back as a `UiEvent::Resize` and wins.
    ResizeWindow {
        w_px: u32,
        h_px: u32,
    },
    /// Iconify (minimize) the window, or restore it — a program asked, with
    /// XTWINOPS `CSI 2 t` / `CSI 1 t`. As with [`Cmd::ResizeWindow`], the window
    /// manager may ignore it.
    SetIconified(bool),
    /// Maximize the window, or restore it (XTWINOPS `CSI 9 ; 1 t` / `9 ; 0 t`).
    /// The single-axis forms (`9 ; 2`, `9 ; 3`) don't come through here — no
    /// platform maximizes one axis, so the model just re-grids and the window
    /// follows through [`Cmd::ResizeWindow`].
    SetMaximized(bool),
    /// Take the window full-screen, or leave (XTWINOPS `CSI 10 ; Ps t`).
    SetFullscreen(bool),
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
    /// Bring a dead-but-remembered session back: the shell respawns it under
    /// the same name from its durable descriptor (command, cwd), seeds it
    /// from its recording, then attaches and replies `UiEvent::AdoptSession`.
    Recreate(SessionId),
    /// The background half of a group relaunch: respawn a dead session like
    /// `Recreate`, but attach nothing — its child command starts only when a
    /// display client first attaches, and its tile revives when the next
    /// listing shows the session alive (claimed on success, never
    /// optimistically). A failed spawn just leaves the tile dead.
    Resurrect(SessionId),
    /// Restart a *remote* session's host under the current binary, keeping its
    /// screen (`r` on a live remote tile, confirmed): the shell runs `ghost
    /// __restart` on the host over the transport, which ends the (possibly older)
    /// host and respawns the session seeded from its recording — bringing a session
    /// served by an older host up to the current protocol level. The running
    /// program on the host is lost; the screen and scrollback survive.
    RestartRemote(SessionId),
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
    /// Open a new window that starts in the "connect to a host" prompt
    /// (Cmd+S / Ctrl+Shift+S). The shell opens a sessionless window showing the
    /// host entry; on submit the window emits [`Cmd::ConnectSshWindow`].
    NewSshWindow,
    /// Open the "connect to a host" prompt in *this* window (Cmd+G / Ctrl+Shift+G /
    /// Alt+G — "go"). Unlike [`NewSshWindow`](Cmd::NewSshWindow) it opens no new
    /// window: the current window (which already owns a session) shows the host
    /// entry, and on submit emits [`Cmd::ConnectSshSession`] to adopt the remote
    /// session as an additional tab.
    NewSshSession,
    /// The connect prompt's host was submitted: make this window an ssh group for
    /// `spec` and begin connecting over the transport. The shell records the
    /// group's connection (so later sessions inherit it) and starts ssh auth in a
    /// PTY; if ssh asks for a password it drives the prompt back to its password
    /// field ([`UiEvent`]-side `connect_request_password`), and on success replies
    /// `UiEvent::AdoptSession` to switch to the remote session.
    ConnectSshWindow {
        spec: ghost_vt::connection::ConnectionSpec,
    },
    /// The connect prompt's host was submitted for a *new session* (Cmd+G): begin
    /// connecting to `spec` and, on success, adopt the remote session as an
    /// additional session in this window. Unlike [`ConnectSshWindow`](Cmd::ConnectSshWindow)
    /// the window is *not* marked an ssh group — it just gains a remote tab; the
    /// shared connect/auth path (password prompt, staging, attach) is otherwise
    /// identical.
    ConnectSshSession {
        spec: ghost_vt::connection::ConnectionSpec,
    },
    /// The connect prompt's password was submitted: the shell feeds it to the
    /// in-flight ssh auth (over its PTY). Not stored — written straight through.
    ConnectPassword(String),
    /// Abort an in-flight connect *without* closing the window (the new-session
    /// flow's Escape): the shell drops the warm-up ssh (its `Drop` kills the
    /// child) and returns to the window's existing session. The window-flow Escape
    /// uses [`CloseWindow`](Cmd::CloseWindow) instead, which drops the whole window.
    CancelConnect,
    /// From the connect prompt's transport-fallback choice screen: the user chose
    /// to fall back to a plain `ssh <host>` child (the remote couldn't host a
    /// protocol-matched ghost). The shell spawns the local ssh-child session it had
    /// queued for this connect and adopts it. Carries nothing — the shell still
    /// holds the pending connect's spec and session name.
    UsePlainSshFallback,
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
    /// Upload a kitty-graphics image's pixels to the renderer, out of band (the
    /// pixels never travel through the `Scene`/`Frame`, which stay cheap to clone
    /// and compare). Sent once per image, before the `Redraw` that first draws it.
    ///
    /// The id is only meaningful next to the `session` that transmitted it: the
    /// program picks it (`i=`), and every session's ids start at 1. A window holds
    /// many sessions, so the renderer caches on the pair — otherwise the first
    /// session to claim an id would hand its picture to every other session that
    /// drew the same one.
    UploadImage {
        session: SessionId,
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
    /// Ask the OS to flag this window for attention (taskbar highlight /
    /// dock bounce) — an owned session rang its bell while the window was
    /// unfocused.
    RequestAttention,
    /// Repaint the window.
    Redraw,
    /// Set the window title.
    SetTitle(String),
    /// Persist the session-group registry (the shell writes it to the data
    /// dir and rebroadcasts it to the other windows). Sent with the full new
    /// state whenever this window's membership changes.
    SaveGroups(Vec<crate::group::Group>),
    /// Ask for a future `UiEvent::Tick` after the given delay.
    ScheduleTick {
        after_ms: u64,
    },
    /// Exit the application.
    Quit,
}

impl Cmd {
    /// Does this command reach *out* of its session, onto state the whole desktop
    /// shares — the window it happens to be in, or the system clipboard?
    ///
    /// A session is one of possibly many in a window, and — in the fleet — may not
    /// even be one the window owns: tiles preview sessions attached elsewhere, on
    /// hosts we don't control. So the things a program can ask for that land
    /// outside its own screen (XTWINOPS and the grid-driven resize behind DECCOLM;
    /// the title; an OSC 52 clipboard write) are only the *visible, foreground*
    /// session's to ask for. The non-foreground feed paths filter these out, or
    /// four bytes from a session the user isn't even looking at would minimize
    /// their window or replace what they last copied.
    ///
    /// This is about *who* may do it, not whether it's allowed at all — a program
    /// in the foreground still does all of this.
    pub fn reaches_the_desktop(&self) -> bool {
        matches!(
            self,
            Cmd::SetTitle(_)
                | Cmd::ResizeWindow { .. }
                | Cmd::SetIconified(_)
                | Cmd::SetMaximized(_)
                | Cmd::SetFullscreen(_)
                | Cmd::WriteClipboard(_)
                | Cmd::WritePrimary(_)
        )
    }
}

/// The pointer shape a [`Cmd::PointerIcon`] asks the window to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerIcon {
    /// The platform's normal arrow / text cursor.
    Default,
    /// The link-hover hand.
    Pointer,
}
