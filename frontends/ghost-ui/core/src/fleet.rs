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

use ghost_render::{
    BadgeKind, CellMetrics, Frame, Layer, RectPx, Rgba, Run, Scene, SceneId, SceneItem, Style,
    layout_frame,
};
use ghost_vt::session::SessionInfo;

use crate::input::{Key, Mods, NamedKey};
use crate::{Cmd, PointPx, PointerPhase, SessionId, TerminalModel, UiEvent};

const GAP: f32 = 8.0;
const FOCUS_BORDER: f32 = 2.0;
const FOCUS_COLOR: Rgba = [0.30, 0.60, 0.95, 1.0];
const BADGE_PX: f32 = 10.0;
/// Height of a section's header band.
const SECTION_HEADER_PX: f32 = 16.0;
/// Colour of section header labels.
const SECTION_LABEL_COLOR: Rgba = [0.65, 0.70, 0.78, 1.0];
/// Placeholder card colours for a tile with no live preview.
const PLACEHOLDER_BG: Rgba = [0.12, 0.13, 0.16, 1.0];
const PLACEHOLDER_FG: Rgba = [0.70, 0.74, 0.80, 1.0];
/// Default grid for a fleet-created tile mirror until the shell hands it a
/// real-size model; previews are scaled to the tile regardless of this size.
const PREVIEW_COLS: u16 = 80;
const PREVIEW_ROWS: u16 = 24;
/// How often (ms) the fleet asks the shell to re-enumerate sessions.
const REFRESH_MS: u64 = 500;

/// A laid-out tile: stable handle, session id, and pixel rect.
type Placement = (u64, SessionId, RectPx);
/// A section header: its locality and the header band's rect.
type SectionHeader = (Locality, RectPx);

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

struct Tile {
    handle: u64,
    id: SessionId,
    model: TerminalModel,
    bell: bool,
    locality: Locality,
    /// Unseen-output count since this tile was last focused (drives the badge).
    activity: u32,
    /// Whether the shell has delivered output for this tile (i.e. this window
    /// drives the session). An unfed tile renders as a placeholder, not a live
    /// preview.
    fed: bool,
    /// Cached laid-out preview, rebuilt only when this tile's content or size
    /// changes (see [`FleetModel::refresh_dirty_frames`]); `view` clones it rather
    /// than re-running `layout_frame` for every tile every frame. `None` until the
    /// first refresh.
    frame: Option<Frame>,
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
}

