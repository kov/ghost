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
    // Some(_) while the rename prompt is on screen; input then feeds the prompt
    // rather than the session, and live output is suppressed until it closes.
    let mut prompt: Option<RenamePrompt> = None;

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

        // stdin -> host (or the rename prompt, when one is open)
        if stdin_re.contains(PollFlags::IN) {
            let n = io::stdin().read(&mut in_buf)?;
            if n == 0 {
                break;
            }
            if prompt.is_some() {
                prompt_input(&in_buf[..n], &mut prompt, &mut stream, stdin)?;
            } else {
                for action in detacher.feed(&in_buf[..n]) {
                    match action {
                        Action::Forward(bytes) => {
                            // A prefix-r mid-batch opens the prompt; route any
                            // bytes typed after it on this same read to it.
                            if prompt.is_some() {
                                prompt_input(&bytes, &mut prompt, &mut stream, stdin)?;
                            } else {
                                stream.write_all(&encode(&ClientMsg::Input(bytes)))?;
                            }
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
                        Action::Rename => {
                            start_prompt(&mut prompt, stdin)?;
                        }
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
                        // While the prompt overlay is up, drop live output; the
                        // screen is repainted from authoritative state when the
                        // prompt closes (via a Repaint request).
                        if prompt.is_none() {
                            let mut out = io::stdout().lock();
                            out.write_all(&bytes)?;
                            out.flush()?;
                        }
                    }
                    ServerMsg::Exited(_code) => {
                        eprint!("\r\n[ghost: session ended]\r\n");
                        return Ok(());
                    }
                    ServerMsg::RenameResult { ok, message } => {
                        rename_result(ok, &message, &mut prompt, &mut stream, stdin)?;
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

/// Rename a session non-interactively (the `ghost rename` command). Connects to
/// the named session and sends a single rename request, returning the host's
/// verdict. Sends no resize, so the host treats it as a control connection and
/// does not disturb any attached client.
pub fn rename(old: &str, new: &str) -> io::Result<()> {
    let sock = paths::socket_path(old);
    let mut stream = UnixStream::connect(&sock)
        .map_err(|e| io::Error::new(e.kind(), format!("cannot reach session '{old}': {e}")))?;
    stream.write_all(&encode(&ClientMsg::Rename(new.to_string())))?;

    let mut reader = FrameReader::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = stream.read(&mut buf)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "session closed before replying",
            ));
        }
        reader.push(&buf[..n]);
        while let Some(msg) = reader.next_msg::<ServerMsg>()? {
            if let ServerMsg::RenameResult { ok, message } = msg {
                return if ok {
                    Ok(())
                } else {
                    Err(io::Error::other(message))
                };
            }
        }
    }
}

/// State of the on-screen rename prompt opened by `C-\ r`.
struct RenamePrompt {
    /// The name typed so far.
    buf: String,
    /// True once submitted: input is ignored until the host's reply arrives.
    awaiting: bool,
}

/// Open the rename prompt: save the cursor (once) and draw the empty prompt on
/// the bottom row.
fn start_prompt(prompt: &mut Option<RenamePrompt>, fd: BorrowedFd<'_>) -> io::Result<()> {
    *prompt = Some(RenamePrompt {
        buf: String::new(),
        awaiting: false,
    });
    let mut out = io::stdout().lock();
    out.write_all(b"\x1b7")?; // DECSC: save cursor, restored when the prompt closes
    render_prompt(&mut out, fd, "", None)?;
    Ok(())
}

/// Feed a chunk of stdin to the open prompt: edit the name, submit on Enter, or
/// cancel on Esc / Ctrl-C.
fn prompt_input(
    bytes: &[u8],
    prompt: &mut Option<RenamePrompt>,
    stream: &mut UnixStream,
    fd: BorrowedFd<'_>,
) -> io::Result<()> {
    if prompt.as_ref().is_none_or(|p| p.awaiting) {
        return Ok(()); // submitted: wait for the host's reply before editing again
    }
    let mut out = io::stdout().lock();
    for &b in bytes {
        match b {
            b'\r' | b'\n' => {
                let name = prompt.as_ref().unwrap().buf.trim().to_string();
                if name.is_empty() {
                    continue;
                }
                stream.write_all(&encode(&ClientMsg::Rename(name)))?;
                prompt.as_mut().unwrap().awaiting = true;
                break;
            }
            0x1b | 0x03 => {
                // Esc / Ctrl-C: cancel. Restore the cursor and ask the host to
                // repaint, healing the row the prompt drew over.
                out.write_all(b"\x1b8")?;
                out.flush()?;
                *prompt = None;
                stream.write_all(&encode(&ClientMsg::Repaint))?;
                return Ok(());
            }
            0x7f | 0x08 => {
                prompt.as_mut().unwrap().buf.pop();
                render_prompt(&mut out, fd, &prompt.as_ref().unwrap().buf, None)?;
            }
            0x20..=0x7e => {
                prompt.as_mut().unwrap().buf.push(b as char);
                render_prompt(&mut out, fd, &prompt.as_ref().unwrap().buf, None)?;
            }
            _ => {} // ignore other control bytes
        }
    }
    Ok(())
}

/// Handle the host's reply to a submitted rename.
fn rename_result(
    ok: bool,
    message: &str,
    prompt: &mut Option<RenamePrompt>,
    stream: &mut UnixStream,
    fd: BorrowedFd<'_>,
) -> io::Result<()> {
    if prompt.is_none() {
        return Ok(());
    }
    if ok {
        *prompt = None;
        let mut out = io::stdout().lock();
        out.write_all(b"\x1b8")?; // restore cursor
        out.flush()?;
        stream.write_all(&encode(&ClientMsg::Repaint))?; // repaint over the prompt
    } else {
        // Refused: re-enable editing and show why, so the user can fix the name.
        if let Some(p) = prompt.as_mut() {
            p.awaiting = false;
        }
        let buf = prompt.as_ref().unwrap().buf.clone();
        let mut out = io::stdout().lock();
        render_prompt(&mut out, fd, &buf, Some(message))?;
    }
    Ok(())
}

/// Draw the prompt on the terminal's bottom row (reverse video), leaving the
/// cursor after the typed text.
fn render_prompt(
    out: &mut impl Write,
    fd: BorrowedFd<'_>,
    buf: &str,
    error: Option<&str>,
) -> io::Result<()> {
    let (_, rows) = term_size(fd);
    let mut s = format!("\x1b[{rows};1H\x1b[2K\x1b[7m");
    match error {
        None => s.push_str(&format!(" rename session to: {buf} ")),
        Some(e) => s.push_str(&format!(" rename to: {buf}  [{e}] — esc to cancel ")),
    }
    s.push_str("\x1b[0m");
    out.write_all(s.as_bytes())?;
    out.flush()
}

/// Current terminal size as `(cols, rows)`, defaulting to 80x24 if unavailable.
fn term_size(fd: BorrowedFd<'_>) -> (u16, u16) {
    tcgetwinsize(fd)
        .map(|ws| (ws.ws_col, ws.ws_row))
        .unwrap_or((80, 24))
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
