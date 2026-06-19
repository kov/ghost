//! E2E: ghost gives the session child a usable `TERM` (and `COLORTERM`),
//! independent of how the session itself was launched.
//!
//! The child talks to ghost's own `vt`/avt emulator, never the user's outer
//! terminal — and a session can be started from a GUI app (launchd on macOS, a
//! GTK process on Linux) whose environment carries no `TERM` at all. If ghost
//! merely inherited that environment, the shell would see an unset or foreign
//! `TERM` and tools would declare the terminal "not fully functional" and drop
//! colors. So the host sets `TERM`/`COLORTERM` itself to match what its emulator
//! implements. We launch with a deliberately bogus `TERM` and no `COLORTERM` and
//! assert the child sees ghost's values regardless.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

const GHOST: &str = env!("CARGO_BIN_EXE_ghost");

fn ghost(xdg: &Path) -> Command {
    let mut c = Command::new(GHOST);
    c.env("XDG_RUNTIME_DIR", xdg.join("run"));
    c.env("XDG_DATA_HOME", xdg.join("data"));
    // The launching environment must NOT decide the child's TERM: pick a bogus
    // value here and drop COLORTERM, so a passing test proves ghost overrides
    // rather than inherits.
    c.env("TERM", "ghost-bogus-launcher-term");
    c.env_remove("COLORTERM");
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
fn child_sees_a_usable_term_regardless_of_launch_environment() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "child-term";
    let _guard = KillOnDrop { xdg, name };

    // The child records its own TERM/COLORTERM to a sentinel file, then idles.
    let sentinel = xdg.join("child-env");
    let script = format!(
        "printf 'TERM=%s\\nCOLORTERM=%s\\n' \"$TERM\" \"$COLORTERM\" > '{}'; exec sleep 60",
        sentinel.display()
    );
    let out = ghost(xdg)
        .args(["new", name, "-d", "--", "sh", "-c", &script])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new -d` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        wait_until(Duration::from_secs(5), || sentinel.exists()),
        "child never wrote its environment"
    );
    let env = std::fs::read_to_string(&sentinel).unwrap();
    assert!(
        env.contains("TERM=xterm-256color"),
        "child did not get ghost's TERM; saw:\n{env}"
    );
    assert!(
        env.contains("COLORTERM=truecolor"),
        "child did not get ghost's COLORTERM; saw:\n{env}"
    );
}
