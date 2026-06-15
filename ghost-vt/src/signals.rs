//! Self-pipe signal helpers shared by the host and client poll loops.
//!
//! `poll()` can't wait on signals directly, so we bridge them onto a file
//! descriptor: an installed handler writes each delivered signal's number into
//! the write end of a pipe, and the non-blocking read end slots into the poll
//! set like any other fd. Linux offers `signalfd` for this, but it doesn't
//! exist on macOS/BSD, so the self-pipe trick keeps the loops portable. The
//! handler only performs an atomic load and a `write(2)`, both async-signal-safe.

use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
use rustix::io::Errno;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, IntoRawFd, OwnedFd};
use std::sync::atomic::{AtomicI32, Ordering};

/// Write end of the self-pipe, shared with the signal handler. A raw fd kept
/// open for the lifetime of the process (signal numbers fit in a byte, so a
/// single global pipe serves all registered signals).
static WRITE_FD: AtomicI32 = AtomicI32::new(-1);

/// The pollable read end of the self-pipe. Implements [`AsFd`] so it can be
/// dropped straight into a `poll()` set.
pub struct Signals {
    read: OwnedFd,
}

impl AsFd for Signals {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.read.as_fd()
    }
}

extern "C" fn handler(signo: libc::c_int) {
    let fd = WRITE_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        let byte = signo as u8;
        // Async-signal-safe; a full pipe (EAGAIN) just coalesces wakeups, which
        // is harmless since the loop re-reads state on every iteration.
        unsafe {
            libc::write(fd, (&byte as *const u8).cast(), 1);
        }
    }
}

/// Install a handler for `signals` that funnels them onto a pollable fd.
pub fn make(signals: &[Signal]) -> io::Result<Signals> {
    // Plain `pipe()` rather than `pipe2`/`pipe_with` for macOS portability; the
    // non-blocking and close-on-exec flags are set explicitly below.
    let (read, write) = rustix::pipe::pipe()?;
    set_cloexec_nonblock(read.as_fd())?;
    set_cloexec_nonblock(write.as_fd())?;
    // Hand the write end to the signal handler. Leaked deliberately: the handler
    // outlives any single caller, and there is exactly one `make` per process.
    WRITE_FD.store(write.into_raw_fd(), Ordering::SeqCst);

    let action = SigAction::new(
        SigHandler::Handler(handler),
        SaFlags::SA_RESTART,
        SigSet::empty(),
    );
    for &sig in signals {
        // SAFETY: `handler` only touches an atomic and calls `write(2)`.
        unsafe { sigaction(sig, &action) }.map_err(nix_io)?;
    }
    Ok(Signals { read })
}

/// Drain all currently pending signals, returning their signal numbers.
pub fn drain(sig: &Signals) -> io::Result<Vec<i32>> {
    let mut signos = Vec::new();
    let mut buf = [0u8; 64];
    loop {
        match rustix::io::read(&sig.read, &mut buf) {
            Ok(0) => break,
            Ok(n) => signos.extend(buf[..n].iter().map(|&b| b as i32)),
            Err(Errno::INTR) => continue,
            Err(Errno::AGAIN) => break,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(signos)
}

/// Set `O_NONBLOCK` and `FD_CLOEXEC` on `fd` via `fcntl`. Done explicitly (not
/// through `pipe2`) so the path is identical on Linux and macOS.
fn set_cloexec_nonblock(fd: BorrowedFd<'_>) -> io::Result<()> {
    let raw = fd.as_raw_fd();
    unsafe {
        let fl = libc::fcntl(raw, libc::F_GETFL);
        if fl < 0 || libc::fcntl(raw, libc::F_SETFL, fl | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
        let fd_fl = libc::fcntl(raw, libc::F_GETFD);
        if fd_fl < 0 || libc::fcntl(raw, libc::F_SETFD, fd_fl | libc::FD_CLOEXEC) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn nix_io(e: nix::Error) -> io::Error {
    io::Error::from_raw_os_error(e as i32)
}
