//! Durable session descriptors: the host writes what recreating a dead session
//! needs (command, cwd, created time, display name) into the DATA dir — unlike
//! the runtime `meta`, which is pruned with the session directory, this file
//! survives the session's death. That is what lets the fleet remember a dead
//! group member and offer to bring it back.

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

#[test]
fn a_sessions_descriptor_survives_its_death() {
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

    // Kill the session: the runtime directory (and its `meta`) is pruned, but
    // the descriptor is the durable memory — it must survive.
    let out = ghost(xdg).args(["kill", "durable"]).output().unwrap();
    assert!(out.status.success());
    assert!(
        wait_until(Duration::from_secs(5), || !ls(xdg).contains("durable")),
        "session still listed after kill"
    );
    assert!(
        read_descriptor(xdg, "durable").is_some(),
        "the descriptor must outlive the session"
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
