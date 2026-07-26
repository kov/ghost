//! SSH transport against a **real `sshd`** — Tier-1 high-fidelity remote.
//!
//! `ssh_remote.rs` gives each destination isolated dirs but fakes `ssh` with a
//! shell shim, so it can't exercise anything about the *real* ssh client/server
//! lifecycle — `ControlMaster`/`ControlPersist`, host-key handling, or what
//! happens to a multiplexed connection when the remote goes away. A whole class
//! of bugs (a session survives a remote *reboot*, a wedged control master, stale
//! control sockets) is structurally invisible to it.
//!
//! These drive ghost's [`RemoteSsh`] transport directly at the fixture; the same
//! fixture carries the shell-level reboot tests in `shell_remote.rs`.
//!
//! Skips (passes) cleanly where no `sshd` is installed, e.g. a minimal CI image.

mod support;

use std::time::Duration;

use ghost_vt::client::Session;
use ghost_vt::remote::RemoteSsh;
use ghost_vt::screen::Screen;
use support::remote::{RealRemote, SERIAL, retry_some, wait_until};

/// The regression this fixture exists for. A remote reboot leaves ghost's shared
/// ControlMaster wedged — TCP dead, process persisted by `ControlPersist`. The
/// bug: `master_alive()` is `ssh -O check`, which reports the *local process* as
/// "Master running" even though the connection is dead, so `reap_wedged_master()`
/// never clears it and every reconnect multiplexes onto the corpse. Correct
/// behaviour: once the host is back, ghost reaps the wedged master and
/// re-negotiates. (Bounded well under the ~45s keepalive backstop so we're
/// testing the reap, not the master eventually self-expiring.)
#[test]
fn ghost_reconnects_a_remote_after_a_reboot() {
    let Some(mut remote) = RealRemote::start() else {
        eprintln!("ssh_reboot: no sshd available; skipping");
        return;
    };
    // Point negotiation at the isolated remote binary. This binary's only test.
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    unsafe { std::env::set_var("GHOST_REMOTE_GHOST", remote.remote_ghost()) };

    let r = RemoteSsh::new_in(remote.spec(), remote.control_dir()).expect("open transport");
    assert!(
        retry_some(Duration::from_secs(10), || r.negotiate().ok()).is_some(),
        "the initial connection should negotiate the remote ghost"
    );

    remote.reboot();

    assert!(
        wait_until(Duration::from_secs(20), || r.negotiate().is_ok()),
        "after a remote reboot ghost must reap the wedged control master and \
         reconnect, but it never did within 20s"
    );
}

/// The recovery *mechanism* a dead REMOTE tile relies on: a session on the remote
/// is lost when the reboot wipes its tmpfs runtime dir, but ghost can recreate it
/// on the returned host over the transport (`spawn_host` = `ghost new -d`). That's
/// what `Cmd::Recreate`/`Cmd::Resurrect` route a remote id to (ghost-ui/src/main.rs)
/// instead of the local `spawn_dead` that refuses a remote id. Proven end-to-end
/// over real ssh, riding the same silent-reboot reconnect as the test above.
#[test]
fn a_remote_session_relaunches_on_its_host_after_a_reboot() {
    let Some(mut remote) = RealRemote::start() else {
        eprintln!("ssh_reboot: no sshd available; skipping");
        return;
    };
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    unsafe { std::env::set_var("GHOST_REMOTE_GHOST", remote.remote_ghost()) };
    let r = RemoteSsh::new_in(remote.spec(), remote.control_dir()).expect("open transport");
    let ghost =
        retry_some(Duration::from_secs(10), || r.negotiate().ok()).expect("initial negotiate");

    let listed = |ghost: &str| {
        r.list_sessions(ghost)
            .map(|s| s.iter().any(|i| i.name == "recovered"))
            .unwrap_or(false)
    };

    // A session exists on the remote before the reboot.
    r.spawn_host(&ghost, "recovered")
        .expect("spawn remote session");
    assert!(
        wait_until(Duration::from_secs(5), || listed(&ghost)),
        "the session was never listed on the remote before the reboot"
    );

    remote.reboot(); // wipes the runtime dir → the session's socket is gone

    // Recovery mirrors exactly what App::respawn_remote_dead / spawn_remote_session
    // now do: reap the wedged master, then relaunch on the host over a fresh
    // connection — reusing the known remote-ghost path, NOT re-negotiating (the
    // recovery path holds the already-negotiated host). Pre-fix this failed: the
    // relaunch multiplexed onto the wedged master.
    r.reap_wedged_master();
    r.spawn_host(&ghost, "recovered")
        .expect("relaunch on the returned host");
    assert!(
        wait_until(Duration::from_secs(10), || listed(&ghost)),
        "the remote session did not come back after relaunch on its host"
    );
}

