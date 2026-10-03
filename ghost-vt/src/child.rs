//! A session child-process handle that survives an in-place re-exec.
//!
//! During a normal spawn ghost owns a [`std::process::Child`]; a self-upgrade
//! (see `docs/host-self-upgrade.md`) re-execs the host in place, keeping the
//! same pid, so the running child stays *our* direct child and is still
//! reapable by pid alone. This type reaps either way: through the owned handle
//! while we have it, or through a raw `waitpid` when all that crossed the exec
//! is the pid.

use std::io;
use std::time::{Duration, Instant};

/// The outcome of waiting on the child, mirroring the slice of
/// [`std::process::ExitStatus`] the session loop needs. `code()` is `Some` on a
/// normal exit (the user typed `exit`, the command ran to completion) and `None`
/// when a signal ended it (a crash, a logout's SIGHUP). The session's
/// discard-vs-resurrect decision turns on exactly that distinction, so the
/// from-pid path below must decode `waitpid` status to match.
#[derive(Debug, Clone, Copy)]
pub struct ExitStatus {
    code: Option<i32>,
}

impl ExitStatus {
    pub fn code(&self) -> Option<i32> {
        self.code
    }
}

/// A spawned session child, reapable through its owned handle or by pid.
#[derive(Debug)]
pub struct Child {
    pid: u32,
    /// `Some` for a child we spawned and still own; `None` for one adopted by
    /// pid across a re-exec, reaped with a raw `waitpid`.
    handle: Option<std::process::Child>,
}

impl Child {
    /// Wrap a freshly spawned child we own.
    pub fn from_handle(handle: std::process::Child) -> Self {
        Self {
            pid: handle.id(),
            handle: Some(handle),
        }
    }

    /// Adopt a child we no longer hold a handle for — its pid survived an
    /// in-place re-exec. The caller warrants the pid is still our direct child
    /// (true after an `execv` that keeps the pid), so `waitpid` can reap it.
    pub fn from_pid(pid: u32) -> Self {
        Self { pid, handle: None }
    }

    /// The child's pid, for `/proc` cwd reads and signalling.
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// Block until the child exits and reap it.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        match self.handle.as_mut() {
            Some(h) => h.wait().map(|s| ExitStatus { code: s.code() }),
            None => wait_pid(self.pid),
        }
    }

    /// SIGKILL and reap, calling `drain` between polls for the exit, for at most
    /// `limit`. Returns whether the child was reaped.
    ///
    /// The draining is what lets the child finish dying. A killed process still
    /// closes its terminal on the way out, and on macOS the last close of a tty
    /// waits for the tty's output queue to drain — so a child killed while it
    /// was flooding a PTY whose reader is the caller itself never finishes
    /// exiting while the caller sits in `wait4`: the host deadlocks against its
    /// own child. `drain` is the caller emptying the master meanwhile.
    ///
    /// The limit is the backstop: whatever the child does, the caller is never
    /// held past it. An unreaped child is left to whoever inherits it when the
    /// caller exits.
    pub fn kill_draining(&mut self, mut drain: impl FnMut(), limit: Duration) -> bool {
        match self.handle.as_mut() {
            Some(h) => {
                let _ = h.kill();
            }
            None => {
                // SAFETY: `kill(2)` only reads the pid/signal arguments; an
                // ESRCH just means the child is already gone.
                unsafe { libc::kill(self.pid as libc::pid_t, libc::SIGKILL) };
            }
        }
        let deadline = Instant::now() + limit;
        loop {
            if self.try_reap() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            drain();
            std::thread::sleep(REAP_POLL);
        }
    }

    /// Reap the child if it has exited, without blocking. A child that is not
    /// ours to wait for any more (already reaped) counts as reaped.
    fn try_reap(&mut self) -> bool {
        match self.handle.as_mut() {
            Some(h) => !matches!(h.try_wait(), Ok(None)),
            None => {
                let mut status: libc::c_int = 0;
                // SAFETY: `waitpid` writes only through `&mut status`.
                let r =
                    unsafe { libc::waitpid(self.pid as libc::pid_t, &mut status, libc::WNOHANG) };
                r != 0
            }
        }
    }
}

/// How often [`Child::kill_draining`] looks for the exit between drains.
const REAP_POLL: Duration = Duration::from_millis(5);

