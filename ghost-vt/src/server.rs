//! The session host: a synchronous `poll()` loop over the PTY master, the
//! listening socket, attached client connections, and signals (via `signalfd`).
//!
//! Single-threaded and lock-free by construction — one owner of the terminal
//! state and the recorder. No async runtime: the fd count is small and fixed,
//! and a plain poll loop keeps backtraces and profiling honest.
//!
//! [`spawn`] daemonizes (classic double-fork) so the host outlives the launching
//! command and has no controlling terminal of its own; it just shuffles bytes
//! between the child's PTY and attached clients.

use crate::paths;
use pty_process::Size;
use pty_process::blocking::Command as PtyCommand;
use rustix::event::{PollFd, PollFlags, poll};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;

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

    // Open the PTY and spawn the child under it.
    let (pty, pts) = pty_process::blocking::open().map_err(io::Error::other)?;
    let (cols, rows) = opts.size;
    pty.resize(Size::new(rows, cols))
        .map_err(io::Error::other)?;
    let (prog, args) = split_command(&opts.command);
    let mut child = PtyCommand::new(&prog)
        .args(&args)
        .spawn(pts)
        .map_err(io::Error::other)?;

    // Keep the PTY master open for the child's lifetime. Draining its output and
    // forwarding it to attached clients arrives with the attach milestone; for
    // now the host only manages the session's lifecycle.
    let _pty = pty;

    let sfd = signals::make()?;

    loop {
        let mut fds = [
            PollFd::new(listener, PollFlags::IN),
            PollFd::new(&sfd, PollFlags::IN),
        ];
        match poll(&mut fds, None) {
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(e.into()),
        }

        if fds[0].revents().contains(PollFlags::IN) {
            // Attach is not implemented yet; accept and drop so the backlog
            // never stalls and `ghost ls`-style probes still see a live socket.
            if let Ok((stream, _)) = listener.accept() {
                drop(stream);
            }
        }

        if fds[1].revents().contains(PollFlags::IN) {
            for signo in signals::drain(&sfd)? {
                match signo {
                    libc::SIGCHLD => {
                        if let Ok(Some(status)) = child.try_wait() {
                            return Ok(status.code().unwrap_or(0));
                        }
                    }
                    libc::SIGTERM | libc::SIGINT => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Ok(0);
                    }
                    _ => {}
                }
            }
        }
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

/// Signal handling for the host's poll loop via `signalfd`.
mod signals {
    use nix::sys::signal::{SigSet, SigmaskHow, Signal, pthread_sigmask};
    use nix::sys::signalfd::{SfdFlags, SignalFd};
    use std::io;

    /// Block SIGCHLD/SIGTERM/SIGINT and return a `signalfd` that surfaces them
    /// in the poll loop.
    pub fn make() -> io::Result<SignalFd> {
        let mut mask = SigSet::empty();
        mask.add(Signal::SIGCHLD);
        mask.add(Signal::SIGTERM);
        mask.add(Signal::SIGINT);
        pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&mask), None).map_err(nix_io)?;
        SignalFd::with_flags(&mask, SfdFlags::SFD_NONBLOCK | SfdFlags::SFD_CLOEXEC).map_err(nix_io)
    }

    /// Drain all pending signals, returning their signal numbers.
    pub fn drain(sfd: &SignalFd) -> io::Result<Vec<i32>> {
        let mut signos = Vec::new();
        while let Some(info) = sfd.read_signal().map_err(nix_io)? {
            signos.push(info.ssi_signo as i32);
        }
        Ok(signos)
    }

    fn nix_io(e: nix::Error) -> io::Error {
        io::Error::from_raw_os_error(e as i32)
    }
}
