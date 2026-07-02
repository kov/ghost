//! The client<->server transport seam.
//!
//! [`Transport`] is the byte channel the protocol runs over — a local Unix
//! socket today; SSH-tunneled stdio and (later) a mosh-style UDP transport plug
//! in behind the same trait. [`Conn`] layers the framed protocol on top of any
//! transport: typed [`send`](Conn::send)/[`queue`](Conn::queue) out, drained
//! [`recv`](Conn::recv) in, with an outgoing buffer for non-blocking
//! backpressure. The session host and the attach client both speak through it
//! instead of hand-rolling [`FrameReader`] loops over a raw socket.

use crate::protocol::{FrameReader, encode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

/// A bidirectional byte channel the protocol runs over. A local Unix socket is
/// the only implementation today; SSH-tunneled stdio and a UDP transport will
/// implement this later without the host or client logic having to change.
pub trait Transport: Read + Write {
    /// The file descriptor to watch for readiness in a `poll()` loop.
    fn as_fd(&self) -> BorrowedFd<'_>;
}

impl Transport for UnixStream {
    fn as_fd(&self) -> BorrowedFd<'_> {
        AsFd::as_fd(self)
    }
}

/// A framed protocol connection over a [`Transport`]: send typed messages, and
/// drain typed messages as whole frames arrive. Outgoing data is buffered so a
/// non-blocking transport can apply backpressure — poll for writability when
/// [`wants_write`](Conn::wants_write) is true, then [`flush`](Conn::flush).
///
/// Shared by the attach [`client`](crate::client) and the session
/// [`host`](crate::server)'s per-connection state, so the framing lives in one
/// place.
pub struct Conn<T: Transport = UnixStream> {
    io: T,
    reader: FrameReader,
    outbuf: Vec<u8>,
}

impl<T: Transport> Conn<T> {
    /// Wrap an already-established transport.
    pub fn new(io: T) -> Self {
        Conn {
            io,
            reader: FrameReader::new(),
            outbuf: Vec::new(),
        }
    }

    /// The connection's file descriptor, for a poll/epoll/GLib readiness watch.
    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.io.as_fd()
    }

    /// Encode and buffer a message without writing. Pair with
    /// [`flush`](Conn::flush) when the transport polls writable.
    pub fn queue<M: Serialize>(&mut self, msg: &M) {
        self.outbuf.extend_from_slice(&encode(msg));
    }

    /// Whether there is buffered output still waiting to be written.
    pub fn wants_write(&self) -> bool {
        !self.outbuf.is_empty()
    }

    /// How many buffered bytes are still waiting to be written — the peer's
    /// backlog, for callers that bound what they queue to a slow reader.
    pub fn pending(&self) -> usize {
        self.outbuf.len()
    }

    /// Write as much buffered output as the transport will accept now, stopping
    /// on would-block and leaving the rest for the next call.
    pub fn flush(&mut self) -> io::Result<()> {
        while !self.outbuf.is_empty() {
            match self.io.write(&self.outbuf) {
                Ok(0) => break,
                Ok(n) => {
                    self.outbuf.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Queue a message and flush as far as the transport accepts now.
    pub fn send<M: Serialize>(&mut self, msg: &M) -> io::Result<()> {
        self.queue(msg);
        self.flush()
    }

    /// Read once and return every message that completed.
    ///
    /// `Ok(None)` means clean EOF (the peer closed). A read that would block,
    /// times out, or is interrupted yields `Ok(Some(vec![]))`, so this is safe to
    /// call when [`as_fd`](Conn::as_fd) polls readable or with a read timeout set.
    pub fn recv<M: DeserializeOwned>(&mut self) -> io::Result<Option<Vec<M>>> {
        let mut buf = [0u8; 8192];
        match self.io.read(&mut buf) {
            Ok(0) => Ok(None),
            Ok(n) => {
                self.reader.push(&buf[..n]);
                let mut msgs = Vec::new();
                while let Some(msg) = self.reader.next_msg::<M>()? {
                    msgs.push(msg);
                }
                Ok(Some(msgs))
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                Ok(Some(Vec::new()))
            }
            Err(e) => Err(e),
        }
    }
}

impl Conn<UnixStream> {
    /// Connect to a session's Unix control socket.
    pub fn connect(path: &Path) -> io::Result<Self> {
        Ok(Conn::new(UnixStream::connect(path)?))
    }

    /// Put the underlying socket into (non-)blocking mode. The host sets this on
    /// accepted connections; the blocking default suits the poll-gated client.
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.io.set_nonblocking(nonblocking)
    }

    /// Bound how long a [`recv`](Conn::recv) read waits for data; `None` blocks
    /// until readable.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.io.set_read_timeout(timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ClientMsg, ServerMsg};

    #[test]
    fn conn_round_trips_framed_messages_and_signals_eof() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut ca = Conn::new(a);
        let mut cb = Conn::new(b);

        // Two messages sent back-to-back arrive as two whole frames.
        ca.send(&ClientMsg::Input(b"hello".to_vec())).unwrap();
        ca.send(&ClientMsg::Resize { cols: 80, rows: 24 }).unwrap();
        let msgs: Vec<ClientMsg> = cb.recv().unwrap().unwrap();
        assert_eq!(
            msgs,
            vec![
                ClientMsg::Input(b"hello".to_vec()),
                ClientMsg::Resize { cols: 80, rows: 24 },
            ]
        );

        // Closing the peer surfaces as a clean EOF.
        drop(ca);
        let after: Option<Vec<ServerMsg>> = cb.recv().unwrap();
        assert_eq!(after, None);
    }
}
