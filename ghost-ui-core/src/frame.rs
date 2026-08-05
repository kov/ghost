//! The window frame ghost draws itself: which part of it the pointer is over.
//!
//! With `[window] decorations = "ghost"` there is no CSD frame above us doing
//! this, so the resize edges are ours to find. The compositor still performs the
//! resize — the shell only says "the user grabbed this edge" — but *which* edge,
//! and whether there is one to grab at all, is decided here so it is settled in
//! tests rather than only in front of a compositor.
//!
//! Until the frame gains margins of its own (the drop-shadow work), the grab
//! band lies just *inside* the window rather than in a margin outside it, which
//! is why it is narrow: it overlaps the window's own inner padding and should
//! not eat into the terminal grid behind it.

use crate::PointPx;
use ghost_render::scene::{Layer, RectPx, Rgba, Scene, SceneId, SceneItem, TextAlign};

/// How tall the titlebar we draw is, in logical pixels. sctk-adwaita's
/// `HEADER_SIZE`, so a window whose frame we took over is the same height as
/// one whose frame we didn't, and as every other window on the desktop.
pub const BAR_HEIGHT: f32 = 35.0;

/// The titlebar's height in physical pixels — 0 when the desktop draws the
/// frame, in which case there is no bar of ours and no inset to make room for
/// it. Whole pixels: the same number insets the model, composes the scene and
/// offsets the pointer, and they must agree exactly.
pub fn bar_height_px(own_frame: bool, scale: f32) -> u32 {
    if !own_frame {
        return 0;
    }
    (BAR_HEIGHT * scale.max(0.0)).round() as u32
}

/// A window-control button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowButton {
    Minimize,
    Maximize,
    Close,
}

impl WindowButton {
    /// The glyph drawn in the button. Symbolic characters rather than an icon
    /// set: the chrome font (with fallback) already covers them, and they are
    /// the shapes every desktop uses for these three.
    fn glyph(self, maximized: bool) -> &'static str {
        match self {
            WindowButton::Minimize => "\u{2212}",              // MINUS SIGN
            WindowButton::Maximize if maximized => "\u{2750}", // UPPER RIGHT DROP-SHADOWED SQUARE
            WindowButton::Maximize => "\u{25A1}",              // WHITE SQUARE
            WindowButton::Close => "\u{2715}",                 // MULTIPLICATION X
        }
    }

    fn name(spec: &str) -> Option<Self> {
        match spec {
            "minimize" => Some(WindowButton::Minimize),
            "maximize" => Some(WindowButton::Maximize),
            "close" => Some(WindowButton::Close),
            // `appmenu`, `menu`, `icon`, `spacer`: nothing we draw.
            _ => None,
        }
    }
}

/// Which buttons sit on which side of the titlebar. The desktop decides both
/// the side and the order, and getting either wrong is the classic sign of a
/// window that drew its own frame without asking.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ButtonLayout {
    pub left: Vec<WindowButton>,
    pub right: Vec<WindowButton>,
}

impl ButtonLayout {
    /// Parse GNOME's `button-layout`: the two sides separated by a colon, each a
    /// comma-separated list — `appmenu:minimize,maximize,close`. Entries that
    /// name nothing we draw are dropped. A spec with no colon at all is all
    /// left, as GNOME reads it.
    pub fn parse(spec: &str) -> Self {
        let spec = spec.trim().trim_matches('\'');
        let (left, right) = spec.split_once(':').unwrap_or((spec, ""));
        let side = |s: &str| {
            s.split(',')
                .map(str::trim)
                .filter_map(WindowButton::name)
                .collect()
        };
        ButtonLayout {
            left: side(left),
            right: side(right),
        }
    }

    /// No buttons at all — a desktop that asked for none, or named only things
    /// we do not draw.
    pub fn is_empty(&self) -> bool {
        self.left.is_empty() && self.right.is_empty()
    }
}

/// Diameter of a window button, in logical pixels — libadwaita's.
pub const BUTTON_SIZE: f32 = 24.0;
/// Gap between adjacent buttons, and between the outermost one and the window
/// edge, in logical pixels.
pub const BUTTON_GAP: f32 = 6.0;

