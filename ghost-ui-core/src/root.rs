//! `RootModel` — the top of the model tree: either the single-terminal view or
//! the fleet overview, with one key (F9) toggling between them.
//!
//! Toggling preserves session state: every session this window drives stays
//! live. Going to the fleet hands the foreground *and* the warm background
//! mirrors to the grid as fed tiles; coming back extracts the chosen one as the
//! foreground and keeps the rest as warm mirrors (fed and resized like the
//! foreground), so previews never go cold and Ctrl-Tab switches are instant. The
//! shell drives this model exactly as it drove a bare `TerminalModel` — `update`
//! in, `Cmd`s out, `view` to draw — so the whole tree stays headlessly testable.

use std::collections::{HashMap, HashSet};

use crate::input::{Key, Mods, NamedKey};
use crate::terminal::{Shortcut, classify_shortcut};
use crate::{
    CellMetrics, Cmd, FleetModel, Scene, SceneItem, SessionId, TerminalModel, Transform, UiEvent,
};

enum Mode {
    Single(Box<TerminalModel>),
    Fleet(Box<FleetModel>),
}

/// Default duration of the fleet zoom in/out animation, in milliseconds. The shell
/// can override it per-window (see [`RootModel::set_anim_ms`]) — e.g. from the
/// `GHOST_DIVE_MS` env var — to slow the dive right down while validating it.
const ANIM_MS: u64 = 180;
/// Frame cadence while animating (~60 fps).
const ANIM_TICK_MS: u64 = 16;

/// An in-flight fleet zoom — a camera over the *fleet world*, interpolated from
/// `from` to `to` (one end is identity = the overview at rest, the other is a tile
/// filling the window) over `dur_ms` once the first tick stamps the start. The mode
/// swap is instant, so this never gates input or logical state — it's purely the
/// visual dive.
///
/// `world` carries a frozen snapshot of the fleet scene, rendered *under* the camera
/// for the whole dive (either direction): on a dive-in the mode is already single, so
/// there'd be no fleet to show otherwise; on a dive-out it freezes the grid against a
/// reconcile arriving mid-flight, so tiles don't reshuffle as we pull back.
struct Anim {
    from: Transform,
    to: Transform,
    current: Transform,
    /// The frozen fleet scene rendered under the camera for the dive's duration.
    world: Option<Scene>,
    /// The start time, stamped on the first tick; `None` until then.
    t0: Option<u64>,
    dur_ms: u64,
}

impl Anim {
    fn new(from: Transform, to: Transform, world: Option<Scene>, dur_ms: u64) -> Self {
        Anim {
            from,
            to,
            current: from,
            world,
            t0: None,
            dur_ms,
        }
    }

    /// Advance the camera to `now_ms`; returns true once the animation is done.
    /// Time is eased (ease-in-out) so the dive accelerates out of rest and settles
    /// gently instead of moving at a constant, mechanical rate.
    fn advance(&mut self, now_ms: u64) -> bool {
        let t0 = *self.t0.get_or_insert(now_ms);
        let elapsed = now_ms.saturating_sub(t0);
        if elapsed >= self.dur_ms {
            self.current = self.to;
            true
        } else {
            let p = ease_in_out(elapsed as f32 / self.dur_ms as f32);
            self.current = Transform::lerp(self.from, self.to, p);
            false
        }
    }

    /// How opaque the fleet *chrome* (everything but the terminal previews) should
    /// be at the current camera: fully shown at the overview end, faded to nothing
    /// as the dive reaches the tile, so a tile becomes a clean terminal rather than
    /// a giant card with buttons. Derived from the camera scale, so it follows the
    /// eased motion. Direction-agnostic (identity end = 1, zoomed-in end = 0).
    fn chrome_alpha(&self) -> f32 {
        let tile_scale = self.from.scale.max(self.to.scale);
        let fleet_scale = self.from.scale.min(self.to.scale);
        if tile_scale <= fleet_scale {
            return 1.0;
        }
        ((tile_scale - self.current.scale) / (tile_scale - fleet_scale)).clamp(0.0, 1.0)
    }
}

/// Cubic ease-in-out on `t` in [0, 1]: slow at both ends, fast in the middle, with
/// exact fixed points at 0 and 1.
fn ease_in_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        let f = -2.0 * t + 2.0;
        1.0 - f * f * f / 2.0
    }
}

pub struct RootModel {
    mode: Mode,
    metrics: CellMetrics,
    size_px: (u32, u32),
    /// Device scale factor, tracked so a fleet toggle preserves HiDPI sizing.
    scale: f32,
    /// Sessions this window owns, for fleet grouping (this-window vs elsewhere).
    mine: HashSet<SessionId>,
    /// The session the single view shows / a fleet toggle returns to. `None` for
    /// a freshly-opened fleet window that hasn't adopted a session yet.
    primary: Option<SessionId>,
    /// Live mirrors of the window's *background* sessions while in the single
    /// view (the foreground lives in `mode`). They are fed and resized exactly
    /// like the foreground, so their previews stay live and Ctrl-Tab switches are
    /// instant and correctly sized. In the fleet, the models live in its tiles, so
    /// this is empty.
    warm: HashMap<SessionId, TerminalModel>,
    /// A dive-out (single → fleet) waiting for the host's session list before it
    /// animates. F9 swaps to the fleet instantly for input, but the grid it builds
    /// only knows *this* window's sessions; the real fleet (foreign/detached tiles,
    /// final order) assembles from the `ListSessions` reply. So we hold the camera
    /// framed on this session (it keeps filling the window) until that reply lands,
    /// then launch the pull-back over the *complete* grid — every tile already in its
    /// final slot, nothing reshuffling at the end. Holds the session to frame.
    pending_dive: Option<SessionId>,
    /// A dive-IN (fleet → single) waiting for a cold tile's preview to load. Opening a
    /// detached session we don't yet drive would otherwise zoom an empty placeholder
    /// sized to the preview, not the window — landing tiny in the top-left with the
    /// contents popping in afterwards. So we size that session to the window, hold in
    /// the fleet until its first output makes the tile live, then dive into the now
    /// full-size, content-bearing preview. Holds the session being opened.
    pending_dive_in: Option<SessionId>,
    /// The in-flight fleet zoom, if any. Purely visual: the mode swap is instant,
    /// so this never affects logical state or input — `view` just renders the
    /// active scene under its camera until it completes.
    anim: Option<Anim>,
    /// Dive duration (ms). Defaults to [`ANIM_MS`]; the shell can slow it down for
    /// validation (kept here rather than read from the env so the core stays pure).
    anim_ms: u64,
}

/// Resize a model to the window (physical px + scale), returning its commands.
fn resize_model(m: &mut TerminalModel, size_px: (u32, u32), scale: f32) -> Vec<Cmd> {
    m.update(UiEvent::Resize {
        w_px: size_px.0.max(1),
        h_px: size_px.1.max(1),
        scale: scale as f64,
    })
}

/// F9 toggles the fleet overview.
fn is_fleet_toggle(key: &Key) -> bool {
    matches!(key, Key::Named(NamedKey::F9))
}

/// Ctrl-Tab / Ctrl-Shift-Tab cycle the window's foreground among its attached
/// sessions: `Some(true)` forward, `Some(false)` backward.
fn cycle_dir(key: &Key, mods: Mods) -> Option<bool> {
    if matches!(key, Key::Named(NamedKey::Tab)) && mods.ctrl {
        Some(!mods.shift)
    } else {
        None
    }
}

