//! Durable session descriptors: the host writes what recreating a dead session
//! needs (command, cwd, created time, display name) into the DATA dir — unlike
//! the runtime `meta`, which is pruned with the session directory, this file
//! survives an *unclean* death (logout, reboot, a crash). That is what lets
//! the fleet remember a dead group member and offer to bring it back.
//!
//! An *explicit* end is different: `ghost kill` and the child exiting of its
//! own accord (the user typed `exit`) throw the session away — descriptor and
//! recording both — so nothing offers to resurrect a session its user ended.

use std::path::{Path, PathBuf};
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

fn descriptor_path(xdg: &Path, name: &str) -> PathBuf {
    xdg.join("data")
        .join("ghost")
        .join("sessions")
        .join(format!("{name}.json"))
}

fn read_descriptor(xdg: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(descriptor_path(xdg, name)).ok()
}

fn recording_path(xdg: &Path, name: &str) -> PathBuf {
    xdg.join("data")
        .join("ghost")
        .join("recordings")
        .join(format!("{name}.ghostrec"))
}

/// The session host's pid, from its runtime pidfile.
fn host_pid(xdg: &Path, name: &str) -> Option<String> {
    let p = xdg.join("run").join("ghost").join(name).join("pid");
    Some(std::fs::read_to_string(p).ok()?.trim().to_string())
}

/// Spawn a detached session running `command` and wait until it is listed
/// with its descriptor written (the child is up).
fn spawn_and_settle(xdg: &Path, name: &str, command: &[&str]) {
    let out = ghost(xdg)
        .args(["new", name, "-d", "--"])
        .args(command)
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
    assert!(
        wait_until(Duration::from_secs(5), || read_descriptor(xdg, name)
            .is_some()),
        "no descriptor was written"
    );
}

#[test]
fn a_terminated_host_keeps_the_sessions_durable_traces() {
    // Termination from outside — logout and reboot deliver exactly this
    // SIGTERM — is an unclean death: the user never said goodbye, so the
    // descriptor and recording survive to resurrect the session.
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    // A distinctive launch directory: the descriptor must record where the
    // child actually started, so a recreate can put its successor back there.
    let workdir = xdg.join("projects").join("web");
    std::fs::create_dir_all(&workdir).unwrap();

    let out = ghost(xdg)
        .current_dir(&workdir)
        .args(["new", "durable", "-d", "--", "sh", "-c", "sleep 30"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _guard = KillOnDrop {
        xdg,
        name: "durable",
    };
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains("durable")),
        "session not listed"
    );

    // The descriptor appears once the child is up, carrying its facts.
    assert!(
        wait_until(Duration::from_secs(5), || read_descriptor(xdg, "durable")
            .is_some()),
        "no descriptor was written"
    );
    let desc = read_descriptor(xdg, "durable").unwrap();
    assert!(desc.contains("sh"), "command recorded: {desc}");
    assert!(desc.contains("sleep 30"), "full command recorded: {desc}");
    assert!(
        desc.contains(workdir.to_str().unwrap()),
        "launch cwd recorded: {desc}"
    );
    assert!(
        desc.contains("created_at"),
        "creation time recorded: {desc}"
    );

    let pid = host_pid(xdg, "durable").expect("host pidfile");
    let out = std::process::Command::new("kill")
        .args(["-TERM", &pid])
        .output()
        .unwrap();
    assert!(out.status.success(), "signalling the host failed");
    assert!(
        wait_until(Duration::from_secs(5), || !ls(xdg).contains("durable")),
        "session still listed after its host was terminated"
    );
    assert!(
        read_descriptor(xdg, "durable").is_some(),
        "the descriptor must outlive an unclean death"
    );
    assert!(
        recording_path(xdg, "durable").exists(),
        "the recording must outlive an unclean death"
    );
}

