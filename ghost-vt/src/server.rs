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
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{FlockOperation, flock};
use serde::{Deserialize, Serialize};
use std::ffi::{CStr, CString};
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Options for starting a session.
#[derive(Clone, Serialize, Deserialize)]
pub struct SpawnOpts {
    /// Session name (used for the socket and pidfile).
    pub name: String,
    /// Command and arguments to run; empty means `$SHELL` (then `/bin/sh`),
    /// unless `connection` is set (which derives the child argv instead).
    pub command: Vec<String>,
    /// A remote connection this session realizes: when set, the child is the
    /// launcher (`ssh`/`mosh`) derived from the spec, and `command` must be
    /// empty (the two are mutually exclusive; a spec + non-empty command is
    /// rejected at spawn). `None` for an ordinary local session. See
    /// [`crate::connection`].
    #[serde(default)]
    pub connection: Option<crate::connection::ConnectionSpec>,
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
/// A checkpoint serializes and compresses the *entire* emulator state, so
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

/// The hidden subcommand a self-upgrade runs on its target to read that binary's
/// [`HANDOFF_VERSION`] before exec'ing onto it (see [`probe_handoff_version`]).
/// Kept in sync with the `__handoff` clap subcommand in `ghost-cli`.
pub const HANDOFF_ARG: &str = "__handoff";

/// A single attached client connection: the framed [`Conn`] plus a little
/// attach state.
struct Client {
    // Host-side connections are always accepted local `UnixStream`s (the ssh
    // tunnel terminates at the *client*, over `Conn<AnyTransport>`).
    conn: Conn<UnixStream>,
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
    /// Set when this connection asked for a self-upgrade (`ClientMsg::Upgrade`)
    /// and is waiting on the verdict. The upgrade is deferred to the next clean
    /// boundary, so a refusal can't be answered inline; the loop tail queues a
    /// [`ServerMsg::UpgradeResult`] here instead (on SUCCESS the exec drops the
    /// connection and the requester reads that EOF as "taken").
    awaiting_upgrade: bool,
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
            awaiting_upgrade: false,
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

/// Queue a [`ServerMsg::UpgradeResult`] refusal to every connection still
/// awaiting an upgrade verdict, and clear its flag. The requester is a control
/// connection in `pending` (or, if an attached terminal asked, the display
/// `client`); either way it is flushed on a following turn. Used for both a
/// refusal at the boundary and a give-up when no boundary arrives.
fn notify_upgrade_refused(pending: &mut [Client], client: &mut Option<Client>, message: &str) {
    for c in pending.iter_mut().chain(client.as_mut()) {
        if c.awaiting_upgrade {
            c.queue(&ServerMsg::UpgradeResult {
                ok: false,
                message: message.to_string(),
            });
            c.awaiting_upgrade = false;
        }
    }
}

/// The exec-handoff blob format version — the wire between an old host and the
/// new binary it re-execs onto (the `HostArgs` layout below). SEPARATE from
/// [`crate::protocol::PROTO_LEVEL`], which versions the *client* protocol: this
/// versions the argv blob, and the two move independently. Postcard is
/// positional and NON-self-describing — it silently ignores trailing bytes and
/// misreads a changed layout — so a mismatched blob cannot be safely decoded.
/// The decode happens AFTER the `execv`, where a failure is unrecoverable
/// (the old image is gone), so a self-upgrade must **probe the target's version
/// before the exec** and refuse on mismatch (see [`self_upgrade`]). BUMP this on
/// any change to `HostArgs`/`Adopt`/`SpawnOpts` layout.
///
/// - v1: child pid + PTY master fd.
/// - v2: adds the screen checkpoint fd (screen survives the swap).
pub const HANDOFF_VERSION: u32 = 2;

/// The host's startup state, serialized onto argv across the re-exec.
#[derive(Serialize, Deserialize)]
struct HostArgs {
    /// The blob format version ([`HANDOFF_VERSION`]) — FIRST field so it decodes
    /// unambiguously before any layout-sensitive field. A decode that finds an
    /// unexpected value here is a version skew a self-upgrade's pre-exec probe
    /// should already have refused; the check in [`run_host`] is a backstop.
    handoff_version: u32,
    opts: SpawnOpts,
    /// The directory the session was launched from, applied to the child (like
    /// dtach) since the daemon itself `chdir`s to `/`.
    launch_dir: Option<std::path::PathBuf>,
    /// Set only for an in-place **self-upgrade** re-exec (see
    /// `docs/host-self-upgrade.md`): the new host adopts an already-running
    /// child on its existing PTY instead of opening a PTY and spawning. `None`
    /// for an ordinary spawn.
    adopt: Option<Adopt>,
}

/// The running child a self-upgrade hands to its successor. The child stays
/// *our* direct child across an in-place `execv` (same pid), so the new host
/// reaps it by pid and talks to it over the carried PTY master fd.
#[derive(Serialize, Deserialize)]
struct Adopt {
    /// The live child's pid, adopted via [`crate::child::Child::from_pid`].
    child_pid: u32,
    /// The PTY master fd, kept open across the exec (CLOEXEC cleared) so the
    /// new host reads/writes the same terminal the child is attached to.
    pty_master_fd: RawFd,
    /// A checkpoint of the pre-upgrade screen, as a standalone one-frame
    /// recording on an unlinked temp file kept open across the exec (CLOEXEC
    /// cleared). The new host reads it once, rebuilds the screen (so the swap
    /// isn't visible as a blank repaint), then closes it — freeing the inode.
    checkpoint_fd: RawFd,
}

/// Start a session in the background.
///
/// Returns `Ok(())` in the calling process once the host has been forked off and
/// re-exec'd. The host runs in that separate, re-exec'd process — never in the
/// caller — so this is safe to call even from a multithreaded process such as a
/// GUI front-end.
pub fn spawn(opts: SpawnOpts) -> io::Result<()> {
    // The name becomes a directory and a socket filename, so it must be a safe
    // path component. Guard here — the single chokepoint every spawn funnels
    // through — so no caller (CLI, GUI, ssh) can create an unsafe id.
    if !crate::session::valid_name(&opts.name) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "'{}' is not a valid session name (letters, digits, '-', '_', '.')",
                opts.name
            ),
        ));
    }
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
    // The exec generation starts at 0. A self-upgrade bumps it just before its
    // `execv`, so a local attach can tell a re-exec (marker advanced) from a
    // take-over (marker unchanged) when its connection drops. Written before the
    // socket binds, like the proto marker, so no client attaches without it.
    std::fs::write(paths::gen_path(&opts.name), "0")?;

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
        handoff_version: HANDOFF_VERSION,
        launch_dir: std::env::current_dir().ok(),
        opts,
        adopt: None,
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
    // Backstop: a blob whose format version isn't ours cannot be trusted (its
    // layout may differ from what we decoded above). A self-upgrade's pre-exec
    // probe should already have refused this skew; reaching it here means the
    // probe was bypassed or wrong, and there is no recovery post-`execv`, so bail
    // rather than adopt a possibly-misdecoded child/fd handoff.
    if host_args.handoff_version != HANDOFF_VERSION {
        return 127;
    }
    // Hold the inherited liveness lock for our whole life. The parent took the
    // flock before forking and it survived the exec; keeping this fd open keeps
    // the lock held, and the kernel frees it when we exit or crash — which is how
    // `session::list` knows we are gone.
    // SAFETY: a fd the parent passed us with CLOEXEC cleared; we own it now.
    let _lock = unsafe { OwnedFd::from_raw_fd(lock_fd) };
    // SAFETY: the listening socket the parent bound and passed with CLOEXEC cleared.
    let listener = unsafe { UnixListener::from_raw_fd(listener_fd) };
    let HostArgs {
        handoff_version: _, // already checked against HANDOFF_VERSION above
        opts,
        launch_dir,
        adopt,
    } = host_args;
    // The name is the session's immutable identity: a rename only changes the
    // display-name label in `meta`, so files never move and cleanup always
    // targets the spawn-time directory. `lock_fd` is passed through so a
    // self-upgrade can re-hand the same held lock to its successor on argv.
    let result = host_main(
        &listener,
        lock_fd,
        &opts,
        launch_dir.as_deref(),
        &opts.name,
        adopt,
    );
    let _ = std::fs::remove_dir_all(paths::session_dir(&opts.name));
    result.unwrap_or(1)
}

