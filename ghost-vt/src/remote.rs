//! The SSH-as-transport *initiator*: from the local machine, reach a host that
//! may run ghost, decide whether it can host one, spawn a host there, and hand
//! back the `ssh … __pipe` command the client attaches over.
//!
//! All ssh invocations share one multiplexed connection (`ControlMaster=auto` +
//! a per-target `ControlPath` + `ControlPersist`), so a single password prompt
//! on the probe covers the later spawn and attach — without it a password-auth
//! host would prompt three times. ssh reads the password from `/dev/tty`, so the
//! prompt reaches the user even when a probe's stdout is captured.

use crate::connection::ConnectionSpec;
use std::io;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Env override for the remote ghost binary. Points the initiator at a specific
/// remote path (e.g. a shared build); when set, staging is skipped and the path
/// is used as-is.
const REMOTE_GHOST_ENV: &str = "GHOST_REMOTE_GHOST";

/// The remote directory a staged ghost lands in, under the given remote `$HOME`.
fn staged_dir(home: &str) -> String {
    format!("{home}/.cache/ghost/bin")
}

/// Where a staged ghost lands on the remote (absolute, so it needs no shell
/// tilde/`$HOME` expansion at exec time), version-stamped so a mismatched build
/// never gets reused and each version is provisioned once.
fn staged_path(home: &str) -> String {
    format!("{}/ghost-{}", staged_dir(home), env!("CARGO_PKG_VERSION"))
}

/// Whether a remote `uname -s -m` names the same OS+arch as this build — the gate
/// for copying our own binary over (cross-arch staging needs a build matrix,
/// which is future work).
fn remote_matches_local(uname_sm: &str) -> bool {
    let mut it = uname_sm.split_whitespace();
    let (Some(sys), Some(machine)) = (it.next(), it.next()) else {
        return false;
    };
    let os = match sys {
        "Linux" => "linux",
        "Darwin" => "macos",
        _ => return false,
    };
    let arch = match machine {
        "x86_64" | "amd64" => "x86_64",
        "aarch64" | "arm64" => "aarch64",
        _ => return false,
    };
    os == std::env::consts::OS && arch == std::env::consts::ARCH
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

    /// ssh options that share one authenticated connection across invocations.
    fn control_opts(&self) -> Vec<String> {
        vec![
            "-o".into(),
            "ControlMaster=auto".into(),
            "-o".into(),
            format!("ControlPath={}", self.control_path.display()),
            "-o".into(),
            "ControlPersist=60".into(),
        ]
    }

    /// The full ssh argv running `remote` on the host over the shared connection.
    fn argv(&self, remote: &[&str]) -> Vec<String> {
        self.spec.ssh_command(&self.control_opts(), remote)
    }

    /// An [`std::process::Command`] running `remote` on the host.
    fn command(&self, remote: &[&str]) -> Command {
        let argv = self.argv(remote);
        let mut c = Command::new(&argv[0]);
        c.args(&argv[1..]);
        c
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
    pub fn negotiate(&self) -> Option<String> {
        if let Ok(path) = std::env::var(REMOTE_GHOST_ENV) {
            return self.probe(&path).then_some(path);
        }
        if self.probe("ghost") {
            return Some("ghost".to_string());
        }
        // Staging needs an absolute path, so learn the remote home first.
        let home = self.remote_home()?;
        let staged = staged_path(&home);
        if self.probe(&staged) {
            return Some(staged);
        }
        match self.stage(&home, &staged) {
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

    /// Copy this binary to `staged` on the remote (OS+arch permitting), so a host
    /// that lacks ghost can still run one. Cross-arch staging is rejected — that
    /// needs prebuilt binaries per platform (future). Terminfo is not shipped: the
    /// remote host self-provisions it on first run (needs remote `tic`).
    fn stage(&self, home: &str, staged: &str) -> io::Result<()> {
        let uname = self.command(&["uname", "-s", "-m"]).output()?;
        if !uname.status.success() {
            return Err(io::Error::other("remote `uname` failed"));
        }
        let uname = String::from_utf8_lossy(&uname.stdout);
        if !remote_matches_local(&uname) {
            return Err(io::Error::other(format!(
                "remote is '{}' but this ghost is {}/{} — cross-platform staging is not supported yet",
                uname.trim(),
                std::env::consts::OS,
                std::env::consts::ARCH,
            )));
        }

        let exe = std::env::current_exe()?;
        let bytes = std::fs::read(&exe)?;
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
        child.stdin.take().expect("piped stdin").write_all(&bytes)?;
        if !child.wait()?.success() {
            return Err(io::Error::other("copying the binary to the remote failed"));
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
                "kov@box",
                "ghost",
                "__pipe",
                "work",
            ]
        );
    }

    #[test]
    fn control_path_sanitizes_the_target() {
        let r = RemoteSsh::new(ConnectionSpec::parse_target("kov@build-box").unwrap()).unwrap();
        let name = r.control_path.file_name().unwrap().to_string_lossy();
        assert_eq!(name, "ssh-kov_build_box.ctl");
    }

    #[test]
    fn remote_matches_local_maps_uname_to_this_platform() {
        // The pair that names *this* build's platform matches; every other does
        // not (cross-arch staging is gated off).
        let os = std::env::consts::OS;
        let arch = std::env::consts::ARCH;
        let sys = match os {
            "linux" => "Linux",
            "macos" => "Darwin",
            other => other,
        };
        // uname's machine string equals Rust's ARCH for the platforms we accept.
        let machine = arch;
        assert!(remote_matches_local(&format!("{sys} {machine}")));
        assert!(remote_matches_local(&format!("{sys}\n{machine}\n")));

        // A different OS or arch, and malformed input, never match. Derive the
        // "other" values from this platform so the test holds on any host.
        let other_sys = if os == "linux" { "Darwin" } else { "Linux" };
        let other_machine = if arch == "x86_64" {
            "aarch64"
        } else {
            "x86_64"
        };
        assert!(!remote_matches_local(&format!("{other_sys} {machine}")));
        assert!(!remote_matches_local(&format!("{sys} {other_machine}")));
        assert!(!remote_matches_local("Plan9 sparc"));
        assert!(!remote_matches_local(""));
    }

    #[test]
    fn staged_path_is_absolute_and_version_stamped_under_the_remote_home() {
        let p = staged_path("/home/claude");
        assert_eq!(
            p,
            format!(
                "/home/claude/.cache/ghost/bin/ghost-{}",
                env!("CARGO_PKG_VERSION")
            )
        );
    }
}
