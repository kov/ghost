//! E2E test that the session host is a *re-exec'd* process, not merely a fork of
//! the launching process.
//!
//! `server::spawn` daemonizes (double-fork) and then `execv`s the `ghost` binary
//! into a hidden `__host` mode, so the long-lived host runs in a fresh,
//! single-threaded address space — which is what makes it safe to start a
//! session from a multithreaded process (e.g. a GTK GUI) and drops any inherited
//! heap/fds. We verify the re-exec happened by reading the host's own argv from
//! `/proc/<pid>/cmdline`: a forked-only host would carry `new …`, while a
//! re-exec'd host carries the internal `__host` argument.

#![cfg(target_os = "linux")]

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

fn ls(xdg: &Path) -> String {
    let out = ghost(xdg).arg("ls").output().expect("run `ghost ls`");
    String::from_utf8_lossy(&out.stdout).into_owned()
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
fn session_host_runs_as_a_reexeced_host_process() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "reexec";
    let _guard = KillOnDrop { xdg, name };

    let out = ghost(xdg)
        .args(["new", name, "-d", "--", "sleep", "600"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains(name)),
        "session not listed"
    );

    // The host writes its own pid into the session dir.
    let pid_path = xdg.join("run").join("ghost").join(name).join("pid");
    assert!(
        wait_until(Duration::from_secs(5), || pid_path.exists()),
        "host never wrote a pidfile"
    );
    let pid: i32 = std::fs::read_to_string(&pid_path)
        .unwrap()
        .trim()
        .parse()
        .expect("pidfile holds a pid");

    // The host's argv: a re-exec'd host carries the internal `__host` argument.
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).expect("read host cmdline");
    let args: Vec<String> = cmdline
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect();
    assert!(
        args.iter().any(|a| a == "__host"),
        "session host is not a re-exec'd `__host` process; argv: {args:?}"
    );
}
