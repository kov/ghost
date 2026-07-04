//! How a session reaches a remote host, as durable structured data.
//!
//! A [`ConnectionSpec`] describes a remote connection — host, user, port, and
//! so on — *independently of how the reach is realized*. Today the only
//! realization is a local `ssh` child (the session's child process is
//! `ssh user@host`, iTerm-profile style); later the very same spec will drive
//! ghost's session host running remotely over the SSH transport
//! (`crate::transport`), and later still mosh. Because the spec is stored and
//! copied — never a memorized command line — a new session created in an ssh
//! group / from an ssh session can inherit the connection, a dead ssh session
//! can *reconnect* on relaunch instead of dropping to a local shell, and mosh
//! can derive a different argv from the same data.
//!
//! [`ConnectionSpec::argv`] is the single place a spec becomes a command line;
//! nothing else may format ssh/mosh arguments.

use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::path::PathBuf;

/// Which launcher realizes a connection. Distinct from `crate::transport`'s
/// `Transport` (how the client reaches a *host process*) — this is which binary
/// the local child runs. Defaults to [`ConnectionKind::Ssh`] so every spec
/// written before mosh exists deserializes as ssh.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConnectionKind {
    #[default]
    Ssh,
    /// The mosh realization — declared from day one so the serialized form is
    /// settled; wired to a `ghost mosh` entry point in a later phase.
    Mosh,
}

/// A remote connection, launcher-agnostic and durable.
///
/// The fields carry `#[serde(default)]` (so pre-existing JSON descriptors/meta
/// parse) but deliberately **not** `skip_serializing_if`: a spec rides inside
/// `SpawnOpts` through postcard — a non-self-describing format — where a skipped
/// field desyncs the byte stream and silently drops the whole spawn.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionSpec {
    /// Hostname, IP, or an `ssh_config` alias (passed through verbatim).
    pub host: String,
    /// Login user; `None` uses the launcher's own default (local user /
    /// `ssh_config`).
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    /// Identity file (`ssh -i`).
    #[serde(default)]
    pub identity: Option<PathBuf>,
    /// Jump host (`ssh -J`), kept as the user typed it.
    #[serde(default)]
    pub jump: Option<String>,
    /// Extra ssh-flavored arguments passed through verbatim (e.g.
    /// `-o ForwardAgent=yes`). The escape hatch.
    #[serde(default)]
    pub extra: Vec<String>,
    /// Which launcher realizes the connection.
    #[serde(default)]
    pub kind: ConnectionKind,
}

