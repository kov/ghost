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
use crate::text_input::TextInput;
use crate::{
    CellMetrics, Cmd, FleetModel, Layer, PointPx, PointerButton, PointerPhase, RectPx, Run, Scene,
    SceneId, SceneItem, SessionId, Style, TerminalModel, Transform, UiEvent,
};
use ghost_vt::connection::ConnectionSpec;
use ghost_vt::query::ThemeColors;

enum Mode {
    Single(Box<TerminalModel>),
    Fleet(Box<FleetModel>),
}

/// Default duration of the UI animations (the fleet zoom and the session slide),
/// in milliseconds. The shell can override it per-window (see
/// [`RootModel::set_anim_ms`]) — e.g. from the `GHOST_ANIM_MS` env var — to slow
/// the animations right down while validating them.
const ANIM_MS: u64 = 180;
/// Frame cadence while animating (~60 fps).
const ANIM_TICK_MS: u64 = 16;

/// An in-flight UI animation — a transform timeline over a few **frozen** content
/// layers. Each [`AnimLayer`] carries a snapshot scene and a `from`→`to` transform;
/// [`Anim::scene`] interpolates them at the current eased progress and stacks the
/// results into one frame. The model swap is always instant, so an animation never
/// gates input or logical state — it's purely visual, and the renderer composites the
/// frozen content as textures rather than re-rasterizing it every frame.
///
/// Two shapes are built today, but the timeline and the renderer are both
/// animation-agnostic, so a new effect is just a new set of layers:
///
/// - a fleet **dive** ([`Anim::dive`]) — one layer holding the frozen fleet world,
///   zoomed by a camera lerped between the overview (identity) and a tile filling the
///   window, with its chrome faded toward the zoomed-in end. Freezing the world keeps
///   tiles from reshuffling if a reconcile lands mid-dive, and gives a dive-in (whose
///   mode is already single) a fleet to pull back to.
/// - a session **slide** ([`Anim::slide`]) — two single-view layers translated past
///   each other so the outgoing session leaves one edge as the incoming one arrives
///   from the other. The frozen outgoing scene is a stable stand-in even if its shell
///   just exited.
struct Anim {
    layers: Vec<AnimLayer>,
    /// Output frame size (the window); every layer composes into this.
    size_px: (u32, u32),
    /// Start time, stamped on the first tick; `None` until then.
    t0: Option<u64>,
    dur_ms: u64,
    /// Eased progress in `[0, 1]`, advanced each tick.
    p: f32,
}

/// One frozen layer of an [`Anim`]: `content` carried from `from` to `to` across the
/// animation. `fade_chrome` dissolves the non-terminal items as the layer zooms in
/// (the dive's card→terminal resolve); a slide layer leaves it off.
struct AnimLayer {
    content: Scene,
    from: Transform,
    to: Transform,
    fade_chrome: bool,
}

impl Anim {
    /// A fleet zoom: the frozen `world` under a camera lerped `from`→`to`, chrome
    /// fading toward the zoomed-in end so a card resolves into a clean terminal.
    fn dive(mut world: Scene, from: Transform, to: Transform, dur_ms: u64) -> Self {
        let size_px = world.size_px;
        freeze_damage(&mut world);
        Anim {
            layers: vec![AnimLayer {
                content: world,
                from,
                to,
                fade_chrome: true,
            }],
            size_px,
            t0: None,
            dur_ms,
            p: 0.0,
        }
    }

    /// A horizontal slide between two single-view sessions: the outgoing leaves one
    /// edge (`+1` dir → incoming arrives from the right, the "next" direction) as the
    /// incoming arrives from the other. Both sides are full-window [`SceneId::Root`]
    /// terminals, but they carry distinct sessions, so the renderer caches each side's
    /// texture independently (keyed by session, not role).
    fn slide(mut outgoing: Scene, mut incoming: Scene, dir: f32, dur_ms: u64) -> Self {
        let size_px = outgoing.size_px;
        freeze_damage(&mut outgoing);
        freeze_damage(&mut incoming);
        let w = size_px.0 as f32;
        let translate = |tx| Transform {
            scale: 1.0,
            tx,
            ty: 0.0,
        };
        Anim {
            layers: vec![
                AnimLayer {
                    content: outgoing,
                    from: Transform::IDENTITY,
                    to: translate(-dir * w),
                    fade_chrome: false,
                },
                AnimLayer {
                    content: incoming,
                    from: translate(dir * w),
                    to: Transform::IDENTITY,
                    fade_chrome: false,
                },
            ],
            size_px,
            t0: None,
            dur_ms,
            p: 0.0,
        }
    }

    /// Advance to `now_ms`; returns true once the animation is done. Time is eased
    /// (ease-in-out) so motion accelerates out of rest and settles gently instead of
    /// moving at a constant, mechanical rate.
    fn advance(&mut self, now_ms: u64) -> bool {
        let t0 = *self.t0.get_or_insert(now_ms);
        let elapsed = now_ms.saturating_sub(t0);
        if elapsed >= self.dur_ms {
            self.p = 1.0;
            true
        } else {
            self.p = ease_in_out(elapsed as f32 / self.dur_ms as f32);
            false
        }
    }

    /// The composed frame at the current progress: each layer's frozen content under
    /// its interpolated transform (chrome faded for a zooming layer), stacked low→high
    /// in declaration order.
    fn scene(&self) -> Scene {
        let mut out = Scene::new(self.size_px);
        for layer in &self.layers {
            let camera = Transform::lerp(layer.from, layer.to, self.p);
            let chrome = if layer.fade_chrome {
                chrome_alpha(layer.from, layer.to, camera)
            } else {
                1.0
            };
            out.layers
                .extend(with_camera(layer.content.clone(), camera, chrome).layers);
        }
        out
    }
}

