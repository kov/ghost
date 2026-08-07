//! End-to-end tests of the **shell** — the real [`App`], driven over a
//! [`HeadlessFrontend`] against real session hosts. Everything is production code
//! but winit and the GPU: real `groups.toml`/`windows.toml`, real descriptors and
//! recordings, real session sockets.
//!
//! These cover the window-lifecycle behaviour that unit tests structurally can't:
//! a unit test feeds `startup_choice` a hand-built session list, so it passes while
//! the shell, fed the state that really accumulates on disk, chooses differently.

mod support;

use std::time::Duration;

use ghost_ui::{App, HeadlessFrontend};
use ghost_ui_core::UiEvent;
use support::{
    attach_elsewhere, remember_dead_member, sees_text, sees_tile, spawn_session, visible_text,
    wait_until, with_isolated_xdg,
};

/// Reconcile a fleet window against the world (the shell answers with a real
/// `session::list()` and the dead-member sweep), the way the runtime-dir watcher
/// does after any session change.
fn reconcile(app: &mut App, wid: winit::window::WindowId, fe: &HeadlessFrontend) {
    app.dispatch(wid, UiEvent::SessionsChanged, fe);
}

/// A group whose member died is meant to render as a relaunchable dead tile — that
/// is what makes the fleet a place to recover work rather than a dead end. Asserted
/// on the **drawn scene**: a tile the fleet remembers but never lays out is an empty
/// fleet to the user, and only the scene tells the two apart.
#[test]
fn a_remembered_dead_member_is_drawn_in_the_fleet() {
    with_isolated_xdg(|_tmp| {
        remember_dead_member("win-old-0", "gone-1");
        let mut app = App::headless();
        let fe = HeadlessFrontend::new();
        let group = app.mint_group();
        let wid = app.open_fleet_window(&fe, group, None);
        let drawn = wait_until(Duration::from_secs(2), || {
            reconcile(&mut app, wid, &fe);
            app.wake(&fe);
            let scene = app.root(wid).expect("window").view(app.states());
            sees_tile(&scene, "gone-1") && sees_text(&scene, "relaunch")
        });
        assert!(
            drawn,
            "the remembered dead member must be DRAWN, with its relaunch chip; \
             the window shows: {:?}",
            visible_text(&app.root(wid).expect("window").view(app.states()))
        );
    });
}

/// The one way a fleet really does open with nothing in it: a group remembering a
/// member whose **descriptor is gone**. The descriptor is the resurrection ticket, so
/// a member without one was discarded — killed, or exited cleanly, possibly in another
/// ghost process whose registry write this one never saw. Such a membership must
/// neither be drawn (there is nothing to resurrect) nor count as a reason to open the
/// fleet, and the sweep must forget it so it stops haunting later launches.
#[test]
fn a_member_whose_descriptor_is_gone_is_forgotten_not_shown() {
    with_isolated_xdg(|_tmp| {
        remember_dead_member("win-old-0", "discarded-1");
        // What a kill in another process leaves behind: the group still names it, the
        // resurrection ticket is gone.
        std::fs::remove_file(ghost_vt::descriptor::path("discarded-1")).expect("drop descriptor");

        let mut app = App::headless();
        let fe = HeadlessFrontend::new();
        let group = app.mint_group();
        let wid = app.open_fleet_window(&fe, group, None);
        for _ in 0..3 {
            reconcile(&mut app, wid, &fe);
            app.wake(&fe);
        }
        let scene = app.root(wid).expect("window").view(app.states());
        assert!(
            !sees_tile(&scene, "discarded-1"),
            "a member with no descriptor cannot be resurrected, so it must not be \
             drawn; the window shows: {:?}",
            visible_text(&scene)
        );
        assert!(
            !app.groups()
                .iter()
                .any(|g| g.members.iter().any(|m| m == "discarded-1")),
            "the sweep must forget it, or it haunts every later launch: {:?}",
            app.groups()
        );
    });
}

