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
    /// Where to start the child, overriding the spawner's own current
    /// directory (the default). What lets a recreate put a session's
    /// successor back in its predecessor's directory.
    pub cwd: Option<std::path::PathBuf>,
    /// Where to record the session, or `None` to not record.
    pub record: Option<std::path::PathBuf>,
    /// A predecessor's recording to seed this session's screen from (a
    /// recreate): the host starts with that recording's final screen and
    /// scrollback already in place, and the new child's output continues
    /// below it. Read before `record` is created, so seeding a session from
    /// its own name's previous recording works.
    pub seed_from: Option<std::path::PathBuf>,
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

/// Cap on an observer's outbound backlog. Past this the host stops queueing
/// live output for it (state events still flow — they are small and bounded)
/// and re-seeds it with a resync when it drains, so a slow or stalled observer
/// costs bounded memory instead of buffering a `cat bigfile` per watcher.
const OBSERVER_MAX_PENDING: usize = 256 * 1024;

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
    /// Whether the connection subscribed to state events (`ClientMsg::Subscribe`).
    /// A subscriber is not a display client: it is answered with a snapshot and
    /// kept, but never promoted, never resized, never resynced.
    subscribed: bool,
    /// Whether the subscription also receives output (`ClientMsg::Observe`):
    /// a read-only mirror of the session as the display client shapes it.
    observing: bool,
    /// An observer whose outbound backlog crossed the cap: live output is
    /// being dropped for it, and once it drains it is re-seeded with a fresh
    /// resync — the host holds authoritative state, so a mirror can always be
    /// rebuilt rather than buffered toward.
    lagged: bool,
    /// The connection's self-reported identity (`ClientMsg::Hello`), surfaced
    /// through [`AttachInfo`] when this connection holds the display.
    hello: Option<String>,
}

impl Client {
    fn new(stream: UnixStream) -> io::Result<Self> {
        let conn = Conn::new(stream);
        conn.set_nonblocking(true)?;
        Ok(Client {
            conn,
            resynced: false,
            subscribed: false,
            observing: false,
            lagged: false,
            hello: None,
        })
    }

    fn queue(&mut self, msg: &ServerMsg) {
        self.conn.queue(msg);
    }

    /// Queue raw output, split so each frame stays under the protocol's size cap.
    /// A resync re-emitting images can exceed it; splitting raw output at any byte
    /// boundary is safe (the client concatenates and feeds its parser).
    fn queue_output(&mut self, bytes: Vec<u8>) {
        for chunk in crate::protocol::output_chunks(&bytes) {
            self.conn.queue(&ServerMsg::Output(chunk.to_vec()));
        }
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

    // Declare which protocol feature level this host speaks, so clients built
    // later know which optional messages are safe to send it. Written before
    // the socket binds so no client can connect without seeing it.
    std::fs::write(
        paths::proto_path(&opts.name),
        crate::protocol::PROTO_LEVEL.to_string(),
    )?;

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
    // The name is the session's immutable identity: a rename only changes the
    // display-name label in `meta`, so files never move and cleanup always
    // targets the spawn-time directory.
    let result = host_main(&listener, &opts, launch_dir.as_deref(), &opts.name);
    let _ = std::fs::remove_dir_all(paths::session_dir(&opts.name));
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
    current_name: &str,
) -> io::Result<i32> {
    // An explicit cwd (a recreate) beats where the spawning process happened
    // to run; everything below sees only the effective launch directory.
    let launch_dir = opts.cwd.as_deref().or(launch_dir);
    // Funnel signals FIRST — before the pid file exists. The pid file is what
    // `ghost kill` SIGTERMs, so from the moment it is written we must die
    // gracefully: a kill racing the rest of this setup used to hit the default
    // disposition and drop the host on the spot, before the recorder below was
    // ever created — leaving no recording. With the handler installed the
    // signal just queues on the self-pipe until the poll loop drains it, after
    // the recorder exists (its drop flushes the file even on that first turn).
    let sfd = crate::signals::make(&[Signal::SIGCHLD, Signal::SIGTERM, Signal::SIGINT])?;
    std::fs::write(
        paths::pid_path(current_name),
        std::process::id().to_string(),
    )?;
    listener.set_nonblocking(true)?;

    let (pty, pts) = open().map_err(io::Error::other)?;
    let (cols, rows) = opts.size;
    pty.resize(Size::new(rows, cols))
        .map_err(io::Error::other)?;

    // Descriptive metadata for discovery (the GUI sidebar). Created time and
    // command are fixed; the title is refreshed below whenever it changes.
    // Built before the child can spawn: the spawn also writes the durable
    // descriptor, which carries these facts.
    let mut meta = crate::meta::Meta {
        // Milliseconds, not seconds: this is the fleet's spatial sort key, so
        // sub-second resolution keeps sessions spawned in the same second in their
        // true creation order rather than tie-breaking by name.
        created_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0),
        command: opts.command.clone(),
        title: String::new(),
        display_name: String::new(),
        size: opts.size,
    };
    let _ = crate::meta::write(&paths::meta_path(current_name), &meta);

