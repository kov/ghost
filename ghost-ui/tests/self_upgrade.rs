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
use ghost_vt::protocol::{PROTO_LEVEL, SessionEvent};
use ghost_vt::screen::Screen;
use ghost_vt::server::HANDOFF_VERSION;

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

/// Send signal `sig` (e.g. "TERM", "STOP", "CONT") to `pid` via `kill(1)`.
fn signal(pid: &str, sig: &str) {
    let _ = Command::new("kill")
        .args([format!("-{sig}").as_str(), pid])
        .status();
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

/// Spawn a `cat` session, point `__upgrade` at `target`, and assert the host
/// REFUSED it: `ghost __upgrade` exits non-zero with a message containing
/// `expect` (the host's result channel surfaced the reason, not a silent
/// timeout), the host kept its pid (no exec), the child is still alive, and the
/// host is responsive again (it echoes fresh input). Used by the
/// target-validation refusal tests, which each hand a differently-untrustworthy
/// target and expect the same "declined, still serving, reported why" outcome.
fn assert_target_refused(name: &str, target: &Path, expect: &str) {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();

    let out = ghost(xdg)
        .args([
            "new",
            name,
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
    let _guard = KillOnDrop { xdg, name };
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains(name)),
        "session not listed"
    );

    let child_pid;
    {
        let mut s = Session::attach_path(&sock(xdg, name), name, 80, 24)
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
    let before_host = host_pid(xdg, name).expect("host pid before refused upgrade");

    let out = ghost(xdg)
        .args(["__upgrade", name, target.to_str().unwrap()])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "`ghost __upgrade` should have reported the refusal, but it succeeded"
    );
    assert!(
        stderr.contains(expect),
        "refusal message did not mention {expect:?}; saw:\n{stderr}"
    );

    assert_eq!(
        host_pid(xdg, name).as_deref(),
        Some(before_host.as_str()),
        "host pid changed — a refused upgrade must not exec"
    );
    assert!(
        alive(&child_pid),
        "the child (pid {child_pid}) died — a refused upgrade must leave it running"
    );
    let mut s =
        Session::attach_path(&sock(xdg, name), name, 80, 24).expect("attach after refused upgrade");
    s.set_read_timeout(Some(Duration::from_millis(25))).unwrap();
    s.send_input(b"STILL-ALIVE\n").unwrap();
    let mut screen = Screen::new(80, 24, 100);
    assert!(
        wait_for_screen(&mut s, &mut screen, "STILL-ALIVE"),
        "the child stopped echoing after a refused upgrade; saw:\n{}",
        screen.text().join("\n")
    );
}

/// Write `body` to `dir/fake-ghost`, mark it executable-plus-`extra` mode bits,
/// and return its path — a stand-in `ghost` whose `__handoff` output the test
/// controls.
fn write_fake_ghost(dir: &Path, body: &str, mode: u32) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let target = dir.join("fake-ghost");
    std::fs::write(&target, body).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode)).unwrap();
    target
}

/// The probe RUNS the target and the exec hands it our process, so a target
/// anyone else can rewrite must be refused before either — even when it reports
/// a perfectly valid handoff. Here a world-writable script that answers the
/// probe with our own `<handoff> <proto>` is still declined on its mode.
///
/// The script also `touch`es a sentinel: because validation runs BEFORE the
/// probe, the target must never be executed, so the sentinel must not appear.
/// That pins the vet-before-probe ordering — reorder the check and the probe
/// would run this script (creating the sentinel) even though it is ultimately
/// refused on mode.
#[test]
fn a_self_upgrade_refuses_a_world_writable_target() {
    let tmp = tempfile::tempdir().unwrap();
    let sentinel = tmp.path().join("EXECUTED");
    // Reports a valid handoff+proto (so only the mode can disqualify it), but is
    // writable by everyone — and records if it is ever run.
    let body = format!(
        "#!/bin/sh\ntouch '{}'\necho {} {}\n",
        sentinel.display(),
        HANDOFF_VERSION,
        PROTO_LEVEL
    );
    let target = write_fake_ghost(tmp.path(), &body, 0o777);
    assert_target_refused("wwup", &target, "writable by group or other");
    assert!(
        !sentinel.exists(),
        "the untrusted target was executed — validation must precede the probe"
    );
}

