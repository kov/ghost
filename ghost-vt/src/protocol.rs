//! The framed control protocol exchanged over a [`transport`](crate::transport):
//! attach, input, output, resize, detach, and kill messages.
//!
//! Detach and kill are first-class protocol actions; *how* a user triggers them
//! is left entirely to the client (a key sequence for the CLI, a button for a
//! GUI) — the protocol never assumes a particular keybinding.
//!
//! Frames are length-prefixed: a little-endian `u32` body length followed by the
//! [postcard]-serialized body. [`FrameReader`] accumulates bytes from a
//! non-blocking stream and yields whole messages as they arrive, so it slots
//! directly into a `poll()`-driven read loop.

use serde::{Deserialize, Serialize};
use std::io;

/// Messages sent from an attach client to the session host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMsg {
    /// Raw input bytes typed by the user, to be written to the child's PTY.
    Input(Vec<u8>),
    /// The attaching terminal was resized.
    Resize { cols: u16, rows: u16 },
    /// Detach: leave the session and its child running.
    Detach,
    /// Kill: terminate the session and its child.
    Kill,
    /// Rename the session to this name. The host stores it as the session's
    /// display name — a label; the socket, pidfile, and recording keep the
    /// immutable spawn-time name, so attached clients are never disturbed —
    /// then replies with [`ServerMsg::RenameResult`].
    Rename(String),
    /// Ask the host to repaint the screen (re-send the current state). Used by
    /// the client to heal its display after drawing a transient overlay such as
    /// the rename prompt.
    Repaint,
    /// The client's theme colors (default fg/bg, cursor). The host keeps the
    /// most recent report as the session's last-attached colors and answers
    /// the child's color queries with them while detached, instead of ghost's
    /// built-in defaults. Clients that know their scheme (the GUI) send it
    /// right after attaching.
    Theme(crate::query::ThemeColors),
    /// Identify this connection: an opaque self-chosen id (e.g. one GUI
    /// window). A display client sends it right after attaching — like
    /// [`ClientMsg::Theme`] — so the host can say *who* holds the display in
    /// [`AttachInfo`]; frontends compare it with their own id to tell
    /// "attached to me" from "attached elsewhere". Optional: an anonymous
    /// display client reports as `AttachInfo { client: None }`.
    Hello { client: String },
    /// Subscribe to state events for this session. A subscriber is *not* a
    /// display client: it never sends [`ClientMsg::Resize`], so it never
    /// steals the display or resizes the PTY. The host replies with one
    /// [`ServerMsg::Snapshot`], then pushes a [`ServerMsg::Event`] on every
    /// state change until the connection closes. Host death is observed as
    /// EOF on the subscription.
    Subscribe,
    /// Subscribe *and* receive the session's output, read-only: everything
    /// [`ClientMsg::Subscribe`] delivers, plus a
    /// [`SessionEvent::Resized`] carrying the session's real grid, a resync
    /// of the current screen, and live [`ServerMsg::Output`] thereafter.
    /// Like a subscriber, an observer never resizes the PTY, never steals
    /// the display, never spawns a deferred child, and a bell it observes is
    /// not "seen" — it watches the session exactly as the display client
    /// shapes it (live fleet previews).
    Observe,
}

/// Who holds a session's display. Richer than the on-disk `attached` marker
/// (a bare bool): carries the display client's self-reported identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AttachInfo {
    /// The display client's identity as reported by [`ClientMsg::Hello`], or
    /// `None` for a client that never identified itself.
    pub client: Option<String>,
}

/// A session's mutable state, sent once as [`ServerMsg::Snapshot`] when a
/// subscription starts so the subscriber is consistent before any delta.
/// Mirrors the marker/meta-backed fields of [`SessionInfo`](crate::session::SessionInfo);
/// immutable facts (pid, command, creation time) stay with discovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SessionState {
    /// The current display attachment, or `None` when the session is detached.
    pub attached: Option<AttachInfo>,
    /// The session rang the bell while unattached and has not been seen since.
    pub bell: bool,
    /// The current terminal title (OSC 0/2), empty if none has been set.
    pub title: String,
    /// The user-chosen display name, empty if never renamed.
    pub display_name: String,
}

/// A state change pushed to subscribers as [`ServerMsg::Event`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionEvent {
    /// The child rang the terminal bell.
    Bell,
    /// The child set the terminal title (OSC 0/2).
    TitleChanged(String),
    /// A display client attached, or replaced the previous one.
    Attached(AttachInfo),
    /// The display client detached; nothing is driving the session.
    Detached,
    /// The child produced output. Coalesced by the host — a burst of output
    /// is a single event, and a slow subscriber is never flooded; it exists
    /// to drive activity badges, not to carry content.
    Activity,
    /// The session's display name changed ([`ClientMsg::Rename`]).
    Renamed(String),
    /// The session's grid changed (the display client resized the PTY), or —
    /// as the first event of an observation — its current size. An observer
    /// re-grids its mirror to `cols`×`rows`; the resync that follows re-seeds
    /// it. Appended after `PROTO_SUBSCRIBE` shipped: level-3 subscribers skip
    /// the unknown frame without losing the stream.
    Resized { cols: u16, rows: u16 },
}

