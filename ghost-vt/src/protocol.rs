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
    /// Rename the session to this name. The host renames its socket, pidfile,
    /// and recording, then replies with [`ServerMsg::RenameResult`].
    Rename(String),
    /// Ask the host to repaint the screen (re-send the current state). Used by
    /// the client to heal its display after drawing a transient overlay such as
    /// the rename prompt.
    Repaint,
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
}

/// Upper bound on a frame body, guarding against corrupt or hostile length
/// prefixes before we allocate.
const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

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
    pub fn next_msg<M: serde::de::DeserializeOwned>(&mut self) -> io::Result<Option<M>> {
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
        let msg = postcard::from_bytes(&self.buf[4..4 + len]).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("malformed protocol frame: {e}"),
            )
        })?;
        self.buf.drain(..4 + len);
        Ok(Some(msg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn oversized_length_is_rejected() {
        let mut r = FrameReader::new();
        r.push(&u32::MAX.to_le_bytes());
        let err = r.next_msg::<ClientMsg>().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