/// Clear `FD_CLOEXEC` so a descriptor survives `execv` into the host process.
fn clear_cloexec(fd: &impl AsRawFd) -> io::Result<()> {
    clear_cloexec_raw(fd.as_raw_fd())
}

/// Clear `FD_CLOEXEC` on a raw fd (a self-upgrade carries the lock fd only as a
/// number, not as an owned handle).
fn clear_cloexec_raw(raw: RawFd) -> io::Result<()> {
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

/// Put the PTY master into non-blocking mode. Writes into the child are then
/// queued and drained under POLLOUT (never a blocking `write_all`, which would
/// wedge the single-threaded loop the moment the child stopped reading); the
/// read path already tolerates `WouldBlock`.
fn set_pty_nonblocking(pty: &pty_process::blocking::Pty) -> io::Result<()> {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    let fd = pty.as_fd();
    let mut flags = fcntl_getfl(fd).map_err(io::Error::from)?;
    flags.set(OFlags::NONBLOCK, true);
    fcntl_setfl(fd, flags).map_err(io::Error::from)
}

/// Build the signal set a self-upgrade blocks across the `execv`: SIGTERM and
/// SIGINT, the two that would otherwise hit the default disposition (terminate)
/// in the window after `execv` resets dispositions to `SIG_DFL` and before the
/// new host installs its handlers.
fn upgrade_signal_set() -> libc::sigset_t {
    // SAFETY: `sigemptyset`/`sigaddset` initialize and populate the set.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTERM);
        libc::sigaddset(&mut set, libc::SIGINT);
        set
    }
}

/// Block SIGTERM/SIGINT before an `execv`. The mask is preserved across the
/// exec, so a signal that arrives during the handler-less window stays PENDING
/// (not delivered at default disposition) until the new host unblocks it.
fn block_upgrade_signals() -> io::Result<()> {
    let set = upgrade_signal_set();
    // SAFETY: valid set pointer; no old-set out param.
    if unsafe { libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Unblock SIGTERM/SIGINT after the new host has reinstalled its handlers. A
/// signal that was blocked-and-pending across the upgrade fires the instant it
/// is unblocked — now into the self-pipe handler, not at default disposition.
/// A no-op for an ordinary spawn (they were never blocked); `signals::make`
/// only sets a per-handler mask, never the process mask, so this is what clears
/// the block a self-upgrade left.
fn unblock_upgrade_signals() {
    let set = upgrade_signal_set();
    // SAFETY: valid set pointer; ignoring failure — an unblock cannot leave us
    // worse off than blocked, and there is no recovery from here anyway.
    unsafe { libc::sigprocmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut()) };
}

/// How long the handoff-version probe waits for the target to answer before
/// giving up and refusing the upgrade. The host runs this on its single event
/// loop, so a target that ignores its argv and blocks (a wrong path, a wedged
/// wrapper script) must not hang it forever — [`wait_bounded`] kills it at the
/// deadline.
pub(crate) const HANDOFF_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the host waits for the terminal to reach a clean handoff boundary
/// after an upgrade is requested before giving up. A child that never quiesces
/// (continuous output, or a program that leaves the parser stuck mid-escape and
/// idles) would otherwise hold the request — and the requester waiting on it —
/// indefinitely. The two host-side costs are SEQUENTIAL in the worst case (wait
/// out this window for a boundary, THEN probe for up to [`HANDOFF_PROBE_TIMEOUT`]
/// once one arrives), so `upgrade_session`'s client deadline is sized as their
/// sum plus slack — see there.
pub(crate) const UPGRADE_BOUNDARY_WINDOW: Duration = Duration::from_secs(5);

/// A pending in-place self-upgrade: the target `path` (`None` = our own current
/// exe) and the `deadline` past which the host abandons it if no clean handoff
/// boundary has arrived.
struct PendingUpgrade {
    path: Option<String>,
    deadline: Instant,
}

/// What the target reports about itself via `ghost __handoff`: the exec-handoff
/// blob format it speaks ([`HANDOFF_VERSION`]) and the client protocol level it
/// serves ([`crate::protocol::PROTO_LEVEL`]). Both gate the upgrade — a handoff
/// mismatch would misdecode the blob, and a lower proto level would silently
/// downgrade the session's capabilities.
struct ProbedTarget {
    handoff: u32,
    proto: u32,
}

/// Ask the target binary what it speaks, by running its `ghost __handoff`
/// subcommand and parsing the `<handoff_version> <proto_level>` line it prints.
/// A binary predating the mechanism has no such subcommand, so this errors —
/// which the caller treats as "refuse the upgrade". Bounded ([`wait_bounded`]):
/// a hung target is killed at the deadline and the upgrade refused, rather than
/// wedging the host loop on a blocking `output()`. Cheap and reversible (a
/// short-lived child that only prints two numbers), so it is safe to run before
/// any of the exec's irreversible-looking steps — but it DOES run the target, so
/// [`validate_upgrade_target`] must vet it first. Extra trailing tokens are
/// ignored, so a future binary may append fields without breaking this parse.
fn probe_target(exe: &std::path::Path) -> io::Result<ProbedTarget> {
    use std::io::Read as _;
    use std::process::Stdio;
    let mut child = std::process::Command::new(exe)
        .arg(HANDOFF_ARG)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| io::Error::new(e.kind(), format!("cannot probe upgrade target: {e}")))?;
    match crate::remote::wait_bounded(&mut child, HANDOFF_PROBE_TIMEOUT) {
        Some(s) if s.success() => {}
        // Exited non-zero (or was signalled): a binary too old to have a
        // `__handoff` subcommand fails its arg parse this way.
        Some(_) => {
            return Err(io::Error::other(
                "upgrade target rejected the handoff-version probe (too old to answer __handoff?)",
            ));
        }
        // Killed at the deadline: it never answered.
        None => {
            return Err(io::Error::other(
                "upgrade target did not answer the handoff-version probe in time (hung?)",
            ));
        }
    }
    let mut buf = String::new();
    child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("handoff probe produced no output"))?
        .read_to_string(&mut buf)
        .map_err(|e| io::Error::new(e.kind(), format!("reading handoff probe output: {e}")))?;
    let mut toks = buf.split_whitespace();
    let handoff = toks.next().and_then(|t| t.parse().ok()).ok_or_else(|| {
        io::Error::other("upgrade target returned an unparseable handoff version")
    })?;
    let proto = toks
        .next()
        .and_then(|t| t.parse().ok())
        .ok_or_else(|| io::Error::other("upgrade target did not report a protocol level"))?;
    Ok(ProbedTarget { handoff, proto })
}

