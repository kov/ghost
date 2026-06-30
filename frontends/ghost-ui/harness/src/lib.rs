//! Headless driver of the **real** ghost-ui frontend, for tests and benchmarks.
//!
//! It owns the actual [`RootModel`] (the shell's whole behaviour) and feeds it
//! injected [`UiEvent`]s, executing the effectful [`Cmd`]s the model returns the
//! way the windowed shell does — only synthetically: a `ListSessions` is answered
//! from a fixed list, a `SendInput` is recorded, a `ScheduleTick` arms the clock,
//! and so on. No winit, no PTYs. The model→cmd→tick→`view`→damage→render path is
//! therefore exercised exactly as the window runs it, but offscreen and
//! deterministically, so a test can assert on the resulting [`Scene`] (or, with the
//! GPU, the rendered pixels) and a benchmark can drive an animation frame by frame.
//!
//! This is the single driver behind both the dive benchmark and the frontend's
//! end-to-end tests — there is no second, hand-rolled frame loop to drift from the
//! real one. The renderer is created lazily, so a pure behaviour test (assert on
//! [`Harness::scene`] / [`Harness::is_animating`]) needs no GPU at all; only
//! [`Harness::render`] / [`Harness::present`] touch wgpu.

pub mod framestats;

use ghost_render::{CellMetrics, Scene};
use ghost_renderer::{Damage, Rendered, Renderer, SceneCache, Theme};
use ghost_shaper::FontRef;
use ghost_ui_core::{Cmd, RootModel, SessionId, TerminalModel, UiEvent};
use ghost_vt::session::SessionInfo;

/// Bundled monospace font, so offscreen rendering is deterministic and self-contained.
const FIRA: &[u8] = include_bytes!("../../shaper/tests/assets/FiraCode-Regular.ttf");

/// Base font size in px (device scale is applied on top via `render_scale`),
/// matching the windowed shell's `SIZE_PX`.
pub const SIZE_PX: f32 = 15.0;

/// Drives the real frontend headlessly. Construct with [`Harness::fleet`] or
/// [`Harness::single`], feed [`UiEvent`]s with [`inject`](Self::inject), advance the
/// animation clock with [`advance`](Self::advance), and assert on
/// [`scene`](Self::scene) / [`render`](Self::render).
pub struct Harness {
    root: RootModel,
    /// Created on first render; absent for pure behaviour tests (no GPU needed).
    renderer: Option<Renderer>,
    cache: SceneCache,
    font: FontRef<'static>,
    /// The synthetic session list answering `Cmd::ListSessions`.
    sessions: Vec<SessionInfo>,
    /// Every `Cmd::SendInput` the model emitted, for input assertions.
    sent: Vec<(SessionId, Vec<u8>)>,
    /// The next armed `Cmd::ScheduleTick` deadline (model-clock ms), if any.
    next_tick_ms: Option<u64>,
    /// Current model clock (advanced by [`advance`](Self::advance)).
    clock_ms: u64,
    /// Set once the model returns `Cmd::Quit`/`Cmd::CloseWindow`.
    quit: bool,
}

impl Harness {
    /// A fleet-overview frontend at `size_px`/`scale`, with no sessions yet.
    pub fn fleet(metrics: CellMetrics, size_px: (u32, u32), scale: f32) -> Self {
        let (root, cmds) = RootModel::fleet(metrics, size_px, scale);
        let mut h = Self::wrap(root);
        h.exec(cmds);
        h.inject(UiEvent::Resize {
            w_px: size_px.0,
            h_px: size_px.1,
            scale: scale as f64,
        });
        h
    }

    /// A single-view frontend showing `name` at `cols`×`rows`.
    pub fn single(
        name: &str,
        cols: u16,
        rows: u16,
        metrics: CellMetrics,
        size_px: (u32, u32),
    ) -> Self {
        let model = TerminalModel::new(name.to_string(), cols, rows, metrics);
        let mut h = Self::wrap(RootModel::single(model, metrics, size_px));
        h.inject(UiEvent::Resize {
            w_px: size_px.0,
            h_px: size_px.1,
            scale: 1.0,
        });
        h
    }

    fn wrap(root: RootModel) -> Self {
        Self {
            root,
            renderer: None,
            cache: SceneCache::default(),
            font: ghost_shaper::font_from_bytes(FIRA).expect("bundled font loads"),
            sessions: Vec::new(),
            sent: Vec::new(),
            next_tick_ms: None,
            clock_ms: 0,
            quit: false,
        }
    }