impl RootModel {
    /// Start in the single-terminal view around `model`.
    pub fn single(model: TerminalModel, metrics: CellMetrics, size_px: (u32, u32)) -> Self {
        let id = model.session().to_string();
        RootModel {
            mode: Mode::Single(Box::new(model)),
            metrics,
            size_px,
            scale: 1.0,
            mine: HashSet::from([id.clone()]),
            primary: Some(id),
            warm: HashMap::new(),
            pending_dive: None,
            pending_dive_in: None,
            anim: None,
            anim_ms: ANIM_MS,
        }
    }

    /// Start in the fleet overview owning no session — a freshly-opened window.
    /// The returned commands enumerate existing sessions to populate the grid
    /// (the reconcile reply re-arms the periodic refresh).
    pub fn fleet(metrics: CellMetrics, size_px: (u32, u32), scale: f32) -> (Self, Vec<Cmd>) {
        let root = RootModel {
            mode: Mode::Fleet(Box::new(FleetModel::new(metrics, size_px, HashSet::new()))),
            metrics,
            size_px,
            scale,
            mine: HashSet::new(),
            primary: None,
            warm: HashMap::new(),
            pending_dive: None,
            pending_dive_in: None,
            anim: None,
            anim_ms: ANIM_MS,
        };
        (root, vec![Cmd::ListSessions, Cmd::Redraw])
    }

    pub fn is_fleet(&self) -> bool {
        matches!(self.mode, Mode::Fleet(_))
    }

    /// Override the dive duration (ms) — e.g. the shell wiring `GHOST_DIVE_MS` to
    /// slow the animation right down for visual validation. Affects dives started
    /// after this call.
    pub fn set_anim_ms(&mut self, ms: u64) {
        self.anim_ms = ms;
    }

    /// Whether a fleet zoom animation is currently playing.
    pub fn is_animating(&self) -> bool {
        self.anim.is_some()
    }

    pub fn update(&mut self, ev: UiEvent) -> Vec<Cmd> {
        // While a zoom plays, the animation owns the tick stream (driving the
        // camera at ~60fps); it hands one tick back to the fleet on completion so
        // the periodic session refresh resumes.
        if let UiEvent::Tick { now_ms } = &ev
            && self.anim.is_some()
        {
            return self.tick_anim(*now_ms);
        }
        if let UiEvent::Key {
            key, mods, kind, ..
        } = &ev
            && kind.is_down()
        {
            if is_fleet_toggle(key) {
                return self.toggle();
            }
            if let Some(forward) = cycle_dir(key, *mods) {
                return self.cycle(forward);
            }
            // Window/app-level shortcuts are handled above the active view so
            // they work in either mode, even when the fleet has no focused tile.
            match classify_shortcut(key, *mods) {
                Some(Shortcut::Quit) => return vec![Cmd::Quit],
                Some(Shortcut::NewWindow) => return vec![Cmd::NewWindow],
                Some(Shortcut::CloseWindow) => return vec![Cmd::CloseWindow],
                Some(Shortcut::NewSession) => return vec![Cmd::SpawnSession],
                _ => {} // Copy/Paste/Zoom are per-terminal: delegate below.
            }
        }
        if let UiEvent::AdoptSession(id) = &ev {
            let id = id.clone();
            return self.adopt(id);
        }
        // Output for a background session keeps its warm mirror live (the fleet
        // owns every model, so this only matters in the single view).
        if let UiEvent::SessionData { name, .. } = &ev
            && let Mode::Single(m) = &self.mode
            && m.session() != name
        {
            return self.feed_warm(ev);
        }
        // In the fleet, feeding a tile can complete a deferred take-over: once the
        // session being opened produces its first output, its preview is live and
        // full-size, so dive into it now (re-entering adopt, which this time sees a
        // fed tile and animates).
        if let UiEvent::SessionData { name, .. } = &ev
            && matches!(self.mode, Mode::Fleet(_))
        {
            let name = name.clone();
            let mut cmds = match &mut self.mode {
                Mode::Fleet(f) => f.update(ev),
                Mode::Single(_) => unreachable!(),
            };
            if self.pending_dive_in.as_deref() == Some(name.as_str())
                && matches!(&self.mode, Mode::Fleet(f) if f.tile_fed(&name))
            {
                self.pending_dive_in = None;
                cmds.extend(self.adopt(name));
            }
            return cmds;
        }
        if let UiEvent::Resize { w_px, h_px, scale } = ev {
            self.size_px = (w_px, h_px);
            if scale > 0.0 {
                self.scale = scale as f32;
            }
            // Resize the foreground and every warm background mirror, so a
            // backgrounded session is never left at a stale size (its prompt or a
            // full-screen program like `top` would come back mis-laid-out).
            let mut cmds = match &mut self.mode {
                Mode::Single(m) => resize_model(m, self.size_px, self.scale),
                Mode::Fleet(f) => return f.update(UiEvent::Resize { w_px, h_px, scale }),
            };
            for m in self.warm.values_mut() {
                cmds.extend(resize_model(m, self.size_px, self.scale));
            }
            return cmds;
        }
        // The session list completes the fleet (foreign/detached tiles, final order).
        // If a dive-out was waiting on it, launch the pull-back now that the grid is
        // whole — every tile already in its final slot, so nothing reshuffles.
        if let UiEvent::SessionList(_) = &ev {
            let mut cmds = match &mut self.mode {
                Mode::Single(m) => m.update(ev),
                Mode::Fleet(f) => f.update(ev),
            };
            if let Some(p) = self.pending_dive.take() {
                cmds.extend(self.launch_dive_out(&p));
            }
            return cmds;
        }
        match &mut self.mode {
            Mode::Single(m) => m.update(ev),
            Mode::Fleet(f) => f.update(ev),
        }
    }

    /// Start the deferred dive-out pull-back over the now-complete fleet: zoom from
    /// the framed session (filling the window) back to the whole grid. A no-op if the
    /// session has no tile (e.g. it ended while we waited).
    fn launch_dive_out(&mut self, framed: &str) -> Vec<Cmd> {
        let Mode::Fleet(f) = &self.mode else {
            return Vec::new();
        };
        let Some(camera) = f.dive_camera(framed) else {
            return vec![Cmd::Redraw];
        };
        self.anim = Some(Anim::new(
            camera,
            Transform::IDENTITY,
            Some(f.view()),
            self.anim_ms,
        ));
        vec![Cmd::ScheduleTick { after_ms: 0 }]
    }

    /// Feed output to a background session's warm mirror, dropping the mirror if
    /// the session ended. Returns any replies the mirror produced (e.g. a program
    /// querying the terminal still gets answered while backgrounded).
    fn feed_warm(&mut self, ev: UiEvent) -> Vec<Cmd> {
        let UiEvent::SessionData { name, ended, .. } = &ev else {
            return Vec::new();
        };
        let (name, ended) = (name.clone(), *ended);
        let cmds = match self.warm.get_mut(&name) {
            Some(m) => m.update(ev),
            None => Vec::new(), // not a session this window mirrors
        };
        if ended {
            self.warm.remove(&name);
        }
        cmds
    }