/// Messages sent from the session host to an attach client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMsg {
    /// Raw output bytes from the child's PTY, to be written to the terminal.
    Output(Vec<u8>),
    /// The child process exited with this status code.
    Exited(i32),
    /// Result of a [`ClientMsg::Rename`]: `ok` true on success, otherwise
    /// `message` explains why it was refused.
    RenameResult { ok: bool, message: String },
    /// One consistent state snapshot, sent as the reply to
    /// [`ClientMsg::Subscribe`] before any [`ServerMsg::Event`].
    Snapshot(SessionState),
    /// A state change, pushed to subscribers.
    Event(SessionEvent),
}

/// The protocol feature level this binary speaks. The host writes it to the
/// session dir's `proto` file at startup so clients can tell what an already
/// running host understands — hosts are long-lived daemons that keep executing
/// the binary that spawned them, and one built before a message existed treats
/// it as a connection error and drops the client. A missing file reads as
/// level 0. Bump this when appending a message clients send unprompted — or
/// when an existing message's *semantics* change in a way clients must gate on
/// — and add a `PROTO_*` constant for it.
pub const PROTO_LEVEL: u32 = 4;

/// Feature level at which the host understands [`ClientMsg::Theme`].
pub const PROTO_THEME: u32 = 1;

/// Feature level at which [`ClientMsg::Rename`] sets a display-name label.
/// Hosts below this level MOVE the session directory instead — file churn that
/// detaches clients and strands the old id in every attached window — so
/// clients refuse to send them a rename rather than trigger it.
pub const PROTO_RENAME_LABEL: u32 = 2;

/// Feature level at which the host serves [`ClientMsg::Subscribe`] and
/// [`ClientMsg::Hello`] and pushes [`ServerMsg::Snapshot`]/[`ServerMsg::Event`].
/// Clients keep polling the marker files of a session whose host predates it.
pub const PROTO_SUBSCRIBE: u32 = 3;

const _: () = assert!(PROTO_SUBSCRIBE > PROTO_RENAME_LABEL);
const _: () = assert!(PROTO_LEVEL >= PROTO_SUBSCRIBE);

/// Feature level at which the host serves [`ClientMsg::Observe`] (read-only
/// output observation) and emits [`SessionEvent::Resized`].
pub const PROTO_OBSERVE: u32 = 4;

const _: () = assert!(PROTO_OBSERVE > PROTO_SUBSCRIBE);
const _: () = assert!(PROTO_LEVEL >= PROTO_OBSERVE);

/// Upper bound on a frame body, guarding against corrupt or hostile length
/// prefixes before we allocate.
const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

/// Largest raw-output payload to put in a single [`ServerMsg::Output`] frame,
/// leaving room under [`MAX_FRAME_LEN`] for the postcard enum tag and length
/// prefix. A resync that re-emits images can exceed the frame cap, so callers
/// split output with [`output_chunks`].
pub const MAX_OUTPUT_CHUNK: usize = MAX_FRAME_LEN - 1024;

/// Split raw output bytes into pieces that each encode to a frame within the size
/// cap. Splitting at any byte boundary is safe: the client concatenates the
/// pieces and feeds them to its parser, which carries state across feeds. Empty
/// input yields no chunks.
pub fn output_chunks(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes.chunks(MAX_OUTPUT_CHUNK)
}

/// Encode a message as a length-prefixed frame ready to write to a stream.
pub fn encode<M: Serialize>(msg: &M) -> Vec<u8> {
    let body =
        postcard::to_allocvec(msg).expect("postcard encoding of protocol messages cannot fail");
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);
    frame
}

/// Accumulates bytes read from a (possibly non-blocking) stream and yields
/// complete messages as whole frames arrive.
#[derive(Debug, Default)]
pub struct FrameReader {
    buf: Vec<u8>,
}