    // The child is started eagerly for a plain detached session, or deferred
    // until the first attach handshake (see `SpawnOpts::start_on_attach`). While
    // deferred we hold the slave (`pts`) so the PTY master never sees EOF and the
    // poll loop just idles until a client attaches.
    let mut pts = Some(pts);
    let mut child: Option<std::process::Child> = None;
    // The last cwd written to the durable descriptor, so refreshes only touch
    // the file when the child actually moved.
    let mut desc_cwd: Option<std::path::PathBuf> = None;
    if !opts.start_on_attach {
        child = Some(spawn_child(
            &opts.command,
            launch_dir,
            pts.take().expect("slave present before first spawn"),
        )?);
        desc_cwd = child_cwd(&child).or_else(|| launch_dir.map(Into::into));
        write_descriptor(current_name, &meta, desc_cwd.clone());
    }

    // Authoritative screen state, fed every byte the child writes so a late
    // attach can be repainted to the current state. A seeded spawn (a
    // recreate) starts from its predecessor's recording instead of blank:
    // read it NOW, before the recorder below truncates the (typically same)
    // path, and reflow the restored state to this session's grid.
    let mut screen = match opts.seed_from.as_ref().and_then(|p| {
        crate::record::read(p)
            .map(|rec| Screen::from_recording(&rec, opts.scrollback))
            .ok()
    }) {
        Some(mut seeded) => {
            seeded.resize(cols, rows);
            seeded
        }
        None => Screen::new(cols, rows, opts.scrollback),
    };

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
    // A seeded session's recording must stand alone: open it with a checkpoint
    // of the inherited state, so replaying it never needs the predecessor's
    // file (which this recording typically just replaced on disk).
    if opts.seed_from.is_some()
        && let Some(r) = &mut recorder
    {
        let (c, rws) = screen.dimensions();
        let dump = screen.dump_without_images();
        let imgs = screen.graphics_images();
        let _ = r.checkpoint_with_images(c, rws, &dump, &imgs);
    }
    let mut bytes_since_checkpoint = 0usize;
    // Whether any viewport row changed since the last checkpoint we wrote. A
    // checkpoint is a whole-state dump, so writing one for a screen that hasn't
    // visibly changed is pure waste; this lets us skip it (see the trigger below).
    let mut dirty_since_checkpoint = false;
    let checkpoint_interval = checkpoint_interval(opts.max_recording_bytes);

