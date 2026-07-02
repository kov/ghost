//! The attach client: a transparent pipe.
//!
//! Puts the terminal in raw mode and forwards stdin<->host byte-for-byte,
//! intercepting only the configurable detach/kill trigger (CLI default: `C-\`
//! prefix, then `d` to detach or `k` to kill; the prefix doubled sends a
//! literal). Everything else — including mouse reports and bracketed paste —
//! passes straight through, so the host terminal's native scrollback and mouse
//! keep working.
//!
//! The transport itself — connect, framed send, drained receive — is factored
//! into [`Client`], a headless attach connection that a GUI front-end drives the
//! same way (`send` a [`ClientMsg`], `recv_ready` to drain [`ServerMsg`]s) while
//! rendering elsewhere. The terminal pipe below is one consumer of it.

use crate::keys::{Action, Detacher};
use crate::paths;
use crate::protocol::{ClientMsg, ServerMsg};
use crate::signals;
use crate::transport::Conn;
use nix::sys::signal::Signal;
use rustix::event::{PollFd, PollFlags, poll};
use rustix::termios::{OptionalActions, Termios, tcgetattr, tcgetwinsize, tcsetattr};
use std::io::{self, Read, Write};
use std::os::fd::BorrowedFd;
use std::path::Path;
use std::time::Duration;

/// A programmatic attach connection to a session host — the engine behind the
/// terminal [`attach`] client and any GUI front-end.
///
/// A thin protocol-typed wrapper over a [`Conn`]: [`send`](Client::send) a
/// [`ClientMsg`], drain [`ServerMsg`]s with [`recv_ready`](Client::recv_ready)
/// when the socket has data, and watch [`as_fd`](Client::as_fd) in a poll/event
/// loop. The stream is blocking; reads happen only when bytes are waiting (poll
/// readable, or set a read timeout).
pub struct Client {
    conn: Conn,
    /// The host's declared protocol feature level, read from the session dir's
    /// `proto` marker at connect time (0 when absent — a host built before the
    /// marker existed). Optional messages are gated on it: an old host treats
    /// a message it cannot decode as a connection error and drops us.
    proto: u32,
}

impl Client {
    /// Connect to the named session, resolving its socket via the XDG paths.
    pub fn connect(name: &str) -> io::Result<Self> {
        Self::connect_path(&paths::socket_path(name)).map_err(|e| {
            io::Error::new(e.kind(), format!("cannot attach to session '{name}': {e}"))
        })
    }

    /// Connect to a session whose control socket is at `sock`.
    pub fn connect_path(sock: &Path) -> io::Result<Self> {
        Ok(Client {
            conn: Conn::connect(sock)?,
            proto: proto_at(sock),
        })
    }

    /// The connection's file descriptor, for a poll/epoll/GLib main-loop watch.
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.conn.as_fd()
    }

    /// The host's declared protocol feature level (0 for a pre-marker host).
    pub fn proto(&self) -> u32 {
        self.proto
    }
}

