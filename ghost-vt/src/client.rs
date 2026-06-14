//! The attach client: a transparent pipe.
//!
//! Puts the terminal in raw mode and forwards stdin<->host byte-for-byte,
//! intercepting only the configurable detach/kill trigger (CLI default: `C-\`
//! prefix, then `d` to detach or `k` to kill; the prefix doubled sends a
//! literal). Everything else — including mouse reports and bracketed paste —
//! passes straight through, so the host terminal's native scrollback and mouse
//! keep working.

use crate::keys::{Action, Detacher};
use crate::paths;
use crate::protocol::{ClientMsg, FrameReader, ServerMsg, encode};
use crate::signals;
use nix::sys::signal::Signal;
use rustix::event::{PollFd, PollFlags, poll};
use rustix::termios::{OptionalActions, Termios, tcgetattr, tcgetwinsize, tcsetattr};
use std::io::{self, Read, Write};
use std::os::fd::BorrowedFd;
use std::os::unix::net::UnixStream;

/// Attach to the named session, returning when the user detaches or the session
/// ends.
pub fn attach(name: &str) -> io::Result<()> {
    let sock = paths::socket_path(name);
    let mut stream = UnixStream::connect(&sock)
        .map_err(|e| io::Error::new(e.kind(), format!("cannot attach to session '{name}': {e}")))?;

    let stdin = rustix::stdio::stdin();

    // Raw mode, restored on return via the guard's Drop.
    let _raw = RawMode::enable(stdin)?;

    // Sync the session to our current size immediately.
    send_resize(&mut stream, stdin)?;

    let sfd = signals::make(&[Signal::SIGWINCH])?;
    let mut detacher = Detacher::with_default_prefix();
    let mut reader = FrameReader::new();
    let mut in_buf = [0u8; 4096];
    let mut sock_buf = [0u8; 8192];

    loop {
        let (stdin_re, sock_re, sig_re) = {
            let mut fds = [
                PollFd::from_borrowed_fd(stdin, PollFlags::IN),
                PollFd::new(&stream, PollFlags::IN),
                PollFd::new(&sfd, PollFlags::IN),
            ];
            match poll(&mut fds, None) {
                Ok(_) => {}
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => return Err(e.into()),
            }
            (fds[0].revents(), fds[1].revents(), fds[2].revents())
        };

        // stdin -> host
        if stdin_re.contains(PollFlags::IN) {
            let n = io::stdin().read(&mut in_buf)?;
            if n == 0 {
                break;
            }
            for action in detacher.feed(&in_buf[..n]) {
                match action {
                    Action::Forward(bytes) => {
                        stream.write_all(&encode(&ClientMsg::Input(bytes)))?;
                    }
                    Action::Detach => {
                        eprint!("\r\n[ghost: detached]\r\n");
                        return Ok(());
                    }
                    Action::Kill => {
                        let _ = stream.write_all(&encode(&ClientMsg::Kill));
                        eprint!("\r\n[ghost: killed session]\r\n");
                        return Ok(());
                    }
                }
            }
        }

        // host -> stdout
        if sock_re.intersects(PollFlags::IN | PollFlags::HUP) {
            let n = stream.read(&mut sock_buf)?;
            if n == 0 {
                eprint!("\r\n[ghost: session closed]\r\n");
                return Ok(());
            }
            reader.push(&sock_buf[..n]);
            while let Some(msg) = reader.next_msg::<ServerMsg>()? {
                match msg {
                    ServerMsg::Output(bytes) => {
                        let mut out = io::stdout().lock();
                        out.write_all(&bytes)?;
                        out.flush()?;
                    }
                    ServerMsg::Exited(_code) => {
                        eprint!("\r\n[ghost: session ended]\r\n");
                        return Ok(());
                    }
                }
            }
        }

        // terminal resize -> host
        if sig_re.contains(PollFlags::IN) {
            signals::drain(&sfd)?;
            send_resize(&mut stream, stdin)?;
        }
    }
    Ok(())
}

fn send_resize(stream: &mut UnixStream, fd: BorrowedFd<'_>) -> io::Result<()> {
    if let Ok(ws) = tcgetwinsize(fd) {
        stream.write_all(&encode(&ClientMsg::Resize {
            cols: ws.ws_col,
            rows: ws.ws_row,
        }))?;
    }
    Ok(())
}

/// Puts a terminal fd into raw mode and restores the original settings on drop.
struct RawMode {
    fd: BorrowedFd<'static>,
    original: Termios,
}

impl RawMode {
    fn enable(fd: BorrowedFd<'static>) -> io::Result<Self> {
        let original = tcgetattr(fd).map_err(io::Error::from)?;
        let mut raw = original.clone();
        raw.make_raw();
        tcsetattr(fd, OptionalActions::Now, &raw).map_err(io::Error::from)?;
        Ok(RawMode { fd, original })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = tcsetattr(self.fd, OptionalActions::Now, &self.original);
    }
}
