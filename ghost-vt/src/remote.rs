//! The SSH-as-transport *initiator*: from the local machine, reach a host that
//! may run ghost, decide whether it can host one, spawn a host there, and hand
//! back the `ssh … __pipe` command the client attaches over.
//!
//! All ssh invocations share one multiplexed connection (`ControlMaster=auto` +
//! a per-target `ControlPath` + `ControlPersist`), so a single authentication —
//! the [`warmup`](RemoteSsh::warmup_argv), run once up front in a PTY — covers
//! the later probe, spawn, and attach, which reuse the open master with no
//! re-auth (without it a password-auth host would prompt on every invocation).

use crate::connection::ConnectionSpec;
use std::collections::HashMap;
use std::io;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// How long a control-socket health check (`ssh -O check`) may run before we treat
/// the master as wedged. A healthy master answers in milliseconds; a master whose
/// TCP died (laptop slept, network changed) but whose process lingers under
/// `ControlPersist` hangs — this bounds that hang.
const MASTER_CHECK_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a `__probe` reusing the master may run before we give up. A backstop:
/// [`RemoteSsh::reap_wedged_master`] normally clears a dead master first, so a probe
/// runs over a fresh one; this only bites if a master wedges mid-negotiation.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a no-op forced over the existing master may run before we treat the
/// master as wedged. `ssh -O check` can't see a peer that vanished silently (a
/// remote reboot / partition drops the link with no FIN/RST), so
/// [`RemoteSsh::master_alive`] confirms with this bounded round-trip; a healthy
/// master answers in milliseconds.
const WEDGE_PROBE_TIMEOUT: Duration = Duration::from_secs(4);
/// How long a `__proto` read (a tiny forced command over the shared master) may run
/// before we give up and fall back. A healthy master answers in milliseconds; this
/// only bites if the master silently wedged, and a caller runs it on the event loop.
const PROTO_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// Wait for `child`, killing it (and returning `None`) if it outruns `timeout`.
/// Polls rather than blocking so a wedged ssh can't hang the caller forever. Used
/// only where the child produces little output (a control command, a probe line), so
/// its pipe never fills while we poll.
fn wait_bounded(child: &mut Child, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
}

/// Env override for the remote ghost binary. Points the initiator at a specific
/// remote path (e.g. a shared build); when set, staging is skipped and the path
/// is used as-is.
const REMOTE_GHOST_ENV: &str = "GHOST_REMOTE_GHOST";

/// Progress of a staging copy: `sent` of `total` bytes written toward the remote.
/// Reported by [`RemoteSsh::negotiate_with_progress`] so a GUI can draw a copy bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StageProgress {
    pub sent: u64,
    pub total: u64,
}

/// Why [`RemoteSsh::negotiate`] could not put a protocol-matched ghost on the
/// remote — the reason a connect degrades to a local ssh child. Carried (instead
/// of a bare `None`) so the connect prompt can show it and offer a
/// reason-appropriate choice, and so logs/tests stop seeing a silent fallback.
/// [`Self::retryable`] separates the transient failures (worth a Retry) from the
/// structural ones (where retrying is futile).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NegotiateFailure {
    /// `GHOST_REMOTE_GHOST` was set but that binary didn't answer our probe
    /// (missing, or below our protocol level).
    EnvOverrideUnusable,
    /// Couldn't interrogate the remote (its `$HOME` didn't resolve) — often
    /// transient (a flaky link, a noisy login shell); a retry may work.
    RemoteUnready,
    /// The remote's `uname` named an OS/arch we don't map, so nothing is stageable.
    UnknownPlatform,
    /// The remote's platform is known but we have no binary for it — our own exe
    /// doesn't match and no `ghost-<os>-<arch>` prebuilt was found. A prebuilt must
    /// be provided (`cargo xtask prebuilt`); retrying won't help.
    NoPrebuilt { platform: String },
    /// Copying the binary to the remote failed (disk full, a dropped link); a retry
    /// may work.
    StageFailed(String),
    /// The binary copied but wouldn't run there (an old libc, a `noexec` home): it
    /// staged yet still doesn't answer the probe. Retrying won't help.
    StagedButUnrunnable,
}

impl NegotiateFailure {
    /// Whether re-running negotiation could plausibly succeed — a transient
    /// failure, versus a structural one (no binary for the platform, or a binary
    /// that won't run there) where a Retry only wastes the user's click.
    ///
    /// `StagedButUnrunnable` is treated as structural: the copy succeeded but the
    /// binary doesn't run there (old libc, `noexec` home), which a retry won't fix.
    /// A retry *would* re-stage (the post-stage probe failed, so the staged path
    /// isn't trusted), re-uploading the whole ~126 MiB binary per click — the exact
    /// futile-click cost this split exists to avoid — so it stays non-retryable even
    /// though a probe can, rarely, fail transiently right after a good upload.
    pub fn retryable(&self) -> bool {
        matches!(self, Self::RemoteUnready | Self::StageFailed(_))
    }
}

impl std::fmt::Display for NegotiateFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EnvOverrideUnusable => {
                write!(f, "the GHOST_REMOTE_GHOST binary didn't answer our probe")
            }
            Self::RemoteUnready => write!(f, "couldn't read the remote's home directory"),
            Self::UnknownPlatform => {
                write!(f, "the remote runs an OS/arch ghost doesn't support")
            }
            Self::NoPrebuilt { platform } => {
                write!(
                    f,
                    "no prebuilt ghost for the remote's platform ({platform})"
                )
            }
            Self::StageFailed(e) => write!(f, "staging ghost to the remote failed: {e}"),
            Self::StagedButUnrunnable => {
                write!(f, "ghost copied to the remote but wouldn't run there")
            }
        }
    }
}

