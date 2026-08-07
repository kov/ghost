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
use support::remote::{RealRemote, retry_some};
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

/// The counterpart to the reboot test above, and the line between them is the
/// whole point: a session the user ENDS (typing `exit`) is discarded on its
/// host, and the window must forget it — no relaunchable corpse, no membership
/// — while a session a reboot KILLS stays remembered and relaunchable. The
/// host-side discard happens where the local descriptor sweep cannot see, so
/// this is the watcher's remembered-set fetch proving itself over a real
/// transport: listing gone + descriptor gone there ⇒ forgotten here.
#[test]
fn a_remote_sessions_clean_exit_is_forgotten_not_offered_for_relaunch() {
    let Some(remote) = RealRemote::start() else {
        eprintln!("shell_remote: no sshd available; skipping");
        return;
    };
    // SAFETY: process-global, held under SERIAL for the duration.
    unsafe { std::env::set_var("GHOST_REMOTE_GHOST", remote.remote_ghost()) };

    with_isolated_xdg(|_tmp| {
        let r = RemoteSsh::new_in(remote.spec(), remote.control_dir()).expect("open transport");
        let remote_ghost =
            retry_some(Duration::from_secs(10), || r.negotiate().ok()).expect("negotiate");
        r.spawn_host(&remote_ghost, "ephemeral")
            .expect("spawn remote session");
        assert!(
            wait_until(Duration::from_secs(5), || r
                .list_sessions(&remote_ghost)
                .map(|s| s.iter().any(|i| i.name == "ephemeral"))
                .unwrap_or(false)),
            "the session never came up on the remote"
        );

        let q: Arc<QueuedEvents> = Arc::default();
        let sink: Arc<dyn EventSink> = q.clone();
        let mut app = App::headless_with_sink(sink);
        let fe = HeadlessFrontend::new();
        let group = app.mint_group();
        let wid = app.open_fleet_window(&fe, group, None);
        app.adopt_remote_host(remote.spec(), remote_ghost.clone(), &fe);

        let discovered = pump_until(&mut app, &fe, &q, Duration::from_secs(20), |app| {
            app.dispatch(wid, UiEvent::SessionsChanged, &fe);
            let root = app.root(wid).expect("window");
            sees_tile(&root.view(app.states()), "ephemeral")
        });
        assert!(
            discovered,
            "the remote session never appeared in the fleet: {:?}",
            visible_text(&app.root(wid).expect("window").view(app.states()))
        );

        // Drive it: click the card, so the window's group remembers it — the
        // state in which a stale corpse used to linger.
        let scene = app.root(wid).expect("window").view(app.states());
        let (x, y) = support::tile_center(&scene, "ephemeral")
            .unwrap_or_else(|| panic!("no card to click: {:?}", visible_text(&scene)));
        for ev in support::click_events(x, y) {
            app.dispatch(wid, ev, &fe);
        }
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
                .any(|g| g.members.iter().any(|m| m.ends_with("ephemeral"))),
            "driving it makes it the window's own: {:?}",
            app.groups()
        );

        // THE USER ENDS IT: `exit` into the remote shell. The child exits of
        // its own accord and the remote host discards the durable traces.
        app.dispatch(wid, UiEvent::Text("exit\r".into()), &fe);

        // The host really does forget it (this also drives `__remembered` over
        // the real transport: the discard must be visible in the fetch).
        assert!(
            wait_until(Duration::from_secs(30), || r
                .remembered_sessions(&remote_ghost)
                .map(|names| !names.contains("ephemeral"))
                .unwrap_or(false)),
            "the remote descriptor must be discarded by the clean exit"
        );

        // And so must the window: with its only session ended and nothing left to
        // return to (here or on the host), it closes rather than waiting on a
        // session the user themselves ended — the same rule a local exit follows.
        let closed = pump_until(&mut app, &fe, &q, Duration::from_secs(60), |app| {
            if let Some(wid) = app.window_ids().first().copied() {
                app.dispatch(wid, UiEvent::SessionsChanged, &fe);
            }
            app.window_ids().is_empty()
        });
        assert!(
            closed,
            "the window must close once its only remote session exits; groups={:?} \
             shows={:?}",
            app.groups(),
            app.root(wid).map(|r| visible_text(&r.view(app.states())))
        );
        // No lingering membership, and no corpse a later fleet would offer to
        // relaunch — a clean exit leaves nothing behind.
        assert!(
            !app.groups()
                .iter()
                .any(|g| g.members.iter().any(|m| m.ends_with("ephemeral"))),
            "a cleanly-exited remote session must not stay remembered: {:?}",
            app.groups()
        );
        let group = app.mint_group();
        let later = app.open_fleet_window(&fe, group, None);
        app.adopt_remote_host(remote.spec(), remote_ghost.clone(), &fe);
        for _ in 0..20 {
            app.dispatch(later, UiEvent::SessionsChanged, &fe);
            pump(&mut app, &fe, &q);
        }
        let scene = app.root(later).expect("window").view(app.states());
        assert!(
            !sees_tile(&scene, "ephemeral") && !sees_text(&scene, "relaunch"),
            "a cleanly-exited remote session must not be offered for relaunch; \
             shows={:?}",
            visible_text(&scene)
        );
    });
}