    // The attached display client, plus connections that have not yet
    // identified themselves. A new connection only becomes the display client
    // (taking over from any current one) once it sends a Resize — the attach
    // handshake. A control connection (e.g. `ghost rename`) sends a Rename
    // first and is serviced without disturbing the attached client.
    let mut client: Option<Client> = None;
    let mut pending: Vec<Client> = Vec::new();
    // State subscribers (`ClientMsg::Subscribe`): long-lived observer
    // connections that are pushed a snapshot on subscribe (and, later, state
    // events). Kept apart from `pending` so watchers never crowd out the
    // half-open connection budget.
    let mut subscribers: Vec<Client> = Vec::new();
    // Mirrors whether a display client is attached into the `attached` marker
    // file, so discovery can report it. Tracked here to touch the filesystem only
    // on the attach/detach transitions, not every loop turn.
    let mut attached_marked = false;
    // Bell count last reflected into the marker, so a fresh ring is spotted by a
    // change rather than re-touching the filesystem every loop turn.
    let mut last_bell_count = 0u64;
    // Mirrors the bell marker's presence so a snapshot never has to stat it.
    let mut bell_marked = false;
    // The state the subscribers were last told about; each loop turn ends by
    // diffing the current state against it and pushing the deltas as events.
    // Dual-written with the marker files, which stay authoritative for polling
    // clients during the migration.
    let mut last_state = crate::protocol::SessionState::default();
    // The grid the subscribers last saw. A change (the display client resized
    // the PTY) re-grids every observer's mirror, with a resync to re-seed it —
    // a reflow cannot be patched from outside.
    let mut last_grid = screen.dimensions();
    let mut ptybuf = [0u8; 8192];
    // Spots the child's terminal queries so the host can answer them while no
    // client is attached to do so (kept fed every chunk to track split sequences).
    let mut queries = crate::query::QueryScanner::new();
    // The last theme a client reported (ClientMsg::Theme); detached color
    // queries answer with it. Ghost's default scheme until someone attaches.
    let mut last_theme = crate::query::ThemeColors::default();

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
        let subs_start = fds.len();
        for s in &subscribers {
            let mut flags = PollFlags::IN;
            if s.conn.wants_write() {
                flags |= PollFlags::OUT;
            }
            fds.push(PollFd::from_borrowed_fd(s.conn.as_fd(), flags));
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
        let subs_re: Vec<PollFlags> = (0..subscribers.len())
            .map(|i| fds[subs_start + i].revents())
            .collect();
        drop(fds);

        // Who holds the display right now, as a snapshot answer. Computed once
        // per turn, before any connection is serviced.
        let attached_info =
            client
                .as_ref()
                .filter(|c| c.resynced)
                .map(|c| crate::protocol::AttachInfo {
                    client: c.hello.clone(),
                });

        // Turn-local facts for the end-of-turn event push: a bell is an
        // occurrence, not a state (the diff below can't see one that rang and
        // was witnessed within the same turn), and activity is any output.
        let mut rang = false;
        let mut activity = false;

        // PTY output -> authoritative screen state, and live to the attached
        // client (if any). State is always tracked so the next attach can be
        // repainted even after a period with nobody attached.
        if pty_re.intersects(PollFlags::IN | PollFlags::HUP) {
            match (&pty).read(&mut ptybuf) {
                Ok(0) => return child_exited(&mut child, &mut client),
                Ok(n) => {
                    activity = n > 0;
                    // `feed` reports the rows it changed; an empty set means this
                    // output was non-rendering (a query, a mode toggle, pen
                    // changes) and left the visible screen untouched.
                    dirty_since_checkpoint |= !screen.feed(&ptybuf[..n]).is_empty();
                    // A ground-state BEL that rings while nobody is attached is an
                    // unseen notification: mark it so a front-end can highlight the
                    // session. Bells seen while a client is attached are witnessed
                    // live, so they need no marker (it would only clear on the next
                    // attach anyway).
                    let bells = screen.bell_count();
                    if bells != last_bell_count {
                        last_bell_count = bells;
                        rang = true;
                        if !client.as_ref().is_some_and(|c| c.resynced) {
                            set_bell_marker(current_name, true);
                            bell_marked = true;
                        }
                    }
                    // Answer the child's terminal queries while detached. When a
                    // client is attached it answers them itself (the query is
                    // forwarded as live output below), so the host stays out of
                    // the way to avoid a doubled reply. The scanner is always fed,
                    // attached or not, so split sequences stay tracked.
                    let asked = queries.scan(&ptybuf[..n]);
                    if client.is_none() && !asked.is_empty() {
                        let mode_state = |m: u16| screen.vt().dec_mode_state(m);
                        let ctx = crate::query::ReplyCtx {
                            cursor: screen.cursor(),
                            size: screen.dimensions(),
                            kitty_flags: screen.kitty_keyboard_flags(),
                            cursor_style: crate::query::decscusr_digit(screen.vt().cursor().shape),
                            // Detached, nobody sees the live scheme; answer
                            // with the last-attached client's colors (ghost's
                            // default if none ever attached), under any
                            // app-set dynamic overrides.
                            colors: screen.effective_colors(last_theme),
                            mode_state: &mode_state,
                        };
                        let mut reply = Vec::new();
                        for q in asked {
                            reply.extend_from_slice(&q.reply(&ctx));
                        }
                        let mut w: &pty_process::blocking::Pty = &pty;
                        let _ = w.write_all(&reply);
                    }
                    // kitty graphics acknowledgements are stateful, so they come
                    // from the emulator rather than the scanner. Drain them every
                    // feed (so they never accumulate) but, like the queries above,
                    // only write them to the child while detached — an attached,
                    // graphics-capable outer terminal answers via the pipe.
                    let graphics_reply = screen.take_graphics_responses();
                    if client.is_none() && !graphics_reply.is_empty() {
                        let mut w: &pty_process::blocking::Pty = &pty;
                        let _ = w.write_all(&graphics_reply);
                    }
                    // OSC 52 clipboard writes are applied by an attached
                    // frontend (which feeds its own emulator from the same
                    // stream); the host just drains its copy so they never
                    // accumulate — detached, there is no clipboard to write.
                    let _ = screen.take_clipboard_writes();
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
                            if dirty_since_checkpoint {
                                let (c, rws) = screen.dimensions();
                                // Bake images into the recording via content-
                                // addressed dedup: store the transmit-free dump
                                // plus references to the graphics images (each
                                // unique image stored once).
                                let dump = screen.dump_without_images();
                                let imgs = screen.graphics_images();
                                let _ = r.checkpoint_with_images(c, rws, &dump, &imgs);
                                dirty_since_checkpoint = false;
                            }
                            // Same cadence: keep the durable descriptor's cwd
                            // current (a cheap /proc readlink; only an actual
                            // move rewrites the file).
                            if let Some(cwd) = child_cwd(&child)
                                && desc_cwd.as_ref() != Some(&cwd)
                            {
                                crate::descriptor::set_cwd(current_name, &cwd);
                                desc_cwd = Some(cwd);
                            }
                            // Reset the budget whether or not we wrote: a screen
                            // unchanged since the last checkpoint waits another full
                            // interval before reconsidering, so a flood of non-
                            // rendering bytes never forces repeated whole-state
                            // dumps. Replay stays correct — the intervening output
                            // frames reconstruct the state from the prior checkpoint.
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
                    // ...and to every read-only observer (their resync was
                    // queued when they observed, so the stream is contiguous).
                    // A slow observer is capped, not buffered toward: past the
                    // backlog cap its live output is dropped and it is marked
                    // lagged; a fresh resync re-seeds it once it drains.
                    for s in subscribers.iter_mut().filter(|s| s.observing) {
                        if s.lagged {
                            continue;
                        }
                        if s.conn.pending() > OBSERVER_MAX_PENDING {
                            s.lagged = true;
                            continue;
                        }
                        s.queue(&ServerMsg::Output(ptybuf[..n].to_vec()));
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
                            &mut meta,
                            &mut last_theme,
                            &attached_info,
                            bell_marked,
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

        // Service the subscribers: a watcher may identify itself, re-subscribe
        // (harmless — it just gets a fresh snapshot), or disconnect. Serviced
        // before the pending pool so `subs_re` still matches: promotions grow
        // `subscribers` below, and the newcomers are polled next turn.
        if !subscribers.is_empty() {
            let mut kept = Vec::new();
            for (i, mut s) in std::mem::take(&mut subscribers).into_iter().enumerate() {
                let re = subs_re.get(i).copied().unwrap_or_else(PollFlags::empty);
                if !re.intersects(PollFlags::IN | PollFlags::HUP) {
                    kept.push(s);
                    continue;
                }
                let disposition = match s.conn.recv::<ClientMsg>() {
                    Ok(None) => Disposition::Drop,
                    Ok(Some(msgs)) => handle_client_messages(
                        &mut s,
                        msgs,
                        &pty,
                        &mut screen,
                        &mut recorder,
                        current_name,
                        &mut meta,
                        &mut last_theme,
                        &attached_info,
                        bell_marked,
                    )?,
                    Err(_) => Disposition::Drop,
                };
                match disposition {
                    Disposition::Kill => {
                        kill_child(&mut child);
                        return Ok(0);
                    }
                    Disposition::Drop => {} // drop s
                    Disposition::Keep => kept.push(s),
                }
            }
            subscribers = kept;
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
                        &mut meta,
                        &mut last_theme,
                        &attached_info,
                        bell_marked,
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
                        } else if p.subscribed {
                            subscribers.push(p); // state observer, kept for pushes
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
            desc_cwd = child_cwd(&child).or_else(|| launch_dir.map(Into::into));
            write_descriptor(current_name, &meta, desc_cwd.clone());
        }

        // Push queued output to the client.
        if let Some(c) = &mut client
            && c.flush().is_err()
        {
            client = None;
        }

        // Reconcile the attach marker with the display client's presence. All the
        // ways `client` can change this turn (handshake takeover, detach, drop,
        // flush error) have run by now, so a single check here covers them.
        let now_attached = client.as_ref().is_some_and(|c| c.resynced);
        if now_attached != attached_marked {
            set_attached_marker(current_name, now_attached);
            if now_attached {
                // Attaching is "switching to" the session: any unseen-bell
                // notification is now seen, so clear its marker.
                set_bell_marker(current_name, false);
                bell_marked = false;
            } else {
                // The user just left: remember where the child was, so a
                // quiet session (no output, so no checkpoint-cadence refresh)
                // still records its final directory for a recreate.
                if let Some(cwd) = child_cwd(&child)
                    && desc_cwd.as_ref() != Some(&cwd)
                {
                    crate::descriptor::set_cwd(current_name, &cwd);
                    desc_cwd = Some(cwd);
                }
            }
            attached_marked = now_attached;
        }

        // Tell the watchers what changed this turn: diff the session's state
        // against what they last saw, plus the turn's occurrences (bell rings,
        // output activity). The state is always tracked — even with no
        // subscriber — so the first event a late subscriber receives is a
        // delta on its snapshot, never a replay of older history.
        {
            let now_state = crate::protocol::SessionState {
                attached: client.as_ref().filter(|c| c.resynced).map(|c| {
                    crate::protocol::AttachInfo {
                        client: c.hello.clone(),
                    }
                }),
                bell: bell_marked,
                title: meta.title.clone(),
                display_name: meta.display_name.clone(),
            };
            let grid = screen.dimensions();
            let regridded = grid != last_grid;
            last_grid = grid;
            if regridded {
                // Keep the discoverable grid size current (coalesced: only an
                // actual change rewrites the file), so a fleet that has never
                // observed this session still shapes its tile correctly.
                meta.size = grid;
                let _ = crate::meta::write(&paths::meta_path(current_name), &meta);
            }
            if !subscribers.is_empty() {
                use crate::protocol::SessionEvent;
                if regridded {
                    for s in &mut subscribers {
                        s.queue(&ServerMsg::Event(SessionEvent::Resized {
                            cols: grid.0,
                            rows: grid.1,
                        }));
                        if s.observing {
                            s.queue_output(screen.resync());
                        }
                    }
                }
                let mut events = Vec::new();
                if rang {
                    events.push(SessionEvent::Bell);
                }
                if now_state.title != last_state.title {
                    events.push(SessionEvent::TitleChanged(now_state.title.clone()));
                }
                if now_state.display_name != last_state.display_name {
                    events.push(SessionEvent::Renamed(now_state.display_name.clone()));
                }
                match (&last_state.attached, &now_state.attached) {
                    (before, Some(info)) if before.as_ref() != Some(info) => {
                        events.push(SessionEvent::Attached(info.clone()));
                    }
                    (Some(_), None) => events.push(SessionEvent::Detached),
                    _ => {}
                }
                for s in &mut subscribers {
                    // Activity is best-effort and high-frequency: skip a
                    // watcher that has not drained its previous push, so a
                    // slow reader never accumulates an unbounded queue of
                    // "something happened" frames.
                    if activity && !s.conn.wants_write() {
                        s.queue(&ServerMsg::Event(SessionEvent::Activity));
                    }
                    for e in &events {
                        s.queue(&ServerMsg::Event(e.clone()));
                    }
                }
            }
            last_state = now_state;
        }
        subscribers.retain_mut(|s| {
            if s.flush().is_err() {
                return false;
            }
            // Re-seed a lagged observer the moment its backlog drains — in
            // this same turn, since a quiet session gives poll no other
            // reason to wake and run a later check.
            if s.lagged && !s.conn.wants_write() {
                s.lagged = false;
                s.queue_output(screen.resync());
                if s.flush().is_err() {
                    return false;
                }
            }
            true
        });

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
#[allow(clippy::too_many_arguments)] // the host's whole mutable state, threaded once
fn handle_client_messages(
    c: &mut Client,
    msgs: Vec<ClientMsg>,
    pty: &pty_process::blocking::Pty,
    screen: &mut Screen,
    recorder: &mut Option<crate::record::FileRecorder>,
    current_name: &str,
    meta: &mut crate::meta::Meta,
    last_theme: &mut crate::query::ThemeColors,
    attached_info: &Option<crate::protocol::AttachInfo>,
    bell_marked: bool,
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
                    c.queue_output(screen.resync());
                    c.resynced = true;
                }
            }
            ClientMsg::Detach => return Ok(Disposition::Drop),
            ClientMsg::Kill => return Ok(Disposition::Kill),
            ClientMsg::Rename(new) => {
                let (ok, message) = match set_display_name(current_name, &new, meta) {
                    Ok(()) => {
                        // The durable descriptor mirrors the label (a no-op
                        // until the child's spawn has written one).
                        crate::descriptor::set_display_name(current_name, &new);
                        (true, new.clone())
                    }
                    Err(e) => (false, e),
                };
                c.queue(&ServerMsg::RenameResult { ok, message });
            }
            ClientMsg::Repaint => {
                if c.resynced {
                    c.queue_output(screen.resync());
                }
            }
            ClientMsg::Theme(colors) => {
                *last_theme = colors;
            }
            ClientMsg::Hello { client } => {
                c.hello = Some(client);
            }
            ClientMsg::Subscribe => {
                // Answer with one consistent snapshot before any delta, and
                // mark the connection so the caller keeps it as a watcher.
                c.queue(&ServerMsg::Snapshot(crate::protocol::SessionState {
                    attached: attached_info.clone(),
                    bell: bell_marked,
                    title: meta.title.clone(),
                    display_name: meta.display_name.clone(),
                }));
                c.subscribed = true;
            }
            ClientMsg::Observe => {
                // A subscription that also mirrors output: snapshot first,
                // then the session's real grid, then a resync of the current
                // screen — the observer sizes its emulator from the grid
                // before feeding the resync. Live output follows from the
                // host loop's fan-out.
                c.queue(&ServerMsg::Snapshot(crate::protocol::SessionState {
                    attached: attached_info.clone(),
                    bell: bell_marked,
                    title: meta.title.clone(),
                    display_name: meta.display_name.clone(),
                }));
                let (cols, rows) = screen.dimensions();
                c.queue(&ServerMsg::Event(crate::protocol::SessionEvent::Resized {
                    cols,
                    rows,
                }));
                c.queue_output(screen.resync());
                c.subscribed = true;
                c.observing = true;
            }
        }
    }
    Ok(Disposition::Keep)
}

/// Create or remove the session's `attached` marker. Best-effort: the marker is
/// advisory (discovery falls back to "detached" if it is missing), and a host
/// that exits without clearing it leaves it inside a directory that the next
/// `list` prunes wholesale, so a stale marker is never read for a live session.
fn set_attached_marker(name: &str, attached: bool) {
    let path = paths::attached_path(name);
    if attached {
        let _ = std::fs::File::create(&path);
    } else {
        let _ = std::fs::remove_file(&path);
    }
}

/// Create or remove the session's `bell` marker, mirroring [`set_attached_marker`].
/// Best-effort and advisory: a host that exits without clearing it leaves it in a
/// directory the next `list` prunes wholesale, so it is never read for a dead one.
fn set_bell_marker(name: &str, rung: bool) {
    let path = paths::bell_path(name);
    if rung {
        let _ = std::fs::File::create(&path);
    } else {
        let _ = std::fs::remove_file(&path);
    }
}

/// Rename the running session by setting its display-name label in `meta`. The
/// session's *identity* — its directory, socket, pid file, and recording — is the
/// immutable spawn-time name and never moves, so a rename cannot disturb attached
/// clients or change attach state. Renaming back to the session's own name clears
/// the label. Returns a human-readable error if the new name is invalid or would
/// collide with another session's name or display name (which would make it
/// ambiguous for `ghost attach`/`kill`/`rename` lookups).
fn set_display_name(
    current_name: &str,
    new_name: &str,
    meta: &mut crate::meta::Meta,
) -> Result<(), String> {
    if !crate::session::valid_name(new_name) {
        return Err(format!(
            "'{new_name}' is not a valid session name (letters, digits, '-', '_', '.')"
        ));
    }
    // Refuse a label another session already answers to (by id or display name),
    // so name-based lookups stay unambiguous.
    match crate::session::list() {
        Ok(sessions)
            if sessions
                .iter()
                .filter(|s| s.name != current_name)
                .any(|s| s.name == new_name || s.display() == new_name) =>
        {
            return Err(format!("a session named '{new_name}' already exists"));
        }
        Ok(_) => {}
        Err(e) => return Err(format!("could not check existing sessions: {e}")),
    }
    // The spawn-time name means "no label" — store it as unset.
    meta.display_name = if new_name == current_name {
        String::new()
    } else {
        new_name.to_string()
    };
    crate::meta::write(&paths::meta_path(current_name), meta)
        .map_err(|e| format!("could not store the display name: {e}"))?;
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
    // The child talks to ghost's own `vt` emulator, not the user's outer
    // terminal — and a session may be launched from a GUI app (launchd, GTK)
    // whose environment carries no usable `TERM`. Set them ourselves to match
    // the emulator's capabilities so tools don't fall back to "not fully
    // functional": apps gate modern features (kitty keyboard protocol,
    // synchronized output) on the TERM name, so `terminfo::session_term()`
    // advertises the kitty profile our emulator implements, providing the
    // terminfo entry itself (bundled or compiled on first use) and handing
    // the child that database via TERMINFO_DIRS.
    let term = crate::terminfo::session_term();
    cmd = cmd.env("TERM", &term.term).env("COLORTERM", "truecolor");
    if let Some(dirs) = &term.terminfo_dirs {
        cmd = cmd.env("TERMINFO_DIRS", dirs);
    }
    if let Some(dir) = launch_dir {
        cmd = cmd.current_dir(dir);
    }
    cmd.spawn(pts).map_err(io::Error::other)
}

/// The child's current working directory, best-effort: Linux reads it from
/// `/proc`; elsewhere (or on any error) `None`, and the descriptor keeps the
/// launch directory.
fn child_cwd(child: &Option<std::process::Child>) -> Option<std::path::PathBuf> {
    #[cfg(target_os = "linux")]
    {
        child
            .as_ref()
            .and_then(|c| std::fs::read_link(format!("/proc/{}/cwd", c.id())).ok())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = child;
        None
    }
}

/// Record the durable descriptor once the child actually starts — the fleet's
/// memory of the session after it dies (see [`crate::descriptor`]). Best-effort,
/// like `meta`.
fn write_descriptor(name: &str, meta: &crate::meta::Meta, cwd: Option<std::path::PathBuf>) {
    let _ = crate::descriptor::write(
        name,
        &crate::descriptor::Descriptor {
            command: meta.command.clone(),
            cwd,
            created_at: meta.created_at,
            display_name: meta.display_name.clone(),
        },
    );
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
