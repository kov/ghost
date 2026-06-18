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
use crate::protocol::{ClientMsg, ServerMsg};
use crate::screen::Screen;
use crate::transport::Conn;
use nix::sys::signal::Signal;
use pty_process::Size;
use pty_process::blocking::{Command as PtyCommand, open};
use rustix::event::{PollFd, PollFlags, poll};
use rustix::fs::{FlockOperation, flock};
use serde::{Deserialize, Serialize};
use std::ffi::{CStr, CString};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::{SystemTime, UNIX_EPOCH};

/// Options for starting a session.
#[derive(Serialize, Deserialize)]
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
    /// Defer spawning the child until the first attach handshake, instead of at
    /// session start. A session that will be attached to (the CLI's default
    /// `ghost new`, and every GUI session) sets this so the child's startup
    /// terminal queries reach a real display client; a plain detached session
    /// (`ghost new -d`) leaves it `false` and starts the child eagerly.
    pub start_on_attach: bool,
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

/// The hidden argv marker that selects host mode: `ghost __host <fd> <blob>`.
/// Not a documented subcommand — an internal handoff used by [`spawn`]'s re-exec
/// and recognized by [`run_host_if_invoked`].
const HOST_ARG: &str = "__host";

/// A single attached client connection: the framed [`Conn`] plus a little
/// attach state.
struct Client {
    conn: Conn,
    /// Whether the initial repaint has been sent. The first thing a client
    /// sends is its size; we hold back live output and the resync until then,
    /// so the repaint is laid out at the client's real geometry and the client
    /// never sees pre-resync bytes.
    resynced: bool,
}

impl Client {
    fn new(stream: UnixStream) -> io::Result<Self> {
        let conn = Conn::new(stream);
        conn.set_nonblocking(true)?;
        Ok(Client {
            conn,
            resynced: false,
        })
    }

    fn queue(&mut self, msg: &ServerMsg) {
        self.conn.queue(msg);
    }

    /// Write as much of the pending output as the socket will accept.
    fn flush(&mut self) -> io::Result<()> {
        self.conn.flush()
    }
}

/// The host's startup state, serialized onto argv across the re-exec.
#[derive(Serialize, Deserialize)]
struct HostArgs {
    opts: SpawnOpts,
    /// The directory the session was launched from, applied to the child (like
    /// dtach) since the daemon itself `chdir`s to `/`.
    launch_dir: Option<std::path::PathBuf>,
}

/// Start a session in the background.
///
/// Returns `Ok(())` in the calling process once the host has been forked off and
/// re-exec'd. The host runs in that separate, re-exec'd process — never in the
/// caller — so this is safe to call even from a multithreaded process such as a
/// GUI front-end.
pub fn spawn(opts: SpawnOpts) -> io::Result<()> {
    paths::ensure_session_dir(&opts.name)?;

    // Acquire the session's lifetime lock. Held by the host across the fork+exec
    // and for its whole life (the kernel releases it on exit or crash), this lock
    // is the single source of truth for liveness: `session::list` prunes a
    // directory exactly when its lock is free. Taking it here also *is* the atomic
    // "already exists" check — a live host of this name still holds it. We create
    // it before binding the socket so a session is never observable without it.
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(paths::lock_path(&opts.name))?;
    match flock(&lock, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {}
        Err(e) if e == rustix::io::Errno::WOULDBLOCK => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("session '{}' already exists", opts.name),
            ));
        }
        Err(e) => return Err(io::Error::from(e)),
    }

    let sock = paths::socket_path(&opts.name);
    // Clear any stale socket left by a dead host, then claim the name.
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock)?;

    // The host runs in a re-exec'd process (see `run_host_if_invoked`) so it gets
    // a fresh, single-threaded address space — what makes spawning safe from a
    // multithreaded process, and what sheds any inherited heap/fds. We hand it its
    // state on argv: the bound listener fd and the held lock fd (both kept open
    // across the exec by clearing CLOEXEC) and the serialized spawn options.
    // Everything that allocates — capturing the cwd, serialization, building the
    // argv `CString`s — happens here in the parent, *before* the fork, so the path
    // from fork to `execv` touches only async-signal-safe syscalls.
    clear_cloexec(&listener)?;
    clear_cloexec(&lock)?;
    let listener_fd = listener.as_raw_fd();
    let lock_fd = lock.as_raw_fd();

    let host_args = HostArgs {
        launch_dir: std::env::current_dir().ok(),
        opts,
    };
    let blob = encode_host_args(&host_args);

    let exe = std::env::current_exe()?;
    let exe_c = CString::new(exe.as_os_str().as_bytes())
        .map_err(|_| io::Error::other("executable path contains a NUL byte"))?;
    let argv_owned = [
        exe_c.clone(),
        CString::new(HOST_ARG).expect("HOST_ARG has no NUL"),
        CString::new(listener_fd.to_string()).expect("fd digits have no NUL"),
        CString::new(lock_fd.to_string()).expect("fd digits have no NUL"),
        CString::new(blob).expect("hex blob has no NUL"),
    ];
    let mut argv: Vec<*const libc::c_char> = argv_owned.iter().map(|c| c.as_ptr()).collect();
    argv.push(std::ptr::null());

    // Daemonize and exec the host. Returns here only in the original process; the
    // daemonized grandchild execs `exe_c` and never returns. `argv_owned`,
    // `listener`, and `lock` must outlive the call (the forked child reads them up
    // to the exec); they drop here in the parent. The parent dropping its `lock`
    // copy does not release the flock — the host's inherited copy keeps it held.
    unsafe { daemonize_and_exec(&exe_c, &argv) }
}

