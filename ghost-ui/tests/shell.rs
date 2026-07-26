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