/// The protocol feature level declared by the host whose socket is at `sock`,
/// read from the session dir's `proto` marker (0 when absent — a host built
/// before the marker existed).
fn proto_at(sock: &Path) -> u32 {
    sock.parent()
        .and_then(|dir| std::fs::read_to_string(dir.join("proto")).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// The named session's declared protocol feature level (see [`proto_at`]).
fn session_proto(name: &str) -> u32 {
    proto_at(&paths::socket_path(name))
}

impl Client {
    /// Send a message to the host.
    pub fn send(&mut self, msg: &ClientMsg) -> io::Result<()> {
        self.conn.send(msg)
    }

    /// Read once from the socket and return every [`ServerMsg`] that completed;
    /// `Ok(None)` on clean EOF. See [`Conn::recv`].
    pub fn recv_ready(&mut self) -> io::Result<Option<Vec<ServerMsg>>> {
        self.conn.recv()
    }

    /// Bound how long a [`recv_ready`](Client::recv_ready) read waits for data;
    /// `None` restores blocking until readable.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.conn.set_read_timeout(timeout)
    }
}

/// Ready state drained from a [`Subscriber`] by [`pump`](Subscriber::pump).
#[derive(Debug, Default)]
pub struct SubscriberPump {
    /// The one consistent starting state, delivered shortly after connect and
    /// before any event.
    pub snapshot: Option<crate::protocol::SessionState>,
    /// State changes since the snapshot (or the previous pump), in order.
    pub events: Vec<crate::protocol::SessionEvent>,
    /// Mirrored session output, in arrival order — only for a connection
    /// opened with [`Subscriber::observe`]; always empty on a plain
    /// subscription. Feed it to an emulator sized by the latest
    /// [`SessionEvent::Resized`](crate::protocol::SessionEvent::Resized)
    /// (which precedes any output).
    pub output: Vec<u8>,
    /// `true` once the subscription has ended — the host exited (socket EOF)
    /// or the connection failed. No further events will arrive.
    pub ended: bool,
}

/// A state-observer connection to a session host: subscribes on connect
/// ([`ClientMsg::Subscribe`]) and is pushed one snapshot followed by
/// [`SessionEvent`](crate::protocol::SessionEvent) deltas. An observer is not
/// a display client — it never resizes the PTY, never steals the display, and
/// a bell it observes is not "seen". Host death arrives as
/// [`ended`](SubscriberPump::ended), not an error.
///
/// Watch [`as_fd`](Subscriber::as_fd) for readiness (or set a read timeout),
/// then [`pump`](Subscriber::pump) — the same loop shape as [`Session`].
pub struct Subscriber {
    client: Client,
}

impl Subscriber {
    /// Subscribe to the named session (resolved via the XDG paths).
    ///
    /// Fails with [`io::ErrorKind::Unsupported`] when the host predates
    /// subscriptions ([`PROTO_SUBSCRIBE`](crate::protocol::PROTO_SUBSCRIBE));
    /// callers fall back to polling the session's marker files.
    pub fn connect(name: &str) -> io::Result<Subscriber> {
        Self::from_client(Client::connect(name)?, ClientMsg::Subscribe)
    }

    /// Subscribe to a session whose control socket is at `sock`.
    pub fn connect_path(sock: &Path) -> io::Result<Subscriber> {
        Self::from_client(Client::connect_path(sock)?, ClientMsg::Subscribe)
    }

    /// Observe the named session: subscribe *and* mirror its output
    /// read-only ([`ClientMsg::Observe`] — live previews). Fails with
    /// [`io::ErrorKind::Unsupported`] when the host predates observation
    /// ([`PROTO_OBSERVE`](crate::protocol::PROTO_OBSERVE)).
    pub fn observe(name: &str) -> io::Result<Subscriber> {
        Self::from_client(Client::connect(name)?, ClientMsg::Observe)
    }

    /// Observe a session whose control socket is at `sock`.
    pub fn observe_path(sock: &Path) -> io::Result<Subscriber> {
        Self::from_client(Client::connect_path(sock)?, ClientMsg::Observe)
    }

    fn from_client(mut client: Client, verb: ClientMsg) -> io::Result<Subscriber> {
        let needed = match verb {
            ClientMsg::Observe => crate::protocol::PROTO_OBSERVE,
            _ => crate::protocol::PROTO_SUBSCRIBE,
        };
        if client.proto() < needed {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "host predates this subscription verb; poll its markers instead",
            ));
        }
        client.send(&verb)?;
        // Never block: a shell pumps a whole pool of subscriptions on its loop
        // cadence, and even a millisecond's read timeout per idle subscription
        // would add up to real latency.
        client.conn.set_nonblocking(true)?;
        Ok(Subscriber { client })
    }

    /// The connection's file descriptor, for a poll/event-loop watch.
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.client.as_fd()
    }

    /// Drain whatever state is ready now; never blocks. Any read failure ends
    /// the subscription (reported via [`ended`](SubscriberPump::ended)) — for
    /// an observer, a broken connection and a dead host mean the same thing.
    pub fn pump(&mut self) -> io::Result<SubscriberPump> {
        let mut pump = SubscriberPump::default();
        match self.client.recv_ready() {
            Ok(Some(msgs)) => {
                for msg in msgs {
                    match msg {
                        ServerMsg::Snapshot(state) => pump.snapshot = Some(state),
                        ServerMsg::Event(e) => pump.events.push(e),
                        // Mirrored output (observe connections only; a plain
                        // subscription never receives it).
                        ServerMsg::Output(bytes) => pump.output.extend_from_slice(&bytes),
                        ServerMsg::Exited(_) | ServerMsg::RenameResult { .. } => {}
                    }
                }
            }
            Ok(None) | Err(_) => pump.ended = true,
        }
        Ok(pump)
    }
}