    /// Switch to the single view of `id` (the shell has just attached it) and
    /// take ownership. From the fleet, the existing tile's screen is preserved;
    /// otherwise (or from another single session) a fresh terminal is created.
    /// The previously-shown session is NOT detached — the window keeps it warm so
    /// Ctrl-Tab and the fleet can switch back to it.
    fn adopt(&mut self, id: SessionId) -> Vec<Cmd> {
        // A new transition cancels any in-flight dive (a still-waiting dive-out, or an
        // animation that hasn't settled) so a stale camera/snapshot can't linger.
        self.pending_dive = None;
        self.pending_dive_in = None;
        self.anim = None;
        // Opening a cold tile (a detached session we don't yet drive): size it to the
        // window and hold in the fleet until its first output makes the preview live,
        // then re-enter to dive into the now full-size, content-bearing tile. The shell
        // has already begun attaching; the resize commands reach the session through it.
        if let Mode::Fleet(f) = &mut self.mode
            && let Some(mut cmds) = f.prepare_takeover(&id, self.size_px, self.scale)
        {
            // Don't claim ownership yet — the re-entry once the preview is live does
            // that. Leaving the tile foreign keeps it put if a reconcile lands first.
            self.pending_dive_in = Some(id);
            cmds.push(Cmd::Redraw);
            return cmds;
        }
        let placeholder = Mode::Single(Box::new(TerminalModel::new(
            String::new(),
            1,
            1,
            self.metrics,
        )));
        let dur = self.anim_ms;
        let current = std::mem::replace(&mut self.mode, placeholder);
        let mut anim = None;
        let (mut model, mut cmds) = match current {
            Mode::Fleet(f) => {
                // Opening a tile dives into where it sat in the grid: snapshot the
                // fleet world so the whole grid stays visible during the descent (a
                // freshly spawned session with no tile yet just opens, no dive).
                anim = f
                    .dive_camera(&id)
                    .map(|to| Anim::new(Transform::IDENTITY, to, Some(f.view()), dur));
                let (kept, warm, cmds) =
                    f.into_single_adopting(id.clone(), self.size_px, self.scale);
                // The window's other driven sessions stay warm in the background.
                for m in warm {
                    self.warm.insert(m.session().to_string(), m);
                }
                (kept, cmds)
            }
            Mode::Single(m) => {
                let old = m.session().to_string();
                if old == id {
                    (*m, Vec::new())
                } else {
                    // Stow the outgoing foreground as a warm mirror; restore the
                    // target's mirror if we have one (instant, no re-attach), else
                    // build a fresh model.
                    self.warm.insert(old, *m);
                    let model = self
                        .warm
                        .remove(&id)
                        .unwrap_or_else(|| TerminalModel::new(id.clone(), 1, 1, self.metrics));
                    (model, Vec::new())
                }
            }
        };
        // Size the (possibly restored or fresh) foreground to the window.
        cmds.extend(resize_model(&mut model, self.size_px, self.scale));
        self.mode = Mode::Single(Box::new(model));
        self.mine.insert(id.clone());
        self.primary = Some(id);
        cmds.push(Cmd::Redraw);
        if let Some(anim) = anim {
            self.anim = Some(anim);
            cmds.push(Cmd::ScheduleTick { after_ms: 0 });
        }
        cmds
    }

    /// Cycle the window's foreground among its attached sessions (Ctrl-Tab),
    /// resolving the target from the owned set in a stable order. The switch is a
    /// warm-mirror swap — no re-attach — so it's instant and correctly sized. A
    /// window with fewer than two sessions has nothing to cycle.
    fn cycle(&mut self, forward: bool) -> Vec<Cmd> {
        let mut names: Vec<&SessionId> = self.mine.iter().collect();
        names.sort();
        if names.len() < 2 {
            return Vec::new();
        }
        let cur = self
            .primary
            .as_ref()
            .and_then(|p| names.iter().position(|n| *n == p))
            .unwrap_or(0);
        let n = names.len();
        let next = if forward {
            (cur + 1) % n
        } else {
            (cur + n - 1) % n
        };
        let to = names[next].clone();
        if Some(&to) == self.primary.as_ref() {
            return Vec::new();
        }
        self.adopt(to)
    }

    pub fn view(&self) -> Scene {
        // A dive-out waiting on the session list: hold the camera framed on the
        // session we left (it keeps filling the window, as in the single view) until
        // the reply lands and the pull-back is launched. Chrome fully faded — we're
        // zoomed all the way in — matching the dive's zoomed-in end.
        if self.anim.is_none()
            && let Some(p) = &self.pending_dive
            && let Mode::Fleet(f) = &self.mode
            && let Some(camera) = f.dive_camera(p)
        {
            return Self::with_camera(f.view(), camera, 0.0);
        }

        // During a dive-in the mode is already single, so render the frozen fleet
        // snapshot the dive launched from; otherwise the live active view.
        let scene = match &self.anim {
            Some(Anim {
                world: Some(world), ..
            }) => world.clone(),
            _ => match &self.mode {
                Mode::Single(m) => m.view(),
                Mode::Fleet(f) => f.view(),
            },
        };
        // While zooming, render the whole world under the animation camera and fade
        // the fleet chrome (everything but the terminal previews) toward the tile,
        // so a card resolves into a clean terminal rather than a giant button bar.
        match &self.anim {
            Some(anim) => Self::with_camera(scene, anim.current, anim.chrome_alpha()),
            None => scene,
        }
    }

    /// Render a scene under a camera transform, fading the fleet chrome (everything
    /// but terminal previews and badges) by `chrome` so a card resolves into a clean
    /// terminal as the camera zooms in.
    fn with_camera(mut scene: Scene, camera: Transform, chrome: f32) -> Scene {
        for layer in &mut scene.layers {
            layer.transform = camera;
            if chrome < 1.0 {
                for item in &mut layer.items {
                    match item {
                        SceneItem::Terminal { .. } | SceneItem::Badge { .. } => {}
                        SceneItem::Rect { color, .. }
                        | SceneItem::Text { color, .. }
                        | SceneItem::Border { color, .. } => color[3] *= chrome,
                    }
                }
            }
        }
        scene
    }

    /// Combined render scale (device × zoom) of the active view, so the shell
    /// rasterizes glyphs at the size the current scene was laid out for.
    pub fn render_scale(&self) -> f32 {
        match &self.mode {
            Mode::Single(m) => m.render_scale(),
            Mode::Fleet(f) => f.render_scale(),
        }
    }

    /// Physical-pixel rect of the text cursor for the IME candidate window. Only
    /// the single view has a well-defined caret; the fleet overview returns None.
    pub fn ime_cursor_area(&self) -> Option<ghost_render::RectPx> {
        match &self.mode {
            Mode::Single(m) => m.ime_cursor_area(),
            Mode::Fleet(_) => None,
        }
    }

    /// Whether the app should exit: the single view's child ended. A fleet tile
    /// ending never quits the app.
    pub fn ended(&self) -> bool {
        match &self.mode {
            Mode::Single(m) => m.ended(),
            Mode::Fleet(_) => false,
        }
    }

