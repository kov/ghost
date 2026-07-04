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
use std::path::PathBuf;
use std::process::Command;

/// Env override for the remote ghost binary. Until staging lands, point this at
/// a path the remote user can execute (e.g. a shared build) to use the transport
/// without a `ghost` on the remote `PATH`.
const REMOTE_GHOST_ENV: &str = "GHOST_REMOTE_GHOST";

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

    /// The remote ghost binary this initiator targets (`GHOST_REMOTE_GHOST`, else
    /// `ghost` on the remote `PATH`).
    pub fn remote_ghost() -> String {
        std::env::var(REMOTE_GHOST_ENV).unwrap_or_else(|_| "ghost".to_string())
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

    /// Decide whether the host can run a ghost session host: run
    /// `<remote_ghost> __probe` and confirm the reply carries the [`PROBE_MARKER`]
    /// (a clean exit is not enough — a shell that echoed the command line would
    /// pass a looser check). `Some(remote_ghost)` ⇒ use the transport; `None` ⇒
    /// fall back to the local ssh child. This is also where the shared connection
    /// first authenticates.
    ///
    /// Version *compatibility* (the remote speaking our `PROTO_LEVEL`) is a
    /// staging-phase concern; for now any ghost that answers the probe is accepted.
    pub fn negotiate(&self) -> Option<String> {
        let ghost = Self::remote_ghost();
        let answers_probe = self
            .command(&[&ghost, "__probe"])
            .output()
            .map(|o| {
                o.status.success() && String::from_utf8_lossy(&o.stdout).contains(PROBE_MARKER)
            })
            .unwrap_or(false);
        answers_probe.then_some(ghost)
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
}