/// The focus trace exists to settle one question in an incident — "did the window I
/// just focused actually get the focus, and what was it showing?" — and per-session
/// lines can't answer it. Two windows swapping focus log a focus-out for one session
/// and a focus-in for another, with nothing saying which window either belongs to,
/// and a window sitting in the fleet reports nothing at all (no session to tell), so
/// the pair reads as a stray focus-out with no matching focus-in. Every OS focus
/// event names its window and what that window shows, in both modes.
#[test]
fn an_os_focus_event_names_its_window_in_the_trace() {
    with_isolated_xdg(|tmp| {
        support::spawn_session_running("focus-1", "echo focus-1 ready; exec cat");
        let mut app = App::headless();
        let fe = HeadlessFrontend::new();
        let group = app.mint_group();
        let single = app
            .open_single_window(&fe, "focus-1", group, None)
            .expect("window");
        let fleet_group = app.mint_group();
        let fleet = app.open_fleet_window(&fe, fleet_group, None);
        app.wake(&fe);

        // Focus leaves the session window for the fleet one — the shape that reads as
        // a bare focus-out today.
        let path = tmp.join("focus-trace.log");
        // SAFETY: `with_isolated_xdg` holds the suite's env lock, so no other test
        // reads or writes the environment concurrently; `focus_trace` re-reads the
        // var per event, so there is no latched state to leave behind.
        unsafe { std::env::set_var("GHOST_FOCUS_TRACE", &path) };
        app.dispatch(single, UiEvent::Focus(false), &fe);
        app.dispatch(fleet, UiEvent::Focus(true), &fe);
        unsafe { std::env::remove_var("GHOST_FOCUS_TRACE") };

        let log = std::fs::read_to_string(&path).expect("the trace file was written");
        let named: Vec<&str> = log.lines().filter(|l| l.contains("os-focus")).collect();
        assert_eq!(named.len(), 2, "one line per OS focus event, got:\n{log}");
        assert!(
            named[0].contains("focus-1") && named[0].contains("focused=false"),
            "the blur names the window's foreground session:\n{log}"
        );
        assert!(
            named[0].contains("mode=single"),
            "...and what mode that window is in:\n{log}"
        );
        assert!(
            named[1].contains("focused=true") && named[1].contains("mode=fleet"),
            "a fleet window's focus is traced too, or its half of a swap is invisible:\n{log}"
        );
        let win_of = |line: &str| {
            line.split("win=")
                .nth(1)
                .expect("the line names a window")
                .split_whitespace()
                .next()
                .expect("a window id")
                .to_string()
        };
        assert_ne!(
            win_of(named[0]),
            win_of(named[1]),
            "the two events must be attributable to DIFFERENT windows — that is the \
             whole point of naming them:\n{log}"
        );
    });
}

/// Typing `exit` / Ctrl-D ends a session for good: the child exits of its own
/// accord, the host discards the durable traces on its way out, and nothing may
/// keep offering to relaunch it — not the fleet the window falls back to, and
/// not a later launch. (An *unclean* death — logout, reboot, crash — is the
/// case that stays relaunchable; this is the clean one.)
#[test]
fn a_cleanly_exited_session_is_not_offered_for_relaunch() {
    with_isolated_xdg(|_tmp| {
        // `exec cat`: the child is `cat`, and the user's Ctrl-D is EOF — it
        // exits 0, of its own accord, exactly like a shell quit by `exit`.
        support::spawn_session_running("done-1", "echo done-1 ready; exec cat");
        let mut app = App::headless();
        let fe = HeadlessFrontend::new();
        let group = app.mint_group();
        let wid = app
            .open_single_window(&fe, "done-1", group, None)
            .expect("window");
        app.wake(&fe);
        assert!(
            app.root(wid).expect("window").foregrounds("done-1"),
            "precondition: the window drives the session"
        );

        // The user ends it from the keyboard.
        app.dispatch(wid, UiEvent::Text("\u{4}".into()), &fe);

        // With nothing else to show and nothing to return to, the window closes.
        assert!(
            wait_until(Duration::from_secs(10), || {
                app.wake(&fe);
                app.root(wid).is_none()
            }),
            "the window must close once its only session exits; it shows: {:?}",
            app.root(wid).map(|r| visible_text(&r.view(app.states())))
        );

        // And no later fleet may remember the session as a relaunchable corpse.
        let group = app.mint_group();
        let fleet = app.open_fleet_window(&fe, group, None);
        assert!(
            wait_until(Duration::from_secs(5), || {
                reconcile(&mut app, fleet, &fe);
                app.wake(&fe);
                let scene = app.root(fleet).expect("window").view(app.states());
                !sees_tile(&scene, "done-1") && !sees_text(&scene, "relaunch")
            }),
            "a cleanly-exited session must not be offered for relaunch; the fleet shows: {:?}",
            visible_text(&app.root(fleet).expect("window").view(app.states()))
        );
        assert!(
            !app.groups()
                .iter()
                .any(|g| g.members.iter().any(|m| m == "done-1")),
            "its membership goes with it: {:?}",
            app.groups()
        );
        assert!(
            ghost_vt::descriptor::read("done-1").is_none(),
            "a cleanly-exited session leaves no descriptor"
        );
        assert!(
            !ghost_vt::paths::recording_path("done-1").exists(),
            "a cleanly-exited session leaves no recording"
        );
    });
}