/// Ready output drained from a [`Session`] by [`pump`](Session::pump).
#[derive(Debug, Default)]
pub struct Pump {
    /// Child output bytes to render, in arrival order.
    pub output: Vec<u8>,
    /// `true` once the session has ended — the child exited or the host closed
    /// the connection. No further output will arrive.
    pub ended: bool,
}

/// An attached session for an event-loop front-end (a GUI): attach and
/// handshake, [`pump`](Session::pump) ready output as bytes, send input and
/// resizes, and **detach by dropping** (the connection closes, the session keeps
/// running and can be reattached).
///
/// A thin, protocol-typed layer over [`Client`] so view code never touches
/// [`ClientMsg`]/[`ServerMsg`]; the byte stream it yields is fed straight to a
/// terminal widget. Watch [`as_fd`](Session::as_fd) for readiness, or set a read
/// timeout, then [`pump`](Session::pump).
pub struct Session {
    name: String,
    client: Client,
}

impl Session {
    /// Attach to the named session (resolved via the XDG paths) and complete the
    /// handshake at `cols`x`rows` — the first [`ClientMsg::Resize`], which starts
    /// a deferred child and turns on live output.
    pub fn attach(name: &str, cols: u16, rows: u16) -> io::Result<Session> {
        Self::from_client(Client::connect(name)?, name, cols, rows)
    }

    /// Attach to a session whose control socket is at `sock`, labelling it
    /// `name`. Like [`attach`](Session::attach) but without name-based path
    /// resolution, so it ignores the process environment.
    pub fn attach_path(sock: &Path, name: &str, cols: u16, rows: u16) -> io::Result<Session> {
        Self::from_client(Client::connect_path(sock)?, name, cols, rows)
    }

    fn from_client(mut client: Client, name: &str, cols: u16, rows: u16) -> io::Result<Session> {
        client.send(&ClientMsg::Resize { cols, rows })?;
        Ok(Session {
            name: name.to_string(),
            client,
        })
    }

    /// Attach *without* completing the handshake: unlike [`attach`](Session::attach)
    /// no initial size is sent, so the host holds output (and any deferred child)
    /// until the caller sends the first [`resize`](Session::resize). A client that
    /// only learns its real display size once its widget is laid out (the GUI) uses
    /// this so the repaint is generated at that real size, not a provisional one.
    pub fn attach_deferred(name: &str) -> io::Result<Session> {
        Ok(Session {
            name: name.to_string(),
            client: Client::connect(name)?,
        })
    }

    /// [`attach_deferred`](Session::attach_deferred) for a socket at an explicit
    /// path, mirroring [`attach_path`](Session::attach_path).
    pub fn attach_deferred_path(sock: &Path, name: &str) -> io::Result<Session> {
        Ok(Session {
            name: name.to_string(),
            client: Client::connect_path(sock)?,
        })
    }

