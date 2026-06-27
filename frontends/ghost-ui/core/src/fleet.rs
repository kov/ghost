//! `FleetModel` — the overview grid of live session previews, as a pure reducer.
//!
//! Each tile is a real attached session mirrored by its own [`TerminalModel`]
//! (the fleet owns them), so previews are live, not snapshots. The model
//! reconciles a `SessionList` into tiles (emitting `Attach`/`Detach`), routes
//! pumped `SessionData` to the matching tile by id, moves focus on arrow/Tab
//! navigation (previews are view-only, so keystrokes/text are never forwarded),
//! focuses on click via a shared grid layout that doubles as the
//! hit-test, and renders the whole grid — dimming unfocused tiles, bordering the
//! focused one, and badging bell/activity. Like [`TerminalModel`] it is pure, so
//! all of this is asserted headlessly by feeding events and inspecting the
//! returned `Vec<Cmd>`, the focused id, per-tile screen text, and the `Scene`.

use std::collections::HashSet;

use ghost_render::{
    BadgeKind, CellMetrics, Frame, Layer, RectPx, Rgba, Scene, SceneId, SceneItem, layout_frame,
};
use ghost_vt::session::SessionInfo;

use crate::input::{Key, Mods, NamedKey};
use crate::{Cmd, PointPx, PointerPhase, SessionId, TerminalModel, UiEvent};

const GAP: f32 = 8.0;
const FOCUS_BORDER: f32 = 2.0;
const FOCUS_COLOR: Rgba = [0.30, 0.60, 0.95, 1.0];
const BADGE_PX: f32 = 10.0;
/// How often (ms) the fleet asks the shell to re-enumerate sessions.
const REFRESH_MS: u64 = 500;

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
    /// Whether the shell ever delivered output for this tile. Until then we keep
    /// re-emitting `Attach` (a lost attach race self-heals on the next refresh).
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

