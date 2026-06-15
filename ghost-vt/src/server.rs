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
    paths::ensure_runtime_dir()?;
    let sock = paths::socket_path(&opts.name);
    let pidf = paths::pid_path(&opts.name);

    if crate::session::list()?.iter().any(|s| s.name == opts.name) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("session '{}' already exists", opts.name),
        ));
    }
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
    let result = host_main(&listener, &opts, launch_dir.as_deref());
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(&pidf);
    std::process::exit(result.unwrap_or(1));
}

fn host_main(
    listener: &UnixListener,
    opts: &SpawnOpts,
    launch_dir: Option<&std::path::Path>,
) -> io::Result<i32> {
    std::fs::write(paths::pid_path(&opts.name), std::process::id().to_string())?;
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

    let mut client: Option<Client> = None;
    let mut ptybuf = [0u8; 8192];

    loop {
        // Build the poll set (client slot only when one is attached).
        let mut fds = vec![
            PollFd::new(&pty, PollFlags::IN),
            PollFd::new(listener, PollFlags::IN),
            PollFd::new(&sfd, PollFlags::IN),
        ];
        if let Some(c) = &client {
            let mut flags = PollFlags::IN;
            if !c.outbuf.is_empty() {
                flags |= PollFlags::OUT;
            }
            fds.push(PollFd::new(&c.stream, flags));
        }
        match poll(&mut fds, None) {
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(e.into()),
        }
        let pty_re = fds[0].revents();
        let listener_re = fds[1].revents();
        let sig_re = fds[2].revents();
        let client_re = if client.is_some() {
            fds[3].revents()
        } else {
            PollFlags::empty()
        };
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

        // New connection: the latest attach takes over. The repaint is deferred
        // until the client reports its size (its first message), so it is laid
        // out at the right geometry.
        if listener_re.contains(PollFlags::IN)
            && let Ok((stream, _)) = listener.accept()
        {
            client = Some(Client::new(stream)?);
        }

        // Client -> host (input and control messages).
        if client_re.intersects(PollFlags::IN | PollFlags::HUP) {
            let mut drop_client = false;
            if let Some(c) = &mut client {
                let mut buf = [0u8; 4096];
                match c.stream.read(&mut buf) {
                    Ok(0) => drop_client = true,
                    Ok(n) => {
                        c.reader.push(&buf[..n]);
                        loop {
                            match c.reader.next_msg::<ClientMsg>() {
                                Ok(Some(ClientMsg::Input(bytes))) => {
                                    (&pty).write_all(&bytes)?;
                                }
                                Ok(Some(ClientMsg::Resize { cols, rows })) => {
                                    let _ = pty.resize(Size::new(rows, cols));
                                    screen.resize(cols, rows);
                                    if let Some(r) = &mut recorder {
                                        let _ = r.resize(cols, rows);
                                    }
                                    // The first resize completes the attach
                                    // handshake: repaint at the now-known size.
                                    if !c.resynced {
                                        c.queue(&ServerMsg::Output(screen.resync()));
                                        c.resynced = true;
                                    }
                                }
                                Ok(Some(ClientMsg::Detach)) => {
                                    drop_client = true;
                                    break;
                                }
                                Ok(Some(ClientMsg::Kill)) => {
                                    let _ = child.kill();
                                    let _ = child.wait();
                                    return Ok(0);
                                }
                                Ok(None) => break,
                                Err(_) => {
                                    drop_client = true;
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(_) => drop_client = true,
                }
            }
            if drop_client {
                client = None;
            }
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