/// The remote directory a staged ghost lands in, under the given remote `$HOME`.
fn staged_dir(home: &str) -> String {
    format!("{home}/.cache/ghost/bin")
}

/// Where a staged ghost lands on the remote (absolute, so it needs no shell
/// tilde/`$HOME` expansion at exec time), stamped with the version *and* a hash
/// of this binary's contents so a *changed* build never reuses a stale copy —
/// crucial during development, where the version string doesn't move between
/// builds but the binary (and its features) do.
fn staged_path(home: &str, stamp: &str) -> String {
    format!(
        "{}/ghost-{}-{stamp}",
        staged_dir(home),
        env!("CARGO_PKG_VERSION")
    )
}

/// How many staged binaries to keep in the remote cache after a stage: the
/// just-copied one plus a couple of recent builds, so flipping between two builds
/// doesn't re-copy while a dev's rebuild-reconnect loop can't pile up ~126 MiB
/// per build forever.
const STAGED_KEEP: usize = 3;

/// A POSIX-sh one-liner (run on the remote after a stage) that keeps the newest
/// `keep` `ghost-*` files in `dir` and deletes the older ones. No-ops on a missing
/// dir or an empty match (`ls` errors are swallowed). Unlinking a binary a running
/// remote host still uses is safe on Unix — the inode lives until that process
/// exits, and a later connect for that exact build just re-stages it.
fn prune_script(dir: &str, keep: usize) -> String {
    format!(
        "cd {dir} 2>/dev/null || exit 0; \
         ls -t ghost-* 2>/dev/null | tail -n +{} | \
         while IFS= read -r f; do rm -f -- \"$f\"; done",
        keep + 1
    )
}

/// A short content hash of the ghost binary at `path`, memoized per path for the
/// process. Stamps the staged path so a *changed* build re-stages while an
/// identical one reuses the existing copy — crucial in development, where the
/// version string doesn't move between builds. `"unknown"` if the file can't be
/// read (staging then falls back to version-only stamping for that binary).
fn build_stamp(path: &Path) -> String {
    use std::hash::{Hash as _, Hasher as _};
    static STAMPS: std::sync::OnceLock<std::sync::Mutex<HashMap<PathBuf, String>>> =
        std::sync::OnceLock::new();
    let cache = STAMPS.get_or_init(Default::default);
    if let Some(s) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(path) {
        return s.clone();
    }
    let stamp = std::fs::read(path)
        .map(|bytes| {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            bytes.hash(&mut h);
            format!("{:016x}", h.finish())
        })
        .unwrap_or_else(|_| "unknown".into());
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(path.to_path_buf(), stamp.clone());
    stamp
}

/// A machine's OS + arch in Rust's [`std::env::consts`] vocabulary
/// (`os` = `linux`/`macos`, `arch` = `x86_64`/`aarch64`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Platform {
    os: &'static str,
    arch: &'static str,
}

/// This build's own platform.
fn local_platform() -> Platform {
    Platform {
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
    }
}

/// Parse a remote `uname -s -m` into a [`Platform`]. `None` for anything we don't
/// map (an unknown OS/arch, or malformed output) — the caller then can't stage.
fn parse_platform(uname_sm: &str) -> Option<Platform> {
    let mut it = uname_sm.split_whitespace();
    let (sys, machine) = (it.next()?, it.next()?);
    let os = match sys {
        "Linux" => "linux",
        "Darwin" => "macos",
        _ => return None,
    };
    let arch = match machine {
        "x86_64" | "amd64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        _ => return None,
    };
    Some(Platform { os, arch })
}

/// Where to look for a cross-arch prebuilt ghost, in order: the
/// `GHOST_PREBUILT_DIR` override, then the durable cache `<data_dir>/prebuilt`. A
/// file named `ghost-<os>-<arch>` (e.g. `ghost-macos-aarch64`) in one of these is
/// staged to a remote of that platform. A future network provider would fetch into
/// the cache dir and be found here with no other change.
fn prebuilt_search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(d) = std::env::var_os("GHOST_PREBUILT_DIR") {
        dirs.push(PathBuf::from(d));
    }
    dirs.push(crate::paths::data_dir().join("prebuilt"));
    dirs
}

/// Pick the local ghost binary to stage to a `target` remote: our own executable
/// when the remote matches this build, else the first `ghost-<os>-<arch>` prebuilt
/// found in `search`. `None` ⇒ nothing stageable (the caller falls back to the ssh
/// child). Split out from [`resolve_ghost_binary`] so it's testable without env.
fn resolve_for(
    target: Platform,
    local: Platform,
    current_exe: Option<&Path>,
    search: &[PathBuf],
) -> Option<PathBuf> {
    if target == local {
        return current_exe.map(Path::to_path_buf);
    }
    let file = format!("ghost-{}-{}", target.os, target.arch);
    search.iter().map(|d| d.join(&file)).find(|c| c.is_file())
}

/// The ghost binary to stage to a `target` remote, resolved against this build and
/// the real [`prebuilt_search_dirs`]. `None` ⇒ fall back to the ssh child.
fn resolve_ghost_binary(target: Platform) -> Option<PathBuf> {
    resolve_for(
        target,
        local_platform(),
        std::env::current_exe().ok().as_deref(),
        &prebuilt_search_dirs(),
    )
}

/// The marker line `ghost __probe` prints, so the initiator can tell a real
/// remote ghost from anything else that happens to exit cleanly (e.g. a shell
/// that echoed the command). Carries the host's `PROTO_LEVEL` for a future
/// compatibility gate.
pub const PROBE_MARKER: &str = "ghost-transport";

