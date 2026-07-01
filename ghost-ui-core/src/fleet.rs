//! `FleetModel` — the overview grid of session previews, as a pure reducer.
//!
//! Sessions this window drives are fed by the shell and mirrored by a per-tile
//! [`TerminalModel`], so their previews are live — laid out at the session's
//! real size and scaled to the tile at draw time. The fleet never attaches
//! sessions itself; a tile with no live source renders as a placeholder. The
//! model reconciles a `SessionList` into tiles bucketed by attach-state section,
//! routes pumped `SessionData` to the matching tile by id, moves focus on
//! arrow/Tab navigation (previews are view-only, so keystrokes/text are never
//! forwarded), focuses on click via a shared grid layout that doubles as the
//! hit-test, and renders the whole grid — dimming unfocused tiles, bordering the
//! focused one, and badging bell/activity. Like [`TerminalModel`] it is pure, so
//! all of this is asserted headlessly by feeding events and inspecting the
//! returned `Vec<Cmd>`, the focused id, per-tile screen text, and the `Scene`.

use std::collections::HashSet;

use std::rc::Rc;

use ghost_render::{
    BadgeKind, CellMetrics, Frame, Layer, RectPx, Rgba, Run, Scene, SceneId, SceneItem, Style,
    Transform, layout_frame,
};
use ghost_vt::session::SessionInfo;

use crate::input::{Key, Mods, NamedKey};
use crate::{Cmd, PointPx, PointerPhase, SessionId, TerminalModel, UiEvent};

const GAP: f32 = 10.0;
const FOCUS_BORDER: f32 = 2.0;
const FOCUS_COLOR: Rgba = [0.30, 0.60, 0.95, 1.0];
const BADGE_PX: f32 = 10.0;
/// Colour of section header labels.
const SECTION_LABEL_COLOR: Rgba = [0.65, 0.70, 0.78, 1.0];
/// Preview area fill and the muted hint drawn on a tile with no live source.
const PLACEHOLDER_BG: Rgba = [0.10, 0.11, 0.14, 1.0];
const PLACEHOLDER_FG: Rgba = [0.42, 0.46, 0.54, 1.0];
/// Default grid for a fleet-created tile mirror until the shell hands it a
/// real-size model; previews are scaled to the tile regardless of this size.
/// Also the assumed aspect ratio used to shape tiles into little terminals.
const PREVIEW_COLS: u16 = 80;
const PREVIEW_ROWS: u16 = 24;
/// Most cards per row; the grid fits as many aspect-locked cards across as the
/// window width allows, up to this many.
const MAX_PER_ROW: usize = 8;
/// A card never shrinks below this many preview lines tall (the two chrome bands
/// are extra). With many sessions the grid scrolls rather than collapsing every
/// preview to an unreadable sliver; this floor guards small windows / odd metrics.
const MIN_PREVIEW_LINES: f32 = 8.0;
/// The COMPACT preview size (fraction of the session's native size) used when the
/// grid is crowded — a readable thumbnail. A few sessions grow ABOVE this, up to
/// native (1:1, beyond which a preview can't get sharper), to use the space; a
/// crowded grid shrinks to it and scrolls. Scale-aware (relative to native).
const PREVIEW_COMPACT_SCALE: f32 = 0.5;
/// A card grows no taller than this fraction of the viewport — a bit under half —
/// so even with few sessions a section header and other cards stay visible on a
/// short window (rather than one card filling the screen). Only binds when the
/// window is short; tall windows are capped by native size instead.
const MAX_CARD_VIEWPORT_FRAC: f32 = 0.45;
/// Lines of vertical scroll per mouse-wheel notch (sign only, like the terminal's
/// scrollback — magnitude is ignored so a touchpad and a notched wheel agree).
const SCROLL_LINES: f32 = 3.0;
/// Card chrome colours (metadata header, button footer).
const CARD_META_COLOR: Rgba = [0.62, 0.66, 0.74, 1.0];
const CARD_BG: Rgba = [0.07, 0.08, 0.10, 1.0];
const BUTTON_BG: Rgba = [0.17, 0.19, 0.24, 1.0];
const BUTTON_FG: Rgba = [0.80, 0.83, 0.89, 1.0];
/// Confirm-overlay colours (a scrim and its prompt text).
const OVERLAY_BG: Rgba = [0.04, 0.04, 0.06, 0.82];
const OVERLAY_FG: Rgba = [0.92, 0.94, 0.97, 1.0];
/// How often (ms) the fleet asks the shell to re-enumerate sessions.
const REFRESH_MS: u64 = 500;

/// A laid-out tile: stable handle, session id, and pixel rect.
type Placement = (u64, SessionId, RectPx);
/// A section header: its locality and the header band's rect.
type SectionHeader = (Locality, RectPx);

/// A per-card action button.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Button {
    Kill,
    Detach,
    Rename,
}

impl Button {
    fn label(self) -> &'static str {
        match self {
            Button::Kill => "kill",
            Button::Detach => "detach",
            Button::Rename => "rename",
        }
    }
}

/// An action awaiting a yes/no confirmation (a modal overlay).
struct Pending {
    id: SessionId,
    action: PendingAction,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PendingAction {
    /// Steal a session held by another window into this one.
    TakeOver,
    /// Kill the session and its process.
    Kill,
}

/// An in-progress inline rename of a tile.
struct Renaming {
    id: SessionId,
    buffer: String,
}

/// Which window owns or sees a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Locality {
    ThisWindow,
    Elsewhere,
    Detached,
}

impl Locality {
    fn rank(self) -> u8 {
        match self {
            Locality::ThisWindow => 0,
            Locality::Elsewhere => 1,
            Locality::Detached => 2,
        }
    }
}

fn locality_for(mine: &HashSet<SessionId>, id: &str, attached: bool) -> Locality {
    if mine.contains(id) {
        Locality::ThisWindow
    } else if attached {
        Locality::Elsewhere
    } else {
        Locality::Detached
    }
}

/// Stable, deterministic sort key for a tile *within* its section — and, when the
/// grouping feature lands, within its group (the comparator is hierarchy-ready:
/// section/group first, then this key). Oldest session first by creation time so
/// a session keeps its slot for life and new ones land at the end; a tile with no
/// recorded creation time sorts last (newest) until reconcile fills it in.
///
/// The tie-break is the session name, not the tile's `handle`: `created_at` is
/// millisecond-resolution but sessions spawned in the same millisecond (rapid
/// scripted launches) still tie, and `handle` is assigned in `SessionList`
/// enumeration order — which varies (directory-read order) and resets every time a
/// fresh fleet is built (each F9 / dive-back). A handle tie-break therefore lets
/// tied tiles swap slots between rebuilds; the globally-unique, stable name never
/// does.
fn tile_order_key(t: &Tile) -> (i64, &str) {
    (t.created_at.unwrap_or(i64::MAX), t.id.as_str())
}

struct Tile {
    handle: u64,
    id: SessionId,
    model: TerminalModel,
    bell: bool,
    locality: Locality,
    /// The command the session runs (empty = the user's `$SHELL`), shown in the
    /// card header.
    command: Vec<String>,
    /// The session's process id, shown in the card header.
    pid: i32,
    /// Unix seconds at which the session was created, or `None` if the host
    /// hasn't recorded it (or the tile was adopted before its first reconcile).
    /// The primary, stable sort key (see [`tile_order_key`]).
    created_at: Option<i64>,
    /// Unseen-output count since this tile was last focused (drives the badge).
    activity: u32,
    /// Whether the shell has delivered output for this tile (i.e. this window
    /// drives the session). An unfed tile renders as a placeholder, not a live
    /// preview.
    fed: bool,
    /// Cached laid-out preview, rebuilt only when this tile's content or size
    /// changes (see [`FleetModel::refresh_dirty_frames`]); `view` clones the `Rc`
    /// rather than re-running `layout_frame` for every tile every frame. Sharing the
    /// SAME `Rc` across presents is what lets the renderer skip re-rastering an
    /// unchanged tile's Surface (an `Rc::ptr_eq` cache hit). `None` until the first
    /// refresh.
    frame: Option<Rc<Frame>>,
    /// Set when `frame` is stale (the tile got output or was resized) so the next
    /// refresh rebuilds it. Focus/bell/activity changes do not set this — they
    /// affect only the border/badge/selection, which `view` composes separately.
    frame_dirty: bool,
}

pub struct FleetModel {
    tiles: Vec<Tile>,
    focused: Option<SessionId>,
    /// Base (1x) cell metrics; physical metrics are these scaled by `scale`.
    metrics: CellMetrics,
    /// Device scale factor, propagated to every tile so previews track HiDPI.
    scale: f32,
    size_px: (u32, u32),
    mine: HashSet<SessionId>,
    next_handle: u64,
    /// Count of tile-frame (re)builds — bumped only when a dirty tile is actually
    /// laid out, never on a cache hit. Lets a test prove unchanged tiles are reused.
    frame_builds: u32,
    /// An action awaiting confirmation (kill, or stealing a session held
    /// elsewhere); drives a modal confirm overlay and swallows input until resolved.
    pending: Option<Pending>,
    /// An in-progress inline rename; swallows text/keys into its buffer.
    renaming: Option<Renaming>,
    /// Vertical scroll offset in physical pixels (0 = top). The grid lays out at a
    /// readable tile size regardless of session count and scrolls when it overflows
    /// the viewport, rather than shrinking previews to fit.
    scroll_y: f32,
}

/// Split a tile rect into its metadata header, preview area, and a row of three
/// equal action buttons, given the chrome `band` height (shared by `view` and
/// pointer hit-testing so the buttons land exactly where they are drawn).
fn card_layout(rect: RectPx, band: f32) -> (RectPx, RectPx, [(Button, RectPx); 3]) {
    let header_h = band.min(rect.h);
    let footer_h = band.min((rect.h - header_h).max(0.0));
    let footer_y = rect.y + rect.h - footer_h;
    let header = RectPx {
        x: rect.x,
        y: rect.y,
        w: rect.w,
        h: header_h,
    };
    let preview = RectPx {
        x: rect.x,
        y: rect.y + header_h,
        w: rect.w,
        h: (footer_y - (rect.y + header_h)).max(0.0),
    };
    let bw = rect.w / 3.0;
    let button = |i: f32, b: Button| {
        (
            b,
            RectPx {
                x: rect.x + i * bw,
                y: footer_y,
                w: bw,
                h: footer_h,
            },
        )
    };
    let buttons = [
        button(0.0, Button::Kill),
        button(1.0, Button::Detach),
        button(2.0, Button::Rename),
    ];
    (header, preview, buttons)
}

/// A navigation action: a 2-D arrow direction over the grid, or a linear Tab
/// step `(delta, wrap)` through tiles in layout order.
enum Nav {
    Dir(Dir),
    Step(i32, bool),
}

#[derive(Clone, Copy)]
enum Dir {
    Up,
    Down,
    Left,
    Right,
}

/// Map a key to a navigation action, or `None` if it isn't one. Arrows move in
/// 2-D (down really goes down a row, crossing section boundaries); Shift+Tab
/// steps backward, Tab forward, both wrapping.
fn nav(key: &Key, mods: Mods) -> Option<Nav> {
    match key {
        Key::Named(NamedKey::ArrowUp) => Some(Nav::Dir(Dir::Up)),
        Key::Named(NamedKey::ArrowDown) => Some(Nav::Dir(Dir::Down)),
        Key::Named(NamedKey::ArrowLeft) => Some(Nav::Dir(Dir::Left)),
        Key::Named(NamedKey::ArrowRight) => Some(Nav::Dir(Dir::Right)),
        Key::Named(NamedKey::Tab) if mods.shift => Some(Nav::Step(-1, true)),
        Key::Named(NamedKey::Tab) => Some(Nav::Step(1, true)),
        _ => None,
    }
}

