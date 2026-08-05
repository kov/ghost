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
