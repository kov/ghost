//! In-place self-upgrade: a running host re-execs itself under a (possibly
//! newer) binary **without killing its child**. Unlike `__restart` — which
//! SIGTERMs the host and respawns it seeded from the recording, losing the
//! running program — a self-upgrade keeps the same pid, the same PTY, and the
//! same live child: only the host's code image is replaced.
//!
//! This is the Phase 2 mechanism that lets a remote host predating a staged
//! binary adopt the new protocol level while a long-lived program keeps running
//! underneath. `ghost __upgrade <name>` triggers it; the child is adopted by pid
//! across the exec (see `ghost_vt::child`).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use ghost_vt::client::{Session, Subscriber};
use ghost_vt::protocol::SessionEvent;
use ghost_vt::screen::Screen;

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

fn sock(xdg: &Path, name: &str) -> PathBuf {
    xdg.join("run").join("ghost").join(name).join("sock")
}

fn host_pid(xdg: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(xdg.join("run").join("ghost").join(name).join("pid"))
        .ok()
        .map(|s| s.trim().to_string())
}

/// True while the pid names a live process we can signal (same UID in these
/// tests). Uses `kill -0`, which succeeds iff the process exists.
fn alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Attach and pump until `needle` renders (or a short deadline passes).
fn wait_for_screen(session: &mut Session, screen: &mut Screen, needle: &str) -> bool {
    wait_until(Duration::from_secs(5), || {
        if let Ok(p) = session.pump() {
            screen.feed(&p.output);
        }
        screen.text().join("\n").contains(needle)
    })
}