/// Alt-N / File > New Window with nothing detached must open a **new session**, not
/// a fleet whose only content is sessions attached in other windows (whose only
/// action is stealing them). The trigger in the wild is an old auto-group: a window
/// that ran days ago leaves its member in `groups.toml`, and once that session dies
/// the shell counted it as "something to return to" forever.
#[test]
fn a_new_window_spawns_when_only_stale_memories_and_elsewhere_sessions_remain() {
    with_isolated_xdg(|_tmp| {
        // The state a real ~/.local/share/ghost accumulates: sessions live but held
        // by other windows, plus a long-gone window's remembered member.
        spawn_session("held-1");
        let _held = attach_elsewhere("held-1");
        remember_dead_member("win-old-0", "gone-1");

        let mut app = App::headless();
        let fe = HeadlessFrontend::new();
        app.open_launch_window(&fe);

        let wid = *app.window_ids().first().expect("the new window opened");
        let root = app.root(wid).expect("window");
        assert!(
            !root.is_fleet(),
            "a new window with nothing detached must open a session, not the fleet"
        );
        assert!(
            root.foreground().is_some(),
            "the new window shows a session"
        );
    });
}

/// The other half of the rule: a genuinely detached session IS something to return
/// to, so a new window opens the fleet rather than piling another session on top.
#[test]
fn a_new_window_opens_the_fleet_when_a_session_is_detached() {
    with_isolated_xdg(|_tmp| {
        spawn_session("detached-1");
        assert!(
            wait_until(Duration::from_secs(5), || {
                ghost_vt::session::list()
                    .unwrap_or_default()
                    .iter()
                    .any(|i| i.name == "detached-1" && !i.attached)
            }),
            "the session never showed up as detached"
        );

        let mut app = App::headless();
        let fe = HeadlessFrontend::new();
        app.open_launch_window(&fe);

        let wid = *app.window_ids().first().expect("the new window opened");
        assert!(
            app.root(wid).expect("window").is_fleet(),
            "a detached session is something to return to: open the fleet"
        );
    });
}

