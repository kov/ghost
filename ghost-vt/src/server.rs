//! The session host: a synchronous `poll()` loop over the PTY master, the
//! listening socket, the attached client connection, and signals (via the
//! self-pipe in [`crate::signals`]).
//!
//! Single-threaded and lock-free by construction — one owner of the terminal
//! and the client connection. No async runtime: the fd count is small and
//! fixed, and a plain poll loop keeps backtraces and profiling honest.
//!
//! [`spawn`] daemonizes (classic double-fork) so the host outlives the launching
//! command and has no controlling terminal of its own; it just shuffles bytes
//! between the child's PTY and the attached client.

use crate::paths;
use crate::protocol::{ClientMsg, FrameReader, ServerMsg, encode};
use crate::screen::Screen;
use nix::sys::signal::Signal;
use pty_process::Size;
use pty_process::blocking::{Command as PtyCommand, open};
use rustix::event::{PollFd, PollFlags, poll};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};

/// Options for starting a session.
pub struct SpawnOpts {
    /// Session name (used for the socket and pidfile).
    pub name: String,
    /// Command and arguments to run; empty means `$SHELL` (then `/bin/sh`).
    pub command: Vec<String>,
    /// Initial terminal size as `(cols, rows)`.
    pub size: (u16, u16),
    /// Where to record the session, or `None` to not record.
    pub record: Option<std::path::PathBuf>,
    /// Bound on retained scrollback lines for resync on attach.
    pub scrollback: usize,
    /// Cap on the recording's on-disk size, or `None` for unbounded.
    pub max_recording_bytes: Option<usize>,
}

/// Lower bound on the checkpoint interval. Small recording caps need frequent
/// checkpoints to be enforceable (see [`checkpoint_interval`]); this also caps
/// how far the file can overshoot a small cap.
const MIN_CHECKPOINT_INTERVAL_BYTES: usize = 128 * 1024;
/// Upper bound on the checkpoint interval. Past this, spacing checkpoints out
/// further saves little CPU but lengthens replay-from-checkpoint.
const MAX_CHECKPOINT_INTERVAL_BYTES: usize = 2 * 1024 * 1024;

/// How many bytes of output to emit between recording checkpoints.
///
/// A checkpoint serializes and zstd-compresses the *entire* emulator state, so
/// under high-throughput output (a big `find`, `cat` of a large file) frequent
/// checkpoints dominate the host's CPU — re-compressing roughly as many bytes as
/// the output stream itself. Spacing them out cuts that cost sharply.
///
/// The interval can't simply be large, though: between checkpoints the recording
/// has no safe cut point, so it can overshoot its size cap by up to one interval
/// before compaction runs. We therefore scale the interval to the cap (a small
/// fraction of it) and clamp it — large caps get rare, cheap checkpoints; small
/// caps keep them frequent enough to stay enforceable. An unbounded recording
/// never compacts, so only replay cost matters and we use the maximum.
fn checkpoint_interval(max_bytes: Option<usize>) -> usize {
    match max_bytes {
        Some(max) => (max / 32).clamp(MIN_CHECKPOINT_INTERVAL_BYTES, MAX_CHECKPOINT_INTERVAL_BYTES),
        None => MAX_CHECKPOINT_INTERVAL_BYTES,
    }
}

/// Cap on connections awaiting classification, bounding memory against a peer
/// that connects but never sends its first message.
const MAX_PENDING: usize = 8;

enum Fork {
    Parent,
    Daemon,
}

/// A single attached client connection.
struct Client {
    stream: UnixStream,
    reader: FrameReader,
    /// Output queued for the client but not yet written (backpressure buffer).
    outbuf: Vec<u8>,
    /// Whether the initial repaint has been sent. The first thing a client
    /// sends is its size; we hold back live output and the resync until then,
    /// so the repaint is laid out at the client's real geometry and the client
    /// never sees pre-resync bytes.
    resynced: bool,
}