/// The line `ghost __probe` emits: the [`PROBE_MARKER`] plus the protocol level
/// this binary speaks.
pub fn probe_line() -> String {
    format!("{PROBE_MARKER} proto={}", crate::protocol::PROTO_LEVEL)
}

/// Whether a `__probe` reply is from a remote ghost this initiator can actually
/// speak to: it must carry the [`PROBE_MARKER`] AND advertise a protocol level at
/// least our own, so every frame we may send is one the remote can decode.
///
/// A remote *below* our level — or one too old to print `proto=` at all — is
/// rejected even though it answered, so [`negotiate`](RemoteSsh::negotiate) skips
/// it and stages a version-matched copy rather than preferring a stale `ghost` on
/// the remote `PATH` that would mis-decode our newer frames (postcard tags enum
/// variants positionally; see [`crate::protocol`]).
fn probe_reply_speaks_our_protocol(reply: &str) -> bool {
    if !reply.contains(PROBE_MARKER) {
        return false;
    }
    reply
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix("proto="))
        .and_then(|level| level.parse::<u32>().ok())
        .is_some_and(|proto| proto >= crate::protocol::PROTO_LEVEL)
}

/// One multiplexed ssh connection to a host, for the transport initiator.
pub struct RemoteSsh {
    spec: ConnectionSpec,
    control_path: PathBuf,
}

impl RemoteSsh {
    /// A connection to `spec`'s host, with a per-target control socket under the
    /// runtime dir (created if missing).
    pub fn new(spec: ConnectionSpec) -> io::Result<Self> {
        Self::new_in(spec, &crate::paths::runtime_dir())
    }

    /// Like [`new`](Self::new) but with an explicit directory for the control
    /// socket. Tests inject a short path: a unix socket's `sun_path` is capped at
    /// 108 bytes, so a deep tempdir would overflow `ControlPath`.
    pub fn new_in(spec: ConnectionSpec, dir: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let control_path = dir.join(format!("ssh-{}.ctl", sanitize(&spec.target())));
        Ok(RemoteSsh { spec, control_path })
    }

    /// The connection's target (`[user@]host`), e.g. for a progress message.
    pub fn target(&self) -> String {
        self.spec.target()
    }

    /// The connection spec this transport was opened for — so a session driven
    /// over it can hand its host down to a new session that inherits it (a remote
    /// session has no local descriptor to read the spec from).
    pub fn spec(&self) -> &ConnectionSpec {
        &self.spec
    }

    /// The argv of the one-shot ssh that opens (and authenticates) the shared
    /// ControlMaster: it runs `true` on the host and exits. Spawn it on a PTY so
    /// ssh prompts for a password on the tty, which the caller feeds through; once
    /// it exits 0 the master is open and every later invocation
    /// ([`negotiate`](Self::negotiate), [`spawn_host`](Self::spawn_host),
    /// [`pipe_command`](Self::pipe_command)) reuses it with no further auth. Returned
    /// as an argv (not a [`Command`]) so a caller can spawn it on a pty.
    pub fn warmup_argv(&self) -> Vec<String> {
        self.argv(&["true"])
    }

    /// ssh options shared by every invocation: one authenticated, multiplexed
    /// connection (`ControlMaster`), and `accept-new` host keys so an unknown
    /// host doesn't block on a confirmation prompt (a *changed* key still errors).
    fn control_opts(&self) -> Vec<String> {
        vec![
            "-o".into(),
            "ControlMaster=auto".into(),
            "-o".into(),
            // Double-quote the path: ssh parses each `-o` as a config line and
            // splits the value on whitespace, so an unquoted path with a space
            // (macOS's `~/Library/Application Support/ghost`) trips "keyword
            // controlpath extra arguments at end of line". Quotes are stripped by
            // ssh's own parser, so this is a no-op for space-free paths.
            format!("ControlPath=\"{}\"", self.control_path.display()),
            "-o".into(),
            "ControlPersist=60".into(),
            "-o".into(),
            "StrictHostKeyChecking=accept-new".into(),
            // Keepalives so a persisting master notices a dead peer (laptop slept /
            // network changed) and EXITS after ~45 s, rather than lingering wedged for
            // the next reuse to hang on. Harmless on a multiplexed client (the master
            // owns the TCP); the point is the master it opens carries them.
            "-o".into(),
            "ServerAliveInterval=15".into(),
            "-o".into(),
            "ServerAliveCountMax=3".into(),
        ]
    }

    /// Argv for an ssh control command (`-O <op>`, e.g. `check` / `exit`) against
    /// this connection's master socket. Only the `ControlPath` matters for `-O`, in
    /// the same double-quoted form (a space in the macOS runtime path).
    fn control_argv(&self, op: &str) -> Vec<String> {
        vec![
            "ssh".into(),
            "-o".into(),
            format!("ControlPath=\"{}\"", self.control_path.display()),
            "-O".into(),
            op.into(),
            self.spec.target(),
        ]
    }