/// A group remembering a session on a **remote host that is away** must show it: the
/// window is supposed to be waiting for that host to come back, and the tile is
/// where the wait is visible (and where a reconnect lands). Nothing local can stand
/// in for it — a remote member has no local descriptor and no local recording, so
/// the dead-member sweep never names it, which is why this needs its own coverage.
#[test]
fn a_remembered_remote_member_whose_host_is_away_is_drawn_as_waiting() {
    with_isolated_xdg(|_tmp| {
        // A group from a window that had a remote session on another host; the host is
        // not connected (rebooting, asleep, off the network).
        let dir = ghost_vt::paths::data_dir();
        std::fs::create_dir_all(&dir).expect("data dir");
        // The separator is a control character: TOML forbids it raw, and ghost's own
        // writer escapes it — as the real `windows.toml`/`groups.toml` show.
        std::fs::write(
            dir.join("groups.toml"),
            "[[group]]\nid = \"win-old-0\"\nname = \"blue\"\ncolor = 0\n\
             members = [\"kov@couve\\u001Fwork\"]\n",
        )
        .expect("write groups.toml");

        let mut app = App::headless();
        let fe = HeadlessFrontend::new();
        let group = app.mint_group();
        let wid = app.open_fleet_window(&fe, group, None);
        for _ in 0..3 {
            reconcile(&mut app, wid, &fe);
            app.wake(&fe);
        }
        let scene = app.root(wid).expect("window").view(app.states());
        assert!(
            sees_tile(&scene, "work"),
            "a remembered remote member must be drawn while its host is away — \
             otherwise the fleet a launch sends us to is empty and the sessions look \
             lost; it shows: {:?}",
            visible_text(&scene)
        );
        assert!(
            sees_text(&scene, "waiting for kov@couve"),
            "and it must say what it is waiting for: {:?}",
            visible_text(&scene)
        );
        assert!(
            !sees_text(&scene, "relaunch"),
            "the session is probably still running there: offering a relaunch would \
             invite abandoning it: {:?}",
            visible_text(&scene)
        );
    });
}

/// Restoring at startup while the hosts are away — the daily shape of a laptop that
/// drives its work over ssh: every window's members are remote, and the hosts are
/// asleep, rebooting, or off the network.
///
/// The window must come back showing those sessions and waiting for their hosts. This
/// is the same defect as the empty fleet above, reached the other way: a restored
/// window's members come from `windows.toml`, so they are remembered before anything
/// has written a group registry — and a wait that only lives in a running process
/// would not survive the quit-and-relaunch this test performs.
#[test]
fn a_restored_window_waits_for_its_remote_sessions_when_the_host_is_away() {
    with_isolated_xdg(|_tmp| {
        let dir = ghost_vt::paths::data_dir();
        std::fs::create_dir_all(&dir).expect("data dir");
        // A window of two sessions on a host reached over ssh (couve's real shape:
        // an ssh group whose members are all `<target>␟<real>` composites).
        std::fs::write(
            dir.join("groups.toml"),
            "[[group]]\nid = \"win-1-0\"\nname = \"blue\"\ncolor = 0\n\
             members = [\"jabuticaba\\u001Fwork-1\", \"jabuticaba\\u001Fwork-2\"]\n\n\
             [group.connection]\nhost = \"jabuticaba\"\nextra = []\nkind = \"ssh\"\n",
        )
        .expect("write groups.toml");
        std::fs::write(
            dir.join("windows.toml"),
            "[[window]]\ngroup_id = \"win-1-0\"\ncols = 120\nrows = 40\nfleet = false\n\
             foreground = \"jabuticaba\\u001Fwork-1\"\n\
             attached = [\"jabuticaba\\u001Fwork-1\", \"jabuticaba\\u001Fwork-2\"]\n",
        )
        .expect("write windows.toml");

        let records = App::saved_workspace();
        assert_eq!(
            records.len(),
            1,
            "the saved window is readable: {records:?}"
        );
        let mut app = App::headless();
        let fe = HeadlessFrontend::new();
        app.restore_workspace(&fe, records);

        let wid = *app
            .window_ids()
            .first()
            .expect("the saved window is restored, not dropped for want of a live host");
        let shown = wait_until(Duration::from_secs(2), || {
            reconcile(&mut app, wid, &fe);
            app.wake(&fe);
            let scene = app.root(wid).expect("window").view(app.states());
            sees_tile(&scene, "work-1") && sees_tile(&scene, "work-2")
        });
        let scene = app.root(wid).expect("window").view(app.states());
        assert!(
            shown,
            "a restored window must show the remote sessions it is waiting for, not \
             an empty fleet; it shows: {:?}",
            visible_text(&scene)
        );
        assert!(
            sees_text(&scene, "waiting for jabuticaba"),
            "and name the host it is waiting for: {:?}",
            visible_text(&scene)
        );
    });
}

