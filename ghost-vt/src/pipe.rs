//! The `ghost __pipe <name>` byte relay — the far end of the SSH transport.
//!
//! Run on the machine that hosts a session, it connects to that session's local
//! control socket and pumps bytes transparently between the socket and this
//! process's stdin/stdout. The local `ghost` reaches it as
//! `ssh user@host -- ghost __pipe <name>`, so its [`Session`](crate::client::Session)
//! / [`Subscriber`](crate::client::Subscriber) speak the ordinary framed
//! protocol to a *real* remote host — the relay never parses a frame, it only
//! moves bytes.

use crate::paths;
use rustix::event::{PollFd, PollFlags, poll};
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::Path;

/// Relay stdin/stdout to the named local session's control socket until either
/// side closes (the local client detaches, or the host exits).
pub fn run(name: &str) -> io::Result<()> {
    run_path(&paths::socket_path(name), name)
}

/// [`run`] against an explicit socket path — the seam the tests drive.
pub fn run_path(sock: &Path, name: &str) -> io::Result<()> {
    let socket = UnixStream::connect(sock)
        .map_err(|e| io::Error::new(e.kind(), format!("cannot reach session '{name}': {e}")))?;
    relay(socket)
}

/// Pump bytes both ways between this process's stdin/stdout and `socket`.
fn relay(mut socket: UnixStream) -> io::Result<()> {
    let stdin = rustix::stdio::stdin();
    let mut in_buf = [0u8; 8192];
    let mut sock_buf = [0u8; 8192];

    loop {
        let (stdin_re, sock_re) = {
            let mut fds = [
                PollFd::from_borrowed_fd(stdin, PollFlags::IN),
                PollFd::from_borrowed_fd(socket.as_fd(), PollFlags::IN),
            ];
            match poll(&mut fds, None) {
                Ok(_) => {}
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(e.into()),
            }
            (fds[0].revents(), fds[1].revents())
        };

        // stdin (the local client's frames) -> socket (the host)
        if stdin_re.intersects(PollFlags::IN | PollFlags::HUP) {
            let n = io::stdin().read(&mut in_buf)?;
            if n == 0 {
                break; // local client detached: EOF the host connection
            }
            socket.write_all(&in_buf[..n])?;
        }

        // socket (the host's frames) -> stdout (back to the local client)
        if sock_re.intersects(PollFlags::IN | PollFlags::HUP) {
            let n = socket.read(&mut sock_buf)?;
            if n == 0 {
                break; // host closed the session
            }
            let mut out = io::stdout().lock();
            out.write_all(&sock_buf[..n])?;
            out.flush()?;
        }
    }
    Ok(())
}