/// Pump a driven session into `screen` until it renders `needle` or `timeout`.
/// Sets a short read timeout so `pump` polls instead of blocking on a quiet
/// session (an idle remote shell would otherwise hang the deadline check).
fn pump_until(session: &mut Session, screen: &mut Screen, needle: &str, timeout: Duration) -> bool {
    let _ = session.set_read_timeout(Some(Duration::from_millis(50)));
    wait_until(timeout, || {
        if let Ok(p) = session.pump() {
            screen.feed(&p.output);
        }
        screen.text().join("\n").contains(needle)
    })
}

/// Type `input` and pump until `screen` renders `needle`. `input` is resent every
/// poll so it can't be lost to the shell-startup race (an early keystroke arrives
/// before the deferred child is reading); `needle` renders from the shell's echo of
/// the typed line, so it doesn't depend on the shell actually executing it.
fn type_until(
    session: &mut Session,
    screen: &mut Screen,
    input: &[u8],
    needle: &str,
    timeout: Duration,
) -> bool {
    let _ = session.set_read_timeout(Some(Duration::from_millis(50)));
    wait_until(timeout, || {
        let _ = session.send_input(input);
        if let Ok(p) = session.pump() {
            screen.feed(&p.output);
        }
        screen.text().join("\n").contains(needle)
    })
}

/// The recovery a lost mid-session connection needs: the connection ends but the
/// remote host and its session KEEP RUNNING (a network partition, not a reboot), so
/// unlike a reboot the session is NOT gone — it must be RE-ATTACHED and its screen
/// resynced by the host, never relaunched. Proven end-to-end over real ssh: attach,
/// put a marker on the screen, drop the client (its transport ends), then reap any
/// wedged master and re-attach — the host resyncs the surviving screen, marker and
/// all. This is the ghost-vt half of the mid-session reconnect feature; the App-level
/// "reconnecting" state machine rides on this being recoverable. (Detecting the loss
/// is the App's job and its own problem — a silently-partitioned ssh connection
/// surfaces no EOF to the client until ssh's keepalive fires ~45s later.)
#[test]
fn a_dropped_connection_reattaches_and_resyncs_a_surviving_session() {
    let Some(remote) = RealRemote::start() else {
        eprintln!("ssh_reboot: no sshd available; skipping");
        return;
    };
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    unsafe { std::env::set_var("GHOST_REMOTE_GHOST", remote.remote_ghost()) };
    let r = RemoteSsh::new_in(remote.spec(), remote.control_dir()).expect("open transport");
    let ghost =
        retry_some(Duration::from_secs(10), || r.negotiate().ok()).expect("initial negotiate");

    // A live session with recognizable content on its screen.
    r.spawn_host(&ghost, "survivor")
        .expect("spawn remote session");
    assert!(
        wait_until(Duration::from_secs(5), || r
            .list_sessions(&ghost)
            .map(|s| s.iter().any(|i| i.name == "survivor"))
            .unwrap_or(false)),
        "the session was never listed on the remote"
    );
    let mut screen = Screen::new(80, 24, 100);
    let mut session = Session::attach_ssh(
        r.pipe_command(&ghost, "survivor"),
        "survivor",
        80,
        24,
        ghost_vt::protocol::PROTO_LEVEL,
    )
    .expect("attach");
    assert!(
        type_until(
            &mut session,
            &mut screen,
            b"echo GHOSTMARK\n",
            "GHOSTMARK",
            Duration::from_secs(10)
        ),
        "the marker never rendered on the attached screen"
    );

    // The client's connection ends but the session keeps running on the host —
    // dropping the `Session` closes the transport (its ssh child is killed), the
    // way a lost connection or the App's reconnect path abandons a dead client.
    // The remote session host, a separate long-lived process, survives.
    drop(session);

    // Recovery: reap any wedged master (a silent partition would leave one), then
    // RE-ATTACH (not relaunch) over a fresh connection — the host resyncs the
    // surviving screen at our geometry. Take-over displaces any stale display client.
    r.reap_wedged_master();
    let mut screen = Screen::new(80, 24, 100);
    let reattached = wait_until(Duration::from_secs(25), || {
        let Ok(mut s) = Session::attach_ssh(
            r.pipe_command(&ghost, "survivor"),
            "survivor",
            80,
            24,
            ghost_vt::protocol::PROTO_LEVEL,
        ) else {
            return false;
        };
        pump_until(&mut s, &mut screen, "GHOSTMARK", Duration::from_secs(5))
    });
    assert!(
        reattached,
        "after the connection dropped and was reaped, re-attaching to the surviving \
         session never resynced its screen (marker never came back)"
    );
}