/// Whether the fleet may attach a session to drive a live preview. Sessions
/// owned by another window (`Elsewhere`) are left alone — attaching would steal
/// their display client and shrink their PTY to the tile size.
fn attachable(locality: Locality) -> bool {
    !matches!(locality, Locality::Elsewhere)
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

/// Arrow/Tab navigation: `(focus delta, wrap?)`, or `None` if not a nav key.
/// Shift+Tab steps backward; arrows clamp at the ends, Tab wraps.
fn nav(key: &Key, mods: Mods) -> Option<(i32, bool)> {
    match key {
        Key::Named(NamedKey::ArrowRight | NamedKey::ArrowDown) => Some((1, false)),
        Key::Named(NamedKey::ArrowLeft | NamedKey::ArrowUp) => Some((-1, false)),
        Key::Named(NamedKey::Tab) if mods.shift => Some((-1, true)),
        Key::Named(NamedKey::Tab) => Some((1, true)),
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
        let mut cmds = f.relayout();
        cmds.push(Cmd::Redraw); // repaint into the overview immediately
        (f, cmds)
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
                self.relayout()
            }
            UiEvent::Key {
                key, mods, kind, ..
            } if kind.is_down() && nav(&key, mods).is_some() => {
                let (delta, wrap) = nav(&key, mods).unwrap();
                self.move_focus(delta, wrap);
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

        // Add new tiles; refresh bell/locality on existing ones. Only attach
        // sessions we may safely drive (ours or detached); re-attach a safe tile
        // that has never produced output (a lost attach race self-heals here).
        for info in &infos {
            let locality = locality_for(&self.mine, &info.name, info.attached);
            if let Some(tile) = self.tiles.iter_mut().find(|t| t.id == info.name) {
                if tile.bell != info.bell || tile.locality != locality {
                    dirty = true;
                }
                tile.bell = info.bell;
                tile.locality = locality;
                if attachable(locality) && !tile.fed {
                    cmds.push(Cmd::Attach(info.name.clone()));
                }
            } else {
                let model = TerminalModel::new(info.name.clone(), 1, 1, self.metrics);
                self.push_tile(info.name.clone(), model, info.bell, locality);
                if attachable(locality) {
                    cmds.push(Cmd::Attach(info.name.clone()));
                }
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

        let resizes = self.relayout();
        if !resizes.is_empty() {
            dirty = true;
        }
        cmds.extend(resizes);
        if dirty {
            cmds.push(Cmd::Redraw);
        }
        // Re-arm the periodic refresh.
        cmds.push(Cmd::ScheduleTick {
            after_ms: REFRESH_MS,
        });
        cmds
    }

    /// Lay tiles out (by locality, then insertion order) and resize each tile's
    /// model to its rect so its preview renders at the tile's size. Returns only
    /// the `Resize` effects for tiles that actually changed size (callers decide
    /// whether to also redraw), so an idle re-layout produces nothing.
    fn relayout(&mut self) -> Vec<Cmd> {
        let placements = self.layout();
        let scale = f64::from(self.scale);
        let mut cmds = Vec::new();
        for (_, id, rect) in placements {
            if let Some(tile) = self.tiles.iter_mut().find(|t| t.id == id) {
                for c in tile.model.update(UiEvent::Resize {
                    w_px: rect.w.max(1.0) as u32,
                    h_px: rect.h.max(1.0) as u32,
                    scale,
                }) {
                    // The tile model emits Resize only when its grid changed; its
                    // Redraw is subsumed by the caller's own redraw decision.
                    if matches!(c, Cmd::Resize { .. }) {
                        tile.frame_dirty = true; // grid changed; preview is stale
                        cmds.push(c);
                    }
                }
            }
        }
        cmds
    }

    /// Tile placements `(handle, id, rect)` in locality grid order.
    fn layout(&self) -> Vec<(u64, SessionId, RectPx)> {
        let mut order: Vec<&Tile> = self.tiles.iter().collect();
        order.sort_by_key(|t| t.locality.rank()); // stable: insertion order within a locality
        let rects = grid_rects(order.len(), self.size_px, GAP);
        order
            .into_iter()
            .zip(rects)
            .map(|(t, r)| (t.handle, t.id.clone(), r))
            .collect()
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

    fn move_focus(&mut self, delta: i32, wrap: bool) {
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
        let mut items = Vec::new();
        for (handle, id, rect) in self.layout() {
            let Some(tile) = self.tiles.iter().find(|t| t.id == id) else {
                continue;
            };
            let focused = self.focused.as_deref() == Some(id.as_str());
            // Cached by `refresh_dirty_frames`; the fallback only fires if a tile
            // is somehow viewed before its first refresh.
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
    fn reconcile_attaches_new_and_detaches_gone() {
        let mut m = fleet();
        let cmds = list(&mut m, &["a", "b"]);
        assert!(cmds.contains(&Cmd::Attach("a".into())));
        assert!(cmds.contains(&Cmd::Attach("b".into())));
        assert_eq!(m.tile_count(), 2);
        assert_eq!(m.focused(), Some("a")); // first tile focused by default

        let cmds = list(&mut m, &["a"]);
        assert!(cmds.contains(&Cmd::Detach("b".into())));
        assert_eq!(m.tile_count(), 1);
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
    fn foreign_attached_session_is_a_placeholder_not_attached() {
        // A session owned by another window (attached elsewhere, not ours) must
        // NOT be attached — that would steal its display client.
        let mut m = fleet(); // mine is empty
        let mut elsewhere = info("foreign");
        elsewhere.attached = true; // attached by some other window
        let cmds = m.update(UiEvent::SessionList(vec![info("mine-detached"), elsewhere]));
        assert!(cmds.contains(&Cmd::Attach("mine-detached".into())));
        assert!(
            !cmds.contains(&Cmd::Attach("foreign".into())),
            "must not attach a session owned elsewhere"
        );
        assert_eq!(m.locality_of("foreign"), Some(Locality::Elsewhere));
        assert_eq!(m.tile_count(), 2); // still shown, as a placeholder tile
    }

    #[test]
    fn into_single_keeps_the_owned_session_even_when_focus_moved() {
        // The window owns "alpha"; the fleet also previews a foreign "beta".
        let mine = HashSet::from(["alpha".to_string()]);
        let primary = TerminalModel::new("alpha".to_string(), 80, 24, METRICS);
        let (mut f, _) = FleetModel::adopting(primary, METRICS, SIZE, 1.0, mine);
        f.update(UiEvent::SessionList(vec![info("alpha"), info("beta")]));
        // Move focus onto the foreign tile.
        f.update(UiEvent::Key {
            key: Key::Named(NamedKey::ArrowRight),
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
