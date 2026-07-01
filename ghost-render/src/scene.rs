//! The drawable scene: a z-ordered tree of positioned primitives that
//! generalizes the terminal-only [`Frame`](crate::Frame) to arbitrary chrome
//! (tabs, sidebar, the fleet grid, overlays). A `Terminal` item embeds a
//! `Frame` unchanged, so a plain single-window terminal is just one layer with
//! one `Terminal` — today's render path as a trivial special case.
//!
//! Every item carries a stable [`SceneId`] and its pixel [`RectPx`], so the
//! scene doubles as the hit-test table: pointer routing asks the same structure
//! that was drawn ([`Scene::hit`]), keeping rendering and input in lockstep and
//! fully testable without a display.

use std::rc::Rc;

use crate::{CellMetrics, Frame, Run, Selection};

/// Straight-alpha RGBA, matching the renderer's instance color format.
pub type Rgba = [f32; 4];

/// A pixel-space rectangle (origin top-left).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RectPx {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl RectPx {
    /// Half-open containment: the left/top edges are inside, right/bottom out.
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }
}

/// Stable identity of a scene item — what pointer routing and tests refer to.
/// Tiles use an opaque handle (NOT a list index), so reordering never retargets
/// input. The producer (the UI core) maps its widgets/sessions onto these.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SceneId {
    /// The single full-window terminal (non-fleet mode). A session slide composites
    /// two of these (the outgoing and incoming sessions); they share this id and are
    /// told apart by their [`SceneItem::Terminal`] `session`, which is what the
    /// renderer keys each side's cached texture by.
    Root,
    /// A fleet tile, by stable handle.
    Tile(u64),
    /// A tile's title label.
    Label(u64),
    /// A tile's activity/bell badge.
    Badge(u64),
    Sidebar,
    NavBar,
    /// An attach-state section header, keyed by locality rank.
    Section(u8),
}

/// A per-tile indicator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BadgeKind {
    Bell,
    Activity,
}

/// A stable per-process key for a session id. The renderer caches a session's
/// rendered texture under this key so the texture follows the *session* across
/// every scene role and rebuild it appears in — a fleet tile (whose
/// [`SceneId::Tile`] handle is reassigned on every rebuild), a slide side, the
/// single-view root — rather than being tied to the ephemeral [`SceneId`]. It
/// keys a live, in-memory cache and is never persisted, so it need only be
/// deterministic within one run; a plain `DefaultHasher` suffices.
pub fn session_key(id: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut h);
    h.finish()
}

/// One drawable primitive. Every variant carries `id` and `rect`.
/// Which rows of a terminal's Surface changed since it was last composited — the
/// renderer's cue for how much to re-raster. The model derives it from the core's
/// per-feed dirty-row hint (`ghost_vt`'s `Screen::feed`), never from a frame diff, so
/// the [`Frame`] is a pure render input.
///
/// Its [`PartialEq`] is deliberately CONSTANT (`true`): damage is per-present metadata
/// that changes even when the rendered content does not, so it must NOT perturb `Scene`
/// equality. The idle-skip (`SceneCache` in `ghost-renderer`) compares scenes to skip
/// identical repaints; a differing damage tag on otherwise-identical content must still
/// compare equal and skip.
#[derive(Clone, Copy, Debug)]
pub enum TermDamage {
    /// Re-render the whole Surface: a first frame, a scroll, a resize/zoom, a selection
    /// change, or any change the model couldn't localize to a contiguous row range.
    All,
    /// Only rows `lo..=hi` changed since the last composite.
    Rows { lo: usize, hi: usize },
    /// Nothing changed since the last composite — the Surface is already current.
    None,
}