/// Stealing a session that is another window's foreground must move it, not clone it.
/// Two windows in one process, the second takes over the first's session: the first
/// has to stop showing it (it drops to the fleet, having nothing else), because a
/// session lives in exactly one view. Before this, the loser kept the session on
/// screen and typeable while the fleet already listed it as attached elsewhere.
#[test]
fn taking_over_another_windows_foreground_moves_it_out_of_that_window() {
    with_isolated_xdg(|_tmp| {
        // A live (printing) session: the dive into a taken-over session lands on its
        // next frame.
        support::spawn_chatty_session("shared-1");
        let mut app = App::headless();
        let fe = HeadlessFrontend::new();

        // Window A drives the session as its foreground.
        let g1 = app.mint_group();
        let a = app
            .open_single_window(&fe, "shared-1", g1, None)
            .expect("first window");
        app.wake(&fe);
        assert!(
            app.root(a).expect("A").foregrounds("shared-1"),
            "precondition: A shows the session"
        );

        // Window B takes it over from its fleet — the same-process adopt-in-place.
        let g2 = app.mint_group();
        let b = app.open_fleet_window(&fe, g2, None);
        // Sessions held by other windows hide behind a band ("N attached elsewhere ·
        // show"), so reveal them first — the same two clicks the user makes. A's
        // attachment reaches B's fleet through the session-meta push, which is not
        // synchronous with A's attach: until it lands, B sees the session as merely
        // detached and there is no band to click. Poll for it rather than assuming
        // one reconcile carried it — under load it does not.
        let mut reveal = None;
        let revealed = wait_until(Duration::from_secs(5), || {
            reconcile(&mut app, b, &fe);
            app.wake(&fe);
            let scene = app.root(b).expect("B").view(app.states());
            reveal = support::button_center(&scene, "show");
            reveal.is_some()
        });
        assert!(
            revealed,
            "no reveal band: {:?}",
            visible_text(&app.root(b).expect("B").view(app.states()))
        );
        let (sx, sy) = reveal.expect("the band was found above");
        for ev in support::click_events(sx, sy) {
            app.dispatch(b, ev, &fe);
        }
        // Click its card, exactly where the user sees it, then confirm the "steal
        // this session" modal — the real gesture, not a synthesized command.
        let scene = app.root(b).expect("B").view(app.states());
        let (x, y) = support::tile_center(&scene, "shared-1").unwrap_or_else(|| {
            panic!(
                "B's fleet draws no card for the session: {:?}",
                visible_text(&scene)
            )
        });
        for ev in support::click_events(x, y) {
            app.dispatch(b, ev, &fe);
        }
        let scene = app.root(b).expect("B").view(app.states());
        let (cx, cy) = support::button_center(&scene, "Take over").unwrap_or_else(|| {
            panic!(
                "no take-over confirmation offered: {:?}",
                visible_text(&scene)
            )
        });
        for ev in support::click_events(cx, cy) {
            app.dispatch(b, ev, &fe);
        }
        // The dive into a taken-over session animates and completes on its first
        // output, so let the loop run as it would live.
        let landed = wait_until(Duration::from_secs(5), || {
            app.wake(&fe);
            app.root(b).expect("B").foregrounds("shared-1")
        });
        assert!(
            landed,
            "B must end up showing the session it took over; it shows: {:?}",
            visible_text(&app.root(b).expect("B").view(app.states()))
        );
        assert!(
            !app.root(a).expect("A").foregrounds("shared-1"),
            "A must not still show a session B now owns: {:?}",
            visible_text(&app.root(a).expect("A").view(app.states()))
        );
        assert!(
            app.root(a).expect("A").is_fleet(),
            "with nothing else to show, A falls back to the fleet"
        );
        support::kill_session("shared-1");
    });
}