#[test]
fn a_kill_discards_the_sessions_durable_traces() {
    // `ghost kill` is the explicit throw-away verb: the session is not
    // resurrectable afterwards, so its descriptor and recording go with it.
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    spawn_and_settle(xdg, "doomed", &["sh", "-c", "sleep 30"]);
    assert!(
        recording_path(xdg, "doomed").exists(),
        "precondition: the session records"
    );

    let out = ghost(xdg).args(["kill", "doomed"]).output().unwrap();
    assert!(out.status.success());
    assert!(
        wait_until(Duration::from_secs(5), || !ls(xdg).contains("doomed")),
        "session still listed after kill"
    );
    assert!(
        read_descriptor(xdg, "doomed").is_none(),
        "a killed session leaves no descriptor"
    );
    assert!(
        !recording_path(xdg, "doomed").exists(),
        "a killed session leaves no recording"
    );
}

#[test]
fn a_clean_child_exit_discards_the_sessions_durable_traces() {
    // The child exiting of its own accord (the user typed `exit`) ends the
    // session just as explicitly as a kill: nothing to resurrect, so the
    // host removes the descriptor and recording on its way out.
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let flag = xdg.join("goodbye");
    let script = format!(
        "while [ ! -e {} ]; do sleep 0.05; done",
        flag.to_str().unwrap()
    );
    spawn_and_settle(xdg, "gone", &["sh", "-c", &script]);
    let _guard = KillOnDrop { xdg, name: "gone" };
    assert!(
        recording_path(xdg, "gone").exists(),
        "precondition: the session records"
    );

    std::fs::write(&flag, b"").unwrap();
    assert!(
        wait_until(Duration::from_secs(5), || !ls(xdg).contains("gone")),
        "session still listed after its child exited"
    );
    assert!(
        read_descriptor(xdg, "gone").is_none(),
        "a cleanly-exited session leaves no descriptor"
    );
    assert!(
        !recording_path(xdg, "gone").exists(),
        "a cleanly-exited session leaves no recording"
    );
}

#[test]
fn an_explicit_cwd_starts_the_child_there_not_where_the_spawner_ran() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let spawner_dir = xdg.join("spawner");
    let target_dir = xdg.join("target");
    std::fs::create_dir_all(&spawner_dir).unwrap();
    std::fs::create_dir_all(&target_dir).unwrap();

    let out = ghost(xdg)
        .current_dir(&spawner_dir)
        .args(["new", "placed", "-d", "--cwd"])
        .arg(&target_dir)
        .args(["--", "sh", "-c", "sleep 30"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new --cwd` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _guard = KillOnDrop {
        xdg,
        name: "placed",
    };
    assert!(
        wait_until(Duration::from_secs(5), || read_descriptor(xdg, "placed")
            .is_some()),
        "no descriptor was written"
    );
    let desc = read_descriptor(xdg, "placed").unwrap();
    assert!(
        desc.contains(target_dir.to_str().unwrap()),
        "the child starts in the explicit cwd: {desc}"
    );
    assert!(
        !desc.contains(spawner_dir.to_str().unwrap()),
        "the spawner's own directory must not leak in: {desc}"
    );
}

#[test]
fn a_rename_reaches_the_descriptor() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let out = ghost(xdg)
        .args(["new", "plain", "-d", "--", "sh", "-c", "sleep 30"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let _guard = KillOnDrop { xdg, name: "plain" };
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains("plain")),
        "session not listed"
    );
    assert!(
        wait_until(Duration::from_secs(5), || read_descriptor(xdg, "plain")
            .is_some()),
        "no descriptor was written"
    );

    let out = ghost(xdg)
        .args(["rename", "plain", "build-box"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost rename` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wait_until(Duration::from_secs(5), || {
            read_descriptor(xdg, "plain").is_some_and(|d| d.contains("build-box"))
        }),
        "the display name must reach the descriptor: {:?}",
        read_descriptor(xdg, "plain")
    );
}
