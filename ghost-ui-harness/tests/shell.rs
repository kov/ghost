//! End-to-end shell tests: drive the REAL frontend through [`Harness`] — injecting
//! events, executing the model's effects, advancing the animation clock — and assert
//! on the resulting `Scene`. These cover the shell/loop path (toggle → dive launch →
//! tick-driven camera → settle) that the model-only unit tests can't reach, with no
//! window and no GPU (pure behaviour: only `scene()`/`is_animating()` are read).

use ghost_render::{CellMetrics, Transform};
use ghost_ui_core::{Key, KeyEventKind, Mods, NamedKey, UiEvent};
use ghost_ui_harness::Harness;
use ghost_vt::session::SessionInfo;

const METRICS: CellMetrics = CellMetrics {
    advance: 9.0,
    line_height: 18.0,
};

fn info(name: &str, attached: bool, created_at: i64) -> SessionInfo {
    SessionInfo {
        name: name.to_string(),
        pid: 1,
        created_at: Some(created_at),
        title: String::new(),
        command: Vec::new(),
        attached,
        bell: false,
    }
}

fn f9() -> UiEvent {
    UiEvent::Key {
        key: Key::Named(NamedKey::F9),
        mods: Mods::NONE,
        kind: KeyEventKind::Press,
        alts: None,
    }
}

/// Does any layer carry a non-identity camera? (i.e. the scene is mid-zoom.)
fn camera_active(h: &Harness) -> bool {
    h.scene()
        .layers
        .iter()
        .any(|l| l.transform != Transform::IDENTITY)
}

/// Advance the clock in ~60fps steps until the animation settles (or a cap), so the
/// test doesn't depend on the exact dive duration. Returns the steps taken.
fn run_to_settle(h: &mut Harness) -> u32 {
    let mut t = 0;
    for step in 1..600 {
        if !h.is_animating() {
            return step - 1;
        }
        t += 16;
        h.advance(t);
    }
    panic!("dive never settled");
}

#[test]
fn f9_from_single_dives_out_to_the_fleet_then_settles() {
    // A live single view of "alpha", plus a second detached session to populate the
    // grid the dive pulls back over.
    let mut h = Harness::single("alpha", 80, 24, METRICS, (1400, 900));
    h.inject(UiEvent::SessionData {
        name: "alpha".into(),
        bytes: b"hello world\r\n".to_vec(),
        ended: false,
    });
    h.set_sessions(vec![info("alpha", true, 1), info("beta", false, 2)]);

    // Pressing F9 toggles toward the fleet. The shell emits Cmd::ListSessions, which
    // the harness answers, completing the grid and launching the dive-out — so the
    // animation is in flight immediately, with the camera framed on alpha (non-id).
    assert!(!h.is_animating(), "static before F9");
    h.inject(f9());
    assert!(h.is_animating(), "F9 launches the dive-out animation");
    assert!(
        camera_active(&h),
        "the dive frames the scene under a camera transform"
    );

    // Drive the tick stream to completion: it settles into the fleet, camera released.
    let steps = run_to_settle(&mut h);
    assert!(
        steps > 1,
        "the dive animated over several frames, not a snap ({steps})"
    );
    assert!(
        !camera_active(&h),
        "the settled fleet draws at identity (no lingering camera)"
    );
}

#[test]
fn f9_round_trips_single_to_fleet_to_single() {
    // Two live-ish sessions; adopt one so there's a single view to dive out of.
    let mut h = Harness::fleet(METRICS, (1400, 900), 1.0);
    h.set_sessions(vec![info("alpha", true, 1), info("beta", true, 2)]);
    for n in ["alpha", "beta"] {
        h.inject(UiEvent::AdoptSession(n.into()));
        h.inject(UiEvent::SessionData {
            name: n.into(),
            bytes: format!("{n} screen\r\n").into_bytes(),
            ended: false,
        });
    }
    h.inject(UiEvent::AdoptSession("alpha".into())); // land in alpha's single view
    run_to_settle(&mut h);

    // Out to the fleet…
    h.inject(f9());
    assert!(h.is_animating(), "dive-out launches");
    run_to_settle(&mut h);

    // …and back into the single view.
    h.inject(f9());
    assert!(h.is_animating(), "dive-in launches");
    let steps = run_to_settle(&mut h);
    assert!(steps > 1, "dive-in animated, not snapped ({steps})");
    assert!(!camera_active(&h), "single view settles at identity");
}
