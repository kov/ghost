//! Frame-pacing bench harness — drives the **real** app through a scripted
//! single↔fleet dive so the pacing can be measured on the actual render+present
//! path (with [`framestats`](crate::framestats)), instead of a separate offscreen
//! reimplementation of the frame loop.
//!
//! It supplies synthetic input only: a fixed [`SessionInfo`] list (so the fleet
//! populates without a running host — `Cmd::ListSessions` is answered from here)
//! and a block of dense output per tile (so previews carry real raster cost). The
//! window, surface, model, damage, scene build, render and present are all the
//! live code. A small state machine then fires F9 / tile-select on a timer to
//! dive out and back, once per `GAP_MS`, for `dives` cycles, then exits.
//!
//! Enabled by `GHOST_BENCH=dive` (`GHOST_BENCH_SESSIONS`, `GHOST_BENCH_DIVES`).
//! Pair with `GHOST_FRAME_STATS=1` to print each dive's drop summary.

use ghost_ui_core::{KeyEventKind, UiEvent};
use ghost_vt::session::SessionInfo;

/// Milliseconds to hold between dives — long enough for the previous dive's
/// animation to finish, its settled frame to present (which flushes the frame-stats
/// summary), and the preview caches to be warm for the next one.
const GAP_MS: u64 = 500;

/// The F9 key event that toggles single↔fleet (a dive).
fn f9() -> UiEvent {
    UiEvent::Key {
        key: ghost_ui_core::Key::Named(ghost_ui_core::NamedKey::F9),
        mods: ghost_ui_core::Mods::NONE,
        kind: KeyEventKind::Press,
        alts: None,
    }
}

/// A block of dense, colourful text — enough lines to fill any plausible window
/// so each preview rasterises a full screen (the real per-tile cost), not a sliver.
fn dense_block() -> Vec<u8> {
    let mut s = String::new();
    for row in 0..200usize {
        s.push_str(&format!("\x1b[38;5;{}m", 16 + (row % 200)));
        for col in 0..220usize {
            s.push(char::from(b'!' + ((row * 7 + col * 3) % 90) as u8));
        }
        s.push_str("\r\n");
    }
    s.into_bytes()
}

/// What the harness wants the shell to do this step.
pub enum Action {
    /// Feed this event to the (single) bench window's model.
    Dispatch(UiEvent),
    /// The scripted dives are done — exit the process.
    Exit,
}

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    /// In the single view; the next step dives OUT to the fleet.
    Single,
    /// In the fleet; the next step dives IN to the target tile.
    Fleet,
    /// All scripted dives ran; the next step exits.
    Done,
}

/// Scripts a repeated single↔fleet dive against the real app.
pub struct Harness {
    names: Vec<String>,
    target: String,
    dives_left: usize,
    phase: Phase,
    /// Earliest clock (ms, the app's monotonic `now_ms`) at which the next dive may
    /// fire; gates the `GAP_MS` spacing between dives.
    next_at_ms: Option<u64>,
}

impl Harness {
    /// Build from the environment, or `None` if `GHOST_BENCH` isn't `dive`.
    pub fn from_env() -> Option<Self> {
        if std::env::var("GHOST_BENCH").ok().as_deref() != Some("dive") {
            return None;
        }
        let n = env_usize("GHOST_BENCH_SESSIONS", 4).clamp(1, 64);
        let dives = env_usize("GHOST_BENCH_DIVES", 4).max(1);
        let names: Vec<String> = (0..n).map(|i| format!("bench-{i}")).collect();
        let target = names[0].clone();
        eprintln!(
            "ghost bench: dive, {n} sessions, {dives} cycles (warm-up first); \
             set GHOST_FRAME_STATS=1 for per-dive drop summaries"
        );
        Some(Self {
            names,
            target,
            dives_left: dives,
            phase: Phase::Single,
            next_at_ms: None,
        })
    }

    /// The synthetic session list answering `Cmd::ListSessions`, so a reconcile
    /// keeps the fleet populated instead of clobbering it with the (empty) host.
    pub fn session_list(&self) -> Vec<SessionInfo> {
        self.names
            .iter()
            .enumerate()
            .map(|(i, n)| SessionInfo {
                name: n.clone(),
                pid: i as i32 + 1,
                created_at: Some(i as i64 + 1), // names[0] oldest → stable order
                title: String::new(),
                command: Vec::new(),
                attached: false,
                bell: false,
            })
            .collect()
    }

    /// Events to feed once the window exists: populate the fleet, give every tile a
    /// dense live preview, then settle into the target's single view (so the first
    /// scripted dive goes OUT). Mirrors the shell's own attach→feed→foreground path.
    pub fn setup_events(&self) -> Vec<UiEvent> {
        let mut evs = vec![UiEvent::SessionList(self.session_list())];
        for n in &self.names {
            evs.push(UiEvent::AdoptSession(n.clone()));
            evs.push(UiEvent::SessionData {
                name: n.clone(),
                bytes: dense_block(),
                ended: false,
            });
        }
        evs.push(UiEvent::SessionList(self.session_list()));
        evs.push(UiEvent::AdoptSession(self.target.clone()));
        evs
    }

    /// Advance the script. Called each loop turn with the app clock and whether the
    /// window is mid-animation; fires the next dive only once the previous one has
    /// settled and `GAP_MS` has elapsed.
    pub fn step(&mut self, now_ms: u64, animating: bool) -> Vec<Action> {
        if animating {
            return Vec::new(); // let the current dive finish
        }
        let due = self.next_at_ms.get_or_insert(now_ms + GAP_MS);
        if now_ms < *due {
            return Vec::new(); // hold the gap so the settled frame + summary land
        }
        self.next_at_ms = Some(now_ms + GAP_MS);
        match self.phase {
            Phase::Single => {
                self.phase = Phase::Fleet;
                // Dive OUT to the fleet; reconcile right after so the grid completes
                // and the dive-out launches (as the shell does after F9).
                vec![
                    Action::Dispatch(f9()),
                    Action::Dispatch(UiEvent::SessionList(self.session_list())),
                ]
            }
            Phase::Fleet => {
                self.dives_left -= 1;
                self.phase = if self.dives_left == 0 {
                    Phase::Done
                } else {
                    Phase::Single
                };
                // Dive IN to the target tile (fleet → single).
                vec![Action::Dispatch(UiEvent::AdoptSession(self.target.clone()))]
            }
            Phase::Done => vec![Action::Exit],
        }
    }
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