/// Vet a target binary before we probe or exec it. The probe RUNS the target
/// and the exec hands it our whole process — so a target we do not trust must be
/// rejected here, ahead of both. We require a regular file owned by us or root
/// (a system install) and not writable by group or other: a path someone else
/// can rewrite could be swapped for a hostile binary. This is inherently a
/// check-then-use (the file could still change between here and the exec), and
/// this is a local action (`ghost __upgrade <name> <path>`), so the bar is "not
/// obviously tamperable", not a full trust chain — enough to reject the easy
/// footguns (a world-writable drop, someone else's file).
fn validate_upgrade_target(exe: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt as _;
    let meta = std::fs::metadata(exe).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("cannot stat upgrade target {}: {e}", exe.display()),
        )
    })?;
    if !meta.is_file() {
        return Err(io::Error::other(format!(
            "refusing to upgrade: target {} is not a regular file",
            exe.display()
        )));
    }
    let us = rustix::process::getuid().as_raw();
    if meta.uid() != 0 && meta.uid() != us {
        return Err(io::Error::other(format!(
            "refusing to upgrade: target {} is owned by uid {} (neither us nor root)",
            exe.display(),
            meta.uid()
        )));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(io::Error::other(format!(
            "refusing to upgrade: target {} is writable by group or other",
            exe.display()
        )));
    }
    Ok(())
}

/// Serialize the current screen into an unlinked temp file as a standalone
/// one-checkpoint recording, so a self-upgrade can hand its successor the live
/// screen across the exec (the fd is carried; the successor reads it once and
/// closes it — an unlinked file, so the inode is then freed). Reversible: on any
/// failure the caller refuses the upgrade and the temp file drops harmlessly.
fn write_checkpoint_tempfile(screen: &Screen, command: &[String]) -> io::Result<std::fs::File> {
    use std::io::Seek;
    let (cols, rows) = screen.dimensions();
    let dump = screen.dump_without_images();
    let images = screen.graphics_images();
    let mut file = tempfile::tempfile()?;
    {
        // `&mut File` is a `Write`; the recorder writes the header + one
        // checkpoint frame, then its `Drop` flushes — leaving `file` owned.
        let mut rec = crate::record::Recorder::new(&mut file, cols, rows, command)?;
        rec.checkpoint_with_images(cols, rows, &dump, &images)?;
        rec.flush()?;
    }
    // Rewind so the successor reads from the start.
    file.rewind()?;
    Ok(file)
}

/// Rebuild a screen from the checkpoint our predecessor left on `fd` (see
/// [`write_checkpoint_tempfile`]). Best-effort: a blank screen if the checkpoint
/// is unreadable, so a decode hiccup degrades to Step-3 behavior (blank until the
/// child repaints) rather than failing the adopt. Consumes the fd (closes it).
fn read_checkpoint(fd: RawFd, scrollback: usize) -> Option<Screen> {
    use std::io::{Read, Seek};
    // SAFETY: an fd our predecessor handed us with CLOEXEC cleared; we own it now
    // and close it when this `File` drops.
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    file.rewind().ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let rec = crate::record::read_bytes(&bytes).ok()?;
    Some(Screen::from_recording(&rec, scrollback))
}

/// How [`self_upgrade`] can come back — it never returns on success (the `execv`
/// replaces this image).
enum UpgradeOutcome {
    /// The upgrade was declined; the host runs on. `String` is the human reason
    /// routed back to the requester.
    Refused(String),
    /// A terminating signal (SIGTERM/SIGINT) arrived during the (possibly slow)
    /// upgrade prep. The caller must HONOR the kill — the signal was pulled off
    /// the self-pipe (whose fd would not cross the exec), so it will not be
    /// delivered again; running on would drop it.
    Terminated,
}

/// Re-exec THIS process in place — same pid, no fork — onto the host at `exe`,
/// adopting the running `child_pid` on the carried PTY master. Only the code
/// image is replaced; the listener, the liveness lock, the PTY, and the child
/// all survive.
///
/// Never returns on success (`execv` replaces the image); a return is a
/// [`UpgradeOutcome`] the caller acts on. A `Refused` return is not fatal — this
/// process still holds the PTY master, the child, and the flock, so the caller
/// MUST keep running, never exit. Every step here is therefore reversible: on
/// any error the live host is left exactly as it was, save the CLOEXEC flags we
/// cleared (harmless) and the signal block, which we lift before returning.
#[allow(clippy::too_many_arguments)] // the whole exec handoff, threaded once
fn self_upgrade(
    listener: &UnixListener,
    lock_fd: RawFd,
    pty: &pty_process::blocking::Pty,
    child_pid: u32,
    screen: &Screen,
    opts: &SpawnOpts,
    launch_dir: Option<&std::path::Path>,
    path: Option<&str>,
    sigfd: &crate::signals::Signals,
    current_name: &str,
) -> UpgradeOutcome {
    // All the reversible prep (validate, probe, checkpoint, clear CLOEXEC, build
    // the argv) that can fail with a reason — an `Err` here means "refuse and run
    // on", nothing has been committed. The owned `exe_c`/`argv` must outlive the
    // `execv`, so they come back in `Prepared`.
    let prepared = match prepare_upgrade(
        listener, lock_fd, pty, child_pid, screen, opts, launch_dir, path,
    ) {
        Ok(p) => p,
        Err(e) => return UpgradeOutcome::Refused(e.to_string()),
    };
    let mut argv: Vec<*const libc::c_char> =
        prepared.argv_owned.iter().map(|c| c.as_ptr()).collect();
    argv.push(std::ptr::null());

    // Block the terminating signals immediately before the exec, so the window
    // they guard is as small as possible. If the exec fails we unblock below.
    if let Err(e) = block_upgrade_signals() {
        return UpgradeOutcome::Refused(e.to_string());
    }
    // Now that new deliveries are blocked, drain the self-pipe for a terminating
    // signal that arrived DURING the prep above (the target probe alone can take
    // seconds). Its fd is CLOEXEC and would not cross the exec, so a SIGTERM/INT
    // sitting there would be lost — worse, `kill_session` would then prune a host
    // that is alive as the successor. Honor it here instead. (A signal arriving
    // AFTER the block is OS-pending, survives the exec, and the successor
    // unblocks and handles it — so nothing is dropped either way.)
    if drained_terminating_signal(sigfd) {
        unblock_upgrade_signals();
        return UpgradeOutcome::Terminated;
    }
    // Publish the new exec generation BEFORE handing off. The `execv` is what
    // closes each display client's connection (their fds are CLOEXEC), so the
    // drop the client observes strictly follows this write — it will read the
    // advanced marker and know this was a re-exec, not a take-over, and reconnect.
    // We are single-threaded and past every fallible step, so this is the last
    // thing before the exec; on the (near-impossible) exec failure below we rewind
    // it so a later take-over of a client that attached before this attempt can't
    // misread the stale bump as a re-exec.
    let gen_path = paths::gen_path(current_name);
    let prev_gen = read_gen_marker(&gen_path);
    let _ = std::fs::write(&gen_path, (prev_gen + 1).to_string());
    // SAFETY: `argv` is NUL-terminated and its pointers (into `prepared.argv_owned`,
    // held alive until the call returns) stay valid; `execv` only returns on failure.
    let err = unsafe {
        libc::execv(prepared.argv_owned[0].as_ptr(), argv.as_ptr());
        io::Error::last_os_error()
    };
    // Only here on failure: rewind the generation bump, lift the block, and
    // report. The host keeps running on its old image at its old generation.
    let _ = std::fs::write(&gen_path, prev_gen.to_string());
    unblock_upgrade_signals();
    UpgradeOutcome::Refused(err.to_string())
}

/// Read a session's exec-generation marker (see [`paths::gen_path`]); `0` when
/// absent or unparsable — the value a fresh spawn writes and the floor a
/// self-upgrade bumps from.
fn read_gen_marker(path: &std::path::Path) -> u64 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// The owned values a [`self_upgrade`] `execv` needs kept alive until it runs:
/// the argv `CString`s (`argv_owned[0]` is the exe path) and the checkpoint
/// tempfile whose fd is carried across the exec.
struct Prepared {
    argv_owned: [CString; 5],
    _checkpoint: std::fs::File,
}