/// Freeze a scene's terminals as unchanged for an animation: the SAME frozen content
/// replays every tick, so each session's Surface must be rendered once (when it first
/// appears) and then reused, never re-rastered per frame. `TermDamage::None` tells the
/// renderer exactly that; a not-yet-rendered session's Surface is still absent, so the
/// renderer falls back to a full render for its first frame regardless.
fn freeze_damage(scene: &mut Scene) {
    for layer in &mut scene.layers {
        for item in &mut layer.items {
            if let SceneItem::Terminal { damage, .. } = item {
                *damage = crate::TermDamage::None;
            }
        }
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

/// How opaque the fleet *chrome* (everything but the terminal previews) should be at
/// the current camera: fully shown at the overview end, faded to nothing as the camera
/// reaches the tile, so a tile becomes a clean terminal rather than a giant card with
/// buttons. Derived from the camera scale, so it follows the eased motion.
/// Direction-agnostic (identity end = 1, zoomed-in end = 0).
fn chrome_alpha(from: Transform, to: Transform, current: Transform) -> f32 {
    let tile_scale = from.scale.max(to.scale);
    let fleet_scale = from.scale.min(to.scale);
    if tile_scale <= fleet_scale {
        return 1.0;
    }
    ((tile_scale - current.scale) / (tile_scale - fleet_scale)).clamp(0.0, 1.0)
}

/// Place a frozen `scene` under a `camera` transform, fading the fleet chrome
/// (everything but terminal previews and badges) by `chrome`. Replaces each layer's
/// own transform with `camera` — single-view layers carry identity, so this is a plain
/// set; the fleet world's layers are already expressed in world space, so the camera
/// is the whole transform there too.
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

pub struct RootModel {
    mode: Mode,
    metrics: CellMetrics,
    /// The scheme's default fg/bg (OSC 10/11 color-query replies), stamped on
    /// every model this root creates or holds. Set once by the shell after
    /// construction (the theme is fixed at startup today).
    theme: ThemeColors,
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
    /// The in-flight animation (a fleet dive or a session slide), if any. Purely
    /// visual: the mode swap is instant, so this never affects logical state or
    /// input — `view` just renders [`Anim::scene`] until it completes.
    anim: Option<Anim>,
    /// Dive duration (ms). Defaults to [`ANIM_MS`]; the shell can slow it down for
    /// validation (kept here rather than read from the env so the core stays pure).
    anim_ms: u64,
    /// Whether this window currently has OS focus (from `UiEvent::Focus`).
    /// Drives the live-bell reaction: a bell in an owned session while the
    /// window is unfocused asks the OS for attention.
    focused_win: bool,
    /// The group registry. The fleet model owns the live editing copy while
    /// open; this carries it across fleet close/reopen (the fleet is rebuilt
    /// each opening) and receives the shell's authoritative
    /// [`UiEvent::GroupsLoaded`] (startup load, cross-window broadcasts).
    groups: Vec<crate::Group>,
    /// This window's group identity (id, name, color), minted by the shell at
    /// window creation. Its registry entry tracks the sessions the window
    /// drives plus the dead ones it remembers; the fleet syncs it from its
    /// tiles while open, and [`Self::claim_member`] keeps it fed from the
    /// single view's adopts.
    my_group: crate::Group,
    /// Inner padding (logical px per side) for the foreground terminal — a small,
    /// DPI-scaled border filled with the terminal background so content doesn't crowd
    /// the window edges. Applied to the foreground/warm models and folded into
    /// [`Self::grid`] so the attach handshake matches. 0 = flush (the historic look).
    pad: f32,
    /// When set, this window is showing the "connect to a host" prompt (a new
    /// ssh window before its first session): it swallows the keyboard into the
    /// entry and renders the prompt overlay instead of the live view.
    connect: Option<ConnectPrompt>,
}

/// The stage of the "connect to a host over SSH" flow — a small state machine
/// the shell drives as ssh auth progresses. The password field only appears when
/// ssh actually asks (the [`Password`](ConnectPhase::Password) phase), in place
/// of the host field.
enum ConnectPhase {
    /// Typing the `[user@]host` (the initial phase).
    Host,
    /// The host was submitted; auth/setup is in flight, nothing to type yet.
    /// `status` reports what it's doing (plain connecting, or staging the binary
    /// with a byte count) so the overlay can show a progress bar.
    Connecting { status: ConnectStatus },
    /// ssh asked for a secret (`prompt` is its wording, e.g. a passphrase); the
    /// masked password field is shown in place of the host.
    Password { prompt: String },
    /// Auth failed; `message` is shown and Enter returns to the host field.
    Error { message: String },
}

/// What the [`Connecting`](ConnectPhase::Connecting) phase is currently doing.
enum ConnectStatus {
    /// Authenticating / negotiating — no measurable progress to show.
    Working,
    /// Copying the ghost binary to the remote: `sent` of `total` bytes.
    Staging { sent: u64, total: u64 },
}

/// What a resolved connect prompt produces — and how Escape backs out of it.
#[derive(Clone, Copy, PartialEq)]
enum ConnectTarget {
    /// A new ssh *window* (Cmd+S): submit makes this (freshly opened, empty)
    /// window an ssh group; Escape closes it.
    Window,
    /// A new ssh *session* in the current window (Cmd+G): submit adopts the
    /// remote session as an additional tab and leaves the window a normal group;
    /// Escape just dismisses the prompt, returning to the existing session.
    Session,
}

/// The modal scale of the connect overlay, shared by its rendering
/// ([`RootModel::connect_scene`]) and its hit-testing
/// ([`RootModel::connect_error_rect`]) so a click lands on the drawn text.
const CONNECT_SCALE: f32 = 1.5;

/// How long (ms) the "Copied" confirmation flashes after the error is copied.
const COPIED_FLASH_MS: u64 = 1_200;

/// The transient "Copied" confirmation shown after the error is lifted to the
/// clipboard. A copy is triggered by a key/click that carries no timestamp, so
/// the flash *arms* immediately (shown at once) and the next tick — the clock —
/// stamps its expiry deadline; it clears on the first tick past that deadline.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CopiedFlash {
    /// Just copied; awaiting a tick to stamp the deadline from a fresh clock.
    Arming,
    /// Shown until this monotonic-ms deadline.
    Until(u64),
}

/// The hint under a connect error names the copy chord for the platform (a
/// click on the message copies it too). ⌘C on macOS; Ctrl+Shift+C elsewhere,
/// matching [`classify_shortcut`]'s copy chord.
const CONNECT_ERROR_HINT: &str = if cfg!(target_os = "macos") {
    "Enter to retry · Esc to cancel · ⌘C or click to copy"
} else {
    "Enter to retry · Esc to cancel · Ctrl+Shift+C or click to copy"
};

/// The "connect to a host over SSH" prompt state.
struct ConnectPrompt {
    host: TextInput,
    password: TextInput,
    phase: ConnectPhase,
    /// Whether submitting opens a new window or a session in this one, and what
    /// Escape does — set when the prompt is opened, preserved across retries.
    target: ConnectTarget,
    /// The transient "Copied" flash, present while it shows (see [`CopiedFlash`]).
    copied: Option<CopiedFlash>,
}

impl ConnectPrompt {
    fn new(target: ConnectTarget) -> Self {
        ConnectPrompt {
            host: TextInput::new(String::new()),
            password: TextInput::new(String::new()),
            phase: ConnectPhase::Host,
            target,
            copied: None,
        }
    }
}

/// Resize a model to the window (physical px + scale), first stamping the inner
/// `pad` (logical px) so the model insets its grid, returning its commands.
fn resize_model(m: &mut TerminalModel, size_px: (u32, u32), scale: f32, pad: f32) -> Vec<Cmd> {
    m.set_padding(pad);
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
            theme: ThemeColors::default(),
            size_px,
            scale: 1.0,
            mine: HashSet::from([id.clone()]),
            primary: Some(id),
            warm: HashMap::new(),
            pending_dive: None,
            pending_dive_in: None,
            anim: None,
            anim_ms: ANIM_MS,
            focused_win: true,
            groups: Vec::new(),
            my_group: crate::Group::auto(String::new(), 0),
            pad: 0.0,
            connect: None,
        }
    }

    /// Start in the fleet overview owning no session — a freshly-opened window.
    /// The returned commands enumerate existing sessions to populate the grid
    /// (the reconcile reply re-arms the periodic refresh).
    pub fn fleet(metrics: CellMetrics, size_px: (u32, u32), scale: f32) -> (Self, Vec<Cmd>) {
        let root = RootModel {
            mode: Mode::Fleet(Box::new(FleetModel::new(metrics, size_px, HashSet::new()))),
            metrics,
            theme: ThemeColors::default(),
            size_px,
            scale,
            mine: HashSet::new(),
            primary: None,
            warm: HashMap::new(),
            pending_dive: None,
            pending_dive_in: None,
            anim: None,
            anim_ms: ANIM_MS,
            focused_win: true,
            groups: Vec::new(),
            my_group: crate::Group::auto(String::new(), 0),
            pad: 0.0,
            connect: None,
        };
        (root, vec![Cmd::ListSessions, Cmd::Redraw])
    }

    pub fn is_fleet(&self) -> bool {
        matches!(self.mode, Mode::Fleet(_))
    }

    /// Set the scheme's default fg/bg (OSC 10/11 color-query replies) on every
    /// model this root holds now or creates later. Returns the mode-2031
    /// dark/light notifications a real change owes subscribed sessions.
    pub fn set_theme(&mut self, theme: ThemeColors) -> Vec<Cmd> {
        self.theme = theme;
        let mut cmds = match &mut self.mode {
            Mode::Single(m) => m.set_theme(theme),
            Mode::Fleet(f) => f.set_theme(theme),
        };
        for m in self.warm.values_mut() {
            cmds.extend(m.set_theme(theme));
        }
        cmds
    }

    /// Adopt this window's group identity (minted by the shell at window
    /// creation), handing it to an already-open fleet too. Sessions the
    /// window already drives join the group on the spot — the returned save
    /// persists them.
    pub fn set_my_group(&mut self, group: crate::Group) -> Vec<Cmd> {
        self.my_group = group.clone();
        if let Mode::Fleet(f) = &mut self.mode {
            f.set_my_group(group);
        }
        let mine: Vec<SessionId> = self.mine.iter().cloned().collect();
        let mut save = Vec::new();
        for id in mine {
            let cmds = self.claim_member(&id);
            if !cmds.is_empty() {
                save = cmds; // each claim re-saves the whole registry; keep the last
            }
        }
        save
    }

    /// Ensure `id` is a member of this window's group — the single-view twin
    /// of the fleet's tile sync — persisting the registry when it changes.
    /// Ownership moved here, so the session also leaves every other group;
    /// an emptied one dissolves.
    fn claim_member(&mut self, id: &str) -> Vec<Cmd> {
        let mut changed = false;
        for g in &mut self.groups {
            if g.id != self.my_group.id && g.members.iter().any(|m| m == id) {
                g.members.retain(|m| m != id);
                changed = true;
            }
        }
        if changed {
            self.groups
                .retain(|g| g.id == self.my_group.id || !g.members.is_empty());
        }
        match self.groups.iter_mut().find(|g| g.id == self.my_group.id) {
            Some(g) if g.members.iter().any(|m| m == id) => {
                if !changed {
                    return Vec::new();
                }
            }
            Some(g) => g.members.push(id.to_string()),
            None => {
                let mut g = self.my_group.clone();
                g.members = vec![id.to_string()];
                self.groups.push(g);
            }
        }
        vec![Cmd::SaveGroups(self.groups.clone())]
    }

    /// The identity this window's attaches report (embedding its group id) —
    /// read fresh by the shell at every attach, since opening a closed group
    /// can rebind the group.
    pub fn client_identity(&self) -> String {
        crate::group::window_identity(&self.my_group.id)
    }

    /// The fleet's per-tile preview-frame cache stats, if a fleet is present (`None`
    /// in the single view). Read by tests and emitted by [`Self::emit_cache_trace`].
    pub fn fleet_frame_cache(&self) -> Option<ghost_render::CacheCounters> {
        match &self.mode {
            Mode::Fleet(f) => Some(f.frame_cache()),
            Mode::Single(_) => None,
        }
    }

    /// Emit the model-side cache hit-rates to `tracing` (target `ghost::cache`), to sit
    /// alongside the renderer's line under `RUST_LOG=ghost::cache=trace`. Free unless
    /// the target is enabled. Call once per presented frame.
    pub fn emit_cache_trace(&self) {
        if let Some(frames) = self.fleet_frame_cache() {
            tracing::trace!(
                target: "ghost::cache",
                fleet_frame_hit_rate = frames.hit_rate(),
                "fleet frames {frames}",
            );
        }
    }

    /// The window's terminal grid in cells at its current pixel size and device
    /// scale — the size a session this window shows is laid out at, and the size
    /// the shell must complete an attach handshake at.
    ///
    /// Attaching at anything else (e.g. a fixed provisional 80×24) makes the host
    /// lay out its resync there: a full-size screen is reflowed down and its
    /// cursor pinned to that smaller bottom row, and a later resize back up can't
    /// recover it — the next output lands mid-screen. So the shell reads this and
    /// hands the host its real geometry up front. Matches the per-cell math a
    /// freshly-adopted [`TerminalModel`] uses when it resizes itself to the window
    /// (device scale, zoom 1), so the handshake size and the model's own first
    /// resize agree and the host never reflows.
    pub fn grid(&self) -> (u16, u16) {
        let advance = self.metrics.advance * self.scale;
        let line_height = self.metrics.line_height * self.scale;
        // Inset by the padding (physical px) so the handshake grid matches the
        // foreground model, which lays out inside the same border.
        let pad = self.pad * self.scale;
        let content_w = (self.size_px.0 as f32 - 2.0 * pad).max(0.0);
        let content_h = (self.size_px.1 as f32 - 2.0 * pad).max(0.0);
        let cols = (content_w / advance).floor().max(1.0) as u16;
        let rows = (content_h / line_height).floor().max(1.0) as u16;
        (cols, rows)
    }

    /// Snapshot this window's restorable state for the workspace file: its group
    /// identity, the grid it is sized to, its view mode, the foreground session,
    /// and the set it drives (sorted so the file is stable). See
    /// [`crate::workspace`].
    /// The window's foreground session (shown in single view / the dive target),
    /// if any — the session a new terminal inherits its connection from.
    pub fn foreground(&self) -> Option<&SessionId> {
        self.primary.as_ref()
    }

    /// The window group's own connection, if it is an explicit "ssh group" — the
    /// default every new session in the window inherits (winning over the
    /// foreground session's).
    pub fn group_connection(&self) -> Option<&ConnectionSpec> {
        self.my_group.connection.as_ref()
    }

    /// Open the "connect to a host" prompt: the window shows a host entry and
    /// swallows the keyboard until the user submits (Enter) or cancels (Escape).
    /// The shell calls this on a freshly-opened, sessionless ssh window — submit
    /// makes it an ssh group, Escape closes it.
    pub fn begin_connect(&mut self) {
        self.connect = Some(ConnectPrompt::new(ConnectTarget::Window));
    }

    /// Open the same prompt for a new ssh *session* in this window (Cmd+G): submit
    /// adopts the remote session as an additional tab (the window stays a normal
    /// group), and Escape dismisses the prompt back to the existing session rather
    /// than closing the window.
    pub fn begin_connect_session(&mut self) {
        self.connect = Some(ConnectPrompt::new(ConnectTarget::Session));
    }

    /// ssh asked for a password/passphrase mid-connect: show the (masked)
    /// password field in place of the host, labelled with ssh's own `prompt`.
    /// A fresh field each time, so a retry after a wrong password starts empty.
    pub fn connect_request_password(&mut self, prompt: String) {
        if let Some(p) = &mut self.connect {
            p.password = TextInput::new(String::new());
            p.phase = ConnectPhase::Password { prompt };
        }
    }

    /// Staging progress from the connect worker (copying the binary to the
    /// remote): show a progress bar. Ignored unless the prompt is mid-connect.
    pub fn connect_progress(&mut self, sent: u64, total: u64) {
        if let Some(p) = &mut self.connect
            && matches!(p.phase, ConnectPhase::Connecting { .. })
        {
            p.phase = ConnectPhase::Connecting {
                status: ConnectStatus::Staging { sent, total },
            };
        }
    }

    /// The connect attempt failed (bad password, unreachable host, …): show
    /// `message`; Enter returns to the host field to try again.
    pub fn connect_failed(&mut self, message: String) {
        if let Some(p) = &mut self.connect {
            p.phase = ConnectPhase::Error { message };
            // A fresh failure clears any lingering flash from a prior copy.
            p.copied = None;
        }
    }

    /// The connect resolved (the remote session is attaching): dismiss the prompt.
    pub fn end_connect(&mut self) {
        self.connect = None;
    }

    /// Whether this window is mid-connect (the prompt owns the view).
    pub fn is_connecting(&self) -> bool {
        self.connect.is_some()
    }

    /// Mark this window's group an explicit "ssh group" for `connection` (or
    /// clear it): every new session opened in the window then inherits it. The
    /// shell sets this when the connect prompt resolves, before adopting the
    /// first session — so the adopt's registry save persists it.
    pub fn set_group_connection(&mut self, connection: Option<ConnectionSpec>) {
        self.my_group.connection = connection;
        // Mirror it into the fleet's copy: a fresh ssh window is in fleet mode, and
        // the next adopt reads `self.my_group` back from `f.my_group()` (root.rs's
        // Fleet→Single hand-off) — so without this the connection is clobbered
        // before `claim_member` persists it, and the group loses its ssh identity.
        if let Mode::Fleet(f) = &mut self.mode {
            f.set_my_group(self.my_group.clone());
        }
    }

    pub fn window_record(&self) -> crate::workspace::WindowRecord {
        let (cols, rows) = self.grid();
        let mut attached: Vec<SessionId> = self.mine.iter().cloned().collect();
        attached.sort();
        crate::workspace::WindowRecord {
            group_id: self.my_group.id.clone(),
            cols,
            rows,
            fleet: self.is_fleet(),
            foreground: self.primary.clone(),
            attached,
        }
    }

    /// Set the foreground terminal's inner padding (logical px per side), propagating
    /// it to the live foreground and every warm mirror so a Ctrl-Tab switch keeps the
    /// border. The shell calls this once at construction from `[window] padding`; it
    /// takes effect on the next resize (the shell always sizes a fresh window).
    pub fn set_padding(&mut self, pad: f32) {
        self.pad = pad.max(0.0);
        if let Mode::Single(m) = &mut self.mode {
            m.set_padding(self.pad);
        }
        for m in self.warm.values_mut() {
            m.set_padding(self.pad);
        }
    }

    /// Override the animation duration (ms) — e.g. the shell wiring `GHOST_ANIM_MS`
    /// to slow the animations right down for visual validation. Affects dives and
    /// slides started after this call.
    pub fn set_anim_ms(&mut self, ms: u64) {
        self.anim_ms = ms;
    }

    /// Whether a visual animation (a fleet zoom or a session slide) is playing.
    pub fn is_animating(&self) -> bool {
        self.anim.is_some()
    }

    pub fn update(&mut self, ev: UiEvent) -> Vec<Cmd> {
        // While an animation plays it owns the tick stream (driving the timeline at
        // ~60fps); the model swap already happened, so this is purely the visual
        // hand-off. On completion it hands one tick back so the periodic session
        // refresh resumes.
        if let UiEvent::Tick { now_ms } = &ev
            && self.anim.is_some()
        {
            return self.tick_anim(*now_ms);
        }
        // Arm/expire the connect "Copied" flash on the clock. Computed here, but
        // the tick still flows on to the view beneath (fleet refresh); these
        // commands are appended to the final result below.
        let flash_cmds = match &ev {
            UiEvent::Tick { now_ms } => self.tick_copied_flash(*now_ms),
            _ => Vec::new(),
        };
        // The connect prompt is modal: while it is open it swallows keyboard,
        // text, and pointer input, so neither the typed host nor a stray click
        // reaches the view beneath it (a click on the error copies it). Resizes
        // and the like still pass through to keep the window sized.
        if self.connect.is_some()
            && matches!(
                ev,
                UiEvent::Key { .. } | UiEvent::Text(_) | UiEvent::Pointer { .. }
            )
        {
            return self.connect_input(ev);
        }
        if let UiEvent::Key {
            key, mods, kind, ..
        } = &ev
            && kind.is_down()
        {
            // Esc backs out of the fleet like F9 — but only when nothing in
            // the fleet claims it first (an open rename/confirm modal, or
            // multi-select marks to clear), and never in the single view,
            // where Esc is terminal input.
            let escape_out = matches!(key, Key::Named(NamedKey::Escape))
                && matches!(&self.mode, Mode::Fleet(f) if !f.consumes_escape());
            if is_fleet_toggle(key) || escape_out {
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
                Some(Shortcut::NewSshWindow) => return vec![Cmd::NewSshWindow],
                Some(Shortcut::NewSshSession) => return vec![Cmd::NewSshSession],
                Some(Shortcut::CloseWindow) => return vec![Cmd::CloseWindow],
                Some(Shortcut::NewSession) => return vec![Cmd::SpawnSession],
                _ => {} // Copy/Paste/Zoom are per-terminal: delegate below.
            }
        }
        if let UiEvent::AdoptSession(id) = &ev {
            let id = id.clone();
            return self.adopt(id);
        }
        // Authoritative groups from the shell (startup load, or another window
        // saved a change): remember them, and apply live to an open fleet.
        if let UiEvent::GroupsLoaded(groups) = ev {
            self.groups = groups.clone();
            return match &mut self.mode {
                Mode::Fleet(f) => f.update(UiEvent::GroupsLoaded(groups)),
                Mode::Single(_) => Vec::new(),
            };
        }
        // A set-change hint (a session appeared/vanished, a subscription ended):
        // re-enumerate now instead of waiting for the fleet's floor tick. Only
        // the fleet tracks the set; the single view has nothing to refresh.
        if let UiEvent::SessionsChanged = &ev {
            return match &self.mode {
                Mode::Fleet(_) => vec![Cmd::ListSessions],
                Mode::Single(_) => Vec::new(),
            };
        }
        // The foreground session's child exited (the shell was quit). Exiting a
        // shell never quits the app: switch to the next attached session, or drop
        // to the fleet overview when this window has none left.
        if let UiEvent::SessionData {
            name, ended: true, ..
        } = &ev
            && let Mode::Single(m) = &self.mode
            && m.session() == name
        {
            return self.foreground_ended();
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
            // An animation's frozen scenes are sized to the old window; drop it so
            // the live view re-renders at the new size rather than animating stale
            // frames (a slide would shear; a dive would zoom the wrong geometry).
            self.anim = None;
            // Resize the foreground and every warm background mirror, so a
            // backgrounded session is never left at a stale size (its prompt or a
            // full-screen program like `top` would come back mis-laid-out).
            let mut cmds = match &mut self.mode {
                Mode::Single(m) => resize_model(m, self.size_px, self.scale, self.pad),
                Mode::Fleet(f) => return f.update(UiEvent::Resize { w_px, h_px, scale }),
            };
            for m in self.warm.values_mut() {
                cmds.extend(resize_model(m, self.size_px, self.scale, self.pad));
            }
            return cmds;
        }
        // The session list completes the fleet (foreign/detached tiles, final order).
        // If a dive-out was waiting on it, launch the pull-back now that the grid is
        // whole — every tile already in its final slot, so nothing reshuffles.
        if let UiEvent::SessionList(infos) = &ev {
            // Teach this window's models their display names — a rename (possibly
            // made in another window) reaches us only through the reconcile. The
            // fleet handles its own tiles below; the foreground drives the window
            // title, so a label change there retitles.
            let mut cmds = Vec::new();
            for info in infos {
                if let Some(m) = self.warm.get_mut(&info.name) {
                    m.set_display_name(info.display_name.clone());
                }
            }
            if let Mode::Single(m) = &mut self.mode
                && let Some(info) = infos.iter().find(|i| i.name == m.session())
            {
                let before = m.title();
                m.set_display_name(info.display_name.clone());
                let after = m.title();
                if after != before {
                    cmds.push(Cmd::SetTitle(after));
                }
            }
            cmds.extend(match &mut self.mode {
                Mode::Single(m) => m.update(ev),
                Mode::Fleet(f) => f.update(ev),
            });
            self.mirror_fleet_identity();
            self.release_detached(&cmds);
            if let Some(p) = self.pending_dive.take() {
                cmds.extend(self.launch_dive_out(&p));
            }
            return cmds;
        }
        // Track OS focus for the live-bell reaction (the event still reaches
        // the terminal below for mode-1004 focus reporting).
        if let UiEvent::Focus(f) = &ev {
            self.focused_win = *f;
        }
        // A bell in one of this window's sessions while the window is
        // unfocused asks the OS for attention — the fleet badge and the
        // terminal feed handle the visible part; this is the "hey, over
        // here" a background window owes its user.
        let bell_attention = matches!(&ev,
            UiEvent::SessionPush {
                name,
                push: crate::SessionPush::Event(ghost_vt::protocol::SessionEvent::Bell),
            } if !self.focused_win && self.mine.contains(name.as_str()));
        let mut cmds = match &mut self.mode {
            Mode::Single(m) => m.update(ev),
            Mode::Fleet(f) => f.update(ev),
        };
        self.mirror_fleet_identity();
        self.release_detached(&cmds);
        if bell_attention {
            cmds.push(Cmd::RequestAttention);
        }
        cmds.extend(flash_cmds);
        cmds
    }

    /// Advance the connect "Copied" flash on a clock tick: stamp the expiry
    /// deadline from this fresh `now_ms` when it was just armed, then clear it
    /// once a later tick passes that deadline. Returns the commands to keep it
    /// ticking (a `ScheduleTick` to arrive at the deadline) or to redraw when it
    /// clears; empty when there's nothing to do.
    fn tick_copied_flash(&mut self, now_ms: u64) -> Vec<Cmd> {
        let Some(p) = &mut self.connect else {
            return Vec::new();
        };
        match p.copied {
            Some(CopiedFlash::Arming) => {
                p.copied = Some(CopiedFlash::Until(now_ms + COPIED_FLASH_MS));
                vec![Cmd::ScheduleTick {
                    after_ms: COPIED_FLASH_MS,
                }]
            }
            Some(CopiedFlash::Until(deadline)) if now_ms >= deadline => {
                p.copied = None;
                vec![Cmd::Redraw]
            }
            _ => Vec::new(),
        }
    }

    /// Mirror the fleet's group identity: opening a closed group from an
    /// empty window ADOPTS it (the window becomes that group), and the shell
    /// reads the identity off this root for the attaches it is about to run.
    fn mirror_fleet_identity(&mut self) {
        if let Mode::Fleet(f) = &self.mode
            && self.my_group != *f.my_group()
        {
            self.my_group = f.my_group().clone();
        }
    }

    /// A delegated command detaching a session means this window stopped
    /// driving it (the fleet's detach buttons, or a driven group member kept
    /// only as a dead tile): drop the ownership and any warm mirror, so the
    /// bell reaction, Ctrl-Tab, and the next fleet all see it as not ours.
    /// In the single view the released session also leaves this window's
    /// group (an open fleet syncs the registry itself), appending the save.
    fn release_detached(&mut self, cmds: &[Cmd]) {
        // Ownership only: the window stops driving the session, but its
        // group membership stays — detaching is not ungrouping, so the
        // member just goes cold in its block.
        for c in cmds {
            if let Cmd::Detach(id) = c {
                self.mine.remove(id);
                self.warm.remove(id);
            }
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
        self.anim = Some(Anim::dive(
            f.view(),
            camera,
            Transform::IDENTITY,
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
            // A background mirror still tracks its own title internally (so a later
            // Ctrl-Tab restores it), but must not drive the window title — only the
            // foreground session does. Same guard the fleet overview applies to tiles.
            Some(m) => m
                .update(ev)
                .into_iter()
                .filter(|c| !matches!(c, Cmd::SetTitle(_)))
                .collect(),
            None => Vec::new(), // not a session this window mirrors
        };
        if ended {
            // A dead background session is no longer ours: drop its mirror and
            // ownership so Ctrl-Tab and the fleet never land on it.
            self.warm.remove(&name);
            self.mine.remove(&name);
        }
        cmds
    }

    /// Switch to the single view of `id` (the shell has just attached it) and
    /// take ownership. From the fleet, the existing tile's screen is preserved;
    /// otherwise (or from another single session) a fresh terminal is created.
    /// The previously-shown session is NOT detached — the window keeps it warm so
    /// Ctrl-Tab and the fleet can switch back to it.
    fn adopt(&mut self, id: SessionId) -> Vec<Cmd> {
        // A new transition cancels any in-flight dive or slide (a still-waiting
        // dive-out, or an animation that hasn't settled) so a stale camera/snapshot
        // can't linger. A slide built *around* an adopt (Ctrl-Tab) re-arms it after.
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
                // Carry the fleet's (possibly edited) groups — and identity,
                // in case it adopted a closed group — out of the closing
                // overview; the next opening is seeded with them.
                self.groups = f.groups().to_vec();
                self.my_group = f.my_group().clone();
                // Opening a tile dives into where it sat in the grid: snapshot the
                // fleet world so the whole grid stays visible during the descent (a
                // freshly spawned session with no tile yet just opens, no dive).
                anim = f
                    .dive_camera(&id)
                    .map(|to| Anim::dive(f.view(), Transform::IDENTITY, to, dur));
                let (kept, warm, cmds) =
                    f.into_single_adopting(id.clone(), self.size_px, self.scale);
                // The window's other driven sessions stay warm in the background.
                // Own them too: a group-open claims sessions fleet-side, and this
                // is where the window learns about them (no-op for ones we knew).
                for m in warm {
                    self.mine.insert(m.session().to_string());
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
                    let model = self.warm.remove(&id).unwrap_or_else(|| {
                        let mut m = TerminalModel::new(id.clone(), 1, 1, self.metrics);
                        m.set_theme(self.theme);
                        m
                    });
                    (model, Vec::new())
                }
            }
        };
        // Size the (possibly restored or fresh) foreground to the window.
        cmds.extend(resize_model(&mut model, self.size_px, self.scale, self.pad));
        // The window title follows the foreground: reassert this session's remembered
        // title on the switch, since it changed no title of its own to trigger one.
        let title = model.title();
        self.mode = Mode::Single(Box::new(model));
        self.mine.insert(id.clone());
        cmds.extend(self.claim_member(&id));
        self.primary = Some(id);
        cmds.push(Cmd::SetTitle(title));
        cmds.push(Cmd::Redraw);
        if let Some(anim) = anim {
            self.anim = Some(anim);
            cmds.push(Cmd::ScheduleTick { after_ms: 0 });
        }
        cmds
    }

    /// The foreground session's child exited. Exiting a shell never quits the
    /// window: drop the dead session and switch to the next attached one — the
    /// forward-cycle successor Ctrl-Tab would pick, reusing its already-attached
    /// warm mirror — or, when the window has none left, fall back to the fleet
    /// overview (which lists whatever sessions still exist, empty if none).
    fn foreground_ended(&mut self) -> Vec<Cmd> {
        let Mode::Single(m) = &self.mode else {
            return Vec::new();
        };
        let gone = m.session().to_string();
        // Freeze the dead session's last frame now, before we discard it — it's the
        // rendered stand-in that slides out under the switch.
        let outgoing = self.live_scene();
        // The session is dead: drop our ownership and any warm mirror of it, and
        // cancel any in-flight dive/slide so a stale camera/snapshot can't linger.
        self.mine.remove(&gone);
        self.warm.remove(&gone);
        self.pending_dive = None;
        self.pending_dive_in = None;
        self.anim = None;

        // Pick the next session in the same forward order Ctrl-Tab walks: the
        // first survivor sorted after the one that exited, wrapping to the first.
        let mut survivors: Vec<String> = self.mine.iter().cloned().collect();
        survivors.sort();
        let next = survivors
            .iter()
            .find(|n| n.as_str() > gone.as_str())
            .or_else(|| survivors.first())
            .cloned();

        if let Some(next) = next {
            // Promote its warm mirror to the foreground (already attached and kept
            // resized); the dead model is discarded, never stowed as a mirror.
            let mut model = self.warm.remove(&next).unwrap_or_else(|| {
                let mut m = TerminalModel::new(next.clone(), 1, 1, self.metrics);
                m.set_theme(self.theme);
                m
            });
            let mut cmds = resize_model(&mut model, self.size_px, self.scale, self.pad);
            // The window title follows the new foreground, not the exited session.
            let title = model.title();
            self.mode = Mode::Single(Box::new(model));
            self.primary = Some(next);
            cmds.push(Cmd::SetTitle(title));
            // Slide the next session in (forward, like a Ctrl-Tab) over the dead
            // session's frozen stand-in.
            let incoming = self.live_scene();
            cmds.extend(self.start_slide(outgoing, incoming, true));
            cmds.push(Cmd::Redraw);
            return cmds;
        }

        // Nothing left to show: drop to the fleet overview.
        let mut fleet = FleetModel::new(self.metrics, self.size_px, self.mine.clone());
        fleet.set_theme(self.theme);
        fleet.set_groups(self.groups.clone());
        fleet.set_my_group(self.my_group.clone());
        // `FleetModel::new` defaults the device scale to 1.0; hand it this window's.
        fleet.update(UiEvent::Resize {
            w_px: self.size_px.0.max(1),
            h_px: self.size_px.1.max(1),
            scale: self.scale as f64,
        });
        self.mode = Mode::Fleet(Box::new(fleet));
        self.primary = None;
        vec![Cmd::ListSessions, Cmd::Redraw]
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
        // Freeze the current view, swap instantly, then slide the new session in
        // from the side we're heading (right when going forward, left when back).
        let outgoing = self.live_scene();
        let mut cmds = self.adopt(to);
        let incoming = self.live_scene();
        cmds.extend(self.start_slide(outgoing, incoming, forward));
        cmds
    }

    /// The window's current live scene (the foreground terminal, or the fleet
    /// grid) — what `view` renders when no animation is in flight, and the frozen
    /// endpoints a slide is built from.
    fn live_scene(&self) -> Scene {
        match &self.mode {
            Mode::Single(m) => m.view(),
            Mode::Fleet(f) => f.view(),
        }
    }

    /// Begin a session slide from `outgoing` to `incoming` (a fresh one replaces any
    /// in flight), and ask for the first frame. `forward` slides the incoming in
    /// from the right; backward, from the left.
    fn start_slide(&mut self, outgoing: Scene, incoming: Scene, forward: bool) -> Vec<Cmd> {
        let dir = if forward { 1.0 } else { -1.0 };
        self.anim = Some(Anim::slide(outgoing, incoming, dir, self.anim_ms));
        vec![Cmd::ScheduleTick { after_ms: 0 }]
    }

    pub fn view(&self) -> Scene {
        // The connect prompt owns the whole window until it resolves.
        if let Some(prompt) = &self.connect {
            return self.connect_scene(prompt);
        }

        // An animation owns the frame while it plays — the composed timeline frame.
        if let Some(anim) = &self.anim {
            return anim.scene();
        }

        // A dive-out waiting on the session list: hold the camera framed on the
        // session we left (it keeps filling the window, as in the single view) until
        // the reply lands and the pull-back is launched. Chrome fully faded — we're
        // zoomed all the way in — matching the dive's zoomed-in end.
        if let Some(p) = &self.pending_dive
            && let Mode::Fleet(f) = &self.mode
            && let Some(camera) = f.dive_camera(p)
        {
            return with_camera(f.view(), camera, 0.0);
        }

        self.live_scene()
    }

    /// Keyboard for the connect prompt, routed by phase. In [`Host`] typing goes
    /// to the host entry and Enter submits a valid `[user@]host` (beginning ssh
    /// auth). In [`Password`] typing goes to the masked password entry and Enter
    /// feeds it to the in-flight auth. [`Connecting`] swallows typing (auth is
    /// running). [`Error`] shows the failure until Enter returns to the host.
    /// Escape always cancels and closes the window.
    ///
    /// [`Host`]: ConnectPhase::Host
    /// [`Password`]: ConnectPhase::Password
    /// [`Connecting`]: ConnectPhase::Connecting
    /// [`Error`]: ConnectPhase::Error
    fn connect_input(&mut self, ev: UiEvent) -> Vec<Cmd> {
        // The entry the current phase types into, if any.
        fn entry(p: &mut ConnectPrompt) -> Option<&mut TextInput> {
            match p.phase {
                ConnectPhase::Host => Some(&mut p.host),
                ConnectPhase::Password { .. } => Some(&mut p.password),
                ConnectPhase::Connecting { .. } | ConnectPhase::Error { .. } => None,
            }
        }
        match ev {
            UiEvent::Text(s) => {
                if let Some(e) = self.connect.as_mut().and_then(entry) {
                    e.insert(&s);
                }
                vec![Cmd::Redraw]
            }
            UiEvent::Key {
                key, mods, kind, ..
            } if kind.is_down() => {
                // The copy chord (⌘C / Ctrl+Shift+C / Alt+C) lifts the error out
                // to the clipboard — there's no OS text layer to select it in.
                if matches!(classify_shortcut(&key, mods), Some(Shortcut::Copy))
                    && let Some(msg) = self.connect_error_message()
                {
                    return self.copy_error(msg);
                }
                match key {
                    Key::Char(s) if !mods.ctrl && !mods.sup => {
                        if let Some(e) = self.connect.as_mut().and_then(entry) {
                            e.insert(&s);
                        }
                        vec![Cmd::Redraw]
                    }
                    Key::Named(NamedKey::Enter) => self.connect_submit(),
                    Key::Named(NamedKey::Escape) => {
                        // A new-window connect closes its (empty) window; a new-session
                        // connect only dismisses the prompt and tells the shell to abort
                        // the in-flight ssh, keeping the window's existing session.
                        let target = self.connect.as_ref().map(|p| p.target);
                        self.connect = None;
                        match target {
                            Some(ConnectTarget::Session) => vec![Cmd::CancelConnect],
                            _ => vec![Cmd::CloseWindow],
                        }
                    }
                    key => {
                        if let Some(e) = self.connect.as_mut().and_then(entry)
                            && e.key(&key, mods)
                        {
                            vec![Cmd::Redraw]
                        } else {
                            Vec::new()
                        }
                    }
                }
            }
            // A left-click on the shown error copies it (the mouse counterpart of
            // the copy chord); every other pointer event is just swallowed so the
            // modal doesn't leak clicks to the view beneath.
            UiEvent::Pointer {
                phase: PointerPhase::Press,
                button: Some(PointerButton::Left),
                pos,
                ..
            } => {
                if self.connect_error_hit(pos)
                    && let Some(msg) = self.connect_error_message()
                {
                    return self.copy_error(msg);
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// The error message currently shown by the connect prompt, if it's in its
    /// [`Error`](ConnectPhase::Error) phase — the text the copy chord and a click
    /// on the message lift to the clipboard.
    fn connect_error_message(&self) -> Option<String> {
        match &self.connect {
            Some(ConnectPrompt {
                phase: ConnectPhase::Error { message },
                ..
            }) => Some(message.clone()),
            _ => None,
        }
    }

    /// Whether `pos` falls on the shown connect error (its click-to-copy target).
    fn connect_error_hit(&self, pos: PointPx) -> bool {
        self.connect
            .as_ref()
            .and_then(|p| self.connect_error_rect(p))
            .is_some_and(|r| r.contains(pos.x as f32, pos.y as f32))
    }

    /// Copy the error to the clipboard and arm the transient "Copied" flash: it
    /// shows at once, and the next tick stamps its expiry (see [`CopiedFlash`]),
    /// so an immediate tick is requested to stamp it promptly.
    fn copy_error(&mut self, msg: String) -> Vec<Cmd> {
        if let Some(p) = &mut self.connect {
            p.copied = Some(CopiedFlash::Arming);
        }
        vec![
            Cmd::WriteClipboard(msg),
            Cmd::ScheduleTick { after_ms: 0 },
            Cmd::Redraw,
        ]
    }

    /// Enter in the connect prompt, by phase: submit the host (→ begin auth),
    /// submit the password (→ feed the in-flight auth), or retry from an error.
    fn connect_submit(&mut self) -> Vec<Cmd> {
        let Some(p) = &mut self.connect else {
            return Vec::new();
        };
        match &p.phase {
            ConnectPhase::Host => match ConnectionSpec::parse_target(p.host.text()) {
                Some(spec) => {
                    let target = p.target;
                    p.phase = ConnectPhase::Connecting {
                        status: ConnectStatus::Working,
                    };
                    // A new-window connect makes the window an ssh group; a
                    // new-session connect adopts a tab into this window instead.
                    match target {
                        ConnectTarget::Window => vec![Cmd::ConnectSshWindow { spec }],
                        ConnectTarget::Session => vec![Cmd::ConnectSshSession { spec }],
                    }
                }
                // Empty or unparseable host: stay in the prompt.
                None => vec![Cmd::Redraw],
            },
            ConnectPhase::Password { .. } => {
                let password = p.password.text().to_string();
                p.phase = ConnectPhase::Connecting {
                    status: ConnectStatus::Working,
                };
                vec![Cmd::ConnectPassword(password)]
            }
            // Retry: back to the host field (its text is preserved).
            ConnectPhase::Error { .. } => {
                p.phase = ConnectPhase::Host;
                vec![Cmd::Redraw]
            }
            ConnectPhase::Connecting { .. } => Vec::new(),
        }
    }

    /// The physical-pixel rect of the connect error line — the exact geometry
    /// [`connect_scene`](Self::connect_scene) draws it at (both scale by
    /// [`CONNECT_SCALE`] and share these formulas), so click-to-copy lands on the
    /// shown text. `None` unless the prompt is in its [`Error`] phase.
    ///
    /// [`Error`]: ConnectPhase::Error
    fn connect_error_rect(&self, prompt: &ConnectPrompt) -> Option<RectPx> {
        let ConnectPhase::Error { message } = &prompt.phase else {
            return None;
        };
        let advance = self.metrics.advance * CONNECT_SCALE;
        let line_height = self.metrics.line_height * CONNECT_SCALE;
        let (w, h) = (self.size_px.0 as f32, self.size_px.1 as f32);
        let tw = (message.chars().count() as f32 * advance).max(1.0);
        let ty = ((h - line_height * 6.0) * 0.5).max(0.0);
        let by = ty + line_height * 1.8;
        Some(RectPx {
            x: ((w - tw) * 0.5).max(0.0),
            y: by,
            w: tw,
            h: line_height,
        })
    }

    /// The "connect to a host" overlay: a full-window scrim, a title, and — by
    /// phase — the host field, a "connecting…" line, the masked password field
    /// (in place of the host, only when ssh asks), or an error, plus a hint line;
    /// centered at the modal scale.
    fn connect_scene(&self, prompt: &ConnectPrompt) -> Scene {
        use crate::Rgba;
        const SCRIM: Rgba = [0.04, 0.04, 0.06, 1.0];
        const FG: Rgba = [0.92, 0.94, 0.97, 1.0];
        const HINT: Rgba = [0.60, 0.63, 0.68, 1.0];
        const ERR: Rgba = [0.95, 0.55, 0.45, 1.0];
        const OK: Rgba = [0.45, 0.85, 0.60, 1.0];
        const FIELD_BG: Rgba = [0.12, 0.13, 0.16, 1.0];
        const BORDER: Rgba = [0.30, 0.60, 0.95, 1.0];
        const SCALE: f32 = CONNECT_SCALE;

        let (w, h) = (self.size_px.0 as f32, self.size_px.1 as f32);
        let m = CellMetrics {
            advance: self.metrics.advance * SCALE,
            line_height: self.metrics.line_height * SCALE,
        };
        let text_w = |s: &str| s.chars().count() as f32 * m.advance;
        let center_x = |tw: f32| ((w - tw) * 0.5).max(0.0);
        let run = |s: &str| Run {
            start_col: 0,
            width_cols: s.chars().count(),
            text: s.to_string(),
            style: Style::default(),
        };
        let line = |y: f32, s: &str, color: Rgba| SceneItem::Text {
            id: SceneId::NavBar,
            rect: RectPx {
                x: center_x(text_w(s)),
                y,
                w: text_w(s).max(1.0),
                h: m.line_height,
            },
            runs: vec![run(s)],
            metrics: m,
            color,
            scale: SCALE,
        };

        // A field's shown text (password masked as bullets) plus the caret's
        // char-column when focused. The caret is drawn as a block *over* its cell
        // (in `field` below), never spliced into the string, so the text stays
        // put as the caret moves and the block sits on the character, not between.
        let shown = |entry: &TextInput, mask: bool, focused: bool| -> (String, Option<usize>) {
            let (before, after) = entry.halves();
            let (before, after) = if mask {
                (
                    "\u{2022}".repeat(before.chars().count()),
                    "\u{2022}".repeat(after.chars().count()),
                )
            } else {
                (before.to_string(), after.to_string())
            };
            let caret = focused.then(|| before.chars().count());
            (format!("{before}{after}"), caret)
        };

        let (host_shown, host_caret) = shown(&prompt.host, false, true);
        let (pw_shown, pw_caret) = shown(&prompt.password, true, true);
        // Ideal 28 columns, widened to fit the content but never past 90% of the
        // window. Written as `.max(min).min(cap)` — NOT `clamp(min, cap)` — so a
        // host wide enough that its content exceeds the cap yields the cap rather
        // than panicking (clamp panics when its low bound exceeds its high bound).
        let field_w = (28.0 * m.advance)
            .max(text_w(&host_shown).max(text_w(&pw_shown)) + 2.0 * m.advance)
            .min((w * 0.9).max(1.0));
        let field_h = m.line_height * 1.6;

        let mut items = vec![SceneItem::Rect {
            id: SceneId::Sidebar,
            rect: RectPx {
                x: 0.0,
                y: 0.0,
                w,
                h,
            },
            color: SCRIM,
            radius: 0.0,
        }];

        // Enough vertical room for title + one labeled field/message + hint,
        // centered.
        let ty = ((h - m.line_height * 6.0) * 0.5).max(0.0);
        items.push(line(ty, "Connect to a host over SSH", FG));

        // Push the sole labeled, bordered field at `y`; return the y below it.
        // `caret` (a char-column, when focused) is rendered as a block over its
        // cell with the underlying glyph inverted, so it reads like a terminal
        // cursor sitting on the character rather than a bar wedged between two.
        let field =
            |items: &mut Vec<SceneItem>, y: f32, label: &str, text: &str, caret: Option<usize>| {
                items.push(line(y, label, HINT));
                let fy = y + m.line_height * 1.1;
                let rect = RectPx {
                    x: center_x(field_w),
                    y: fy,
                    w: field_w,
                    h: field_h,
                };
                items.push(SceneItem::Rect {
                    id: SceneId::NavBar,
                    rect,
                    color: FIELD_BG,
                    radius: 5.0,
                });
                items.push(SceneItem::Border {
                    id: SceneId::NavBar,
                    rect,
                    color: BORDER,
                    width: 2.0,
                });
                let tx = rect.x + m.advance * 0.5;
                let ty = fy + (field_h - m.line_height) * 0.5;
                items.push(SceneItem::Text {
                    id: SceneId::NavBar,
                    rect: RectPx {
                        x: tx,
                        y: ty,
                        w: (field_w - m.advance).max(1.0),
                        h: m.line_height,
                    },
                    runs: vec![run(text)],
                    metrics: m,
                    color: FG,
                    scale: SCALE,
                });
                // The block caret, over the cell at `caret`, and the glyph under it
                // redrawn in the field colour so it stays legible (an inverted cell).
                if let Some(col) = caret {
                    let cell = RectPx {
                        x: tx + col as f32 * m.advance,
                        y: ty,
                        w: m.advance,
                        h: m.line_height,
                    };
                    items.push(SceneItem::Rect {
                        id: SceneId::NavBar,
                        rect: cell,
                        color: FG,
                        radius: 0.0,
                    });
                    if let Some(ch) = text.chars().nth(col) {
                        items.push(SceneItem::Text {
                            id: SceneId::NavBar,
                            rect: cell,
                            runs: vec![run(&ch.to_string())],
                            metrics: m,
                            color: FIELD_BG,
                            scale: SCALE,
                        });
                    }
                }
                fy + field_h
            };

        // The body depends on the phase: the host field, a "connecting…" line,
        // the masked password field (in place of the host, only when ssh asks),
        // or the error message. Each yields the y below it and its hint.
        let by = ty + m.line_height * 1.8;
        let (after, hint) = match &prompt.phase {
            ConnectPhase::Host => (
                field(&mut items, by, "Host", &host_shown, host_caret),
                "Enter to connect · Esc to cancel",
            ),
            ConnectPhase::Connecting { status } => {
                let host = prompt.host.text();
                match status {
                    ConnectStatus::Working => {
                        items.push(line(by, &format!("Connecting to {host}\u{2026}"), FG));
                        (by + m.line_height, "Esc to cancel")
                    }
                    ConnectStatus::Staging { sent, total } => {
                        let frac = if *total > 0 {
                            (*sent as f32 / *total as f32).clamp(0.0, 1.0)
                        } else {
                            0.0
                        };
                        let pct = (frac * 100.0).round() as u32;
                        items.push(line(
                            by,
                            &format!("Staging ghost to {host}\u{2026} {pct}%"),
                            FG,
                        ));
                        // A rounded track with a proportional fill, field-width.
                        let bar_y = by + m.line_height * 1.3;
                        let bar_h = m.line_height * 0.5;
                        let track = RectPx {
                            x: center_x(field_w),
                            y: bar_y,
                            w: field_w,
                            h: bar_h,
                        };
                        items.push(SceneItem::Rect {
                            id: SceneId::NavBar,
                            rect: track,
                            color: FIELD_BG,
                            radius: bar_h * 0.5,
                        });
                        let fill_w = field_w * frac;
                        if fill_w > 0.5 {
                            items.push(SceneItem::Rect {
                                id: SceneId::NavBar,
                                rect: RectPx {
                                    x: center_x(field_w),
                                    y: bar_y,
                                    w: fill_w,
                                    h: bar_h,
                                },
                                color: BORDER,
                                radius: bar_h * 0.5,
                            });
                        }
                        (bar_y + bar_h, "Esc to cancel")
                    }
                }
            }
            ConnectPhase::Password { prompt: label } => {
                let label = if label.is_empty() { "Password" } else { label };
                (
                    field(&mut items, by, label, &pw_shown, pw_caret),
                    "Enter to submit · Esc to cancel",
                )
            }
            ConnectPhase::Error { message } => {
                items.push(line(by, message, ERR));
                // A line under the message is reserved for the transient "Copied"
                // flash, so the hint below doesn't jump as it comes and goes.
                let flash_y = by + m.line_height;
                if prompt.copied.is_some() {
                    items.push(line(flash_y, "✓ Copied", OK));
                }
                (flash_y + m.line_height, CONNECT_ERROR_HINT)
            }
        };

        items.push(line(after + m.line_height * 0.9, hint, HINT));

        let mut scene = Scene::new(self.size_px);
        scene.layers.push(Layer::new(0, items));
        scene
    }

    /// Tell the live foreground session its view was just composited, so its next
    /// [`view`](Self::view) measures [`TermDamage`](crate::TermDamage) from here (see
    /// [`TerminalModel::mark_presented`]). The shell calls this after a successful
    /// present. A no-op during an animation (frozen textures, not a live model, are on
    /// screen) and in the fleet (downscaled previews carry no row-localized damage), so
    /// on returning to a single view the foreground repaints in full once and resumes.
    pub fn mark_presented(&mut self) {
        if self.anim.is_some() {
            return;
        }
        if let Mode::Single(m) = &mut self.mode {
            m.mark_presented();
        }
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

    fn toggle(&mut self) -> Vec<Cmd> {
        // Swap the mode out behind a cheap placeholder so we can move the owned
        // model/fleet into the conversion.
        let placeholder = Mode::Single(Box::new(TerminalModel::new(
            String::new(),
            1,
            1,
            self.metrics,
        )));
        // A new transition cancels any in-flight dive or slide (a still-waiting
        // dive-out, an animation that hasn't settled, or a take-over awaiting its
        // preview) so a stale camera/snapshot can't linger.
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
                let (mut fleet, mut cmds) = FleetModel::adopting(
                    *m,
                    warm,
                    self.metrics,
                    self.size_px,
                    self.scale,
                    self.mine.clone(),
                );
                fleet.set_theme(self.theme);
                fleet.set_groups(self.groups.clone());
                fleet.set_my_group(self.my_group.clone());
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
                // Carry the fleet's (possibly edited) groups — and identity,
                // in case it adopted a closed group — out of the closing
                // overview; the next opening is seeded with them.
                self.groups = f.groups().to_vec();
                self.my_group = f.my_group().clone();
                // Dive in: snapshot the fleet world so the whole grid stays visible
                // while we descend into the tile we land on, then take over with the
                // live single view once the dive lands. Return only to a session
                // THIS window drives; an overview-only window (nothing owned) has
                // nothing to return to, so F9/Esc stays in the fleet rather than
                // adopting a foreign tile — which would attach that session here
                // while it's still attached in its own window, in two groups.
                // Choosing a specific tile to open is Enter/click, not F9/Esc.
                let Some(target) = f.owned_tile(self.primary.as_deref()) else {
                    self.mode = Mode::Fleet(f);
                    return Vec::new();
                };
                let to = f.dive_camera(&target);
                let anim = to.map(|to| Anim::dive(f.view(), Transform::IDENTITY, to, dur));
                let (model, warm, mut cmds) =
                    f.into_single_adopting(target.clone(), self.size_px, self.scale);
                // The extracted session becomes the foreground; the rest of the
                // window's driven sessions stay warm in the background. Own them
                // too: a group-open claims sessions fleet-side, and this is where
                // the window learns about them (no-op for ones we knew).
                for m in warm {
                    self.mine.insert(m.session().to_string());
                    self.warm.insert(m.session().to_string(), m);
                }
                cmds.push(Cmd::Redraw);
                let id = model.session().to_string();
                self.mine.insert(id.clone());
                self.primary = Some(id);
                // Follow the foreground: diving in reasserts this session's remembered
                // title, since the fleet filtered any title changes while overviewing.
                cmds.push(Cmd::SetTitle(model.title()));
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

    /// Advance the in-flight animation on a clock tick: repaint (and schedule the
    /// next frame) while running; on completion clear it and, if we landed in the
    /// fleet, hand one tick back so its periodic session refresh resumes.
    fn tick_anim(&mut self, now_ms: u64) -> Vec<Cmd> {
        let Some(anim) = self.anim.as_mut() else {
            return Vec::new();
        };
        let done = anim.advance(now_ms);
        let mut cmds = vec![Cmd::Redraw];
        if done {
            self.anim = None;
            // Hand the settling tick back to whichever view is now live. The
            // animation owned the tick stream while it played, so a foreground
            // terminal holding a synchronized-output frame (DEC 2026) never saw
            // its release tick — forward one now, or the hold latches and the
            // view stays frozen on the pre-frame content until some input forces
            // a repaint. The fleet needs it too, to resume its periodic refresh.
            cmds.extend(match &mut self.mode {
                Mode::Fleet(f) => f.update(UiEvent::Tick { now_ms }),
                Mode::Single(m) => m.update(UiEvent::Tick { now_ms }),
            });
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

    fn click(r: &mut RootModel, x: f32, y: f32) -> Vec<Cmd> {
        r.update(UiEvent::Pointer {
            phase: PointerPhase::Press,
            button: Some(PointerButton::Left),
            pos: crate::PointPx {
                x: x as f64,
                y: y as f64,
            },
            mods: Mods::NONE,
            wheel_dy: 0.0,
            clicks: 1,
        })
    }

    /// The platform's copy chord (⌘C on macOS; Alt+C elsewhere — both resolve to
    /// [`Shortcut::Copy`] in [`classify_shortcut`]).
    fn copy_mods() -> Mods {
        if cfg!(target_os = "macos") {
            Mods::SUPER
        } else {
            Mods::ALT
        }
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
            display_name: String::new(),
            cwd: None,
            size: None,
            connection: None,
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
    fn a_bell_from_an_owned_session_requests_attention_only_when_unfocused() {
        let bell = |r: &mut RootModel, name: &str| {
            r.update(UiEvent::SessionPush {
                name: name.into(),
                push: crate::SessionPush::Event(ghost_vt::protocol::SessionEvent::Bell),
            })
        };
        let mut r = root(); // owns "alpha", single view
        // Unfocused + owned session rings: ask the OS for attention.
        r.update(UiEvent::Focus(false));
        assert!(
            bell(&mut r, "alpha").contains(&Cmd::RequestAttention),
            "an unfocused window flags its own session's bell"
        );
        // Focused: the user is looking at it; no attention theatrics.
        r.update(UiEvent::Focus(true));
        assert!(!bell(&mut r, "alpha").contains(&Cmd::RequestAttention));
        // Unfocused, but someone else's session: not this window's news.
        r.update(UiEvent::Focus(false));
        assert!(!bell(&mut r, "beta").contains(&Cmd::RequestAttention));
    }

    #[test]
    fn window_record_captures_the_windows_restorable_state() {
        let mut r = root(); // single view, owns "alpha"
        r.set_my_group(crate::Group::auto("w1".into(), 2));
        let rec = r.window_record();
        assert_eq!(rec.group_id, "w1");
        assert_eq!((rec.cols, rec.rows), r.grid(), "sized to the window grid");
        assert!(rec.cols > 0 && rec.rows > 0);
        assert!(!rec.fleet, "single view");
        assert_eq!(rec.foreground.as_deref(), Some("alpha"));
        assert_eq!(rec.attached, vec!["alpha".to_string()]);
        // Diving to the fleet flips the mode; the owned set and group persist.
        dive_out(&mut r, &[sess("alpha", true, 1)]);
        let rec = r.window_record();
        assert!(rec.fleet, "fleet overview");
        assert_eq!(rec.group_id, "w1");
        assert_eq!(rec.attached, vec!["alpha".to_string()]);
    }

    #[test]
    fn f9_in_an_overview_only_window_adopts_nothing_and_stays_in_the_fleet() {
        // A freshly-opened overview window owns no session; the fleet lists one
        // that is attached in another window ("ghost-mac", attached elsewhere).
        let (mut r, _) = RootModel::fleet(METRICS, SIZE, 1.0);
        r.update(UiEvent::SessionList(vec![sess("ghost-mac", true, 1)]));
        // F9 must not dive into — and thereby adopt — that foreign session. With
        // nothing of its own to return to, the window stays in the overview
        // rather than attaching a session that's already attached (in two groups).
        let cmds = key(&mut r, Key::Named(NamedKey::F9), Mods::NONE);
        assert!(r.is_fleet(), "F9 with no owned session stays in the fleet");
        assert!(
            r.primary.is_none(),
            "no foreign session is adopted as foreground"
        );
        assert!(
            !r.mine.contains("ghost-mac"),
            "the foreign session is not claimed by this window"
        );
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, Cmd::SetTitle(_) | Cmd::SaveGroups(_))),
            "no dive-in / group-claim side effects: {cmds:?}"
        );
        // Esc routes through the same dive-back and is likewise inert here.
        key(&mut r, Key::Named(NamedKey::Escape), Mods::NONE);
        assert!(
            r.is_fleet(),
            "Esc with no owned session also stays in the fleet"
        );
        assert!(r.primary.is_none() && !r.mine.contains("ghost-mac"));
    }

    #[test]
    fn session_pushes_are_inert_in_the_single_view() {
        let mut r = root();
        let cmds = r.update(UiEvent::SessionPush {
            name: "alpha".into(),
            push: crate::SessionPush::Event(ghost_vt::protocol::SessionEvent::Bell),
        });
        assert!(cmds.is_empty());
        assert!(!r.is_fleet());
    }

    #[test]
    fn a_pushed_bell_reaches_the_fleet_tile() {
        let mut r = root();
        dive_out(&mut r, &[sess("alpha", true, 1), sess("beta", false, 2)]);
        let cmds = r.update(UiEvent::SessionPush {
            name: "beta".into(),
            push: crate::SessionPush::Event(ghost_vt::protocol::SessionEvent::Bell),
        });
        assert!(cmds.contains(&Cmd::Redraw), "the tile badge repaints");
    }

    #[test]
    fn a_sessions_changed_hint_relists_in_the_fleet_only() {
        let mut r = root();
        assert!(
            r.update(UiEvent::SessionsChanged).is_empty(),
            "the single view doesn't track the session set"
        );
        dive_out(&mut r, &[sess("alpha", true, 1)]);
        assert_eq!(r.update(UiEvent::SessionsChanged), vec![Cmd::ListSessions]);
    }

    /// Whether the fleet scene shows a `name` group header.
    fn shows_group(r: &RootModel, name: &str) -> bool {
        r.view().layers.iter().any(|l| {
            l.items.iter().any(|it| match it {
                SceneItem::Text { runs, .. } => runs.iter().any(|run| run.text == name),
                _ => false,
            })
        })
    }

    #[test]
    fn my_group_renders_automatically_and_survives_fleet_toggles() {
        let mut r = root(); // owns "alpha", single view
        r.set_my_group(crate::Group::auto("w1".into(), 2));
        dive_out(&mut r, &[sess("alpha", true, 1), sess("beta", false, 2)]);
        settle(&mut r);
        // The owned session renders in this window's block — no ceremony.
        assert!(shows_group(&r, "orange"), "my block renders, color-named");
        assert!(
            r.groups
                .iter()
                .any(|g| g.id == "w1" && g.members == vec!["alpha".to_string()])
                || matches!(&r.mode, Mode::Fleet(f) if f
                    .groups()
                    .iter()
                    .any(|g| g.id == "w1" && g.members == vec!["alpha".to_string()])),
            "membership synced into the registry"
        );
        key(&mut r, Key::Named(NamedKey::F9), Mods::NONE); // -> single
        settle(&mut r);
        assert!(!r.is_fleet());
        assert!(
            r.groups
                .iter()
                .any(|g| g.id == "w1" && g.members == vec!["alpha".to_string()]),
            "closing the fleet carries the entry out: {:?}",
            r.groups
        );
        dive_out(&mut r, &[sess("alpha", true, 1), sess("beta", false, 2)]);
        settle(&mut r);
        assert!(
            shows_group(&r, "orange"),
            "my block persists across fleet close/reopen"
        );
    }

    #[test]
    fn adopting_a_closed_group_rebinds_the_window_identity() {
        let (mut r, _) = RootModel::fleet(METRICS, SIZE, 1.0);
        r.set_my_group(crate::Group::auto("w1".into(), 0));
        assert_eq!(r.client_identity(), "ghost-ui:w1");
        r.update(UiEvent::GroupsLoaded(vec![crate::Group {
            id: "g2".into(),
            name: "green".into(),
            color: 1,
            members: vec!["x".into(), "y".into()],
            connection: None,
        }]));
        r.update(UiEvent::SessionList(vec![
            sess("x", false, 1),
            sess("y", false, 2),
        ]));
        // Ctrl-Enter on the focused member (the empty window drives
        // nothing): the window BECOMES the group, and the identity the
        // shell reads for the very next attach already says so.
        let cmds = r.update(UiEvent::Key {
            key: Key::Named(NamedKey::Enter),
            mods: Mods {
                ctrl: true,
                ..Mods::NONE
            },
            kind: KeyEventKind::Press,
            alts: None,
        });
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::TakeOver(_))),
            "the group opens: {cmds:?}"
        );
        assert_eq!(
            r.client_identity(),
            "ghost-ui:g2",
            "the adopted identity is what the attaches will report"
        );
    }

    #[test]
    fn a_single_view_adopt_joins_this_windows_group() {
        let mut r = root(); // owns "alpha", single view
        r.set_my_group(crate::Group::auto("w1".into(), 0));
        let cmds = r.update(UiEvent::AdoptSession("beta".into()));
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::SaveGroups(gs)
                if gs.iter().any(|g| g.id == "w1" && g.members.contains(&"beta".to_string())))),
            "adopting a session persists its membership: {cmds:?}"
        );
    }

    #[test]
    fn loaded_groups_reach_an_open_fleet_and_later_openings() {
        let mut r = root();
        let infra = crate::Group {
            id: "g-infra".into(),
            name: "infra".into(),
            color: 0,
            members: vec!["beta".into()],
            connection: None,
        };
        // Loaded before the fleet ever opens: seeds the next opening. (A
        // foreign group draws no block of its own yet, so assert on the
        // registry the fleet carries.)
        r.update(UiEvent::GroupsLoaded(vec![infra.clone()]));
        dive_out(&mut r, &[sess("alpha", true, 1), sess("beta", false, 2)]);
        settle(&mut r);
        let fleet_has = |r: &RootModel, gid: &str| match &r.mode {
            Mode::Fleet(f) => f.groups().iter().any(|g| g.id == gid),
            Mode::Single(_) => false,
        };
        assert!(fleet_has(&r, "g-infra"), "startup groups seed the fleet");
        // Re-broadcast while open (another window saved): applies live.
        r.update(UiEvent::GroupsLoaded(Vec::new()));
        assert!(!fleet_has(&r, "g-infra"), "a live broadcast replaces them");
    }

    #[test]
    fn a_delegated_detach_releases_the_windows_ownership() {
        let mut r = root();
        dive_out(&mut r, &[sess("alpha", true, 1), sess("beta", false, 2)]);
        settle(&mut r);
        r.update(UiEvent::GroupsLoaded(vec![crate::Group {
            id: "g-web".into(),
            name: "web".into(),
            color: 0,
            members: vec!["alpha".into()],
            connection: None,
        }]));
        assert!(r.mine.contains("alpha"));
        // The driven, grouped member dies: the fleet keeps a dead tile and
        // emits a Detach for the window's client — which must also release
        // the root's ownership, or the next fleet would claim the corpse.
        r.update(UiEvent::SessionList(vec![sess("beta", false, 2)]));
        assert!(
            !r.mine.contains("alpha"),
            "a session this window no longer drives is not ours"
        );
    }

    #[test]
    fn opening_a_group_lands_on_its_first_member_with_the_rest_attached() {
        let mut r = root();
        dive_out(
            &mut r,
            &[
                sess("alpha", true, 1),
                sess("beta", false, 2),
                sess("gamma", false, 3),
            ],
        );
        settle(&mut r);
        r.update(UiEvent::GroupsLoaded(vec![crate::Group {
            id: "g-web".into(),
            name: "web".into(),
            color: 0,
            members: vec!["alpha".into(), "gamma".into()],
            connection: None,
        }]));
        // Ctrl-Enter on the focused member opens the whole group; the shell
        // answers each take-over with an adopt, in command order.
        let cmds = r.update(UiEvent::Key {
            key: Key::Named(NamedKey::Enter),
            mods: Mods {
                ctrl: true,
                ..Mods::NONE
            },
            kind: KeyEventKind::Press,
            alts: None,
        });
        assert!(
            cmds.contains(&Cmd::TakeOver("alpha".into()))
                && cmds.contains(&Cmd::Attach("gamma".into())),
            "the first member is adopted, the rest attach in the background: {cmds:?}"
        );
        r.update(UiEvent::AdoptSession("alpha".into()));
        assert!(!r.is_fleet(), "opening the group lands in the single view");
        assert_eq!(
            r.primary.as_deref(),
            Some("alpha"),
            "the group's FIRST member is the foreground"
        );
        assert!(
            r.mine.contains("gamma"),
            "the other member is attached to this window (Ctrl-Tab reaches it)"
        );
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
    fn escape_in_the_fleet_dives_back_like_f9() {
        let mut r = root(); // single view of alpha
        key(&mut r, Key::Named(NamedKey::F9), Mods::NONE);
        assert!(r.is_fleet());
        let cmds = key(&mut r, Key::Named(NamedKey::Escape), Mods::NONE);
        assert!(!r.is_fleet(), "Esc leaves the fleet like F9: {cmds:?}");
        // Back in the single view, Esc is terminal input again, never a toggle.
        let cmds = key(&mut r, Key::Named(NamedKey::Escape), Mods::NONE);
        assert!(!r.is_fleet(), "Esc in the single view stays put");
        assert_eq!(
            cmds,
            vec![Cmd::SendInput {
                session: "alpha".into(),
                bytes: b"\x1b".to_vec()
            }],
            "Esc reaches the terminal as input"
        );
    }

    #[test]
    fn escape_cancels_a_fleet_modal_instead_of_leaving() {
        let mut r = root(); // owns "alpha"
        key(&mut r, Key::Named(NamedKey::F9), Mods::NONE);
        r.update(UiEvent::SessionList(vec![sess("alpha", true, 1)]));
        // F2 opens a rename on the focused tile; Esc must close the modal and
        // stay in the fleet, only leaving on a second, unclaimed press.
        key(&mut r, Key::Named(NamedKey::F2), Mods::NONE);
        key(&mut r, Key::Named(NamedKey::Escape), Mods::NONE);
        assert!(
            r.is_fleet(),
            "Esc with a rename open only cancels the rename"
        );
        key(&mut r, Key::Named(NamedKey::Escape), Mods::NONE);
        assert!(!r.is_fleet(), "the next Esc dives back in");
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
                display_name: String::new(),
                cwd: None,
                size: None,
                connection: None,
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
    fn grid_is_the_window_size_and_matches_the_models_own_resize() {
        // A window much larger than the legacy 80×24 default: `grid` must report
        // the real cell grid, never a fixed provisional size, so an attach
        // handshake lays the host's resync out where we'll actually show it.
        let mut r = root();
        r.update(UiEvent::Resize {
            w_px: 1600,
            h_px: 900,
            scale: 1.0,
        });
        // 1600/9 = 177 cols, 900/18 = 50 rows.
        assert_eq!(r.grid(), (177, 50));

        // The handshake size must equal what a freshly-adopted model resizes
        // itself to at the same geometry — otherwise the host would reflow
        // between the handshake and the model's first resize.
        let mut m = TerminalModel::new("x".to_string(), 1, 1, METRICS);
        m.update(UiEvent::Resize {
            w_px: 1600,
            h_px: 900,
            scale: 1.0,
        });
        assert_eq!(r.grid(), m.dims());

        // HiDPI: a 2× surface doubles the cells, so the grid halves — still the
        // real size, not a constant.
        r.update(UiEvent::Resize {
            w_px: 1600,
            h_px: 900,
            scale: 2.0,
        });
        assert_eq!(r.grid(), (88, 25));
    }

    #[test]
    fn padding_insets_the_grid_and_the_foreground_model() {
        use crate::SceneItem;
        let mut r = root();
        r.set_padding(18.0);
        r.update(UiEvent::Resize {
            w_px: 720,
            h_px: 432,
            scale: 1.0,
        });
        // The handshake grid folds in the border (== two cols / one row here), so it
        // still matches the foreground model's own inset resize.
        assert_eq!(r.grid(), (76, 22));
        // The foreground lays out inside the same border: its item rect is inset while
        // the canvas stays the whole window, leaving a bg-filled frame.
        let scene = r.view();
        assert_eq!(scene.size_px, (720, 432));
        match scene.terminals().next().unwrap() {
            SceneItem::Terminal { rect, .. } => {
                assert_eq!((rect.x, rect.y, rect.w, rect.h), (18.0, 18.0, 684.0, 396.0));
            }
            _ => unreachable!(),
        }
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
        // A full-zoom framing fills the window up to the sub-cell remainder —
        // exactly how the live single view draws (a 1400px window holds 155
        // 9px columns = 1395px, with the leftover as a right gap).
        let covers = |r: RectPx| {
            r.x <= 0.5
                && r.y <= 0.5
                && r.x + r.w >= w - METRICS.advance
                && r.y + r.h >= h - METRICS.line_height
        };
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
            display_name: String::new(),
            cwd: None,
            size: None,
            connection: None,
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
            assert_eq!(
                key(&mut r, Key::Char("s".into()), chord),
                vec![Cmd::NewSshWindow],
                "Cmd+S / Ctrl+Shift+S opens a new ssh window"
            );
        }
        // Bare Ctrl+S is NOT a shortcut — it stays terminal flow control (XOFF).
        let mut r = root();
        assert!(matches!(
            key(&mut r, Key::Char("s".into()), Mods::CTRL).as_slice(),
            [Cmd::SendInput { .. }]
        ));
        // On Linux, Alt+S also opens an ssh window (mirroring Alt+N/Alt+T); on
        // macOS Alt stays Option/Meta, so it is encoded to the child instead.
        #[cfg(not(target_os = "macos"))]
        {
            let mut r = root();
            assert_eq!(
                key(&mut r, Key::Char("s".into()), Mods::ALT),
                vec![Cmd::NewSshWindow]
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

    /// Does any Text run in the current scene contain `needle`?
    fn scene_has(r: &RootModel, needle: &str) -> bool {
        r.view().layers.iter().any(|l| {
            l.items.iter().any(|it| {
                matches!(it, SceneItem::Text { runs, .. }
                    if runs.iter().any(|run| run.text.contains(needle)))
            })
        })
    }

    fn typed(r: &mut RootModel, s: &str) {
        for ch in s.chars() {
            key(r, Key::Char(ch.to_string()), Mods::NONE);
        }
    }

    #[test]
    fn the_connect_prompt_captures_typing_and_shows_the_host() {
        let (mut r, _) = RootModel::fleet(METRICS, SIZE, 1.0);
        r.begin_connect();
        assert!(scene_has(&r, "Connect"), "the prompt title shows");
        typed(&mut r, "kov@box");
        assert!(
            scene_has(&r, "kov@box"),
            "the typed host shows in the field"
        );
        // Editing chords work: backspace trims the last char.
        key(&mut r, Key::Named(NamedKey::Backspace), Mods::NONE);
        assert!(scene_has(&r, "kov@bo") && !scene_has(&r, "kov@box"));
    }

    #[test]
    fn submitting_a_host_begins_connecting_over_the_transport() {
        let (mut r, _) = RootModel::fleet(METRICS, SIZE, 1.0);
        r.begin_connect();
        typed(&mut r, "dev@example");
        let cmds = key(&mut r, Key::Named(NamedKey::Enter), Mods::NONE);
        let spec = ghost_vt::connection::ConnectionSpec::parse_target("dev@example").unwrap();
        // Submitting the host begins the transport connect — no password inline.
        assert_eq!(cmds, vec![Cmd::ConnectSshWindow { spec }]);
        // The prompt stays up in its "connecting" phase (auth may still ask).
        assert!(scene_has(&r, "Connecting"), "shows the connecting phase");
        assert!(r.is_connecting());
    }

    #[test]
    fn ssh_asking_shows_a_masked_password_field_and_submits_it() {
        let (mut r, _) = RootModel::fleet(METRICS, SIZE, 1.0);
        r.begin_connect();
        typed(&mut r, "dev@example");
        key(&mut r, Key::Named(NamedKey::Enter), Mods::NONE); // → Connecting

        // The shell relays ssh's own prompt; the password field appears in place
        // of the host, labelled with that wording.
        r.connect_request_password("dev@example's password:".into());
        assert!(scene_has(&r, "password:"), "ssh's prompt labels the field");
        assert!(
            !scene_has(&r, "dev@example") || scene_has(&r, "dev@example's password:"),
            "the host field is gone (only the prompt label mentions the host)"
        );

        // Typing renders masked, never in the clear.
        typed(&mut r, "s3cret");
        assert!(
            !scene_has(&r, "s3cret"),
            "the password is masked, never shown in the clear"
        );
        assert!(scene_has(&r, "\u{2022}"), "the password renders as bullets");

        // Enter feeds the secret straight through to the in-flight auth.
        let cmds = key(&mut r, Key::Named(NamedKey::Enter), Mods::NONE);
        assert_eq!(cmds, vec![Cmd::ConnectPassword("s3cret".to_string())]);
        assert!(
            scene_has(&r, "Connecting"),
            "back to connecting after submit"
        );
    }

    #[test]
    fn the_host_field_caret_is_a_block_overlay_that_does_not_shift_the_text() {
        let (mut r, _) = RootModel::fleet(METRICS, SIZE, 1.0);
        r.begin_connect();
        typed(&mut r, "asdasd");
        // Move the caret to the start. The text must stay put — no spliced cell
        // that pushes it right — and the block caret must not be a text glyph.
        key(&mut r, Key::Named(NamedKey::Home), Mods::NONE);
        let has_exact_run = |needle: &str| {
            r.view().layers.iter().any(|l| {
                l.items.iter().any(|it| {
                    matches!(it, SceneItem::Text { runs, .. }
                    if runs.iter().any(|run| run.text == needle))
                })
            })
        };
        assert!(
            has_exact_run("asdasd"),
            "the field text renders unshifted, as one run"
        );
        assert!(
            !scene_has(&r, "\u{2588}"),
            "the caret is a block overlay rect, not a glyph spliced into the text"
        );
    }

    #[test]
    fn a_very_long_host_does_not_panic_the_connect_overlay() {
        // A host long enough that its rendered field would exceed the window cap
        // made `field_w`'s lower bound (content width) exceed its upper bound
        // (90% of the window), and `f32::clamp` panics when min > max. The field
        // is capped at the window instead. Regression for that panic.
        let (mut r, _) = RootModel::fleet(METRICS, SIZE, 1.0);
        r.begin_connect();
        typed(&mut r, &"a".repeat(300));
        // Rendering drives the `field_w` computation; it must not panic.
        let _ = r.view();
    }

    #[test]
    fn a_failed_connect_shows_the_error_and_enter_retries_from_the_host() {
        let (mut r, _) = RootModel::fleet(METRICS, SIZE, 1.0);
        r.begin_connect();
        typed(&mut r, "dev@example");
        key(&mut r, Key::Named(NamedKey::Enter), Mods::NONE); // → Connecting
        r.connect_failed("Permission denied".into());
        assert!(scene_has(&r, "Permission denied"), "the error shows");
        // Enter returns to the host field (its text preserved) to try again.
        key(&mut r, Key::Named(NamedKey::Enter), Mods::NONE);
        assert!(
            scene_has(&r, "dev@example"),
            "host text preserved for a retry"
        );
        assert!(scene_has(&r, "Host"), "back on the host field");
    }

    #[test]
    fn the_copy_chord_lifts_the_connect_error_to_the_clipboard() {
        let (mut r, _) = RootModel::fleet(METRICS, SIZE, 1.0);
        r.begin_connect();
        typed(&mut r, "dev@example");
        key(&mut r, Key::Named(NamedKey::Enter), Mods::NONE); // → Connecting
        let msg = "command-line line 0: keyword controlpath extra arguments at end of line";
        r.connect_failed(msg.into());
        let cmds = key(&mut r, Key::Char("c".into()), copy_mods());
        assert!(
            cmds.contains(&Cmd::WriteClipboard(msg.to_string())),
            "the copy chord copies the shown error: {cmds:?}"
        );
        // Back on the host field there's no error, so the chord copies nothing.
        key(&mut r, Key::Named(NamedKey::Enter), Mods::NONE); // retry → Host
        let cmds = key(&mut r, Key::Char("c".into()), copy_mods());
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::WriteClipboard(_))),
            "nothing to copy outside the error phase: {cmds:?}"
        );
    }

    #[test]
    fn clicking_the_connect_error_copies_it_and_other_clicks_are_swallowed() {
        let (mut r, _) = RootModel::fleet(METRICS, SIZE, 1.0);
        r.begin_connect();
        typed(&mut r, "dev@example");
        key(&mut r, Key::Named(NamedKey::Enter), Mods::NONE); // → Connecting
        let msg = "keyword controlpath extra arguments at end of line";
        r.connect_failed(msg.into());
        // Click where the error is actually drawn: this proves the hit rect
        // (connect_error_rect) agrees with what connect_scene renders.
        let rect = r
            .view()
            .layers
            .iter()
            .flat_map(|l| &l.items)
            .find_map(|it| match it {
                SceneItem::Text { rect, runs, .. } if runs.iter().any(|run| run.text == msg) => {
                    Some(*rect)
                }
                _ => None,
            })
            .expect("the error line is drawn");
        let cmds = click(&mut r, rect.x + rect.w * 0.5, rect.y + rect.h * 0.5);
        assert!(
            cmds.contains(&Cmd::WriteClipboard(msg.to_string())),
            "clicking the error copies it: {cmds:?}"
        );
        // A click away from the message copies nothing and doesn't leak to the
        // view beneath (the modal is exclusive while open).
        let cmds = click(&mut r, 1.0, 1.0);
        assert!(
            cmds.is_empty(),
            "a click off the message is inert: {cmds:?}"
        );
    }

    #[test]
    fn copying_the_error_flashes_a_transient_copied_confirmation() {
        let (mut r, _) = RootModel::fleet(METRICS, SIZE, 1.0);
        r.begin_connect();
        typed(&mut r, "dev@example");
        key(&mut r, Key::Named(NamedKey::Enter), Mods::NONE); // → Connecting
        r.connect_failed("boom".into());
        assert!(!scene_has(&r, "Copied"), "no confirmation before a copy");

        // Copying arms the flash: it shows at once (before any clock tick) and
        // asks for an immediate tick to stamp its deadline.
        let cmds = key(&mut r, Key::Char("c".into()), copy_mods());
        assert!(cmds.contains(&Cmd::WriteClipboard("boom".into())));
        assert!(
            cmds.contains(&Cmd::ScheduleTick { after_ms: 0 }),
            "an immediate tick is requested to stamp the deadline: {cmds:?}"
        );
        assert!(scene_has(&r, "Copied"), "the confirmation shows right away");

        // The first tick stamps the deadline (and schedules the tick that clears
        // it); the flash stays up until then.
        let cmds = r.update(UiEvent::Tick { now_ms: 1_000 });
        assert!(
            cmds.contains(&Cmd::ScheduleTick {
                after_ms: COPIED_FLASH_MS
            }),
            "the clearing tick is scheduled from a fresh clock: {cmds:?}"
        );
        assert!(
            scene_has(&r, "Copied"),
            "still shown within the flash window"
        );

        // A stray tick before the deadline must NOT clear it early (this is why
        // the deadline is stamped from a real clock, not the copy-time state).
        r.update(UiEvent::Tick {
            now_ms: 1_000 + COPIED_FLASH_MS - 1,
        });
        assert!(scene_has(&r, "Copied"), "not cleared before the deadline");

        // A tick past the deadline clears it.
        r.update(UiEvent::Tick {
            now_ms: 1_000 + COPIED_FLASH_MS,
        });
        assert!(!scene_has(&r, "Copied"), "the confirmation is transient");
    }

    #[test]
    fn staging_progress_shows_a_percentage_and_a_bar() {
        let (mut r, _) = RootModel::fleet(METRICS, SIZE, 1.0);
        r.begin_connect();
        typed(&mut r, "dev@example");
        key(&mut r, Key::Named(NamedKey::Enter), Mods::NONE); // → Connecting (Working)
        assert!(
            scene_has(&r, "Connecting"),
            "plain connecting before staging"
        );

        // A staging update switches the line to a percentage and draws a bar.
        r.connect_progress(3, 4);
        assert!(scene_has(&r, "Staging"), "shows the staging line");
        assert!(scene_has(&r, "75%"), "shows the byte percentage");
        // The bar is a filled rect on the modal layer, narrower than the track.
        let scene = r.view();
        let rects: Vec<f32> = scene.layers[0]
            .items
            .iter()
            .filter_map(|it| match it {
                SceneItem::Rect { rect, .. } => Some(rect.w),
                _ => None,
            })
            .collect();
        assert!(
            rects.len() >= 2,
            "a track and a fill rect are present: {rects:?}"
        );

        // Progress is ignored once we're no longer connecting.
        r.connect_failed("nope".into());
        r.connect_progress(1, 4);
        assert!(scene_has(&r, "nope"), "progress doesn't clobber the error");
    }

    #[test]
    fn connecting_an_ssh_window_persists_the_group_connection() {
        // Mirror the shell's Cmd::ConnectSshWindow: mark the (fleet-mode) window's
        // group an ssh group, then adopt its freshly spawned first session. The
        // adopt's registry save must carry the connection — else the group loses
        // its ssh identity on disk and never shows the badge.
        let (mut r, _) = RootModel::fleet(METRICS, SIZE, 1.0);
        let spec = ghost_vt::connection::ConnectionSpec::parse_target("kov@box").unwrap();
        r.set_group_connection(Some(spec.clone()));
        let cmds = r.update(UiEvent::AdoptSession("s1".into()));
        let saved = cmds
            .iter()
            .find_map(|c| match c {
                Cmd::SaveGroups(gs) => Some(gs),
                _ => None,
            })
            .expect("the adopt saves the group registry");
        assert!(
            saved.iter().any(|g| g.connection.as_ref() == Some(&spec)),
            "the persisted group carries the ssh connection: {saved:?}"
        );
    }

    #[test]
    fn an_empty_or_invalid_host_keeps_prompting() {
        let (mut r, _) = RootModel::fleet(METRICS, SIZE, 1.0);
        r.begin_connect();
        let cmds = key(&mut r, Key::Named(NamedKey::Enter), Mods::NONE);
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, Cmd::ConnectSshWindow { .. })),
            "an empty host does not open a window"
        );
        assert!(scene_has(&r, "Connect"), "still prompting");
    }

    #[test]
    fn escape_cancels_the_connect_prompt_and_closes_the_window() {
        let (mut r, _) = RootModel::fleet(METRICS, SIZE, 1.0);
        r.begin_connect();
        typed(&mut r, "abc");
        assert_eq!(
            key(&mut r, Key::Named(NamedKey::Escape), Mods::NONE),
            vec![Cmd::CloseWindow]
        );
        assert!(!scene_has(&r, "Connect"), "prompt cleared on cancel");
    }

    #[test]
    fn new_ssh_session_is_bound_to_the_go_key() {
        // "go" mnemonic: Cmd+G (macOS) / Ctrl+Shift+G elsewhere opens the connect
        // prompt in THIS window (a new ssh session/tab), mirroring the Cmd+S gating
        // that keeps a control char free — here bare Ctrl+G must stay BEL.
        for chord in [Mods::SUPER, Mods::CTRL | Mods::SHIFT] {
            let mut r = root();
            assert_eq!(
                key(&mut r, Key::Char("g".into()), chord),
                vec![Cmd::NewSshSession],
                "Cmd+G / Ctrl+Shift+G opens a new ssh session in this window"
            );
        }
        // Bare Ctrl+G is NOT a shortcut — it stays BEL (^G) to the child.
        let mut r = root();
        assert!(matches!(
            key(&mut r, Key::Char("g".into()), Mods::CTRL).as_slice(),
            [Cmd::SendInput { .. }]
        ));
        // On Linux, Alt+G also opens a new ssh session (mirroring Alt+S); on macOS
        // Alt stays Option/Meta and is encoded to the child instead.
        #[cfg(not(target_os = "macos"))]
        {
            let mut r = root();
            assert_eq!(
                key(&mut r, Key::Char("g".into()), Mods::ALT),
                vec![Cmd::NewSshSession]
            );
        }
        // Like the other window/session chords, it also fires in the fleet overview.
        let (mut f, _) = RootModel::fleet(METRICS, SIZE, 1.0);
        assert_eq!(
            key(&mut f, Key::Char("g".into()), Mods::SUPER),
            vec![Cmd::NewSshSession]
        );
    }

    #[test]
    fn a_session_connect_submits_to_this_window_not_a_new_one() {
        // The connect prompt opened for a new *session* (Cmd+G) submits
        // `ConnectSshSession` — the shell adopts the remote session as a tab in this
        // window, and (unlike ConnectSshWindow) does NOT make the window an ssh group.
        let mut r = root();
        r.begin_connect_session();
        typed(&mut r, "dev@example");
        let cmds = key(&mut r, Key::Named(NamedKey::Enter), Mods::NONE);
        let spec = ghost_vt::connection::ConnectionSpec::parse_target("dev@example").unwrap();
        assert_eq!(cmds, vec![Cmd::ConnectSshSession { spec }]);
        assert!(scene_has(&r, "Connecting"), "shows the connecting phase");
        assert!(r.is_connecting());
    }

    #[test]
    fn escape_in_a_session_connect_dismisses_without_closing_the_window() {
        // Escaping a new-session connect must NOT close the window (it holds a live
        // session): it cancels the in-flight connect and returns to that session.
        let mut r = root();
        r.begin_connect_session();
        typed(&mut r, "abc");
        let cmds = key(&mut r, Key::Named(NamedKey::Escape), Mods::NONE);
        assert_eq!(cmds, vec![Cmd::CancelConnect]);
        assert!(!scene_has(&r, "Connect"), "prompt cleared on cancel");
        assert!(!r.is_connecting());
        // The window-mode escape still closes the (empty) ssh window — regression guard.
        let (mut w, _) = RootModel::fleet(METRICS, SIZE, 1.0);
        w.begin_connect();
        assert_eq!(
            key(&mut w, Key::Named(NamedKey::Escape), Mods::NONE),
            vec![Cmd::CloseWindow]
        );
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

    /// Feed a session an OSC 2 window-title change.
    fn set_title(r: &mut RootModel, name: &str, title: &str) -> Vec<Cmd> {
        r.update(UiEvent::SessionData {
            name: name.to_string(),
            bytes: format!("\x1b]2;{title}\x07").into_bytes(),
            ended: false,
        })
    }

    #[test]
    fn the_foreground_session_retitles_the_window() {
        let mut r = root(); // foreground alpha
        let cmds = set_title(&mut r, "alpha", "editing README");
        assert!(
            cmds.contains(&Cmd::SetTitle("editing README".into())),
            "the foreground session drives the window title: {cmds:?}"
        );
    }

    #[test]
    fn a_background_session_does_not_retitle_the_window() {
        let mut r = root(); // foreground alpha
        r.update(UiEvent::AdoptSession("beta".into())); // beta foreground, alpha warm
        // alpha is now a warm background mirror; its OSC title change must stay put.
        let cmds = set_title(&mut r, "alpha", "background noise");
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::SetTitle(_))),
            "a background session must not retitle the window: {cmds:?}"
        );
    }

    #[test]
    fn switching_the_foreground_restores_that_sessions_title() {
        let mut r = root(); // foreground alpha
        set_title(&mut r, "alpha", "alpha-title");
        r.update(UiEvent::AdoptSession("beta".into())); // beta foreground, alpha warm
        set_title(&mut r, "beta", "beta-title");
        // Switching back to alpha must reassert alpha's remembered title, not leave
        // the window showing beta's.
        let cmds = r.update(UiEvent::AdoptSession("alpha".into()));
        assert!(
            cmds.contains(&Cmd::SetTitle("alpha-title".into())),
            "switching the foreground restores that session's title: {cmds:?}"
        );
    }

    #[test]
    fn adopting_a_titleless_session_shows_its_name() {
        let mut r = root(); // foreground alpha
        // beta has set no OSC title yet: the window falls back to its session name
        // rather than lingering on alpha's title or going blank.
        let cmds = r.update(UiEvent::AdoptSession("beta".into()));
        assert!(
            cmds.contains(&Cmd::SetTitle("beta".into())),
            "a titleless foreground shows its session name: {cmds:?}"
        );
    }

    #[test]
    fn a_session_list_teaches_the_foreground_its_display_name() {
        let mut r = root(); // single view of alpha, no OSC title
        // The reconcile carries the display name a rename (possibly from another
        // window) gave the session; the foreground's window title follows it.
        let mut s = sess("alpha", true, 1);
        s.display_name = "build box".into();
        let cmds = r.update(UiEvent::SessionList(vec![s]));
        assert!(
            cmds.contains(&Cmd::SetTitle("build box".into())),
            "learning a display name retitles the window: {cmds:?}"
        );
        // An unchanged list does not re-emit.
        let mut s = sess("alpha", true, 1);
        s.display_name = "build box".into();
        let cmds = r.update(UiEvent::SessionList(vec![s]));
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::SetTitle(_))),
            "an unchanged display name must not retitle: {cmds:?}"
        );
    }

    #[test]
    fn a_rename_of_a_titled_foreground_prefixes_the_window_title() {
        let mut r = root(); // single view of alpha
        set_title(&mut r, "alpha", "vim");
        // A rename (possibly from another window) arrives via the reconcile;
        // the window title gains the label as a prefix, keeping the app title.
        let mut s = sess("alpha", true, 1);
        s.display_name = "build box".into();
        let cmds = r.update(UiEvent::SessionList(vec![s]));
        assert!(
            cmds.contains(&Cmd::SetTitle("build box — vim".into())),
            "a custom label prefixes the foreground's app title: {cmds:?}"
        );
    }

    #[test]
    fn diving_back_in_reasserts_the_foreground_title() {
        let mut r = root(); // single view of alpha
        set_title(&mut r, "alpha", "vim");
        key(&mut r, Key::Named(NamedKey::F9), Mods::NONE); // dive out to the fleet
        // While overviewing, alpha changes its title. The fleet filters it, so the
        // window keeps showing "vim" even though alpha's title is now "alpha:~".
        let cmds = set_title(&mut r, "alpha", "alpha:~");
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::SetTitle(_))),
            "the fleet must not retitle the window for a tile: {cmds:?}"
        );
        // Diving back in (F9) must reassert alpha's current title, not leave the
        // titlebar stale on "vim".
        let cmds = key(&mut r, Key::Named(NamedKey::F9), Mods::NONE);
        assert!(
            cmds.contains(&Cmd::SetTitle("alpha:~".into())),
            "diving back into a session reasserts its title: {cmds:?}"
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

    /// The foreground session's child exited (the shell was quit).
    fn end_foreground(r: &mut RootModel, name: &str) -> Vec<Cmd> {
        r.update(UiEvent::SessionData {
            name: name.into(),
            bytes: Vec::new(),
            ended: true,
        })
    }

    #[test]
    fn exiting_the_foreground_shell_switches_to_the_next_session_not_quit() {
        let mut r = root();
        with_three(&mut r); // owns alpha, beta, gamma; foreground gamma
        let cmds = end_foreground(&mut r, "gamma");
        // The window stays a live single view — it must not end/close.
        assert!(
            !r.is_fleet(),
            "other sessions remain, so stay in the single view"
        );
        // Switches to the forward-cycle successor of gamma (wraps to alpha), the
        // same target Ctrl-Tab would pick — via the warm mirror, no re-attach.
        assert_eq!(foreground(&mut r), "alpha");
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, Cmd::Attach(_) | Cmd::TakeOver(_) | Cmd::Spawn { .. })),
            "the next session is already attached; no re-attach: {cmds:?}"
        );
    }

    #[test]
    fn exiting_the_last_shell_shows_the_fleet_not_quit() {
        let mut r = root(); // owns only alpha (foreground)
        let cmds = end_foreground(&mut r, "alpha");
        assert!(
            r.is_fleet(),
            "no sessions left in the window -> fleet overview, not quit"
        );
        assert!(
            cmds.contains(&Cmd::ListSessions),
            "the fleet repopulates from the host: {cmds:?}"
        );
    }

    #[test]
    fn ctrl_tab_plays_a_slide_between_the_two_sessions() {
        let mut r = root();
        with_three(&mut r); // foreground gamma; owns alpha, beta, gamma
        assert!(!r.is_animating(), "settled before the cycle");

        let cmds = ctrl_tab(&mut r, false); // forward: gamma -> alpha
        assert!(r.is_animating(), "Ctrl-Tab plays a slide");
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::ScheduleTick { .. })),
            "the slide schedules its first frame: {cmds:?}"
        );
        // Mid-slide the leaving and arriving sessions are both drawn.
        assert_eq!(
            r.view().terminals().count(),
            2,
            "both sessions are on screen during the slide"
        );

        // It settles to just the new foreground — which input already routes to,
        // the swap being instant.
        let ticks = settle(&mut r);
        assert!(ticks > 1, "the slide ran for several frames, not one");
        assert_eq!(r.view().terminals().count(), 1, "settles to one view");
        assert_eq!(foreground(&mut r), "alpha");
    }

    #[test]
    fn a_settling_slide_releases_the_foregrounds_synchronized_hold() {
        // The freeze this reproduces: a session repositioning mid-frame emits DEC
        // 2026 (begin synchronized output) and pauses before the matching reset,
        // so the terminal correctly holds its repaint pending a release tick. But
        // while a slide plays, the animation owns the tick stream (`tick_anim`) and
        // swallows every tick — including that release. On completion the tick must
        // be handed back to the foreground, or the hold latches: a still-held
        // session never re-arms its backstop, so later output accumulates unseen
        // and the view stays frozen until some input forces a full repaint. The
        // user hit exactly this — a terminal stuck until a scroll revived it.
        let mut r = root();
        with_three(&mut r); // owns alpha, beta, gamma; foreground gamma

        // Ctrl-Shift-Tab slides backward into beta, which becomes the foreground
        // (see `ctrl_shift_tab_cycles_to_the_previous_attached_session`).
        ctrl_tab(&mut r, true); // gamma -> beta
        assert!(r.is_animating(), "the cycle plays a slide");

        // Mid-slide beta opens a synchronized frame and stops before closing it:
        // the repaint is held (no redraw) and a release backstop is scheduled.
        let held = feed(&mut r, "beta", b"\x1b[?2026hhello");
        assert!(
            !held.contains(&Cmd::Redraw),
            "a mid-frame feed is held, not painted: {held:?}"
        );
        assert!(
            held.iter().any(|c| matches!(c, Cmd::ScheduleTick { .. })),
            "the hold arms a release backstop: {held:?}"
        );

        // The slide settles; its final tick must release beta's hold.
        settle(&mut r);
        assert!(!r.is_animating(), "the slide completed");

        // Further held output must still be able to arm a backstop — proof the
        // hold was released at settle rather than latched. With the bug (tick_anim
        // drops the completion tick in the single view) beta stays held and
        // schedules nothing, so its screen never catches up.
        let more = feed(&mut r, "beta", b"world");
        assert!(
            more.iter().any(|c| matches!(c, Cmd::ScheduleTick { .. })),
            "a settled slide must release the sync hold so later frames re-arm a backstop: {more:?}"
        );
    }

    #[test]
    fn slide_terminals_carry_distinct_sessions_so_the_texture_cache_wont_collide() {
        // The renderer caches each terminal's rastered texture by SESSION. Both sliding
        // terminals are SceneId::Root (the single view's id), so if they didn't carry
        // distinct sessions they'd evict each other every frame (defeating the
        // render-once-composite-many win). Their sessions must differ.
        let mut r = root();
        with_three(&mut r);
        ctrl_tab(&mut r, false);
        let scene = r.view();
        let sessions: Vec<u64> = scene
            .terminals()
            .filter_map(|t| match t {
                SceneItem::Terminal { session, .. } => Some(*session),
                _ => None,
            })
            .collect();
        assert_eq!(
            sessions.len(),
            2,
            "both sessions are drawn during the slide"
        );
        assert_ne!(
            sessions[0], sessions[1],
            "the outgoing and incoming terminals need distinct cache sessions"
        );
    }

    #[test]
    fn exiting_the_foreground_shell_slides_to_the_next_session() {
        let mut r = root();
        with_three(&mut r); // foreground gamma
        end_foreground(&mut r, "gamma");

        // The auto-switch to the next session plays a slide, with the dead
        // session's frozen last frame as the outgoing stand-in.
        assert!(r.is_animating(), "the auto-switch plays a slide");
        assert_eq!(
            r.view().terminals().count(),
            2,
            "the dead session's stand-in slides out as the next slides in"
        );

        settle(&mut r);
        assert!(!r.is_animating());
        assert!(!r.is_fleet(), "still a live single view");
        assert_eq!(r.view().terminals().count(), 1);
        assert_eq!(foreground(&mut r), "alpha");
    }

    #[test]
    fn a_slide_interpolates_the_two_sides_to_half_width_at_mid_progress() {
        // The unified animation is a transform timeline: each side is a frozen scene
        // carried by a from->to translate. At half (eased) progress the outgoing side
        // has slid half a window-width toward the edge it's leaving and the incoming
        // side sits half a width in from the edge it entered. This pins the actual
        // interpolated geometry, not just that "two terminals are on screen".
        let mut r = root();
        with_three(&mut r); // foreground gamma; sorted alpha, beta, gamma
        ctrl_tab(&mut r, false); // forward (gamma -> alpha): incoming arrives from the right

        let w = SIZE.0 as f32;
        // The first tick stamps the start (progress 0); a tick at half the duration
        // lands exactly mid-timeline, since ease_in_out(0.5) == 0.5.
        let base = 10_000u64;
        r.update(UiEvent::Tick { now_ms: base });
        r.update(UiEvent::Tick {
            now_ms: base + ANIM_MS / 2,
        });

        // Identify the sides by where they slid: the outgoing moves left (negative tx),
        // the incoming sits to its right. Both are SceneId::Root now — they are told
        // apart only by their distinct sessions, which is exactly how the renderer
        // caches each side's texture independently.
        let mut sides: Vec<(u64, f32)> = r
            .view()
            .layers
            .iter()
            .flat_map(|l| l.items.iter().map(move |it| (l.transform.tx, it)))
            .filter_map(|(tx, it)| match it {
                SceneItem::Terminal { session, .. } => Some((*session, tx)),
                _ => None,
            })
            .collect();
        sides.sort_by(|a, b| a.1.total_cmp(&b.1)); // ascending tx: outgoing (left) first
        assert_eq!(sides.len(), 2, "both sides are on screen mid-slide");
        let (out_session, out_tx) = sides[0];
        let (in_session, in_tx) = sides[1];
        assert_ne!(
            out_session, in_session,
            "the two sides carry distinct sessions"
        );
        assert_eq!(
            out_session,
            ghost_render::session_key("gamma"),
            "the left side is the outgoing (foreground) session"
        );
        assert_eq!(
            in_session,
            ghost_render::session_key("alpha"),
            "the right side is the incoming session"
        );
        assert!(
            (out_tx + w / 2.0).abs() < 0.5,
            "the outgoing side slid half a width left, got tx={out_tx}"
        );
        assert!(
            (in_tx - w / 2.0).abs() < 0.5,
            "the incoming side sits half a width in from the right, got tx={in_tx}"
        );
    }

    /// Each terminal's (session, shared frame) in `scene`.
    fn frames_by_session(scene: &Scene) -> HashMap<u64, std::rc::Rc<ghost_render::Frame>> {
        scene
            .layers
            .iter()
            .flat_map(|l| &l.items)
            .filter_map(|it| match it {
                SceneItem::Terminal { session, frame, .. } => Some((*session, frame.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_slide_shares_frozen_frames_across_ticks_instead_of_deep_cloning() {
        // The payoff of compositing cached textures: during an animation the frozen
        // content is not re-copied each tick — only the camera moves. Pin it
        // structurally — the frame each side carries is the SAME `Rc` allocation across
        // two consecutive ticks, so `Anim::scene` shares it (a refcount bump) rather than
        // deep-cloning the laid-out rows/runs/strings every frame (the cost that made a
        // colorized session jank the animation while a plain one didn't).
        let mut r = root();
        with_three(&mut r);
        ctrl_tab(&mut r, false); // start a slide (gamma -> alpha)

        let base = 10_000u64;
        r.update(UiEvent::Tick { now_ms: base }); // stamps progress 0
        let a = frames_by_session(&r.view());
        r.update(UiEvent::Tick {
            now_ms: base + ANIM_MS / 4,
        }); // still mid-slide, camera moved
        let b = frames_by_session(&r.view());

        assert_eq!(a.len(), 2, "both slide sides are on screen");
        for (session, fa) in &a {
            let fb = b.get(session).expect("the same sessions on the next tick");
            assert!(
                std::rc::Rc::ptr_eq(fa, fb),
                "session {session:#018x}'s frame must be shared across ticks, not deep-cloned"
            );
        }
    }
}
