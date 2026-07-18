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
use std::time::{Duration, Instant};

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

    /// Tunnel to a remote session host over SSH: `cmd` is the `ssh … -- ghost
    /// __pipe <name>` whose stdio relays to the remote host's control socket.
    ///
    /// `proto` is the *running host's* level, which the caller reads over the
    /// transport ([`RemoteSsh::session_proto`](crate::remote::RemoteSsh::session_proto)) —
    /// NOT this binary's, because a staged binary can outrun a host still serving
    /// an older session, and gating a post-marker message on the wrong level makes
    /// the old host drop us. A freshly-spawned session (spawned by the current
    /// staged binary) passes [`PROTO_LEVEL`](crate::protocol::PROTO_LEVEL).
    pub fn connect_ssh(cmd: std::process::Command, proto: u32) -> io::Result<Self> {
        Ok(Client {
            conn: Conn::connect_ssh(cmd)?,
            proto,
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

/// The named session's declared protocol feature level (see [`proto_at`]). Read
/// from the session's own `proto` marker, so it reflects the *running* host that
/// serves it, not this binary — the `ghost __proto <name>` far end of the SSH
/// transport prints it so an initiator with a newer binary learns whether a
/// session's (possibly older) host can decode a post-marker message.
pub fn session_proto(name: &str) -> u32 {
    proto_at(&paths::socket_path(name))
}

impl Client {
    /// Send a message to the host.
    pub fn send(&mut self, msg: &ClientMsg) -> io::Result<()> {
        self.conn.send(msg)
    }

    /// Flush any output an earlier [`send`](Client::send) left buffered when the
    /// transport was not writable — [`Conn::flush`] stops on `WouldBlock` and
    /// [`Conn::send`] swallows it, so bytes can linger in the outbuf. A caller that
    /// then only reads (a display client pumping while idle) would never retry the
    /// write, stranding e.g. a query reply the child is blocked on. Pumps call this
    /// each tick; the host loop already does the equivalent.
    pub fn flush_pending(&mut self) -> io::Result<()> {
        if self.conn.wants_write() {
            self.conn.flush()?;
        }
        Ok(())
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

    /// Put the connection into (non-)blocking mode. A front-end that pumps a
    /// whole pool of clients on one event-loop cadence wants this so an idle
    /// client's [`recv_ready`](Client::recv_ready) returns at once instead of
    /// blocking — the same choice [`Subscriber`] makes.
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.conn.set_nonblocking(nonblocking)
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

    /// Observe a *remote* session over the SSH transport: `cmd` is the
    /// `ssh … -- ghost __pipe <name>` tunnel to the remote host's control socket.
    /// Same read-only mirror as [`observe`](Subscriber::observe), for live remote
    /// fleet previews. `proto` is the running host's level (see
    /// [`Client::connect_ssh`]): an older host that predates `Observe` is refused
    /// here rather than sent a verb it drops the connection over.
    pub fn observe_ssh(cmd: std::process::Command, proto: u32) -> io::Result<Subscriber> {
        Self::from_client(Client::connect_ssh(cmd, proto)?, ClientMsg::Observe)
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
        self.client.flush_pending()?;
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
                        ServerMsg::Exited(_)
                        | ServerMsg::RenameResult { .. }
                        | ServerMsg::UpgradeResult { .. }
                        | ServerMsg::Superseded => {}
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
    /// `true` once no further output will arrive on this transport — either the
    /// child exited ([`ServerMsg::Exited`]) or the connection closed (EOF).
    pub ended: bool,
    /// Set alongside `ended` **only** when the end was the transport closing (EOF)
    /// rather than an explicit [`ServerMsg::Exited`]. For a *local* session these
    /// are the same thing (the host process is gone), but for a *remote* session an
    /// EOF is a lost connection whose session may still be alive on the far side —
    /// so the caller can try to reconnect and resync instead of tearing the tile
    /// down. A clean child exit leaves this `false`.
    pub disconnected: bool,
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

    /// Attach to a remote session over an SSH tunnel (`cmd` = `ssh … -- ghost
    /// __pipe <name>`), completing the handshake at `cols`x`rows`. The remote
    /// host is a real ghost host; only the transport differs from
    /// [`attach`](Session::attach).
    pub fn attach_ssh(
        cmd: std::process::Command,
        name: &str,
        cols: u16,
        rows: u16,
        proto: u32,
    ) -> io::Result<Session> {
        Self::from_client(Client::connect_ssh(cmd, proto)?, name, cols, rows)
    }

    /// [`attach_deferred`](Session::attach_deferred) over an SSH tunnel: connect
    /// but send no size until the caller's first [`resize`](Session::resize), so
    /// the remote repaint is generated at the GUI's real geometry. `proto` is the
    /// running host's level (see [`Client::connect_ssh`]).
    pub fn attach_deferred_ssh(
        cmd: std::process::Command,
        name: &str,
        proto: u32,
    ) -> io::Result<Session> {
        Ok(Session {
            name: name.to_string(),
            client: Client::connect_ssh(cmd, proto)?,
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

    /// Put the session's socket into (non-)blocking mode. A GUI pumps every
    /// attached session on its frame loop, so non-blocking is what keeps an idle
    /// session's [`pump`](Session::pump) from stalling the loop — see
    /// [`Client::set_nonblocking`].
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.client.set_nonblocking(nonblocking)
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

    /// Report the policy this terminal enforces — what a program on the session's
    /// tty may change about the terminal (see [`ghost_term::policy`]). The host
    /// adopts it for its own emulator and keeps enforcing it while detached, so the
    /// session and the terminal showing it never disagree about what a program got
    /// away with.
    ///
    /// Sent right after attaching, like [`report_theme`](Session::report_theme), and
    /// silently skipped when the host predates [`ClientMsg::Policy`] (it never
    /// declared [`PROTO_POLICY`](crate::protocol::PROTO_POLICY)) — an old host would
    /// treat the unknown message as a connection error and drop us. Such a session
    /// keeps running under the policy its host was spawned with; there is nothing
    /// this client can do about that from out here.
    pub fn report_policy(&mut self, policy: ghost_term::TerminalPolicy) -> io::Result<()> {
        if self.client.proto < crate::protocol::PROTO_POLICY {
            return Ok(());
        }
        self.client.send(&ClientMsg::Policy(policy))
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
        self.client.flush_pending()?;
        let mut pump = Pump::default();
        match self.client.recv_ready()? {
            None => {
                // The transport closed with no `Exited` — a lost connection, not a
                // clean end (a remote session may still be alive to reconnect to).
                pump.ended = true;
                pump.disconnected = true;
            }
            Some(msgs) => {
                for msg in msgs {
                    match msg {
                        ServerMsg::Output(bytes) => pump.output.extend_from_slice(&bytes),
                        ServerMsg::Exited(_code) => pump.ended = true,
                        // `Superseded` is ignored here: the GUI's take-over is
                        // coordinated through the shared model, and the drop that
                        // follows surfaces via the usual EOF path — unchanged from
                        // before this message existed.
                        ServerMsg::RenameResult { .. }
                        | ServerMsg::UpgradeResult { .. }
                        | ServerMsg::Superseded => {}
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
    // A local attach can reconnect by name: if the connection drops but the
    // host still holds its lock (a self-upgrade re-execs in place), re-attach
    // instead of exiting.
    run_attach(Client::connect(name)?, Some(name))
}

/// Attach to a *remote* session over the SSH transport: `cmd` is the
/// `ssh … -- ghost __pipe <name>` whose stdio tunnels to the remote host. The
/// terminal pipe is identical to a local attach — only the transport differs.
/// `proto` is the running host's level (see [`Client::connect_ssh`]); this raw
/// attach sends only Resize/Input/Kill so it never trips the gate, but it carries
/// the real level for consistency with the GUI paths.
pub fn attach_ssh(cmd: std::process::Command, proto: u32) -> io::Result<()> {
    // No by-name reconnect for a remote pipe: the liveness lock lives on the
    // remote host, which we can't flock from here — the GUI's remote path rides
    // its own `disconnected` reconnect (see [`Session::pump`]) instead.
    run_attach(Client::connect_ssh(cmd, proto)?, None)
}

/// After a local attach's connection drops, decide whether the host is still
/// there and, if so, reconnect. A self-upgrade re-execs the host in place — same
/// pid, same lock, same listener socket — so the drop is not the session ending;
/// the lock is still held and the socket still accepts. Retries briefly because
/// we may notice the drop a hair before the successor is serving again (in
/// practice the listener fd survives the exec so it accepts immediately; the
/// grace just covers a slow successor). Returns `None` when the host is really
/// gone (lock free) so the caller exits.
fn try_reconnect(name: &str) -> Option<Client> {
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        // Lock free ⇒ the host is genuinely gone; don't wait, just exit.
        if !crate::session::host_is_live(name) {
            return None;
        }
        if let Ok(c) = Client::connect(name) {
            return Some(c);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Whether an I/O error is a dropped connection (the peer closed) rather than a
/// real failure — the class that, for a local attach, might be a self-upgrade
/// re-exec worth reconnecting across.
fn is_disconnect(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::UnexpectedEof
    )
}

/// Re-attach `client` in place after its connection dropped (a local
/// self-upgrade re-exec keeps the lock, socket and child). Drops any in-flight
/// rename prompt — its reply died with the old connection, so leaving it up
/// would wedge the client swallowing all input — and re-handshakes with a resize
/// so the successor resyncs the screen. `Ok(true)` = reconnected (caller
/// continues); `Ok(false)` = nothing to reconnect to (a remote pipe, or the host
/// is really gone) so the caller should exit.
fn reconnect_in_place(
    client: &mut Client,
    reconnect: Option<&str>,
    stdin: BorrowedFd<'_>,
    prompt: &mut Option<RenamePrompt>,
) -> io::Result<bool> {
    let Some(name) = reconnect else {
        return Ok(false);
    };
    let Some(new) = try_reconnect(name) else {
        return Ok(false);
    };
    *client = new;
    *prompt = None;
    send_resize(client, stdin)?;
    Ok(true)
}

/// The raw-mode terminal pipe shared by [`attach`] and [`attach_ssh`]: forward
/// stdin<->host until the user detaches or the session ends. `reconnect` is the
/// session name for a LOCAL attach (enabling by-name re-attach across a host
/// self-upgrade) or `None` for a remote pipe.
fn run_attach(mut client: Client, reconnect: Option<&str>) -> io::Result<()> {
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
                            } else if let Err(e) = client.send(&ClientMsg::Input(bytes)) {
                                // The host may have re-exec'd out from under this
                                // send (a local self-upgrade). Reconnect rather
                                // than exit; otherwise propagate the real error.
                                // The unsent `bytes` (this read — up to a whole
                                // paste chunk) are dropped, an accepted loss in
                                // the narrow window where a send races the exec.
                                if !(is_disconnect(&e)
                                    && reconnect_in_place(
                                        &mut client,
                                        reconnect,
                                        stdin,
                                        &mut prompt,
                                    )?)
                                {
                                    return Err(e);
                                }
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
                // The connection dropped without a clean `Exited`. For a local
                // attach this can be a self-upgrade (the host re-exec'd in place,
                // keeping its lock, socket and child) rather than the session
                // ending — reconnect if the host still holds its lock. Take-over
                // is NOT this path: the host sends `Superseded` first, handled
                // below, so we never mistake it for a re-exec.
                if reconnect_in_place(&mut client, reconnect, stdin, &mut prompt)? {
                    continue;
                }
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
                    // An interactive attach doesn't drive upgrades (only the
                    // `ghost __upgrade` control path awaits this), so ignore it.
                    ServerMsg::UpgradeResult { .. } => {}
                    // Another client took over the display. Exit cleanly — this is
                    // NOT the ambiguous EOF the reconnect path handles, so we must
                    // not reconnect (that would fight the new client for the
                    // display forever).
                    ServerMsg::Superseded => {
                        eprint!("\r\n[ghost: another terminal took over this session]\r\n");
                        return Ok(());
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
            if let Err(e) = send_resize(&mut client, stdin) {
                // A resize racing a self-upgrade re-exec: reconnect (the resize
                // is re-sent as the reconnect handshake) rather than exit.
                if !(is_disconnect(&e)
                    && reconnect_in_place(&mut client, reconnect, stdin, &mut prompt)?)
                {
                    return Err(e);
                }
            }
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
/// Ask a session's host to upgrade itself in place onto a (possibly newer)
/// binary, keeping its running child, PTY, socket, and liveness lock — only the
/// host's code image is replaced (see `docs/host-self-upgrade.md`). `path` names
/// the target binary, or `None` for the host's own current executable.
///
/// Refused for a host predating the mechanism
/// ([`PROTO_UPGRADE`](crate::protocol::PROTO_UPGRADE)): such a host cannot
/// self-upgrade and would treat the unknown message as a broken connection. This
/// is the going-forward-only gate — restart the session (`__restart`) to bring
/// an older host up instead.
pub fn upgrade_session(name: &str, path: Option<String>) -> io::Result<()> {
    if session_proto(name) < crate::protocol::PROTO_UPGRADE {
        return Err(io::Error::other(format!(
            "session '{name}' is hosted by an older ghost that cannot upgrade itself \
             in place; restart it to bring it up to the current binary"
        )));
    }
    let mut conn = Conn::connect(&paths::socket_path(name))
        .map_err(|e| io::Error::new(e.kind(), format!("cannot reach session '{name}': {e}")))?;
    conn.send(&ClientMsg::Upgrade { path })?;
    // The host performs the upgrade at its next clean handoff boundary, then
    // re-execs — closing this control connection, whose fd does not cross the
    // exec. So EOF is our SUCCESS signal: the request was taken and the successor
    // now serves. A refusal instead comes back as `ServerMsg::UpgradeResult`
    // (the host holds the connection open to answer). `Conn::recv` reports a read
    // timeout as `Ok(Some(vec![]))` (not an error), so we clock our own deadline.
    // The host's two costs are SEQUENTIAL in the worst case — wait out the
    // boundary window, then probe — so size the deadline as their sum plus slack,
    // or a slow-but-genuine refusal would land after we stopped listening and be
    // misreported as success. Post-Step-5 a healthy host ALWAYS answers (EOF on
    // success, an `UpgradeResult` on refusal), so hitting this deadline means the
    // host is wedged or overloaded — an error, not a silent "delivered".
    conn.set_read_timeout(Some(Duration::from_millis(250)))?;
    let deadline = Instant::now()
        + crate::server::UPGRADE_BOUNDARY_WINDOW
        + crate::server::HANDOFF_PROBE_TIMEOUT
        + Duration::from_secs(5);
    loop {
        match conn.recv::<ServerMsg>()? {
            // EOF: the host re-exec'd (its control-connection fd is CLOEXEC, so
            // the exec closes it) — the upgrade was taken.
            None => return Ok(()),
            Some(msgs) => {
                for msg in msgs {
                    if let ServerMsg::UpgradeResult { ok, message } = msg {
                        return if ok {
                            Ok(())
                        } else {
                            Err(io::Error::other(message))
                        };
                    }
                }
                // An empty read-timeout batch (or unrelated late frames): keep
                // waiting until the deadline.
                if Instant::now() >= deadline {
                    return Err(io::Error::other(
                        "the upgrade request was delivered but the host did not confirm it in \
                         time (it may be wedged or overloaded) — check the session before retrying",
                    ));
                }
            }
        }
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::AnyTransport;
    use std::os::unix::net::UnixStream;

    /// A `Session` on one end of a socketpair, plus a [`Conn`] on the other end
    /// standing in for the host — send `ServerMsg`s or drop it to close.
    fn paired_session() -> (Conn<AnyTransport>, Session) {
        let (host, cli) = UnixStream::pair().unwrap();
        cli.set_nonblocking(true).unwrap();
        let host = Conn::new(AnyTransport::Unix(host));
        let session = Session {
            name: "t".into(),
            client: Client {
                conn: Conn::new(AnyTransport::Unix(cli)),
                proto: crate::protocol::PROTO_LEVEL,
            },
        };
        (host, session)
    }

    /// The distinction the mid-session reconnect feature turns on: `pump` must tell
    /// a lost connection (transport EOF → `disconnected`, the session may still live
    /// on the far side) from a clean child exit (`ServerMsg::Exited` → not
    /// `disconnected`, the child is gone). Before this split both were an
    /// undifferentiated `ended`, so a dropped remote connection was torn down like
    /// an exit and never reconnected.
    #[test]
    fn pump_flags_a_dropped_connection_but_not_a_clean_exit() {
        // A clean exit: an explicit `Exited` frame ends the session, not dropped.
        let (mut host, mut session) = paired_session();
        host.send(&ServerMsg::Exited(0)).unwrap();
        let p = session.pump().unwrap();
        assert!(p.ended, "an Exited frame ends the session");
        assert!(!p.disconnected, "a clean exit is not a dropped connection");

        // A lost connection: the peer closes with no `Exited` → disconnected.
        let (host, mut session) = paired_session();
        drop(host);
        let p = session.pump().unwrap();
        assert!(p.ended, "a closed connection ends the session");
        assert!(
            p.disconnected,
            "a closed connection is flagged disconnected"
        );
    }

    /// A `Subscriber` on one end of a socketpair, plus the host-side [`Conn`].
    fn paired_subscriber() -> (Conn<AnyTransport>, Subscriber) {
        let (host, cli) = UnixStream::pair().unwrap();
        cli.set_nonblocking(true).unwrap();
        let host = Conn::new(AnyTransport::Unix(host));
        let sub = Subscriber {
            client: Client {
                conn: Conn::new(AnyTransport::Unix(cli)),
                proto: crate::protocol::PROTO_LEVEL,
            },
        };
        (host, sub)
    }

    /// A `send()` that hits `WouldBlock` leaves its bytes in the `Conn`'s outbuf —
    /// `flush` swallows `WouldBlock`. An idle display client that then only ever
    /// reads (its child blocked awaiting a query reply) never re-flushes, so the
    /// reply strands indefinitely until the user happens to type. `pump` must drain
    /// any pending output each tick.
    #[test]
    fn session_pump_flushes_output_stranded_by_back_pressure() {
        let (mut host, mut session) = paired_session();
        host.set_nonblocking(true).unwrap();

        // Stand in for a query reply a prior flush left buffered under back-pressure.
        session
            .client
            .conn
            .queue(&ClientMsg::Input(b"\x1b[0n".to_vec()));
        assert!(
            session.client.conn.wants_write(),
            "precondition: the reply is stranded in the outbuf"
        );

        // The idle client only pumps (reads) — it sends nothing new to re-flush.
        session.pump().unwrap();

        assert!(
            !session.client.conn.wants_write(),
            "pump drained the stranded reply"
        );
        let got: Vec<ClientMsg> = host.recv().unwrap().unwrap();
        assert_eq!(
            got,
            vec![ClientMsg::Input(b"\x1b[0n".to_vec())],
            "the host received the stranded reply"
        );
    }

    /// The same drain contract on the observer/subscriber pump: output left
    /// buffered under back-pressure must not strand.
    #[test]
    fn subscriber_pump_flushes_stranded_output() {
        let (mut host, mut sub) = paired_subscriber();
        host.set_nonblocking(true).unwrap();

        sub.client.conn.queue(&ClientMsg::Subscribe);
        assert!(sub.client.conn.wants_write());

        sub.pump().unwrap();

        assert!(
            !sub.client.conn.wants_write(),
            "pump drained the stranded frame"
        );
        let got: Vec<ClientMsg> = host.recv().unwrap().unwrap();
        assert_eq!(got, vec![ClientMsg::Subscribe]);
    }
}