    /// The session's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The connection's file descriptor, for a poll/epoll/GLib readiness watch.
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.client.as_fd()
    }

    /// Bound how long a [`pump`](Session::pump) read waits for data; `None`
    /// blocks until readable.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.client.set_read_timeout(timeout)
    }

    /// Send user input (keystrokes, mouse reports, paste, query replies) to the
    /// session's child.
    pub fn send_input(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.client.send(&ClientMsg::Input(bytes.to_vec()))
    }

    /// Tell the session the display was resized.
    pub fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        self.client.send(&ClientMsg::Resize { cols, rows })
    }

    /// Report this client's theme colors. The host keeps them as the session's
    /// last-attached colors, answering the child's color queries with them
    /// while detached.
    ///
    /// Silently skipped when the host predates [`ClientMsg::Theme`] (it never
    /// declared [`PROTO_THEME`](crate::protocol::PROTO_THEME)): the report is
    /// advisory, and an old host would treat the unknown message as a
    /// connection error and drop us right after attaching.
    pub fn report_theme(&mut self, colors: crate::query::ThemeColors) -> io::Result<()> {
        if self.client.proto < crate::protocol::PROTO_THEME {
            return Ok(());
        }
        self.client.send(&ClientMsg::Theme(colors))
    }

    /// Identify this display client to the host ([`ClientMsg::Hello`]) so
    /// state subscribers can see *who* holds the display ([`AttachInfo`]
    /// (crate::protocol::AttachInfo)). Advisory, sent right after attaching
    /// like [`report_theme`](Session::report_theme); silently skipped on a
    /// host that predates it.
    pub fn hello(&mut self, client: &str) -> io::Result<()> {
        if self.client.proto < crate::protocol::PROTO_SUBSCRIBE {
            return Ok(());
        }
        self.client.send(&ClientMsg::Hello {
            client: client.to_string(),
        })
    }

    /// Drain whatever output is ready now, flattening protocol messages into
    /// bytes plus an end-of-session flag. Non-blocking when [`as_fd`](Session::as_fd)
    /// polls readable or a read timeout is set; see [`Client::recv_ready`].
    pub fn pump(&mut self) -> io::Result<Pump> {
        let mut pump = Pump::default();
        match self.client.recv_ready()? {
            None => pump.ended = true,
            Some(msgs) => {
                for msg in msgs {
                    match msg {
                        ServerMsg::Output(bytes) => pump.output.extend_from_slice(&bytes),
                        ServerMsg::Exited(_code) => pump.ended = true,
                        ServerMsg::RenameResult { .. } => {}
                        // Pushed subscription state; a display client is not a
                        // subscriber, so nothing to do.
                        ServerMsg::Snapshot(_) | ServerMsg::Event(_) => {}
                    }
                }
            }
        }
        Ok(pump)
    }
}

/// Attach to the named session, returning when the user detaches or the session
/// ends.
pub fn attach(name: &str) -> io::Result<()> {
    let mut client = Client::connect(name)?;

    let stdin = rustix::stdio::stdin();

    // Raw mode, restored on return via the guard's Drop.
    let _raw = RawMode::enable(stdin)?;

    // Sync the session to our current size immediately. The first resize is also
    // the attach handshake that promotes us to the host's display client.
    send_resize(&mut client, stdin)?;

    let sfd = signals::make(&[Signal::SIGWINCH])?;
    let mut detacher = Detacher::with_default_prefix();
    let mut in_buf = [0u8; 4096];
    // Some(_) while the rename prompt is on screen; input then feeds the prompt
    // rather than the session, and live output is suppressed until it closes.
    let mut prompt: Option<RenamePrompt> = None;

    loop {
        let (stdin_re, sock_re, sig_re) = {
            let mut fds = [
                PollFd::from_borrowed_fd(stdin, PollFlags::IN),
                PollFd::from_borrowed_fd(client.as_fd(), PollFlags::IN),
                PollFd::new(&sfd, PollFlags::IN),
            ];
            match poll(&mut fds, None) {
                Ok(_) => {}
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(e.into()),
            }
            (fds[0].revents(), fds[1].revents(), fds[2].revents())
        };

        // stdin -> host (or the rename prompt, when one is open)
        if stdin_re.contains(PollFlags::IN) {
            let n = io::stdin().read(&mut in_buf)?;
            if n == 0 {
                break;
            }
            if prompt.is_some() {
                prompt_input(&in_buf[..n], &mut prompt, &mut client, stdin)?;
            } else {
                for action in detacher.feed(&in_buf[..n]) {
                    match action {
                        Action::Forward(bytes) => {
                            // A prefix-r mid-batch opens the prompt; route any
                            // bytes typed after it on this same read to it.
                            if prompt.is_some() {
                                prompt_input(&bytes, &mut prompt, &mut client, stdin)?;
                            } else {
                                client.send(&ClientMsg::Input(bytes))?;
                            }
                        }
                        Action::Detach => {
                            eprint!("\r\n[ghost: detached]\r\n");
                            return Ok(());
                        }
                        Action::Kill => {
                            let _ = client.send(&ClientMsg::Kill);
                            eprint!("\r\n[ghost: killed session]\r\n");
                            return Ok(());
                        }
                        Action::Rename => {
                            start_prompt(&mut prompt, stdin)?;
                        }
                    }
                }
            }
        }

        // host -> stdout
        if sock_re.intersects(PollFlags::IN | PollFlags::HUP) {
            let Some(msgs) = client.recv_ready()? else {
                eprint!("\r\n[ghost: session closed]\r\n");
                return Ok(());
            };
            for msg in msgs {
                match msg {
                    ServerMsg::Output(bytes) => {
                        // While the prompt overlay is up, drop live output; the
                        // screen is repainted from authoritative state when the
                        // prompt closes (via a Repaint request).
                        if prompt.is_none() {
                            let mut out = io::stdout().lock();
                            out.write_all(&bytes)?;
                            out.flush()?;
                        }
                    }
                    ServerMsg::Exited(_code) => {
                        eprint!("\r\n[ghost: session ended]\r\n");
                        return Ok(());
                    }
                    ServerMsg::RenameResult { ok, message } => {
                        rename_result(ok, &message, &mut prompt, &mut client, stdin)?;
                    }
                    // Pushed subscription state; a display client is not a
                    // subscriber, so nothing to do.
                    ServerMsg::Snapshot(_) | ServerMsg::Event(_) => {}
                }
            }
        }

        // terminal resize -> host
        if sig_re.contains(PollFlags::IN) {
            signals::drain(&sfd)?;
            send_resize(&mut client, stdin)?;
        }
    }
    Ok(())
}