/// How many times `needle` is rendered on session `id`'s screen.
fn rendered_count(app: &App, id: &str, needle: &str) -> usize {
    app.states()
        .text_of(id)
        .map(|lines| lines.join("\n").matches(needle).count())
        .unwrap_or(0)
}

/// The shell driving one remote session whose child subscribed to focus
/// reporting — the stage both focus-recovery tests walk onto.
struct FocusRig {
    app: App,
    fe: HeadlessFrontend,
    q: Arc<QueuedEvents>,
    wid: winit::window::WindowId,
    /// The composite id (`<target>\u{241f}focus`) the window knows the session by.
    composite: String,
}

/// Stand up the shell driving remote session "focus", whose child has enabled
/// focus reporting (DEC ?1004) and echoes every byte it receives visibly
/// (`cat -v`: ESC renders as `^[`), with the first `^[[I` — the rising-edge
/// focus report — already on its screen. Every later focus report the child
/// hears becomes another visible `^[[I`.
fn rig_focus_child(remote: &RealRemote) -> FocusRig {
    let r = RemoteSsh::new_in(remote.spec(), remote.control_dir()).expect("open transport");
    let remote_ghost =
        retry_some(Duration::from_secs(10), || r.negotiate().ok()).expect("negotiate");
    r.spawn_host(&remote_ghost, "focus")
        .expect("spawn remote session");
    assert!(
        wait_until(Duration::from_secs(5), || r
            .list_sessions(&remote_ghost)
            .map(|s| s.iter().any(|i| i.name == "focus"))
            .unwrap_or(false)),
        "the session never came up on the remote"
    );

    let q: Arc<QueuedEvents> = Arc::default();
    let sink: Arc<dyn EventSink> = q.clone();
    let mut app = App::headless_with_sink(sink);
    let fe = HeadlessFrontend::new();
    let group = app.mint_group();
    let wid = app.open_fleet_window(&fe, group, None);
    app.adopt_remote_host(remote.spec(), remote_ghost.clone(), &fe);

    let discovered = pump_until(&mut app, &fe, &q, Duration::from_secs(20), |app| {
        app.dispatch(wid, UiEvent::SessionsChanged, &fe);
        let root = app.root(wid).expect("window");
        sees_tile(&root.view(app.states()), "focus")
    });
    assert!(
        discovered,
        "the remote session never appeared in the fleet: {:?}",
        visible_text(&app.root(wid).expect("window").view(app.states()))
    );

    // Drive it, the way the user does: click the card.
    let scene = app.root(wid).expect("window").view(app.states());
    let (x, y) = support::tile_center(&scene, "focus")
        .unwrap_or_else(|| panic!("no card to click: {:?}", visible_text(&scene)));
    for ev in support::click_events(x, y) {
        app.dispatch(wid, ev, &fe);
    }
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

    let composite = app
        .groups()
        .iter()
        .flat_map(|g| &g.members)
        .find(|m| m.ends_with("focus"))
        .expect("the window remembers its remote session")
        .clone();

    // The child subscribes to focus reporting and then echoes every byte it is
    // sent, control bytes rendered visibly. Typed once; the pty buffers it even
    // if the shell is still starting up.
    app.dispatch(
        wid,
        UiEvent::Text("printf '\\033[?1004h'; exec cat -v\r".into()),
        &fe,
    );

    // The ?1004 rising edge reports the current focus state to the child —
    // the already-fixed baseline, proven across the ssh transport.
    let baseline = pump_until(&mut app, &fe, &q, Duration::from_secs(20), |app| {
        rendered_count(app, &composite, "^[[I") >= 1
    });
    assert!(
        baseline,
        "enabling ?1004 never reported focus to the remote child: {:?}",
        app.states().text_of(&composite)
    );

    FocusRig {
        app,
        fe,
        q,
        wid,
        composite,
    }
}

