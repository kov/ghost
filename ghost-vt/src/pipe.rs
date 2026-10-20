//! The `ghost __pipe <name>` byte relay — the far end of the SSH transport.
//!
//! Run on the machine that hosts a session, it connects to that session's local
//! control socket and pumps bytes transparently between the socket and this
//! process's stdin/stdout. The local `ghost` reaches it as
//! `ssh user@host -- ghost __pipe <name>`, so its [`Session`](crate::client::Session)
//! / [`Subscriber`](crate::client::Subscriber) speak the ordinary framed
//! protocol to a *real* remote host — the relay never parses a frame, it only
//! moves bytes.
//!
//! For tests, `GHOST_TEST_PIPE_DELAY_MS` holds every chunk that long in each
//! direction (order kept), so a loopback remote behaves like a slow link: the
//! round trip grows by twice the delay.

use crate::paths;
use rustix::event::{PollFd, PollFlags, poll};
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

/// The env var naming the test-only per-direction delay, in milliseconds.
pub const DELAY_ENV: &str = "GHOST_TEST_PIPE_DELAY_MS";

/// Relay stdin/stdout to the named local session's control socket until either
/// side closes (the local client detaches, or the host exits).
pub fn run(name: &str) -> io::Result<()> {
    run_path(&paths::socket_path(name), name)
}

/// [`run`] against an explicit socket path — the seam the tests drive.
pub fn run_path(sock: &Path, name: &str) -> io::Result<()> {
    let socket = UnixStream::connect(sock)
        .map_err(|e| io::Error::new(e.kind(), format!("cannot reach session '{name}': {e}")))?;
    let delay = std::env::var(DELAY_ENV)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .map(Duration::from_millis)
        .unwrap_or_default();
    relay(socket, delay)
}

/// Bytes held back until their due time, in arrival order.
#[derive(Default)]
struct DelayLine {
    delay: Duration,
    queue: VecDeque<(Instant, Vec<u8>)>,
}

impl DelayLine {
    fn new(delay: Duration) -> Self {
        DelayLine {
            delay,
            queue: VecDeque::new(),
        }
    }

    /// Hold `bytes`, read at `now`, until `now + delay`.
    fn push(&mut self, now: Instant, bytes: &[u8]) {
        self.queue.push_back((now + self.delay, bytes.to_vec()));
    }

    /// Everything due by `now`, in arrival order.
    fn take_due(&mut self, now: Instant) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some((due, _)) = self.queue.front() {
            if *due > now {
                break;
            }
            out.extend(self.queue.pop_front().unwrap().1);
        }
        out
    }

    /// When the oldest held chunk falls due, if any is held.
    fn next_due(&self) -> Option<Instant> {
        self.queue.front().map(|(due, _)| *due)
    }

    fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

/// Pump bytes both ways between this process's stdin/stdout and `socket`,
/// holding each chunk back by `delay` (zero in production).
fn relay(mut socket: UnixStream, delay: Duration) -> io::Result<()> {
    let stdin = rustix::stdio::stdin();
    let mut in_buf = [0u8; 8192];
    let mut sock_buf = [0u8; 8192];
    let mut to_host = DelayLine::new(delay);
    let mut to_client = DelayLine::new(delay);
    // A side that closed stops being read; the relay ends once what it sent
    // before closing has been delivered.
    let mut stdin_open = true;
    let mut sock_open = true;

    loop {
        let now = Instant::now();
        let up = to_host.take_due(now);
        if !up.is_empty() {
            socket.write_all(&up)?;
        }
        let down = to_client.take_due(now);
        if !down.is_empty() {
            let mut out = io::stdout().lock();
            out.write_all(&down)?;
            out.flush()?;
        }
        if (!stdin_open && to_host.is_empty()) || (!sock_open && to_client.is_empty()) {
            break;
        }

        let timeout = [to_host.next_due(), to_client.next_due()]
            .into_iter()
            .flatten()
            .min()
            .map(|due| {
                rustix::fs::Timespec::try_from(due.saturating_duration_since(Instant::now()))
                    .unwrap_or_default()
            });
        let (stdin_re, sock_re) = {
            let in_flags = if stdin_open {
                PollFlags::IN
            } else {
                PollFlags::empty()
            };
            let sock_flags = if sock_open {
                PollFlags::IN
            } else {
                PollFlags::empty()
            };
            let mut fds = [
                PollFd::from_borrowed_fd(stdin, in_flags),
                PollFd::from_borrowed_fd(socket.as_fd(), sock_flags),
            ];
            match poll(&mut fds, timeout.as_ref()) {
                Ok(_) => {}
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(e.into()),
            }
            (fds[0].revents(), fds[1].revents())
        };
        let now = Instant::now();

        // stdin (the local client's frames) -> socket (the host)
        if stdin_open && stdin_re.intersects(PollFlags::IN | PollFlags::HUP) {
            let n = io::stdin().read(&mut in_buf)?;
            if n == 0 {
                stdin_open = false; // local client detached: EOF the host connection
            } else {
                to_host.push(now, &in_buf[..n]);
            }
        }

        // socket (the host's frames) -> stdout (back to the local client)
        if sock_open && sock_re.intersects(PollFlags::IN | PollFlags::HUP) {
            let n = socket.read(&mut sock_buf)?;
            if n == 0 {
                sock_open = false; // host closed the session
            } else {
                to_client.push(now, &sock_buf[..n]);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_delay_line_releases_chunks_in_order_once_due() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut line = DelayLine::new(ms(100));
        line.push(t0, b"ab");
        line.push(t0 + ms(30), b"cd");
        assert_eq!(line.next_due(), Some(t0 + ms(100)));
        assert!(
            line.take_due(t0 + ms(99)).is_empty(),
            "nothing is due early"
        );
        assert_eq!(line.take_due(t0 + ms(100)), b"ab");
        assert_eq!(line.next_due(), Some(t0 + ms(130)));
        line.push(t0 + ms(40), b"ef");
        assert_eq!(
            line.take_due(t0 + ms(200)),
            b"cdef",
            "both due, in arrival order"
        );
        assert!(line.is_empty());
        assert_eq!(line.next_due(), None);
    }

    #[test]
    fn a_zero_delay_line_passes_bytes_straight_through() {
        let t0 = Instant::now();
        let mut line = DelayLine::new(Duration::ZERO);
        line.push(t0, b"now");
        assert_eq!(line.take_due(t0), b"now");
    }
}
