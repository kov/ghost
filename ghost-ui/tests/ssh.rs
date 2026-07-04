//! `ghost ssh`: a session whose child is `ssh <target>`. Driven end-to-end with
//! a fake `ssh` first on `PATH` — a shim that echoes its argv (so the derived
//! command is assertable on screen) then execs a shell (so the session lives,
//! like ssh dropping into a remote shell). Proves the connection spec reaches
//! the child as the right command line and is recorded in the descriptor.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use ghost_vt::client::Session;
use ghost_vt::descriptor::Descriptor;
use ghost_vt::screen::Screen;

const GHOST: &str = env!("CARGO_BIN_EXE_ghost");

/// A directory holding a fake `ssh` that prints `SSH-SHIM #N: <args>` (N counts
/// invocations via `$SSH_SHIM_COUNT`, so a reconnect is distinguishable from the
/// original) then execs a shell, so a `ghost ssh` session runs it instead of the
/// real ssh.
fn shim_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let ssh = dir.path().join("ssh");
    std::fs::write(
        &ssh,
        "#!/bin/sh\n\
         n=$(cat \"$SSH_SHIM_COUNT\" 2>/dev/null || echo 0); n=$((n+1))\n\
         echo \"$n\" > \"$SSH_SHIM_COUNT\"\n\
         echo \"SSH-SHIM #$n: $*\"\n\
         exec sh\n",
    )
    .unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

fn ghost(xdg: &Path, shim: &Path) -> Command {
    let mut c = Command::new(GHOST);
    c.env("XDG_RUNTIME_DIR", xdg.join("run"));
    c.env("XDG_DATA_HOME", xdg.join("data"));
    // The shim's directory first, so the session's child finds our fake `ssh`;
    // the real PATH stays appended so the shim's `exec sh` still resolves.
    let path = std::env::var("PATH").unwrap_or_default();
    c.env("PATH", format!("{}:{path}", shim.display()));
    // Per-workspace invocation counter file, so the shim can number its runs.
    c.env("SSH_SHIM_COUNT", xdg.join("shim-count"));
    c
}

fn recording_path(xdg: &Path, name: &str) -> PathBuf {
    xdg.join("data")
        .join("ghost")
        .join("recordings")
        .join(format!("{name}.ghostrec"))
}

/// End the session the way a logout does — SIGTERM to its host. An unclean death
/// leaves the recording in place to seed the reconnect (a `ghost kill` would
/// discard it).
fn terminate_host(xdg: &Path, name: &str) {
    let pidfile = xdg.join("run").join("ghost").join(name).join("pid");
    let pid = std::fs::read_to_string(pidfile).expect("host pidfile");
    let out = Command::new("kill")
        .args(["-TERM", pid.trim()])
        .output()
        .unwrap();
    assert!(out.status.success(), "signalling the host failed");
}

fn ls(xdg: &Path, shim: &Path) -> String {
    let out = ghost(xdg, shim).arg("ls").output().expect("run `ghost ls`");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn sock(xdg: &Path, name: &str) -> PathBuf {
    xdg.join("run").join("ghost").join(name).join("sock")
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
    shim: &'a Path,
    name: &'a str,
}

impl Drop for KillOnDrop<'_> {
    fn drop(&mut self) {
        let _ = ghost(self.xdg, self.shim)
            .args(["kill", self.name])
            .output();
    }
}