    fn toggle(&mut self) -> Vec<Cmd> {
        // Swap the mode out behind a cheap placeholder so we can move the owned
        // model/fleet into the conversion.
        let placeholder = Mode::Single(Box::new(TerminalModel::new(
            String::new(),
            1,
            1,
            self.metrics,
        )));
        // A new transition cancels any in-flight dive (a still-waiting dive-out, an
        // animation that hasn't settled, or a take-over awaiting its preview) so a
        // stale camera/snapshot can't linger.
        self.pending_dive = None;
        self.pending_dive_in = None;
        self.anim = None;
        let dur = self.anim_ms;
        let current = std::mem::replace(&mut self.mode, placeholder);
        let (next, mut cmds, anim) = match current {
            Mode::Single(m) => {
                // Hand the foreground and every warm background mirror to the
                // fleet, so all of this window's previews are live, not cold.
                let warm: Vec<TerminalModel> = self.warm.drain().map(|(_, m)| m).collect();
                let (fleet, mut cmds) = FleetModel::adopting(
                    *m,
                    warm,
                    self.metrics,
                    self.size_px,
                    self.scale,
                    self.mine.clone(),
                );
                cmds.insert(0, Cmd::ListSessions); // fetch the complete grid
                // Dive out, but don't animate yet: the grid we just built only knows
                // this window's sessions. Wait for the ListSessions reply to assemble
                // the whole fleet (foreign/detached tiles, final order), then launch
                // the pull-back so it animates the ACTUAL result with nothing
                // reshuffling at the end. Until then `view` holds the camera framed on
                // this session (it keeps filling the window, as in the single view).
                self.pending_dive = self.primary.clone();
                (Mode::Fleet(Box::new(fleet)), cmds, None)
            }
            Mode::Fleet(f) => {
                // Dive in: snapshot the fleet world so the whole grid stays visible
                // while we descend into the tile we land on, then take over with the
                // live single view once the dive lands.
                let target = self
                    .primary
                    .clone()
                    .or_else(|| f.focused().map(str::to_string));
                let to = target.as_deref().and_then(|t| f.dive_camera(t));
                let anim = to.map(|to| Anim::new(Transform::IDENTITY, to, Some(f.view()), dur));
                let (model, warm, mut cmds) =
                    f.into_single_keeping(self.primary.clone(), self.size_px, self.scale);
                // The extracted session becomes the foreground; the rest of the
                // window's driven sessions stay warm in the background.
                for m in warm {
                    self.warm.insert(m.session().to_string(), m);
                }
                cmds.push(Cmd::Redraw);
                let id = model.session().to_string();
                self.mine.insert(id.clone());
                self.primary = Some(id);
                (Mode::Single(Box::new(model)), cmds, anim)
            }
        };
        self.mode = next;
        // Kick the (purely visual) dive: the first scheduled tick stamps its start.
        if let Some(anim) = anim {
            self.anim = Some(anim);
            cmds.push(Cmd::ScheduleTick { after_ms: 0 });
        }
        cmds
    }