/// Wait until the child's screen shows `want` focus reports, logging the
/// reconnect hold's dim transitions for diagnosis. On timeout, types a marker
/// to distinguish "never reattached" from "reattached but never re-told".
fn wait_for_focus_reports(rig: &mut FocusRig, want: usize, timeout: Duration) {
    let FocusRig {
        app,
        fe,
        q,
        wid,
        composite,
    } = rig;
    let mut last_dim = None;
    let retold = pump_until(app, fe, q, timeout, |app| {
        let scene = app.root(*wid).expect("window").view(app.states());
        let dim = support::session_dimmed(&scene, composite);
        if dim != last_dim {
            eprintln!("shell_remote: dim {last_dim:?} -> {dim:?}");
            last_dim = dim;
        }
        rendered_count(app, composite, "^[[I") >= want
    });
    if !retold {
        // A marker renders only if the transport is live again.
        app.dispatch(*wid, UiEvent::Text("PING\r".into()), fe);
        let alive = pump_until(app, fe, q, Duration::from_secs(10), |app| {
            rendered_count(app, composite, "PING") >= 1
        });
        panic!(
            "the child was never (re-)told the focus state (want {want} reports; \
             transport live again: {alive}): {:?}",
            app.states().text_of(composite)
        );
    }
}

/// An app that subscribes to focus reporting (DEC ?1004) and then loses its
/// transport mid-session must be RE-TOLD the terminal's focus state once the
/// connection is back — Claude Code's question prompt swallows keys while it
/// believes the terminal is unfocused, and any focus event sent during the
/// outage died in the dead pipe (`send_input` on a dropped transport is a
/// silent no-op). The reattach resync replays the host's `?1004h`, so the
/// local rising edge fires again and delivers the current state to the child:
/// a second visible `^[[I`.
#[test]
fn a_reattached_remote_session_retells_the_child_the_focus_state() {
    let Some(mut remote) = RealRemote::start() else {
        eprintln!("shell_remote: no sshd available; skipping");
        return;
    };
    // SAFETY: process-global, held under SERIAL for the duration (as ssh_reboot does).
    unsafe { std::env::set_var("GHOST_REMOTE_GHOST", remote.remote_ghost()) };

    with_isolated_xdg(|_tmp| {
        let mut rig = rig_focus_child(&remote);

        // Mid-session, the connection dies FAST (the peer closes — no keepalive
        // wait). The session keeps running on the remote; ghost notices,
        // reattaches, and resyncs on its own. The hold can be sub-poll brief,
        // so the wait is on the outcome, not on observing the dim.
        remote.sever_connections();
        wait_for_focus_reports(&mut rig, 2, Duration::from_secs(90));
    });
}

/// A transport that dies SILENTLY — no FIN/RST reaches the local master when a
/// laptop sleeps or the network moves underneath it — leaves every keystroke
/// dying in the dead pipe until ssh's keepalive gives up (~45s of a Claude
/// question that "won't accept input"). The shell must not sit out that window
/// when it has reason to suspect a suspend: probing the remote transports
/// reaps the wedged master, the mux'd session pipes EOF at once, and the
/// ordinary drop→hold→reattach→resync path re-tells the child its focus state
/// within seconds.
#[test]
fn a_probed_wedged_transport_reconnects_without_waiting_for_the_keepalive() {
    let Some(mut remote) = RealRemote::start() else {
        eprintln!("shell_remote: no sshd available; skipping");
        return;
    };
    // SAFETY: process-global, held under SERIAL for the duration (as ssh_reboot does).
    unsafe { std::env::set_var("GHOST_REMOTE_GHOST", remote.remote_ghost()) };

    with_isolated_xdg(|_tmp| {
        let mut rig = rig_focus_child(&remote);

        // The peer goes silent; nothing EOFs, nothing RSTs. Only a probe (or
        // the ~45s keepalive) can notice.
        remote.silent_partition();

        // What the suspend hook runs on a wake suspicion. The 25s bound is
        // generous CI slop but well under the keepalive window — reaching the
        // second report inside it proves the probe did the detecting.
        rig.app.probe_remote_transports();
        wait_for_focus_reports(&mut rig, 2, Duration::from_secs(25));
    });
}