/// A self-upgrade must not silently DOWNGRADE the session: a target that speaks
/// a lower protocol level than we serve is refused (rolling back is `__restart`
/// territory, not an in-place adopt).
#[test]
fn a_self_upgrade_refuses_a_protocol_downgrade() {
    let tmp = tempfile::tempdir().unwrap();
    // Valid handoff, but one proto level below ours.
    let body = format!("#!/bin/sh\necho {} {}\n", HANDOFF_VERSION, PROTO_LEVEL - 1);
    let target = write_fake_ghost(tmp.path(), &body, 0o755);
    assert_target_refused("downup", &target, "downgrade");
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
/// the child. A refusal leaves the running host and its child untouched, and the
/// host reports why (`/bin/true` answers the probe with no version to parse).
#[test]
fn a_self_upgrade_refuses_a_target_that_cannot_speak_the_handoff_format() {
    assert_target_refused("refuseup", Path::new("/bin/true"), "handoff version");
}

/// The recording must CONTINUE across an upgrade, not restart: the successor
/// appends to the existing file instead of truncating it, so a marker recorded
/// before the upgrade is still there afterward. (`ghost search` replays the
/// recording through the emulator and greps the rendered lines.)
#[test]
fn a_self_upgrade_continues_the_recording_instead_of_truncating_it() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();

    // Recording is on by default; the child prints a distinctive marker, then
    // becomes `cat` so the session stays live for the upgrade.
    let out = ghost(xdg)
        .args([
            "new",
            "recup",
            "-d",
            "--",
            "sh",
            "-c",
            "echo BEFORE-UPGRADE-MARK; exec cat",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let _guard = KillOnDrop { xdg, name: "recup" };

    // Attach and confirm the marker rendered, so the host has recorded it.
    {
        let mut s = Session::attach_path(&sock(xdg, "recup"), "recup", 80, 24)
            .expect("attach before upgrade");
        s.set_read_timeout(Some(Duration::from_millis(25))).unwrap();
        let mut screen = Screen::new(80, 24, 100);
        assert!(
            wait_for_screen(&mut s, &mut screen, "BEFORE-UPGRADE-MARK"),
            "marker never rendered; saw:\n{}",
            screen.text().join("\n")
        );
    }

    // Upgrade in place.
    let out = ghost(xdg).args(["__upgrade", "recup"]).output().unwrap();
    assert!(
        out.status.success(),
        "`ghost __upgrade` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The pre-upgrade marker is STILL searchable: the successor appended to the
    // recording rather than truncating it. A `create`-truncating adopt would
    // have wiped this.
    assert!(
        wait_until(Duration::from_secs(5), || {
            let out = ghost(xdg).args(["search", "BEFORE-UPGRADE-MARK"]).output();
            out.map(|o| String::from_utf8_lossy(&o.stdout).contains("BEFORE-UPGRADE-MARK"))
                .unwrap_or(false)
        }),
        "the pre-upgrade recording was truncated across the upgrade"
    );
}

/// A target that HANGS on the handoff-version probe (a wedged wrapper, a broken
/// binary, a wrong path that happens to be an executable that never exits) must
/// not wedge the host with it. The probe is spawned under a timeout: it is
/// killed at the deadline and the upgrade is refused, so the host — which is
/// blocked in the probe while it runs — comes back and keeps serving its child.
/// Without the timeout the probe's `.output()` would block the host loop for as
/// long as the target runs (here effectively forever), and the child would go
/// dark.
#[test]
fn a_self_upgrade_refuses_a_target_that_hangs_on_the_probe() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();

    // A stand-in "ghost" that ignores its argv and just blocks for far longer
    // than the probe's deadline. `exec` so the script process *is* the sleep,
    // and the probe's kill takes the whole thing down (no orphaned sleep).
    let hang = xdg.join("hang.sh");
    std::fs::write(&hang, "#!/bin/sh\nexec sleep 600\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hang, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let out = ghost(xdg)
        .args([
            "new",
            "hangup",
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
        name: "hangup",
    };
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains("hangup")),
        "session not listed"
    );

    let child_pid;
    {
        let mut s = Session::attach_path(&sock(xdg, "hangup"), "hangup", 80, 24)
            .expect("attach before hung upgrade");
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
    let before_host = host_pid(xdg, "hangup").expect("host pid before hung upgrade");

    // Trigger the upgrade against the hanging target. The probe times out (~5s)
    // and the host refuses. Whether the refusal is *reported* to the CLI or the
    // CLI's own deadline fires first is a timing race under load — this test's
    // job is only that the host survives the hung probe — so we don't assert on
    // the exit code here (the deterministic result-channel reporting is covered
    // by the other refusal tests). We only require the command returned.
    let _ = ghost(xdg)
        .args(["__upgrade", "hangup", hang.to_str().unwrap()])
        .output()
        .unwrap();

    // The host never exec'd, the child is still alive, and — the crux — the host
    // is RESPONSIVE again: it echoes fresh input, proving the hung probe did not
    // wedge the loop.
    assert_eq!(
        host_pid(xdg, "hangup").as_deref(),
        Some(before_host.as_str()),
        "host pid changed — a hung-probe upgrade must not exec"
    );
    assert!(
        alive(&child_pid),
        "the child (pid {child_pid}) died — a hung-probe upgrade must leave it running"
    );
    let mut s = Session::attach_path(&sock(xdg, "hangup"), "hangup", 80, 24)
        .expect("attach after hung upgrade");
    s.set_read_timeout(Some(Duration::from_millis(25))).unwrap();
    s.send_input(b"UNWEDGED\n").unwrap();
    let mut screen = Screen::new(80, 24, 100);
    assert!(
        wait_for_screen(&mut s, &mut screen, "UNWEDGED"),
        "the host never came back after a hung probe; saw:\n{}",
        screen.text().join("\n")
    );
}

/// A child that never returns the parser to a clean boundary (here it emits an
/// unterminated CSI, then idles as `cat`) would leave a requested upgrade
/// pending forever — and the requester blocked on it. The host gives the
/// boundary a bounded window and then ABANDONS the request, reporting why, so
/// `ghost __upgrade` fails instead of hanging. The child is left running.
#[test]
fn a_self_upgrade_gives_up_when_no_boundary_arrives() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();

    // `printf %s <ESC>[38;2` writes an incomplete CSI (ESC [ 3 8 ; 2), leaving
    // the host's parser mid-sequence; `cat` then idles, so it never returns to
    // Ground and no clean boundary ever arrives. The ESC is a real 0x1B byte in
    // the command string (portable — no reliance on the shell's octal escapes).
    let out = ghost(xdg)
        .args([
            "new",
            "noboundary",
            "-d",
            "--",
            "sh",
            "-c",
            "echo CHILD=$$; printf %s '\x1b[38;2'; exec cat",
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
        name: "noboundary",
    };
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains("noboundary")),
        "session not listed"
    );

    let child_pid;
    {
        let mut s = Session::attach_path(&sock(xdg, "noboundary"), "noboundary", 80, 24)
            .expect("attach before give-up upgrade");
        s.set_read_timeout(Some(Duration::from_millis(25))).unwrap();
        let mut screen = Screen::new(80, 24, 100);
        // The CHILD= line rendered before the incomplete CSI, so it is readable.
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
    let before_host = host_pid(xdg, "noboundary").expect("host pid before give-up upgrade");

    // A valid target (our own binary) — the refusal is purely that no boundary
    // arrives. `ghost __upgrade` must return non-zero naming the give-up.
    let out = ghost(xdg)
        .args(["__upgrade", "noboundary"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "`ghost __upgrade` should have reported the give-up, but it succeeded"
    );
    assert!(
        stderr.contains("boundary"),
        "give-up message did not mention the boundary; saw:\n{stderr}"
    );

    // The host never exec'd and the child is still alive.
    assert_eq!(
        host_pid(xdg, "noboundary").as_deref(),
        Some(before_host.as_str()),
        "host pid changed — a give-up must not exec"
    );
    assert!(
        alive(&child_pid),
        "the child (pid {child_pid}) died — a give-up must leave it running"
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
    let mut screen = Screen::new(80, 24, 100);

    // The pre-upgrade SCREEN survives the swap: the new host rebuilt it from a
    // checkpoint carried across the exec, so attaching resyncs the `CHILD=…`
    // line the old host had rendered — not a blank screen.
    assert!(
        wait_for_screen(&mut s, &mut screen, "CHILD="),
        "the pre-upgrade screen did not survive the upgrade; saw:\n{}",
        screen.text().join("\n")
    );

    // …and the child is live: it still echoes what we type, below the survived
    // screen (proving `cat` was adopted, not restarted).
    s.send_input(b"PONG-AFTER-UPGRADE\n").unwrap();
    assert!(
        wait_for_screen(&mut s, &mut screen, "PONG-AFTER-UPGRADE"),
        "the adopted child did not echo input after the upgrade; saw:\n{}",
        screen.text().join("\n")
    );
}

/// Spawn a detached `cat` session named `name` whose child prints its own pid
/// (then `exec`s `cat`, inheriting it), attach briefly to read that pid off the
/// screen, and return it. The session stays alive for the caller to act on.
fn spawn_cat_and_child_pid(xdg: &Path, name: &str) -> String {
    let out = ghost(xdg)
        .args([
            "new",
            name,
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
    assert!(
        wait_until(Duration::from_secs(5), || ls(xdg).contains(name)),
        "session not listed"
    );
    let mut s =
        Session::attach_path(&sock(xdg, name), name, 80, 24).expect("attach to read child pid");
    s.set_read_timeout(Some(Duration::from_millis(25))).unwrap();
    let mut screen = Screen::new(80, 24, 100);
    assert!(
        wait_for_screen(&mut s, &mut screen, "CHILD="),
        "child pid never printed; saw:\n{}",
        screen.text().join("\n")
    );
    let text = screen.text().join("\n");
    text.split("CHILD=")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .expect("CHILD=<pid> on screen")
        .to_string()
}

/// After a self-upgrade, a `ghost kill` (a SIGTERM to the host) must still
/// cleanly end the session and its adopted child. This proves the successor
/// REINSTALLED its terminating-signal handler and UNBLOCKED the signals the exec
/// had blocked: without that, the kill would be ignored (the child lingers) or
/// hit the default disposition and orphan the child. The signal half of the
/// re-exec contract.
#[test]
fn a_kill_after_a_self_upgrade_cleanly_ends_the_session_and_child() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let child_pid = spawn_cat_and_child_pid(xdg, "killup");
    let _guard = KillOnDrop {
        xdg,
        name: "killup",
    };

    let out = ghost(xdg).args(["__upgrade", "killup"]).output().unwrap();
    assert!(
        out.status.success(),
        "`ghost __upgrade` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The child was adopted across the exec.
    assert!(alive(&child_pid), "child did not survive the upgrade");

    // A kill now goes to the successor (same pid — an in-place exec).
    let out = ghost(xdg).args(["kill", "killup"]).output().unwrap();
    assert!(
        out.status.success(),
        "`ghost kill` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert!(
        wait_until(Duration::from_secs(5), || !alive(&child_pid)
            && !ls(xdg).contains("killup")),
        "a kill after the upgrade left the child (pid {child_pid}) or the session behind"
    );
}

/// A `ghost kill` (a raw SIGTERM) racing an in-flight self-upgrade must resolve
/// cleanly: the child is never ORPHANED (host gone, child still running) and the
/// session is never left half-torn. We force the race with SIGSTOP — freeze the
/// host, queue an upgrade request AND a SIGTERM against the frozen process, then
/// SIGCONT — so both are pending together when it resumes. Whichever wins, the
/// kill is honored (drained off the self-pipe before the exec, or handled by the
/// successor once it unblocks) and the teardown is clean. This asserts the
/// observable outcome; the mechanisms behind it (the pre-exec signal drain, and
/// `kill_session` refusing to prune a still-live host) are what make it hold.
#[test]
fn a_kill_racing_a_self_upgrade_never_orphans_the_child() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let child_pid = spawn_cat_and_child_pid(xdg, "raceup");
    let _guard = KillOnDrop {
        xdg,
        name: "raceup",
    };
    let host = host_pid(xdg, "raceup").expect("host pid");

    // Freeze the host so the upgrade request and the SIGTERM both land while it
    // can't act, guaranteeing they are pending together when it resumes.
    signal(&host, "STOP");
    // Queue an upgrade: the client connects and writes its request into the
    // socket buffer even though the frozen host hasn't read it. Background — it
    // blocks on the reply until the host resumes.
    let mut up = ghost(xdg).args(["__upgrade", "raceup"]).spawn().unwrap();
    // No observable predicate for "the request is queued in the kernel", so give
    // the client a brief moment to connect and send before we deliver the kill.
    std::thread::sleep(Duration::from_millis(300));
    signal(&host, "TERM"); // pending on the stopped host
    signal(&host, "CONT"); // resume: the kill wins, the upgrade doesn't leak past it

    // Clean outcome: the host is gone, the child is gone with it (never
    // orphaned), and the dead session is pruned from the listing.
    assert!(
        wait_until(Duration::from_secs(5), || {
            !alive(&host) && !alive(&child_pid) && !ls(xdg).contains("raceup")
        }),
        "a kill racing the upgrade left a bad state (host alive={}, child alive={}, listed={})",
        alive(&host),
        alive(&child_pid),
        ls(xdg).contains("raceup")
    );

    let _ = up.kill();
    let _ = up.wait();
}

/// A kill that lands while the host is INSIDE the handoff probe (a real window —
/// the probe can take seconds) must still be honored, not lost across the exec.
/// The target here is a valid but SLOW shim (`sleep`, then `exec` the real
/// ghost), so the host sits in the probe for ~1s; a `ghost kill` mid-probe must
/// tear the session down cleanly rather than exec into the (successful) upgrade
/// and drop the kill on the floor. Guards the pre-exec self-pipe drain.
#[test]
fn a_kill_during_the_upgrade_probe_is_honored_not_lost() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let child_pid = spawn_cat_and_child_pid(xdg, "probekill");
    let _guard = KillOnDrop {
        xdg,
        name: "probekill",
    };
    let host = host_pid(xdg, "probekill").expect("host pid");

    // A VALID upgrade target (it execs the real ghost, so absent a kill the
    // upgrade would succeed and the session would survive) whose `__handoff`
    // probe is slow — giving a wide window for a kill to land mid-prep.
    let body = format!("#!/bin/sh\nsleep 1\nexec {GHOST} \"$@\"\n");
    let target = write_fake_ghost(tmp.path(), &body, 0o755);

    // Trigger the upgrade (background — it blocks awaiting the outcome), then kill
    // the host while it is in the ~1s probe.
    let mut up = ghost(xdg)
        .args(["__upgrade", "probekill", target.to_str().unwrap()])
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let out = ghost(xdg).args(["kill", "probekill"]).output().unwrap();
    assert!(
        out.status.success(),
        "`ghost kill` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The kill was honored: host gone, child gone, session pruned — NOT upgraded
    // with the kill lost, and never a live host with its files pruned away.
    assert!(
        wait_until(Duration::from_secs(5), || {
            !alive(&host) && !alive(&child_pid) && !ls(xdg).contains("probekill")
        }),
        "a kill during the probe was not honored (host alive={}, child alive={}, listed={})",
        alive(&host),
        alive(&child_pid),
        ls(xdg).contains("probekill")
    );

    let _ = up.kill();
    let _ = up.wait();
}