/// Where each button goes in a `bar` strip at `scale`, left group then right.
/// The right group is laid out from the window's right edge inwards, so the
/// order reads left-to-right on screen either way.
pub fn button_rects(layout: &ButtonLayout, bar: RectPx, scale: f32) -> Vec<(WindowButton, RectPx)> {
    let size = BUTTON_SIZE * scale;
    let gap = BUTTON_GAP * scale;
    let y = bar.y + (bar.h - size) * 0.5;
    let mut out = Vec::new();
    for (i, b) in layout.left.iter().enumerate() {
        out.push((
            *b,
            RectPx {
                x: bar.x + gap + i as f32 * (size + gap),
                y,
                w: size,
                h: size,
            },
        ));
    }
    let right_edge = bar.x + bar.w - gap;
    let n = layout.right.len() as f32;
    for (i, b) in layout.right.iter().enumerate() {
        out.push((
            *b,
            RectPx {
                x: right_edge - (n - i as f32) * size - (n - 1.0 - i as f32) * gap,
                y,
                w: size,
                h: size,
            },
        ));
    }
    out
}

/// The button under `pos`, if any.
pub fn button_at(
    pos: PointPx,
    layout: &ButtonLayout,
    bar: RectPx,
    scale: f32,
) -> Option<WindowButton> {
    button_rects(layout, bar, scale)
        .into_iter()
        .find(|(_, r)| r.contains(pos.x as f32, pos.y as f32))
        .map(|(b, _)| b)
}

/// Everything the titlebar draws: its height and colours, and the title.
#[derive(Clone, Debug, PartialEq)]
pub struct Titlebar {
    /// Height in physical px — [`bar_height_px`]. 0 draws no bar at all.
    pub height_px: u32,
    pub bg: Rgba,
    /// The title's colour. The frame signals focus by dimming it.
    pub fg: Rgba,
    pub title: String,
    /// Em size of the title, in physical px.
    pub font_px: f32,
    /// Which buttons, on which side — the desktop's choice.
    pub buttons: ButtonLayout,
    /// The button under the pointer, drawn with its hover circle.
    pub hovered: Option<WindowButton>,
    /// The button being pressed, drawn deeper still.
    pub pressed: Option<WindowButton>,
    /// Whether the window is maximized, which changes the maximize glyph.
    pub maximized: bool,
    /// Scale, for sizing the buttons in physical px.
    pub scale: f32,
}

/// The layer depth the titlebar draws at. Above everything the model builds:
/// the bar is the window's own frame, not content, and no overlay of ours
/// covers a titlebar any more than a dialog covers the desktop's.
const BAR_Z: i32 = i32::MAX;

/// Lay `content` — a scene the model built for the space *under* the bar — into
/// a window `bar_px` taller, with the titlebar in the strip it makes.
///
/// Everything the model drew moves down by the bar's height. It laid out in a
/// window that size, so nothing needs re-laying: the shell sizes the model to
/// the content area, and this puts that area where it belongs.
pub fn with_titlebar(content: Scene, bar: &Titlebar) -> Scene {
    if bar.height_px == 0 {
        return content;
    }
    let bar_px = bar.height_px;
    let (w, h) = content.size_px;
    let mut scene = Scene::new((w, h + bar_px));
    scene.layers = content.layers;
    for layer in &mut scene.layers {
        layer.transform.ty += bar_px as f32;
    }
    let strip = RectPx {
        x: 0.0,
        y: 0.0,
        w: w as f32,
        h: bar_px as f32,
    };
    let mut items = vec![SceneItem::Rect {
        id: SceneId::Titlebar,
        rect: strip,
        color: bar.bg,
        radius: 0.0,
    }];
    let buttons = button_rects(&bar.buttons, strip, bar.scale);
    if !bar.title.is_empty() {
        // Centred between the button groups rather than on the window, so a
        // title long enough to reach them stays clear of them — and a window
        // with buttons on one side only still reads as centred in what is left.
        let left = buttons
            .iter()
            .filter(|(b, _)| bar.buttons.left.contains(b))
            .map(|(_, r)| r.x + r.w)
            .fold(strip.x, f32::max);
        let right = buttons
            .iter()
            .filter(|(b, _)| bar.buttons.right.contains(b))
            .map(|(_, r)| r.x)
            .fold(strip.x + strip.w, f32::min);
        items.push(SceneItem::ChromeText {
            id: SceneId::Titlebar,
            rect: RectPx {
                x: left,
                y: strip.y,
                w: (right - left).max(0.0),
                h: strip.h,
            },
            text: bar.title.clone(),
            color: bar.fg,
            size_px: bar.font_px,
            align: TextAlign::Center,
        });
    }
    for (button, rect) in buttons {
        // The circle only appears under the pointer, as libadwaita's do: three
        // permanent discs in the corner of a terminal would be loud.
        let shade = if bar.pressed == Some(button) {
            Some(0.16)
        } else if bar.hovered == Some(button) {
            Some(0.09)
        } else {
            None
        };
        if let Some(alpha) = shade {
            items.push(SceneItem::Rect {
                id: SceneId::WindowButton,
                rect,
                color: [bar.fg[0], bar.fg[1], bar.fg[2], alpha],
                radius: rect.h * 0.5,
            });
        }
        items.push(SceneItem::ChromeText {
            id: SceneId::WindowButton,
            rect,
            text: button.glyph(bar.maximized).into(),
            color: bar.fg,
            size_px: bar.font_px,
            align: TextAlign::Center,
        });
    }
    scene.layers.push(Layer::new(BAR_Z, items));
    scene
}

