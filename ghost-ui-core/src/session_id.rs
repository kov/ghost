//! A session's identity: the machine hosting it, and its name there.
//!
//! Focus, input routing and every per-session map key on this, never a list
//! index — so reordering tiles can't silently retarget input.

use crate::group::REMOTE_ID_SEP;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// Which machine hosts a session.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Host {
    /// This machine.
    Local,
    /// A host reached over ssh, by its connection target (`user@host`).
    Remote(String),
}

/// A session: the machine hosting it and the name that machine gives it.
///
/// Converting from a string (`"a".into()`) names a *local* session; a remote
/// one is only ever made on purpose, with [`SessionId::remote`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId {
    host: Host,
    name: String,
}

impl SessionId {
    /// The session `name` on this machine.
    pub fn local(name: impl Into<String>) -> Self {
        Self {
            host: Host::Local,
            name: name.into(),
        }
    }

    /// The session `name` on the ssh host `target`.
    pub fn remote(target: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            host: Host::Remote(target.into()),
            name: name.into(),
        }
    }

    /// The machine hosting the session.
    pub fn host(&self) -> &Host {
        &self.host
    }

    /// The session's name on its host — what that host's own commands call it.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The ssh target of a remote session; `None` for a local one.
    pub fn target(&self) -> Option<&str> {
        match &self.host {
            Host::Local => None,
            Host::Remote(target) => Some(target),
        }
    }

    /// The name of a session on *this* machine; `None` for a remote one. What
    /// local-only machinery (sockets, descriptors, recordings) is keyed by, so
    /// a remote session's bare name can never reach a same-named local one.
    pub fn local_name(&self) -> Option<&str> {
        match &self.host {
            Host::Local => Some(&self.name),
            Host::Remote(_) => None,
        }
    }

    /// Whether the session lives on another machine.
    pub fn is_remote(&self) -> bool {
        matches!(self.host, Host::Remote(_))
    }

    /// The id as one string: the name for a local session, `<target>␟<name>`
    /// for a remote one. The on-disk form (`groups.toml`, `windows.toml`), and
    /// a key for anything that can only hold a string.
    pub fn to_composite(&self) -> String {
        match &self.host {
            Host::Local => self.name.clone(),
            Host::Remote(target) => format!("{target}{REMOTE_ID_SEP}{}", self.name),
        }
    }

    /// Read back [`to_composite`](Self::to_composite)'s form.
    pub fn parse_composite(s: &str) -> Self {
        match s.split_once(REMOTE_ID_SEP) {
            Some((target, name)) => Self::remote(target, name),
            None => Self::local(s),
        }
    }
}

impl From<&str> for SessionId {
    fn from(name: &str) -> Self {
        Self::local(name)
    }
}

impl From<String> for SessionId {
    fn from(name: String) -> Self {
        Self::local(name)
    }
}

impl From<&String> for SessionId {
    fn from(name: &String) -> Self {
        Self::local(name.as_str())
    }
}

impl From<&SessionId> for SessionId {
    fn from(id: &SessionId) -> Self {
        id.clone()
    }
}

impl PartialEq<&SessionId> for SessionId {
    fn eq(&self, other: &&SessionId) -> bool {
        self == *other
    }
}

impl PartialEq<SessionId> for &SessionId {
    fn eq(&self, other: &SessionId) -> bool {
        *self == other
    }
}

// Tests compare ids with the strings they were written as: a bare name is a
// local session, a `<target>␟<name>` composite a remote one. Test-only, so
// production code cannot compare an id with a string.
#[cfg(test)]
impl PartialEq<str> for SessionId {
    fn eq(&self, other: &str) -> bool {
        *self == SessionId::parse_composite(other)
    }
}

#[cfg(test)]
impl PartialEq<&str> for SessionId {
    fn eq(&self, other: &&str) -> bool {
        *self == SessionId::parse_composite(other)
    }
}

#[cfg(test)]
impl PartialEq<String> for SessionId {
    fn eq(&self, other: &String) -> bool {
        *self == SessionId::parse_composite(other)
    }
}

/// For people: the name, prefixed with its host when remote (`kov@box:work`).
impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.host {
            Host::Local => f.write_str(&self.name),
            Host::Remote(target) => write!(f, "{target}:{}", self.name),
        }
    }
}

impl Serialize for SessionId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_composite())
    }
}

impl<'de> Deserialize<'de> for SessionId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Ok(Self::parse_composite(&s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_composite_form_round_trips_both_kinds() {
        for id in [
            SessionId::local("alpha"),
            SessionId::remote("kov@box", "work"),
        ] {
            assert_eq!(SessionId::parse_composite(&id.to_composite()), id);
        }
        assert_eq!(SessionId::local("alpha").to_composite(), "alpha");
    }

    #[test]
    fn a_string_names_a_local_session() {
        let id: SessionId = "kov@box".into();
        assert_eq!(id, SessionId::local("kov@box"));
        assert!(!id.is_remote());
    }

    #[test]
    fn it_reads_as_host_and_name() {
        assert_eq!(SessionId::local("a").to_string(), "a");
        assert_eq!(
            SessionId::remote("kov@box", "work").to_string(),
            "kov@box:work"
        );
    }
}
