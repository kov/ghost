//! E2E test that a session host escapes the launching app's systemd scope.
//!
//! A host is already daemonized (double-fork + `setsid`, reparented to init),
//! but on a systemd desktop that buys it nothing: it is born inside the
//! launcher's `app-*.scope`, and those scopes are `PartOf=graphical-session.target`,
//! so logging out stops the target, stops the scope, and SIGTERMs every process
//! in it — parentage and session leadership are irrelevant to a cgroup teardown.
//!
//! So the host moves *itself* into its own transient scope under
//! `background.slice`, which nothing binds to the graphical session. We verify
//! the property that actually matters — the host's unit is not tied to the
//! graphical session — by reading `/proc/<pid>/cgroup` and asking the user
//! manager what the resulting unit is `PartOf`.

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

/// The leaf cgroup name for a pid: the last path segment of its cgroup v2 line,
/// which for a systemd-managed process is its unit name.
fn leaf_cgroup(pid: i32) -> String {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).expect("read cgroup");
    raw.trim()
        .rsplit('/')
        .next()
        .expect("cgroup path has a leaf")
        .to_string()
}

/// Is there a systemd user manager we can talk to? Without one (containers, CI,
/// a non-systemd distro) there is no scope to escape into and nothing to assert.
fn have_user_manager() -> bool {
    std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some()
        && Command::new("busctl")
            .args(["--user", "--no-pager", "status"])
            .output()
            .is_ok_and(|o| o.status.success())
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
fn session_host_lives_outside_the_graphical_session() {
    if !have_user_manager() {
        eprintln!("skipping: no systemd user manager on this machine");
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "scope";
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

    // The move into a fresh scope is asynchronous — the user manager queues a job
    // for it — so give it a moment to land rather than reading a stale cgroup.
    let ours = leaf_cgroup(std::process::id() as i32);
    assert!(
        wait_until(Duration::from_secs(5), || leaf_cgroup(pid) != ours),
        "host stayed in the launching process's cgroup ({ours}) — a logout would kill it"
    );

    let unit = leaf_cgroup(pid);
    assert!(
        unit.ends_with(".scope"),
        "host is not in a scope unit of its own: {unit}"
    );

    // The property that matters: nothing ties the host's unit to the graphical
    // session, so ending that session cannot take the host down with it.
    let out = Command::new("systemctl")
        .args(["--user", "show", &unit, "-p", "PartOf", "-p", "Slice"])
        .output()
        .expect("run systemctl --user show");
    let props = String::from_utf8_lossy(&out.stdout);
    let part_of = props
        .lines()
        .find_map(|l| l.strip_prefix("PartOf="))
        .unwrap_or_default();
    assert!(
        part_of.is_empty(),
        "host's scope {unit} is PartOf={part_of} — a logout would stop it"
    );
    assert!(
        props.contains("Slice=background.slice"),
        "host's scope {unit} is not in background.slice: {props}"
    );
}