/// The mirror of "a new window with nothing detached spawns a session": when a
/// window's last session exits and there is nothing to return to, the window has
/// nothing to offer — an empty fleet is a dead end — so it closes. With no other
/// window left, that quits ghost, exactly like closing it by hand.
#[test]
fn the_last_session_exiting_closes_the_window_when_nothing_is_attachable() {
    with_isolated_xdg(|_tmp| {
        support::spawn_session_running("solo-1", "echo solo-1 ready; exec cat");
        let mut app = App::headless();
        let fe = HeadlessFrontend::new();
        let group = app.mint_group();
        let wid = app
            .open_single_window(&fe, "solo-1", group, None)
            .expect("window");
        app.wake(&fe);
        assert!(
            app.root(wid).expect("window").foregrounds("solo-1"),
            "precondition: the window drives the session"
        );

        // The user ends it from the keyboard (Ctrl-D into `cat` == `exit`).
        app.dispatch(wid, UiEvent::Text("\u{4}".into()), &fe);

        assert!(
            wait_until(Duration::from_secs(10), || {
                app.wake(&fe);
                app.window_ids().is_empty()
            }),
            "the window must close, not sit on an empty fleet; it shows: {:?}",
            app.root(wid).map(|r| visible_text(&r.view(app.states())))
        );
        assert!(
            fe.exited(),
            "and with no window left, ghost quits — as it does when the last \
             window is closed by hand"
        );
    });
}

/// The other half of the rule, symmetric with the new-window choice: a genuinely
/// detached session IS something to return to, so the window stays open on the
/// fleet where it can be reached.
#[test]
fn the_last_session_exiting_keeps_the_window_when_a_session_is_detached() {
    with_isolated_xdg(|_tmp| {
        support::spawn_session_running("quitter-1", "echo quitter-1 ready; exec cat");
        spawn_session("elsewhere-1");
        assert!(
            wait_until(Duration::from_secs(5), || {
                ghost_vt::session::list()
                    .unwrap_or_default()
                    .iter()
                    .any(|i| i.name == "elsewhere-1" && !i.attached)
            }),
            "the other session never showed up as detached"
        );

        let mut app = App::headless();
        let fe = HeadlessFrontend::new();
        let group = app.mint_group();
        let wid = app
            .open_single_window(&fe, "quitter-1", group, None)
            .expect("window");
        app.wake(&fe);
        app.dispatch(wid, UiEvent::Text("\u{4}".into()), &fe);

        assert!(
            wait_until(Duration::from_secs(10), || {
                app.wake(&fe);
                app.root(wid).is_some_and(|r| r.is_fleet())
            }),
            "the window must fall back to the fleet, not close: {:?}",
            app.root(wid).map(|r| visible_text(&r.view(app.states())))
        );
        assert!(!fe.exited(), "and ghost keeps running");
        support::kill_session("elsewhere-1");
    });
}

/// Closing an emptied window must not take the rest of the app with it: another
/// window holding its own session keeps ghost alive.
#[test]
fn an_emptied_window_closes_alone_while_another_window_keeps_ghost_running() {
    with_isolated_xdg(|_tmp| {
        support::spawn_session_running("gone-a", "echo gone-a ready; exec cat");
        support::spawn_session_running("stays-b", "echo stays-b ready; exec cat");
        let mut app = App::headless();
        let fe = HeadlessFrontend::new();
        let ga = app.mint_group();
        let a = app
            .open_single_window(&fe, "gone-a", ga, None)
            .expect("window A");
        let gb = app.mint_group();
        let b = app
            .open_single_window(&fe, "stays-b", gb, None)
            .expect("window B");
        app.wake(&fe);

        app.dispatch(a, UiEvent::Text("\u{4}".into()), &fe);

        assert!(
            wait_until(Duration::from_secs(10), || {
                app.wake(&fe);
                app.root(a).is_none()
            }),
            "A must close once its only session exits and nothing is attachable"
        );
        assert!(
            app.root(b).is_some_and(|r| r.foregrounds("stays-b")),
            "B keeps its own session"
        );
        assert!(!fe.exited(), "ghost stays running while a window remains");
        support::kill_session("stays-b");
    });
}
