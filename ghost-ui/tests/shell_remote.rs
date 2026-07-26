//! The **shell** against a **real remote host that reboots** — the highest-fidelity
//! test ghost has of the thing that used to lose sessions.
//!
//! `ssh_reboot.rs` proves the transport can recover; it says so itself: *"detecting
//! the loss is the App's job and its own problem"*. That was the gap this file
//! closes. Everything here is production code except winit and the GPU: a real
//! unprivileged `sshd` on loopback, the real `ssh` binary with its ControlMaster, a
//! real remote `ghost` in its own HOME/XDG, the real [`App`] with its real background
//! workers ([`ghost_ui::QueuedEvents`] stands in for winit's proxy, and the test
//! plays the event loop by draining it).
//!
//! The reboot is the real thing too (see [`support::remote::RealRemote::reboot`]):
//! the peer goes silent with no FIN/RST, so the local ControlMaster wedges exactly as
//! it does when a machine actually goes down, and the tmpfs runtime dir is wiped, so
//! the sessions on it are genuinely gone.
//!
//! Skips (passes) cleanly where no `sshd` is installed.

mod support;

use std::sync::Arc;
use std::time::Duration;

use ghost_ui::{App, EventSink, HeadlessFrontend, QueuedEvents};
use ghost_ui_core::UiEvent;
use ghost_vt::remote::RemoteSsh;
use support::remote::{RealRemote, SERIAL, retry_some};
use support::{sees_text, sees_tile, visible_text, wait_until, with_isolated_xdg};

/// Drive the loop the way winit does: run the App's once-per-wake pass, then apply
/// everything the background workers posted. Returns how many events were applied.
fn pump(app: &mut App, fe: &HeadlessFrontend, q: &QueuedEvents) -> usize {
    app.wake(fe);
    let events = q.take();
    let n = events.len();
    for ev in events {
        app.on_user_event(fe, ev);
    }
    n
}

/// Pump until `pred` holds (or time out), so a test never sleeps on a fixed guess.
fn pump_until(
    app: &mut App,
    fe: &HeadlessFrontend,
    q: &QueuedEvents,
    timeout: Duration,
    mut pred: impl FnMut(&mut App) -> bool,
) -> bool {
    wait_until(timeout, || {
        pump(app, fe, q);
        pred(app)
    })
}