impl FrameReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append freshly-read bytes.
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Pull the next complete message, if one has fully arrived. Returns
    /// `Ok(None)` when more bytes are still needed.
    ///
    /// A message that cannot be decoded — a newer peer sent something this
    /// binary predates — is skipped, not fatal: the length prefix bounds the
    /// frame, so the stream stays in sync and the next message decodes
    /// normally. Only a corrupt length prefix (oversized frame) errors, since
    /// there is no way to resynchronize past it.
    pub fn next_msg<M: serde::de::DeserializeOwned>(&mut self) -> io::Result<Option<M>> {
        loop {
            if self.buf.len() < 4 {
                return Ok(None);
            }
            let len = u32::from_le_bytes(self.buf[..4].try_into().unwrap()) as usize;
            if len > MAX_FRAME_LEN {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "protocol frame exceeds maximum length",
                ));
            }
            if self.buf.len() < 4 + len {
                return Ok(None);
            }
            let msg = postcard::from_bytes(&self.buf[4..4 + len]);
            self.buf.drain(..4 + len);
            if let Ok(msg) = msg {
                return Ok(Some(msg));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_chunks_each_fit_a_frame_and_reassemble() {
        // A payload larger than one frame (as a resync carrying images can be)
        // splits into chunks that each encode within the cap and reassemble.
        let big = vec![7u8; MAX_OUTPUT_CHUNK * 2 + 123];
        let chunks: Vec<&[u8]> = output_chunks(&big).collect();
        assert!(chunks.len() >= 3, "a >2x payload splits into 3+ chunks");

        let mut reassembled = Vec::new();
        for chunk in &chunks {
            let frame = encode(&ServerMsg::Output(chunk.to_vec()));
            assert!(frame.len() <= MAX_FRAME_LEN, "each chunk fits one frame");
            reassembled.extend_from_slice(chunk);
        }
        assert_eq!(reassembled, big, "chunks concatenate back to the input");

        // A small payload is a single chunk.
        assert_eq!(output_chunks(b"hi").count(), 1);
    }

    #[test]
    fn roundtrip_client_msgs() {
        for msg in [
            ClientMsg::Input(b"hello".to_vec()),
            ClientMsg::Resize { cols: 80, rows: 24 },
            ClientMsg::Detach,
            ClientMsg::Kill,
            ClientMsg::Rename("new-name".to_string()),
            ClientMsg::Repaint,
        ] {
            let mut r = FrameReader::new();
            r.push(&encode(&msg));
            let got: ClientMsg = r.next_msg().unwrap().unwrap();
            assert_eq!(got, msg);
            assert!(r.next_msg::<ClientMsg>().unwrap().is_none());
        }
    }

    #[test]
    fn roundtrip_server_msgs() {
        for msg in [
            ServerMsg::Output(b"world".to_vec()),
            ServerMsg::Exited(0),
            ServerMsg::Exited(137),
            ServerMsg::RenameResult {
                ok: true,
                message: "ok".to_string(),
            },
        ] {
            let mut r = FrameReader::new();
            r.push(&encode(&msg));
            assert_eq!(r.next_msg::<ServerMsg>().unwrap().unwrap(), msg);
        }
    }

    #[test]
    fn roundtrip_observe_surface() {
        // The observer verb (PROTO_OBSERVE): subscribe-plus-output.
        let mut r = FrameReader::new();
        r.push(&encode(&ClientMsg::Observe));
        assert_eq!(
            r.next_msg::<ClientMsg>().unwrap().unwrap(),
            ClientMsg::Observe
        );

        let resized = ServerMsg::Event(SessionEvent::Resized {
            cols: 155,
            rows: 42,
        });
        let mut r = FrameReader::new();
        r.push(&encode(&resized));
        assert_eq!(r.next_msg::<ServerMsg>().unwrap().unwrap(), resized);
    }

    #[test]
    fn a_proto3_subscriber_skips_a_resized_event_without_losing_sync() {
        // `Resized` was appended to SessionEvent after PROTO_SUBSCRIBE shipped:
        // a subscriber built at level 3 must skip the unknown frame and keep
        // decoding the events it knows.
        #[derive(Debug, PartialEq, serde::Deserialize)]
        enum OldSessionEvent {
            Bell,
            TitleChanged(String),
            Attached(AttachInfo),
            Detached,
            Activity,
            Renamed(String),
        }
        #[derive(Debug, PartialEq, serde::Deserialize)]
        enum OldServerMsg {
            Output(Vec<u8>),
            Exited(i32),
            RenameResult { ok: bool, message: String },
            Snapshot(SessionState),
            Event(OldSessionEvent),
        }

        let mut bytes = encode(&ServerMsg::Event(SessionEvent::Resized {
            cols: 80,
            rows: 24,
        }));
        bytes.extend_from_slice(&encode(&ServerMsg::Event(SessionEvent::Bell)));
        let mut r = FrameReader::new();
        r.push(&bytes);
        assert_eq!(
            r.next_msg::<OldServerMsg>().unwrap().unwrap(),
            OldServerMsg::Event(OldSessionEvent::Bell),
            "the unknown Resized frame is skipped and the Bell decodes"
        );
    }

    #[test]
    fn roundtrip_subscribe_surface() {
        // The subscription verbs a state observer speaks (PROTO_SUBSCRIBE).
        for msg in [
            ClientMsg::Subscribe,
            ClientMsg::Hello {
                client: "gui:4242:1".to_string(),
            },
        ] {
            let mut r = FrameReader::new();
            r.push(&encode(&msg));
            assert_eq!(r.next_msg::<ClientMsg>().unwrap().unwrap(), msg);
        }

        let snapshot = ServerMsg::Snapshot(SessionState {
            attached: Some(AttachInfo {
                client: Some("gui:4242:1".to_string()),
            }),
            bell: true,
            title: "vim".to_string(),
            display_name: "build box".to_string(),
        });
        let detached_snapshot = ServerMsg::Snapshot(SessionState {
            attached: None,
            bell: false,
            title: String::new(),
            display_name: String::new(),
        });
        let events = [
            SessionEvent::Bell,
            SessionEvent::TitleChanged("make -j8".to_string()),
            SessionEvent::Attached(AttachInfo { client: None }),
            SessionEvent::Detached,
            SessionEvent::Activity,
            SessionEvent::Renamed("otter".to_string()),
        ]
        .map(ServerMsg::Event);
        for msg in [snapshot, detached_snapshot].into_iter().chain(events) {
            let mut r = FrameReader::new();
            r.push(&encode(&msg));
            assert_eq!(r.next_msg::<ServerMsg>().unwrap().unwrap(), msg);
        }
    }

    #[test]
    fn old_client_skips_snapshot_and_event_frames() {
        // A display client built before PROTO_SUBSCRIBE that shares a stream
        // with pushed state (a dual-written migration host, or a bug) must
        // skip the frames it predates and keep decoding output.
        #[derive(Debug, PartialEq, serde::Deserialize)]
        enum OldServerMsg {
            Output(Vec<u8>),
            Exited(i32),
            RenameResult { ok: bool, message: String },
        }

        let mut bytes = encode(&ServerMsg::Event(SessionEvent::Bell));
        bytes.extend_from_slice(&encode(&ServerMsg::Snapshot(SessionState::default())));
        bytes.extend_from_slice(&encode(&ServerMsg::Output(b"still here".to_vec())));
        let mut r = FrameReader::new();
        r.push(&bytes);
        assert_eq!(
            r.next_msg::<OldServerMsg>().unwrap().unwrap(),
            OldServerMsg::Output(b"still here".to_vec()),
            "unknown pushed frames are skipped and output still decodes"
        );
        assert!(r.next_msg::<OldServerMsg>().unwrap().is_none());
    }

    #[test]
    fn partial_frame_yields_none_until_complete() {
        let frame = encode(&ClientMsg::Input(b"abc".to_vec()));
        let mut r = FrameReader::new();
        r.push(&frame[..2]);
        assert!(r.next_msg::<ClientMsg>().unwrap().is_none());
        r.push(&frame[2..]);
        assert_eq!(
            r.next_msg::<ClientMsg>().unwrap().unwrap(),
            ClientMsg::Input(b"abc".to_vec())
        );
    }

    #[test]
    fn two_messages_in_one_buffer() {
        let mut bytes = encode(&ClientMsg::Detach);
        bytes.extend_from_slice(&encode(&ClientMsg::Kill));
        let mut r = FrameReader::new();
        r.push(&bytes);
        assert_eq!(
            r.next_msg::<ClientMsg>().unwrap().unwrap(),
            ClientMsg::Detach
        );
        assert_eq!(r.next_msg::<ClientMsg>().unwrap().unwrap(), ClientMsg::Kill);
        assert!(r.next_msg::<ClientMsg>().unwrap().is_none());
    }

    #[test]
    fn unknown_message_is_skipped_not_fatal() {
        // A decoder built before a message existed sees an unknown enum tag.
        // The frame boundary is still known from the length prefix, so the
        // reader must skip the message rather than poison the connection —
        // hosts are long-lived daemons that routinely outlive the binary that
        // talks to them.
        #[derive(Debug, PartialEq, serde::Deserialize)]
        enum OldClientMsg {
            Input(Vec<u8>),
            Resize { cols: u16, rows: u16 },
            Detach,
            Kill,
            Rename(String),
            Repaint,
        }

        let mut bytes = encode(&ClientMsg::Theme(crate::query::ThemeColors::default()));
        bytes.extend_from_slice(&encode(&ClientMsg::Input(b"still here".to_vec())));
        let mut r = FrameReader::new();
        r.push(&bytes);
        assert_eq!(
            r.next_msg::<OldClientMsg>().unwrap().unwrap(),
            OldClientMsg::Input(b"still here".to_vec()),
            "the unknown frame is skipped and the next one decodes"
        );
        assert!(r.next_msg::<OldClientMsg>().unwrap().is_none());
    }

    #[test]
    fn oversized_length_is_rejected() {
        let mut r = FrameReader::new();
        r.push(&u32::MAX.to_le_bytes());
        let err = r.next_msg::<ClientMsg>().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
