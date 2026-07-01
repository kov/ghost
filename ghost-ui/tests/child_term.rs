//! E2E: ghost gives the session child a usable `TERM` (and `COLORTERM`),
//! independent of how the session itself was launched.
//!
//! The child talks to ghost's own `vt` emulator, never the user's outer
//! terminal — and a session can be started from a GUI app (launchd on macOS, a
//! GTK process on Linux) whose environment carries no `TERM` at all. If ghost
//! merely inherited that environment, the shell would see an unset or foreign
//! `TERM` and tools would declare the terminal "not fully functional" and drop
//! colors. So the host sets `TERM`/`COLORTERM` itself to match what its emulator
//! implements. We launch with a deliberately bogus `TERM` and no `COLORTERM` and
//! assert the child sees ghost's values regardless.
//!
//! ghost's emulator implements the kitty feature profile (kitty keyboard
//! protocol both sides, kitty graphics), and apps gate those features on the
//! TERM *name* — Claude Code, notably, only enables its kitty-keyboard /
//! synchronized-output path when TERM says so. So ghost advertises
//! `xterm-kitty` when that terminfo entry exists on the host, falls back to
//! `xterm-256color` when it doesn't, and honors a `GHOST_TERM` override.

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

/// Launch a session whose child dumps TERM/COLORTERM to a sentinel file and
/// return that file's contents.
fn child_env(xdg: &Path, name: &str, extra_env: &[(&str, &str)]) -> String {
    let sentinel = xdg.join(format!("child-env-{name}"));
    let script = format!(
        "printf 'TERM=%s\\nCOLORTERM=%s\\nTERMINFO_DIRS=%s\\n' \
         \"$TERM\" \"$COLORTERM\" \"$TERMINFO_DIRS\" > '{}'; exec sleep 60",
        sentinel.display()
    );
    let mut cmd = ghost(xdg);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let out = cmd
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
    std::fs::read_to_string(&sentinel).unwrap()
}

#[test]
fn child_sees_a_usable_term_regardless_of_launch_environment() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "child-term";
    let _guard = KillOnDrop { xdg, name };

    let env = child_env(xdg, name, &[]);
    // ghost provisions its own xterm-kitty entry (compiling the embedded
    // source with `tic` on first use), so the kitty name is what children see
    // wherever a `tic` exists — which is every supported dev platform. Hosts
    // without `tic` fall back to the system database probe, unit-tested in
    // ghost-vt.
    assert!(
        env.contains("TERM=xterm-kitty"),
        "child did not get ghost's TERM; saw:\n{env}"
    );
    assert!(
        env.contains("COLORTERM=truecolor"),
        "child did not get ghost's COLORTERM; saw:\n{env}"
    );
}

#[test]
fn child_gets_ghost_provisioned_terminfo() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "child-term-provision";
    let _guard = KillOnDrop { xdg, name };

    // A fresh XDG data dir has no terminfo: ghost must compile its embedded
    // xterm-kitty entry into <data>/ghost/terminfo and point the child at it
    // via TERMINFO_DIRS (with a trailing empty entry so the compiled-in
    // system defaults still resolve every other TERM).
    let env = child_env(xdg, name, &[]);
    let provisioned = xdg.join("data").join("ghost").join("terminfo");
    assert!(
        env.lines().any(|l| {
            l.strip_prefix("TERMINFO_DIRS=").is_some_and(|v| {
                v.split(':').any(|d| d == provisioned.to_str().unwrap())
                    && v.split(':').any(str::is_empty)
            })
        }),
        "child's TERMINFO_DIRS does not include ghost's dir plus the default; saw:\n{env}"
    );
    let entry = provisioned.join("x").join("xterm-kitty");
    assert!(
        entry.is_file(),
        "no compiled entry at {}; contents:\n{env}",
        entry.display()
    );
}

#[test]
fn ghost_term_env_overrides_the_advertised_term() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "child-term-override";
    let _guard = KillOnDrop { xdg, name };

    let env = child_env(xdg, name, &[("GHOST_TERM", "screen-256color")]);
    assert!(
        env.contains("TERM=screen-256color"),
        "GHOST_TERM override not honored; saw:\n{env}"
    );
}