/// A window driving a session on a remote host must not lose it when that host
/// reboots. It holds — showing the session and naming the host it is waiting for —
/// reconnects on its own once the host is back, and then, because the reboot really
/// did take the session with it, offers the explicit relaunch. What it must never do
/// is quietly forget the session, which is what "a remote reboot loses my windows"
/// was: with no tile and no memory of it, there was nothing left to recover.
/// **Slow by nature (~60s):** the loss is noticed by ssh's keepalive, so the hold
/// cannot begin before `ServerAliveInterval=15` × `ServerAliveCountMax=3`. Left in
/// the default suite regardless — an `#[ignore]`d test rots, and this one covers the
/// exact failure the user reported ("a remote reboot loses my sessions").
#[test]
fn a_window_survives_its_remote_hosts_reboot_and_keeps_the_session_recoverable() {
    let Some(mut remote) = RealRemote::start() else {
        eprintln!("shell_remote: no sshd available; skipping");
        return;
    };
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: process-global, held under SERIAL for the duration (as ssh_reboot does).
    unsafe { std::env::set_var("GHOST_REMOTE_GHOST", remote.remote_ghost()) };

    with_isolated_xdg(|_tmp| {
        // A real transport to the fixture, and a real session living on it.
        let r = RemoteSsh::new_in(remote.spec(), remote.control_dir()).expect("open transport");
        let remote_ghost =
            retry_some(Duration::from_secs(10), || r.negotiate().ok()).expect("negotiate");
        r.spawn_host(&remote_ghost, "work")
            .expect("spawn remote session");
        assert!(
            wait_until(Duration::from_secs(5), || r
                .list_sessions(&remote_ghost)
                .map(|s| s.iter().any(|i| i.name == "work"))
                .unwrap_or(false)),
            "the session never came up on the remote"
        );

        // The shell, with its workers live and this test playing the event loop.
        let q: Arc<QueuedEvents> = Arc::default();
        let sink: Arc<dyn EventSink> = q.clone();
        let mut app = App::headless_with_sink(sink);
        let fe = HeadlessFrontend::new();
        let group = app.mint_group();
        let wid = app.open_fleet_window(&fe, group, None);
        // The host is ours: hand it over as a finished connect would. Its watcher now
        // streams the host's session set into the fleet.
        app.adopt_remote_host(remote.spec(), remote_ghost.clone(), &fe);

        // The remote session shows up as a card, discovered over the transport.
        let discovered = pump_until(&mut app, &fe, &q, Duration::from_secs(20), |app| {
            app.dispatch(wid, UiEvent::SessionsChanged, &fe);
            let root = app.root(wid).expect("window");
            sees_tile(&root.view(app.states()), "work")
        });
        assert!(
            discovered,
            "the remote session never appeared in the fleet: {:?}",
            visible_text(&app.root(wid).expect("window").view(app.states()))
        );

        // Attach it into the window, the way the user does: click the card. This is
        // the state that matters — a window *driving* a remote session — and it is
        // what makes the session the window's own (its group remembers it).
        let scene = app.root(wid).expect("window").view(app.states());
        let (x, y) = support::tile_center(&scene, "work")
            .unwrap_or_else(|| panic!("no card to click: {:?}", visible_text(&scene)));
        for ev in support::click_events(x, y) {
            app.dispatch(wid, ev, &fe);
        }
        // A confirmation appears only if something else holds it; take it if offered.
        let scene = app.root(wid).expect("window").view(app.states());
        if let Some((cx, cy)) = support::button_center(&scene, "Take over") {
            for ev in support::click_events(cx, cy) {
                app.dispatch(wid, ev, &fe);
            }
        }
        let attached = pump_until(&mut app, &fe, &q, Duration::from_secs(20), |app| {
            !app.root(wid).expect("window").is_fleet()
        });
        assert!(
            attached,
            "the window never opened the remote session: {:?}",
            visible_text(&app.root(wid).expect("window").view(app.states()))
        );
        assert!(
            app.groups()
                .iter()
                .any(|g| g.members.iter().any(|m| m.ends_with("work"))),
            "driving it makes it the window's own: {:?}",
            app.groups()
        );

        // THE HOST GOES DOWN: silent peer (the local master wedges, as in life) and
        // its runtime dir is wiped, so the session is truly gone.
        remote.reboot();

        // The composite id the window knows it by (`<target>␟work`).
        let composite = app
            .groups()
            .iter()
            .flat_map(|g| &g.members)
            .find(|m| m.ends_with("work"))
            .expect("the window remembers its remote session")
            .clone();

        // WHAT MUST HAPPEN: the window holds. It keeps drawing the session's last
        // screen, dimmed — the visible "I am waiting, not gone" — and never ends it.
        let holding = pump_until(&mut app, &fe, &q, Duration::from_secs(120), |app| {
            let scene = app.root(wid).expect("window").view(app.states());
            support::session_dimmed(&scene, &composite) == Some(true)
        });
        assert!(
            holding,
            "after its host went down the window must hold the session's frozen \
             screen, dimmed; drawn={:?} shows={:?}",
            support::session_dimmed(
                &app.root(wid).expect("window").view(app.states()),
                &composite
            ),
            visible_text(&app.root(wid).expect("window").view(app.states()))
        );

        // And it is still remembered, so nothing about it has been thrown away.
        assert!(
            app.groups().iter().any(|g| g.members.contains(&composite)),
            "the session must stay remembered across the outage: {:?}",
            app.groups()
        );

        // Going to the fleet (F9) shows it there too — rather than the empty fleet
        // this used to be — and offers the way forward. Which affordance that is
        // depends on how far the recovery has got, and both are correct: "waiting
        // for <host>" with nothing to click while the host is still unreachable, and
        // an explicit `relaunch` once ghost's own retry has reconnected and found the
        // session really did go down with the host. The fixture's listener never
        // stopped accepting, so it lands on the latter within seconds; `shell.rs`
        // pins the waiting wording deterministically.
        app.dispatch(
            wid,
            UiEvent::Key {
                key: ghost_ui_core::Key::Named(ghost_ui_core::NamedKey::F9),
                mods: ghost_ui_core::Mods::default(),
                kind: ghost_ui_core::KeyEventKind::Press,
                alts: None,
            },
            &fe,
        );
        let in_fleet = pump_until(&mut app, &fe, &q, Duration::from_secs(60), |app| {
            app.dispatch(wid, UiEvent::SessionsChanged, &fe);
            let scene = app.root(wid).expect("window").view(app.states());
            sees_tile(&scene, "work")
                && (sees_text(&scene, "waiting for") || sees_text(&scene, "relaunch"))
        });
        assert!(
            in_fleet,
            "the fleet must show the session, waiting or relaunchable, not nothing: {:?}",
            visible_text(&app.root(wid).expect("window").view(app.states()))
        );

        // And "recoverable" has to mean it: click the relaunch the fleet offered and
        // the session comes back on the host — a live preview where the dead card was.
        // (This is the resurrection being explicit, per the locked decision: ghost
        // never brings a session back on its own.)
        let scene = app.root(wid).expect("window").view(app.states());
        let Some((rx, ry)) = support::button_center(&scene, "relaunch") else {
            panic!(
                "expected a relaunch to click once the host was back: {:?}",
                visible_text(&scene)
            );
        };
        for ev in support::click_events(rx, ry) {
            app.dispatch(wid, ev, &fe);
        }
        let revived = pump_until(&mut app, &fe, &q, Duration::from_secs(60), |app| {
            app.dispatch(wid, UiEvent::SessionsChanged, &fe);
            let scene = app.root(wid).expect("window").view(app.states());
            // A live tile draws a terminal preview; a dead card draws none.
            support::session_dimmed(&scene, &composite).is_some() && !sees_text(&scene, "relaunch")
        });
        assert!(
            revived,
            "relaunch must bring the session back on the host: {:?}",
            visible_text(&app.root(wid).expect("window").view(app.states()))
        );
    });
}