impl Client {
    fn new(stream: UnixStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Client {
            stream,
            reader: FrameReader::new(),
            outbuf: Vec::new(),
            resynced: false,
        })
    }

    fn queue(&mut self, msg: &ServerMsg) {
        self.outbuf.extend_from_slice(&encode(msg));
    }

    /// Write as much of the pending output as the socket will accept.
    fn flush(&mut self) -> io::Result<()> {
        while !self.outbuf.is_empty() {
            match self.stream.write(&self.outbuf) {
                Ok(0) => break,
                Ok(n) => {
                    self.outbuf.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// Start a session in the background.
///
/// Returns `Ok(())` in the calling process once the host has been forked off.
/// In the host process this function does not return — it runs the session and
/// exits the process when the session ends.
pub fn spawn(opts: SpawnOpts) -> io::Result<()> {
    // Check for a live session of this name first. `session::list` also prunes
    // dead sessions' directories, so a stale `<name>/` left by a crashed host is
    // cleared here — and crucially this runs before we create our own directory,
    // which has no pidfile yet and would otherwise be pruned as "dead".
    if crate::session::list()?.iter().any(|s| s.name == opts.name) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("session '{}' already exists", opts.name),
        ));
    }
    paths::ensure_session_dir(&opts.name)?;
    let sock = paths::socket_path(&opts.name);
    // Clear any stale socket left by a dead host, then claim the name.
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock)?;

    // Capture the caller's working directory before daemonize() chdir's to `/`,
    // so the session's child starts where `ghost new` was invoked (like dtach),
    // not at the daemon's `/`.
    let launch_dir = std::env::current_dir().ok();

    match unsafe { daemonize()? } {
        Fork::Parent => return Ok(()),
        Fork::Daemon => {}
    }

    // We are now the long-lived host; there is no caller to return errors to.
    // `current_name` may change under us if the session is renamed, so the
    // host reports the final name and we clean up its directory by that.
    let mut current_name = opts.name.clone();
    let result = host_main(&listener, &opts, launch_dir.as_deref(), &mut current_name);
    let _ = std::fs::remove_dir_all(paths::session_dir(&current_name));
    std::process::exit(result.unwrap_or(1));
}