    /// Advance the in-flight zoom on a clock tick: repaint (and schedule the next
    /// frame) while running; on completion clear the animation and hand one tick
    /// back to the fleet so its periodic session refresh resumes.
    fn tick_anim(&mut self, now_ms: u64) -> Vec<Cmd> {
        let Some(anim) = self.anim.as_mut() else {
            return Vec::new();
        };
        let done = anim.advance(now_ms);
        let mut cmds = vec![Cmd::Redraw];
        if done {
            self.anim = None;
            if let Mode::Fleet(f) = &mut self.mode {
                cmds.extend(f.update(UiEvent::Tick { now_ms }));
            }
        } else {
            cmds.push(Cmd::ScheduleTick {
                after_ms: ANIM_TICK_MS,
            });
        }
        cmds
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{KeyEventKind, Mods};

    const METRICS: CellMetrics = CellMetrics {
        advance: 9.0,
        line_height: 18.0,
    };
    const SIZE: (u32, u32) = (720, 432);

    fn root() -> RootModel {
        let m = TerminalModel::new("alpha".to_string(), 80, 24, METRICS);
        RootModel::single(m, METRICS, SIZE)
    }

    fn key(r: &mut RootModel, k: Key, mods: Mods) -> Vec<Cmd> {
        r.update(UiEvent::Key {
            key: k,
            mods,
            kind: KeyEventKind::Press,
            alts: None,
        })
    }

    fn sess(name: &str, attached: bool, created_at: i64) -> ghost_vt::session::SessionInfo {
        ghost_vt::session::SessionInfo {
            name: name.to_string(),
            pid: 1,
            created_at: Some(created_at),
            title: name.to_string(),
            command: vec![],
            attached,
            bell: false,
        }
    }

    /// F9 to dive out, then deliver the host's session list so the deferred dive
    /// launches over the complete fleet (mirrors the real flow). After this the dive
    /// is animating.
    fn dive_out(r: &mut RootModel, sessions: &[ghost_vt::session::SessionInfo]) {
        key(r, Key::Named(NamedKey::F9), Mods::NONE);
        r.update(UiEvent::SessionList(sessions.to_vec()));
    }

    #[test]
    fn single_delegates_text_to_the_terminal() {
        let mut r = root();
        assert_eq!(
            r.update(UiEvent::Text("a".into())),
            vec![Cmd::SendInput {
                session: "alpha".into(),
                bytes: b"a".to_vec()
            }]
        );
    }

    #[test]
    fn toggle_enters_fleet_and_lists_sessions() {
        let mut r = root();
        assert!(!r.is_fleet());
        let cmds = key(&mut r, Key::Named(NamedKey::F9), Mods::NONE);
        assert!(r.is_fleet());
        assert!(
            cmds.contains(&Cmd::ListSessions),
            "entering fleet enumerates sessions"
        );
    }

    #[test]
    fn toggle_round_trips_back_to_single() {
        let mut r = root();
        key(&mut r, Key::Named(NamedKey::F9), Mods::NONE);
        assert!(r.is_fleet());
        key(&mut r, Key::Named(NamedKey::F9), Mods::NONE);
        assert!(!r.is_fleet());
        // Back in single view, input is delegated to the (preserved) terminal.
        assert_eq!(
            r.update(UiEvent::Text("z".into())),
            vec![Cmd::SendInput {
                session: "alpha".into(),
                bytes: b"z".to_vec()
            }]
        );
    }

    #[test]
    fn toggle_back_targets_owned_session_after_focus_moved() {
        use crate::input::NamedKey;
        use ghost_vt::session::SessionInfo;
        fn info(name: &str, attached: bool) -> SessionInfo {
            SessionInfo {
                name: name.to_string(),
                pid: 1,
                created_at: None,
                title: name.to_string(),
                command: vec![],
                attached,
                bell: false,
            }
        }
        let mut r = root(); // owns "alpha"
        key(&mut r, Key::Named(NamedKey::F9), Mods::NONE); // -> fleet
        // The shell's ListSessions reply: our alpha plus a foreign detached beta.
        r.update(UiEvent::SessionList(vec![
            info("alpha", true),
            info("beta", false),
        ]));
        // Move focus onto the foreign tile (in the section below), then toggle back.
        r.update(UiEvent::Key {
            key: Key::Named(NamedKey::ArrowDown),
            mods: Mods::NONE,
            kind: KeyEventKind::Press,
            alts: None,
        });
        key(&mut r, Key::Named(NamedKey::F9), Mods::NONE); // -> single
        assert!(!r.is_fleet());
        // The single view returns to the OWNED session, not the focused foreign one.
        assert_eq!(
            r.update(UiEvent::Text("z".into())),
            vec![Cmd::SendInput {
                session: "alpha".into(),
                bytes: b"z".to_vec()
            }]
        );
    }

    #[test]
    fn fleet_toggle_preserves_device_scale() {
        use crate::SceneItem;
        let mut r = root();
        // HiDPI: a 2x surface. Cells double, so the grid halves and the rendered
        // frame carries the physical metrics.
        r.update(UiEvent::Resize {
            w_px: 720,
            h_px: 432,
            scale: 2.0,
        });
        // Round-trip through the fleet overview.
        key(&mut r, Key::Named(NamedKey::F9), Mods::NONE);
        key(&mut r, Key::Named(NamedKey::F9), Mods::NONE);
        assert!(!r.is_fleet());
        match r.view().terminals().next().unwrap() {
            SceneItem::Terminal { frame, .. } => {
                assert_eq!(
                    frame.metrics.advance, 18.0,
                    "single view still renders at 2x"
                );
            }
            _ => unreachable!(),
        }
    }

    /// Drive the animation clock to completion, returning the number of ticks fed.
    fn settle(r: &mut RootModel) -> u32 {
        let mut n = 0;
        let mut now = 10_000;
        while r.is_animating() && n < 1000 {
            r.update(UiEvent::Tick { now_ms: now });
            now += 16;
            n += 1;
        }
        n
    }

    /// The on-screen rect of the (single) terminal preview with the camera applied —
    /// i.e. what the renderer would actually draw. With one session there's exactly
    /// one terminal in the scene, so this is unambiguous.
    fn target_onscreen(r: &RootModel) -> crate::RectPx {
        let scene = r.view();
        scene
            .layers
            .iter()
            .flat_map(|l| l.items.iter().map(move |it| (l.transform, it)))
            .find_map(|(t, it)| match it {
                SceneItem::Terminal { rect, .. } => Some(t.apply_rect(*rect)),
                _ => None,
            })
            .expect("a terminal is on screen")
    }

    #[test]
    fn dive_geometry_zooms_the_target_between_its_tile_and_the_whole_window() {
        use crate::RectPx;
        let (w, h) = (1400.0f32, 900.0f32);
        // A "cover" framing fills the window (one dimension exact, the other spills).
        let covers =
            |r: RectPx| r.x <= 0.5 && r.y <= 0.5 && r.x + r.w >= w - 0.5 && r.y + r.h >= h - 0.5;
        let area = |r: RectPx| r.w * r.h;

        let mut r = root();
        r.update(UiEvent::Resize {
            w_px: w as u32,
            h_px: h as u32,
            scale: 1.0,
        });

        // Dive OUT (single → fleet): begins framed on the tile (filling the window)
        // and pulls back, so the on-screen target shrinks monotonically to a tile.
        // The dive launches once the session list arrives.
        dive_out(&mut r, &[sess("alpha", true, 1)]);
        let base = 10_000u64;
        let out: Vec<RectPx> = [0u64, 25, 50, 75]
            .iter()
            .map(|pct| {
                r.update(UiEvent::Tick {
                    now_ms: base + ANIM_MS * pct / 100,
                });
                target_onscreen(&r)
            })
            .collect();
        assert!(
            covers(out[0]),
            "dive-out begins with the tile filling the window: {:?}",
            out[0]
        );
        for pair in out.windows(2) {
            assert!(
                area(pair[1]) < area(pair[0]),
                "the target shrinks monotonically while pulling back: {out:?}"
            );
        }
        r.update(UiEvent::Tick {
            now_ms: base + 10_000,
        }); // settle
        let settled = target_onscreen(&r);
        assert!(
            !covers(settled) && area(settled) < w * h,
            "dive-out settles to a tile smaller than the window: {settled:?}"
        );

        // Dive IN (open the tile): the on-screen target grows monotonically back to
        // the whole window, landing in the single view.
        r.update(UiEvent::AdoptSession("alpha".to_string()));
        let base = 30_000u64;
        let inn: Vec<RectPx> = [0u64, 25, 50, 75]
            .iter()
            .map(|pct| {
                r.update(UiEvent::Tick {
                    now_ms: base + ANIM_MS * pct / 100,
                });
                target_onscreen(&r)
            })
            .collect();
        assert!(
            !covers(inn[0]),
            "dive-in begins from the small grid tile: {:?}",
            inn[0]
        );
        for pair in inn.windows(2) {
            assert!(
                area(pair[1]) > area(pair[0]),
                "the target grows monotonically while diving in: {inn:?}"
            );
        }
        r.update(UiEvent::Tick {
            now_ms: base + 10_000,
        }); // settle
        assert!(!r.is_fleet(), "dive-in lands in the single view");
        assert!(
            covers(target_onscreen(&r)),
            "and the landed target fills the window"
        );
    }

    #[test]
    fn f9_starts_a_zoom_animation_and_completes() {
        let mut r = root(); // single view of alpha (owned)
        let cmds = key(&mut r, Key::Named(NamedKey::F9), Mods::NONE);
        assert!(r.is_fleet(), "the mode swaps immediately");
        assert!(
            !r.is_animating(),
            "the dive waits for the session list before animating"
        );
        assert!(
            cmds.contains(&Cmd::ListSessions),
            "F9 fetches the complete grid first: {cmds:?}"
        );
        // The session list arrives: now the pull-back animation launches.
        let launched = r.update(UiEvent::SessionList(vec![sess("alpha", true, 1)]));
        assert!(r.is_animating(), "the session list launches the zoom");
        assert!(
            launched
                .iter()
                .any(|c| matches!(c, Cmd::ScheduleTick { .. })),
            "the animation is kicked by scheduling a tick: {launched:?}"
        );
        // A tick mid-flight re-arms the next frame and keeps animating.
        let mid = r.update(UiEvent::Tick { now_ms: 1_000 });
        assert!(r.is_animating(), "still animating shortly after the start");
        assert!(
            mid.iter().any(|c| matches!(c, Cmd::ScheduleTick { .. })),
            "an in-flight tick re-arms the next frame: {mid:?}"
        );
        // A tick past the duration completes the animation and stops re-arming.
        let done = r.update(UiEvent::Tick {
            now_ms: 1_000 + 10_000,
        });
        assert!(!r.is_animating(), "completes after its duration");
        assert!(
            done.contains(&Cmd::Redraw),
            "a final repaint settles it: {done:?}"
        );
        assert!(
            !done.iter().any(|c| matches!(c, Cmd::ScheduleTick { .. })),
            "a completed animation stops scheduling frames: {done:?}"
        );
    }

    #[test]
    fn dive_out_freezes_the_fleet_against_a_mid_dive_reconcile() {
        // Capture (which tile, where) — not just positions: a reorder swaps which
        // session sits at each position, so comparing bare rects would miss it.
        let tiles = |r: &RootModel| -> Vec<(crate::SceneId, crate::RectPx)> {
            r.view()
                .layers
                .iter()
                .flat_map(|l| &l.items)
                .filter_map(|it| match it {
                    SceneItem::Terminal { id, rect, .. } => Some((*id, *rect)),
                    _ => None,
                })
                .collect()
        };

        // Own two sessions; foreground = beta. Both fed so they render as live tiles.
        let mut r = root(); // single view of alpha
        r.update(UiEvent::AdoptSession("beta".to_string())); // foreground beta, alpha warm
        r.update(UiEvent::SessionData {
            name: "beta".to_string(),
            bytes: b"beta".to_vec(),
            ended: false,
        });
        r.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"alpha".to_vec(),
            ended: false,
        });
        // Dive out launches over the complete, stable grid once the list arrives.
        dive_out(&mut r, &[sess("alpha", true, 1), sess("beta", true, 2)]);
        r.update(UiEvent::Tick { now_ms: 1_000 }); // progress 0
        let before = tiles(&r);
        assert!(before.len() >= 2, "both sessions render as preview tiles");