/// Rename a session non-interactively (the `ghost rename` command). Connects to
/// the session by its immutable id and asks the host to set its display name,
/// returning the host's verdict. A label change only — the session's files and
/// attach state are untouched — and sent over a control connection (no resize),
/// so any attached client is left undisturbed.
///
/// Refused for a host predating label renames (see
/// [`PROTO_RENAME_LABEL`](crate::protocol::PROTO_RENAME_LABEL)): such a host
/// would move the session's files, detaching clients — the very churn the
/// label design removed.
pub fn rename(old: &str, new: &str) -> io::Result<()> {
    if session_proto(old) < crate::protocol::PROTO_RENAME_LABEL {
        return Err(io::Error::other(format!(
            "session '{old}' is hosted by an older ghost that would move its \
             files to rename it (detaching clients); restart the session to \
             rename it safely"
        )));
    }
    let mut conn = Conn::connect(&paths::socket_path(old))
        .map_err(|e| io::Error::new(e.kind(), format!("cannot reach session '{old}': {e}")))?;
    conn.send(&ClientMsg::Rename(new.to_string()))?;

    loop {
        match conn.recv::<ServerMsg>()? {
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "session closed before replying",
                ));
            }
            Some(msgs) => {
                for msg in msgs {
                    if let ServerMsg::RenameResult { ok, message } = msg {
                        return if ok {
                            Ok(())
                        } else {
                            Err(io::Error::other(message))
                        };
                    }
                }
            }
        }
    }
}

/// State of the on-screen rename prompt opened by `C-\ r`.
struct RenamePrompt {
    /// The name typed so far.
    buf: String,
    /// True once submitted: input is ignored until the host's reply arrives.
    awaiting: bool,
}

/// Open the rename prompt: save the cursor (once) and draw the empty prompt on
/// the bottom row.
fn start_prompt(prompt: &mut Option<RenamePrompt>, fd: BorrowedFd<'_>) -> io::Result<()> {
    *prompt = Some(RenamePrompt {
        buf: String::new(),
        awaiting: false,
    });
    let mut out = io::stdout().lock();
    out.write_all(b"\x1b7")?; // DECSC: save cursor, restored when the prompt closes
    render_prompt(&mut out, fd, "", None)?;
    Ok(())
}