/// Which edge or corner of the window is under the pointer. The compositor's
/// eight-way resize; the shell maps this onto its toolkit's own enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResizeEdge {
    North,
    South,
    East,
    West,
    NorthEast,
    NorthWest,
    SouthEast,
    SouthWest,
}

/// How deep the resize band reaches into the window, in logical pixels.
pub const RESIZE_BAND: f32 = 6.0;

/// How far a corner's grab reaches along each edge, in logical pixels — a
/// corner is easier to hit than a hairline of edge, and grabbing one resizes
/// both axes at once. sctk-adwaita's `RESIZE_HANDLE_CORNER_SIZE`, so ours is as
/// forgiving as the frame we are replacing.
pub const RESIZE_CORNER: f32 = 24.0;

/// What the window can offer the pointer right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameGrab {
    /// ghost draws the decorations. Without this the desktop's frame owns the
    /// edges and reaching for them ourselves would fight it.
    pub own_frame: bool,
    /// Maximized, fullscreen or tiled: the edges meet the screen or a
    /// neighbour, so there is nothing to drag them to.
    pub boxed_in: bool,
    /// A button is already held. Whatever the press started — selecting text,
    /// dragging a tile — owns the pointer until it is let go, so a drag that
    /// wanders into the band must not turn into a resize under it.
    pub pointer_down: bool,
}