/// The reversible half of [`self_upgrade`]: everything that can fail cleanly,
/// leaving the live host exactly as it was (save the harmless CLOEXEC flags we
/// clear). An `Err` is a refusal reason.
#[allow(clippy::too_many_arguments)] // the whole exec handoff, threaded once
fn prepare_upgrade(
    listener: &UnixListener,
    lock_fd: RawFd,
    pty: &pty_process::blocking::Pty,
    child_pid: u32,
    screen: &Screen,
    opts: &SpawnOpts,
    launch_dir: Option<&std::path::Path>,
    path: Option<&str>,
) -> io::Result<Prepared> {
    let exe = match path {
        Some(p) => std::path::PathBuf::from(p),
        None => std::env::current_exe()?,
    };
    let exe_c = CString::new(exe.as_os_str().as_bytes())
        .map_err(|_| io::Error::other("executable path contains a NUL byte"))?;

    // VET, THEN PROBE, BOTH BEFORE THE EXEC. `validate_upgrade_target` runs
    // first because the probe RUNS the target: we won't exec (nor even probe) a
    // file we don't trust (foreign-owned, group/world-writable, not a regular
    // file).
    validate_upgrade_target(&exe)?;
    // The blob we are about to hand the target is in OUR format
    // ([`HANDOFF_VERSION`]); a target speaking a different version would
    // misdecode it (postcard is positional and ignores trailing bytes, so the
    // miss can be SILENT — a wrong-layout adopt that spawns a duplicate child and
    // orphans the real one). The decode is post-`execv`, where failure is
    // unrecoverable, so we must catch the skew here, while a refusal simply
    // leaves the host running. A target that predates the mechanism has no
    // `__handoff` subcommand, so the probe errors and we refuse — never exec into
    // it.
    let probed = probe_target(&exe)?;
    if probed.handoff != HANDOFF_VERSION {
        return Err(io::Error::other(format!(
            "refusing to upgrade: target speaks handoff version {}, we speak {HANDOFF_VERSION} — \
             restart the session instead",
            probed.handoff
        )));
    }
    // Refuse an in-place DOWNGRADE. All attached clients are dropped by the exec
    // and reconnect to the successor, renegotiating at its proto level; a target
    // below ours would silently lower what the session can do. Rolling back to an
    // older binary is `__restart` territory (a fresh respawn), not a self-upgrade.
    if probed.proto < crate::protocol::PROTO_LEVEL {
        return Err(io::Error::other(format!(
            "refusing to upgrade: target serves protocol level {} but we serve {} — an in-place \
             downgrade would silently lower the session's capabilities; restart to roll back",
            probed.proto,
            crate::protocol::PROTO_LEVEL
        )));
    }

    // Checkpoint the current screen onto an unlinked temp file to carry it across
    // the exec (reversible: a throwaway file dropped on any later failure). Built
    // before we touch fd flags so nothing is half-applied if it fails.
    let checkpoint = write_checkpoint_tempfile(screen, &opts.command)?;

    // Keep the listener, the lock, the PTY master, and the checkpoint open across
    // the exec.
    clear_cloexec(listener)?;
    clear_cloexec_raw(lock_fd)?;
    let master_fd = pty.as_raw_fd();
    clear_cloexec_raw(master_fd)?;
    let checkpoint_fd = checkpoint.as_raw_fd();
    clear_cloexec_raw(checkpoint_fd)?;

    let host_args = HostArgs {
        handoff_version: HANDOFF_VERSION,
        opts: opts.clone(),
        launch_dir: launch_dir.map(Into::into),
        adopt: Some(Adopt {
            child_pid,
            pty_master_fd: master_fd,
            checkpoint_fd,
        }),
    };
    let blob = encode_host_args(&host_args);
    let argv_owned = [
        exe_c,
        CString::new(HOST_ARG).expect("HOST_ARG has no NUL"),
        CString::new(listener.as_raw_fd().to_string()).expect("fd digits have no NUL"),
        CString::new(lock_fd.to_string()).expect("fd digits have no NUL"),
        CString::new(blob).expect("hex blob has no NUL"),
    ];
    Ok(Prepared {
        argv_owned,
        _checkpoint: checkpoint,
    })
}