impl PartialEq for TermDamage {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum SceneItem {
    /// A solid (optionally rounded) fill.
    Rect {
        id: SceneId,
        rect: RectPx,
        color: Rgba,
        radius: f32,
    },
    /// A run of styled text; `rect` is the text box (origin top-left).
    Text {
        id: SceneId,
        rect: RectPx,
        runs: Vec<Run>,
        metrics: CellMetrics,
        color: Rgba,
    },
    /// An embedded terminal viewport, drawn from a [`Frame`] offset to `rect`
    /// and clipped to it. `dim` darkens an unfocused tile. `session` is the
    /// stable [`session_key`] of the session shown here — the renderer keys its
    /// rendered texture by it, so the texture follows the session across roles
    /// and rebuilds (see [`session_key`]); independent of `id`, which is the
    /// item's z/role identity for hit-testing.
    Terminal {
        id: SceneId,
        session: u64,
        rect: RectPx,
        /// Shared so cloning a scene (each animation tick, and the damage cache's
        /// snapshot) is a refcount bump, not a deep copy of the laid-out rows — the
        /// frozen content of an animation must not be re-copied every frame. Pointer
        /// identity also serves as a cheap "unchanged since last render" check.
        frame: Rc<Frame>,
        selection: Option<Selection>,
        dim: bool,
        /// Which rows changed since this session was last composited — the renderer's
        /// cue for how much of its Surface to re-raster (see [`TermDamage`]).
        damage: TermDamage,
    },
    /// A rectangular outline (e.g. the focused-tile border).
    Border {
        id: SceneId,
        rect: RectPx,
        color: Rgba,
        width: f32,
    },
    /// A bell/activity indicator.
    Badge {
        id: SceneId,
        rect: RectPx,
        kind: BadgeKind,
    },
}

impl SceneItem {
    pub fn id(&self) -> SceneId {
        match self {
            SceneItem::Rect { id, .. }
            | SceneItem::Text { id, .. }
            | SceneItem::Terminal { id, .. }
            | SceneItem::Border { id, .. }
            | SceneItem::Badge { id, .. } => *id,
        }
    }

    pub fn rect(&self) -> RectPx {
        match self {
            SceneItem::Rect { rect, .. }
            | SceneItem::Text { rect, .. }
            | SceneItem::Terminal { rect, .. }
            | SceneItem::Border { rect, .. }
            | SceneItem::Badge { rect, .. } => *rect,
        }
    }
}

/// A 2-D camera: a uniform scale about the layer origin, then a translation (in
/// pixels). This is all the spatial-navigation zoom needs — no rotation or shear —
/// so it stays a cheap, exactly-invertible mapping (the inverse routes a pointer
/// back through a transformed layer for hit-testing).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transform {
    pub scale: f32,
    pub tx: f32,
    pub ty: f32,
}

impl Default for Transform {
    fn default() -> Self {
        Transform::IDENTITY
    }
}

impl Transform {
    /// The no-op transform: scale 1, no translation.
    pub const IDENTITY: Transform = Transform {
        scale: 1.0,
        tx: 0.0,
        ty: 0.0,
    };

    /// Map a point from layer space to screen space.
    pub fn apply(&self, x: f32, y: f32) -> (f32, f32) {
        (x * self.scale + self.tx, y * self.scale + self.ty)
    }

    /// Map a rect from layer space to screen space.
    pub fn apply_rect(&self, r: RectPx) -> RectPx {
        RectPx {
            x: r.x * self.scale + self.tx,
            y: r.y * self.scale + self.ty,
            w: r.w * self.scale,
            h: r.h * self.scale,
        }
    }

    /// Map a screen-space point back to layer space — the inverse of [`apply`],
    /// used to route a pointer through a transformed layer. A degenerate (zero)
    /// scale isn't hit-tested in practice; guard against it rather than divide by 0.
    pub fn invert(&self, x: f32, y: f32) -> (f32, f32) {
        if self.scale == 0.0 {
            return (x, y);
        }
        ((x - self.tx) / self.scale, (y - self.ty) / self.scale)
    }

    /// The transform that frames `rect` to fill `viewport` (w, h): a uniform
    /// "cover" scale — the rect's tighter-fitting dimension fills exactly, the
    /// other overflows — centred on the viewport. This is the camera that zooms a
    /// fleet tile up until it fills the window (the spatial-nav dive); lerped from
    /// [`IDENTITY`](Self::IDENTITY) it animates the zoom, and reversed it zooms out.
    pub fn zoom_to(rect: RectPx, viewport: (f32, f32)) -> Transform {
        let (vw, vh) = viewport;
        if rect.w <= 0.0 || rect.h <= 0.0 {
            return Transform::IDENTITY;
        }
        let scale = (vw / rect.w).max(vh / rect.h);
        let cx = rect.x + rect.w * 0.5;
        let cy = rect.y + rect.h * 0.5;
        Transform {
            scale,
            tx: vw * 0.5 - cx * scale,
            ty: vh * 0.5 - cy * scale,
        }
    }