/// The edge `pos` grabs on a `size` window at `scale`, or `None` when the
/// pointer is over the window's content (or there is no edge to grab).
///
/// Corners win over edges, and reach further along each edge than the band is
/// deep — the point of a corner is that it is easy to hit.
pub fn resize_edge_at(
    pos: PointPx,
    size: (u32, u32),
    scale: f32,
    grab: FrameGrab,
) -> Option<ResizeEdge> {
    if !grab.own_frame || grab.boxed_in || grab.pointer_down {
        return None;
    }
    let (w, h) = (size.0 as f64, size.1 as f64);
    let (x, y) = (pos.x, pos.y);
    if x < 0.0 || y < 0.0 || x >= w || y >= h {
        return None;
    }
    let band = f64::from(RESIZE_BAND * scale.max(0.0));
    let corner = f64::from(RESIZE_CORNER * scale.max(0.0));

    let (west, east) = (x < band, x >= w - band);
    let (north, south) = (y < band, y >= h - band);
    // The corner's reach along the *other* axis: a pointer in the top band and
    // within a corner's reach of the left edge is grabbing the top-left corner,
    // and so is one in the left band near the top.
    let (c_west, c_east) = (x < corner, x >= w - corner);
    let (c_north, c_south) = (y < corner, y >= h - corner);

    let edge = if (north && c_west) || (west && c_north) {
        ResizeEdge::NorthWest
    } else if (north && c_east) || (east && c_north) {
        ResizeEdge::NorthEast
    } else if (south && c_west) || (west && c_south) {
        ResizeEdge::SouthWest
    } else if (south && c_east) || (east && c_south) {
        ResizeEdge::SouthEast
    } else if north {
        ResizeEdge::North
    } else if south {
        ResizeEdge::South
    } else if west {
        ResizeEdge::West
    } else if east {
        ResizeEdge::East
    } else {
        return None;
    };
    Some(edge)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: (u32, u32) = (800, 600);

    fn grab() -> FrameGrab {
        FrameGrab {
            own_frame: true,
            boxed_in: false,
            pointer_down: false,
        }
    }

    fn at(x: f64, y: f64) -> Option<ResizeEdge> {
        resize_edge_at(PointPx { x, y }, SIZE, 1.0, grab())
    }

    fn titlebar(height_px: u32) -> Titlebar {
        Titlebar {
            height_px,
            bg: [0.1, 0.1, 0.1, 1.0],
            fg: [1.0, 1.0, 1.0, 1.0],
            title: "ghost".into(),
            font_px: 15.0,
            buttons: ButtonLayout::default(),
            hovered: None,
            pressed: None,
            maximized: false,
            scale: 1.0,
        }
    }

    const BAR: RectPx = RectPx {
        x: 0.0,
        y: 0.0,
        w: 800.0,
        h: 35.0,
    };

    #[test]
    fn the_desktop_decides_which_buttons_and_on_which_side() {
        // Both halves of `button-layout` matter: a window with its close button
        // on the wrong side is the classic sign of an app that drew its own
        // frame without asking the desktop.
        let l = ButtonLayout::parse("'appmenu:minimize,maximize,close'");
        assert_eq!(l.left, vec![]);
        assert_eq!(
            l.right,
            vec![
                WindowButton::Minimize,
                WindowButton::Maximize,
                WindowButton::Close
            ]
        );

        // The other convention, and the order within a side is kept.
        let l = ButtonLayout::parse("close,minimize,maximize:");
        assert_eq!(
            l.left,
            vec![
                WindowButton::Close,
                WindowButton::Minimize,
                WindowButton::Maximize
            ]
        );
        assert!(l.right.is_empty());

        // Entries naming things we do not draw are simply not drawn.
        assert!(ButtonLayout::parse("appmenu:spacer").is_empty());
    }

    #[test]
    fn the_right_hand_buttons_are_laid_out_from_the_right_edge_in() {
        // Laid out in reading order but anchored to the right edge, so the
        // close button is the outermost one and the group does not float.
        let l = ButtonLayout::parse(":minimize,maximize,close");
        let rects = button_rects(&l, BAR, 1.0);
        assert_eq!(rects.len(), 3);
        let xs: Vec<f32> = rects.iter().map(|(_, r)| r.x).collect();
        assert!(xs[0] < xs[1] && xs[1] < xs[2], "reading order: {xs:?}");
        assert_eq!(rects[0].0, WindowButton::Minimize);
        assert_eq!(rects[2].0, WindowButton::Close);
        let last = rects[2].1;
        assert_eq!(
            last.x + last.w,
            BAR.w - BUTTON_GAP,
            "the outermost button sits a gap from the edge"
        );
        // Vertically centred in the bar.
        assert_eq!(last.y, (BAR.h - BUTTON_SIZE) * 0.5);
    }

    #[test]
    fn the_left_hand_buttons_start_at_the_left_edge() {
        let l = ButtonLayout::parse("close:");
        let rects = button_rects(&l, BAR, 1.0);
        assert_eq!(rects[0].1.x, BUTTON_GAP);
    }

    #[test]
    fn a_click_finds_the_button_under_it() {
        let l = ButtonLayout::parse(":minimize,close");
        let rects = button_rects(&l, BAR, 1.0);
        for (button, r) in &rects {
            let mid = PointPx {
                x: (r.x + r.w * 0.5) as f64,
                y: (r.y + r.h * 0.5) as f64,
            };
            assert_eq!(button_at(mid, &l, BAR, 1.0), Some(*button));
        }
        // The empty stretch of bar is not a button.
        assert_eq!(button_at(PointPx { x: 400.0, y: 17.0 }, &l, BAR, 1.0), None);
    }

    #[test]
    fn only_the_button_under_the_pointer_wears_a_circle() {
        // libadwaita's buttons are bare until hovered; three permanent discs in
        // the corner of a terminal would be loud.
        let discs = |hovered| {
            let bar = Titlebar {
                buttons: ButtonLayout::parse(":minimize,maximize,close"),
                hovered,
                ..titlebar(35)
            };
            with_titlebar(content(800, 565), &bar)
                .layers
                .iter()
                .flat_map(|l| &l.items)
                .filter(|i| {
                    matches!(i, SceneItem::Rect { id, radius, .. }
                        if *id == SceneId::WindowButton && *radius > 0.0)
                })
                .count()
        };
        assert_eq!(discs(None), 0, "no circles until the pointer is on one");
        assert_eq!(discs(Some(WindowButton::Close)), 1, "and then exactly one");
    }

    #[test]
    fn the_title_keeps_clear_of_the_buttons() {
        // Centred between the groups, not on the window: a long title that
        // reached the buttons would run under them.
        let bar = Titlebar {
            buttons: ButtonLayout::parse(":minimize,maximize,close"),
            ..titlebar(35)
        };
        let scene = with_titlebar(content(800, 565), &bar);
        let title = scene
            .layers
            .iter()
            .flat_map(|l| &l.items)
            .find(|i| matches!(i, SceneItem::ChromeText { id, .. } if *id == SceneId::Titlebar))
            .expect("the title is drawn");
        let leftmost_button = button_rects(&bar.buttons, BAR, 1.0)
            .iter()
            .map(|(_, r)| r.x)
            .fold(f32::MAX, f32::min);
        let r = title.rect();
        assert!(
            r.x + r.w <= leftmost_button,
            "the title's box stops before the buttons ({} vs {leftmost_button})",
            r.x + r.w
        );
    }

    /// A stand-in for whatever the model drew, one layer with one item in it.
    fn content(w: u32, h: u32) -> Scene {
        let mut s = Scene::new((w, h));
        s.layers.push(Layer::new(
            0,
            vec![SceneItem::Rect {
                id: SceneId::Root,
                rect: RectPx {
                    x: 0.0,
                    y: 0.0,
                    w: w as f32,
                    h: h as f32,
                },
                color: [0.0, 0.0, 0.0, 1.0],
                radius: 0.0,
            }],
        ));
        s
    }

    #[test]
    fn the_titlebar_takes_a_strip_and_the_content_moves_under_it() {
        // The model lays out in the space below the bar, so the window it fills
        // is that much taller than the scene it drew — and everything in it
        // moves down to make room. Anything still drawn at the top of the window
        // would be under the titlebar.
        let bar = 35;
        let scene = with_titlebar(content(800, 565), &titlebar(bar));
        assert_eq!(scene.size_px, (800, 600), "the window is the bar taller");

        let drawn = scene
            .layers
            .iter()
            .find(|l| l.items.iter().any(|i| i.id() == SceneId::Root))
            .expect("the content is still there");
        assert_eq!(
            drawn.transform.ty, bar as f32,
            "the content sits below the bar"
        );

        let titlebar = scene
            .layers
            .iter()
            .flat_map(|l| &l.items)
            .find(|i| i.id() == SceneId::Titlebar)
            .expect("the bar is drawn");
        let r = titlebar.rect();
        assert_eq!((r.x, r.y, r.w, r.h), (0.0, 0.0, 800.0, 35.0));
    }

    #[test]
    fn the_titlebar_draws_over_everything_the_model_built() {
        // The bar is the window's frame, not content: no overlay of ours covers
        // it, any more than a dialog covers the desktop.
        let scene = with_titlebar(content(800, 565), &titlebar(35));
        let bar_z = scene
            .layers
            .iter()
            .find(|l| l.items.iter().any(|i| i.id() == SceneId::Titlebar))
            .expect("the bar is drawn")
            .z;
        assert!(
            scene
                .layers
                .iter()
                .filter(|l| !l.items.iter().any(|i| i.id() == SceneId::Titlebar))
                .all(|l| l.z < bar_z),
            "nothing the model drew is above the bar"
        );
    }

    #[test]
    fn a_window_the_desktop_frames_is_left_exactly_as_it_was() {
        // No bar of ours means no strip and no inset — the scene must come
        // through untouched, not translated by zero into a different-looking one.
        let plain = content(800, 600);
        let framed = with_titlebar(plain.clone(), &titlebar(0));
        assert_eq!(framed, plain);
        assert_eq!(bar_height_px(false, 1.0), 0);
    }

    #[test]
    fn the_bar_is_measured_in_logical_pixels() {
        assert_eq!(bar_height_px(true, 1.0), BAR_HEIGHT as u32);
        assert_eq!(bar_height_px(true, 2.0), (BAR_HEIGHT * 2.0) as u32);
    }

    #[test]
    fn the_four_edges_resize_along_their_own_axis() {
        assert_eq!(at(400.0, 1.0), Some(ResizeEdge::North));
        assert_eq!(at(400.0, 599.0), Some(ResizeEdge::South));
        assert_eq!(at(1.0, 300.0), Some(ResizeEdge::West));
        assert_eq!(at(799.0, 300.0), Some(ResizeEdge::East));
    }

    #[test]
    fn a_corner_wins_over_the_edges_that_meet_there() {
        // Both axes at once, and reaching further along each edge than the band
        // is deep — a corner you have to hit within 6px is a corner you miss.
        assert_eq!(at(0.0, 0.0), Some(ResizeEdge::NorthWest));
        assert_eq!(at(799.0, 0.0), Some(ResizeEdge::NorthEast));
        assert_eq!(at(0.0, 599.0), Some(ResizeEdge::SouthWest));
        assert_eq!(at(799.0, 599.0), Some(ResizeEdge::SouthEast));
        // 20px along the top edge is still the corner; 40px in is the edge.
        assert_eq!(at(20.0, 1.0), Some(ResizeEdge::NorthWest));
        assert_eq!(at(40.0, 1.0), Some(ResizeEdge::North));
        // And the same reach measured down the side.
        assert_eq!(at(1.0, 20.0), Some(ResizeEdge::NorthWest));
        assert_eq!(at(1.0, 40.0), Some(ResizeEdge::West));
    }

    #[test]
    fn the_content_is_not_a_resize_handle() {
        assert_eq!(at(400.0, 300.0), None);
        assert_eq!(at(400.0, 10.0), None, "just inside the top band");
        // Outside the window entirely — a stale position after the pointer left.
        assert_eq!(at(-1.0, 300.0), None);
        assert_eq!(at(800.0, 300.0), None);
    }

    #[test]
    fn the_band_is_measured_in_logical_pixels() {
        // The window reports physical pixels, so on a 2x display the same
        // logical band is twice as many of them — otherwise the frame gets half
        // as grabbable exactly where the pixels are smallest.
        let hidpi = |x: f64, y: f64| resize_edge_at(PointPx { x, y }, SIZE, 2.0, grab());
        assert_eq!(hidpi(400.0, 10.0), Some(ResizeEdge::North));
        assert_eq!(at(400.0, 10.0), None, "the same point at 1x is content");
    }

    #[test]
    fn a_window_with_no_free_edge_offers_no_handles() {
        // Maximized or tiled: every edge meets the screen or a neighbour.
        let boxed = FrameGrab {
            boxed_in: true,
            ..grab()
        };
        assert_eq!(
            resize_edge_at(PointPx { x: 0.0, y: 0.0 }, SIZE, 1.0, boxed),
            None
        );
    }

    #[test]
    fn the_desktops_own_frame_keeps_its_edges() {
        // With system decorations the frame around us handles resizing; reaching
        // for the same edges would fight it.
        let system = FrameGrab {
            own_frame: false,
            ..grab()
        };
        assert_eq!(
            resize_edge_at(PointPx { x: 0.0, y: 0.0 }, SIZE, 1.0, system),
            None
        );
    }

    #[test]
    fn a_drag_already_underway_is_never_hijacked_into_a_resize() {
        // Selecting text to the very edge of the window drags the pointer into
        // the band with the button down. Whatever the press started owns the
        // pointer until it is released.
        let dragging = FrameGrab {
            pointer_down: true,
            ..grab()
        };
        assert_eq!(
            resize_edge_at(PointPx { x: 0.0, y: 0.0 }, SIZE, 1.0, dragging),
            None
        );
    }
}
