//! Frame-pacing bench harness — drives the **real** app through a scripted
//! animation so the pacing can be measured on the actual render+present path (with
//! [`framestats`](crate::framestats)), instead of a separate offscreen
//! reimplementation of the frame loop. Three scripts:
//!
//! - `GHOST_BENCH=dive` — repeated single↔fleet dives (F9 out, tile-select in).
//! - `GHOST_BENCH=slide` — repeated Ctrl-Tab cycles between the window's attached
//!   sessions (the `RootModel` slide). The same setup adopts every session into one
//!   window, so Ctrl-Tab has somewhere to go.
//! - `GHOST_BENCH=stream` — continuous bulk output to the foreground (a fresh
//!   screenful per turn), measuring the full-window redraw + present path (the
//!   `cat bigfile` workload), reported as build/present percentiles over a run.
//!
//! It supplies synthetic input only: a fixed [`SessionInfo`] list (so the fleet
//! populates without a running host — `Cmd::ListSessions` is answered from here)
//! and a block of dense output per session (so its preview / slide frame carries
//! real raster cost). The window, surface, model, damage, scene build, render and
//! present are all the live code. A small state machine then fires the scripted
//! input on a timer, once per `GAP_MS`, for `cycles` iterations, then exits.
//!
//! Counts come from `GHOST_BENCH_SESSIONS` and `GHOST_BENCH_CYCLES` (the legacy
//! `GHOST_BENCH_DIVES` is still honoured). Pair with `GHOST_FRAME_STATS=1` to print
//! each animation's drop summary.

use ghost_ui_core::{KeyEventKind, UiEvent};
use ghost_vt::session::SessionInfo;

/// Milliseconds to hold between animations — long enough for the previous one's
/// frames to finish, its settled frame to present (which flushes the frame-stats
/// summary), and the caches to be warm for the next one.
const GAP_MS: u64 = 500;

/// Which animation the bench scripts.
#[derive(Clone, Copy, PartialEq)]
enum Bench {
    /// Repeated single↔fleet dives (F9 out, tile-select in).
    Dive,
    /// Repeated Ctrl-Tab cycles between the window's attached sessions.
    Slide,
    /// Continuous bulk output to the foreground (a screenful per turn), measuring the
    /// full-window redraw + present path — the `cat bigfile` workload.
    Stream,
}

/// The F9 key event that toggles single↔fleet (a dive).
fn f9() -> UiEvent {
    UiEvent::Key {
        key: ghost_ui_core::Key::Named(ghost_ui_core::NamedKey::F9),
        mods: ghost_ui_core::Mods::NONE,
        kind: KeyEventKind::Press,
        alts: None,
    }
}

/// The Ctrl-Tab key event that cycles the foreground to the next session (a slide).
fn ctrl_tab() -> UiEvent {
    UiEvent::Key {
        key: ghost_ui_core::Key::Named(ghost_ui_core::NamedKey::Tab),
        mods: ghost_ui_core::Mods::CTRL,
        kind: KeyEventKind::Press,
        alts: None,
    }
}