    /// Spawn `argv` with null stdio and return whether it exits 0 within `bound`
    /// ([`wait_bounded`] kills and reports false on overrun).
    fn run_bounded(&self, argv: &[String], bound: Duration) -> bool {
        let Ok(mut child) = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return false;
        };
        matches!(wait_bounded(&mut child, bound), Some(s) if s.success())
    }

    /// Whether the shared master is alive AND its connection is actually usable.
    ///
    /// `ssh -O check` proves only that the local master PROCESS answers the control
    /// socket. A master whose peer vanished *silently* — a remote reboot or network
    /// partition drops the link with no FIN/RST — keeps reporting "running" for the
    /// whole `ServerAlive` keepalive window (~45s). Trusting `-O check` there leaves
    /// [`reap_wedged_master`](Self::reap_wedged_master) skipping the corpse, so every
    /// reuse (and every reconnect after the host returns) multiplexes onto a dead
    /// connection and hangs. So confirm with a bounded no-op forced *over* the
    /// existing master (`ControlMaster=no` reuses it, never opens a fresh one): a
    /// healthy master answers in milliseconds; a wedged one hangs until the bound
    /// and reads as not-alive, so the master is reaped and the next connect opens a
    /// fresh one.
    fn master_alive(&self) -> bool {
        if !self.run_bounded(&self.control_argv("check"), MASTER_CHECK_TIMEOUT) {
            return false;
        }
        let opts = vec![
            "-o".into(),
            "ControlMaster=no".into(),
            "-o".into(),
            format!("ControlPath=\"{}\"", self.control_path.display()),
            "-o".into(),
            "BatchMode=yes".into(),
        ];
        self.run_bounded(
            &self.spec.ssh_command(&opts, &["true"]),
            WEDGE_PROBE_TIMEOUT,
        )
    }

    /// Clear a wedged master before reusing the connection: if a control socket exists
    /// but its master no longer answers ([`master_alive`](Self::master_alive) is false
    /// — the TCP died while `ControlPersist` kept the process, or the socket is stale),
    /// ask it to exit and remove the socket so the next `ControlMaster=auto` opens a
    /// FRESH master. Without this, every reuse (probe, warmup, attach) multiplexes onto
    /// the dead master and hangs forever. A healthy master is left untouched; a fresh
    /// connect (no socket yet) is a no-op — no ssh is spawned.
    ///
    /// Public for callers that open the master *themselves* rather than through
    /// [`open_master_batch`](Self::open_master_batch) / [`negotiate`](Self::negotiate)
    /// — the GUI's interactive connect spawns [`warmup_argv`](Self::warmup_argv) on a
    /// PTY, and a stale socket there would make ssh "disable multiplexing": the
    /// warm-up would authenticate a one-shot connection and leave no master for the
    /// PTY-less probes that follow.
    pub fn reap_wedged_master(&self) {
        if !self.control_path.exists() || self.master_alive() {
            return;
        }
        let argv = self.control_argv("exit");
        if let Ok(mut child) = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            let _ = wait_bounded(&mut child, MASTER_CHECK_TIMEOUT);
        }
        // Belt and braces: `-O exit` normally removes the socket, but a truly stale one
        // (no master to answer) needs the file cleared so a fresh master can bind it.
        let _ = std::fs::remove_file(&self.control_path);
    }

    /// The full ssh argv running `remote` on the host over the shared connection.
    fn argv(&self, remote: &[&str]) -> Vec<String> {
        self.spec.ssh_command(&self.control_opts(), remote)
    }

    /// An [`std::process::Command`] running `remote` on the host over the shared
    /// (already-authenticated) connection.
    fn command(&self, remote: &[&str]) -> Command {
        let argv = self.argv(remote);
        let mut c = Command::new(&argv[0]);
        c.args(&argv[1..]);
        c
    }

    /// [`negotiate_with_progress`](Self::negotiate_with_progress) without progress
    /// reporting — for callers (the CLI) that don't render a staging bar.
    pub fn negotiate(&self) -> Result<String, NegotiateFailure> {
        self.negotiate_with_progress(&mut |_| {})
    }

    /// Open the shared ControlMaster non-interactively, for a background reconnect
    /// (startup restore) where there is no tty to prompt on. `BatchMode=yes` makes
    /// a host that needs a password fail fast instead of hanging or popping an
    /// askpass dialog; `ConnectTimeout` bounds an unreachable host; null stdio so
    /// the backgrounded master holds no captured pipe open. Returns whether the
    /// master opened — key/agent auth only. A later [`negotiate`](Self::negotiate)
    /// reuses the now-open master with no further auth. Not for the interactive
    /// flow, which opens the master on a PTY so ssh CAN prompt.
    pub fn open_master_batch(&self) -> bool {
        // A master left wedged by a prior run (its TCP died while `ControlPersist` kept
        // the process) would make this `true` multiplex onto it and hang; clear it so we
        // open a fresh one.
        self.reap_wedged_master();
        let mut opts = self.control_opts();
        opts.extend([
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "ConnectTimeout=10".into(),
        ]);
        let argv = self.spec.ssh_command(&opts, &["true"]);
        Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Find (or provision) a usable remote ghost. `Some(remote_ghost)` ⇒ use the
    /// transport with that binary; `None` ⇒ fall back to the local ssh child. This
    /// is also where the shared connection first authenticates. In order:
    ///
    /// 1. `GHOST_REMOTE_GHOST` — used as-is (no staging), or `None` if it doesn't
    ///    answer.
    /// 2. `ghost` on the remote `PATH`.
    /// 3. an already-staged, version-stamped copy in the remote cache.
    /// 4. staging: copy our own binary over (OS+arch permitting), then re-probe.
    ///
    /// `on_progress` is called during staging (step 4) with the running byte count,
    /// so a GUI can show a copy progress bar; it's a no-op for the other steps.
    pub fn negotiate_with_progress(
        &self,
        on_progress: &mut dyn FnMut(StageProgress),
    ) -> Result<String, NegotiateFailure> {
        use NegotiateFailure as F;
        // Clear a wedged master up front so the probes below open (and reuse) a fresh
        // one instead of hanging on a dead socket left by a prior run.
        self.reap_wedged_master();
        if let Ok(path) = std::env::var(REMOTE_GHOST_ENV) {
            return self
                .probe(&path)
                .then_some(path)
                .ok_or(F::EnvOverrideUnusable);
        }
        if self.probe("ghost") {
            return Ok("ghost".to_string());
        }
        // Staging needs the remote home (for an absolute path) and its platform
        // (to pick the binary — our own exe for a matching host, else a prebuilt).
        let home = self.remote_home().ok_or(F::RemoteUnready)?;
        let platform = self.remote_platform()?;
        let binary = resolve_ghost_binary(platform).ok_or_else(|| F::NoPrebuilt {
            platform: format!("{}-{}", platform.os, platform.arch),
        })?;
        let staged = staged_path(&home, &build_stamp(&binary));
        if self.probe(&staged) {
            return Ok(staged);
        }
        match self.stage(&binary, &home, &staged, on_progress) {
            Ok(()) if self.probe(&staged) => Ok(staged),
            Ok(()) => Err(F::StagedButUnrunnable),
            Err(e) => Err(F::StageFailed(e.to_string())),
        }
    }

    /// The remote user's `$HOME` (expanded by the remote shell), for building
    /// absolute staged paths. `None` if it can't be resolved.
    fn remote_home(&self) -> Option<String> {
        let out = self
            .command(&["sh", "-c", "printf %s \"$HOME\""])
            .output()
            .ok()?;
        let home = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (out.status.success() && !home.is_empty()).then_some(home)
    }

    /// The remote's OS+arch from `uname -s -m`, so staging can pick a binary for
    /// it. Distinguishes a failed interrogation ([`RemoteUnready`] — the ssh
    /// command didn't run/exit cleanly, a transient link failure worth a retry)
    /// from a platform we simply don't map ([`UnknownPlatform`] — structural):
    /// they earn different reasons and different Retry offers, so they must not
    /// collapse to one `None`.
    ///
    /// [`RemoteUnready`]: NegotiateFailure::RemoteUnready
    /// [`UnknownPlatform`]: NegotiateFailure::UnknownPlatform
    fn remote_platform(&self) -> Result<Platform, NegotiateFailure> {
        let out = self
            .command(&["uname", "-s", "-m"])
            .output()
            .map_err(|_| NegotiateFailure::RemoteUnready)?;
        if !out.status.success() {
            return Err(NegotiateFailure::RemoteUnready);
        }
        parse_platform(&String::from_utf8_lossy(&out.stdout))
            .ok_or(NegotiateFailure::UnknownPlatform)
    }

    /// Run `<candidate> __probe` and confirm the reply carries the
    /// [`PROBE_MARKER`] — a clean exit is not enough (a shell that echoed the
    /// command line would pass a looser check).
    fn probe(&self, candidate: &str) -> bool {
        let mut cmd = self.command(&[candidate, "__probe"]);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let Ok(mut child) = cmd.spawn() else {
            return false;
        };
        // Bounded so a master that wedged mid-negotiation can't hang the probe forever
        // (`reap_wedged_master` clears a KNOWN-dead master up front; this is a backstop).
        if !matches!(wait_bounded(&mut child, PROBE_TIMEOUT), Some(s) if s.success()) {
            return false;
        }
        let mut buf = String::new();
        if let Some(mut out) = child.stdout.take() {
            use std::io::Read as _;
            let _ = out.read_to_string(&mut buf);
        }
        probe_reply_speaks_our_protocol(&buf)
    }

    /// Copy `binary` (the resolver's pick — our own exe or a prebuilt for the
    /// remote's platform) to `staged` on the remote, so a host that lacks ghost can
    /// still run one. Terminfo is not shipped: the remote host self-provisions it on
    /// first run (needs remote `tic`). Reports byte progress through `on_progress`
    /// so a GUI can draw a copy bar. A wrong binary that copies fine still fails the
    /// caller's post-stage `probe`, so it falls back cleanly.
    fn stage(
        &self,
        binary: &Path,
        home: &str,
        staged: &str,
        on_progress: &mut dyn FnMut(StageProgress),
    ) -> io::Result<()> {
        let bytes = std::fs::read(binary)?;
        let total = bytes.len() as u64;
        eprintln!(
            "ghost: staging ghost ({} MiB) to {}…",
            bytes.len() / (1 << 20),
            self.spec.target(),
        );

        // Write atomically: stream into a temp path, chmod, then rename over the
        // final name so a concurrent connect never sees a half-copied binary.
        let dir = staged_dir(home);
        let script = format!(
            "mkdir -p {dir} && cat > {staged}.tmp && chmod +x {staged}.tmp && mv {staged}.tmp {staged}"
        );
        let mut child = self
            .command(&["sh", "-c", &script])
            .stdin(Stdio::piped())
            .spawn()?;
        // Stream in chunks and report progress; the pipe's bounded buffer makes
        // each `write_all` block until ssh forwards it, so the count tracks the
        // upload closely enough for a bar. `sent == total` marks the copy done.
        let mut stdin = child.stdin.take().expect("piped stdin");
        on_progress(StageProgress { sent: 0, total });
        let mut sent = 0u64;
        for chunk in bytes.chunks(1 << 20) {
            stdin.write_all(chunk)?;
            sent += chunk.len() as u64;
            on_progress(StageProgress { sent, total });
        }
        drop(stdin); // close the pipe so the remote `cat` sees EOF and exits
        if !child.wait()?.success() {
            return Err(io::Error::other("copying the binary to the remote failed"));
        }
        self.prune_staged(&dir);
        Ok(())
    }

    /// Sweep older staged binaries out of `dir`, keeping the newest [`STAGED_KEEP`]
    /// (the just-staged one plus a couple of recent builds). Best-effort: a failure
    /// here never fails a connect, so it's fire-and-forget over the shared conn.
    fn prune_staged(&self, dir: &str) {
        let _ = self
            .command(&["sh", "-c", &prune_script(dir, STAGED_KEEP)])
            .output();
    }

    /// Enumerate the remote host's sessions by running `<remote_ghost> ls --json`
    /// over the shared connection and parsing the listing — the remote half of the
    /// fleet. Reuses the open ControlMaster (no auth), so a fleet can poll it
    /// cheaply. Errors on a non-zero exit or unparseable output.
    pub fn list_sessions(
        &self,
        remote_ghost: &str,
    ) -> io::Result<Vec<crate::session::SessionInfo>> {
        let out = self.command(&[remote_ghost, "ls", "--json"]).output()?;
        if !out.status.success() {
            return Err(io::Error::other(format!(
                "remote `ghost ls` failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        serde_json::from_slice(&out.stdout).map_err(io::Error::other)
    }

    /// The protocol level of the *running host* serving remote session `name`
    /// (`<remote_ghost> __proto <name>`), read from that session's `proto` marker
    /// over the shared connection. A staged binary can be newer than a host still
    /// serving an older session (`negotiate` never restarts running hosts), so an
    /// attach/observe must gate its post-marker messages on THIS, not the binary's
    /// level, or an old host drops the client on a message it can't decode.
    ///
    /// Falls back to the initiator's own [`PROTO_LEVEL`](crate::protocol::PROTO_LEVEL)
    /// when the read fails — an older remote binary lacks `__proto` (unknown
    /// subcommand), which leaves today's optimistic assumption in place rather than
    /// silently disabling features against a genuinely-current host; the read only
    /// changes behavior where it succeeds and reports a *lower* level.
    ///
    /// Bounded ([`PROTO_READ_TIMEOUT`]): a caller runs this on the event loop, and a
    /// silently-dead master (reboot/partition, no FIN/RST) would otherwise hang the
    /// tiny read until ssh's keepalive gives up (~45s), freezing every window — the
    /// same bound [`probe`](Self::probe)/[`negotiate`](Self::negotiate) apply. A
    /// timeout is a failed read, so it falls back to `PROTO_LEVEL`.
    pub fn session_proto(&self, remote_ghost: &str, name: &str) -> u32 {
        let level = || -> Option<u32> {
            let mut child = self
                .command(&[remote_ghost, "__proto", name])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .ok()?;
            if !matches!(wait_bounded(&mut child, PROTO_READ_TIMEOUT), Some(s) if s.success()) {
                return None;
            }
            use std::io::Read as _;
            let mut buf = String::new();
            child.stdout.take()?.read_to_string(&mut buf).ok()?;
            buf.trim().parse().ok()
        };
        level().unwrap_or(crate::protocol::PROTO_LEVEL)
    }

    /// Kill a remote session by id over the shared connection (`<remote_ghost>
    /// kill <name>`). For the fleet's kill action on a remote tile.
    pub fn kill_session(&self, remote_ghost: &str, name: &str) -> io::Result<()> {
        let out = self.command(&[remote_ghost, "kill", name]).output()?;
        if !out.status.success() {
            return Err(io::Error::other(format!(
                "remote `ghost kill` failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(())
    }

    /// Restart a remote session's host under the staged (current) binary, keeping
    /// its screen (`<remote_ghost> __restart <name>`), over the shared connection.
    /// The remote host is ended gracefully and respawned seeded from its recording,
    /// so a session that was served by an OLDER host comes back speaking the current
    /// protocol level. The running program on the remote is lost.
    pub fn restart_session(&self, remote_ghost: &str, name: &str) -> io::Result<()> {
        let out = self.command(&[remote_ghost, "__restart", name]).output()?;
        if !out.status.success() {
            return Err(io::Error::other(format!(
                "remote `ghost __restart` failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(())
    }

    /// Rename a remote session over the shared connection (`<remote_ghost> rename
    /// <old> <new>`). For the fleet's rename action on a remote tile.
    pub fn rename_session(&self, remote_ghost: &str, old: &str, new: &str) -> io::Result<()> {
        let out = self.command(&[remote_ghost, "rename", old, new]).output()?;
        if !out.status.success() {
            return Err(io::Error::other(format!(
                "remote `ghost rename` failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(())
    }

    /// Ensure a detached remote host named `name` exists. A fresh session is
    /// created; a failure here (the name already hosts a live session) is
    /// tolerated — the caller then attaches to whatever is there.
    pub fn spawn_host(&self, remote_ghost: &str, name: &str) -> io::Result<()> {
        let out = self.command(&[remote_ghost, "new", "-d", name]).output()?;
        if !out.status.success() {
            eprintln!(
                "ghost: remote session '{name}' already present, attaching to it \
                 ({})",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }

    /// The `ssh … <remote_ghost> __pipe <name>` command whose stdio the client
    /// attaches over — the far end relays it to the remote host's control socket.
    ///
    /// The relay's stderr is discarded: when the session is gone (e.g. right after
    /// a remote reboot) `ghost __pipe` prints "cannot reach session …", ssh forwards
    /// it to our stderr, and the observe/attach retries turn it into a storm. The
    /// local side already reports any attach/observe failure, so the far-side copy is
    /// pure noise. ssh's OWN diagnostics (host-key, auth) aren't lost — they belong
    /// to the warmup/negotiate ssh that opens the master, not this reuse of it.
    pub fn pipe_command(&self, remote_ghost: &str, name: &str) -> Command {
        let mut c = self.command(&[remote_ghost, "__pipe", name]);
        c.stderr(Stdio::null());
        c
    }

    /// The `ssh … <remote_ghost> __watch` command whose stdout streams the host's
    /// session listing (one JSON line per change) — the fleet's push channel for
    /// this host, replacing the periodic `list_sessions` poll.
    pub fn watch_command(&self, remote_ghost: &str) -> Command {
        self.command(&[remote_ghost, "__watch"])
    }
}

/// Make a target safe for a filename (control-socket path): keep it short and
/// free of path separators.
fn sanitize(target: &str) -> String {
    target
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(target: &str) -> RemoteSsh {
        RemoteSsh {
            spec: ConnectionSpec::parse_target(target).unwrap(),
            control_path: PathBuf::from("/run/ghost/ssh-box.ctl"),
        }
    }

    #[test]
    fn argv_shares_one_control_connection_then_runs_the_remote_command() {
        let r = remote("kov@box");
        assert_eq!(
            r.argv(&["ghost", "__pipe", "work"]),
            vec![
                "ssh",
                "-o",
                "ControlMaster=auto",
                "-o",
                "ControlPath=\"/run/ghost/ssh-box.ctl\"",
                "-o",
                "ControlPersist=60",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "kov@box",
                // Remote words are single-quoted (ssh reparses them remotely).
                "'ghost'",
                "'__pipe'",
                "'work'",
            ]
        );
    }

    #[test]
    fn list_sessions_runs_ghost_ls_json_over_the_shared_connection() {
        let r = remote("kov@box");
        assert_eq!(
            r.argv(&["ghost", "ls", "--json"]),
            vec![
                "ssh",
                "-o",
                "ControlMaster=auto",
                "-o",
                "ControlPath=\"/run/ghost/ssh-box.ctl\"",
                "-o",
                "ControlPersist=60",
                "-o",
                "StrictHostKeyChecking=accept-new",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "kov@box",
                "'ghost'",
                "'ls'",
                "'--json'",
            ]
        );
    }

    #[test]
    fn admin_commands_quote_the_remote_words() {
        // Kill/rename go over the shared connection with each remote word single-
        // quoted, like every other invocation (ssh reparses them remotely).
        let r = remote("kov@box");
        assert_eq!(
            &r.argv(&["ghost", "kill", "work"])[13..],
            &["kov@box", "'ghost'", "'kill'", "'work'"]
        );
        assert_eq!(
            &r.argv(&["ghost", "rename", "old", "new"])[13..],
            &["kov@box", "'ghost'", "'rename'", "'old'", "'new'"]
        );
    }

    #[test]
    fn watch_command_runs_ghost_watch_over_the_shared_connection() {
        let r = remote("kov@box");
        assert_eq!(
            &r.argv(&["ghost", "__watch"])[13..],
            &["kov@box", "'ghost'", "'__watch'"]
        );
    }

    #[test]
    fn control_path_is_double_quoted_so_a_space_in_the_path_parses() {
        // macOS has no XDG_RUNTIME_DIR, so the runtime base is the data-local dir
        // `~/Library/Application Support/ghost` — whose space makes ssh's `-o`
        // config parser see two words and bail with "keyword controlpath extra
        // arguments at end of line". The value must be double-quoted so ssh reads
        // the whole path. Regression test for that macOS failure.
        let r = RemoteSsh {
            spec: ConnectionSpec::parse_target("kov@box").unwrap(),
            control_path: PathBuf::from("/Users/kov/Library/Application Support/ghost/ssh-box.ctl"),
        };
        let cp = r
            .argv(&["true"])
            .into_iter()
            .find(|a| a.starts_with("ControlPath="))
            .expect("ControlPath option present");
        assert_eq!(
            cp,
            "ControlPath=\"/Users/kov/Library/Application Support/ghost/ssh-box.ctl\""
        );
    }

    #[test]
    fn control_path_sanitizes_the_target() {
        let r = RemoteSsh::new(ConnectionSpec::parse_target("kov@build-box").unwrap()).unwrap();
        let name = r.control_path.file_name().unwrap().to_string_lossy();
        assert_eq!(name, "ssh-kov_build_box.ctl");
    }

    #[test]
    fn parse_platform_maps_uname_and_rejects_the_unknown() {
        let linux = Platform {
            os: "linux",
            arch: "x86_64",
        };
        let mac = Platform {
            os: "macos",
            arch: "aarch64",
        };
        assert_eq!(parse_platform("Linux x86_64"), Some(linux));
        assert_eq!(parse_platform("Linux amd64"), Some(linux)); // the amd64 alias
        assert_eq!(parse_platform("Darwin arm64"), Some(mac)); // the arm64 alias
        assert_eq!(parse_platform("Darwin aarch64"), Some(mac));
        // Trailing newline / extra columns are tolerated (first two tokens count).
        assert_eq!(parse_platform("Linux x86_64\n"), Some(linux));
        // Unknown OS or arch, and malformed input, don't map.
        assert_eq!(parse_platform("Plan9 sparc"), None);
        assert_eq!(parse_platform("Linux riscv64"), None);
        assert_eq!(parse_platform("Linux"), None);
        assert_eq!(parse_platform(""), None);
    }

    #[test]
    fn resolve_for_uses_the_exe_locally_and_a_prebuilt_cross_arch() {
        let dir = tempfile::tempdir().unwrap();
        let local = Platform {
            os: "linux",
            arch: "x86_64",
        };
        let foreign = Platform {
            os: "macos",
            arch: "aarch64",
        };
        let exe = PathBuf::from("/proc/self/exe");
        let search = [dir.path().to_path_buf()];

        // Same platform ⇒ our own executable, no prebuilt needed.
        assert_eq!(
            resolve_for(local, local, Some(&exe), &search),
            Some(exe.clone())
        );

        // Cross platform with no matching prebuilt ⇒ nothing to stage.
        assert_eq!(resolve_for(foreign, local, Some(&exe), &search), None);

        // Drop in a prebuilt named for the foreign platform ⇒ it's picked.
        let prebuilt = dir.path().join("ghost-macos-aarch64");
        std::fs::write(&prebuilt, b"binary").unwrap();
        assert_eq!(
            resolve_for(foreign, local, Some(&exe), &search),
            Some(prebuilt)
        );
    }

    #[test]
    fn a_probe_reply_is_accepted_only_at_or_above_our_protocol_level() {
        // Our own probe line passes, and a remote at or above our level passes — a
        // newer host decodes every frame we might send.
        assert!(probe_reply_speaks_our_protocol(&probe_line()));
        assert!(probe_reply_speaks_our_protocol("ghost-transport proto=999"));

        // A remote BELOW our level is rejected even though it answered, so negotiate
        // stages a version-matched copy instead of preferring a stale PATH ghost that
        // would mis-decode our newer frames.
        let below = crate::protocol::PROTO_LEVEL - 1;
        assert!(
            !probe_reply_speaks_our_protocol(&format!("ghost-transport proto={below}")),
            "an under-level remote must be rejected"
        );
        // A ghost too old to print `proto=` at all is rejected.
        assert!(!probe_reply_speaks_our_protocol("ghost-transport"));
        // Not a ghost — a shell that echoed the command line carries no marker.
        assert!(!probe_reply_speaks_our_protocol("ghost __probe"));
        // A non-numeric proto is not a pass.
        assert!(!probe_reply_speaks_our_protocol(
            "ghost-transport proto=abc"
        ));
    }

    #[test]
    fn staged_path_is_absolute_and_version_and_build_stamped_under_the_remote_home() {
        let p = staged_path("/home/claude", "deadbeefcafef00d");
        assert_eq!(
            p,
            format!(
                "/home/claude/.cache/ghost/bin/ghost-{}-deadbeefcafef00d",
                env!("CARGO_PKG_VERSION")
            )
        );
        // A different build stamp routes to a different path, so a changed binary
        // never reuses a stale staged copy.
        assert_ne!(p, staged_path("/home/claude", "0000000000000000"));
    }

    #[test]
    fn prune_script_keeps_the_newest_and_removes_the_rest() {
        let s = prune_script("/home/claude/.cache/ghost/bin", 3);
        // Enters the dir, tolerates it being absent.
        assert!(s.contains("cd /home/claude/.cache/ghost/bin"));
        // Newest-first, scoped to our staged binaries.
        assert!(s.contains("ls -t ghost-*"));
        // Keeps 3 → deletes from the 4th oldest-ward line on.
        assert!(
            s.contains("tail -n +4"),
            "keep=3 must skip the newest 3: {s}"
        );
        assert!(s.contains("rm -f"));
    }

    #[test]
    fn control_opts_ask_the_master_to_self_reap_a_dead_peer() {
        // ServerAlive* make a persisting master notice a dead peer (the laptop slept /
        // the network changed) and exit, instead of lingering wedged for the next reuse
        // to hang on. Regression guard for the wedged-mux connect hang.
        let opts = remote("kov@box").argv(&["true"]);
        assert!(
            opts.windows(2)
                .any(|w| w == ["-o", "ServerAliveInterval=15"]),
            "opts carry ServerAliveInterval: {opts:?}"
        );
        assert!(
            opts.windows(2)
                .any(|w| w == ["-o", "ServerAliveCountMax=3"]),
            "opts carry ServerAliveCountMax: {opts:?}"
        );
    }

    #[test]
    fn wait_bounded_kills_a_child_that_overruns_its_deadline() {
        use std::time::{Duration, Instant};
        // A child outrunning the timeout is killed and reported None, promptly — not
        // after its own runtime. This is what stops a wedged ssh hanging the connect.
        let mut slow = Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let t = Instant::now();
        assert!(wait_bounded(&mut slow, Duration::from_millis(150)).is_none());
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "returned at the deadline, not after the child's 30s"
        );
        // A fast child is waited normally and its status returned.
        let mut quick = Command::new("true").spawn().expect("spawn true");
        assert!(wait_bounded(&mut quick, Duration::from_secs(5)).is_some_and(|s| s.success()));
    }

    #[test]
    fn a_stale_control_socket_is_reaped_before_reuse() {
        // A leftover socket whose master no longer answers must be removed so the next
        // connect opens a FRESH master rather than multiplexing onto a dead one. A plain
        // file stands in for a stale socket: `ssh -O check` against a non-socket fails
        // locally (no network), so `master_alive` is false and reap removes it.
        let dir = tempfile::tempdir().unwrap();
        let ctl = dir.path().join("ssh-box.ctl");
        std::fs::write(&ctl, b"stale").unwrap();
        let r = RemoteSsh {
            spec: ConnectionSpec::parse_target("box").unwrap(),
            control_path: ctl.clone(),
        };
        assert!(!r.master_alive(), "a non-socket path is not a live master");
        r.reap_wedged_master();
        assert!(!ctl.exists(), "the stale socket was removed");
    }

    #[test]
    fn reap_is_a_no_op_when_there_is_no_socket() {
        // A fresh connect (no socket yet) must not spawn an ssh, error, or block.
        let r = RemoteSsh {
            spec: ConnectionSpec::parse_target("box").unwrap(),
            control_path: PathBuf::from("/nonexistent/ghost/ssh-box.ctl"),
        };
        r.reap_wedged_master();
    }
}
