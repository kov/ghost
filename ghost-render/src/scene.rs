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
    /// The single full-window terminal (non-fleet mode).
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

/// One drawable primitive. Every variant carries `id` and `rect`.
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
    /// and clipped to it. `dim` darkens an unfocused tile.
    Terminal {
        id: SceneId,
        rect: RectPx,
        frame: Frame,
        selection: Option<Selection>,
        dim: bool,
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

/// A z-ordered group of items. Layers draw low `z` first.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Layer {
    pub z: i32,
    pub items: Vec<SceneItem>,
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
            for item in &layer.items {
                if item.rect().contains(x, y) {
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
            rect: RectPx { x, y, w, h },
            frame: Frame {
                cols: 0,
                rows: 0,
                metrics: CellMetrics {
                    advance: 1.0,
                    line_height: 1.0,
                },
                rows_layout: vec![],
                cursor: None,
                images: vec![],
            },
            selection: None,
            dim: false,
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
    fn hit_returns_topmost_by_z_then_order() {
        let mut scene = Scene::new((100, 100));
        // Two overlapping tiles on different layers; the higher-z one wins.
        scene.layers.push(Layer {
            z: 0,
            items: vec![term(SceneId::Tile(1), 0.0, 0.0, 50.0, 50.0)],
        });
        scene.layers.push(Layer {
            z: 10,
            items: vec![term(SceneId::Tile(2), 0.0, 0.0, 50.0, 50.0)],
        });
        assert_eq!(scene.hit(10.0, 10.0), Some(SceneId::Tile(2)));

        // Move the top tile away; the point now only the lower tile covers hits it.
        scene.layers[1].items[0] = term(SceneId::Tile(2), 60.0, 60.0, 30.0, 30.0);
        assert_eq!(scene.hit(10.0, 10.0), Some(SceneId::Tile(1)));

        // Empty space hits nothing.
        assert_eq!(scene.hit(99.0, 5.0), None);
    }

    #[test]
    fn terminals_lists_only_terminal_items() {
        let mut scene = Scene::new((100, 100));
        scene.layers.push(Layer {
            z: 0,
            items: vec![
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
        });
        assert_eq!(scene.terminals().count(), 2);
    }
}
