//! E2E: the terminal that attaches tells the session host what a program on the
//! tty is allowed to do to it, and the host goes on enforcing that after it leaves.
//!
//! A session outlives every terminal that shows it, and while detached the host
//! *is* the terminal — it filters the child's output and answers its queries alone.
//! So the policy can't live in the window: it's negotiated at attach, adopted by
//! the host's own emulator, and persisted, or a program would get away with things
//! the moment the user looked away.

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
        if start.elapsed() > timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
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

fn host_meta(xdg: &Path, name: &str) -> Option<serde_json::Value> {
    serde_json::from_slice(&std::fs::read(xdg.join("run/ghost").join(name).join("meta")).ok()?).ok()
}

/// The host's view of the session's title — what discovery and the fleet's cards
/// read, and the thing an OSC 2 sets.
fn host_title(xdg: &Path, name: &str) -> Option<String> {
    Some(host_meta(xdg, name)?.get("title")?.as_str()?.to_string())
}

/// The policy in the session's durable descriptor — what a restarted host, a
/// recreate or a resurrect reads back.
fn host_policy(xdg: &Path, name: &str) -> Option<serde_json::Value> {
    host_meta(xdg, name)?.get("policy").cloned()
}

#[test]
fn the_attached_terminal_tells_the_host_what_a_program_may_do_and_it_sticks() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "policy-neg";
    let _guard = KillOnDrop { xdg, name };

    // The child waits for a marker, then sets a window title. It runs while nobody
    // is attached, so whether the title takes is entirely the *host's* decision.
    let marker = xdg.join("go");
    let script = format!(
        "while [ ! -e '{}' ]; do sleep 0.05; done; printf '\\033]2;pwned\\007'; exec sleep 60",
        marker.display()
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

    // Attach as a display client, report a policy that forbids the title, detach.
    {
        let sock = xdg.join("run/ghost").join(name).join("sock");
        assert!(
            wait_until(Duration::from_secs(5), || sock.exists()),
            "session socket never appeared"
        );
        let mut s =
            ghost_vt::client::Session::attach_path(&sock, name, 80, 24).expect("attach failed");
        s.report_policy(ghost_term::TerminalPolicy {
            title: false,
            ..Default::default()
        })
        .expect("report_policy failed");
        // Stay until the resync lands, so the host has certainly read our frames
        // (dropping straight away races its first read — see `detached_query`).
        s.set_read_timeout(Some(Duration::from_millis(25))).unwrap();
        assert!(
            wait_until(Duration::from_secs(5), || s
                .pump()
                .map(|p| !p.output.is_empty())
                .unwrap_or(false)),
            "resync repaint never arrived"
        );
    }

    // Now, with nobody watching, the program tries to retitle the session.
    std::fs::write(&marker, b"").unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        host_title(xdg, name).as_deref(),
        Some(""),
        "the host kept enforcing the policy of the terminal that left"
    );

    // And it is on DISK, not just in the live host's memory: the session's
    // descriptor is what a recreate or a resurrect reads, and a policy that only
    // survived as long as the process did would let a restart silently hand the
    // session back to a program that had been refused.
    assert_eq!(
        host_policy(xdg, name).and_then(|p| p.get("title").and_then(|t| t.as_bool())),
        Some(false),
        "the negotiated policy was persisted"
    );
}

/// The policy in the session's *durable* descriptor — the one that outlives the
/// session, and the only thing a recreate or a resurrect actually reads. (The
/// runtime `meta` is pruned along with the session directory when the host exits.)
fn durable_policy(xdg: &Path, name: &str) -> Option<serde_json::Value> {
    let d: serde_json::Value = serde_json::from_slice(
        &std::fs::read(xdg.join("data/ghost/sessions").join(format!("{name}.json"))).ok()?,
    )
    .ok()?;
    d.get("policy").cloned()
}

#[test]
fn a_negotiated_policy_outlives_the_session_that_negotiated_it() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "policy-durable";
    let _guard = KillOnDrop { xdg, name };

    // A session whose host is gone — killed, rebooted, or simply finished — is
    // recreated from its *durable descriptor*, not from the runtime directory,
    // which is pruned with it. A policy that lived only in the runtime dir would
    // hand the resurrected session straight back to a program that had been
    // refused. So the descriptor has to carry it.
    let out = ghost(xdg)
        .args(["new", name, "-d", "--", "sleep", "60"])
        .output()
        .unwrap();
    assert!(out.status.success(), "`ghost new -d` failed");

    let sock = xdg.join("run/ghost").join(name).join("sock");
    assert!(
        wait_until(Duration::from_secs(5), || sock.exists()),
        "session socket never appeared"
    );
    {
        let mut s =
            ghost_vt::client::Session::attach_path(&sock, name, 80, 24).expect("attach failed");
        s.report_policy(ghost_term::TerminalPolicy {
            title: false,
            ..Default::default()
        })
        .expect("report_policy failed");
        s.set_read_timeout(Some(Duration::from_millis(25))).unwrap();
        assert!(
            wait_until(Duration::from_secs(5), || s
                .pump()
                .map(|p| !p.output.is_empty())
                .unwrap_or(false)),
            "resync repaint never arrived"
        );
    }

    assert!(
        wait_until(Duration::from_secs(5), || durable_policy(xdg, name)
            .and_then(|p| p.get("title").and_then(|t| t.as_bool()))
            == Some(false)),
        "the descriptor that outlives the session carries the policy, got {:?}",
        durable_policy(xdg, name)
    );
}

#[test]
fn a_terminal_that_allows_it_still_gets_its_title() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path();
    let name = "policy-allow";
    let _guard = KillOnDrop { xdg, name };

    // The same session, the same program, the default policy: the title lands. The
    // negotiation is what changed the outcome, not some other refusal.
    let out = ghost(xdg)
        .args([
            "new",
            name,
            "-d",
            "--",
            "bash",
            "-c",
            "printf '\\033]2;pwned\\007'; exec sleep 60",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "`ghost new -d` failed");

    assert!(
        wait_until(Duration::from_secs(5), || host_title(xdg, name).as_deref()
            == Some("pwned")),
        "an unrestricted session still lets a program set its title, got {:?}",
        host_title(xdg, name)
    );
}
