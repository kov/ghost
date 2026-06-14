//! The session host: a synchronous `poll()` loop over the PTY master, the
//! listening socket, the attached client connection, and signals (via
//! `signalfd`).
//!
//! Single-threaded and lock-free by construction — one owner of the terminal
//! and the client connection. No async runtime: the fd count is small and
//! fixed, and a plain poll loop keeps backtraces and profiling honest.
//!
//! [`spawn`] daemonizes (classic double-fork) so the host outlives the launching
//! command and has no controlling terminal of its own; it just shuffles bytes
//! between the child's PTY and the attached client.

use crate::paths;
use crate::protocol::{ClientMsg, FrameReader, ServerMsg, encode};
use crate::screen::{DEFAULT_SCROLLBACK, Screen};
use nix::sys::signal::Signal;
use pty_process::Size;
use pty_process::blocking::{Command as PtyCommand, open};
use rustix::event::{PollFd, PollFlags, poll};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};

/// Options for starting a session.
pub struct SpawnOpts {
    /// Session name (used for the socket and pidfile).
    pub name: String,
    /// Command and arguments to run; empty means `$SHELL` (then `/bin/sh`).
    pub command: Vec<String>,
    /// Initial terminal size as `(cols, rows)`.
    pub size: (u16, u16),
}

enum Fork {
    Parent,
    Daemon,
}

/// A single attached client connection.
struct Client {
    stream: UnixStream,
    reader: FrameReader,
    /// Output queued for the client but not yet written (backpressure buffer).
    outbuf: Vec<u8>,
}

impl Client {
    fn new(stream: UnixStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Client {
            stream,
            reader: FrameReader::new(),
            outbuf: Vec::new(),
        })
    }

    fn queue(&mut self, msg: &ServerMsg) {
        self.outbuf.extend_from_slice(&encode(msg));
    }

    /// Write as much of the pending output as the socket will accept.
    fn flush(&mut self) -> io::Result<()> {
        while !self.outbuf.is_empty() {
            match self.stream.write(&self.outbuf) {
                Ok(0) => break,
                Ok(n) => {
                    self.outbuf.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// Start a session in the background.
///
/// Returns `Ok(())` in the calling process once the host has been forked off.
/// In the host process this function does not return — it runs the session and
/// exits the process when the session ends.
pub fn spawn(opts: SpawnOpts) -> io::Result<()> {
    paths::ensure_runtime_dir()?;
    let sock = paths::socket_path(&opts.name);
    let pidf = paths::pid_path(&opts.name);

    if crate::session::list()?.iter().any(|s| s.name == opts.name) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("session '{}' already exists", opts.name),
        ));
    }
    // Clear any stale socket left by a dead host, then claim the name.
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock)?;

    match unsafe { daemonize()? } {
        Fork::Parent => return Ok(()),
        Fork::Daemon => {}
    }

    // We are now the long-lived host; there is no caller to return errors to.
    let result = host_main(&listener, &opts);
    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(&pidf);
    std::process::exit(result.unwrap_or(1));
}