    /// The transform that places `content` (a w×h size, drawn from the origin)
    /// inside `target`: a uniform "contain" scale (fits the tighter dimension, no
    /// overflow) centred in the target. This is the camera that shrinks the
    /// full-window single view down to a fleet tile's spot; lerped to
    /// [`IDENTITY`](Self::IDENTITY) it grows the view up into the full terminal.
    pub fn place_in(content: (f32, f32), target: RectPx) -> Transform {
        let (cw, ch) = content;
        if cw <= 0.0 || ch <= 0.0 {
            return Transform::IDENTITY;
        }
        let scale = (target.w / cw).min(target.h / ch);
        Transform {
            scale,
            tx: target.x + (target.w - cw * scale) * 0.5,
            ty: target.y + (target.h - ch * scale) * 0.5,
        }
    }

    /// The transform that maps `from` onto `to` (uniform scale + translate),
    /// top-left anchored. `from` and `to` are assumed to share an aspect ratio, so
    /// the width ratio sets the uniform scale; the corners then line up exactly. This
    /// frames a fleet tile's drawn frame onto the single view's rect, so the dive's
    /// full-zoom endpoint matches the single view pixel-for-pixel (no cover-stretch).
    pub fn map_rect(from: RectPx, to: RectPx) -> Transform {
        if from.w <= 0.0 || from.h <= 0.0 {
            return Transform::IDENTITY;
        }
        let scale = to.w / from.w;
        Transform {
            scale,
            tx: to.x - from.x * scale,
            ty: to.y - from.y * scale,
        }
    }

    /// Linear interpolation between two transforms at `t` (0 = `a`, 1 = `b`). The
    /// animation clock drives `t`; easing, if any, is applied to `t` by the caller.
    pub fn lerp(a: Transform, b: Transform, t: f32) -> Transform {
        let l = |x: f32, y: f32| x + (y - x) * t;
        Transform {
            scale: l(a.scale, b.scale),
            tx: l(a.tx, b.tx),
            ty: l(a.ty, b.ty),
        }
    }
}

/// A z-ordered group of items, optionally transformed and faded as a unit. Layers
/// draw low `z` first. `transform` is a camera applied to every item's rect before
/// rasterization (and inverted for hit-testing); `opacity` multiplies every item's
/// alpha. Identity + fully opaque by default, so a plain layer draws exactly as its
/// items specify.
#[derive(Clone, Debug, PartialEq)]
pub struct Layer {
    pub z: i32,
    pub items: Vec<SceneItem>,
    pub transform: Transform,
    pub opacity: f32,
}

impl Default for Layer {
    fn default() -> Self {
        Layer {
            z: 0,
            items: Vec::new(),
            transform: Transform::IDENTITY,
            opacity: 1.0,
        }
    }
}

impl Layer {
    /// A layer at depth `z` holding `items`, untransformed and fully opaque.
    pub fn new(z: i32, items: Vec<SceneItem>) -> Self {
        Layer {
            z,
            items,
            ..Default::default()
        }
    }

    /// Builder: set the layer's camera transform.
    pub fn with_transform(mut self, transform: Transform) -> Self {
        self.transform = transform;
        self
    }

    /// Builder: set the layer's opacity (0 = transparent, 1 = opaque).
    pub fn with_opacity(mut self, opacity: f32) -> Self {
        self.opacity = opacity;
        self
    }
}

/// A full frame to draw: the window size plus its layers, low z to high.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Scene {
    pub size_px: (u32, u32),
    pub layers: Vec<Layer>,
}

impl Scene {
    pub fn new(size_px: (u32, u32)) -> Self {
        Scene {
            size_px,
            layers: Vec::new(),
        }
    }

    /// The id of the topmost item whose rect contains `(x, y)` — highest `z`,
    /// then latest inserted. `None` if nothing is hit. This is the canonical
    /// pointer hit-test: routing and rendering share one structure.
    pub fn hit(&self, x: f32, y: f32) -> Option<SceneId> {
        let mut best: Option<((i32, usize), SceneId)> = None;
        let mut seq = 0usize;
        for layer in &self.layers {
            // Items live in layer space; route the pointer back through the layer's
            // camera so hit-testing follows what was actually drawn on screen.
            let (lx, ly) = layer.transform.invert(x, y);
            for item in &layer.items {
                if item.rect().contains(lx, ly) {
                    let key = (layer.z, seq);
                    if best.as_ref().is_none_or(|(bk, _)| key > *bk) {
                        best = Some((key, item.id()));
                    }
                }
                seq += 1;
            }
        }
        best.map(|(_, id)| id)
    }