/// Feed a chunk of stdin to the open prompt: edit the name, submit on Enter, or
/// cancel on Esc / Ctrl-C.
fn prompt_input(
    bytes: &[u8],
    prompt: &mut Option<RenamePrompt>,
    client: &mut Client,
    fd: BorrowedFd<'_>,
) -> io::Result<()> {
    if prompt.as_ref().is_none_or(|p| p.awaiting) {
        return Ok(()); // submitted: wait for the host's reply before editing again
    }
    let mut out = io::stdout().lock();
    for &b in bytes {
        match b {
            b'\r' | b'\n' => {
                let name = prompt.as_ref().unwrap().buf.trim().to_string();
                if name.is_empty() {
                    continue;
                }
                // A host predating label renames would move the session's files
                // (detaching us); refuse in the prompt rather than trigger it.
                if client.proto() < crate::protocol::PROTO_RENAME_LABEL {
                    render_prompt(
                        &mut out,
                        fd,
                        &name,
                        Some("host too old; restart the session to rename"),
                    )?;
                    continue;
                }
                client.send(&ClientMsg::Rename(name))?;
                prompt.as_mut().unwrap().awaiting = true;
                break;
            }
            0x1b | 0x03 => {
                // Esc / Ctrl-C: cancel. Restore the cursor and ask the host to
                // repaint, healing the row the prompt drew over.
                out.write_all(b"\x1b8")?;
                out.flush()?;
                *prompt = None;
                client.send(&ClientMsg::Repaint)?;
                return Ok(());
            }
            0x7f | 0x08 => {
                prompt.as_mut().unwrap().buf.pop();
                render_prompt(&mut out, fd, &prompt.as_ref().unwrap().buf, None)?;
            }
            0x20..=0x7e => {
                prompt.as_mut().unwrap().buf.push(b as char);
                render_prompt(&mut out, fd, &prompt.as_ref().unwrap().buf, None)?;
            }
            _ => {} // ignore other control bytes
        }
    }
    Ok(())
}

/// Handle the host's reply to a submitted rename.
fn rename_result(
    ok: bool,
    message: &str,
    prompt: &mut Option<RenamePrompt>,
    client: &mut Client,
    fd: BorrowedFd<'_>,
) -> io::Result<()> {
    if prompt.is_none() {
        return Ok(());
    }
    if ok {
        *prompt = None;
        let mut out = io::stdout().lock();
        out.write_all(b"\x1b8")?; // restore cursor
        out.flush()?;
        client.send(&ClientMsg::Repaint)?; // repaint over the prompt
    } else {
        // Refused: re-enable editing and show why, so the user can fix the name.
        if let Some(p) = prompt.as_mut() {
            p.awaiting = false;
        }
        let buf = prompt.as_ref().unwrap().buf.clone();
        let mut out = io::stdout().lock();
        render_prompt(&mut out, fd, &buf, Some(message))?;
    }
    Ok(())
}

/// Draw the prompt on the terminal's bottom row (reverse video), leaving the
/// cursor after the typed text.
fn render_prompt(
    out: &mut impl Write,
    fd: BorrowedFd<'_>,
    buf: &str,
    error: Option<&str>,
) -> io::Result<()> {
    let (_, rows) = term_size(fd);
    let mut s = format!("\x1b[{rows};1H\x1b[2K\x1b[7m");
    match error {
        None => s.push_str(&format!(" rename session to: {buf} ")),
        Some(e) => s.push_str(&format!(" rename to: {buf}  [{e}] — esc to cancel ")),
    }
    s.push_str("\x1b[0m");
    out.write_all(s.as_bytes())?;
    out.flush()
}

/// Current terminal size as `(cols, rows)`, defaulting to 80x24 if unavailable.
fn term_size(fd: BorrowedFd<'_>) -> (u16, u16) {
    tcgetwinsize(fd)
        .map(|ws| (ws.ws_col, ws.ws_row))
        .unwrap_or((80, 24))
}

fn send_resize(client: &mut Client, fd: BorrowedFd<'_>) -> io::Result<()> {
    if let Ok(ws) = tcgetwinsize(fd) {
        client.send(&ClientMsg::Resize {
            cols: ws.ws_col,
            rows: ws.ws_row,
        })?;
    }
    Ok(())
}

/// Puts a terminal fd into raw mode and restores the original settings on drop.
struct RawMode {
    fd: BorrowedFd<'static>,
    original: Termios,
}

impl RawMode {
    fn enable(fd: BorrowedFd<'static>) -> io::Result<Self> {
        let original = tcgetattr(fd).map_err(io::Error::from)?;
        let mut raw = original.clone();
        raw.make_raw();
        tcsetattr(fd, OptionalActions::Now, &raw).map_err(io::Error::from)?;
        Ok(RawMode { fd, original })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = tcsetattr(self.fd, OptionalActions::Now, &self.original);
    }
}