/// If this process was re-exec'd as a session host (`__host <fd> <blob>` on
/// argv), run the host to completion and exit; otherwise return so normal
/// startup continues. Any binary that links `ghost-vt` and may [`spawn`]
/// sessions must call this first thing in `main()`.
pub fn run_host_if_invoked() {
    let mut args = std::env::args_os();
    let _argv0 = args.next();
    if args.next().as_deref() != Some(HOST_ARG.as_ref()) {
        return;
    }
    std::process::exit(run_host(args.next(), args.next(), args.next()));
}

/// The host-mode body: reclaim the passed listener, liveness lock, and spawn
/// options, run the session loop, clean up its directory, and return the exit
/// code.
fn run_host(
    listener_arg: Option<std::ffi::OsString>,
    lock_arg: Option<std::ffi::OsString>,
    blob: Option<std::ffi::OsString>,
) -> i32 {
    let parsed = (|| -> Option<(RawFd, RawFd, HostArgs)> {
        let listener_fd: RawFd = listener_arg?.to_str()?.parse().ok()?;
        let lock_fd: RawFd = lock_arg?.to_str()?.parse().ok()?;
        Some((listener_fd, lock_fd, decode_host_args(blob?.to_str()?)?))
    })();
    let Some((listener_fd, lock_fd, host_args)) = parsed else {
        return 127; // malformed __host handoff — an internal bug
    };
    // Hold the inherited liveness lock for our whole life. The parent took the
    // flock before forking and it survived the exec; keeping this fd open keeps
    // the lock held, and the kernel frees it when we exit or crash — which is how
    // `session::list` knows we are gone.
    // SAFETY: a fd the parent passed us with CLOEXEC cleared; we own it now.
    let _lock = unsafe { OwnedFd::from_raw_fd(lock_fd) };
    // SAFETY: the listening socket the parent bound and passed with CLOEXEC cleared.
    let listener = unsafe { UnixListener::from_raw_fd(listener_fd) };
    let HostArgs { opts, launch_dir } = host_args;
    // `current_name` may change under us if the session is renamed, so the host
    // reports the final name and cleans up its directory by that.
    let mut current_name = opts.name.clone();
    let result = host_main(&listener, &opts, launch_dir.as_deref(), &mut current_name);
    let _ = std::fs::remove_dir_all(paths::session_dir(&current_name));
    result.unwrap_or(1)
}