impl FleetModel {
    pub fn new(metrics: CellMetrics, size_px: (u32, u32), mine: HashSet<SessionId>) -> Self {
        FleetModel {
            tiles: Vec::new(),
            focused: None,
            metrics,
            scale: 1.0,
            size_px,
            mine,
            next_handle: 0,
            frame_builds: 0,
            pending: None,
            renaming: None,
            scroll_y: 0.0,
        }
    }

    /// Physical cell metrics: the base metrics scaled by the device scale factor.
    fn effective_metrics(&self) -> CellMetrics {
        CellMetrics {
            advance: self.metrics.advance * self.scale,
            line_height: self.metrics.line_height * self.scale,
        }
    }

    /// Render scale for the overview — device scale only (tiles auto-size to the
    /// grid, so the single view's user zoom doesn't apply here).
    pub fn render_scale(&self) -> f32 {
        self.scale
    }

    /// Start a fleet that already holds `primary` as its focused tile, so its
    /// screen state survives a toggle from the single-terminal view.
    pub fn adopting(
        primary: TerminalModel,
        warm: Vec<TerminalModel>,
        metrics: CellMetrics,
        size_px: (u32, u32),
        scale: f32,
        mine: HashSet<SessionId>,
    ) -> (Self, Vec<Cmd>) {
        let mut f = FleetModel::new(metrics, size_px, mine);
        f.scale = scale;
        let id = primary.session().to_string();
        f.focused = Some(id.clone());
        // The primary and the window's other driven sessions are all already
        // live; add each as a fed tile so every preview is warm, not "starting…".
        // Command/pid fill in on the next reconcile.
        for model in std::iter::once(primary).chain(warm) {
            let id = model.session().to_string();
            let locality = locality_for(&f.mine, &id, true);
            // No SessionInfo yet (these are live models handed over on a toggle);
            // creation time fills in on the next reconcile. Until then they sort
            // last — which is correct, they are the window's current sessions.
            f.push_tile(id, model, false, locality, Vec::new(), 0, None);
            if let Some(t) = f.tiles.last_mut() {
                t.fed = true;
            }
        }
        // The fleet never resizes its tiles' sessions; just repaint.
        (f, vec![Cmd::Redraw])
    }

    #[allow(clippy::too_many_arguments)]
    fn push_tile(
        &mut self,
        id: SessionId,
        model: TerminalModel,
        bell: bool,
        locality: Locality,
        command: Vec<String>,
        pid: i32,
        created_at: Option<i64>,
    ) {
        let handle = self.next_handle;
        self.next_handle += 1;
        self.tiles.push(Tile {
            handle,
            id,
            model,
            bell,
            locality,
            command,
            pid,
            created_at,
            activity: 0,
            fed: false,
            frame: None,
            frame_dirty: true,
        });
    }

    // ---- projections (for the shell + tests) ----

    pub fn focused(&self) -> Option<&str> {
        self.focused.as_deref()
    }

    pub fn tile_count(&self) -> usize {
        self.tiles.len()
    }