impl ConnectionSpec {
    /// Parse a `ghost ssh` positional `[user@]host` into a spec (ssh kind, no
    /// options). `None` for an empty host — the only hard error; everything
    /// else is passed through to the launcher, which validates for real.
    pub fn parse_target(s: &str) -> Option<ConnectionSpec> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        let (user, host) = match s.split_once('@') {
            Some((u, h)) => (Some(u), h),
            None => (None, s),
        };
        if host.is_empty() {
            return None;
        }
        Some(ConnectionSpec {
            host: host.to_string(),
            user: user.filter(|u| !u.is_empty()).map(str::to_string),
            ..Default::default()
        })
    }

    /// `user@host` (or bare `host`): the connection's display label and the
    /// destination argument handed to ssh/mosh.
    pub fn target(&self) -> String {
        match &self.user {
            Some(u) => format!("{u}@{}", self.host),
            None => self.host.clone(),
        }
    }

    /// The child argv realizing this connection — the single place a spec
    /// becomes a command line. Never empty (always at least the launcher and
    /// the destination).
    pub fn argv(&self) -> Vec<String> {
        match self.kind {
            ConnectionKind::Ssh => self.ssh_argv(),
            ConnectionKind::Mosh => self.mosh_argv(),
        }
    }

    /// `ssh [-p PORT] [-i IDENTITY] [-J JUMP] [extra…] <target>` — options
    /// before the destination, as ssh requires.
    fn ssh_argv(&self) -> Vec<String> {
        let mut v = vec!["ssh".to_string()];
        if let Some(p) = self.port {
            v.push("-p".into());
            v.push(p.to_string());
        }
        if let Some(id) = &self.identity {
            v.push("-i".into());
            v.push(id.display().to_string());
        }
        if let Some(j) = &self.jump {
            v.push("-J".into());
            v.push(j.clone());
        }
        v.extend(self.extra.iter().cloned());
        v.push(self.target());
        v
    }

    /// `mosh [--ssh="ssh …"] <target>` — mosh owns the UDP side, so the ssh
    /// port/identity/jump and any extra ssh args fold into its `--ssh` override
    /// rather than mosh's own flags. Phase 6 exercises this fully; kept minimal
    /// and space-joined for now (args with spaces are a known limitation).
    fn mosh_argv(&self) -> Vec<String> {
        let mut ssh = String::from("ssh");
        if let Some(p) = self.port {
            let _ = write!(ssh, " -p {p}");
        }
        if let Some(id) = &self.identity {
            let _ = write!(ssh, " -i {}", id.display());
        }
        if let Some(j) = &self.jump {
            let _ = write!(ssh, " -J {j}");
        }
        for e in &self.extra {
            ssh.push(' ');
            ssh.push_str(e);
        }
        let mut v = vec!["mosh".to_string()];
        if ssh != "ssh" {
            v.push(format!("--ssh={ssh}"));
        }
        v.push(self.target());
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_argv_bare_host() {
        let spec = ConnectionSpec::parse_target("build-box").unwrap();
        assert_eq!(spec.argv(), vec!["ssh", "build-box"]);
        assert_eq!(spec.target(), "build-box");
    }

    #[test]
    fn ssh_argv_user_at_host() {
        let spec = ConnectionSpec::parse_target("kov@build-box").unwrap();
        assert_eq!(spec.user.as_deref(), Some("kov"));
        assert_eq!(spec.argv(), vec!["ssh", "kov@build-box"]);
        assert_eq!(spec.target(), "kov@build-box");
    }

    #[test]
    fn ssh_argv_with_port_identity_jump_and_extra() {
        let spec = ConnectionSpec {
            host: "box".into(),
            user: Some("kov".into()),
            port: Some(2222),
            identity: Some("/home/kov/.ssh/id".into()),
            jump: Some("bastion".into()),
            extra: vec!["-o".into(), "ForwardAgent=yes".into()],
            kind: ConnectionKind::Ssh,
        };
        assert_eq!(
            spec.argv(),
            vec![
                "ssh",
                "-p",
                "2222",
                "-i",
                "/home/kov/.ssh/id",
                "-J",
                "bastion",
                "-o",
                "ForwardAgent=yes",
                "kov@box",
            ]
        );
    }

    #[test]
    fn parse_target_rejects_empty_and_dangling_at() {
        assert!(ConnectionSpec::parse_target("").is_none());
        assert!(ConnectionSpec::parse_target("   ").is_none());
        assert!(ConnectionSpec::parse_target("kov@").is_none());
        // A leading '@' is a missing user, not a hard error — host stands.
        let spec = ConnectionSpec::parse_target("@box").unwrap();
        assert_eq!(spec.host, "box");
        assert_eq!(spec.user, None);
    }

    #[test]
    fn serde_json_round_trips() {
        let spec = ConnectionSpec {
            host: "box".into(),
            user: Some("kov".into()),
            port: Some(2222),
            ..Default::default()
        };
        let json = serde_json::to_string(&spec).unwrap();
        let back: ConnectionSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back, spec);
    }

    #[test]
    fn a_spec_without_a_kind_deserializes_as_ssh() {
        // Every spec serialized before mosh existed must read back as ssh.
        let json = r#"{"host":"box"}"#;
        let spec: ConnectionSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.kind, ConnectionKind::Ssh);
        assert_eq!(spec.argv(), vec!["ssh", "box"]);
    }

    #[test]
    fn a_spec_round_trips_through_postcard() {
        // A spec rides inside SpawnOpts through postcard (non-self-describing),
        // so partially-set specs must survive it — a skipped field would desync
        // the stream and drop the spawn (regression guard).
        let spec = ConnectionSpec {
            host: "box".into(),
            user: Some("kov".into()),
            port: Some(2222),
            ..Default::default()
        };
        let bytes = postcard::to_allocvec(&spec).unwrap();
        let back: ConnectionSpec = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back, spec);
    }

    #[test]
    fn mosh_folds_ssh_options_into_the_ssh_override() {
        // Phase-6 territory, but the kind must be honest today: port/identity go
        // through --ssh (mosh's own -p is the UDP port), destination stays bare.
        let spec = ConnectionSpec {
            host: "box".into(),
            user: Some("kov".into()),
            port: Some(2222),
            kind: ConnectionKind::Mosh,
            ..Default::default()
        };
        assert_eq!(spec.argv(), vec!["mosh", "--ssh=ssh -p 2222", "kov@box"]);

        let plain = ConnectionSpec {
            host: "box".into(),
            kind: ConnectionKind::Mosh,
            ..Default::default()
        };
        assert_eq!(plain.argv(), vec!["mosh", "box"]);
    }
}