/// Place `n` tiles in a near-square grid within `size_px`, with a uniform gap,
/// row-major. Pure geometry — shared by `view` and pointer hit-testing.
fn grid_rects(n: usize, size_px: (u32, u32), gap: f32) -> Vec<RectPx> {
    if n == 0 {
        return Vec::new();
    }
    let cols = (n as f32).sqrt().ceil() as usize;
    let rows = n.div_ceil(cols);
    let (w, h) = (size_px.0 as f32, size_px.1 as f32);
    let cell_w = ((w - gap * (cols as f32 + 1.0)) / cols as f32).max(1.0);
    let cell_h = ((h - gap * (rows as f32 + 1.0)) / rows as f32).max(1.0);
    (0..n)
        .map(|i| {
            let (r, c) = (i / cols, i % cols);
            // Clamp the origin into the window, then clamp the size to what's
            // left, so a degenerate (tiny) window can never overflow the bounds
            // even though the cell size is floored at 1px.
            let x = (gap + c as f32 * (cell_w + gap)).min((w - 1.0).max(0.0));
            let y = (gap + r as f32 * (cell_h + gap)).min((h - 1.0).max(0.0));
            RectPx {
                x,
                y,
                w: cell_w.min(w - x),
                h: cell_h.min(h - y),
            }
        })
        .collect()
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
        metrics: CellMetrics,
        size_px: (u32, u32),
        scale: f32,
        mine: HashSet<SessionId>,
    ) -> (Self, Vec<Cmd>) {
        let mut f = FleetModel::new(metrics, size_px, mine);
        f.scale = scale;
        let id = primary.session().to_string();
        let locality = locality_for(&f.mine, &id, true);
        f.focused = Some(id.clone());
        // The adopted primary is already live; mark it fed so it isn't re-attached.
        f.push_tile(id, primary, false, locality);
        if let Some(t) = f.tiles.last_mut() {
            t.fed = true;
        }
        // The fleet never resizes its tiles' sessions; just repaint.
        (f, vec![Cmd::Redraw])
    }

    fn push_tile(&mut self, id: SessionId, model: TerminalModel, bell: bool, locality: Locality) {
        let handle = self.next_handle;
        self.next_handle += 1;
        self.tiles.push(Tile {
            handle,
            id,
            model,
            bell,
            locality,
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

    /// Extract a single terminal for a toggle back to the single view, detaching
    /// every other tile's session. The window's *owned* session is kept (stable
    /// identity), falling back to the focused tile, then any tile — never a
    /// foreign session we merely previewed, so the single view always returns to
    /// what the window actually drives.
    pub fn into_single(self, size_px: (u32, u32), scale: f32) -> (TerminalModel, Vec<Cmd>) {
        self.into_single_keeping(None, size_px, scale)
    }

    /// Like [`into_single`](Self::into_single) but, when `target` names a present
    /// tile, keeps *that* session (a take-over of a specific tile). Otherwise it
    /// falls back to the owned session, then the focused tile, then any tile.
    pub fn into_single_keeping(
        self,
        target: Option<SessionId>,
        size_px: (u32, u32),
        scale: f32,
    ) -> (TerminalModel, Vec<Cmd>) {
        let metrics = self.metrics;
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
        let mut kept = None;
        let mut cmds = Vec::new();
        for tile in self.tiles {
            if Some(&tile.id) == keep.as_ref() {
                kept = Some(tile.model);
            } else {
                cmds.push(Cmd::Detach(tile.id));
            }
        }
        let mut model =
            kept.unwrap_or_else(|| TerminalModel::new(keep.unwrap_or_default(), 1, 1, metrics));
        cmds.append(&mut model.update(UiEvent::Resize {
            w_px: size_px.0.max(1),
            h_px: size_px.1.max(1),
            scale: scale as f64,
        }));
        (model, cmds)
    }

    /// Leave the overview showing `id` *specifically* — a spawn or take-over.
    /// Keeps `id`'s tile if it has one (preserving its screen); otherwise builds
    /// a fresh terminal for it (the just-spawned session has no tile yet). Either
    /// way every other previewed tile is detached. Unlike
    /// [`into_single_keeping`](Self::into_single_keeping) there is no fallback to
    /// a different session: the caller asked for `id`.
    pub fn into_single_adopting(
        self,
        id: SessionId,
        size_px: (u32, u32),
        scale: f32,
    ) -> (TerminalModel, Vec<Cmd>) {
        let metrics = self.metrics;
        let mut kept = None;
        let mut cmds = Vec::new();
        for tile in self.tiles {
            if tile.id == id {
                kept = Some(tile.model);
            } else {
                cmds.push(Cmd::Detach(tile.id));
            }
        }
        let mut model = kept.unwrap_or_else(|| TerminalModel::new(id, 1, 1, metrics));
        cmds.append(&mut model.update(UiEvent::Resize {
            w_px: size_px.0.max(1),
            h_px: size_px.1.max(1),
            scale: scale as f64,
        }));
        (model, cmds)
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
            UiEvent::Key {
                key, mods, kind, ..
            } if kind.is_down() && nav(&key, mods).is_some() => {
                match nav(&key, mods).unwrap() {
                    Nav::Dir(d) => self.move_focus_dir(d),
                    Nav::Step(delta, wrap) => self.move_focus_linear(delta, wrap),
                }
                vec![Cmd::Redraw]
            }
            // Re-enumerate on the scheduled refresh tick.
            UiEvent::Tick { .. } => vec![Cmd::ListSessions],
            UiEvent::Pointer {
                phase, pos, clicks, ..
            } => self.pointer(phase, pos, clicks),
            // Previews are view-only: text, ordinary keys, focus and paste
            // replies are never forwarded to a tile as input.
            _ => Vec::new(),
        };
        // Rebuild any preview whose content or size this event changed, so `view`
        // can stay a pure read of cached frames.
        self.refresh_dirty_frames();
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
                tile.frame = Some(layout_frame(tile.model.screen().vt(), metrics));
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
                if tile.bell != info.bell || tile.locality != locality {
                    dirty = true;
                }
                tile.bell = info.bell;
                tile.locality = locality;
            } else {
                let model =
                    TerminalModel::new(info.name.clone(), PREVIEW_COLS, PREVIEW_ROWS, self.metrics);
                self.push_tile(info.name.clone(), model, info.bell, locality);
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

    /// Section headers `(locality, rect)` for every non-empty attach-state
    /// section, plus all tile placements `(handle, id, rect)`, laid out as
    /// stacked per-section grids (a header band atop each section's grid). The
    /// height is shared between sections in proportion to the rows each needs.
    /// Shared by `view`, navigation and pointer hit-testing.
    fn sections_layout(&self) -> (Vec<SectionHeader>, Vec<Placement>) {
        let (w, h) = (self.size_px.0 as f32, self.size_px.1 as f32);
        // Tiles grouped by locality, preserving insertion order within each;
        // empty sections are dropped so they get no header.
        let sections: Vec<(Locality, Vec<&Tile>)> = [
            Locality::ThisWindow,
            Locality::Elsewhere,
            Locality::Detached,
        ]
        .into_iter()
        .map(|loc| {
            (
                loc,
                self.tiles.iter().filter(|t| t.locality == loc).collect(),
            )
        })
        .filter(|(_, ts): &(_, Vec<&Tile>)| !ts.is_empty())
        .collect();
        if sections.is_empty() {
            return (Vec::new(), Vec::new());
        }
        // Rows each section needs (near-square), to share height proportionally.
        let cols_of = |n: usize| (n as f32).sqrt().ceil().max(1.0) as usize;
        let rows_of = |n: usize| n.div_ceil(cols_of(n));
        let total_rows: usize = sections.iter().map(|(_, ts)| rows_of(ts.len())).sum();
        let grid_h_total = (h - SECTION_HEADER_PX * sections.len() as f32).max(1.0);

        let mut headers = Vec::new();
        let mut tiles = Vec::new();
        let mut y = 0.0_f32;
        for (loc, ts) in &sections {
            headers.push((
                *loc,
                RectPx {
                    x: GAP,
                    y,
                    w: (w - 2.0 * GAP).max(1.0),
                    h: SECTION_HEADER_PX,
                },
            ));
            y += SECTION_HEADER_PX;
            let band_h = (grid_h_total * rows_of(ts.len()) as f32 / total_rows as f32).max(1.0);
            for (t, mut r) in ts
                .iter()
                .zip(grid_rects(ts.len(), (w as u32, band_h as u32), GAP))
            {
                r.y += y; // offset the section's grid into its band
                tiles.push((t.handle, t.id.clone(), r));
            }
            y += band_h;
        }
        (headers, tiles)
    }

    /// Tile placements `(handle, id, rect)` in section order.
    fn layout(&self) -> Vec<Placement> {
        self.sections_layout().1
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

    fn pointer(&mut self, phase: PointerPhase, pos: PointPx, clicks: u8) -> Vec<Cmd> {
        if phase != PointerPhase::Press {
            return Vec::new(); // the overview only reacts to clicks (focus / open)
        }
        let hit = self
            .layout()
            .into_iter()
            .find(|(_, _, r)| r.contains(pos.x as f32, pos.y as f32));
        let Some((_, id, _)) = hit else {
            return Vec::new();
        };
        self.set_focus(id.clone());
        // A double-click opens the tile: take it over into this window's single
        // view. A session live in another window (`Elsewhere`) is left alone —
        // taking it over would steal its display client (no observer-attach yet).
        if clicks >= 2 && self.locality_of(&id) != Some(Locality::Elsewhere) {
            vec![Cmd::TakeOver(id), Cmd::Redraw]
        } else {
            vec![Cmd::Redraw]
        }
    }

    // ---- view ----

    pub fn view(&self) -> Scene {
        let (headers, placements) = self.sections_layout();
        let mut items = Vec::new();
        for (loc, rect) in headers {
            items.push(SceneItem::Text {
                id: SceneId::Section(loc.rank()),
                rect,
                runs: vec![label_run(section_label(loc))],
                metrics: self.effective_metrics(),
                color: SECTION_LABEL_COLOR,
            });
        }
        for (handle, id, rect) in placements {
            let Some(tile) = self.tiles.iter().find(|t| t.id == id) else {
                continue;
            };
            let focused = self.focused.as_deref() == Some(id.as_str());
            if tile.fed {
                // A session this window drives: a live preview laid out at its
                // real size, which the renderer scales to fit the tile. The cache
                // fallback only fires if a tile is viewed before its first refresh.
                let frame = tile.frame.clone().unwrap_or_else(|| {
                    layout_frame(tile.model.screen().vt(), self.effective_metrics())
                });
                items.push(SceneItem::Terminal {
                    id: SceneId::Tile(handle),
                    rect,
                    frame,
                    selection: if focused {
                        tile.model.selection()
                    } else {
                        None
                    },
                    dim: !focused,
                });
            } else {
                // No live source yet (detached / attached elsewhere): a
                // placeholder card carrying the session name.
                items.push(SceneItem::Rect {
                    id: SceneId::Tile(handle),
                    rect,
                    color: PLACEHOLDER_BG,
                    radius: 4.0,
                });
                items.push(SceneItem::Text {
                    id: SceneId::Label(handle),
                    rect: RectPx {
                        x: rect.x + 6.0,
                        y: rect.y + 6.0,
                        w: (rect.w - 12.0).max(1.0),
                        h: self.effective_metrics().line_height,
                    },
                    runs: vec![label_run(&id)],
                    metrics: self.effective_metrics(),
                    color: PLACEHOLDER_FG,
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
        let mut scene = Scene::new(self.size_px);
        scene.layers.push(Layer { z: 0, items });
        scene
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
                    && a.x + a.w <= SIZE.0 as f32
                    && a.y + a.h <= SIZE.1 as f32,
                "tile {a:?} must fit the {SIZE:?} viewport"
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
        assert_eq!(
            borders[0], undimmed[0],
            "the border outlines the focused tile"
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

    #[test]
    fn click_focuses_the_hit_tile() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]);
        // Find b's rect and click its center.
        let (_, _, rect) = m.layout().into_iter().find(|(_, id, _)| id == "b").unwrap();
        let cmds = m.update(UiEvent::Pointer {
            phase: PointerPhase::Press,
            button: Some(crate::PointerButton::Left),
            pos: PointPx {
                x: (rect.x + rect.w / 2.0) as f64,
                y: (rect.y + rect.h / 2.0) as f64,
            },
            mods: crate::Mods::NONE,
            wheel_dy: 0.0,
            clicks: 1,
        });
        assert_eq!(m.focused(), Some("b"));
        assert_eq!(cmds, vec![Cmd::Redraw]);
    }

    /// Press at the centre of `id`'s tile with the given click count.
    fn press(m: &mut FleetModel, id: &str, clicks: u8) -> Vec<Cmd> {
        let (_, _, rect) = m.layout().into_iter().find(|(_, i, _)| i == id).unwrap();
        m.update(UiEvent::Pointer {
            phase: PointerPhase::Press,
            button: Some(crate::PointerButton::Left),
            pos: PointPx {
                x: (rect.x + rect.w / 2.0) as f64,
                y: (rect.y + rect.h / 2.0) as f64,
            },
            mods: crate::Mods::NONE,
            wheel_dy: 0.0,
            clicks,
        })
    }

    #[test]
    fn double_click_takes_over_a_detached_tile() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]); // both detached → take-over-able
        let cmds = press(&mut m, "b", 2);
        assert!(
            cmds.contains(&Cmd::TakeOver("b".into())),
            "double-click opens (takes over) the tile: {cmds:?}"
        );
        assert_eq!(m.focused(), Some("b"));
    }

    #[test]
    fn single_click_focuses_without_taking_over() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]);
        let cmds = press(&mut m, "b", 1);
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::TakeOver(_))),
            "a single click only focuses: {cmds:?}"
        );
    }

    #[test]
    fn double_click_leaves_a_session_live_elsewhere_alone() {
        let mut m = fleet();
        // Attached but not ours = Elsewhere; taking it over would steal its
        // display client, which we don't do without observer-attach.
        let mut a = info("a");
        a.attached = true;
        m.update(UiEvent::SessionList(vec![a]));
        assert_eq!(m.locality_of("a"), Some(Locality::Elsewhere));
        let cmds = press(&mut m, "a", 2);
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::TakeOver(_))),
            "must not steal an Elsewhere session: {cmds:?}"
        );
    }

    #[test]
    fn view_dims_unfocused_tiles_and_borders_the_focused_one() {
        let mut m = fleet();
        list(&mut m, &["a", "b"]); // focus "a"
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
        data(&mut m, "b", b"work");
        assert_eq!(badges(&m), 1);
        // Focusing b clears its activity badge.
        key(&mut m, Key::Named(NamedKey::ArrowRight));
        assert_eq!(m.focused(), Some("b"));
        assert_eq!(badges(&m), 0);
    }

    #[test]
    fn grid_rects_stay_within_bounds() {
        // Including degenerate tiny windows that force the 1px cell clamp.
        for &size in &[(400u32, 200u32), (60, 40), (20, 20), (1, 1)] {
            for n in 1..=16 {
                for r in grid_rects(n, size, GAP) {
                    assert!(r.x >= 0.0 && r.y >= 0.0, "size {size:?} n {n}: {r:?}");
                    assert!(r.w >= 1.0 && r.h >= 1.0, "size {size:?} n {n}: {r:?}");
                    assert!(
                        r.x + r.w <= size.0 as f32 + 0.01,
                        "x overflow at size {size:?} n {n}: {r:?}"
                    );
                    assert!(
                        r.y + r.h <= size.1 as f32 + 0.01,
                        "y overflow at size {size:?} n {n}: {r:?}"
                    );
                }
            }
        }
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
        let (mut f, _) = FleetModel::adopting(primary, METRICS, SIZE, 1.0, mine);
        f.update(UiEvent::SessionList(vec![info("alpha"), info("beta")]));
        // Move focus onto the foreign tile (it's in the section below ours).
        f.update(UiEvent::Key {
            key: Key::Named(NamedKey::ArrowDown),
            mods: crate::Mods::NONE,
            kind: KeyEventKind::Press,
            alts: None,
        });
        assert_eq!(f.focused(), Some("beta"));
        // Toggling back returns the OWNED session, and detaches the foreign one.
        let (model, cmds) = f.into_single(SIZE, 1.0);
        assert_eq!(model.session(), "alpha", "keeps the window's own session");
        assert!(cmds.contains(&Cmd::Detach("beta".into())));
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
