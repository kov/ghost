//! E2E test: a session's child inherits the umask ghost was launched with.
//!
//! The host is daemonized with the textbook dance — `setsid`, double fork,
//! `chdir("/")`, clear the umask — but a session host is not an ordinary daemon:
//! it goes on to spawn the *user's shell*. A cleared umask is therefore inherited
//! by every process in every session, and everything created from a ghost
//! terminal comes out `0666`/`0777`: the user's own builds, and ghost's own
//! recordings and session files.
//!
//! It also defeats ghost's own exec-safety guard, which refuses to upgrade a host
//! into a group- or other-writable binary — that binary being world-writable
//! precisely because ghost had cleared the umask of the shell that built it.
//!
//! The `chdir` is left alone: it keeps a long-lived background host from pinning
//! the directory it was launched in, and it cannot leak the same way, because a
//! child's working directory is always passed explicitly (`launch_dir`).

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

const GHOST: &str = env!("CARGO_BIN_EXE_ghost");

fn ghost(xdg: &Path) -> Command {
    let mut c = Command::new(GHOST);
    c.env("XDG_RUNTIME_DIR", xdg.join("run"));
    c.env("XDG_DATA_HOME", xdg.join("data"));
    c
}

fn wait_until(timeout: Duration, mut pred: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    loop {
        if pred() {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Kills a session on drop so a failed test never leaks a daemon.
struct KillOnDrop<'a> {
    xdg: &'a Path,
    name: &'a str,
}

impl Drop for KillOnDrop<'_> {
    fn drop(&mut self) {
        let _ = ghost(self.xdg).args(["kill", self.name]).output();
    }
}

#[test]
fn a_sessions_child_inherits_the_umask_ghost_was_launched_with() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let work = xdg.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let name = "umask-1";
    let _guard = KillOnDrop { xdg, name };

    // Launch ghost under a umask nobody reaches by accident — and set it in the
    // spawned shell, not in this process: the umask is process-global, so a test
    // that changed its own would change every other test's in this binary.
    // `exec "$@"` keeps the arguments out of the quoting.
    let out = Command::new("sh")
        .args(["-c", "umask 027; exec \"$@\"", "sh", GHOST])
        .args(["new", name, "-d", "--"])
        .args(["sh", "-c", "umask > reported; : > made; exec cat"])
        .env("XDG_RUNTIME_DIR", xdg.join("run"))
        .env("XDG_DATA_HOME", xdg.join("data"))
        .current_dir(&work)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wait_until(Duration::from_secs(5), || work.join("made").exists()),
        "the child never ran"
    );

    // What the user feels: a file the child creates carries the umask they were
    // working under — 0666 & ~0027 — not the daemon's cleared one (0666).
    let mode = std::fs::metadata(work.join("made"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(
        format!("{mode:04o}"),
        "0640",
        "a file made by the session's child is world-writable: the host cleared the umask"
    );
    assert_eq!(
        std::fs::read_to_string(work.join("reported"))
            .unwrap()
            .trim(),
        "0027",
        "the child's own umask is the one ghost was launched with"
    );
}