/// Raw `waitpid` reaping the given pid, decoding the wait status into the
/// `code() == Some` (normal exit) / `None` (signalled) contract [`ExitStatus`]
/// promises.
fn wait_pid(pid: u32) -> io::Result<ExitStatus> {
    let mut status: libc::c_int = 0;
    // SAFETY: `waitpid` writes only through `&mut status`; `pid` is our own
    // child, so it is a valid wait target.
    let r = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, 0) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    let code = if libc::WIFEXITED(status) {
        Some(libc::WEXITSTATUS(status))
    } else {
        None
    };
    Ok(ExitStatus { code })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A child flooding a PTY nobody reads: by the time it is killed the tty's
    /// output queue is full. Returns the master (non-blocking) and the child.
    fn flooding_pty_child() -> (pty_process::blocking::Pty, Child) {
        use std::os::fd::AsFd;
        let (pty, pts) = pty_process::blocking::open().unwrap();
        let fd = pty.as_fd();
        let mut flags = rustix::fs::fcntl_getfl(fd).unwrap();
        flags.set(rustix::fs::OFlags::NONBLOCK, true);
        rustix::fs::fcntl_setfl(fd, flags).unwrap();
        let handle = pty_process::blocking::Command::new("sh")
            .args(["-c", "while :; do echo flood; done"])
            .spawn(pts)
            .unwrap();
        // Long enough for the flood to fill the tty's output queue.
        std::thread::sleep(std::time::Duration::from_millis(300));
        (pty, Child::from_handle(handle))
    }

    fn discard(pty: &pty_process::blocking::Pty) {
        use std::io::Read;
        let mut buf = [0u8; 16 * 1024];
        while matches!((&*pty).read(&mut buf), Ok(n) if n > 0) {}
    }

    /// Killing a child whose PTY output nobody reads must still reap it. On
    /// macOS a dying process's last close of its tty waits for the output
    /// queue to drain, and the only reader is the host that is waiting on it —
    /// a `wait4` there never returns. Draining the master while waiting lets
    /// the exit finish.
    #[test]
    fn a_child_killed_with_its_pty_full_is_reaped_while_draining() {
        let (pty, mut child) = flooding_pty_child();
        let t0 = std::time::Instant::now();
        let reaped = child.kill_draining(|| discard(&pty), std::time::Duration::from_secs(5));
        assert!(reaped, "the child was never reaped");
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(2),
            "{:?}",
            t0.elapsed()
        );
    }

    /// And whatever the child does, the kill gives up at its deadline rather
    /// than blocking the caller forever: a host that cannot reap its child must
    /// still be able to exit.
    #[test]
    fn a_kill_that_cannot_reap_gives_up_at_its_deadline() {
        let (pty, mut child) = flooding_pty_child();
        let limit = std::time::Duration::from_millis(300);
        let t0 = std::time::Instant::now();
        // No draining at all — the case that used to wedge.
        let _ = child.kill_draining(|| {}, limit);
        assert!(
            t0.elapsed() < limit + std::time::Duration::from_secs(1),
            "the kill blocked past its deadline: {:?}",
            t0.elapsed()
        );
        // Let it finish dying, so the test leaves no zombie behind.
        assert!(child.kill_draining(|| discard(&pty), std::time::Duration::from_secs(5)));
    }

    /// The owned-handle path reports a normal exit code straight through
    /// `std::process::ExitStatus`.
    #[test]
    fn owned_handle_reports_a_normal_exit_code() {
        let handle = std::process::Command::new("sh")
            .args(["-c", "exit 7"])
            .spawn()
            .unwrap();
        let mut child = Child::from_handle(handle);
        assert_eq!(child.wait().unwrap().code(), Some(7));
    }

    /// The from-pid path reaps a real child of this process and decodes a
    /// normal exit the same way the owned handle does — this is the contract
    /// the re-exec relies on.
    #[test]
    fn from_pid_reaps_a_normal_exit() {
        let handle = std::process::Command::new("sh")
            .args(["-c", "exit 7"])
            .spawn()
            .unwrap();
        let pid = handle.id();
        // Give up the handle without reaping: `std` never waits on drop, so the
        // child stays reapable by pid. `forget` also avoids closing pipes we
        // never opened racing anything.
        std::mem::forget(handle);
        let mut child = Child::from_pid(pid);
        assert_eq!(child.wait().unwrap().code(), Some(7));
    }

    /// A signalled child yields `code() == None` on the from-pid path, so the
    /// caller keeps the session resurrectable instead of discarding it.
    #[test]
    fn from_pid_reports_a_signalled_exit_as_none() {
        let handle = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = handle.id();
        std::mem::forget(handle);
        // SAFETY: signalling our own live child.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
        let mut child = Child::from_pid(pid);
        assert_eq!(child.wait().unwrap().code(), None);
    }
}
