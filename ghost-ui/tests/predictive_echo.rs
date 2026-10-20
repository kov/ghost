//! Predictive local echo over a slow link (see `docs/predictive-echo-design.md`).
//!
//! The real shell drives a session on a real loopback `sshd` whose session pipe
//! holds every byte back (`RealRemote::set_pipe_delay`), so each keystroke's echo
//! takes a long round trip. What the user sees is the drawn Scene, so that is what
//! these tests read: a prediction lives only in the drawing, never in the
//! session's replica.
//!
//! Skips (passes) cleanly where no `sshd` is installed.

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use ghost_ui::{App, EventSink, HeadlessFrontend, QueuedEvents};
use ghost_ui_core::{SessionId, UiEvent};
use ghost_vt::remote::RemoteSsh;
use support::remote::{RealRemote, retry_some};
use support::{drawn_terminal_text, sees_tile, visible_text, wait_until, with_isolated_xdg};

/// Each direction's hold on the session pipe; the round trip grows by twice it.
const PIPE_DELAY: Duration = Duration::from_millis(250);

/// Drive the loop the way winit does: run the App's once-per-wake pass, then
/// apply everything the background workers posted.
fn pump(app: &mut App, fe: &HeadlessFrontend, q: &QueuedEvents) {
    app.wake(fe);
    for ev in q.take() {
        app.on_user_event(fe, ev);
    }
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

/// The shell driving one remote session over the slow pipe.
struct Rig {
    app: App,
    fe: HeadlessFrontend,
    q: Arc<QueuedEvents>,
    wid: winit::window::WindowId,
    id: SessionId,
}

impl Rig {
    /// The session's lines as the host has echoed them (the replica).
    fn echoed(&self) -> String {
        self.app
            .states()
            .text_of(&self.id)
            .map(|lines| lines.join("\n"))
            .unwrap_or_default()
    }

    /// The window's text as drawn.
    fn drawn(&self) -> String {
        drawn_terminal_text(
            &self
                .app
                .root(self.wid)
                .expect("window")
                .view(self.app.states()),
        )
        .join("\n")
    }

    fn type_text(&mut self, text: &str) {
        self.app
            .dispatch(self.wid, UiEvent::Text(text.into()), &self.fe);
    }

    fn pump_until(&mut self, timeout: Duration, pred: impl FnMut(&Self) -> bool) -> bool {
        let mut pred = pred;
        wait_until(timeout, || {
            pump(&mut self.app, &self.fe, &self.q);
            pred(self)
        })
    }
}

/// Stand up the shell driving remote session `name`, whose child is `command`,
/// through a pipe delayed by [`PIPE_DELAY`]. Started straight on the command
/// rather than typed into a shell, so the test opens on a quiet screen with no
/// earlier typing still in flight.
fn rig(remote: &RealRemote, name: &str, command: &[&str]) -> Rig {
    let r = RemoteSsh::new_in(remote.spec(), remote.control_dir()).expect("open transport");
    let remote_ghost =
        retry_some(Duration::from_secs(10), || r.negotiate().ok()).expect("negotiate");
    // The fixture's remote is this machine under its own XDG dirs: its `ghost`
    // wrapper runs right here.
    let out = std::process::Command::new(remote.remote_ghost())
        .args(["new", "-d", name, "--"])
        .args(command)
        .output()
        .expect("run the remote ghost");
    assert!(
        out.status.success(),
        "remote `ghost new` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        wait_until(Duration::from_secs(5), || r
            .list_sessions(&remote_ghost)
            .map(|s| s.iter().any(|i| i.name == name))
            .unwrap_or(false)),
        "the session never came up on the remote"
    );
    // Every session pipe opened from here on is slow, including the display's.
    remote.set_pipe_delay(PIPE_DELAY);

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
        sees_tile(&root.view(app.states()), name)
    });
    assert!(
        discovered,
        "the remote session never appeared in the fleet: {:?}",
        visible_text(&app.root(wid).expect("window").view(app.states()))
    );

    // Drive it, the way the user does: click the card.
    let scene = app.root(wid).expect("window").view(app.states());
    let (x, y) = support::tile_center(&scene, name)
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
    let id = app
        .groups()
        .iter()
        .flat_map(|g| &g.members)
        .find(|m| m.name() == name)
        .expect("the window remembers its remote session")
        .clone();

    Rig {
        app,
        fe,
        q,
        wid,
        id,
    }
}