        // A later reply that REVERSES the order would reshuffle the live fleet. The
        // dive renders a frozen snapshot, so each tile must stay put — otherwise a
        // different session slides under the camera mid-dive.
        r.update(UiEvent::SessionList(vec![
            sess("alpha", true, 9), // now newer
            sess("beta", true, 1),  // now older
        ]));
        assert_eq!(
            before,
            tiles(&r),
            "the dive renders a frozen snapshot, immune to a mid-dive reconcile"
        );
    }

    #[test]
    fn diving_back_out_keeps_the_settled_order_so_tiles_do_not_swap() {
        // Tiles left-to-right, by their stable SceneId::Tile(handle).
        let order = |r: &RootModel| -> Vec<crate::SceneId> {
            let mut ts: Vec<(f32, crate::SceneId)> = r
                .view()
                .layers
                .iter()
                .flat_map(|l| &l.items)
                .filter_map(|it| match it {
                    SceneItem::Terminal { id, rect, .. } => Some((rect.x, *id)),
                    _ => None,
                })
                .collect();
            ts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
            ts.into_iter().map(|(_, id)| id).collect()
        };

        // Window owns alpha (older) and beta (newer); beta is the foreground. Both
        // fed so they render as live preview tiles.
        let mut r = root(); // single view of alpha
        r.update(UiEvent::AdoptSession("beta".to_string()));
        for n in ["alpha", "beta"] {
            r.update(UiEvent::SessionData {
                name: n.to_string(),
                bytes: n.as_bytes().to_vec(),
                ended: false,
            });
        }
        // Dive out: launches over the complete, stable grid (oldest-first) once the
        // list arrives, so it animates the very order it will settle into.
        dive_out(&mut r, &[sess("alpha", true, 1), sess("beta", true, 2)]);
        let during = order(&r); // the order the dive animates
        // A further reply lands mid-dive, as the host's poll does.
        r.update(UiEvent::SessionList(vec![
            sess("alpha", true, 1),
            sess("beta", true, 2),
        ]));
        let mut t = 1_000_000;
        while r.is_animating() {
            r.update(UiEvent::Tick { now_ms: t });
            t += 100_000;
        }
        let settled = order(&r); // the order it lands in
        assert_eq!(
            during, settled,
            "dive-out must animate the same order it settles into — no end-of-dive swap"
        );
    }

    #[test]
    fn dive_out_waits_for_the_session_list_then_animates_the_complete_fleet() {
        // Distinct tiles (live previews AND placeholders) by stable handle.
        let tile_count = |r: &RootModel| -> usize {
            r.view()
                .layers
                .iter()
                .flat_map(|l| &l.items)
                .filter_map(|it| match it.id() {
                    crate::SceneId::Tile(h) => Some(h),
                    _ => None,
                })
                .collect::<std::collections::HashSet<_>>()
                .len()
        };

        let mut r = root(); // single view of alpha, mine = {alpha}
        r.update(UiEvent::Resize {
            w_px: 1400,
            h_px: 900,
            scale: 1.0,
        }); // roomy enough that all four tiles fit without scrolling
        // F9 dives out, but holds framed on alpha until the host's session list lands.
        let cmds = key(&mut r, Key::Named(NamedKey::F9), Mods::NONE);
        assert!(r.is_fleet(), "mode swaps for input immediately");
        assert!(!r.is_animating(), "the dive waits for the complete grid");
        assert!(
            cmds.contains(&Cmd::ListSessions),
            "F9 fetches the whole fleet: {cmds:?}"
        );

        // The reply: this window's attached alpha plus three detached foreign sessions
        // (the user's "1 attached + 3 detached" case).
        r.update(UiEvent::SessionList(vec![
            sess("alpha", true, 1),
            sess("x", false, 2),
            sess("y", false, 3),
            sess("z", false, 4),
        ]));
        assert!(r.is_animating(), "the dive launches once the grid is whole");
        assert_eq!(
            tile_count(&r),
            4,
            "the dive animates every session — detached tiles included, in final position"
        );
    }

    #[test]
    fn the_view_carries_the_camera_while_animating_then_settles_to_identity() {
        use crate::Transform;
        let mut r = root();
        r.update(UiEvent::Resize {
            w_px: 1000,
            h_px: 700,
            scale: 1.0,
        });
        dive_out(&mut r, &[sess("alpha", true, 1)]); // -> fleet, zoom-out launched
        r.update(UiEvent::Tick { now_ms: 5_000 }); // progress 0: camera = "from"
        let scene = r.view();
        assert!(
            scene
                .layers
                .iter()
                .any(|l| l.transform != Transform::IDENTITY),
            "mid-zoom the world renders under a non-identity camera"
        );
        settle(&mut r);
        let scene = r.view();
        assert!(
            scene
                .layers
                .iter()
                .all(|l| l.transform == Transform::IDENTITY),
            "after the zoom the fleet renders untransformed"
        );
    }

    #[test]
    fn ease_in_out_has_fixed_endpoints_and_is_monotonic() {
        assert_eq!(ease_in_out(0.0), 0.0);
        assert_eq!(ease_in_out(1.0), 1.0);
        assert!((ease_in_out(0.5) - 0.5).abs() < 1e-3, "symmetric midpoint");
        assert!(ease_in_out(0.25) < 0.25, "slow start (eased in)");
        assert!(ease_in_out(0.75) > 0.75, "slow end (eased out)");
        let mut prev = -1.0;
        for i in 0..=10 {
            let v = ease_in_out(i as f32 / 10.0);
            assert!(v >= prev, "monotonic non-decreasing");
            prev = v;
        }
    }

    #[test]
    fn chrome_fades_during_the_dive() {
        use crate::{SceneId, SceneItem};
        let mut r = root(); // single view of alpha (owned)
        dive_out(&mut r, &[sess("alpha", true, 1)]); // -> fleet, dive-out launched
        // Progress 0: the camera sits at the tile end, so the chrome is faded out.
        r.update(UiEvent::Tick { now_ms: 1_000 });
        let section_alpha = |r: &RootModel| {
            r.view()
                .layers
                .iter()
                .flat_map(|l| &l.items)
                .find_map(|it| match it {
                    SceneItem::Text {
                        id: SceneId::Section(_),
                        color,
                        ..
                    } => Some(color[3]),
                    _ => None,
                })
        };
        let during = section_alpha(&r).expect("a section header is present in the fleet");
        assert!(
            during < 0.99,
            "fleet chrome is faded during the dive (a={during})"
        );
        settle(&mut r);
        let rest = section_alpha(&r).expect("section header at rest");
        assert!(
            rest > 0.99,
            "chrome is fully opaque once the dive settles (a={rest})"
        );
    }

    #[test]
    fn dive_in_renders_the_frozen_fleet_world_until_it_lands() {
        use crate::{SceneId, SceneItem};
        let mut r = root(); // single view of alpha (owned)
        key(&mut r, Key::Named(NamedKey::F9), Mods::NONE); // -> fleet
        settle(&mut r); // finish the dive-out
        // F9 back: the mode swaps to single immediately, but while the dive plays
        // the *fleet world* is on screen (its section header proves it's the grid,
        // not the single terminal) — the symmetric grid-dive, both directions.
        key(&mut r, Key::Named(NamedKey::F9), Mods::NONE);
        assert!(!r.is_fleet(), "mode is single immediately");
        assert!(r.is_animating(), "a dive-in is playing");
        r.update(UiEvent::Tick { now_ms: 5_000 });
        let scene = r.view();
        let has_section_header = scene.layers.iter().flat_map(|l| &l.items).any(|it| {
            matches!(
                it,
                SceneItem::Text {
                    id: SceneId::Section(_),
                    ..
                }
            )
        });
        assert!(
            has_section_header,
            "the frozen fleet world (with its section header) renders during the dive"
        );
        // Once it lands, the real single view takes over: one terminal, no chrome.
        settle(&mut r);
        let scene = r.view();
        assert!(
            !scene
                .layers
                .iter()
                .flat_map(|l| &l.items)
                .any(|it| matches!(
                    it,
                    SceneItem::Text {
                        id: SceneId::Section(_),
                        ..
                    }
                )),
            "after landing, the single view (no fleet chrome) is shown"
        );
        assert_eq!(scene.terminals().count(), 1, "just the one terminal");
    }

    #[test]
    fn zoom_animation_does_not_block_input_routing() {
        // The animation is purely visual: the mode swaps instantly, so input still
        // routes to the freshly-shown session even while the camera is mid-flight.
        let mut r = root();
        dive_out(&mut r, &[sess("alpha", true, 1)]); // -> fleet (animating)
        assert!(r.is_animating());
        key(&mut r, Key::Named(NamedKey::F9), Mods::NONE); // -> single (animating)
        assert!(!r.is_fleet(), "mode is single immediately");
        assert_eq!(
            r.update(UiEvent::Text("z".into())),
            vec![Cmd::SendInput {
                session: "alpha".into(),
                bytes: b"z".to_vec()
            }],
            "text routes to the session even while the zoom plays"
        );
    }

    #[test]
    fn opening_a_fleet_tile_animates_the_zoom_in() {
        let mut r = root(); // owns alpha
        key(&mut r, Key::Named(NamedKey::F9), Mods::NONE); // -> fleet
        settle(&mut r);
        r.update(UiEvent::SessionList(vec![
            info("alpha", true),
            info("beta", false),
        ]));
        // The shell attaches a clicked tile, then replies AdoptSession. beta is a
        // cold detached tile, so the open waits for its preview to load; its first
        // output lands the dive into the single view.
        r.update(UiEvent::AdoptSession("beta".into()));
        let cmds = r.update(UiEvent::SessionData {
            name: "beta".into(),
            bytes: b"$ ".to_vec(),
            ended: false,
        });
        assert!(!r.is_fleet(), "adopting drops into the single view");
        assert!(r.is_animating(), "opening a tile plays a zoom-in");
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::ScheduleTick { .. })),
            "the zoom is kicked by scheduling a tick: {cmds:?}"
        );
    }

    #[test]
    fn adopting_a_session_without_a_tile_does_not_animate() {
        // A freshly spawned session has no tile in the grid yet, so there's nothing
        // to zoom from — it just opens.
        let mut r = root();
        key(&mut r, Key::Named(NamedKey::F9), Mods::NONE);
        settle(&mut r);
        let cmds = r.update(UiEvent::AdoptSession("gamma".into())); // never listed
        assert!(!r.is_animating(), "no tile to zoom from → no animation");
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::ScheduleTick { .. })),
            "no zoom scheduled: {cmds:?}"
        );
    }

    #[test]
    fn fleet_toggle_key_is_not_forwarded_as_input() {
        let mut r = root();
        // The toggle key must drive the app, never reach the child as bytes.
        let cmds = key(&mut r, Key::Named(NamedKey::F9), Mods::NONE);
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::SendInput { .. })));
        // A plain 'e' still types into the terminal.
        let mut r = root();
        assert!(matches!(
            key(&mut r, Key::Char("e".into()), Mods::NONE).as_slice(),
            [Cmd::SendInput { .. }]
        ));
    }

    #[test]
    fn f9_toggles_the_fleet_overview() {
        let mut r = root();
        assert!(!r.is_fleet());
        let cmds = key(&mut r, Key::Named(NamedKey::F9), Mods::NONE);
        assert!(r.is_fleet(), "F9 enters the fleet overview");
        assert!(
            cmds.contains(&Cmd::ListSessions),
            "entering the fleet enumerates sessions"
        );
        // F9 again returns to the single view.
        key(&mut r, Key::Named(NamedKey::F9), Mods::NONE);
        assert!(!r.is_fleet(), "F9 toggles back to the single view");
    }

    #[test]
    fn ctrl_shift_e_no_longer_toggles_the_fleet() {
        let mut r = root();
        let _ = key(&mut r, Key::Char("e".into()), Mods::CTRL | Mods::SHIFT);
        assert!(
            !r.is_fleet(),
            "Ctrl+Shift+E is no longer the fleet toggle (F9 is)"
        );
    }

    use ghost_vt::session::SessionInfo;
    fn info(name: &str, attached: bool) -> SessionInfo {
        SessionInfo {
            name: name.to_string(),
            pid: 1,
            created_at: None,
            title: name.to_string(),
            command: vec![],
            attached,
            bell: false,
        }
    }

    #[test]
    fn window_and_session_shortcuts_are_intercepted_above_the_view() {
        // Window management: Cmd+X on macOS, Ctrl+Shift+X elsewhere.
        for chord in [Mods::SUPER, Mods::CTRL | Mods::SHIFT] {
            let mut r = root();
            assert_eq!(
                key(&mut r, Key::Char("n".into()), chord),
                vec![Cmd::NewWindow]
            );
            assert_eq!(
                key(&mut r, Key::Char("w".into()), chord),
                vec![Cmd::CloseWindow]
            );
        }
        // New session is Cmd+T on macOS, Alt+T elsewhere.
        let new_session = if cfg!(target_os = "macos") {
            Mods::SUPER
        } else {
            Mods::ALT
        };
        let mut r = root();
        assert_eq!(
            key(&mut r, Key::Char("t".into()), new_session),
            vec![Cmd::SpawnSession]
        );
        // They also fire in the fleet overview, which may have no focused tile to
        // forward keys to — so they can't be left to the per-terminal path.
        let (mut f, _) = RootModel::fleet(METRICS, SIZE, 1.0);
        assert!(f.is_fleet());
        assert_eq!(
            key(&mut f, Key::Char("n".into()), Mods::SUPER),
            vec![Cmd::NewWindow]
        );
        assert_eq!(
            key(&mut f, Key::Char("t".into()), new_session),
            vec![Cmd::SpawnSession]
        );
        // Bare Ctrl+N (no Shift) is NOT a shortcut: it must reach the child.
        let mut r = root();
        assert!(matches!(
            key(&mut r, Key::Char("n".into()), Mods::CTRL).as_slice(),
            [Cmd::SendInput { .. }]
        ));
    }

    #[test]
    fn fleet_constructor_starts_in_the_overview_and_enumerates() {
        let (r, cmds) = RootModel::fleet(METRICS, SIZE, 1.0);
        assert!(r.is_fleet());
        assert!(
            cmds.contains(&Cmd::ListSessions),
            "a fresh fleet window enumerates sessions: {cmds:?}"
        );
    }

    fn feed(r: &mut RootModel, name: &str, bytes: &[u8]) -> Vec<Cmd> {
        r.update(UiEvent::SessionData {
            name: name.into(),
            bytes: bytes.to_vec(),
            ended: false,
        })
    }

    #[test]
    fn background_sessions_stay_live_and_keep_their_screens() {
        // The window drives two sessions; alpha starts foreground.
        let mut r = root(); // single view of alpha, mine = {alpha}
        // A tall window so both readable tiles are on screen at once (otherwise the
        // grid scrolls and culls the off-screen one — that is exercised in fleet's
        // own tests; here we only care that both are live, not "starting…").
        r.update(UiEvent::Resize {
            w_px: 720,
            h_px: 1200,
            scale: 1.0,
        });
        feed(&mut r, "alpha", b"alpha-screen");
        // Switch to beta (alpha goes to the background, kept warm).
        r.update(UiEvent::AdoptSession("beta".into()));
        feed(&mut r, "beta", b"beta-screen");
        // Background alpha keeps receiving output while beta is shown.
        feed(&mut r, "alpha", b" still-running");

        // Opening the fleet must show BOTH as live previews, not "starting…".
        key(&mut r, Key::Named(NamedKey::F9), Mods::NONE);
        assert!(r.is_fleet());
        assert_eq!(
            r.view().terminals().count(),
            2,
            "every session the window drives previews live"
        );

        // Switching back to alpha restores its (warm) screen and routes input to
        // it — a warm-mirror swap, no re-attach (no Attach/Spawn/TakeOver).
        key(&mut r, Key::Named(NamedKey::F9), Mods::NONE); // -> single (beta)
        let cmds = r.update(UiEvent::AdoptSession("alpha".into()));
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, Cmd::Attach(_) | Cmd::Spawn { .. } | Cmd::TakeOver(_))),
            "switching to a warm session needs no re-attach: {cmds:?}"
        );
        assert_eq!(
            r.update(UiEvent::Text("x".into())),
            vec![Cmd::SendInput {
                session: "alpha".into(),
                bytes: b"x".to_vec()
            }]
        );
    }

    #[test]
    fn refleeting_an_adopted_session_shows_a_live_preview_not_starting() {
        // A fleet-started window (owns nothing); the user attaches a detached
        // session, the shell feeds it, then the user reopens the fleet.
        let (mut r, _) = RootModel::fleet(METRICS, SIZE, 1.0);
        r.update(UiEvent::SessionList(vec![info("d", false)])); // detached
        r.update(UiEvent::AdoptSession("d".into())); // attach + show single
        r.update(UiEvent::SessionData {
            name: "d".into(),
            bytes: b"hello$ ".to_vec(),
            ended: false,
        });
        key(&mut r, Key::Named(NamedKey::F9), Mods::NONE); // back to the fleet
        assert!(r.is_fleet());
        // d is now a session this window drives, so its tile must be a live
        // preview (a Terminal in the scene) — never the "starting…" placeholder.
        assert_eq!(
            r.view().terminals().count(),
            1,
            "the adopted session previews live, not as a placeholder"
        );
    }

    #[test]
    fn opening_a_detached_session_loads_its_preview_before_diving() {
        // A fleet window (owns nothing) previewing a detached foreign session: its
        // tile is a cold placeholder with no live preview yet. The window is larger
        // than a preview, so taking the session over genuinely resizes it.
        let (mut r, _) = RootModel::fleet(METRICS, (1400, 900), 1.0);
        r.update(UiEvent::SessionList(vec![sess("d", false, 1)]));
        // Open it. The shell has begun attaching; this is its AdoptSession reply.
        let cmds = r.update(UiEvent::AdoptSession("d".into()));
        assert!(r.is_fleet(), "stays in the fleet while the preview loads");
        assert!(!r.is_animating(), "no dive yet — the preview is still cold");
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::Resize { session, .. } if session == "d")),
            "sizes the session to the window so the dive and single view are full-size: {cmds:?}"
        );
        // Its content arrives → the tile goes live → now it dives into the live
        // preview (with the contents already showing), zooming up to the full window.
        r.update(UiEvent::SessionData {
            name: "d".into(),
            bytes: b"user@host:~$ ".to_vec(),
            ended: false,
        });
        assert!(
            !r.is_fleet(),
            "dives into the session once its preview is live"
        );
        assert!(
            r.is_animating(),
            "the zoom plays, with content already on the preview"
        );
    }

    #[test]
    fn adopt_from_fleet_drops_into_that_sessions_single_view() {
        let mut r = root(); // owns alpha
        key(&mut r, Key::Named(NamedKey::F9), Mods::NONE); // -> fleet
        r.update(UiEvent::SessionList(vec![
            info("alpha", true),
            info("beta", false),
        ]));
        // What the shell sends after attaching a double-clicked / spawned session.
        // beta is a cold detached tile, so the open waits for its preview to load
        // (see opening_a_detached_session_…); feeding it lands the dive into single.
        let cmds = r.update(UiEvent::AdoptSession("beta".into()));
        r.update(UiEvent::SessionData {
            name: "beta".into(),
            bytes: b"$ ".to_vec(),
            ended: false,
        });
        assert!(
            !r.is_fleet(),
            "adopting leaves the overview once the preview loads"
        );
        assert!(cmds.contains(&Cmd::Redraw));
        // Input now routes to the adopted session.
        assert_eq!(
            r.update(UiEvent::Text("z".into())),
            vec![Cmd::SendInput {
                session: "beta".into(),
                bytes: b"z".to_vec()
            }]
        );
    }

    #[test]
    fn adopt_of_a_freshly_spawned_session_makes_a_new_terminal_and_keeps_previews() {
        let mut r = root(); // owns alpha
        key(&mut r, Key::Named(NamedKey::F9), Mods::NONE); // -> fleet
        r.update(UiEvent::SessionList(vec![
            info("alpha", true),
            info("beta", false),
        ]));
        // Adopt a session that is NOT a tile yet (just spawned by the shell).
        let cmds = r.update(UiEvent::AdoptSession("gamma".into()));
        assert!(!r.is_fleet());
        // Nothing detaches — the window keeps its sessions warm.
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Detach(_))),
            "previews stay attached: {cmds:?}"
        );
        assert_eq!(
            r.update(UiEvent::Text("z".into())),
            vec![Cmd::SendInput {
                session: "gamma".into(),
                bytes: b"z".to_vec()
            }]
        );
    }

    #[test]
    fn adopt_from_single_view_keeps_the_previous_session_attached() {
        let mut r = root(); // single view of alpha
        let cmds = r.update(UiEvent::AdoptSession("beta".into()));
        assert!(!r.is_fleet());
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Detach(_))),
            "the previous session stays attached (warm): {cmds:?}"
        );
        assert_eq!(
            r.update(UiEvent::Text("z".into())),
            vec![Cmd::SendInput {
                session: "beta".into(),
                bytes: b"z".to_vec()
            }]
        );
    }

    /// Adopt sessions into the window so it owns several, then return the sorted
    /// owned set the cycle walks.
    fn with_three(r: &mut RootModel) {
        r.update(UiEvent::AdoptSession("beta".into()));
        r.update(UiEvent::AdoptSession("gamma".into()));
        // Owns alpha, beta, gamma; foreground is gamma (last adopted).
    }

    fn ctrl_tab(r: &mut RootModel, shift: bool) -> Vec<Cmd> {
        let mods = if shift {
            Mods::CTRL | Mods::SHIFT
        } else {
            Mods::CTRL
        };
        key(r, Key::Named(NamedKey::Tab), mods)
    }

    /// The session the single view currently routes input to.
    fn foreground(r: &mut RootModel) -> String {
        match r.update(UiEvent::Text("x".into())).into_iter().next() {
            Some(Cmd::SendInput { session, .. }) => session,
            other => panic!("expected SendInput, got {other:?}"),
        }
    }

    #[test]
    fn ctrl_tab_cycles_to_the_next_attached_session() {
        let mut r = root(); // owns alpha
        with_three(&mut r); // -> alpha, beta, gamma (foreground gamma)
        // Forward from gamma wraps to alpha (sorted: alpha, beta, gamma); the
        // switch is a warm swap, not a re-attach.
        let cmds = ctrl_tab(&mut r, false);
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::SendInput { .. })));
        assert_eq!(
            foreground(&mut r),
            "alpha",
            "Ctrl-Tab advances the foreground"
        );
    }

    #[test]
    fn ctrl_shift_tab_cycles_to_the_previous_attached_session() {
        let mut r = root();
        with_three(&mut r); // foreground gamma
        ctrl_tab(&mut r, true);
        assert_eq!(
            foreground(&mut r),
            "beta",
            "Ctrl-Shift-Tab steps the foreground backward"
        );
    }

    #[test]
    fn ctrl_tab_with_a_single_session_is_a_noop() {
        let mut r = root(); // owns only alpha
        assert!(
            ctrl_tab(&mut r, false).is_empty(),
            "nothing to cycle with one session"
        );
        // And the Tab is not forwarded to the child as input.
        assert!(
            !ctrl_tab(&mut r, false)
                .iter()
                .any(|c| matches!(c, Cmd::SendInput { .. })),
        );
    }
}
