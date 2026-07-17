//! A session child-process handle that survives an in-place re-exec.
//!
//! During a normal spawn ghost owns a [`std::process::Child`]; a self-upgrade
//! (see `docs/host-self-upgrade.md`) re-execs the host in place, keeping the
//! same pid, so the running child stays *our* direct child and is still
//! reapable by pid alone. This type reaps either way: through the owned handle
//! while we have it, or through a raw `waitpid` when all that crossed the exec
//! is the pid.

use std::io;

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
    #[allow(dead_code)] // First caller lands with Phase 2 Step 3 (child adoption).
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

    /// SIGKILL and reap. Best-effort, matching `std::process::Child::kill`
    /// followed by `wait`.
    pub fn kill(&mut self) {
        match self.handle.as_mut() {
            Some(h) => {
                let _ = h.kill();
                let _ = h.wait();
            }
            None => {
                // SAFETY: `kill(2)` only reads the pid/signal arguments; an
                // ESRCH just means the child is already gone.
                unsafe { libc::kill(self.pid as libc::pid_t, libc::SIGKILL) };
                let _ = wait_pid(self.pid);
            }
        }
    }
}

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
