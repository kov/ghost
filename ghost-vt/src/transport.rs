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
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
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

/// A child process whose stdio *is* the transport: bytes written go to its
/// stdin, bytes read come from its stdout. This is how the protocol tunnels over
/// SSH — the child is `ssh user@host -- ghost __pipe <name>`, and `ghost __pipe`
/// on the far side relays those bytes to the remote session host's control
/// socket transparently, so the local client speaks the ordinary framed protocol
/// to a real remote host. The two pipes are distinct fds: reads poll and drain
/// stdout, writes go to stdin.
pub struct SshChild {
    child: Child,
    stdin: ChildStdin,
    stdout: ChildStdout,
}

impl SshChild {
    /// Spawn `cmd` with piped stdin/stdout as the byte channel. Its stderr is
    /// left inherited so ssh's own diagnostics (auth prompts, host-key warnings)
    /// still reach the user's terminal.
    pub fn spawn(mut cmd: Command) -> io::Result<Self> {
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        Ok(SshChild {
            child,
            stdin,
            stdout,
        })
    }

    /// Put both pipe ends into (non-)blocking mode. Pipes have no read-timeout
    /// (`SO_RCVTIMEO` is socket-only), so the event-loop client relies on
    /// non-blocking reads gated by a `poll()` on [`as_fd`](SshChild::as_fd).
    fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        set_fd_nonblocking(self.stdout.as_fd(), nonblocking)?;
        set_fd_nonblocking(self.stdin.as_fd(), nonblocking)
    }
}

impl Read for SshChild {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stdout.read(buf)
    }
}

impl Write for SshChild {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stdin.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.stdin.flush()
    }
}

impl Transport for SshChild {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.stdout.as_fd()
    }
}

impl Drop for SshChild {
    fn drop(&mut self) {
        // Detaching (dropping the transport) should tear the ssh channel down,
        // not leave a stray `ssh` lingering: closing our pipe ends gives it EOF,
        // and we reap it so it can't zombie.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Toggle `O_NONBLOCK` on a raw fd (pipe ends have no higher-level setter).
fn set_fd_nonblocking(fd: BorrowedFd<'_>, nonblocking: bool) -> io::Result<()> {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    let mut flags = fcntl_getfl(fd)?;
    flags.set(OFlags::NONBLOCK, nonblocking);
    fcntl_setfl(fd, flags)?;
    Ok(())
}

/// The concrete transports the client stack speaks over: a local Unix socket
/// (an ordinary local session) or an [`SshChild`] tunnel (a remote host reached
/// over SSH). Kept a plain enum rather than making [`Conn`]/`Client`/`Session`
/// generic, so the whole GUI session machinery stays one concrete type with only
/// a tiny per-call match.
pub enum AnyTransport {
    Unix(UnixStream),
    Ssh(SshChild),
}

impl Read for AnyTransport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            AnyTransport::Unix(s) => s.read(buf),
            AnyTransport::Ssh(s) => s.read(buf),
        }
    }
}

impl Write for AnyTransport {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            AnyTransport::Unix(s) => s.write(buf),
            AnyTransport::Ssh(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            AnyTransport::Unix(s) => s.flush(),
            AnyTransport::Ssh(s) => s.flush(),
        }
    }
}

impl Transport for AnyTransport {
    fn as_fd(&self) -> BorrowedFd<'_> {
        match self {
            AnyTransport::Unix(s) => Transport::as_fd(s),
            AnyTransport::Ssh(s) => s.as_fd(),
        }
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
pub struct Conn<T: Transport = AnyTransport> {
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
    /// Put an accepted Unix connection into (non-)blocking mode. The host sets
    /// this on every connection it accepts; the client speaks over
    /// [`AnyTransport`] and uses the variant-aware setter below.
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.io.set_nonblocking(nonblocking)
    }
}

impl Conn<AnyTransport> {
    /// Connect to a session's local Unix control socket.
    pub fn connect(path: &Path) -> io::Result<Self> {
        Ok(Conn::new(AnyTransport::Unix(UnixStream::connect(path)?)))
    }

    /// Tunnel to a remote session host over an [`SshChild`]: spawn `cmd` (an
    /// `ssh … -- ghost __pipe <name>`) and speak the framed protocol over its
    /// stdio, exactly as over a local socket.
    pub fn connect_ssh(cmd: Command) -> io::Result<Self> {
        Ok(Conn::new(AnyTransport::Ssh(SshChild::spawn(cmd)?)))
    }

    /// Put the underlying channel into (non-)blocking mode. The blocking default
    /// suits the poll-gated client; the shell's subscription pool sets
    /// non-blocking so no idle read stalls its loop.
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        match &self.io {
            AnyTransport::Unix(s) => s.set_nonblocking(nonblocking),
            AnyTransport::Ssh(s) => s.set_nonblocking(nonblocking),
        }
    }

    /// Bound how long a [`recv`](Conn::recv) read waits for data; `None` blocks
    /// until readable. A pipe (the ssh tunnel) has no read timeout, so any
    /// bounded wait there is honoured as "don't block" — the caller polls
    /// [`as_fd`](Conn::as_fd) for readiness instead.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        match &self.io {
            AnyTransport::Unix(s) => s.set_read_timeout(timeout),
            AnyTransport::Ssh(s) => s.set_nonblocking(timeout.is_some()),
        }
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

    #[test]
    fn ssh_child_transport_round_trips_frames_through_a_child_process() {
        // `cat` forwards its stdin to its stdout byte-for-byte — a stand-in for
        // ssh's transparent stdio relay to a remote `ghost __pipe`. A frame sent
        // through it comes back out and decodes, proving the SshChild wiring
        // (write→stdin, read→stdout) and that AnyTransport::Ssh carries the
        // framed protocol unchanged.
        let child = SshChild::spawn(Command::new("cat")).expect("spawn cat");
        let mut conn = Conn::new(AnyTransport::Ssh(child));
        conn.send(&ClientMsg::Input(b"ping".to_vec())).unwrap();

        // cat may deliver the echo in more than one read; drain until the frame
        // completes.
        let mut got: Vec<ClientMsg> = Vec::new();
        for _ in 0..1000 {
            if let Some(msgs) = conn.recv::<ClientMsg>().unwrap() {
                got.extend(msgs);
            }
            if !got.is_empty() {
                break;
            }
        }
        assert_eq!(got, vec![ClientMsg::Input(b"ping".to_vec())]);
    }

    #[test]
    fn a_nonblocking_recv_returns_at_once_when_no_data_is_ready() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut ca = Conn::new(a);
        ca.set_nonblocking(true).unwrap();
        // Keep the peer open so the read is "no data yet", not EOF.
        let _peer = b;
        // With nothing buffered, recv must not block: it yields an empty batch
        // (would-block mapped to "drained"), never `None` (that is EOF) and
        // never hangs. This is what lets a front-end pump a pool of idle
        // sessions each frame without a per-session read stalling the loop.
        let got: Option<Vec<ClientMsg>> = ca.recv().unwrap();
        assert_eq!(got, Some(vec![]));
    }
}