    /// Every embedded terminal viewport, in draw order.
    pub fn terminals(&self) -> impl Iterator<Item = &SceneItem> {
        self.layers
            .iter()
            .flat_map(|l| &l.items)
            .filter(|it| matches!(it, SceneItem::Terminal { .. }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(id: SceneId, x: f32, y: f32, w: f32, h: f32) -> SceneItem {
        SceneItem::Terminal {
            id,
            session: 0,
            rect: RectPx { x, y, w, h },
            frame: Rc::new(Frame {
                cols: 0,
                rows: 0,
                metrics: CellMetrics {
                    advance: 1.0,
                    line_height: 1.0,
                },
                rows_layout: vec![],
                cursor: None,
                images: vec![],
                default_fg: None,
                default_bg: None,
                cursor_color: None,
            }),
            selection: None,
            dim: false,
            damage: TermDamage::All,
        }
    }

    #[test]
    fn rect_contains_is_half_open() {
        let r = RectPx {
            x: 10.0,
            y: 20.0,
            w: 5.0,
            h: 5.0,
        };
        assert!(r.contains(10.0, 20.0)); // top-left corner is inside
        assert!(r.contains(14.9, 24.9));
        assert!(!r.contains(15.0, 20.0)); // right edge is exclusive
        assert!(!r.contains(10.0, 25.0)); // bottom edge is exclusive
        assert!(!r.contains(9.9, 20.0));
    }

    #[test]
    fn zoom_to_frames_a_rect_to_fill_the_viewport() {
        // A rect with the viewport's aspect fills it exactly, centred.
        let r = RectPx {
            x: 10.0,
            y: 10.0,
            w: 40.0,
            h: 20.0,
        };
        let f = Transform::zoom_to(r, (200.0, 100.0)).apply_rect(r);
        assert!(f.x.abs() < 1e-3 && f.y.abs() < 1e-3, "{f:?}");
        assert!(
            (f.w - 200.0).abs() < 1e-3 && (f.h - 100.0).abs() < 1e-3,
            "{f:?}"
        );

        // A wide rect "covers": its shorter-relative side fills exactly, the other
        // overflows, and it stays centred on the viewport.
        let wide = RectPx {
            x: 0.0,
            y: 0.0,
            w: 40.0,
            h: 10.0,
        };
        let f = Transform::zoom_to(wide, (100.0, 100.0)).apply_rect(wide);
        assert!((f.h - 100.0).abs() < 1e-3, "height fills: {f:?}");
        assert!(f.w > 100.0, "width overflows (cover): {f:?}");
        assert!((f.x + f.w * 0.5 - 50.0).abs() < 1e-3, "centred x: {f:?}");
        assert!((f.y + f.h * 0.5 - 50.0).abs() < 1e-3, "centred y: {f:?}");
    }

    #[test]
    fn map_rect_lands_from_exactly_onto_to_top_left_anchored() {
        let from = RectPx {
            x: 30.0,
            y: 12.0,
            w: 80.0,
            h: 50.0,
        };
        // A same-aspect target the frame should land on exactly (corners coincide).
        let to = RectPx {
            x: 0.0,
            y: 0.0,
            w: 160.0,
            h: 100.0,
        };
        let f = Transform::map_rect(from, to).apply_rect(from);
        assert!(
            (f.x - to.x).abs() < 1e-3
                && (f.y - to.y).abs() < 1e-3
                && (f.w - to.w).abs() < 1e-3
                && (f.h - to.h).abs() < 1e-3,
            "from must map exactly onto to: {f:?}"
        );
        // Degenerate input is the identity (no panic / NaN).
        let zero = RectPx {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 0.0,
        };
        assert_eq!(Transform::map_rect(zero, to), Transform::IDENTITY);
    }

    #[test]
    fn place_in_contains_content_centered_in_a_target() {
        // A 200x100 single view placed into a 40x40 tile: "contain" scale (fits the
        // tighter dimension), centred — the start of the grow-into-terminal zoom.
        let t = Transform::place_in(
            (200.0, 100.0),
            RectPx {
                x: 60.0,
                y: 20.0,
                w: 40.0,
                h: 40.0,
            },
        );
        let f = t.apply_rect(RectPx {
            x: 0.0,
            y: 0.0,
            w: 200.0,
            h: 100.0,
        });
        // Contain on the 200x100 content into 40x40 → scale 0.2 → 40x20, centred.
        assert!(
            (f.w - 40.0).abs() < 1e-3 && (f.h - 20.0).abs() < 1e-3,
            "{f:?}"
        );
        assert!(
            f.x >= 60.0 - 1e-3 && f.x + f.w <= 100.0 + 1e-3,
            "within target x: {f:?}"
        );
        assert!(
            (f.x + f.w * 0.5 - 80.0).abs() < 1e-3,
            "centred x in target: {f:?}"
        );
        assert!(
            (f.y + f.h * 0.5 - 40.0).abs() < 1e-3,
            "centred y in target: {f:?}"
        );
    }

    #[test]
    fn lerp_interpolates_endpoints() {
        let a = Transform::IDENTITY;
        let b = Transform {
            scale: 3.0,
            tx: 10.0,
            ty: -20.0,
        };
        assert_eq!(Transform::lerp(a, b, 0.0), a);
        assert_eq!(Transform::lerp(a, b, 1.0), b);
        let mid = Transform::lerp(a, b, 0.5);
        assert!((mid.scale - 2.0).abs() < 1e-3); // 1 -> 3
        assert!((mid.tx - 5.0).abs() < 1e-3);
        assert!((mid.ty + 10.0).abs() < 1e-3);
    }

    #[test]
    fn hit_returns_topmost_by_z_then_order() {
        let mut scene = Scene::new((100, 100));
        // Two overlapping tiles on different layers; the higher-z one wins.
        scene.layers.push(Layer::new(
            0,
            vec![term(SceneId::Tile(1), 0.0, 0.0, 50.0, 50.0)],
        ));
        scene.layers.push(Layer::new(
            10,
            vec![term(SceneId::Tile(2), 0.0, 0.0, 50.0, 50.0)],
        ));
        assert_eq!(scene.hit(10.0, 10.0), Some(SceneId::Tile(2)));

        // Move the top tile away; the point now only the lower tile covers hits it.
        scene.layers[1].items[0] = term(SceneId::Tile(2), 60.0, 60.0, 30.0, 30.0);
        assert_eq!(scene.hit(10.0, 10.0), Some(SceneId::Tile(1)));

        // Empty space hits nothing.
        assert_eq!(scene.hit(99.0, 5.0), None);
    }

    #[test]
    fn hit_inverts_a_layer_transform() {
        let mut scene = Scene::new((200, 200));
        // A tile at layer-space (0,0,50,50) in a layer scaled 2x and shifted by
        // (10,10): on screen it covers (10,10)..(110,110).
        scene.layers.push(
            Layer::new(0, vec![term(SceneId::Tile(1), 0.0, 0.0, 50.0, 50.0)]).with_transform(
                Transform {
                    scale: 2.0,
                    tx: 10.0,
                    ty: 10.0,
                },
            ),
        );
        // A screen point inside the *transformed* rect hits the tile.
        assert_eq!(scene.hit(60.0, 60.0), Some(SceneId::Tile(1)));
        assert_eq!(scene.hit(10.0, 10.0), Some(SceneId::Tile(1))); // top-left corner
        // A point inside the *untransformed* rect but outside the transformed one
        // must miss — routing has to follow what was actually drawn.
        assert_eq!(scene.hit(5.0, 5.0), None);
        assert_eq!(scene.hit(120.0, 120.0), None);
    }

    #[test]
    fn terminals_lists_only_terminal_items() {
        let mut scene = Scene::new((100, 100));
        scene.layers.push(Layer::new(
            0,
            vec![
                SceneItem::Rect {
                    id: SceneId::Sidebar,
                    rect: RectPx {
                        x: 0.0,
                        y: 0.0,
                        w: 10.0,
                        h: 10.0,
                    },
                    color: [0.0; 4],
                    radius: 0.0,
                },
                term(SceneId::Tile(1), 0.0, 0.0, 5.0, 5.0),
                term(SceneId::Tile(2), 5.0, 5.0, 5.0, 5.0),
            ],
        ));
        assert_eq!(scene.terminals().count(), 2);
    }
}