fn host_main(
    listener: &UnixListener,
    opts: &SpawnOpts,
    launch_dir: Option<&std::path::Path>,
    current_name: &mut String,
) -> io::Result<i32> {
    std::fs::write(
        paths::pid_path(current_name),
        std::process::id().to_string(),
    )?;
    listener.set_nonblocking(true)?;

    let (pty, pts) = open().map_err(io::Error::other)?;
    let (cols, rows) = opts.size;
    pty.resize(Size::new(rows, cols))
        .map_err(io::Error::other)?;
    let (prog, args) = split_command(&opts.command);
    let mut cmd = PtyCommand::new(&prog).args(&args);
    if let Some(dir) = launch_dir {
        cmd = cmd.current_dir(dir);
    }
    let mut child = cmd.spawn(pts).map_err(io::Error::other)?;

    let sfd = crate::signals::make(&[Signal::SIGCHLD, Signal::SIGTERM, Signal::SIGINT])?;

    // Authoritative screen state, fed every byte the child writes so a late
    // attach can be repainted to the current state.
    let mut screen = Screen::new(cols, rows, opts.scrollback);

    // Optional durable recording. Best-effort: if it cannot be created, the
    // session still runs (just unrecorded).
    let mut recorder = opts.record.as_ref().and_then(|path| {
        crate::record::FileRecorder::create(
            path,
            cols,
            rows,
            &opts.command,
            opts.max_recording_bytes,
        )
        .ok()
    });
    let mut bytes_since_checkpoint = 0usize;
    let checkpoint_interval = checkpoint_interval(opts.max_recording_bytes);

    // The attached display client, plus connections that have not yet
    // identified themselves. A new connection only becomes the display client
    // (taking over from any current one) once it sends a Resize — the attach
    // handshake. A control connection (e.g. `ghost rename`) sends a Rename
    // first and is serviced without disturbing the attached client.
    let mut client: Option<Client> = None;
    let mut pending: Vec<Client> = Vec::new();
    let mut ptybuf = [0u8; 8192];

    loop {
        // Build the poll set: fixed fds first, then the display client (if any),
        // then the pending connections.
        let mut fds = vec![
            PollFd::new(&pty, PollFlags::IN),
            PollFd::new(listener, PollFlags::IN),
            PollFd::new(&sfd, PollFlags::IN),
        ];
        let client_idx = client.as_ref().map(|c| {
            let mut flags = PollFlags::IN;
            if !c.outbuf.is_empty() {
                flags |= PollFlags::OUT;
            }
            fds.push(PollFd::new(&c.stream, flags));
            fds.len() - 1
        });
        let pending_start = fds.len();
        for p in &pending {
            let mut flags = PollFlags::IN;
            if !p.outbuf.is_empty() {
                flags |= PollFlags::OUT;
            }
            fds.push(PollFd::new(&p.stream, flags));
        }
        match poll(&mut fds, None) {
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(e.into()),
        }
        let pty_re = fds[0].revents();
        let listener_re = fds[1].revents();
        let sig_re = fds[2].revents();
        let client_re = client_idx
            .map(|i| fds[i].revents())
            .unwrap_or_else(PollFlags::empty);
        let pending_re: Vec<PollFlags> = (0..pending.len())
            .map(|i| fds[pending_start + i].revents())
            .collect();
        drop(fds);

        // PTY output -> authoritative screen state, and live to the attached
        // client (if any). State is always tracked so the next attach can be
        // repainted even after a period with nobody attached.
        if pty_re.intersects(PollFlags::IN | PollFlags::HUP) {
            match (&pty).read(&mut ptybuf) {
                Ok(0) => return child_exited(&mut child, &mut client),
                Ok(n) => {
                    screen.feed(&ptybuf[..n]);
                    if let Some(r) = &mut recorder {
                        let _ = r.output(&ptybuf[..n]);
                        bytes_since_checkpoint += n;
                        if bytes_since_checkpoint >= checkpoint_interval {
                            let (c, rws) = screen.dimensions();
                            let _ = r.checkpoint(c, rws, &screen.dump());
                            bytes_since_checkpoint = 0;
                        }
                    }
                    // Live output only flows once the client has been resynced;
                    // anything before that is already captured in the resync.
                    if let Some(c) = &mut client
                        && c.resynced
                    {
                        c.queue(&ServerMsg::Output(ptybuf[..n].to_vec()));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                // EIO on the master means the child closed the slave (exited).
                Err(_) => return child_exited(&mut child, &mut client),
            }
        }

        // New connections wait in the pending pool until their first message
        // classifies them (attach vs. control). Drain the whole accept backlog.
        if listener_re.contains(PollFlags::IN) {
            while let Ok((stream, _)) = listener.accept() {
                if pending.len() < MAX_PENDING {
                    pending.push(Client::new(stream)?);
                }
                // Otherwise drop it: too many half-open connections.
            }
        }

        // Display client -> host. Read once, then process every buffered message
        // (so a Resize batched with input is fully handled, not just the first).
        if client_re.intersects(PollFlags::IN | PollFlags::HUP) {
            let mut disposition = Disposition::Keep;
            if let Some(c) = &mut client {
                let mut buf = [0u8; 4096];
                match c.stream.read(&mut buf) {
                    Ok(0) => disposition = Disposition::Drop,
                    Ok(n) => {
                        c.reader.push(&buf[..n]);
                        disposition = handle_client_messages(
                            c,
                            &pty,
                            &mut screen,
                            &mut recorder,
                            current_name,
                        )?;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(_) => disposition = Disposition::Drop,
                }
            }
            match disposition {
                Disposition::Keep => {}
                Disposition::Drop => client = None,
                Disposition::Kill => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Ok(0);
                }
            }
        }

        // Service pending connections through the same handler. A connection
        // that completes the attach handshake (a Resize, which sets `resynced`)
        // is promoted to the display client, taking over from any current one. A
        // control-only connection (e.g. `ghost rename`, which sends a Rename and
        // never resyncs) is serviced and kept until it disconnects, leaving any
        // attached client undisturbed.
        if !pending.is_empty() {
            let mut still_pending = Vec::new();
            for (i, mut p) in std::mem::take(&mut pending).into_iter().enumerate() {
                let re = pending_re.get(i).copied().unwrap_or_else(PollFlags::empty);
                if !re.intersects(PollFlags::IN | PollFlags::HUP) {
                    let _ = p.flush();
                    still_pending.push(p);
                    continue;
                }
                let mut buf = [0u8; 4096];
                let disposition = match p.stream.read(&mut buf) {
                    Ok(0) => Disposition::Drop,
                    Ok(n) => {
                        p.reader.push(&buf[..n]);
                        handle_client_messages(
                            &mut p,
                            &pty,
                            &mut screen,
                            &mut recorder,
                            current_name,
                        )?
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => Disposition::Keep,
                    Err(_) => Disposition::Drop,
                };
                match disposition {
                    Disposition::Kill => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Ok(0);
                    }
                    Disposition::Drop => {} // drop p
                    Disposition::Keep => {
                        let _ = p.flush();
                        if p.resynced {
                            client = Some(p); // attach handshake done -> display client
                        } else {
                            still_pending.push(p); // control / not yet identified
                        }
                    }
                }
            }
            pending = still_pending;
        }

        // Push queued output to the client.
        if let Some(c) = &mut client
            && c.flush().is_err()
        {
            client = None;
        }

        // Signals.
        if sig_re.contains(PollFlags::IN) {
            for signo in crate::signals::drain(&sfd)? {
                match signo {
                    // The child exiting is NOT our cue to stop: there may still
                    // be buffered output on the PTY. Exit is driven by PTY EOF
                    // (read returns 0 / EIO) once the master is fully drained,
                    // so the tail — and the recording — is complete. The child
                    // is reaped there via `child_exited`.
                    libc::SIGCHLD => {}
                    libc::SIGTERM | libc::SIGINT => {
                        let _ = child.kill();
                        let _ = child.wait();
                        notify_exit(&mut client, 0);
                        return Ok(0);
                    }
                    _ => {}
                }
            }
        }
    }
}

/// How the caller should treat a connection after processing its messages.
enum Disposition {
    /// Keep the connection.
    Keep,
    /// Drop the connection (it detached, closed, or errored).
    Drop,
    /// Kill the whole session.
    Kill,
}

/// Process every complete message buffered on `c`: write input to the PTY, apply
/// resizes, handle renames and repaints. A Resize sets `c.resynced` (and queues
/// the repaint), which is how the caller knows the connection is an attach client
/// rather than a control-only one. Returns how the connection should be treated.
fn handle_client_messages(
    c: &mut Client,
    pty: &pty_process::blocking::Pty,
    screen: &mut Screen,
    recorder: &mut Option<crate::record::FileRecorder>,
    current_name: &mut String,
) -> io::Result<Disposition> {
    loop {
        match c.reader.next_msg::<ClientMsg>() {
            Ok(Some(ClientMsg::Input(bytes))) => {
                let mut w: &pty_process::blocking::Pty = pty;
                w.write_all(&bytes)?;
            }
            Ok(Some(ClientMsg::Resize { cols, rows })) => {
                let _ = pty.resize(Size::new(rows, cols));
                screen.resize(cols, rows);
                if let Some(r) = recorder {
                    let _ = r.resize(cols, rows);
                }
                // First resize completes the attach handshake: repaint at size.
                if !c.resynced {
                    c.queue(&ServerMsg::Output(screen.resync()));
                    c.resynced = true;
                }
            }
            Ok(Some(ClientMsg::Detach)) => return Ok(Disposition::Drop),
            Ok(Some(ClientMsg::Kill)) => return Ok(Disposition::Kill),
            Ok(Some(ClientMsg::Rename(new))) => {
                let (ok, message) = match rename_session(current_name, &new, recorder) {
                    Ok(()) => (true, current_name.clone()),
                    Err(e) => (false, e),
                };
                c.queue(&ServerMsg::RenameResult { ok, message });
            }
            Ok(Some(ClientMsg::Repaint)) => {
                if c.resynced {
                    c.queue(&ServerMsg::Output(screen.resync()));
                }
            }
            Ok(None) => return Ok(Disposition::Keep),
            Err(_) => return Ok(Disposition::Drop),
        }
    }
}

/// Rename the running session: move its runtime directory (sock + pid together,
/// atomically) and its recording, updating `current_name` so cleanup targets the
/// right directory. Returns a human-readable error if the new name is invalid or
/// already taken.
fn rename_session(
    current_name: &mut String,
    new_name: &str,
    recorder: &mut Option<crate::record::FileRecorder>,
) -> Result<(), String> {
    if current_name.as_str() == new_name {
        return Ok(()); // no-op
    }
    if !crate::session::valid_name(new_name) {
        return Err(format!(
            "'{new_name}' is not a valid session name (letters, digits, '-', '_', '.')"
        ));
    }
    // Refuse to clobber a live session. `list` also prunes dead sessions, so a
    // stale directory for `new_name` is cleared and the rename can proceed.
    match crate::session::list() {
        Ok(sessions) if sessions.iter().any(|s| s.name == new_name) => {
            return Err(format!("a session named '{new_name}' already exists"));
        }
        Ok(_) => {}
        Err(e) => return Err(format!("could not check existing sessions: {e}")),
    }
    let new_dir = paths::session_dir(new_name);
    // Defensive: clear any leftover empty directory so the rename target is free.
    let _ = std::fs::remove_dir_all(&new_dir);
    std::fs::rename(paths::session_dir(current_name), &new_dir)
        .map_err(|e| format!("could not move session directory: {e}"))?;
    // The directory move is authoritative — the session is now `new_name`.
    *current_name = new_name.to_string();
    // Move the recording too. Best effort: the session itself is already renamed,
    // and discovery never depends on the recording.
    if let Some(r) = recorder {
        let _ = r.rename(&paths::recording_path(new_name));
    }
    Ok(())
}

/// Reap the child after the PTY signalled EOF and tell the client.
fn child_exited(child: &mut std::process::Child, client: &mut Option<Client>) -> io::Result<i32> {
    let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(0);
    notify_exit(client, code);
    Ok(code)
}

/// Best-effort notification to the client that the session ended.
fn notify_exit(client: &mut Option<Client>, code: i32) {
    if let Some(c) = client {
        let _ = c.flush();
        let _ = c.stream.write_all(&encode(&ServerMsg::Exited(code)));
    }
}

fn split_command(command: &[String]) -> (String, Vec<String>) {
    match command.split_first() {
        Some((first, rest)) => (first.clone(), rest.to_vec()),
        None => {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
            (shell, Vec::new())
        }
    }
}

/// Classic double-fork daemonization. Returns [`Fork::Parent`] in the original
/// process and [`Fork::Daemon`] in the detached grandchild.
unsafe fn daemonize() -> io::Result<Fork> {
    match unsafe { libc::fork() } {
        -1 => return Err(io::Error::last_os_error()),
        0 => {}
        _ => return Ok(Fork::Parent),
    }
    // New session: detach from the controlling terminal.
    rustix::process::setsid().map_err(io::Error::from)?;
    // Second fork: ensure we are not a session leader, so we can never reacquire
    // a controlling terminal.
    match unsafe { libc::fork() } {
        -1 => return Err(io::Error::last_os_error()),
        0 => {}
        _ => std::process::exit(0),
    }
    let _ = std::env::set_current_dir("/");
    unsafe { libc::umask(0) };
    if let Ok(devnull) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
    {
        let nfd = devnull.as_raw_fd();
        unsafe {
            libc::dup2(nfd, 0);
            libc::dup2(nfd, 1);
            libc::dup2(nfd, 2);
        }
    }
    Ok(Fork::Daemon)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::DEFAULT_MAX_RECORDING_BYTES;

    #[test]
    fn checkpoint_interval_scales_with_cap_and_clamps() {
        // The default cap yields the maximum interval (rare, cheap checkpoints).
        assert_eq!(
            checkpoint_interval(Some(DEFAULT_MAX_RECORDING_BYTES)),
            MAX_CHECKPOINT_INTERVAL_BYTES
        );
        // An unbounded recording never compacts, so it also uses the maximum.
        assert_eq!(checkpoint_interval(None), MAX_CHECKPOINT_INTERVAL_BYTES);

        // A tiny cap clamps up to the minimum so checkpoints stay frequent
        // enough to keep the cap enforceable.
        assert_eq!(
            checkpoint_interval(Some(256 * 1024)),
            MIN_CHECKPOINT_INTERVAL_BYTES
        );
        assert_eq!(checkpoint_interval(Some(0)), MIN_CHECKPOINT_INTERVAL_BYTES);

        // A mid-range cap scales linearly (1/32 of the cap) between the bounds.
        assert_eq!(checkpoint_interval(Some(32 * 1024 * 1024)), 1024 * 1024);

        // The interval is always a small fraction of the cap (bounding overshoot)
        // except where the minimum floor lifts it, and never exceeds the cap.
        for &mb in &[1usize, 4, 8, 16, 32, 64, 128, 512] {
            let cap = mb * 1024 * 1024;
            let interval = checkpoint_interval(Some(cap));
            assert!(interval >= MIN_CHECKPOINT_INTERVAL_BYTES);
            assert!(interval <= MAX_CHECKPOINT_INTERVAL_BYTES);
            assert!(
                interval <= cap,
                "interval {interval} exceeds cap {cap}, cap unenforceable"
            );
        }
    }
}