fn host_main(listener: &UnixListener, opts: &SpawnOpts) -> io::Result<i32> {
    std::fs::write(paths::pid_path(&opts.name), std::process::id().to_string())?;
    listener.set_nonblocking(true)?;

    let (pty, pts) = open().map_err(io::Error::other)?;
    let (cols, rows) = opts.size;
    pty.resize(Size::new(rows, cols))
        .map_err(io::Error::other)?;
    let (prog, args) = split_command(&opts.command);
    let mut child = PtyCommand::new(&prog)
        .args(&args)
        .spawn(pts)
        .map_err(io::Error::other)?;

    let sfd = crate::signals::make(&[Signal::SIGCHLD, Signal::SIGTERM, Signal::SIGINT])?;

    // Authoritative screen state, fed every byte the child writes so a late
    // attach can be repainted to the current state.
    let mut screen = Screen::new(cols, rows, DEFAULT_SCROLLBACK);

    let mut client: Option<Client> = None;
    let mut ptybuf = [0u8; 8192];

    loop {
        // Build the poll set (client slot only when one is attached).
        let mut fds = vec![
            PollFd::new(&pty, PollFlags::IN),
            PollFd::new(listener, PollFlags::IN),
            PollFd::new(&sfd, PollFlags::IN),
        ];
        if let Some(c) = &client {
            let mut flags = PollFlags::IN;
            if !c.outbuf.is_empty() {
                flags |= PollFlags::OUT;
            }
            fds.push(PollFd::new(&c.stream, flags));
        }
        match poll(&mut fds, None) {
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(e.into()),
        }
        let pty_re = fds[0].revents();
        let listener_re = fds[1].revents();
        let sig_re = fds[2].revents();
        let client_re = if client.is_some() {
            fds[3].revents()
        } else {
            PollFlags::empty()
        };
        drop(fds);

        // PTY output -> authoritative screen state, and live to the attached
        // client (if any). State is always tracked so the next attach can be
        // repainted even after a period with nobody attached.
        if pty_re.intersects(PollFlags::IN | PollFlags::HUP) {
            match (&pty).read(&mut ptybuf) {
                Ok(0) => return child_exited(&mut child, &mut client),
                Ok(n) => {
                    screen.feed(&ptybuf[..n]);
                    if let Some(c) = &mut client {
                        c.queue(&ServerMsg::Output(ptybuf[..n].to_vec()));
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                // EIO on the master means the child closed the slave (exited).
                Err(_) => return child_exited(&mut child, &mut client),
            }
        }

        // New connection: the latest attach takes over and is repainted to the
        // current screen state before any live bytes follow.
        if listener_re.contains(PollFlags::IN)
            && let Ok((stream, _)) = listener.accept()
        {
            let mut c = Client::new(stream)?;
            c.queue(&ServerMsg::Output(screen.resync()));
            client = Some(c);
        }

        // Client -> host (input and control messages).
        if client_re.intersects(PollFlags::IN | PollFlags::HUP) {
            let mut drop_client = false;
            if let Some(c) = &mut client {
                let mut buf = [0u8; 4096];
                match c.stream.read(&mut buf) {
                    Ok(0) => drop_client = true,
                    Ok(n) => {
                        c.reader.push(&buf[..n]);
                        loop {
                            match c.reader.next_msg::<ClientMsg>() {
                                Ok(Some(ClientMsg::Input(bytes))) => {
                                    (&pty).write_all(&bytes)?;
                                }
                                Ok(Some(ClientMsg::Resize { cols, rows })) => {
                                    let _ = pty.resize(Size::new(rows, cols));
                                    screen.resize(cols, rows);
                                }
                                Ok(Some(ClientMsg::Detach)) => {
                                    drop_client = true;
                                    break;
                                }
                                Ok(Some(ClientMsg::Kill)) => {
                                    let _ = child.kill();
                                    let _ = child.wait();
                                    return Ok(0);
                                }
                                Ok(None) => break,
                                Err(_) => {
                                    drop_client = true;
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(_) => drop_client = true,
                }
            }
            if drop_client {
                client = None;
            }
        }

        // Push queued output to the client.
        if let Some(c) = &mut client
            && c.flush().is_err()
        {
            client = None;
        }

        // Signals.
        if sig_re.contains(PollFlags::IN) {
            for signo in crate::signals::drain(&sfd)? {
                match signo {
                    libc::SIGCHLD => {
                        if let Ok(Some(status)) = child.try_wait() {
                            let code = status.code().unwrap_or(0);
                            notify_exit(&mut client, code);
                            return Ok(code);
                        }
                    }
                    libc::SIGTERM | libc::SIGINT => {
                        let _ = child.kill();
                        let _ = child.wait();
                        notify_exit(&mut client, 0);
                        return Ok(0);
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Reap the child after the PTY signalled EOF and tell the client.
fn child_exited(child: &mut std::process::Child, client: &mut Option<Client>) -> io::Result<i32> {
    let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(0);
    notify_exit(client, code);
    Ok(code)
}

/// Best-effort notification to the client that the session ended.
fn notify_exit(client: &mut Option<Client>, code: i32) {
    if let Some(c) = client {
        let _ = c.flush();
        let _ = c.stream.write_all(&encode(&ServerMsg::Exited(code)));
    }
}

fn split_command(command: &[String]) -> (String, Vec<String>) {
    match command.split_first() {
        Some((first, rest)) => (first.clone(), rest.to_vec()),
        None => {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
            (shell, Vec::new())
        }
    }
}

/// Classic double-fork daemonization. Returns [`Fork::Parent`] in the original
/// process and [`Fork::Daemon`] in the detached grandchild.
unsafe fn daemonize() -> io::Result<Fork> {
    match unsafe { libc::fork() } {
        -1 => return Err(io::Error::last_os_error()),
        0 => {}
        _ => return Ok(Fork::Parent),
    }
    // New session: detach from the controlling terminal.
    rustix::process::setsid().map_err(io::Error::from)?;
    // Second fork: ensure we are not a session leader, so we can never reacquire
    // a controlling terminal.
    match unsafe { libc::fork() } {
        -1 => return Err(io::Error::last_os_error()),
        0 => {}
        _ => std::process::exit(0),
    }
    let _ = std::env::set_current_dir("/");
    unsafe { libc::umask(0) };
    if let Ok(devnull) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")
    {
        let nfd = devnull.as_raw_fd();
        unsafe {
            libc::dup2(nfd, 0);
            libc::dup2(nfd, 1);
            libc::dup2(nfd, 2);
        }
    }
    Ok(Fork::Daemon)
}