/// Observe a session (read-only — never resizes its PTY) and return the grid it
/// reports plus its display name, so a test can read the post-upgrade geometry
/// and identity without an attach's resize perturbing them.
fn observe_grid_and_name(xdg: &Path, name: &str) -> (Option<(u16, u16)>, String) {
    let mut sub = Subscriber::observe_path(&sock(xdg, name)).expect("observe session");
    let mut grid = None;
    let mut display = String::new();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) && grid.is_none() {
        let p = sub.pump().unwrap_or_default();
        if let Some(state) = p.snapshot {
            display = state.display_name;
        }
        for e in p.events {
            if let SessionEvent::Resized { cols, rows } = e {
                grid = Some((cols, rows));
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    (grid, display)
}

/// An upgrade must keep the session the SAME session: the child's terminal
/// geometry (the old code reverted it to the stale spawn-time `opts.size`) and
/// the durable identity — the rename label, the creation time — must survive.
#[test]
fn a_self_upgrade_preserves_terminal_geometry_and_session_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();

    // Spawn at the default 80x24.
    let out = ghost(xdg)
        .args(["new", "geo", "-d", "--", "sh", "-c", "echo READY; exec cat"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _guard = KillOnDrop { xdg, name: "geo" };
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains("geo")),
        "session not listed"
    );

    // Attach at a DIFFERENT size (resizing the PTY + host screen to 120x40), then
    // detach — the child is now on a 120x40 terminal, not the spawn-time 80x24.
    {
        let mut s =
            Session::attach_path(&sock(xdg, "geo"), "geo", 120, 40).expect("attach at 120x40");
        s.set_read_timeout(Some(Duration::from_millis(25))).unwrap();
        let mut screen = Screen::new(120, 40, 100);
        assert!(
            wait_for_screen(&mut s, &mut screen, "READY"),
            "child output never arrived; saw:\n{}",
            screen.text().join("\n")
        );
    }
    // Give it a durable label distinct from the id.
    let out = ghost(xdg)
        .args(["rename", "geo", "My Session"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost rename` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Upgrade in place.
    let out = ghost(xdg).args(["__upgrade", "geo"]).output().unwrap();
    assert!(
        out.status.success(),
        "`ghost __upgrade` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Observe (no resize): the new host reports the live 120x40 grid and keeps
    // the rename label — not the spawn-time 80x24 / empty label a fresh-meta,
    // stale-size adopt would show.
    let (grid, name) = observe_grid_and_name(xdg, "geo");
    assert_eq!(
        grid,
        Some((120, 40)),
        "the terminal geometry reverted across the upgrade"
    );
    assert_eq!(
        name, "My Session",
        "the session's rename label was lost across the upgrade"
    );
}

/// The pre-exec probe must REFUSE a target that can't speak our handoff format
/// (here `/bin/true`, which has no `__handoff` subcommand) rather than exec into
/// it — an exec into an incompatible binary would misdecode the handoff and kill
/// the child. A refusal leaves the running host and its child untouched.
#[test]
fn a_self_upgrade_refuses_a_target_that_cannot_speak_the_handoff_format() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();

    let out = ghost(xdg)
        .args([
            "new",
            "refuseup",
            "-d",
            "--",
            "sh",
            "-c",
            "echo CHILD=$$; exec cat",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _guard = KillOnDrop {
        xdg,
        name: "refuseup",
    };
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains("refuseup")),
        "session not listed"
    );

    let child_pid;
    {
        let mut s = Session::attach_path(&sock(xdg, "refuseup"), "refuseup", 80, 24)
            .expect("attach before refused upgrade");
        s.set_read_timeout(Some(Duration::from_millis(25))).unwrap();
        let mut screen = Screen::new(80, 24, 100);
        assert!(
            wait_for_screen(&mut s, &mut screen, "CHILD="),
            "child pid never printed; saw:\n{}",
            screen.text().join("\n")
        );
        let text = screen.text().join("\n");
        child_pid = text
            .split("CHILD=")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .expect("CHILD=<pid> on screen")
            .to_string();
    }
    let before_host = host_pid(xdg, "refuseup").expect("host pid before refused upgrade");

    // `/bin/true` exits 0 with no output, so the handoff-version probe finds
    // nothing to parse and the host refuses. (Delivery still succeeds — the
    // request reached the host, which declined it internally.)
    let out = ghost(xdg)
        .args(["__upgrade", "refuseup", "/bin/true"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost __upgrade` delivery failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The host never exec'd: same pid, child still alive, still interactive.
    assert_eq!(
        host_pid(xdg, "refuseup").as_deref(),
        Some(before_host.as_str()),
        "host pid changed — a refused upgrade must not exec"
    );
    assert!(
        alive(&child_pid),
        "the child (pid {child_pid}) died — a refused upgrade must leave it running"
    );
    let mut s = Session::attach_path(&sock(xdg, "refuseup"), "refuseup", 80, 24)
        .expect("attach after refused upgrade");
    s.set_read_timeout(Some(Duration::from_millis(25))).unwrap();
    s.send_input(b"STILL-ALIVE\n").unwrap();
    let mut screen = Screen::new(80, 24, 100);
    assert!(
        wait_for_screen(&mut s, &mut screen, "STILL-ALIVE"),
        "the child stopped echoing after a refused upgrade; saw:\n{}",
        screen.text().join("\n")
    );
}

#[test]
fn a_self_upgrade_replaces_the_host_in_place_and_keeps_its_child_alive() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();

    // A live session whose child prints its own pid, then becomes `cat` (via
    // `exec`, so `cat` inherits that pid). The printed pid is thus the running
    // child's pid, and `cat` keeps the session interactive so we can prove the
    // SAME child is still there after the upgrade by round-tripping input.
    let out = ghost(xdg)
        .args([
            "new",
            "upgrademe",
            "-d",
            "--",
            "sh",
            "-c",
            "echo CHILD=$$; exec cat",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _guard = KillOnDrop {
        xdg,
        name: "upgrademe",
    };
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains("upgrademe")),
        "session not listed"
    );

    // Attach, read the child's pid off the screen, note the host pid.
    let child_pid;
    {
        let mut s = Session::attach_path(&sock(xdg, "upgrademe"), "upgrademe", 80, 24)
            .expect("attach before upgrade");
        s.set_read_timeout(Some(Duration::from_millis(25))).unwrap();
        let mut screen = Screen::new(80, 24, 100);
        assert!(
            wait_for_screen(&mut s, &mut screen, "CHILD="),
            "child pid never printed; saw:\n{}",
            screen.text().join("\n")
        );
        let text = screen.text().join("\n");
        child_pid = text
            .split("CHILD=")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .expect("CHILD=<pid> on screen")
            .to_string();
    }
    let before_host = host_pid(xdg, "upgrademe").expect("host pid before upgrade");
    assert!(alive(&child_pid), "child not alive before upgrade");

    // Upgrade the host in place (to itself — a newer binary is unnecessary to
    // prove the mechanism).
    let out = ghost(xdg)
        .args(["__upgrade", "upgrademe"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost __upgrade` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The host kept its pid (an in-place execv, not a respawn)…
    assert!(
        wait_until(Duration::from_secs(5), || {
            ls(xdg).contains("upgrademe")
                && host_pid(xdg, "upgrademe").as_deref() == Some(before_host.as_str())
        }),
        "host pid changed or session vanished across the upgrade (before={before_host}, \
         after={:?})",
        host_pid(xdg, "upgrademe")
    );

    // …and the SAME child is still running on the same PTY: its pid is still
    // alive, and it still echoes what we type (proving `cat` was adopted, not
    // restarted).
    assert!(
        alive(&child_pid),
        "the child (pid {child_pid}) did not survive the in-place upgrade"
    );
    let mut s = Session::attach_path(&sock(xdg, "upgrademe"), "upgrademe", 80, 24)
        .expect("attach after upgrade");
    s.set_read_timeout(Some(Duration::from_millis(25))).unwrap();
    s.send_input(b"PONG-AFTER-UPGRADE\n").unwrap();
    let mut screen = Screen::new(80, 24, 100);
    assert!(
        wait_for_screen(&mut s, &mut screen, "PONG-AFTER-UPGRADE"),
        "the adopted child did not echo input after the upgrade; saw:\n{}",
        screen.text().join("\n")
    );
}
