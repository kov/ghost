//! Detach/kill trigger parsing for the attach client.
//!
//! The client is a transparent pipe: it forwards input byte-for-byte and
//! intercepts only a prefix sequence. The default prefix is `Ctrl-\` (`0x1c`),
//! chosen because raw mode disables `ISIG` (so it never raises `SIGQUIT`) and
//! the `0x1c`–`0x1e` range is essentially unbound by shells and editors.
//!
//! After the prefix:
//! - `d` → [`Action::Detach`]
//! - `k` → [`Action::Kill`]
//! - `r` → [`Action::Rename`] (the client then prompts for the new name)
//! - the prefix again → one *literal* prefix byte is forwarded
//! - anything else → the prefix is swallowed and the key is forwarded
//!
//! [`Detacher`] is stateful so a prefix split across reads is handled correctly.

/// Default detach/kill prefix: `Ctrl-\`.
pub const DEFAULT_PREFIX: u8 = 0x1c;
/// Key that, after the prefix, detaches from the session.
pub const DETACH_KEY: u8 = b'd';
/// Key that, after the prefix, kills the session.
pub const KILL_KEY: u8 = b'k';
/// Key that, after the prefix, begins renaming the session.
pub const RENAME_KEY: u8 = b'r';

/// What the client should do with a span of input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Forward these bytes to the session host (write to the child's PTY).
    Forward(Vec<u8>),
    /// Detach from the session, leaving it running.
    Detach,
    /// Kill the session and its child.
    Kill,
    /// Begin renaming the session (the client collects the new name).
    Rename,
}

/// Stateful scanner turning a raw input stream into [`Action`]s.
pub struct Detacher {
    prefix: u8,
    /// True when the previous byte was an unconsumed prefix awaiting its command.
    armed: bool,
}

impl Detacher {
    pub fn new(prefix: u8) -> Self {
        Self {
            prefix,
            armed: false,
        }
    }

    pub fn with_default_prefix() -> Self {
        Self::new(DEFAULT_PREFIX)
    }

    /// Process a chunk of input, returning the actions it produces in order.
    /// State persists across calls, so the prefix may straddle chunk boundaries.
    pub fn feed(&mut self, input: &[u8]) -> Vec<Action> {
        let mut actions = Vec::new();
        let mut pending: Vec<u8> = Vec::new();
        for &b in input {
            if self.armed {
                self.armed = false;
                if b == DETACH_KEY {
                    flush(&mut pending, &mut actions);
                    actions.push(Action::Detach);
                } else if b == KILL_KEY {
                    flush(&mut pending, &mut actions);
                    actions.push(Action::Kill);
                } else if b == RENAME_KEY {
                    flush(&mut pending, &mut actions);
                    actions.push(Action::Rename);
                } else if b == self.prefix {
                    pending.push(self.prefix); // doubled prefix -> one literal
                } else {
                    pending.push(b); // unknown command: swallow prefix, forward the key
                }
            } else if b == self.prefix {
                self.armed = true;
            } else {
                pending.push(b);
            }
        }
        flush(&mut pending, &mut actions);
        actions
    }
}

fn flush(pending: &mut Vec<u8>, actions: &mut Vec<Action>) {
    if !pending.is_empty() {
        actions.push(Action::Forward(std::mem::take(pending)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d() -> Detacher {
        Detacher::with_default_prefix()
    }
    const P: u8 = DEFAULT_PREFIX;

    #[test]
    fn plain_bytes_pass_through() {
        assert_eq!(d().feed(b"hello"), vec![Action::Forward(b"hello".to_vec())]);
    }

    #[test]
    fn empty_input() {
        assert_eq!(d().feed(b""), vec![]);
    }

    #[test]
    fn prefix_then_detach() {
        assert_eq!(d().feed(&[P, b'd']), vec![Action::Detach]);
    }

    #[test]
    fn prefix_then_kill() {
        assert_eq!(d().feed(&[P, b'k']), vec![Action::Kill]);
    }

    #[test]
    fn prefix_then_rename() {
        assert_eq!(d().feed(&[P, b'r']), vec![Action::Rename]);
    }

    #[test]
    fn bytes_before_trigger_forwarded_first() {
        assert_eq!(
            d().feed(&[b'a', b'b', P, b'd']),
            vec![Action::Forward(b"ab".to_vec()), Action::Detach]
        );
    }

    #[test]
    fn doubled_prefix_emits_one_literal() {
        assert_eq!(d().feed(&[P, P]), vec![Action::Forward(vec![P])]);
    }

    #[test]
    fn prefix_then_unknown_swallows_prefix_passes_key() {
        assert_eq!(d().feed(&[P, b'x']), vec![Action::Forward(b"x".to_vec())]);
    }

    #[test]
    fn prefix_split_across_feeds() {
        let mut det = d();
        assert_eq!(det.feed(&[P]), vec![]);
        assert_eq!(det.feed(b"d"), vec![Action::Detach]);
    }

    #[test]
    fn doubled_prefix_split_across_feeds() {
        let mut det = d();
        assert_eq!(det.feed(&[P]), vec![]);
        assert_eq!(det.feed(&[P]), vec![Action::Forward(vec![P])]);
    }

    #[test]
    fn action_then_trailing_bytes_in_same_chunk() {
        // The parser reports everything; the client acts on the first terminal
        // action (Detach) and ignores what follows.
        assert_eq!(
            d().feed(&[b'a', P, b'd', b'b']),
            vec![
                Action::Forward(b"a".to_vec()),
                Action::Detach,
                Action::Forward(b"b".to_vec())
            ]
        );
    }
}