#[test]
fn ghost_ssh_runs_the_derived_command_and_records_the_connection() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let shim = shim_dir();
    let shim = shim.path();

    // Start detached so the CLI returns without needing a PTY around it; the
    // child (our shim) starts eagerly and prints its marker.
    let out = ghost(xdg, shim)
        .args(["ssh", "dev@example", "-p", "2222", "--name", "work", "-d"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost ssh` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let _guard = KillOnDrop {
        xdg,
        shim,
        name: "work",
    };
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg, shim).contains("work")),
        "ssh session not listed"
    );

    // Attach and watch the screen: the shim echoes exactly the argv the spec
    // derived — `ssh -p 2222 dev@example` — proving the connection reached the
    // child as the right command line.
    let mut session = Session::attach_path(&sock(xdg, "work"), "work", 80, 24).expect("attach");
    session
        .set_read_timeout(Some(Duration::from_millis(25)))
        .unwrap();
    let mut screen = Screen::new(80, 24, 100);
    assert!(
        wait_until(Duration::from_secs(5), || {
            if let Ok(p) = session.pump() {
                screen.feed(&p.output);
            }
            screen
                .text()
                .join("\n")
                .contains("SSH-SHIM #1: -p 2222 dev@example")
        }),
        "the derived ssh command never showed; saw:\n{}",
        screen.text().join("\n")
    );

    // The durable descriptor records the connection (not a memorized command):
    // an empty command plus the spec, so a relaunch can reconnect (Phase 3).
    let desc_path = xdg
        .join("data")
        .join("ghost")
        .join("sessions")
        .join("work.json");
    let d: Descriptor =
        serde_json::from_slice(&std::fs::read(&desc_path).expect("descriptor written")).unwrap();
    assert!(d.command.is_empty(), "an ssh session stores no command");
    let spec = d.connection.expect("the descriptor carries the connection");
    assert_eq!(spec.target(), "dev@example");
    assert_eq!(spec.port, Some(2222));
}

#[test]
fn a_dead_ssh_session_reconnects_seeded_from_its_recording() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let shim = shim_dir();
    let shim = shim.path();

    // First connection (detached → eager child): the shim prints "#1".
    let out = ghost(xdg, shim)
        .args(["ssh", "example", "--name", "work", "-d"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost ssh` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg, shim).contains("work")),
        "ssh session not listed"
    );
    // Wait until #1 reaches the host, so the recording captures it before death.
    let mut first = Session::attach_path(&sock(xdg, "work"), "work", 80, 24).expect("attach first");
    first
        .set_read_timeout(Some(Duration::from_millis(25)))
        .unwrap();
    let mut fscreen = Screen::new(80, 24, 100);
    assert!(
        wait_until(Duration::from_secs(5), || {
            if let Ok(p) = first.pump() {
                fscreen.feed(&p.output);
            }
            fscreen.text().join("\n").contains("SSH-SHIM #1: example")
        }),
        "the first connection never showed; saw:\n{}",
        fscreen.text().join("\n")
    );
    drop(first); // detach; the session lives on

    // Unclean death (a logout's SIGTERM): the recording survives to seed the
    // reconnect.
    terminate_host(xdg, "work");
    assert!(
        wait_until(Duration::from_secs(5), || !ls(xdg, shim).contains("work")),
        "session still listed after its host was terminated"
    );
    let rec = recording_path(xdg, "work");
    assert!(rec.exists(), "the recording survives the death");

    // Reconnect: the exact shape `spawn_dead` produces — empty command, the
    // connection carried forward, seeded from the recording (via the hidden
    // `--seed-from`, as the GUI's in-process relaunch does).
    let out = ghost(xdg, shim)
        .args(["ssh", "example", "--name", "work", "--seed-from"])
        .arg(&rec)
        .arg("-d")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "reconnect `ghost ssh` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _guard = KillOnDrop {
        xdg,
        shim,
        name: "work",
    };
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg, shim).contains("work")),
        "reconnected session not listed"
    );

    // Attach: the seed shows the OLD connection (#1), and the reconnect's fresh
    // login (#2) lands below it — history preserved, connection re-established.
    let mut session = Session::attach_path(&sock(xdg, "work"), "work", 80, 24).expect("attach");
    session
        .set_read_timeout(Some(Duration::from_millis(25)))
        .unwrap();
    let mut screen = Screen::new(80, 24, 100);
    assert!(
        wait_until(Duration::from_secs(5), || {
            if let Ok(p) = session.pump() {
                screen.feed(&p.output);
            }
            let text = screen.text().join("\n");
            text.contains("SSH-SHIM #1: example") && text.contains("SSH-SHIM #2: example")
        }),
        "the reconnect shows the old history and a fresh login; saw:\n{}",
        screen.text().join("\n")
    );
    let text = screen.text().join("\n");
    assert!(
        text.find("SSH-SHIM #1").unwrap() < text.find("SSH-SHIM #2").unwrap(),
        "the old connection's history sits above the fresh reconnect:\n{text}"
    );
}