/// A block of dense, colourful text — enough lines to fill any plausible window
/// so each preview rasterises a full screen (the real per-tile cost), not a sliver.
///
/// `GHOST_BENCH_HEAVY=1` recolours every other column instead of once per row, so each
/// row becomes ~110 style runs (each its own `String`) rather than one — the run-density
/// of colorized `ls` output vs. plain text. It changes NOTHING the compositor renders
/// (the surface is cached and blitted the same way); it only stresses per-frame content
/// handling, so it isolates whether animation cost is content-INDEPENDENT as intended.
/// `tick` shifts the colour ramp and glyph pattern so consecutive blocks render a
/// *different* screenful — otherwise the deterministic block leaves the visible
/// screen byte-identical after each feed and the damage layer (correctly) skips the
/// present. A moving `tick` mimics `cat bigfile` scrolling genuinely-new content in,
/// which is the full-window-redraw workload the stream bench exists to measure.
fn dense_block(tick: usize) -> Vec<u8> {
    let heavy = std::env::var_os("GHOST_BENCH_HEAVY").is_some();
    let mut s = String::new();
    for row in 0..200usize {
        s.push_str(&format!("\x1b[38;5;{}m", 16 + ((row + tick) % 200)));
        for col in 0..220usize {
            if heavy && col % 2 == 0 {
                s.push_str(&format!("\x1b[38;5;{}m", 16 + ((row + col + tick) % 200)));
            }
            s.push(char::from(b'!' + ((row * 7 + col * 3 + tick) % 90) as u8));
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

/// Scripts a repeated animation (dive or slide) against the real app.
pub struct Harness {
    mode: Bench,
    names: Vec<String>,
    target: String,
    cycles_left: usize,
    phase: Phase,
    /// Earliest clock (ms, the app's monotonic `now_ms`) at which the next animation
    /// may fire; gates the `GAP_MS` spacing between them.
    next_at_ms: Option<u64>,
    /// Stream mode: presented-frame samples `(build_ms, present_ms)`, and how many
    /// leading (cold-cache) frames to discard before measuring.
    stream_samples: Vec<(f32, f32)>,
    stream_warmup_left: usize,
    stream_measure_left: usize,
    /// Monotonic feed counter for stream mode; shifts each `dense_block` so the
    /// screen genuinely changes frame-to-frame (see [`dense_block`]).
    stream_tick: usize,
}

impl Harness {
    /// Build from the environment, or `None` unless `GHOST_BENCH` is
    /// `dive`/`slide`/`stream`.
    pub fn from_env() -> Option<Self> {
        let mode = match std::env::var("GHOST_BENCH").ok().as_deref() {
            Some("dive") => Bench::Dive,
            Some("slide") => Bench::Slide,
            Some("stream") => Bench::Stream,
            _ => return None,
        };
        // A slide needs at least two sessions to have somewhere to cycle to.
        let floor = match mode {
            Bench::Slide => 2,
            Bench::Dive | Bench::Stream => 1,
        };
        let n = env_usize("GHOST_BENCH_SESSIONS", 4).clamp(floor, 64);
        let cycles = env_usize("GHOST_BENCH_CYCLES", env_usize("GHOST_BENCH_DIVES", 4)).max(1);
        let names: Vec<String> = (0..n).map(|i| format!("bench-{i}")).collect();
        let target = names[0].clone();
        let label = match mode {
            Bench::Dive => "dive",
            Bench::Slide => "slide",
            Bench::Stream => "stream",
        };
        // Stream measures presented frames, not cycles; default to a longer run.
        let stream_measure = env_usize("GHOST_BENCH_FRAMES", 240).max(1);
        let stream_warmup = env_usize("GHOST_BENCH_WARMUP", 30);
        if mode == Bench::Stream {
            eprintln!(
                "ghost bench: stream, {stream_measure} frames ({stream_warmup} warm-up \
                 discarded); GHOST_BENCH_HEAVY recolours for run-density"
            );
        } else {
            eprintln!(
                "ghost bench: {label}, {n} sessions, {cycles} cycles (warm-up first); \
                 set GHOST_FRAME_STATS=1 for per-animation drop summaries"
            );
        }
        Some(Self {
            mode,
            names,
            target,
            cycles_left: cycles,
            phase: Phase::Single,
            next_at_ms: None,
            stream_samples: Vec::new(),
            stream_warmup_left: stream_warmup,
            stream_measure_left: stream_measure,
            stream_tick: 0,
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

    /// Events to feed once the window exists: populate the fleet, give every session
    /// a dense live preview, then settle into the target's single view. Mirrors the
    /// shell's own attach→feed→foreground path. The dive then starts from the single
    /// view (first toggle goes OUT); the slide starts with every session adopted, so
    /// Ctrl-Tab can cycle among them.
    pub fn setup_events(&self) -> Vec<UiEvent> {
        let mut evs = vec![UiEvent::SessionList(self.session_list())];
        for n in &self.names {
            evs.push(UiEvent::AdoptSession(n.clone()));
            evs.push(UiEvent::SessionData {
                name: n.clone(),
                bytes: dense_block(0),
                ended: false,
            });
        }
        evs.push(UiEvent::SessionList(self.session_list()));
        evs.push(UiEvent::AdoptSession(self.target.clone()));
        evs
    }

    /// Advance the script. Called each loop turn with the app clock and whether the
    /// window is mid-animation; fires the next animation only once the previous one
    /// has settled and `GAP_MS` has elapsed.
    pub fn step(&mut self, now_ms: u64, animating: bool) -> Vec<Action> {
        // Stream feeds a fresh screenful every turn (no animation gate, no GAP) so each
        // present is a full bulk-output redraw; collection + exit are driven by
        // `record_stream_present` from the present path.
        if self.mode == Bench::Stream {
            self.stream_tick += 1;
            return vec![Action::Dispatch(UiEvent::SessionData {
                name: self.target.clone(),
                bytes: dense_block(self.stream_tick),
                ended: false,
            })];
        }
        if animating {
            return Vec::new(); // let the current animation finish
        }
        let due = self.next_at_ms.get_or_insert(now_ms + GAP_MS);
        if now_ms < *due {
            return Vec::new(); // hold the gap so the settled frame + summary land
        }
        self.next_at_ms = Some(now_ms + GAP_MS);
        match self.mode {
            Bench::Dive => self.step_dive(),
            Bench::Slide => self.step_slide(),
            Bench::Stream => unreachable!("stream handled above"),
        }
    }

    /// Record one presented bulk-output frame (stream mode only). Discards the leading
    /// warm-up frames, accumulates the rest, and returns `true` — after printing the
    /// summary — once the run is complete, so the caller exits. A no-op otherwise.
    pub fn record_stream_present(
        &mut self,
        build: std::time::Duration,
        present: std::time::Duration,
    ) -> bool {
        if self.mode != Bench::Stream {
            return false;
        }
        if self.stream_warmup_left > 0 {
            self.stream_warmup_left -= 1;
            return false;
        }
        if self.stream_measure_left == 0 {
            return false;
        }
        self.stream_samples
            .push((build.as_secs_f32() * 1000.0, present.as_secs_f32() * 1000.0));
        self.stream_measure_left -= 1;
        if self.stream_measure_left == 0 {
            self.print_stream_summary();
            return true;
        }
        false
    }

    fn print_stream_summary(&self) {
        let pct = |mut v: Vec<f32>, p: f32| -> f32 {
            if v.is_empty() {
                return 0.0;
            }
            v.sort_by(f32::total_cmp);
            v[((v.len() as f32 * p) as usize).min(v.len() - 1)]
        };
        let mean = |v: &[f32]| -> f32 {
            if v.is_empty() {
                0.0
            } else {
                v.iter().sum::<f32>() / v.len() as f32
            }
        };
        let builds: Vec<f32> = self.stream_samples.iter().map(|s| s.0).collect();
        let presents: Vec<f32> = self.stream_samples.iter().map(|s| s.1).collect();
        eprintln!(
            "ghost bench stream: {} frames | build avg {:.2} p50 {:.2} p90 {:.2} worst {:.2} ms \
             | present avg {:.2} p90 {:.2} worst {:.2} ms",
            self.stream_samples.len(),
            mean(&builds),
            pct(builds.clone(), 0.5),
            pct(builds.clone(), 0.9),
            builds.iter().copied().fold(0.0, f32::max),
            mean(&presents),
            pct(presents.clone(), 0.9),
            presents.iter().copied().fold(0.0, f32::max),
        );
    }

    /// One dive step: out to the fleet, then in to the target tile, counting a cycle
    /// per round-trip.
    fn step_dive(&mut self) -> Vec<Action> {
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
                self.cycles_left -= 1;
                self.phase = if self.cycles_left == 0 {
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

    /// One slide step: a single Ctrl-Tab to the next attached session, counting a
    /// cycle per slide.
    fn step_slide(&mut self) -> Vec<Action> {
        if self.cycles_left == 0 {
            return vec![Action::Exit];
        }
        self.cycles_left -= 1;
        vec![Action::Dispatch(ctrl_tab())]
    }
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
