//! E2E: a detached/never-attached session answers terminal queries itself.
//!
//! While attached, the client (a real terminal or VTE) answers queries like
//! cursor-position. With no client attached, nobody would — so a program that
//! queries on startup and blocks on the reply (a shell) stalls. The host fills
//! that gap, replying from its own screen state while detached.

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
fn detached_session_answers_cursor_position_query() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "cpr-detached";
    let _guard = KillOnDrop { xdg, name };

    // The eager, never-attached child emits a cursor-position query (CSI 6 n) and
    // waits up to 2s for the reply (terminated by `R`). It touches the sentinel
    // only if the reply arrives — and since no client is attached, only the host
    // could have sent it.
    let sentinel = xdg.join("cpr-answered");
    let script = format!(
        "printf '\\033[6n'; if IFS= read -r -s -d R -t 2 _; then touch '{}'; fi; exec sleep 60",
        sentinel.display()
    );
    let out = ghost(xdg)
        .args(["new", name, "-d", "--", "bash", "-c", &script])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new -d` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        wait_until(Duration::from_secs(5), || sentinel.exists()),
        "detached session did not answer the cursor-position query"
    );
}
