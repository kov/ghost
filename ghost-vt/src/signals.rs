//! `signalfd` helpers shared by the host and client poll loops.
//!
//! The given signals are blocked (so their default disposition never fires) and
//! delivered instead through a non-blocking `signalfd` that slots into `poll()`.

use nix::sys::signal::{SigSet, SigmaskHow, Signal, pthread_sigmask};
use nix::sys::signalfd::{SfdFlags, SignalFd};
use std::io;

/// Block `signals` and return a `signalfd` that surfaces them in a poll loop.
pub fn make(signals: &[Signal]) -> io::Result<SignalFd> {
    let mut mask = SigSet::empty();
    for &sig in signals {
        mask.add(sig);
    }
    pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&mask), None).map_err(nix_io)?;
    SignalFd::with_flags(&mask, SfdFlags::SFD_NONBLOCK | SfdFlags::SFD_CLOEXEC).map_err(nix_io)
}

/// Drain all currently pending signals, returning their signal numbers.
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