/// Drain the signal self-pipe and report whether a terminating signal
/// (SIGTERM/SIGINT) was among what it held. Consumes every buffered signal;
/// SIGCHLD is a no-op (child exit is driven by PTY EOF, not this pipe), so
/// swallowing one here is harmless.
fn drained_terminating_signal(sigfd: &crate::signals::Signals) -> bool {
    crate::signals::drain(sigfd)
        .unwrap_or_default()
        .into_iter()
        .any(|s| s == libc::SIGTERM || s == libc::SIGINT)
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

#[allow(clippy::too_many_arguments)] // the host's whole startup state
fn host_main(
    listener: &UnixListener,
    lock_fd: RawFd,
    opts: &SpawnOpts,
    launch_dir: Option<&std::path::Path>,
    current_name: &str,
    adopt: Option<Adopt>,
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
    // A self-upgrade blocks SIGTERM/SIGINT across the `execv` (so a racing
    // `ghost kill` can't hit the default disposition mid-exec and orphan the
    // child) and leaves them blocked-and-pending for us to inherit. Unblock them
    // now that handlers are installed: a signal that raced the upgrade fires into
    // the self-pipe here instead of killing us. Idempotent for an ordinary spawn,
    // whose mask is already empty. See `docs/host-self-upgrade.md`.
    unblock_upgrade_signals();
    // A self-upgrade must republish the protocol level: `spawn` wrote the OLD
    // host's level before binding the socket, and the exec path never revisits
    // `spawn`, so without this the marker would keep naming the predecessor's
    // level and every `session_proto` gate would stay shut — the upgrade would
    // adopt no new protocol at all. Only on the adopt path: a fresh spawn already
    // wrote it atomically (before the socket bound), and rewriting it here would
    // race anything that reads or overwrites the marker between spawn and now.
    if adopt.is_some() {
        let _ = std::fs::write(
            paths::proto_path(current_name),
            crate::protocol::PROTO_LEVEL.to_string(),
        );
    }
    std::fs::write(
        paths::pid_path(current_name),
        std::process::id().to_string(),
    )?;
    listener.set_nonblocking(true)?;

    // PTY, child, and the grid to build state at. An ordinary spawn opens a fresh
    // PTY, sizes it to `opts.size`, and (eagerly or on the first attach) spawns
    // the child itself. A self-upgrade instead ADOPTS the running child on its
    // existing PTY master, carried across the `execv`: no new PTY, no spawn — the
    // same terminal, the same live process. Its grid is whatever the child is
    // *currently* on (read from the carried master, whose kernel winsize survives
    // the exec), NOT the stale spawn-time `opts.size`: resizing it back would fire
    // a spurious SIGWINCH and corrupt a full-screen TUI. Non-blocking master
    // either way: writes into the child are queued and drained under POLLOUT (see
    // `pty_out`), never with a blocking `write_all`, which would wedge this
    // single-threaded loop the moment the child stopped reading.
    #[allow(clippy::type_complexity)] // a one-off setup tuple, named right below
    let (pty, mut pts, mut child, cols, rows): (
        pty_process::blocking::Pty,
        Option<pty_process::blocking::Pts>,
        Option<crate::child::Child>,
        u16,
        u16,
    ) = match &adopt {
        Some(a) => {
            // SAFETY: the master fd our predecessor handed us with CLOEXEC
            // cleared; we take sole ownership now. The child is still attached.
            let pty = unsafe {
                pty_process::blocking::Pty::from_fd(OwnedFd::from_raw_fd(a.pty_master_fd))
            };
            set_pty_nonblocking(&pty)?;
            let ws = rustix::termios::tcgetwinsize(pty.as_fd()).map_err(io::Error::from)?;
            (
                pty,
                None,
                Some(crate::child::Child::from_pid(a.child_pid)),
                ws.ws_col,
                ws.ws_row,
            )
        }
        None => {
            let (pty, pts) = open().map_err(io::Error::other)?;
            set_pty_nonblocking(&pty)?;
            let (cols, rows) = opts.size;
            pty.resize(Size::new(rows, cols))
                .map_err(io::Error::other)?;
            (pty, Some(pts), None, cols, rows)
        }
    };

    // The child argv, resolved once: a connection spec derives the launcher
    // (`ssh …`), otherwise the literal command. Rejects a spec + command clash
    // before anything is written. `meta.command` keeps the *literal* command
    // (empty for a connection session — the spec is the authoritative record).
    let child_command = effective_command(&opts.command, opts.connection.as_ref())?;

    // Descriptive metadata for discovery (the GUI sidebar). Created time and
    // command are fixed; the title is refreshed below whenever it changes.
    // Built before the child can spawn: the spawn also writes the durable
    // descriptor, which carries these facts.
    // A session that ran before under this name (a recreate, a resurrect) already
    // has a policy the user's terminal negotiated for it; going back to permissive
    // just because the process restarted would be a silent downgrade. The durable
    // descriptor is what survives a host's death — the runtime `meta` is pruned
    // with the session directory — so that is what we inherit from.
    let inherited_policy = crate::descriptor::read(current_name)
        .map(|d| d.policy)
        .or_else(|| crate::meta::read(&paths::meta_path(current_name)).map(|m| m.policy))
        .unwrap_or_default();
    // A self-upgrade keeps the SAME session — its identity must survive the swap.
    // Reuse the running host's on-disk `meta` (creation time — the fleet's sort
    // key — display-name label, title, size) rather than minting a fresh one that
    // would reorder the session in the fleet and drop its rename. A fresh spawn
    // (or the rare missing-meta case) builds one from scratch.
    let mut meta = match adopt
        .is_some()
        .then(|| crate::meta::read(&paths::meta_path(current_name)))
        .flatten()
    {
        Some(existing) => existing,
        None => crate::meta::Meta {
            // Milliseconds, not seconds: this is the fleet's spatial sort key, so
            // sub-second resolution keeps sessions spawned in the same second in
            // their true creation order rather than tie-breaking by name.
            created_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0),
            command: opts.command.clone(),
            title: String::new(),
            display_name: String::new(),
            size: opts.size,
            connection: opts.connection.clone(),
            policy: inherited_policy,
        },
    };
    let _ = crate::meta::write(&paths::meta_path(current_name), &meta);

    // The child is started eagerly for a plain detached session, or deferred
    // until the first attach handshake (see `SpawnOpts::start_on_attach`). While
    // deferred we hold the slave (`pts`) so the PTY master never sees EOF and the
    // poll loop just idles until a client attaches. A self-upgrade already
    // adopted its child above, so neither branch runs.
    // The last cwd written to the durable descriptor, so refreshes only touch
    // the file when the child actually moved.
    let mut desc_cwd: Option<std::path::PathBuf> = None;
    if adopt.is_none() && !opts.start_on_attach {
        child = Some(spawn_child(
            &child_command,
            current_name,
            launch_dir,
            pts.take().expect("slave present before first spawn"),
        )?);
        desc_cwd = child_cwd(&child).or_else(|| launch_dir.map(Into::into));
        write_descriptor(current_name, &meta, desc_cwd.clone());
    }

    // Authoritative screen state, fed every byte the child writes so a late
    // attach can be repainted to the current state. Its initial contents depend
    // on how this host started:
    // - A self-upgrade rebuilds the pre-upgrade screen from the checkpoint its
    //   predecessor handed us on `checkpoint_fd`, so the swap isn't a blank
    //   repaint; the child's live PTY continues below it.
    // - A seeded spawn (a recreate) starts from its predecessor's recording:
    //   read it NOW, before the recorder below truncates the (typically same)
    //   path.
    // - Otherwise blank.
    // Either restored state is reflowed to this session's grid.
    let restored = match &adopt {
        Some(a) => read_checkpoint(a.checkpoint_fd, opts.scrollback),
        None => opts.seed_from.as_ref().and_then(|p| {
            crate::record::read(p)
                .map(|rec| Screen::from_recording(&rec, opts.scrollback))
                .ok()
        }),
    };
    let mut screen = match restored {
        Some(mut seeded) => {
            seeded.resize(cols, rows);
            seeded
        }
        None => Screen::new(cols, rows, opts.scrollback),
    };
    // Detached, the host *is* the terminal: it filters the child's output and
    // answers its queries alone, so it enforces the policy the last terminal to
    // attach reported (see `ClientMsg::Policy`).
    screen.set_policy(inherited_policy);

    // Optional durable recording. Best-effort: if it cannot be created, the
    // session still runs (just unrecorded). A self-upgrade CONTINUES the existing
    // recording (append, no truncation) so `ghost search` history survives the
    // swap; a fresh spawn (or an upgrade whose file is somehow gone) creates one.
    let mut recorder = opts.record.as_ref().and_then(|path| {
        if adopt.is_some()
            && let Ok(r) = crate::record::FileRecorder::open_append(path, opts.max_recording_bytes)
        {
            return Some(r);
        }
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
    //
    // Unlike the cadence checkpoint below, this one is NOT gated on a pending
    // UTF-8 tail: this is a fresh child, so any incomplete trailing bytes the seed
    // ended on have no continuation coming — they are dead bytes, and dropping
    // them from the standalone checkpoint is correct.
    if adopt.is_none()
        && opts.seed_from.is_some()
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
    // Bytes bound for the child's PTY, buffered here rather than written with a
    // blocking `write_all`: client input, and — while detached — the query and
    // graphics replies the host answers itself. Drained under POLLOUT at the end
    // of each turn (see the drain step below). Unbounded, like the display
    // client's own outbound queue: a child that never reads lets this grow, but
    // that is strictly better than the deadlock a blocking write would cause, and
    // realistically the child resumes reading. (Read-side throttling of the
    // client socket is the future direction if that ever needs a bound.)
    let mut pty_out: Vec<u8> = Vec::new();
    // Spots the child's terminal queries so the host can answer them while no
    // client is attached to do so (kept fed every chunk to track split sequences).
    let mut queries = crate::query::QueryScanner::new();
    // The last theme a client reported (ClientMsg::Theme); detached color
    // queries answer with it. Ghost's default scheme until someone attaches.
    let mut last_theme = crate::query::ThemeColors::default();
    // A requested in-place self-upgrade (`ClientMsg::Upgrade`), held until the
    // screen reaches a clean handoff boundary and the child exists to adopt, or
    // until its deadline lapses (a child that never quiesces). See the boundary
    // trigger at the loop's tail and `docs/host-self-upgrade.md`.
    let mut pending_upgrade: Option<PendingUpgrade> = None;

    loop {
        // Build the poll set: fixed fds first, then the display client (if any),
        // then the pending connections.
        let mut pty_flags = PollFlags::IN;
        if !pty_out.is_empty() {
            // Queued input waiting on a full child: wake when the master can take
            // more, so the end-of-turn drain makes progress.
            pty_flags |= PollFlags::OUT;
        }
        let mut fds = vec![
            PollFd::new(&pty, pty_flags),
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
        // Normally block until an fd is ready. But a pending upgrade is decided
        // by wall clock (its boundary window can lapse with the child idle and no
        // fd ever waking us), so while one is pending, cap the wait at the time
        // left until its deadline — the loop tail re-checks it every wake.
        let poll_timeout: Option<Timespec> = pending_upgrade.as_ref().map(|p| {
            let left = p.deadline.saturating_duration_since(Instant::now());
            Timespec {
                tv_sec: left.as_secs() as i64,
                tv_nsec: left.subsec_nanos() as i64,
            }
        });
        match poll(&mut fds, poll_timeout.as_ref()) {
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

        // Display client -> host, BEFORE the PTY is read: a query the PTY hands us
        // is answered by whoever is attached *now*, so who is attached must be the
        // freshest thing we know. See `service_display_client`.
        if client_re.intersects(PollFlags::IN | PollFlags::HUP) {
            match service_display_client(
                &mut client,
                &pty,
                &mut pty_out,
                &mut screen,
                &mut recorder,
                current_name,
                &mut meta,
                &mut last_theme,
                &attached_info,
                bell_marked,
                &mut pending_upgrade,
            )? {
                Disposition::Keep | Disposition::Drop => {}
                Disposition::Kill => {
                    kill_child(&mut child);
                    discard_traces(current_name, opts.record.as_deref());
                    return Ok(0);
                }
            }
        }

        // PTY output -> authoritative screen state, and live to the attached
        // client (if any). State is always tracked so the next attach can be
        // repainted even after a period with nobody attached.
        if pty_re.intersects(PollFlags::IN | PollFlags::HUP) {
            match (&pty).read(&mut ptybuf) {
                Ok(0) => {
                    return child_exited(
                        &mut child,
                        &mut client,
                        current_name,
                        opts.record.as_deref(),
                    );
                }
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
                    // Scanning consumed the queries, so this is the last moment
                    // anyone can answer them: whatever we decide here, the bytes are
                    // gone. `poll` told us who was attached when it returned, which
                    // is not the same as who is attached now — a client that closed
                    // in between still looks alive in `client_re`, and forwarding a
                    // query into its dead socket loses the query for good. So ask the
                    // socket itself, which answers with a read: a closed peer reads
                    // EOF no matter what the last poll believed.
                    if !asked.is_empty()
                        && client.is_some()
                        && matches!(
                            service_display_client(
                                &mut client,
                                &pty,
                                &mut pty_out,
                                &mut screen,
                                &mut recorder,
                                current_name,
                                &mut meta,
                                &mut last_theme,
                                &attached_info,
                                bell_marked,
                                &mut pending_upgrade,
                            )?,
                            Disposition::Kill
                        )
                    {
                        kill_child(&mut child);
                        discard_traces(current_name, opts.record.as_deref());
                        return Ok(0);
                    }
                    if client.is_none() && !asked.is_empty() {
                        let mode_state = |m: u16| screen.vt().dec_mode_state(m);
                        let ansi_mode_state = |m: u16| screen.vt().ansi_mode_state(m);
                        let checksum = |t, l, b, r| screen.vt().rect_checksum(t, l, b, r);
                        let palette = |i: u8| screen.vt().palette_color(i);
                        let special = |t| screen.vt().special_color(t);
                        let (lm, rm) = screen.vt().left_right_margins();
                        let (tm, bm) = screen.vt().top_bottom_margins();
                        let policy = screen.vt().policy();
                        let ctx = crate::query::ReplyCtx {
                            cursor: screen.cursor_report(),
                            size: screen.dimensions(),
                            policy,
                            // Detached: no window to iconify, and no display to
                            // maximize onto — answer from the nominal one, so a
                            // program's arithmetic stays sane until a client attaches.
                            display_size: crate::query::NOMINAL_DISPLAY_CHARS,
                            iconified: false,
                            // No window, so no true pixel sizes: report the nominal
                            // display and the cell a client would draw at, so a
                            // program's pixel arithmetic still adds up.
                            size_px: {
                                let (cols, rows) = screen.dimensions();
                                (
                                    u32::from(cols) * crate::query::NOMINAL_CELL_PX.0,
                                    u32::from(rows) * crate::query::NOMINAL_CELL_PX.1,
                                )
                            },
                            display_px: crate::query::NOMINAL_DISPLAY_PX,
                            cell_px: crate::query::NOMINAL_CELL_PX,
                            title: screen.title(),
                            icon_title: screen.icon_title(),
                            kitty_flags: screen.kitty_keyboard_flags(),
                            cursor_style: crate::query::decscusr_digit(screen.vt().cursor().shape),
                            left_right_margins: (lm as u16, rm as u16),
                            top_bottom_margins: (tm as u16, bm as u16),
                            sgr_report: screen.vt().sgr_report(),
                            decsca: screen.vt().decsca_report(),
                            conformance_level: screen.vt().conformance_level(),
                            ansi_mode_state: &ansi_mode_state,
                            // Detached, nobody sees the live scheme; answer
                            // with the last-attached client's colors (ghost's
                            // default if none ever attached), under any
                            // app-set dynamic overrides.
                            colors: screen.effective_colors(last_theme),
                            palette: &palette,
                            special: &special,
                            mode_state: &mode_state,
                            checksum: &checksum,
                        };
                        let mut reply = Vec::new();
                        for q in asked {
                            reply.extend_from_slice(&q.reply(&ctx));
                        }
                        pty_out.extend_from_slice(&reply);
                    }
                    // kitty graphics acknowledgements are stateful, so they come
                    // from the emulator rather than the scanner. Drain them every
                    // feed (so they never accumulate) but, like the queries above,
                    // only write them to the child while detached — an attached,
                    // graphics-capable outer terminal answers via the pipe.
                    let graphics_reply = screen.take_graphics_responses();
                    if client.is_none() && !graphics_reply.is_empty() {
                        pty_out.extend_from_slice(&graphics_reply);
                    }
                    // OSC 52 clipboard writes are applied by an attached
                    // frontend (which feeds its own emulator from the same
                    // stream); the host just drains its copy so they never
                    // accumulate — detached, there is no clipboard to write.
                    let _ = screen.take_clipboard_writes();
                    // Same for the window ops (XTWINOPS iconify/maximize/…): the
                    // attached frontend carries them out from its own emulator,
                    // and the host has no window to carry them out on.
                    let _ = screen.take_window_ops();
                    // Refresh the discoverable title when the child changes it
                    // (coalesced — only an actual change rewrites the meta file).
                    if screen.title() != meta.title {
                        meta.title = screen.title().to_string();
                        let _ = crate::meta::write(&paths::meta_path(current_name), &meta);
                    }
                    if let Some(r) = &mut recorder {
                        let _ = r.output(&ptybuf[..n]);
                        bytes_since_checkpoint += n;
                        // Never checkpoint mid-split-char: the checkpoint dump can't
                        // carry the pending UTF-8 tail, and truncation discards the
                        // frame that held the leading bytes, so replay from it would
                        // render the completing byte as U+FFFD. Defer to the next feed
                        // (a byte or two away) — the budget is left UNSPENT (we don't
                        // reach the reset below), so we retry on the very next chunk
                        // rather than a whole interval later.
                        if bytes_since_checkpoint >= checkpoint_interval && !screen.has_pending() {
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
                Err(_) => {
                    return child_exited(
                        &mut child,
                        &mut client,
                        current_name,
                        opts.record.as_deref(),
                    );
                }
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
                        &mut pty_out,
                        &mut screen,
                        &mut recorder,
                        current_name,
                        &mut meta,
                        &mut last_theme,
                        &attached_info,
                        bell_marked,
                        &mut pending_upgrade,
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
                        &mut pty_out,
                        &mut screen,
                        &mut recorder,
                        current_name,
                        &mut meta,
                        &mut last_theme,
                        &attached_info,
                        bell_marked,
                        &mut pending_upgrade,
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
                            // Attach handshake done -> promote to display client,
                            // taking over from any current one. Tell the outgoing
                            // client it was superseded FIRST: a bare EOF is
                            // ambiguous (a self-upgrade re-exec looks identical),
                            // and a local attach now reconnects on that ambiguity,
                            // so without this the dropped client would take the
                            // display straight back — an endless take-over war.
                            //
                            // Best-effort and NON-blocking: drop the outgoing
                            // client's (worthless, being-discarded) output backlog
                            // and send only the tiny farewell. A blocking flush
                            // here would WEDGE the whole host on a stalled display
                            // client (a frozen terminal / Ctrl-S — the usual reason
                            // the user is attaching from elsewhere), unrecoverably
                            // (SIGTERM can't interrupt the write). If the farewell
                            // can't land (the client's receive buffer is also
                            // full), that client reconnects once on resume and is
                            // superseded again — bounded, not a war.
                            if let Some(mut old) = client.take() {
                                old.conn.discard_queued();
                                let _ = old.conn.send(&ServerMsg::Superseded);
                            }
                            client = Some(p);
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
                &child_command,
                current_name,
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

        // Drain queued child-bound input as far as the non-blocking master will
        // accept, keeping the remainder for the next POLLOUT wake — so a child
        // that has stopped reading throttles the input rather than wedging the
        // loop. Runs every turn after all the sites that enqueue (client input,
        // detached query/graphics replies). A hard write error means the child is
        // gone: there is nowhere to deliver these bytes, so drop them and let the
        // read path drive the clean, tail-complete exit — finalizing from here
        // could truncate output still buffered on the master.
        while !pty_out.is_empty() {
            match (&pty).write(&pty_out) {
                Ok(0) => break,
                Ok(n) => {
                    pty_out.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    pty_out.clear();
                    break;
                }
            }
        }

        // A requested in-place self-upgrade fires only at a clean handoff
        // boundary (parser ground, no pending UTF-8, no in-flight graphics
        // chunk) and only once the child exists to adopt — until then we hold it
        // and re-check next turn. Placed after the input drain so queued
        // keystrokes reach the child first. On SUCCESS `self_upgrade` never
        // returns: this process image is replaced by the new host, which adopts
        // `child` on the same PTY. A return means the exec — or its reversible
        // prep — failed; we take the request (a one-shot attempt, so a
        // structural failure can't spin re-exec every turn) and run on as a
        // fully-working host. See `docs/host-self-upgrade.md`.
        if pending_upgrade.is_some()
            && pty_out.is_empty()
            && screen.at_boundary()
            // Don't start an upgrade with a terminating signal already visible
            // this turn: honor it PROMPTLY at the tail instead of first running
            // the (probe-bearing, up-to-5s) prep and only catching it in
            // `self_upgrade`'s pre-exec drain. That drain is the completeness
            // guarantee — a kill landing DURING the prep is still honored — this
            // is just the fast path for one already sitting in the pipe.
            && !sig_re.contains(PollFlags::IN)
            && let Some(pid) = child.as_ref().map(|c| c.id())
        {
            let path = pending_upgrade.take().and_then(|p| p.path);
            // Flush the recorder's buffered frame to disk BEFORE the exec so the
            // successor appends after COMPLETE data (it reopens the file to
            // continue it). If the flush fails (a full or erroring disk), the tail
            // may be a torn frame; appending past it would bury the tear mid-file
            // and the successor's whole recording would be silently discarded on
            // the next read. So refuse the upgrade on a flush error — the request
            // is already taken (one-shot), so it just doesn't happen and the host
            // runs on unchanged.
            let outcome = match &mut recorder {
                Some(r) => r.flush().err().map(|e| {
                    UpgradeOutcome::Refused(format!(
                        "cannot flush the recording before upgrading (refused): {e}"
                    ))
                }),
                None => None,
            }
            // On SUCCESS `self_upgrade` never returns (the image is replaced); a
            // return is a refusal (bad/incompatible target, downgrade, failed
            // reversible prep) or a kill that landed during the prep.
            .unwrap_or_else(|| {
                self_upgrade(
                    listener,
                    lock_fd,
                    &pty,
                    pid,
                    &screen,
                    opts,
                    launch_dir,
                    path.as_deref(),
                    &sfd,
                    current_name,
                )
            });
            match outcome {
                // Report the refusal to whoever asked so `ghost __upgrade` fails
                // loudly instead of waiting out its own deadline on a silent hold.
                UpgradeOutcome::Refused(message) => {
                    notify_upgrade_refused(&mut pending, &mut client, &message);
                }
                // A kill raced the prep and was pulled off the self-pipe — honor
                // it now, exactly as the loop's tail signal handler would.
                UpgradeOutcome::Terminated => {
                    kill_child(&mut child);
                    notify_exit(&mut client, 0);
                    return Ok(0);
                }
            }
        }

        // Bounded patience: if the request never reached a boundary within its
        // window (a child producing continuous output, or one that left the
        // parser stuck mid-escape and idled), abandon it and report why, rather
        // than hold the request — and the requester — indefinitely.
        if pending_upgrade
            .as_ref()
            .is_some_and(|p| Instant::now() >= p.deadline)
        {
            pending_upgrade = None;
            notify_upgrade_refused(
                &mut pending,
                &mut client,
                "the session never reached a clean upgrade boundary in time \
                 (a program producing continuous output, or stuck mid-escape?)",
            );
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
                // Preface the re-seed with the current grid, exactly as the
                // regridded path does: the observer rebuilds its mirror at the
                // host's size before the resync lands, so the dump — written for
                // that grid — never reflows onto a stale one.
                let (cols, rows) = screen.dimensions();
                s.queue(&ServerMsg::Event(crate::protocol::SessionEvent::Resized {
                    cols,
                    rows,
                }));
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

/// Read the display client once and act on whatever it sent, dropping it (to
/// `None`) if it has gone. Safe to call at any point in a turn: the socket is
/// non-blocking, so with nothing to say it reads as would-block and keeps the
/// client.
///
/// It exists to be called *twice* — once when the poll says the client is ready,
/// and again just before the host decides who answers a query — because those two
/// facts must not disagree. A `read` is the only thing that can tell us the client
/// is gone; `poll`'s readiness is a memory of the moment it returned.
#[allow(clippy::too_many_arguments)] // the host's whole mutable state, threaded once
fn service_display_client(
    client: &mut Option<Client>,
    pty: &pty_process::blocking::Pty,
    pty_out: &mut Vec<u8>,
    screen: &mut Screen,
    recorder: &mut Option<crate::record::FileRecorder>,
    current_name: &str,
    meta: &mut crate::meta::Meta,
    last_theme: &mut crate::query::ThemeColors,
    attached_info: &Option<crate::protocol::AttachInfo>,
    bell_marked: bool,
    pending_upgrade: &mut Option<PendingUpgrade>,
) -> io::Result<Disposition> {
    let Some(c) = client.as_mut() else {
        return Ok(Disposition::Keep);
    };
    let disposition = match c.conn.recv::<ClientMsg>() {
        Ok(None) => Disposition::Drop,
        Ok(Some(msgs)) => handle_client_messages(
            c,
            msgs,
            pty,
            pty_out,
            screen,
            recorder,
            current_name,
            meta,
            last_theme,
            attached_info,
            bell_marked,
            pending_upgrade,
        )?,
        Err(_) => Disposition::Drop,
    };
    if matches!(disposition, Disposition::Drop) {
        *client = None;
    }
    Ok(disposition)
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
    pty_out: &mut Vec<u8>,
    screen: &mut Screen,
    recorder: &mut Option<crate::record::FileRecorder>,
    current_name: &str,
    meta: &mut crate::meta::Meta,
    last_theme: &mut crate::query::ThemeColors,
    attached_info: &Option<crate::protocol::AttachInfo>,
    bell_marked: bool,
    pending_upgrade: &mut Option<PendingUpgrade>,
) -> io::Result<Disposition> {
    for msg in msgs {
        match msg {
            ClientMsg::Input(bytes) => {
                pty_out.extend_from_slice(&bytes);
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
            // The terminal the user is driving owns the policy; the rest are only
            // watching. So an observer's report is ignored — it has no window and no
            // clipboard at stake here, and letting a fleet preview tighten (or
            // loosen) the session someone else is typing in would be absurd.
            //
            // Adopting scrubs whatever a stricter policy now forbids, so the resync
            // below can't hand the client state its own policy would refuse — and
            // the *whole* screen is re-sent, because a scrub can change any cell's
            // color and drop images anywhere on it.
            ClientMsg::Policy(policy) if !c.subscribed && !c.observing => {
                if policy != screen.vt().policy() {
                    screen.set_policy(policy);
                    // Straight to disk (coalesced on an actual change, like the
                    // title and the grid): the descriptor is what a restarted host,
                    // a recreate or a resurrect reads, and a policy that lived only
                    // as long as this process would hand the session back to a
                    // program that had been refused the moment anything restarted.
                    meta.policy = policy;
                    let _ = crate::meta::write(&paths::meta_path(current_name), meta);
                    // And the *durable* descriptor, which is the one that outlives
                    // this host: `meta` is pruned with the session directory the
                    // moment we exit, so a recreate reads the descriptor or nothing
                    // at all. (A session whose child hasn't started yet has no
                    // descriptor; it gets written from `meta` when it does, policy
                    // and all.)
                    if let Some(mut d) = crate::descriptor::read(current_name) {
                        d.policy = policy;
                        let _ = crate::descriptor::write(current_name, &d);
                    }
                    if c.resynced {
                        c.queue_output(screen.resync());
                    }
                }
            }
            ClientMsg::Policy(_) => {}
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
            // Request an in-place self-upgrade. Like `Policy`, only a
            // display/control client may ask (never an observer — a fleet
            // preview must not re-exec the session someone is typing in). We
            // only RECORD the request here; the loop's tail performs it once the
            // screen is at a clean handoff boundary and a child exists to adopt.
            // The last request wins if several arrive before a boundary.
            ClientMsg::Upgrade { path } if !c.subscribed && !c.observing => {
                *pending_upgrade = Some(PendingUpgrade {
                    path,
                    deadline: Instant::now() + UPGRADE_BOUNDARY_WINDOW,
                });
                // Remember to answer THIS connection: on a refusal the loop tail
                // queues a `UpgradeResult` to every awaiting client (success
                // never returns to answer).
                c.awaiting_upgrade = true;
            }
            ClientMsg::Upgrade { .. } => {}
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
    if !crate::session::valid_display_name(new_name) {
        return Err(format!(
            "'{new_name}' is not a valid display name (1–64 characters, no control characters)"
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
    child: &mut Option<crate::child::Child>,
    client: &mut Option<Client>,
    name: &str,
    record: Option<&std::path::Path>,
) -> io::Result<i32> {
    let status = child.as_mut().and_then(|c| c.wait().ok());
    let code = status.as_ref().and_then(|s| s.code()).unwrap_or(0);
    // A child that exited of its own accord (WIFEXITED: the user typed
    // `exit`, or the command ran to completion) ended the session as
    // explicitly as a kill — its durable traces go with it. A signaled child
    // (a crash, a logout's SIGHUP) died uncleanly and stays resurrectable;
    // so does one already reaped by `kill_child`, whose cached wait status
    // is likewise signal-coded.
    if status.is_some_and(|s| s.code().is_some()) {
        discard_traces(name, record);
    }
    notify_exit(client, code);
    Ok(code)
}

/// An explicitly-ended session leaves nothing behind: drop the durable
/// descriptor and the recording this host was writing. The SIGTERM exit path
/// must never come here — external termination (a logout delivers exactly
/// that signal) keeps the session resurrectable, and `ghost kill` cleans up
/// on the killer's side instead.
fn discard_traces(name: &str, record: Option<&std::path::Path>) {
    crate::descriptor::remove(name);
    if let Some(p) = record {
        let _ = std::fs::remove_file(p);
    }
}

/// Build and spawn the session's child on the given PTY slave, honoring the
/// launch directory. Shared by eager start and deferred (first-attach) start.
fn spawn_child(
    command: &[String],
    session_name: &str,
    launch_dir: Option<&std::path::Path>,
    pts: pty_process::blocking::Pts,
) -> io::Result<crate::child::Child> {
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
    cmd = cmd
        .env("TERM", &term.term)
        .env("COLORTERM", "truecolor")
        .env("GHOST_SESSION_ID", session_name);
    if let Some(dirs) = &term.terminfo_dirs {
        cmd = cmd.env("TERMINFO_DIRS", dirs);
    }
    if let Some(dir) = launch_dir {
        cmd = cmd.current_dir(dir);
    }
    cmd.spawn(pts)
        .map(crate::child::Child::from_handle)
        .map_err(io::Error::other)
}

/// The child's current working directory, best-effort: Linux reads it from
/// `/proc`; elsewhere (or on any error) `None`, and the descriptor keeps the
/// launch directory.
fn child_cwd(child: &Option<crate::child::Child>) -> Option<std::path::PathBuf> {
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
            connection: meta.connection.clone(),
            policy: meta.policy,
        },
    );
}

/// Kill and reap the child if one has been spawned; a no-op for a deferred
/// session whose child never started.
fn kill_child(child: &mut Option<crate::child::Child>) {
    if let Some(c) = child {
        c.kill();
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

/// The child argv for a session: a connection spec (if any) wins and derives
/// its own launcher argv; otherwise the literal command (empty is left as-is,
/// resolved to `$SHELL` by [`split_command`] at spawn). A spec paired with a
/// non-empty command is contradictory and rejected.
fn effective_command(
    command: &[String],
    connection: Option<&crate::connection::ConnectionSpec>,
) -> io::Result<Vec<String>> {
    match connection {
        Some(_) if !command.is_empty() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a session with a connection cannot also set a command",
        )),
        Some(spec) => Ok(spec.argv()),
        None => Ok(command.to_vec()),
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
    use crate::connection::ConnectionSpec;
    use crate::record::DEFAULT_MAX_RECORDING_BYTES;

    #[test]
    fn spawn_opts_with_a_connection_round_trip_through_postcard() {
        // SpawnOpts crosses to the re-exec'd host as a postcard blob
        // (`encode_host_args`); a connection spec must survive that intact, or
        // the host silently drops the spawn.
        let opts = SpawnOpts {
            name: "work".into(),
            command: Vec::new(),
            size: (80, 24),
            cwd: None,
            record: None,
            seed_from: None,
            scrollback: 100,
            max_recording_bytes: None,
            start_on_attach: true,
            connection: ConnectionSpec::parse_target("dev@example").map(|mut s| {
                s.port = Some(2222);
                s
            }),
        };
        let bytes = postcard::to_allocvec(&opts).unwrap();
        let back: SpawnOpts = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back.connection, opts.connection);
    }

    #[test]
    fn effective_command_resolves_spec_command_and_shell() {
        // No connection, empty command: left empty for `split_command` → $SHELL.
        assert_eq!(effective_command(&[], None).unwrap(), Vec::<String>::new());
        // No connection, explicit command: used verbatim.
        let cmd = vec!["vim".to_string(), "main.rs".to_string()];
        assert_eq!(effective_command(&cmd, None).unwrap(), cmd);
        // A connection derives the launcher argv and beats an empty command.
        let spec = ConnectionSpec::parse_target("kov@box").unwrap();
        assert_eq!(
            effective_command(&[], Some(&spec)).unwrap(),
            vec!["ssh", "kov@box"]
        );
        // A connection *and* a command is contradictory → rejected.
        assert!(effective_command(&cmd, Some(&spec)).is_err());
    }

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