    /// Set the synthetic session list that answers `Cmd::ListSessions` (and feed it
    /// in now, as a reconcile would). Use [`SessionInfo`]s the model will bucket.
    pub fn set_sessions(&mut self, sessions: Vec<SessionInfo>) {
        self.sessions = sessions.clone();
        self.inject(UiEvent::SessionList(sessions));
    }

    /// Feed one event to the model and execute the effects it returns.
    pub fn inject(&mut self, ev: UiEvent) {
        let cmds = self.root.update(ev);
        self.exec(cmds);
    }

    /// Synthetic interpreter for the model's effects: the headless analogue of the
    /// shell's `exec`. Reads are answered from in-memory state, the clock is armed,
    /// input is recorded; effects with no headless meaning (real PTY/clipboard/
    /// window I/O) are dropped — they don't shape the model/render path under test.
    fn exec(&mut self, cmds: Vec<Cmd>) {
        for cmd in cmds {
            match cmd {
                Cmd::SendInput { session, bytes } => self.sent.push((session, bytes)),
                Cmd::ListSessions => {
                    let more = self
                        .root
                        .update(UiEvent::SessionList(self.sessions.clone()));
                    self.exec(more);
                }
                Cmd::ScheduleTick { after_ms } => {
                    self.next_tick_ms = Some(self.clock_ms + after_ms);
                }
                Cmd::Quit | Cmd::CloseWindow => self.quit = true,
                // No headless analogue (real session/clipboard/window I/O, repaint).
                _ => {}
            }
        }
    }

    /// Advance the model clock to `now_ms`, firing every `ScheduleTick` that has come
    /// due (each may re-arm the next) — this is what drives an animation forward, the
    /// way the shell's tick loop does.
    pub fn advance(&mut self, now_ms: u64) {
        self.clock_ms = now_ms;
        // A re-arm always schedules strictly ahead, so this terminates; the guard is
        // a backstop against a degenerate 0-delay reschedule loop.
        let mut guard = 0;
        while self.next_tick_ms.is_some_and(|t| t <= now_ms) && guard < 10_000 {
            guard += 1;
            self.next_tick_ms = None;
            self.inject(UiEvent::Tick { now_ms });
        }
    }

    /// The model's current `Scene` — the thing the renderer draws, and what a
    /// behaviour test asserts on.
    pub fn scene(&self) -> Scene {
        self.root.view()
    }

    /// Whether an animation (e.g. a dive) is in flight.
    pub fn is_animating(&self) -> bool {
        self.root.is_animating()
    }

    /// Whether the model has asked to exit.
    pub fn quit_requested(&self) -> bool {
        self.quit
    }

    /// Every `SendInput` the model emitted, oldest first (keys/paste/replies).
    pub fn sent_input(&self) -> &[(SessionId, Vec<u8>)] {
        &self.sent
    }

    /// The render font size (base × device scale), as the shell computes it.
    pub fn render_px(&self) -> f32 {
        SIZE_PX * self.root.render_scale()
    }

    fn renderer(&mut self) -> &mut Renderer {
        self.renderer
            .get_or_insert_with(|| Renderer::headless(Theme::default()))
    }

    /// Render the current scene offscreen and read back its pixels — for tests that
    /// assert on what was drawn. Needs a GPU (lazily creates the renderer).
    pub fn render(&mut self) -> Rendered {
        let scene = self.root.view();
        let px = self.render_px();
        let font = self.font;
        self.renderer().render_offscreen_scene(&scene, font, px)
    }

    /// Render the current scene through the live damage path into the renderer's
    /// cached target (no pixel read-back), returning the [`Damage`] the shell would
    /// have acted on — the faithful per-frame work a benchmark measures. Needs a GPU.
    pub fn present(&mut self) -> Damage {
        let scene = self.root.view();
        let px = self.render_px();
        let font = self.font;
        let damage = self.cache.damage(&scene, px);
        let band = match damage {
            Damage::Band(b) => Some(b),
            _ => None,
        };
        self.renderer()
            .render_to_cached_target(&scene, font, px, band);
        damage
    }

    /// Count of preview textures the renderer has (re)rasterised — a benchmark/test
    /// hook for the fleet preview cache. Zero until something has rendered.
    pub fn preview_renders(&self) -> u32 {
        self.renderer.as_ref().map_or(0, Renderer::preview_renders)
    }
}