    /// The screen text of a tile, for assertions.
    pub fn tile_text(&self, id: &str) -> Option<Vec<String>> {
        self.tiles
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.model.screen().text())
    }

    pub fn locality_of(&self, id: &str) -> Option<Locality> {
        self.tiles.iter().find(|t| t.id == id).map(|t| t.locality)
    }

    /// The on-screen preview rect of a tile (post-scroll), exactly as [`view`](Self::view)
    /// draws the little terminal — the box the session's frame is fit into.
    /// `None` if the tile isn't present.
    pub fn preview_rect(&self, id: &str) -> Option<RectPx> {
        let (_, placements, band, _) = self.sections_layout();
        let (_, _, mut rect) = placements.into_iter().find(|(_, i, _)| i == id)?;
        rect.y -= self.scroll_y;
        Some(card_layout(rect, band).1)
    }

    /// The on-screen rect the session's *content* actually occupies in its tile: its
    /// frame contain-fit into the preview box, anchored at the box's top-left, exactly
    /// as the renderer draws the preview. This — not the preview box, which is shaped
    /// to a fixed aspect — is the camera target for the fleet-zoom, so a full zoom
    /// lands the session at native size and matches the live single view, with no
    /// scale jump at the dive boundary. `None` if the tile isn't present.
    pub fn dive_target_rect(&self, id: &str) -> Option<RectPx> {
        let preview = self.preview_rect(id)?;
        let tile = self.tiles.iter().find(|t| t.id == id)?;
        let (cols, rows) = tile.model.dims();
        let (fw, fh) = (
            cols as f32 * self.metrics.advance,
            rows as f32 * self.metrics.line_height,
        );
        if fw <= 0.0 || fh <= 0.0 {
            return Some(preview);
        }
        // Contain-fit, never magnifying — matching the renderer's preview scale.
        let s = (preview.w / fw).min(preview.h / fh).min(1.0);
        Some(RectPx {
            x: preview.x,
            y: preview.y,
            w: fw * s,
            h: fh * s,
        })
    }

    /// The full-zoom camera for a dive into/out of `id`: it maps the tile's drawn
    /// frame onto its NATIVE size at the window origin — exactly how the single view
    /// draws it (top-left, no cover-stretch). Using this (rather than `zoom_to`,
    /// which fills the window and so stretches a frame whose native width is a few
    /// pixels shy of the window) makes the dive's endpoint line up with the single
    /// view pixel-for-pixel. `None` if the tile isn't present.
    pub fn dive_camera(&self, id: &str) -> Option<Transform> {
        let from = self.dive_target_rect(id)?;
        let tile = self.tiles.iter().find(|t| t.id == id)?;
        let (cols, rows) = tile.model.dims();
        let to = RectPx {
            x: 0.0,
            y: 0.0,
            w: cols as f32 * self.metrics.advance,
            h: rows as f32 * self.metrics.line_height,
        };
        Some(Transform::map_rect(from, to))
    }

    /// Whether `id`'s tile is showing a live preview (it has had output fed in).
    /// `false` if the tile is a cold placeholder, or absent.
    pub fn tile_fed(&self, id: &str) -> bool {
        self.tiles.iter().any(|t| t.id == id && t.fed)
    }

    /// Prepare a cold tile (a detached session we're taking over, with no live
    /// preview yet) for a deferred dive-in: size its session to the window so its
    /// preview loads — and the dive lands — at full size, returning the resize
    /// commands for the shell to forward. `None` if the tile is already live (its
    /// preview is ready, so the caller should dive immediately) or absent.
    pub fn prepare_takeover(
        &mut self,
        id: &str,
        size_px: (u32, u32),
        scale: f32,
    ) -> Option<Vec<Cmd>> {
        let tile = self.tiles.iter_mut().find(|t| t.id == id)?;
        if tile.fed {
            return None;
        }
        Some(tile.model.update(UiEvent::Resize {
            w_px: size_px.0.max(1),
            h_px: size_px.1.max(1),
            scale: scale as f64,
        }))
    }

    /// Extract a single terminal for a toggle back to the single view, detaching
    /// every other tile's session. The window's *owned* session is kept (stable
    /// identity), falling back to the focused tile, then any tile — never a
    /// foreign session we merely previewed, so the single view always returns to
    /// what the window actually drives.
    pub fn into_single(
        self,
        size_px: (u32, u32),
        scale: f32,
    ) -> (TerminalModel, Vec<TerminalModel>, Vec<Cmd>) {
        self.into_single_keeping(None, size_px, scale)
    }

    /// Like [`into_single`](Self::into_single) but, when `target` names a present
    /// tile, keeps *that* session (a take-over of a specific tile). Otherwise it
    /// falls back to the owned session, then the focused tile, then any tile.
    /// Returns the kept model, the *other driven sessions'* models (to keep warm
    /// in the single view so their previews and Ctrl-Tab switches stay live), and
    /// the resize commands. Placeholder tiles for sessions this window doesn't
    /// drive are dropped.
    pub fn into_single_keeping(
        self,
        target: Option<SessionId>,
        size_px: (u32, u32),
        scale: f32,
    ) -> (TerminalModel, Vec<TerminalModel>, Vec<Cmd>) {
        let keep = target
            .filter(|id| self.tiles.iter().any(|t| &t.id == id))
            .or_else(|| {
                self.tiles
                    .iter()
                    .find(|t| self.mine.contains(&t.id))
                    .map(|t| t.id.clone())
            })
            .or_else(|| {
                self.tiles
                    .iter()
                    .find(|t| Some(&t.id) == self.focused.as_ref())
                    .map(|t| t.id.clone())
            })
            .or_else(|| self.tiles.first().map(|t| t.id.clone()));
        self.extract(keep.clone(), keep.unwrap_or_default(), size_px, scale)
    }

    /// Leave the overview showing `id` *specifically* — a spawn or take-over.
    /// Keeps `id`'s tile if it has one (preserving its screen); otherwise builds
    /// a fresh terminal for it (the just-spawned session has no tile yet). The
    /// other driven sessions are returned to be kept warm. Unlike
    /// [`into_single_keeping`](Self::into_single_keeping) there is no fallback to
    /// a different session: the caller asked for `id`.
    pub fn into_single_adopting(
        self,
        id: SessionId,
        size_px: (u32, u32),
        scale: f32,
    ) -> (TerminalModel, Vec<TerminalModel>, Vec<Cmd>) {
        self.extract(Some(id.clone()), id, size_px, scale)
    }

    /// Consume the fleet, returning `keep`'s model (or a fresh one named `fresh`),
    /// every other *driven* (this-window) session's model to keep warm, and the
    /// resize commands sizing them all to the window.
    fn extract(
        self,
        keep: Option<SessionId>,
        fresh: SessionId,
        size_px: (u32, u32),
        scale: f32,
    ) -> (TerminalModel, Vec<TerminalModel>, Vec<Cmd>) {
        let metrics = self.metrics;
        let mine = self.mine.clone();
        let mut kept = None;
        let mut warm = Vec::new();
        for tile in self.tiles {
            if Some(&tile.id) == keep.as_ref() {
                kept = Some(tile.model);
            } else if mine.contains(&tile.id) {
                warm.push(tile.model); // a driven session: keep it warm
            }
            // else: a placeholder for a session we don't drive — drop it.
        }
        let resize = UiEvent::Resize {
            w_px: size_px.0.max(1),
            h_px: size_px.1.max(1),
            scale: scale as f64,
        };
        let mut model = kept.unwrap_or_else(|| TerminalModel::new(fresh, 1, 1, metrics));
        let mut cmds = model.update(resize.clone());
        for m in &mut warm {
            cmds.append(&mut m.update(resize.clone()));
        }
        (model, warm, cmds)
    }

    // ---- update ----

    pub fn update(&mut self, ev: UiEvent) -> Vec<Cmd> {
        let cmds = match ev {
            UiEvent::SessionList(infos) => self.reconcile(infos),
            UiEvent::SessionData { name, bytes, ended } => self.session_data(&name, bytes, ended),
            UiEvent::Resize { w_px, h_px, scale } => {
                self.size_px = (w_px, h_px);
                if scale > 0.0 && self.scale != scale as f32 {
                    self.scale = scale as f32;
                    // Effective metrics changed for every tile, so every preview
                    // must be re-laid-out even if its grid size is unchanged.
                    for tile in &mut self.tiles {
                        tile.frame_dirty = true;
                    }
                }
                // Tiles never resize their sessions; previews are scaled to fit
                // at draw time, so a window resize just repaints.
                vec![Cmd::Redraw]
            }
            // Re-enumerate on the scheduled refresh tick.
            UiEvent::Tick { .. } => vec![Cmd::ListSessions],
            // Input goes through the modal router (rename / confirm / normal).
            ev @ (UiEvent::Key { .. } | UiEvent::Text(_) | UiEvent::Pointer { .. }) => {
                self.input(ev)
            }
            _ => Vec::new(),
        };
        // Rebuild any preview whose content or size this event changed, so `view`
        // can stay a pure read of cached frames.
        self.refresh_dirty_frames();
        // A resize, or tiles appearing/disappearing, can change the content height
        // or viewport — keep the scroll offset valid (nav/wheel clamp themselves).
        self.clamp_scroll();
        cmds
    }

    /// Re-lay-out the previews of tiles marked [`Tile::frame_dirty`] and clear the
    /// flag, leaving unchanged tiles' cached frames untouched. Effective metrics
    /// are the same for every tile, so they're computed once.
    fn refresh_dirty_frames(&mut self) {
        let metrics = self.effective_metrics();
        let mut builds = 0;
        for tile in &mut self.tiles {
            if tile.frame_dirty {
                tile.frame = Some(Rc::new(layout_frame(tile.model.screen().vt(), metrics)));
                tile.frame_dirty = false;
                builds += 1;
            }
        }
        self.frame_builds += builds;
    }

    /// Total tile-frame (re)builds — see [`FleetModel::frame_builds`](Self::frame_builds).
    pub fn frame_builds(&self) -> u32 {
        self.frame_builds
    }

    fn reconcile(&mut self, infos: Vec<SessionInfo>) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        let mut dirty = false;
        let new_ids: HashSet<&str> = infos.iter().map(|i| i.name.as_str()).collect();

        // Drop sessions that disappeared.
        let mut gone = Vec::new();
        self.tiles.retain(|t| {
            let keep = new_ids.contains(t.id.as_str());
            if !keep {
                gone.push(t.id.clone());
            }
            keep
        });
        for id in gone {
            cmds.push(Cmd::Detach(id));
            dirty = true;
        }

        // Add placeholder tiles; refresh bell/locality on existing ones. The
        // fleet never attaches sessions itself — only sessions this window
        // already drives (fed by the shell) get a live preview; the rest stay
        // placeholders until the snapshot follow-up.
        for info in &infos {
            let locality = locality_for(&self.mine, &info.name, info.attached);
            if let Some(tile) = self.tiles.iter_mut().find(|t| t.id == info.name) {
                if tile.bell != info.bell
                    || tile.locality != locality
                    || tile.command != info.command
                    || tile.pid != info.pid
                    || tile.created_at != info.created_at
                {
                    // A creation-time change reorders the grid (it's the sort key),
                    // so it warrants a repaint just like locality/metadata changes.
                    dirty = true;
                }
                tile.bell = info.bell;
                tile.locality = locality;
                tile.command = info.command.clone();
                tile.pid = info.pid;
                tile.created_at = info.created_at;
            } else {
                let model =
                    TerminalModel::new(info.name.clone(), PREVIEW_COLS, PREVIEW_ROWS, self.metrics);
                self.push_tile(
                    info.name.clone(),
                    model,
                    info.bell,
                    locality,
                    info.command.clone(),
                    info.pid,
                    info.created_at,
                );
                dirty = true;
            }
        }

        // Keep focus valid; default to the visually-first tile.
        if self
            .focused
            .as_ref()
            .is_none_or(|f| !self.tiles.iter().any(|t| &t.id == f))
        {
            self.focused = self.layout().into_iter().next().map(|(_, id, _)| id);
        }

        if dirty {
            cmds.push(Cmd::Redraw);
        }
        // Re-arm the periodic refresh.
        cmds.push(Cmd::ScheduleTick {
            after_ms: REFRESH_MS,
        });
        cmds
    }

    /// Lay out section headers and tile placements in *content* space (unscrolled,
    /// origin at the top of the grid): a labelled band per non-empty attach-state
    /// section, each followed by its tiles in a fixed-width grid sized to the
    /// terminal's aspect ratio so previews look like little terminals. Returns the
    /// headers, the tile placements (full card rects), the chrome `band` height (so
    /// `card_layout` and the view split each card the same way), and the total
    /// content height. The grid is sized for readability and simply grows past the
    /// viewport, which then scrolls (`view`/`pointer` apply [`Self::scroll_y`]).
    fn sections_layout(&self) -> (Vec<SectionHeader>, Vec<Placement>, f32, f32) {
        let (w, h) = (self.size_px.0 as f32, self.size_px.1 as f32);
        let metrics = self.effective_metrics();
        let base_band = metrics.line_height + 6.0;
        // Tiles grouped by locality, preserving insertion order within each;
        // empty sections are dropped so they get no header.
        let sections: Vec<(Locality, Vec<&Tile>)> = [
            Locality::ThisWindow,
            Locality::Elsewhere,
            Locality::Detached,
        ]
        .into_iter()
        .map(|loc| {
            let mut ts: Vec<&Tile> = self.tiles.iter().filter(|t| t.locality == loc).collect();
            // Stable spatial order within the section: a session keeps its slot
            // for life regardless of enumeration order (see [`tile_order_key`]).
            // `sort_by` (not `sort_by_key`) since the key borrows the tile's name.
            ts.sort_by(|a, b| tile_order_key(a).cmp(&tile_order_key(b)));
            (loc, ts)
        })
        .filter(|(_, ts): &(_, Vec<&Tile>)| !ts.is_empty())
        .collect();
        if sections.is_empty() {
            return (Vec::new(), Vec::new(), base_band, 0.0);
        }

        let (band, gap) = (base_band, GAP);
        // Preview pixel aspect (width : height) of the terminal grid.
        let aspect =
            (PREVIEW_COLS as f32 * metrics.advance) / (PREVIEW_ROWS as f32 * metrics.line_height);

        // A card is an aspect-locked little terminal (the preview) plus two chrome
        // bands. Its SIZE adapts to the session count: a crowded grid uses the
        // compact thumbnail size and scrolls, while a few sessions GROW (up to
        // native 1:1 — past that the preview can't get any sharper) to use the
        // space. Width follows the terminal aspect ratio rather than stretching to
        // fill the column (which would distort the preview); a narrow window shrinks
        // it to fit.
        let avail_w = (w - 2.0 * gap).max(1.0);
        let min_card_h = 2.0 * band + MIN_PREVIEW_LINES * metrics.line_height;
        let native_card_h = 2.0 * band + PREVIEW_ROWS as f32 * metrics.line_height;
        let compact_card_h =
            2.0 * band + PREVIEW_ROWS as f32 * metrics.line_height * PREVIEW_COMPACT_SCALE;
        // Grow no taller than native, and on a short window no taller than a bit
        // under half the viewport (so a header + other cards stay visible); never
        // below the readable floor.
        let cap = native_card_h.min((h * MAX_CARD_VIEWPORT_FRAC).max(min_card_h));
        let floor = compact_card_h.clamp(min_card_h, cap);

        // Total content height for a candidate card height, recomputing the column
        // count (cards are aspect-locked, so a taller card is wider and fewer fit).
        let seg: Vec<usize> = sections.iter().map(|(_, ts)| ts.len()).collect();
        let content_for = |ch: f32| -> f32 {
            let pw = ((ch - 2.0 * band) * aspect).min(avail_w);
            let per_row = (((w - gap) / (pw + gap)).floor() as usize).clamp(1, MAX_PER_ROW);
            let mut yy = gap;
            for &n in &seg {
                yy += band + n.div_ceil(per_row) as f32 * (ch + gap);
            }
            yy
        };
        // Largest card height in [floor, cap] whose whole grid still fits the
        // viewport (few sessions enlarge to use the space); if even the compact grid
        // overflows, use the compact size and let it scroll (crowded stays dense).
        let card_h = if content_for(floor) >= h {
            floor
        } else {
            let (mut lo, mut hi) = (floor, cap);
            for _ in 0..24 {
                let mid = 0.5 * (lo + hi);
                if content_for(mid) <= h {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            lo
        };

        let mut preview_h = card_h - 2.0 * band;
        let mut card_w = preview_h * aspect;
        if card_w > avail_w {
            card_w = avail_w; // width-bound (narrow window): keep aspect, shrink
            preview_h = card_w / aspect;
        }
        let card_h = (preview_h + 2.0 * band).max(min_card_h);

        // Fit as many aspect-locked cards across as the width allows, then centre
        // the row so the leftover width is shared as margins (the scroll, when the
        // grid overflows, is vertical only).
        let per_row = (((w - gap) / (card_w + gap)).floor() as usize).clamp(1, MAX_PER_ROW);
        let row_w = per_row as f32 * card_w + (per_row as f32 - 1.0) * gap;
        let left = ((w - row_w) / 2.0).max(gap);

        let mut headers = Vec::new();
        let mut placements = Vec::new();
        let mut y = gap;
        for (loc, ts) in &sections {
            let nrows = ts.len().div_ceil(per_row);
            headers.push((
                *loc,
                RectPx {
                    x: left,
                    y,
                    w: row_w.max(1.0),
                    h: band,
                },
            ));
            y += band;
            for (i, t) in ts.iter().enumerate() {
                let (r, c) = (i / per_row, i % per_row);
                placements.push((
                    t.handle,
                    t.id.clone(),
                    RectPx {
                        x: left + c as f32 * (card_w + gap),
                        y: y + r as f32 * (card_h + gap),
                        w: card_w,
                        h: card_h,
                    },
                ));
            }
            y += nrows as f32 * (card_h + gap);
        }
        (headers, placements, band, y)
    }

    /// Tile placements `(handle, id, rect)` in section order, content space.
    fn layout(&self) -> Vec<Placement> {
        self.sections_layout().1
    }

    /// Greatest valid scroll offset: how far the content extends past the viewport
    /// (0 when everything fits).
    fn max_scroll(&self) -> f32 {
        let content_h = self.sections_layout().3;
        (content_h - self.size_px.1 as f32).max(0.0)
    }

    /// Keep [`Self::scroll_y`] within `[0, max_scroll]` after anything that changes
    /// the content height or viewport (resize, tile add/remove, wheel, navigation).
    fn clamp_scroll(&mut self) {
        self.scroll_y = self.scroll_y.clamp(0.0, self.max_scroll());
    }

    /// Scroll a mouse-wheel notch. Sign only (magnitude ignored, like the
    /// terminal's scrollback); wheel up reveals tiles above. Returns a redraw iff
    /// the offset actually moved.
    fn wheel(&mut self, dy: f64) -> Vec<Cmd> {
        if dy == 0.0 {
            return Vec::new();
        }
        let step = SCROLL_LINES * self.effective_metrics().line_height;
        let before = self.scroll_y;
        self.scroll_y += if dy > 0.0 { -step } else { step };
        self.clamp_scroll();
        if self.scroll_y == before {
            Vec::new()
        } else {
            vec![Cmd::Redraw]
        }
    }

    /// After moving focus with the arrows, scroll just enough to bring the focused
    /// tile fully into view (with a small margin).
    fn scroll_to_focused(&mut self) {
        let (_, placements, _, _) = self.sections_layout();
        let Some((_, _, rect)) = placements
            .iter()
            .find(|(_, id, _)| Some(id.as_str()) == self.focused.as_deref())
        else {
            return;
        };
        let view_h = self.size_px.1 as f32;
        if rect.y - GAP < self.scroll_y {
            self.scroll_y = (rect.y - GAP).max(0.0);
        } else if rect.y + rect.h + GAP > self.scroll_y + view_h {
            self.scroll_y = rect.y + rect.h + GAP - view_h;
        }
        self.clamp_scroll();
    }

    fn session_data(&mut self, name: &str, bytes: Vec<u8>, ended: bool) -> Vec<Cmd> {
        let background = self.focused.as_deref() != Some(name);
        let Some(tile) = self.tiles.iter_mut().find(|t| t.id == name) else {
            return Vec::new();
        };
        let had_output = !bytes.is_empty();
        let cmds = tile.model.update(UiEvent::SessionData {
            name: name.to_string(),
            bytes,
            ended,
        });
        if had_output {
            tile.fed = true; // attached and live: stop re-attaching it
            tile.frame_dirty = true; // its screen changed; preview is stale
            if background {
                tile.activity = tile.activity.saturating_add(1);
            }
        }
        // The overview doesn't drive the window title; a tile changing its OSC
        // title must not retitle the window out from under the single view.
        cmds.into_iter()
            .filter(|c| !matches!(c, Cmd::SetTitle(_)))
            .collect()
    }

    /// Tab/Shift+Tab: step linearly through tiles in layout order, wrapping.
    fn move_focus_linear(&mut self, delta: i32, wrap: bool) {
        let order: Vec<SessionId> = self.layout().into_iter().map(|(_, id, _)| id).collect();
        if order.is_empty() {
            return;
        }
        let cur = self
            .focused
            .as_ref()
            .and_then(|f| order.iter().position(|id| id == f))
            .unwrap_or(0) as i32;
        let n = order.len() as i32;
        let next = if wrap {
            (cur + delta).rem_euclid(n)
        } else {
            (cur + delta).clamp(0, n - 1)
        };
        self.set_focus(order[next as usize].clone());
    }

    /// Arrow keys: move focus to the nearest tile in `dir`, using laid-out tile
    /// centres so it tracks the visual grid and crosses section boundaries
    /// naturally. Stays put when there is no tile that way.
    fn move_focus_dir(&mut self, dir: Dir) {
        let placements = self.layout();
        let Some((_, _, cur)) = placements
            .iter()
            .find(|(_, id, _)| Some(id.as_str()) == self.focused.as_deref())
            .cloned()
        else {
            // No valid focus yet: fall back to the first tile.
            if let Some((_, id, _)) = placements.first() {
                self.set_focus(id.clone());
            }
            return;
        };
        let (cx, cy) = (cur.x + cur.w / 2.0, cur.y + cur.h / 2.0);
        let mut best: Option<(f32, SessionId)> = None;
        for (_, id, r) in &placements {
            if Some(id.as_str()) == self.focused.as_deref() {
                continue;
            }
            let (dx, dy) = (r.x + r.w / 2.0 - cx, r.y + r.h / 2.0 - cy);
            // Keep only tiles in the half-plane of `dir`; score = distance along
            // the axis plus a perpendicular penalty so aligned tiles win.
            let score = match dir {
                Dir::Down if dy > 0.5 => dy + dx.abs(),
                Dir::Up if dy < -0.5 => -dy + dx.abs(),
                Dir::Right if dx > 0.5 => dx + dy.abs(),
                Dir::Left if dx < -0.5 => -dx + dy.abs(),
                _ => continue,
            };
            if best.as_ref().is_none_or(|(b, _)| score < *b) {
                best = Some((score, id.clone()));
            }
        }
        if let Some((_, id)) = best {
            self.set_focus(id);
        }
    }

    fn set_focus(&mut self, id: SessionId) {
        if let Some(t) = self.tiles.iter_mut().find(|t| t.id == id) {
            t.activity = 0; // focusing clears the activity badge
        }
        self.focused = Some(id);
    }

    /// Route an input event. An active inline rename or confirm dialog swallows
    /// input until resolved; otherwise arrows/Tab navigate, Enter activates the
    /// focused tile, and a press hits a button or activates a tile.
    fn input(&mut self, ev: UiEvent) -> Vec<Cmd> {
        if self.renaming.is_some() {
            return self.rename_input(ev);
        }
        if self.pending.is_some() {
            return self.pending_input(ev);
        }
        match ev {
            UiEvent::Key {
                key, mods, kind, ..
            } if kind.is_down() && nav(&key, mods).is_some() => {
                match nav(&key, mods).unwrap() {
                    Nav::Dir(d) => self.move_focus_dir(d),
                    Nav::Step(delta, wrap) => self.move_focus_linear(delta, wrap),
                }
                // Keep the newly-focused tile on screen when the grid scrolls.
                self.scroll_to_focused();
                vec![Cmd::Redraw]
            }
            UiEvent::Key { key, kind, .. }
                if kind.is_down() && matches!(key, Key::Named(NamedKey::Enter)) =>
            {
                self.activate(self.focused.clone())
            }
            UiEvent::Pointer {
                phase: PointerPhase::Wheel,
                wheel_dy,
                ..
            } => self.wheel(wheel_dy),
            UiEvent::Pointer { phase, pos, .. } => self.pointer(phase, pos),
            // Otherwise view-only: text and ordinary keys are dropped.
            _ => Vec::new(),
        }
    }

    /// Open `id` into this window's single view. A session held by another window
    /// is confirmed first, since taking it over steals its display client.
    fn activate(&mut self, id: Option<SessionId>) -> Vec<Cmd> {
        let Some(id) = id else {
            return Vec::new();
        };
        match self.locality_of(&id) {
            Some(Locality::Elsewhere) => {
                self.pending = Some(Pending {
                    id,
                    action: PendingAction::TakeOver,
                });
                vec![Cmd::Redraw]
            }
            Some(_) => vec![Cmd::TakeOver(id), Cmd::Redraw],
            None => Vec::new(),
        }
    }

    /// Run a card button's action: detach immediately, confirm a kill, or open an
    /// inline rename.
    fn button(&mut self, button: Button, id: SessionId) -> Vec<Cmd> {
        match button {
            Button::Detach => vec![Cmd::Detach(id), Cmd::Redraw],
            Button::Kill => {
                self.pending = Some(Pending {
                    id,
                    action: PendingAction::Kill,
                });
                vec![Cmd::Redraw]
            }
            Button::Rename => {
                self.renaming = Some(Renaming {
                    buffer: id.clone(),
                    id,
                });
                vec![Cmd::Redraw]
            }
        }
    }

    /// Keyboard for the confirm dialog: Enter runs the pending action, Escape
    /// cancels it.
    fn pending_input(&mut self, ev: UiEvent) -> Vec<Cmd> {
        let UiEvent::Key { key, kind, .. } = ev else {
            return Vec::new();
        };
        if !kind.is_down() {
            return Vec::new();
        }
        match key {
            Key::Named(NamedKey::Enter) => {
                let p = self.pending.take().expect("pending checked by caller");
                let cmd = match p.action {
                    PendingAction::TakeOver => Cmd::TakeOver(p.id),
                    PendingAction::Kill => Cmd::Kill(p.id),
                };
                vec![cmd, Cmd::Redraw]
            }
            Key::Named(NamedKey::Escape) => {
                self.pending = None;
                vec![Cmd::Redraw]
            }
            _ => Vec::new(),
        }
    }

    /// Keyboard for an inline rename: text appends, Backspace deletes, Enter
    /// commits (a no-op for an empty/unchanged name), Escape cancels.
    fn rename_input(&mut self, ev: UiEvent) -> Vec<Cmd> {
        match ev {
            UiEvent::Text(s) => {
                if let Some(r) = &mut self.renaming {
                    r.buffer.push_str(&s);
                }
                vec![Cmd::Redraw]
            }
            UiEvent::Key { key, kind, .. } if kind.is_down() => match key {
                Key::Named(NamedKey::Backspace) => {
                    if let Some(r) = &mut self.renaming {
                        r.buffer.pop();
                    }
                    vec![Cmd::Redraw]
                }
                Key::Named(NamedKey::Enter) => {
                    let r = self.renaming.take().expect("renaming checked by caller");
                    if r.buffer.is_empty() || r.buffer == r.id {
                        vec![Cmd::Redraw]
                    } else {
                        vec![
                            Cmd::Rename {
                                session: r.id,
                                name: r.buffer,
                            },
                            Cmd::Redraw,
                        ]
                    }
                }
                Key::Named(NamedKey::Escape) => {
                    self.renaming = None;
                    vec![Cmd::Redraw]
                }
                _ => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    fn pointer(&mut self, phase: PointerPhase, pos: PointPx) -> Vec<Cmd> {
        if phase != PointerPhase::Press {
            return Vec::new(); // the overview only reacts to presses
        }
        // Hit-test in content space: the viewport point plus the scroll offset.
        let (px, py) = (pos.x as f32, pos.y as f32 + self.scroll_y);
        let (_, placements, band, _) = self.sections_layout();
        let hit = placements.into_iter().find(|(_, _, r)| r.contains(px, py));
        let Some((_, id, rect)) = hit else {
            return Vec::new();
        };
        self.set_focus(id.clone());
        // A press on a card button runs that action; anywhere else opens the tile.
        let (_, _, buttons) = card_layout(rect, band);
        match buttons.iter().find(|(_, r)| r.contains(px, py)) {
            Some((button, _)) => self.button(*button, id),
            None => self.activate(Some(id)),
        }
    }

    // ---- view ----

    pub fn view(&self) -> Scene {
        let (headers, placements, band, _content_h) = self.sections_layout();
        let metrics = self.effective_metrics();
        let view_h = self.size_px.1 as f32;
        let sy = self.scroll_y;
        let mut items = Vec::new();
        for (loc, mut rect) in headers {
            rect.y -= sy;
            items.push(SceneItem::Text {
                id: SceneId::Section(loc.rank()),
                rect: text_line(rect, metrics, GAP * 0.5),
                runs: vec![label_run(section_label(loc))],
                metrics,
                color: SECTION_LABEL_COLOR,
            });
        }
        for (handle, id, mut rect) in placements {
            rect.y -= sy;
            // Cull tiles fully outside the viewport: otherwise their previews are
            // re-rendered to textures (costly with many sessions) only to be
            // scissored away. Headers above stay, so the section structure shows.
            if rect.y + rect.h <= 0.0 || rect.y >= view_h {
                continue;
            }
            let Some(tile) = self.tiles.iter().find(|t| t.id == id) else {
                continue;
            };
            let focused = self.focused.as_deref() == Some(id.as_str());
            let (header, preview, buttons) = card_layout(rect, band);

            // The whole card on a solid panel, so it reads as one unit.
            items.push(SceneItem::Rect {
                id: SceneId::Tile(handle),
                rect,
                color: CARD_BG,
                radius: 5.0,
            });

            // Metadata header — or the live buffer of an in-progress rename.
            let header_text = match self.renaming.as_ref().filter(|r| r.id == id) {
                Some(r) => format!("{}\u{2588}", r.buffer), // trailing caret block
                None => card_meta(&tile.id, &tile.command, tile.pid),
            };
            items.push(SceneItem::Text {
                id: SceneId::Label(handle),
                rect: text_line(header, metrics, 6.0),
                runs: vec![label_run(&header_text)],
                metrics,
                color: CARD_META_COLOR,
            });

            // Preview area: a live, scaled terminal, or a placeholder + hint.
            if tile.fed {
                // Laid out at the session's real size; the renderer scales it to
                // the preview rect. Cloning the cached `Rc` (not re-wrapping a fresh
                // one) preserves pointer identity across presents, so an unchanged
                // tile is an `Rc::ptr_eq` cache hit in the renderer. The fallback only
                // fires before the first refresh.
                let frame = tile
                    .frame
                    .clone()
                    .unwrap_or_else(|| Rc::new(layout_frame(tile.model.screen().vt(), metrics)));
                items.push(SceneItem::Terminal {
                    id: SceneId::Tile(handle),
                    session: ghost_render::session_key(&tile.id),
                    rect: preview,
                    frame,
                    selection: if focused {
                        tile.model.selection()
                    } else {
                        None
                    },
                    dim: !focused,
                    // A preview is downscaled (contain_scale < 1), so when its cached
                    // frame DOES change the renderer re-rasters the whole Surface (no
                    // row band applies); an UNCHANGED tile short-circuits on `Rc`
                    // identity before this is consulted.
                    damage: ghost_render::TermDamage::All,
                });
            } else {
                items.push(SceneItem::Rect {
                    id: SceneId::Label(handle),
                    rect: preview,
                    color: PLACEHOLDER_BG,
                    radius: 3.0,
                });
                let hint = placeholder_hint(tile.locality);
                items.push(SceneItem::Text {
                    id: SceneId::Badge(handle),
                    rect: centered_line(preview, metrics, hint),
                    runs: vec![label_run(hint)],
                    metrics,
                    color: PLACEHOLDER_FG,
                });
            }

            // Action buttons — a centred label on its own inset chip.
            for (button, brect) in buttons {
                let chip = inset(brect, 3.0);
                items.push(SceneItem::Rect {
                    id: SceneId::Tile(handle),
                    rect: chip,
                    color: BUTTON_BG,
                    radius: 3.0,
                });
                items.push(SceneItem::Text {
                    id: SceneId::Label(handle),
                    rect: centered_line(chip, metrics, button.label()),
                    runs: vec![label_run(button.label())],
                    metrics,
                    color: BUTTON_FG,
                });
            }

            if focused {
                items.push(SceneItem::Border {
                    id: SceneId::Tile(handle),
                    rect,
                    color: FOCUS_COLOR,
                    width: FOCUS_BORDER,
                });
            }
            if let Some(kind) = badge_kind(tile, focused) {
                // Clamp the badge into the tile so a tiny preview can't float it
                // outside (negative x / oversized).
                let bw = BADGE_PX.min(rect.w);
                let bh = BADGE_PX.min(rect.h);
                items.push(SceneItem::Badge {
                    id: SceneId::Badge(handle),
                    rect: RectPx {
                        x: (rect.x + rect.w - bw - 2.0).max(rect.x),
                        y: rect.y + 2.0,
                        w: bw,
                        h: bh,
                    },
                    kind,
                });
            }
        }

        // A pending action scrims the whole grid with a confirm prompt.
        if let Some(p) = &self.pending {
            let (w, h) = (self.size_px.0 as f32, self.size_px.1 as f32);
            items.push(SceneItem::Rect {
                id: SceneId::Sidebar,
                rect: RectPx {
                    x: 0.0,
                    y: 0.0,
                    w,
                    h,
                },
                color: OVERLAY_BG,
                radius: 0.0,
            });
            items.push(SceneItem::Text {
                id: SceneId::NavBar,
                rect: RectPx {
                    x: 16.0,
                    y: (h - metrics.line_height) / 2.0,
                    w: (w - 32.0).max(1.0),
                    h: metrics.line_height,
                },
                runs: vec![label_run(&confirm_prompt(p))],
                metrics,
                color: OVERLAY_FG,
            });
        }

        let mut scene = Scene::new(self.size_px);
        scene.layers.push(Layer::new(0, items));
        scene
    }
}

/// Inset a rect by `pad` on every side (clamped to a positive size).
fn inset(rect: RectPx, pad: f32) -> RectPx {
    RectPx {
        x: rect.x + pad,
        y: rect.y + pad,
        w: (rect.w - 2.0 * pad).max(1.0),
        h: (rect.h - 2.0 * pad).max(1.0),
    }
}

/// A single left-aligned text line, vertically centred in `band` with `pad_x`
/// horizontal padding. The renderer draws the baseline at 0.8·line_height below
/// the rect top, so we offset the rect to centre the line within the band.
fn text_line(band: RectPx, m: CellMetrics, pad_x: f32) -> RectPx {
    RectPx {
        x: band.x + pad_x,
        y: band.y + ((band.h - m.line_height) * 0.5).max(0.0),
        w: (band.w - 2.0 * pad_x).max(1.0),
        h: m.line_height,
    }
}

/// A single text line centred both horizontally and vertically within `area`.
fn centered_line(area: RectPx, m: CellMetrics, text: &str) -> RectPx {
    let tw = text.chars().count() as f32 * m.advance;
    RectPx {
        x: area.x + ((area.w - tw) * 0.5).max(0.0),
        y: area.y + ((area.h - m.line_height) * 0.5).max(0.0),
        w: tw.max(1.0),
        h: m.line_height,
    }
}

/// The muted hint shown in a tile that has no live preview yet.
fn placeholder_hint(loc: Locality) -> &'static str {
    match loc {
        Locality::ThisWindow => "starting\u{2026}",
        Locality::Elsewhere => "attached elsewhere",
        Locality::Detached => "detached",
    }
}

/// One-line card metadata: `name · command · pid`. The command is omitted when
/// the session just runs the user's `$SHELL` (an empty command) — it's always the
/// shell there, so it's noise; the pid is omitted when unknown.
fn card_meta(id: &str, command: &[String], pid: i32) -> String {
    let mut s = id.to_string();
    if !command.is_empty() {
        s.push_str(" \u{b7} ");
        s.push_str(&command.join(" "));
    }
    if pid > 0 {
        s.push_str(" \u{b7} ");
        s.push_str(&pid.to_string());
    }
    s
}

/// The prompt shown in the confirm overlay.
fn confirm_prompt(p: &Pending) -> String {
    match p.action {
        PendingAction::Kill => {
            format!("Kill {}?  Enter = confirm, Esc = cancel", p.id)
        }
        PendingAction::TakeOver => {
            format!(
                "{} is open in another window — take it over?  Enter / Esc",
                p.id
            )
        }
    }
}

/// The header label for an attach-state section.
fn section_label(loc: Locality) -> &'static str {
    match loc {
        Locality::ThisWindow => "Attached",
        Locality::Elsewhere => "Attached elsewhere",
        Locality::Detached => "Detached",
    }
}

/// A single left-aligned chrome label run. The renderer draws chrome text in the
/// item's colour and ignores per-run style, so a default `Style` is fine.
fn label_run(text: &str) -> Run {
    Run {
        start_col: 0,
        width_cols: text.chars().count(),
        text: text.to_string(),
        style: Style::default(),
    }
}

fn badge_kind(tile: &Tile, focused: bool) -> Option<BadgeKind> {
    if tile.bell {
        Some(BadgeKind::Bell)
    } else if !focused && tile.activity > 0 {
        Some(BadgeKind::Activity)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::KeyEventKind;

    const METRICS: CellMetrics = CellMetrics {
        advance: 9.0,
        line_height: 18.0,
    };
    const SIZE: (u32, u32) = (400, 200);

    /// A detached session — safe for the fleet to attach and preview.
    fn info(name: &str) -> SessionInfo {
        SessionInfo {
            name: name.to_string(),
            pid: 1,
            created_at: None,
            title: name.to_string(),
            command: vec![],
            attached: false,
            bell: false,
        }
    }

    fn fleet() -> FleetModel {
        FleetModel::new(METRICS, SIZE, HashSet::new())
    }

    /// A window wide enough for a multi-column grid, for tests that exercise
    /// horizontal arrow nav or 2-D layout (the narrow default fits one column).
    const WIDE: (u32, u32) = (1000, 700);

    fn widen(m: &mut FleetModel) {
        m.update(UiEvent::Resize {
            w_px: WIDE.0,
            h_px: WIDE.1,
            scale: 1.0,
        });
    }

    fn list(m: &mut FleetModel, names: &[&str]) -> Vec<Cmd> {
        m.update(UiEvent::SessionList(
            names.iter().map(|n| info(n)).collect(),
        ))
    }

    fn data(m: &mut FleetModel, name: &str, bytes: &[u8]) -> Vec<Cmd> {
        m.update(UiEvent::SessionData {
            name: name.to_string(),
            bytes: bytes.to_vec(),
            ended: false,
        })
    }

    fn key(m: &mut FleetModel, k: Key) -> Vec<Cmd> {
        m.update(UiEvent::Key {
            key: k,
            mods: crate::Mods::NONE,
            kind: KeyEventKind::Press,
            alts: None,
        })
    }

    fn wheel(m: &mut FleetModel, dy: f64) -> Vec<Cmd> {
        m.update(UiEvent::Pointer {
            phase: PointerPhase::Wheel,
            button: None,
            pos: PointPx { x: 0.0, y: 0.0 },
            mods: crate::Mods::NONE,
            wheel_dy: dy,
            clicks: 1,
        })
    }

    /// List `n` detached sessions named `s0..sn`.
    fn list_many(m: &mut FleetModel, n: usize) {
        let infos: Vec<SessionInfo> = (0..n).map(|i| info(&format!("s{i}"))).collect();
        m.update(UiEvent::SessionList(infos));
    }

    fn sinfo(name: &str, attached: bool) -> SessionInfo {
        SessionInfo {
            attached,
            ..info(name)
        }
    }

    /// `(label, top-y)` for each section header in the rendered scene (the
    /// Section-id Text items, not placeholder name labels).
    fn headers(m: &FleetModel) -> Vec<(String, f32)> {
        m.view().layers[0]
            .items
            .iter()
            .filter_map(|it| match it {
                SceneItem::Text {
                    id: SceneId::Section(_),
                    runs,
                    rect,
                    ..
                } => Some((runs.iter().map(|r| r.text.as_str()).collect(), rect.y)),
                _ => None,
            })
            .collect()
    }

    /// The laid-out top-y of a tile (tests reach into the private layout).
    fn tile_y(m: &FleetModel, id: &str) -> f32 {
        m.layout()
            .into_iter()
            .find(|(_, i, _)| i == id)
            .unwrap()
            .2
            .y
    }

    /// A detached session with a recorded creation time (Unix seconds).
    fn info_at(name: &str, created_at: i64) -> SessionInfo {
        SessionInfo {
            created_at: Some(created_at),
            ..info(name)
        }
    }

    /// Session ids in laid-out order (section order, then within-section order).
    fn order(m: &FleetModel) -> Vec<String> {
        m.layout().into_iter().map(|(_, id, _)| id).collect()
    }

    #[test]
    fn within_a_section_tiles_order_by_creation_time_not_enumeration() {
        let mut m = fleet();
        // Enumerated in a scrambled order; creation time is the intended spatial
        // order, so the grid must not follow how the host happened to list them.
        m.update(UiEvent::SessionList(vec![
            info_at("c", 30),
            info_at("a", 10),
            info_at("b", 20),
        ]));
        assert_eq!(
            order(&m),
            vec!["a", "b", "c"],
            "oldest session first, regardless of enumeration order"
        );
        // A later reconcile in yet another order must not reshuffle the grid.
        m.update(UiEvent::SessionList(vec![
            info_at("b", 20),
            info_at("c", 30),
            info_at("a", 10),
        ]));
        assert_eq!(
            order(&m),
            vec!["a", "b", "c"],
            "ordering is stable across reconciles"
        );
        // A brand-new (newest) session lands at the end; existing tiles keep slots.
        m.update(UiEvent::SessionList(vec![
            info_at("d", 40),
            info_at("b", 20),
            info_at("a", 10),
            info_at("c", 30),
        ]));
        assert_eq!(order(&m), vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn tied_creation_times_break_the_tie_by_name_not_enumeration() {
        // Sessions spawned in the same millisecond tie on `created_at`. The
        // tiebreak must be deterministic — the session name — so the grid order is
        // identical however the host happened to enumerate them. Every F9 /
        // dive-back builds a *fresh* fleet, so a tiebreak that depends on
        // enumeration (or a per-fleet handle assigned in enumeration order) lets
        // the tied tiles swap slots between rebuilds.
        //
        // Mirrors the real report: four sessions where the middle two share a
        // creation instant. The first and last (distinct times) stay put; the tied
        // pair must not swap.
        let build = |enumerated: &[(&str, i64)]| {
            let mut m = fleet();
            m.update(UiEvent::SessionList(
                enumerated.iter().map(|(n, t)| info_at(n, *t)).collect(),
            ));
            order(&m)
        };
        let want = vec!["s1", "s2", "s3", "s4"];
        // s2 and s3 share creation second 200; enumerated in creation order...
        assert_eq!(
            build(&[("s1", 100), ("s2", 200), ("s3", 200), ("s4", 300)]),
            want
        );
        // ...and scrambled, as a different directory-read order would deliver them:
        // the name tiebreak keeps s2 before s3 either way.
        assert_eq!(
            build(&[("s4", 300), ("s3", 200), ("s2", 200), ("s1", 100)]),
            want,
            "tied tiles must order by name, not by how the host listed them"
        );
    }

    #[test]
    fn tiles_are_split_into_attach_state_sections() {
        let mut m = FleetModel::new(METRICS, SIZE, HashSet::from(["a".to_string()]));
        m.update(UiEvent::SessionList(vec![
            sinfo("a", false), // ours -> Attached
            sinfo("b", true),  // attached elsewhere
            sinfo("c", false), // detached
        ]));
        assert_eq!(m.locality_of("a"), Some(Locality::ThisWindow));
        assert_eq!(m.locality_of("b"), Some(Locality::Elsewhere));
        assert_eq!(m.locality_of("c"), Some(Locality::Detached));
        // Three headers, in attach-state order, stacked top to bottom.
        let hs = headers(&m);
        let labels: Vec<&str> = hs.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(labels, vec!["Attached", "Attached elsewhere", "Detached"]);
        assert!(
            hs[0].1 < hs[1].1 && hs[1].1 < hs[2].1,
            "sections stack downward: {hs:?}"
        );
        // Each tile sits in its section's vertical band.
        assert!(tile_y(&m, "a") < tile_y(&m, "b"));
        assert!(tile_y(&m, "b") < tile_y(&m, "c"));
    }

    #[test]
    fn only_nonempty_sections_get_a_header() {
        let mut m = fleet(); // empty mine
        list(&mut m, &["x", "y"]); // both detached
        let labels: Vec<String> = headers(&m).into_iter().map(|(l, _)| l).collect();
        assert_eq!(labels, vec!["Detached".to_string()]);
    }

    #[test]
    fn arrow_down_moves_to_the_tile_below_not_the_next() {
        let mut m = fleet();
        list(&mut m, &["a", "b", "c", "d"]); // one section, 2x2 grid: a b / c d
        widen(&mut m);
        assert_eq!(m.focused(), Some("a")); // top-left
        key(&mut m, Key::Named(NamedKey::ArrowDown));
        assert_eq!(
            m.focused(),
            Some("c"),
            "Down moves to the tile below, not the next in order"
        );
        // Right is the horizontal neighbour.
        let mut m = fleet();
        list(&mut m, &["a", "b", "c", "d"]);
        widen(&mut m);
        key(&mut m, Key::Named(NamedKey::ArrowRight));
        assert_eq!(m.focused(), Some("b"));
    }

    #[test]
    fn arrow_down_crosses_into_the_next_section() {
        let mut m = FleetModel::new(
            METRICS,
            SIZE,
            HashSet::from(["a1".to_string(), "a2".to_string()]),
        );
        m.update(UiEvent::SessionList(vec![
            sinfo("a1", false),
            sinfo("a2", false),
            sinfo("d1", false),
            sinfo("d2", false),
        ]));
        widen(&mut m);
        assert_eq!(m.focused(), Some("a1"));
        assert_eq!(m.locality_of("a1"), Some(Locality::ThisWindow));
        key(&mut m, Key::Named(NamedKey::ArrowDown));
        assert_eq!(
            m.locality_of(m.focused().unwrap()),
            Some(Locality::Detached),
            "Down from the attached row enters the detached section"
        );
    }

    #[test]
    fn only_the_tile_that_changed_rebuilds_its_preview_frame() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]);
        // Both tiles laid out once by the reconcile; baseline the counter there.
        let base = m.frame_builds();

        // Output to one tile rebuilds exactly that tile's frame.
        data(&mut m, "a", b"hello");
        assert_eq!(m.frame_builds(), base + 1, "only tile a is re-laid-out");

        // Rendering reads cached frames — it never re-lays-out.
        let _ = m.view();
        let _ = m.view();
        assert_eq!(m.frame_builds(), base + 1, "view reuses cached frames");

        // A nav keypress changes focus, not content: no frame is rebuilt.
        key(&mut m, Key::Named(NamedKey::ArrowRight));
        assert_eq!(m.frame_builds(), base + 1, "focus change rebuilds nothing");

        // Output to the other tile rebuilds only it.
        data(&mut m, "b", b"world");
        assert_eq!(m.frame_builds(), base + 2);
    }

    #[test]
    fn a_scale_change_re_lays_out_every_tile() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]);
        let base = m.frame_builds();
        // A DPI change alters effective metrics for all tiles even if grids match.
        m.update(UiEvent::Resize {
            w_px: SIZE.0,
            h_px: SIZE.1,
            scale: 2.0,
        });
        assert_eq!(
            m.frame_builds(),
            base + 2,
            "both previews re-laid-out on rescale"
        );
    }

    fn rects_overlap(a: &RectPx, b: &RectPx) -> bool {
        a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
    }

    #[test]
    fn reconcile_creates_tiles_and_detaches_gone() {
        let mut m = fleet();
        let cmds = list(&mut m, &["a", "b"]);
        // The fleet never attaches sessions itself.
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::Attach(_))));
        assert_eq!(m.tile_count(), 2);
        assert_eq!(m.focused(), Some("a")); // first tile focused by default

        let cmds = list(&mut m, &["a"]);
        assert!(cmds.contains(&Cmd::Detach("b".into())));
        assert_eq!(m.tile_count(), 1);
    }

    #[test]
    fn the_fleet_never_resizes_a_previewed_session() {
        let mut m = fleet();
        let cmds = list(&mut m, &["a", "b"]);
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Resize { .. })),
            "reconcile must not resize sessions: {cmds:?}"
        );
        // A window resize repaints but never resizes the previewed sessions.
        let cmds = m.update(UiEvent::Resize {
            w_px: 800,
            h_px: 600,
            scale: 1.0,
        });
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::Resize { .. })));
        assert!(cmds.contains(&Cmd::Redraw));
    }

    #[test]
    fn the_dive_target_matches_the_session_aspect_not_the_card_box() {
        // A window-sized session has the window's aspect (1400:900 ≈ 1.56), which is
        // not the card's fixed 80×24 preview-box aspect (≈ 1.67).
        let win = (1400u32, 900u32);
        let mut primary = TerminalModel::new("alpha".to_string(), 80, 24, METRICS);
        primary.update(UiEvent::Resize {
            w_px: win.0,
            h_px: win.1,
            scale: 1.0,
        });
        let (cols, rows) = primary.dims();
        let session_aspect = (cols as f32 * METRICS.advance) / (rows as f32 * METRICS.line_height);
        let mine = HashSet::from(["alpha".to_string()]);
        let (f, _) = FleetModel::adopting(primary, Vec::new(), METRICS, win, 1.0, mine);

        let aspect = |r: RectPx| r.w / r.h;
        let target = f.dive_target_rect("alpha").expect("the tile is present");
        let preview = f.preview_rect("alpha").expect("the tile is present");

        // The dive aims at where the content is actually drawn, so a cover-zoom lands
        // the session at native size (matching the live single view) — no boundary pop.
        assert!(
            (aspect(target) - session_aspect).abs() < 0.02,
            "dive target aspect {} should match the session aspect {session_aspect}",
            aspect(target)
        );
        // Which is meaningfully different from the fixed-aspect preview box (the bug).
        assert!(
            (aspect(preview) - aspect(target)).abs() > 0.05,
            "and should differ from the preview box aspect ({} vs {})",
            aspect(preview),
            aspect(target)
        );
        // It is the frame's drawn sub-rect: same top-left, contained within the box.
        assert!(
            target.x == preview.x
                && target.y == preview.y
                && target.w <= preview.w + 0.5
                && target.h <= preview.h + 0.5,
            "the target is the contain-fit sub-rect of the preview box"
        );
    }

    #[test]
    fn an_unfed_tile_is_a_placeholder_and_a_fed_tile_is_a_live_preview() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]);
        // Neither has produced output: both render as placeholders, no Terminal.
        assert_eq!(
            m.view().terminals().count(),
            0,
            "unfed tiles are placeholders"
        );
        // Feeding output to "a" promotes it to a live Terminal preview.
        data(&mut m, "a", b"hello");
        assert_eq!(
            m.view().terminals().count(),
            1,
            "only the fed tile is a live preview"
        );
    }

    #[test]
    fn view_lays_tiles_in_a_non_overlapping_grid_with_one_focus_border() {
        let mut m = fleet();
        list(&mut m, &["a", "b", "c"]);
        // A viewport tall enough to show all three readable tiles (otherwise the
        // grid scrolls and culls the off-screen ones — exercised separately).
        const TALL: (u32, u32) = (400, 1000);
        m.update(UiEvent::Resize {
            w_px: TALL.0,
            h_px: TALL.1,
            scale: 1.0,
        });
        // Distinct content per tile, so each previews real routed output.
        data(&mut m, "a", b"AAA");
        data(&mut m, "b", b"BBB");
        data(&mut m, "c", b"CCC");

        let scene = m.view();
        let items = &scene.layers[0].items;

        // One Terminal preview per session.
        let terminals: Vec<RectPx> = items
            .iter()
            .filter_map(|it| match it {
                SceneItem::Terminal { rect, .. } => Some(*rect),
                _ => None,
            })
            .collect();
        assert_eq!(terminals.len(), 3, "one preview tile per session");

        // Tiles tile the grid — no two overlap, and each fits the viewport.
        for (i, a) in terminals.iter().enumerate() {
            assert!(
                a.x >= 0.0
                    && a.y >= 0.0
                    && a.x + a.w <= TALL.0 as f32
                    && a.y + a.h <= TALL.1 as f32,
                "tile {a:?} must fit the {TALL:?} viewport"
            );
            for b in &terminals[i + 1..] {
                assert!(
                    !rects_overlap(a, b),
                    "tiles must not overlap: {a:?} vs {b:?}"
                );
            }
        }

        // Exactly one tile is focused: the only one bordered and the only one not
        // dimmed, and the border tracks that tile's rect.
        let borders: Vec<RectPx> = items
            .iter()
            .filter_map(|it| match it {
                SceneItem::Border { rect, .. } => Some(*rect),
                _ => None,
            })
            .collect();
        assert_eq!(borders.len(), 1, "only the focused tile is bordered");
        let undimmed: Vec<RectPx> = items
            .iter()
            .filter_map(|it| match it {
                SceneItem::Terminal {
                    rect, dim: false, ..
                } => Some(*rect),
                _ => None,
            })
            .collect();
        assert_eq!(undimmed.len(), 1, "exactly one focused (undimmed) tile");
        // The border outlines the whole focused card; its live preview sits
        // inside that card (the preview is a sub-rect, below the metadata header).
        let (b, t) = (borders[0], undimmed[0]);
        assert!(
            t.x >= b.x && t.y >= b.y && t.x + t.w <= b.x + b.w && t.y + t.h <= b.y + b.h,
            "the focused preview {t:?} must sit within its card border {b:?}"
        );
    }

    #[test]
    fn reconcile_schedules_a_refresh_tick() {
        let mut m = fleet();
        let cmds = list(&mut m, &["a"]);
        assert!(cmds.contains(&Cmd::ScheduleTick {
            after_ms: REFRESH_MS
        }));
        // The refresh tick asks for a fresh listing.
        assert_eq!(
            m.update(UiEvent::Tick { now_ms: 1 }),
            vec![Cmd::ListSessions]
        );
    }

    #[test]
    fn session_data_routes_to_its_tile() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]);
        data(&mut m, "a", b"hello");
        assert!(m.tile_text("a").unwrap()[0].starts_with("hello"));
        assert!(m.tile_text("b").unwrap()[0].trim().is_empty());
    }

    #[test]
    fn tile_title_change_does_not_retitle_the_window() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]);
        // A tile setting its OSC title must not drive the overview window title.
        let cmds = data(&mut m, "a", b"\x1b]2;tile-a\x07");
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::SetTitle(_))),
            "the fleet overview does not retitle the window for a tile"
        );
    }

    #[test]
    fn previews_are_read_only() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]); // focus defaults to "a"
        // Text and ordinary keys must NOT reach a tile as input — fleet previews
        // are view-only, so neither forwards anything to the focused session.
        assert_eq!(
            m.update(UiEvent::Text("x".into())),
            vec![],
            "typed text is not forwarded to a preview"
        );
        assert_eq!(
            key(&mut m, Key::Char("a".into())),
            vec![],
            "ordinary keys are not forwarded to a preview"
        );
    }

    #[test]
    fn arrow_moves_focus_without_sending_input() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]);
        widen(&mut m);
        assert_eq!(m.focused(), Some("a"));
        let cmds = key(&mut m, Key::Named(NamedKey::ArrowRight));
        assert_eq!(m.focused(), Some("b"));
        assert_eq!(cmds, vec![Cmd::Redraw]); // focus only, nothing forwarded
        // Clamped at the end (no wrap on arrows).
        key(&mut m, Key::Named(NamedKey::ArrowRight));
        assert_eq!(m.focused(), Some("b"));
        key(&mut m, Key::Named(NamedKey::ArrowLeft));
        assert_eq!(m.focused(), Some("a"));
    }

    /// Press at the centre of `id`'s tile (its preview area).
    fn press(m: &mut FleetModel, id: &str) -> Vec<Cmd> {
        let (_, _, rect) = m.layout().into_iter().find(|(_, i, _)| i == id).unwrap();
        press_at(m, rect.x + rect.w / 2.0, rect.y + rect.h / 2.0)
    }

    /// Press at point `(x, y)`.
    fn press_at(m: &mut FleetModel, x: f32, y: f32) -> Vec<Cmd> {
        m.update(UiEvent::Pointer {
            phase: PointerPhase::Press,
            button: Some(crate::PointerButton::Left),
            pos: PointPx {
                x: x as f64,
                y: y as f64,
            },
            mods: crate::Mods::NONE,
            wheel_dy: 0.0,
            clicks: 1,
        })
    }

    /// The pixel rect of `id`'s `button` (centre is a good press target).
    fn button_rect(m: &FleetModel, id: &str, button: Button) -> RectPx {
        let (_, placements, band, _) = m.sections_layout();
        let (_, _, rect) = placements.into_iter().find(|(_, i, _)| i == id).unwrap();
        let (_, _, buttons) = card_layout(rect, band);
        buttons.iter().find(|(b, _)| *b == button).unwrap().1
    }

    #[test]
    fn clicking_a_detached_tile_focuses_and_opens_it() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]); // both detached
        let cmds = press(&mut m, "b");
        assert_eq!(m.focused(), Some("b"));
        assert!(
            cmds.contains(&Cmd::TakeOver("b".into())),
            "clicking a detached tile opens it: {cmds:?}"
        );
    }

    #[test]
    fn clicking_the_detach_button_detaches_instead_of_opening() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]);
        let r = button_rect(&m, "b", Button::Detach);
        let cmds = press_at(&mut m, r.x + r.w / 2.0, r.y + r.h / 2.0);
        assert!(
            cmds.contains(&Cmd::Detach("b".into())),
            "the detach button detaches: {cmds:?}"
        );
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::TakeOver(_))),
            "a button press must not also open the tile"
        );
    }

    #[test]
    fn the_kill_button_confirms_then_kills() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]);
        let r = button_rect(&m, "b", Button::Kill);
        let cmds = press_at(&mut m, r.x + r.w / 2.0, r.y + r.h / 2.0);
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Kill(_))),
            "kill is confirmed, not immediate: {cmds:?}"
        );
        let cmds = key(&mut m, Key::Named(NamedKey::Enter));
        assert!(
            cmds.contains(&Cmd::Kill("b".into())),
            "Enter confirms the kill: {cmds:?}"
        );
    }

    #[test]
    fn escape_cancels_a_pending_confirmation() {
        let mut m = fleet();
        list(&mut m, &["a"]);
        let r = button_rect(&m, "a", Button::Kill);
        press_at(&mut m, r.x + r.w / 2.0, r.y + r.h / 2.0);
        let cmds = key(&mut m, Key::Named(NamedKey::Escape));
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Kill(_))),
            "Esc cancels the confirmation: {cmds:?}"
        );
    }

    #[test]
    fn the_rename_button_opens_an_inline_edit_committed_on_enter() {
        let mut m = fleet();
        list(&mut m, &["a"]);
        let r = button_rect(&m, "a", Button::Rename);
        press_at(&mut m, r.x + r.w / 2.0, r.y + r.h / 2.0);
        // Typing buffers into the rename; nothing is committed yet.
        let cmds = m.update(UiEvent::Text("X".into()));
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Rename { .. })),
            "rename buffers until Enter: {cmds:?}"
        );
        // Enter commits the edited name.
        let cmds = key(&mut m, Key::Named(NamedKey::Enter));
        assert!(
            cmds.contains(&Cmd::Rename {
                session: "a".into(),
                name: "aX".into()
            }),
            "Enter commits the rename: {cmds:?}"
        );
    }

    #[test]
    fn opening_an_elsewhere_tile_asks_for_confirmation_first() {
        let mut m = fleet();
        let mut a = info("a");
        a.attached = true; // attached by another window
        m.update(UiEvent::SessionList(vec![a]));
        assert_eq!(m.locality_of("a"), Some(Locality::Elsewhere));
        let cmds = press(&mut m, "a");
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::TakeOver(_))),
            "must confirm before stealing an elsewhere session: {cmds:?}"
        );
        // Confirming with Enter issues the take-over.
        let cmds = key(&mut m, Key::Named(NamedKey::Enter));
        assert!(
            cmds.contains(&Cmd::TakeOver("a".into())),
            "Enter confirms the take-over: {cmds:?}"
        );
    }

    #[test]
    fn enter_opens_the_focused_tile() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]); // focus defaults to "a" (detached)
        let cmds = key(&mut m, Key::Named(NamedKey::Enter));
        assert!(
            cmds.contains(&Cmd::TakeOver("a".into())),
            "Enter opens the focused tile: {cmds:?}"
        );
    }

    #[test]
    fn view_dims_unfocused_tiles_and_borders_the_focused_one() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]); // focus "a"
        widen(&mut m); // both tiles on one visible row (else the 2nd scrolls off)
        data(&mut m, "a", b"A"); // make both tiles live previews
        data(&mut m, "b", b"B");
        let scene = m.view();
        let terminals: Vec<_> = scene.terminals().collect();
        assert_eq!(terminals.len(), 2);
        let dimmed = terminals
            .iter()
            .filter(|t| matches!(t, SceneItem::Terminal { dim: true, .. }))
            .count();
        assert_eq!(dimmed, 1, "exactly the one unfocused tile is dimmed");
        // The focused tile carries a border item.
        let borders = scene.layers[0]
            .items
            .iter()
            .filter(|it| matches!(it, SceneItem::Border { .. }))
            .count();
        assert_eq!(borders, 1);
    }

    #[test]
    fn bell_and_background_activity_raise_badges() {
        let mut m = fleet();
        widen(&mut m); // keep both tiles on one visible row
        let mut infos = vec![info("a"), info("b")];
        infos[1].bell = true; // "b" rang the bell
        m.update(UiEvent::SessionList(infos));
        let badges = |m: &FleetModel| {
            m.view().layers[0]
                .items
                .iter()
                .filter(|it| matches!(it, SceneItem::Badge { .. }))
                .count()
        };
        assert_eq!(badges(&m), 1, "bell raises a badge on b");

        // Background output on b (focus is on a) raises an activity badge even
        // without a bell.
        let mut m = fleet();
        list(&mut m, &["a", "b"]); // focus a
        widen(&mut m);
        data(&mut m, "b", b"work");
        assert_eq!(badges(&m), 1);
        // Focusing b clears its activity badge.
        key(&mut m, Key::Named(NamedKey::ArrowRight));
        assert_eq!(m.focused(), Some("b"));
        assert_eq!(badges(&m), 0);
    }

    #[test]
    fn card_metadata_omits_the_shell_command() {
        // A shell session (empty command) shows just name · pid — no "$SHELL".
        assert_eq!(card_meta("build", &[], 4012), "build \u{b7} 4012");
        // A real command is shown.
        assert_eq!(
            card_meta("edit", &["nvim".into(), "x.rs".into()], 40),
            "edit \u{b7} nvim x.rs \u{b7} 40"
        );
        // Unknown pid is omitted too.
        assert_eq!(card_meta("s", &[], 0), "s");
    }

    #[test]
    fn cards_stay_within_the_window_width_keep_a_min_height_and_do_not_overlap() {
        // A spread of session counts. Cards fit the window WIDTH and never overlap,
        // and every card keeps a readable minimum height — the grid grows past the
        // viewport (and scrolls) rather than collapsing previews to fit.
        let min_card_h =
            2.0 * (METRICS.line_height + 6.0) + MIN_PREVIEW_LINES * METRICS.line_height;
        for n in 1..=12usize {
            let mut m = fleet();
            let infos: Vec<SessionInfo> = (0..n).map(|i| info(&format!("s{i}"))).collect();
            m.update(UiEvent::SessionList(infos));
            let (_, placements, _, content_h) = m.sections_layout();
            assert_eq!(placements.len(), n);
            let w = SIZE.0 as f32;
            for (_, _, r) in &placements {
                assert!(r.x >= 0.0 && r.y >= 0.0, "n {n}: {r:?}");
                assert!(r.x + r.w <= w + 0.5, "width overflow n {n}: {r:?}");
                assert!(
                    r.h >= min_card_h - 0.5,
                    "card collapsed below the minimum height n {n}: {r:?}"
                );
            }
            for (i, (_, _, a)) in placements.iter().enumerate() {
                for (_, _, b) in &placements[i + 1..] {
                    assert!(!rects_overlap(a, b), "overlap n {n}: {a:?} vs {b:?}");
                }
            }
            // With enough sessions the grid overflows the viewport (then scrolls).
            if n >= 2 {
                assert!(
                    content_h > SIZE.1 as f32,
                    "n {n}: {content_h} should overflow the {}px viewport",
                    SIZE.1
                );
            }
        }
    }

    #[test]
    fn cards_keep_the_terminal_aspect_ratio() {
        // The preview area must keep the terminal's width:height ratio, not stretch
        // to full column width once the card height is capped.
        let aspect =
            (PREVIEW_COLS as f32 * METRICS.advance) / (PREVIEW_ROWS as f32 * METRICS.line_height);
        // A range of windows, including the default that surfaced the bug.
        for (w, h) in [(720, 432), (1400, 900), (1000, 700), (500, 1000)] {
            let mut m = fleet();
            m.update(UiEvent::Resize {
                w_px: w,
                h_px: h,
                scale: 1.0,
            });
            list_many(&mut m, 4);
            let (_, placements, band, _) = m.sections_layout();
            for (_, _, r) in &placements {
                let preview_w = r.w; // the preview spans the full card width
                let preview_h = r.h - 2.0 * band; // minus the header + footer bands
                let got = preview_w / preview_h;
                assert!(
                    (got - aspect).abs() < 0.02,
                    "card {w}x{h}: preview aspect {got:.3} != terminal {aspect:.3} ({r:?})"
                );
            }
        }
    }

    #[test]
    fn crowded_grids_use_the_compact_card_size() {
        let band = METRICS.line_height + 6.0;
        let compact =
            2.0 * band + PREVIEW_ROWS as f32 * METRICS.line_height * PREVIEW_COMPACT_SCALE;

        // Far more sessions than fit: cards settle at the compact thumbnail size and
        // the grid scrolls, rather than each card growing.
        let mut m = fleet();
        m.update(UiEvent::Resize {
            w_px: 2000,
            h_px: 1200,
            scale: 1.0,
        });
        list_many(&mut m, 40);
        let ch = m.sections_layout().1[0].2.h;
        assert!(
            (ch - compact).abs() < 1.0,
            "a crowded grid should use the compact card ({compact}), got {ch}"
        );
    }

    #[test]
    fn a_few_sessions_get_larger_previews() {
        let band = METRICS.line_height + 6.0;
        let native = 2.0 * band + PREVIEW_ROWS as f32 * METRICS.line_height;
        let size = UiEvent::Resize {
            w_px: 2000,
            h_px: 1200,
            scale: 1.0,
        };

        // A couple of sessions grow to use the space; a crowded grid stays compact.
        let mut few = fleet();
        few.update(size.clone());
        list_many(&mut few, 2);
        let few_h = few.sections_layout().1[0].2.h;

        let mut many = fleet();
        many.update(size);
        list_many(&mut many, 40);
        let many_h = many.sections_layout().1[0].2.h;

        assert!(
            few_h > many_h + 1.0,
            "a couple of sessions should preview larger than a crowded grid ({few_h} vs {many_h})"
        );
        assert!(
            few_h <= native + 1.0,
            "previews should not grow past native size ({few_h} > {native})"
        );
    }

    #[test]
    fn the_grid_scrolls_with_the_wheel_and_clamps_to_the_ends() {
        let mut m = fleet();
        list_many(&mut m, 6); // overflows the 400x200 viewport
        assert!(
            m.max_scroll() > 0.0,
            "the grid must overflow to be scrollable"
        );
        assert_eq!(m.scroll_y, 0.0, "starts pinned to the top");

        // Wheel up at the top is a no-op (already clamped).
        assert_eq!(wheel(&mut m, 1.0), vec![], "no scroll past the top");
        assert_eq!(m.scroll_y, 0.0);

        // Wheel down scrolls toward the bottom.
        assert_eq!(wheel(&mut m, -1.0), vec![Cmd::Redraw]);
        assert!(m.scroll_y > 0.0, "wheel down scrolled");

        // Many notches clamp at the bottom; further ones do nothing.
        for _ in 0..50 {
            wheel(&mut m, -1.0);
        }
        assert_eq!(m.scroll_y, m.max_scroll(), "clamps at the bottom");
        assert_eq!(wheel(&mut m, -1.0), vec![], "no scroll past the bottom");

        // And back up to the top.
        for _ in 0..50 {
            wheel(&mut m, 1.0);
        }
        assert_eq!(m.scroll_y, 0.0, "returns to the top");
    }

    #[test]
    fn arrow_navigation_scrolls_the_focused_tile_into_view() {
        let mut m = fleet();
        // A viewport taller than one tile but shorter than the whole column, so
        // walking down must scroll while keeping the focused tile fully visible.
        m.update(UiEvent::Resize {
            w_px: 400,
            h_px: 500,
            scale: 1.0,
        });
        list_many(&mut m, 6); // single column (narrow), taller than 500px
        assert_eq!(m.focused(), Some("s0"));
        assert_eq!(m.scroll_y, 0.0);

        let view_h = 500.0;
        for _ in 0..5 {
            key(&mut m, Key::Named(NamedKey::ArrowDown));
            let (_, placements, _, _) = m.sections_layout();
            let (_, _, r) = placements
                .into_iter()
                .find(|(_, id, _)| Some(id.as_str()) == m.focused())
                .unwrap();
            let (top, bottom) = (r.y - m.scroll_y, r.y + r.h - m.scroll_y);
            assert!(
                top >= -0.5 && bottom <= view_h + 0.5,
                "focused tile {top}..{bottom} must stay within the {view_h}px viewport"
            );
        }
        assert!(m.scroll_y > 0.0, "navigating down scrolled the grid");
    }

    #[test]
    fn offscreen_tiles_are_culled_but_the_section_header_stays() {
        let mut m = fleet();
        m.update(UiEvent::Resize {
            w_px: 400,
            h_px: 400,
            scale: 1.0,
        });
        list_many(&mut m, 6);
        for i in 0..6 {
            data(&mut m, &format!("s{i}"), b"live"); // every tile a live preview
        }
        let visible = m.view().terminals().count();
        assert!(
            (1..6).contains(&visible),
            "only on-screen tiles render previews, got {visible}/6"
        );
        // The section header is still emitted even though most tiles are culled.
        assert_eq!(headers(&m).len(), 1, "the section header survives culling");
    }

    #[test]
    fn enlarging_the_window_clamps_the_scroll_offset() {
        let mut m = fleet();
        list_many(&mut m, 6);
        for _ in 0..50 {
            wheel(&mut m, -1.0); // scroll to the bottom
        }
        assert!(m.scroll_y > 0.0 && m.scroll_y == m.max_scroll());

        // A viewport tall enough to hold everything leaves nothing to scroll.
        m.update(UiEvent::Resize {
            w_px: 400,
            h_px: 5000,
            scale: 1.0,
        });
        assert_eq!(m.max_scroll(), 0.0, "a tall window fits the whole grid");
        assert_eq!(m.scroll_y, 0.0, "scroll clamps back to the top");
    }

    #[test]
    fn shift_tab_moves_focus_backward() {
        let mut m = fleet();
        list(&mut m, &["a", "b", "c"]); // focus "a"
        key(&mut m, Key::Named(NamedKey::Tab)); // a -> b
        assert_eq!(m.focused(), Some("b"));
        // Shift+Tab steps back and wraps.
        let cmds = m.update(UiEvent::Key {
            key: Key::Named(NamedKey::Tab),
            mods: crate::Mods::SHIFT,
            kind: KeyEventKind::Press,
            alts: None,
        });
        assert_eq!(m.focused(), Some("a"));
        assert_eq!(cmds, vec![Cmd::Redraw]); // focus only, never SendInput
        // Wrapping backward past the start lands on the last tile.
        m.update(UiEvent::Key {
            key: Key::Named(NamedKey::Tab),
            mods: crate::Mods::SHIFT,
            kind: KeyEventKind::Press,
            alts: None,
        });
        assert_eq!(m.focused(), Some("c"));
    }

    #[test]
    fn the_fleet_never_attaches_sessions_itself() {
        // No auto-attach for anyone — neither a detached session nor one owned by
        // another window. Live previews come only from sessions this window drives.
        let mut m = fleet(); // mine is empty
        let mut elsewhere = info("foreign");
        elsewhere.attached = true; // attached by some other window
        let cmds = m.update(UiEvent::SessionList(vec![info("mine-detached"), elsewhere]));
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Attach(_))),
            "the fleet must not attach any session: {cmds:?}"
        );
        assert_eq!(m.locality_of("foreign"), Some(Locality::Elsewhere));
        assert_eq!(m.locality_of("mine-detached"), Some(Locality::Detached));
        assert_eq!(m.tile_count(), 2); // both shown, as placeholder tiles
    }

    #[test]
    fn into_single_keeps_the_owned_session_even_when_focus_moved() {
        // The window owns "alpha"; the fleet also previews a foreign "beta".
        let mine = HashSet::from(["alpha".to_string()]);
        let primary = TerminalModel::new("alpha".to_string(), 80, 24, METRICS);
        let (mut f, _) = FleetModel::adopting(primary, Vec::new(), METRICS, SIZE, 1.0, mine);
        f.update(UiEvent::SessionList(vec![info("alpha"), info("beta")]));
        // Move focus onto the foreign tile (it's in the section below ours).
        f.update(UiEvent::Key {
            key: Key::Named(NamedKey::ArrowDown),
            mods: crate::Mods::NONE,
            kind: KeyEventKind::Press,
            alts: None,
        });
        assert_eq!(f.focused(), Some("beta"));
        // Toggling back returns the OWNED session and detaches nothing — the
        // other sessions stay attached (warm) for Ctrl-Tab and live previews.
        let (model, _warm, cmds) = f.into_single(SIZE, 1.0);
        assert_eq!(model.session(), "alpha", "keeps the window's own session");
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Detach(_))),
            "no session is detached on toggle-back: {cmds:?}"
        );
    }

    #[test]
    fn idle_reconcile_with_no_changes_does_not_redraw() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]);
        // A re-list of the same sessions changes nothing visible.
        let cmds = list(&mut m, &["a", "b"]);
        assert!(
            !cmds.contains(&Cmd::Redraw),
            "an idle reconcile must not emit a redraw"
        );
        // It still re-arms the refresh tick.
        assert!(cmds.contains(&Cmd::ScheduleTick {
            after_ms: REFRESH_MS
        }));
    }
}