/// Clear `FD_CLOEXEC` so a descriptor survives `execv` into the host process.
fn clear_cloexec(fd: &impl AsRawFd) -> io::Result<()> {
    let raw = fd.as_raw_fd();
    // SAFETY: plain fcntl on a fd we own.
    unsafe {
        let flags = libc::fcntl(raw, libc::F_GETFD);
        if flags == -1 {
            return Err(io::Error::last_os_error());
        }
        if libc::fcntl(raw, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Serialize the host's startup state to a NUL-free, argv-safe hex string.
fn encode_host_args(args: &HostArgs) -> String {
    let bytes = postcard::to_allocvec(args).expect("postcard encoding cannot fail");
    to_hex(&bytes)
}

/// Inverse of [`encode_host_args`]; `None` if the blob is malformed.
fn decode_host_args(hex: &str) -> Option<HostArgs> {
    postcard::from_bytes(&from_hex(hex)?).ok()
}

fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

fn from_hex(s: &str) -> Option<Vec<u8>> {
    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    fn nibble(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            _ => None,
        }
    }
    bytes
        .chunks_exact(2)
        .map(|p| Some((nibble(p[0])? << 4) | nibble(p[1])?))
        .collect()
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

    // The child is started eagerly for a plain detached session, or deferred
    // until the first attach handshake (see `SpawnOpts::start_on_attach`). While
    // deferred we hold the slave (`pts`) so the PTY master never sees EOF and the
    // poll loop just idles until a client attaches.
    let mut pts = Some(pts);
    let mut child: Option<std::process::Child> = None;
    if !opts.start_on_attach {
        child = Some(spawn_child(
            &opts.command,
            launch_dir,
            pts.take().expect("slave present before first spawn"),
        )?);
    }

    let sfd = crate::signals::make(&[Signal::SIGCHLD, Signal::SIGTERM, Signal::SIGINT])?;

    // Authoritative screen state, fed every byte the child writes so a late
    // attach can be repainted to the current state.
    let mut screen = Screen::new(cols, rows, opts.scrollback);

    // Descriptive metadata for discovery (the GUI sidebar). Created time and
    // command are fixed; the title is refreshed below whenever it changes.
    let mut meta = crate::meta::Meta {
        created_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
        command: opts.command.clone(),
        title: String::new(),
    };
    let _ = crate::meta::write(&paths::meta_path(current_name), &meta);

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
            if c.conn.wants_write() {
                flags |= PollFlags::OUT;
            }
            fds.push(PollFd::from_borrowed_fd(c.conn.as_fd(), flags));
            fds.len() - 1
        });
        let pending_start = fds.len();
        for p in &pending {
            let mut flags = PollFlags::IN;
            if p.conn.wants_write() {
                flags |= PollFlags::OUT;
            }
            fds.push(PollFd::from_borrowed_fd(p.conn.as_fd(), flags));
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
                    // Refresh the discoverable title when the child changes it
                    // (coalesced — only an actual change rewrites the meta file).
                    if screen.title() != meta.title {
                        meta.title = screen.title().to_string();
                        let _ = crate::meta::write(&paths::meta_path(current_name), &meta);
                    }
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
                match c.conn.recv::<ClientMsg>() {
                    Ok(None) => disposition = Disposition::Drop,
                    Ok(Some(msgs)) => {
                        disposition = handle_client_messages(
                            c,
                            msgs,
                            &pty,
                            &mut screen,
                            &mut recorder,
                            current_name,
                        )?;
                    }
                    Err(_) => disposition = Disposition::Drop,
                }
            }
            match disposition {
                Disposition::Keep => {}
                Disposition::Drop => client = None,
                Disposition::Kill => {
                    kill_child(&mut child);
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
                let disposition = match p.conn.recv::<ClientMsg>() {
                    Ok(None) => Disposition::Drop,
                    Ok(Some(msgs)) => handle_client_messages(
                        &mut p,
                        msgs,
                        &pty,
                        &mut screen,
                        &mut recorder,
                        current_name,
                    )?,
                    Err(_) => Disposition::Drop,
                };
                match disposition {
                    Disposition::Kill => {
                        kill_child(&mut child);
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

        // Deferred start: the first client to complete the attach handshake
        // (which sets `resynced`) brings the child to life, so its startup
        // queries are emitted with a display client already attached to answer
        // them. Eager sessions already have a child, so this never fires.
        if child.is_none() && client.as_ref().is_some_and(|c| c.resynced) {
            child = Some(spawn_child(
                &opts.command,
                launch_dir,
                pts.take()
                    .expect("slave present until the deferred child spawns"),
            )?);
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
                        kill_child(&mut child);
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

/// Process a batch of decoded client messages: write input to the PTY, apply
/// resizes, handle renames and repaints. A Resize sets `c.resynced` (and queues
/// the repaint), which is how the caller knows the connection is an attach client
/// rather than a control-only one. Returns how the connection should be treated.
fn handle_client_messages(
    c: &mut Client,
    msgs: Vec<ClientMsg>,
    pty: &pty_process::blocking::Pty,
    screen: &mut Screen,
    recorder: &mut Option<crate::record::FileRecorder>,
    current_name: &mut String,
) -> io::Result<Disposition> {
    for msg in msgs {
        match msg {
            ClientMsg::Input(bytes) => {
                let mut w: &pty_process::blocking::Pty = pty;
                w.write_all(&bytes)?;
            }
            ClientMsg::Resize { cols, rows } => {
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
            ClientMsg::Detach => return Ok(Disposition::Drop),
            ClientMsg::Kill => return Ok(Disposition::Kill),
            ClientMsg::Rename(new) => {
                let (ok, message) = match rename_session(current_name, &new, recorder) {
                    Ok(()) => (true, current_name.clone()),
                    Err(e) => (false, e),
                };
                c.queue(&ServerMsg::RenameResult { ok, message });
            }
            ClientMsg::Repaint => {
                if c.resynced {
                    c.queue(&ServerMsg::Output(screen.resync()));
                }
            }
        }
    }
    Ok(Disposition::Keep)
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

/// Reap the child after the PTY signalled EOF and tell the client. The child is
/// always present here — EOF can only follow a spawn — but it is threaded as an
/// `Option` because the session may not have spawned its child yet.
fn child_exited(
    child: &mut Option<std::process::Child>,
    client: &mut Option<Client>,
) -> io::Result<i32> {
    let code = child
        .as_mut()
        .and_then(|c| c.wait().ok())
        .and_then(|s| s.code())
        .unwrap_or(0);
    notify_exit(client, code);
    Ok(code)
}

/// Build and spawn the session's child on the given PTY slave, honoring the
/// launch directory. Shared by eager start and deferred (first-attach) start.
fn spawn_child(
    command: &[String],
    launch_dir: Option<&std::path::Path>,
    pts: pty_process::blocking::Pts,
) -> io::Result<std::process::Child> {
    let (prog, args) = split_command(command);
    let mut cmd = PtyCommand::new(&prog).args(&args);
    if let Some(dir) = launch_dir {
        cmd = cmd.current_dir(dir);
    }
    cmd.spawn(pts).map_err(io::Error::other)
}

/// Kill and reap the child if one has been spawned; a no-op for a deferred
/// session whose child never started.
fn kill_child(child: &mut Option<std::process::Child>) {
    if let Some(c) = child {
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// Best-effort notification to the client that the session ended.
fn notify_exit(client: &mut Option<Client>, code: i32) {
    if let Some(c) = client {
        let _ = c.conn.send(&ServerMsg::Exited(code));
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

/// Classic double-fork daemonization, then `execv` the host.
///
/// Returns `Ok(())` only in the original process. The daemonized grandchild
/// execs `exe` with `argv` and never returns; on any failure it `_exit`s. From
/// the first fork to the exec only async-signal-safe syscalls run — no
/// allocation, no env access — so this is safe to call from a multithreaded
/// process (where a fork that ran arbitrary code could deadlock on an inherited
/// lock). All the argv setup is done by the caller before the fork.
///
/// # Safety
/// `argv` must be NUL-terminated and its pointers (and `exe`) must stay valid for
/// the duration of the call.
unsafe fn daemonize_and_exec(exe: &CStr, argv: &[*const libc::c_char]) -> io::Result<()> {
    match unsafe { libc::fork() } {
        -1 => return Err(io::Error::last_os_error()),
        0 => {}
        _ => return Ok(()), // original process
    }
    // --- async-signal-safe only, from here until execv (or _exit) ---
    unsafe {
        // New session: detach from the controlling terminal.
        if libc::setsid() == -1 {
            libc::_exit(127);
        }
        // Second fork: never a session leader, so we can't reacquire a terminal.
        match libc::fork() {
            -1 => libc::_exit(127),
            0 => {}
            _ => libc::_exit(0),
        }
        libc::chdir(c"/".as_ptr());
        libc::umask(0);
        let nfd = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
        if nfd >= 0 {
            libc::dup2(nfd, 0);
            libc::dup2(nfd, 1);
            libc::dup2(nfd, 2);
            if nfd > 2 {
                libc::close(nfd);
            }
        }
        // Replace this image with the host. Only returns on failure.
        libc::execv(exe.as_ptr(), argv.as_ptr());
        libc::_exit(127);
    }
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
