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
use std::process::{Command, Stdio};

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

/// One multiplexed ssh connection to a host, for the transport initiator.
pub struct RemoteSsh {
    spec: ConnectionSpec,
    control_path: PathBuf,
}

impl RemoteSsh {
    /// A connection to `spec`'s host, with a per-target control socket under the
    /// runtime dir (created if missing).
    pub fn new(spec: ConnectionSpec) -> io::Result<Self> {
        let dir = crate::paths::runtime_dir();
        std::fs::create_dir_all(&dir)?;
        let control_path = dir.join(format!("ssh-{}.ctl", sanitize(&spec.target())));
        Ok(RemoteSsh { spec, control_path })
    }

    /// The connection's target (`[user@]host`), e.g. for a progress message.
    pub fn target(&self) -> String {
        self.spec.target()
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
            format!("ControlPath={}", self.control_path.display()),
            "-o".into(),
            "ControlPersist=60".into(),
            "-o".into(),
            "StrictHostKeyChecking=accept-new".into(),
        ]
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
    pub fn negotiate(&self) -> Option<String> {
        self.negotiate_with_progress(&mut |_| {})
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
    ) -> Option<String> {
        if let Ok(path) = std::env::var(REMOTE_GHOST_ENV) {
            return self.probe(&path).then_some(path);
        }
        if self.probe("ghost") {
            return Some("ghost".to_string());
        }
        // Staging needs the remote home (for an absolute path) and its platform
        // (to pick the binary — our own exe for a matching host, else a prebuilt).
        let home = self.remote_home()?;
        let platform = self.remote_platform()?;
        let binary = resolve_ghost_binary(platform)?;
        let staged = staged_path(&home, &build_stamp(&binary));
        if self.probe(&staged) {
            return Some(staged);
        }
        match self.stage(&binary, &home, &staged, on_progress) {
            Ok(()) if self.probe(&staged) => Some(staged),
            Ok(()) => None,
            Err(e) => {
                eprintln!("ghost: cannot use the remote host transport ({e}); using ssh directly");
                None
            }
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
    /// it. `None` if the command fails or names a platform we don't map.
    fn remote_platform(&self) -> Option<Platform> {
        let out = self.command(&["uname", "-s", "-m"]).output().ok()?;
        out.status.success().then_some(())?;
        parse_platform(&String::from_utf8_lossy(&out.stdout))
    }

    /// Run `<candidate> __probe` and confirm the reply carries the
    /// [`PROBE_MARKER`] — a clean exit is not enough (a shell that echoed the
    /// command line would pass a looser check).
    fn probe(&self, candidate: &str) -> bool {
        self.command(&[candidate, "__probe"])
            .output()
            .map(|o| {
                o.status.success() && String::from_utf8_lossy(&o.stdout).contains(PROBE_MARKER)
            })
            .unwrap_or(false)
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
    pub fn pipe_command(&self, remote_ghost: &str, name: &str) -> Command {
        self.command(&[remote_ghost, "__pipe", name])
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
                "ControlPath=/run/ghost/ssh-box.ctl",
                "-o",
                "ControlPersist=60",
                "-o",
                "StrictHostKeyChecking=accept-new",
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
                "ControlPath=/run/ghost/ssh-box.ctl",
                "-o",
                "ControlPersist=60",
                "-o",
                "StrictHostKeyChecking=accept-new",
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
            &r.argv(&["ghost", "kill", "work"])[9..],
            &["kov@box", "'ghost'", "'kill'", "'work'"]
        );
        assert_eq!(
            &r.argv(&["ghost", "rename", "old", "new"])[9..],
            &["kov@box", "'ghost'", "'rename'", "'old'", "'new'"]
        );
    }

    #[test]
    fn watch_command_runs_ghost_watch_over_the_shared_connection() {
        let r = remote("kov@box");
        assert_eq!(
            &r.argv(&["ghost", "__watch"])[9..],
            &["kov@box", "'ghost'", "'__watch'"]
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
}