/// Once a line has proven it echoes, the next keys are drawn locally, before
/// the host's echo comes back. The first key of the line is not: like mosh,
/// prediction waits for one confirmed echo after every Enter.
#[test]
fn typed_keys_are_drawn_before_the_slow_echo_returns() {
    let Some(remote) = RealRemote::start() else {
        eprintln!("predictive_echo: no sshd available; skipping");
        return;
    };
    // SAFETY: process-global, held under the fixture's SERIAL lock.
    unsafe { std::env::set_var("GHOST_REMOTE_GHOST", remote.remote_ghost()) };

    with_isolated_xdg(|_tmp| {
        // The tty echoes each key; `cat` repeats each line.
        let mut rig = rig(&remote, "predict", &["cat"]);
        let round_trip = PIPE_DELAY * 2;

        // The first key waits for its echo — and the harness really is slow.
        let typed = Instant::now();
        rig.type_text("q");
        let echoed = rig.pump_until(Duration::from_secs(10), |rig| rig.echoed().contains("q"));
        assert!(echoed, "the first key never echoed: {:?}", rig.echoed());
        assert!(
            typed.elapsed() >= round_trip * 9 / 10,
            "the delayed pipe must make the echo take a round trip ({round_trip:?}); \
             it took {:?}",
            typed.elapsed()
        );

        // The second key is drawn before its own echo arrives: the window shows
        // `qw` at a moment the host has not echoed it yet.
        rig.type_text("w");
        let mut ahead = false;
        let shown = rig.pump_until(Duration::from_secs(10), |rig| {
            let drawn = rig.drawn().contains("qw");
            ahead |= drawn && !rig.echoed().contains("qw");
            drawn
        });
        assert!(shown, "the second key was never drawn: {:?}", rig.drawn());
        assert!(
            ahead,
            "the second key must be drawn before the host echoes it, not with the echo"
        );

        // With the line confirmed, each further key is drawn at once, well
        // inside one round trip.
        for (i, key) in ["e", "r", "t"].into_iter().enumerate() {
            let want = format!("qw{}", ["e", "er", "ert"][i]);
            let typed = Instant::now();
            rig.type_text(key);
            let shown = rig.pump_until(round_trip, |rig| rig.drawn().contains(&want));
            assert!(
                shown && typed.elapsed() < round_trip / 2,
                "{key:?} must be drawn immediately, not after the echo; drawn {:?} after {:?}",
                rig.drawn(),
                typed.elapsed()
            );
        }

        // And the host's echo agrees with what was drawn.
        let agreed = rig.pump_until(Duration::from_secs(10), |rig| {
            rig.echoed().contains("qwert")
        });
        assert!(agreed, "the host never echoed the line: {:?}", rig.echoed());
        assert!(rig.drawn().contains("qwert"));

        // Backspace is drawn at once too.
        let typed = Instant::now();
        rig.type_text("\x7f");
        let erased = rig.pump_until(round_trip, |rig| {
            let drawn = rig.drawn();
            drawn.contains("qwer") && !drawn.contains("qwert")
        });
        assert!(
            erased && typed.elapsed() < round_trip / 2,
            "backspace must be drawn immediately; drawn {:?} after {:?}",
            rig.drawn(),
            typed.elapsed()
        );
    });
}

/// Keys typed at a prompt that does not echo them — a password — are never
/// drawn, not even for a moment. Enter starts a new epoch, so the first key is
/// held back, its missing echo proves the guess wrong, and so on for every key.
#[test]
fn keys_at_a_password_prompt_are_never_drawn() {
    let Some(remote) = RealRemote::start() else {
        eprintln!("predictive_echo: no sshd available; skipping");
        return;
    };
    // SAFETY: process-global, held under the fixture's SERIAL lock.
    unsafe { std::env::set_var("GHOST_REMOTE_GHOST", remote.remote_ghost()) };

    with_isolated_xdg(|_tmp| {
        // One echoed line, then a silent prompt.
        let script = "head -n1 >/dev/null; stty -echo; printf 'Password: '; exec cat >/dev/null";
        let mut rig = rig(&remote, "secret", &["sh", "-c", script]);
        let round_trip = PIPE_DELAY * 2;

        // Prove the line echoes, so predictions are live when Enter is pressed.
        rig.type_text("a");
        assert!(rig.pump_until(Duration::from_secs(10), |rig| rig.echoed().contains('a')));
        rig.type_text("b");
        assert!(rig.pump_until(Duration::from_secs(10), |rig| rig.drawn().contains("ab")));
        rig.type_text("\r");
        assert!(
            rig.pump_until(Duration::from_secs(10), |rig| rig
                .echoed()
                .contains("Password:")),
            "the prompt never came: {:?}",
            rig.echoed()
        );

        // The prompt's row must only ever read `Password:`.
        let prompt_row = |rig: &Rig| {
            rig.drawn()
                .lines()
                .find(|l| l.contains("Password:"))
                .map(|l| l.trim_end().to_string())
        };
        for key in ["h", "u", "n", "t", "e", "r", "2"] {
            rig.type_text(key);
            let leaked = rig.pump_until(round_trip + PIPE_DELAY, |rig| {
                prompt_row(rig).is_none_or(|row| row != "Password:")
            });
            assert!(
                !leaked,
                "a key typed at the password prompt was drawn: {:?}",
                rig.drawn()
            );
        }
    });
}
