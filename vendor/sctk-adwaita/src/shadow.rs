use crate::{parts::DecorationParts, theme};
use std::collections::BTreeMap;
use tiny_skia::{Pixmap, PixmapMut, PixmapRef, Point, PremultipliedColorU8};

/// How far out of the window the frame reserves room for the shadow.
///
/// The falloff below is spent well inside this — it is under 0.005 by ~25
/// logical pixels — but the margin is also the invisible resize-grab area, so
/// it stays where upstream put it.
pub const SHADOW_SIZE: u32 = 43;

/// One `box-shadow` layer: the window rectangle grown by `spread`, moved down by
/// `dy`, blurred, and painted in black at `alpha`.
struct Layer {
    alpha: f32,
    /// Gaussian sigma, in logical pixels.
    ///
    /// Not half the CSS blur radius, as the spec would have it: GTK3 blurs with
    /// a box-blur approximation that spreads a shadow about twice as far as the
    /// spec's Gaussian, and the numbers below are what a real window casts (see
    /// the tests), not what the stylesheet arithmetic predicts.
    sigma: f32,
    spread: f32,
    /// How far down the shadow is displaced — what makes the window look lit
    /// from above rather than floating in even light.
    dy: f32,
}

/// The window shadow GTK's Adwaita casts, transcribed from its own stylesheet
/// (compiled into `libgtk-3.so.0`):
///
/// ```css
/// decoration          { box-shadow: 0 3px 9px 1px rgba(0, 0, 0, 0.5),
///                                   0 0   0   1px rgba(0, 0, 0, 0.75); }
/// decoration:backdrop { box-shadow: 0 3px 9px 1px transparent,
///                                   0 2px 6px 2px rgba(0, 0, 0, 0.2),
///                                   0 0   0   1px rgba(0, 0, 0, 0.75); }
/// ```
///
/// The `0 0 0 1px` layer is the window's outer border, which this frame draws
/// as a border rather than as a shadow (see [`crate::theme::ColorMap`]), so only
/// the blurred layers are here.
///
/// This is the shadow gnome-terminal wears, and it is deliberately not
/// libadwaita's — libadwaita casts a soft, even `0 0 14px 5px rgb(0 0 0/15%)`
/// with no offset at all, which reads as flat next to it.
const ACTIVE_LAYERS: &[Layer] = &[Layer {
    alpha: 0.5,
    sigma: 9.0,
    spread: 1.0,
    dy: 3.0,
}];
const INACTIVE_LAYERS: &[Layer] = &[Layer {
    alpha: 0.2,
    sigma: 6.0,
    spread: 2.0,
    dy: 2.0,
}];

/// The alpha a blurred rectangle edge casts `dist` logical pixels outside
/// itself, in a direction `down` of which points downward: the Gaussian's tail
/// past that point, with the layer's offset projected onto the direction the
/// point lies in.
fn layer_alpha(layer: &Layer, dist: f32, down: f32) -> f32 {
    let spread = layer.spread + layer.dy * down;
    if layer.sigma <= 0.0 {
        // An unblurred layer is a hard step at the spread edge.
        return if dist <= spread { layer.alpha } else { 0.0 };
    }
    layer.alpha * normal_cdf((spread - dist) / layer.sigma)
}

fn normal_cdf(z: f32) -> f32 {
    0.5 * (1.0 + erf(z / std::f32::consts::SQRT_2))
}

/// Abramowitz & Stegun 7.1.26 — good to ~1.5e-7, far past what an 8-bit alpha
/// channel can hold.
fn erf(x: f32) -> f32 {
    const A: [f32; 5] = [
        0.254_829_59,
        -0.284_496_74,
        1.421_413_7,
        -1.453_152_0,
        1.061_405_4,
    ];
    const P: f32 = 0.327_591_1;

    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let x = x.abs();
    let t = 1.0 / (1.0 + P * x);
    let poly = A.iter().rev().fold(0.0, |acc, a| (acc + a) * t);
    sign * (1.0 - poly * (-x * x).exp())
}

/// The alpha the shadow casts `out` logical pixels beyond a *bottom* corner of
/// the window, along the diagonal such a corner opens up.
///
/// Public because the frame cannot draw everywhere the shadow falls: a client
/// that rounds its own corners opens a notch *inside* the window rectangle,
/// which no decoration subsurface reaches. Left unpainted that notch is the one
/// place around the window with no shadow at all, and it reads as a bright
/// shard. A client in that position paints the notch itself, and asks here so
/// its shadow is the same shadow — including the downward offset, which at 45°
/// counts for its own share.
pub fn bottom_corner_alpha(out: f32, active: bool) -> f32 {
    shadow(out, std::f32::consts::FRAC_1_SQRT_2, 1, active)
}

/// The alpha `pixel_dist` device pixels outside the window's nearest edge, in a
/// direction `down` of which points downward: 1 straight down, -1 straight up,
/// 0 out to a side, and the projection in between around a corner.
fn shadow(pixel_dist: f32, down: f32, scale: u32, active: bool) -> f32 {
    let dist = pixel_dist / scale as f32;
    let layers = if active {
        ACTIVE_LAYERS
    } else {
        INACTIVE_LAYERS
    };

    // The layers are stacked, so what comes through is what none of them
    // covered.
    1.0 - layers
        .iter()
        .map(|layer| 1.0 - layer_alpha(layer, dist, down))
        .product::<f32>()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a real focused gnome-terminal casts, read off its three free edges
    /// with the clean-plate method: screenshot the window, close it, screenshot
    /// the bare desktop, and take `alpha = 1 - observed / plate` pixel for
    /// pixel, so nothing in the wallpaper's own gradient or texture can be
    /// mistaken for shadow. Distances are logical pixels out from the edge.
    ///
    /// The three lists together are the whole point of this shadow: at the same
    /// distance the bottom is nearly twice the top. That is the offset, and it
    /// is what reads as relief.
    const MEASURED_BOTTOM: &[(f32, f32)] = &[
        (0.5, 0.311),
        (4.5, 0.238),
        (8.5, 0.168),
        (13.5, 0.089),
        (19.5, 0.032),
    ];
    const MEASURED_SIDE: &[(f32, f32)] = &[
        (0.5, 0.238),
        (4.5, 0.170),
        (8.5, 0.107),
        (12.5, 0.059),
        (16.5, 0.028),
    ];
    const MEASURED_TOP: &[(f32, f32)] = &[
        (1.5, 0.168),
        (4.5, 0.116),
        (8.5, 0.068),
        (11.5, 0.042),
        (14.5, 0.023),
    ];

    /// Every edge, with which way is out.
    fn measured() -> [(&'static str, f32, &'static [(f32, f32)]); 3] {
        [
            ("bottom", 1.0, MEASURED_BOTTOM),
            ("side", 0.0, MEASURED_SIDE),
            ("top", -1.0, MEASURED_TOP),
        ]
    }

    #[test]
    fn the_shadow_matches_a_real_gnome_terminal_window() {
        for (edge, down, points) in measured() {
            for &(dist, expected) in points {
                let got = shadow(dist, down, 1, true);
                assert!(
                    (got - expected).abs() <= 0.03,
                    "{edge} at {dist} logical px: we cast {got}, gnome-terminal casts {expected}",
                );
            }
        }
    }

    #[test]
    fn the_shadow_falls_downward() {
        // A window lit from above. Without this the shadow is the same on all
        // four sides, which is what libadwaita does and what reads as flat.
        for dist in [0.5, 2.0, 6.0, 12.0] {
            let (below, beside, above) = (
                shadow(dist, 1.0, 1, true),
                shadow(dist, 0.0, 1, true),
                shadow(dist, -1.0, 1, true),
            );
            assert!(
                below > beside && beside > above,
                "at {dist}px: below {below}, beside {beside}, above {above}",
            );
        }
        // And around a corner it is the projection, not a step: halfway between
        // straight down and straight out lands halfway between their alphas.
        let diagonal = shadow(4.0, std::f32::consts::FRAC_1_SQRT_2, 1, true);
        let (below, beside) = (shadow(4.0, 1.0, 1, true), shadow(4.0, 0.0, 1, true));
        assert!(diagonal > beside && diagonal < below);
    }

    #[test]
    fn the_shadow_is_concentrated_at_the_edge_not_smeared_across_the_margin() {
        // The failure this replaced: a single wide exponential that was
        // lighter than the real thing where the eye reads an edge and heavier
        // everywhere else, which looks like grey haze rather than a shadow.
        assert!(
            shadow(0.5, 1.0, 1, true) > 0.25,
            "too light against the window"
        );

        for down in [1.0, 0.0, -1.0] {
            let mut prev = f32::MAX;
            for step in 0..=(SHADOW_SIZE * 4) {
                let got = shadow(step as f32 / 4.0, down, 1, true);
                assert!(got <= prev, "the falloff rises again at {step}/4 px");
                prev = got;
            }
        }
    }

    #[test]
    fn a_backdrop_window_recedes() {
        for (edge, down, points) in measured() {
            for &(dist, _) in points {
                let (active, backdrop) = (
                    shadow(dist, down, 1, true),
                    shadow(dist, down, 1, false),
                );
                assert!(
                    backdrop < active,
                    "{edge} at {dist} logical px: focused {active}, backdrop {backdrop}",
                );
            }
        }
    }

    #[test]
    fn the_shadow_reaches_nothing_by_the_edge_of_its_margin() {
        // Past the falloff the margin is left transparent; a shadow still
        // opaque at `SHADOW_SIZE` would end in a visible hard cutoff.
        for active in [true, false] {
            let edge = shadow(SHADOW_SIZE as f32, 1.0, 1, active);
            assert!(edge <= 0.002, "shadow is {edge} at the margin edge");
        }
    }

    #[test]
    fn scaling_stretches_the_falloff_over_more_device_pixels() {
        for (_, down, points) in measured() {
            for &(dist, _) in points {
                assert_eq!(
                    shadow(dist * 2.0, down, 2, true),
                    shadow(dist, down, 1, true)
                );
            }
        }
    }

    #[test]
    fn the_notch_a_rounded_corner_opens_gets_the_corner_of_the_shadow() {
        // What the client paints into the notch has to be what the frame would
        // have painted there: the bottom corner's own diagonal, offset and all.
        for out in [0.0, 1.0, 4.0] {
            let (below, beside) = (shadow(out, 1.0, 1, true), shadow(out, 0.0, 1, true));
            let corner = bottom_corner_alpha(out, true);
            assert!(
                corner > beside && corner < below,
                "at {out}px out: corner {corner} should sit between {beside} and {below}"
            );
        }
    }

    /// The alpha the drawn shadow leaves in one column of a decoration part.
    fn column(part_idx: usize, window: (u32, u32), x: u32) -> Vec<u8> {
        let (width, height) = match part_idx {
            DecorationParts::LEFT | DecorationParts::RIGHT => {
                (theme::BORDER_SIZE, window.1 + theme::HEADER_SIZE)
            }
            _ => unreachable!("only the sides are asked about here"),
        };
        #[allow(clippy::unwrap_used)]
        let mut pixmap = Pixmap::new(width, height).unwrap();
        RenderedShadow::new(1, true).draw(&mut pixmap.as_mut(), 1, part_idx);
        (0..height)
            .map(|y| {
                #[allow(clippy::unwrap_used)]
                pixmap.pixel(x, y).unwrap().alpha()
            })
            .collect()
    }

    #[test]
    fn the_drawn_parts_carry_the_offset_the_profile_describes() {
        // The profile above knows the shadow leans downward; this is the part
        // that has to lay the right strip against the right edge of the window.
        let width = 400 + 2 * theme::BORDER_SIZE;
        let mut top = Pixmap::new(width, theme::BORDER_SIZE).expect("a valid pixmap size");
        let mut bottom = Pixmap::new(width, theme::BORDER_SIZE).expect("a valid pixmap size");
        let rendered = RenderedShadow::new(1, true);
        rendered.draw(&mut top.as_mut(), 1, DecorationParts::TOP);
        rendered.draw(&mut bottom.as_mut(), 1, DecorationParts::BOTTOM);

        let x = width / 2;
        // The row each part puts against the window: the top part's last, the
        // bottom part's first.
        let above = top
            .pixel(x, theme::BORDER_SIZE - 1)
            .expect("a pixel in the top part")
            .alpha();
        let below = bottom
            .pixel(x, theme::VISIBLE_BORDER_SIZE)
            .expect("a pixel in the bottom part")
            .alpha();
        assert!(
            u32::from(below) > u32::from(above) * 3 / 2,
            "the shadow under the window ({below}) should be far deeper than over it ({above})"
        );
    }

    #[test]
    fn the_column_under_the_visible_border_is_shadowed_all_the_way_down() {
        // The border stops a corner short of the bottom, so the client can round
        // it — and then whatever the shadow left unpainted in that column is the
        // one place around the window with nothing on it at all. Against the
        // shadow on either side it reads as a bright shard poking out of the
        // curve, which is exactly the artefact the rounded corner was supposed
        // to remove.
        let window = (400, 300);
        for (part, x, side) in [
            (DecorationParts::LEFT, theme::BORDER_SIZE - 1, "left"),
            (DecorationParts::RIGHT, 0, "right"),
        ] {
            let alphas = column(part, window, x);
            let bottom = alphas.len() - 1;
            for (y, alpha) in alphas.iter().enumerate().skip(theme::HEADER_SIZE as usize) {
                assert!(
                    *alpha > 0,
                    "the {side} border column has no shadow at row {y} of {bottom}"
                );
            }
        }
    }

    #[test]
    fn erf_is_accurate_enough_for_an_8_bit_alpha() {
        // Reference values from the error function's series expansion.
        for &(x, expected) in &[
            (0.0, 0.0),
            (0.5, 0.520_499_9),
            (1.0, 0.842_700_8),
            (2.0, 0.995_322_3),
            (3.0, 0.999_977_9),
        ] {
            assert!((erf(x) - expected).abs() < 1e-5, "erf({x}) = {}", erf(x));
            assert!((erf(-x) + expected).abs() < 1e-5, "erf({}) wrong", -x);
        }
    }
}

#[derive(Debug)]
struct RenderedShadow {
    /// One strip per direction the shadow can leave the window in. They differ
    /// now that it is offset downward: at the same distance out, the bottom is
    /// nearly twice the top.
    side: Pixmap,
    top: Pixmap,
    bottom: Pixmap,
    edges: Pixmap,
}

impl RenderedShadow {
    fn new(scale: u32, active: bool) -> RenderedShadow {
        let shadow_size = SHADOW_SIZE * scale;
        let corner_radius = theme::CORNER_RADIUS * scale;

        let strip = |down: f32| {
            #[allow(clippy::unwrap_used)]
            let mut strip = Pixmap::new(shadow_size, 1).unwrap();
            for x in 0..strip.width() as usize {
                let alpha =
                    (shadow(x as f32 + 0.5, down, scale, active) * u8::MAX as f32).round() as u8;

                #[allow(clippy::unwrap_used)]
                let color = PremultipliedColorU8::from_rgba(0, 0, 0, alpha).unwrap();
                strip.pixels_mut()[x] = color;
            }
            strip
        };

        let edges_size = (corner_radius + shadow_size) * 2;
        #[allow(clippy::unwrap_used)]
        let mut edges = Pixmap::new(edges_size, edges_size).unwrap();
        let edges_middle = Point::from_xy(edges_size as f32 / 2.0, edges_size as f32 / 2.0);
        for y in 0..edges_size as usize {
            let y_pos = y as f32 + 0.5;
            for x in 0..edges_size as usize {
                let reach = edges_middle.distance(Point::from_xy(x as f32 + 0.5, y_pos));
                let dist = reach - corner_radius as f32;
                // Around a corner the way out swings from straight up to
                // straight down, and the offset counts for whatever share of it
                // points downward.
                let down = if reach > 0.0 {
                    (y_pos - edges_middle.y) / reach
                } else {
                    0.0
                };
                let alpha = (shadow(dist, down, scale, active) * u8::MAX as f32).round() as u8;

                #[allow(clippy::unwrap_used)]
                let color = PremultipliedColorU8::from_rgba(0, 0, 0, alpha).unwrap();
                edges.pixels_mut()[y * edges_size as usize + x] = color;
            }
        }

        RenderedShadow {
            side: strip(0.0),
            top: strip(-1.0),
            bottom: strip(1.0),
            edges,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn side_draw(
        &self,
        strip: &Pixmap,
        flipped: bool,
        rotated: bool,
        stack: usize,
        dst_pixmap: &mut PixmapMut,
        dst_left: usize,
        dst_top: usize,
    ) {
        fn iter_copy<'a>(
            src: impl Iterator<Item = &'a PremultipliedColorU8>,
            dst: impl Iterator<Item = &'a mut PremultipliedColorU8>,
        ) {
            src.zip(dst).for_each(|(src, dst)| *dst = *src)
        }

        let dst_width = dst_pixmap.width() as usize;
        let dst_pixels = dst_pixmap.pixels_mut();
        match (flipped, rotated) {
            (false, false) => (0..stack).for_each(|i| {
                let dst = dst_pixels
                    .iter_mut()
                    .skip((dst_top + i) * dst_width + dst_left);
                iter_copy(strip.pixels().iter(), dst);
            }),
            (false, true) => (0..stack).for_each(|i| {
                let dst = dst_pixels
                    .iter_mut()
                    .skip(dst_top * dst_width + dst_left + i)
                    .step_by(dst_width);
                iter_copy(strip.pixels().iter(), dst);
            }),
            (true, false) => (0..stack).for_each(|i| {
                let dst = dst_pixels
                    .iter_mut()
                    .skip((dst_top + i) * dst_width + dst_left);
                iter_copy(strip.pixels().iter().rev(), dst);
            }),
            (true, true) => (0..stack).for_each(|i| {
                let dst = dst_pixels
                    .iter_mut()
                    .skip(dst_top * dst_width + dst_left + i)
                    .step_by(dst_width);
                iter_copy(strip.pixels().iter().rev(), dst);
            }),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn edges_draw(
        &self,
        src_x_offset: isize,
        src_y_offset: isize,
        dst_pixmap: &mut PixmapMut,
        dst_rect_left: usize,
        dst_rect_top: usize,
        dst_rect_width: usize,
        dst_rect_height: usize,
    ) {
        let src_width = self.edges.width() as usize;
        let src_pixels = self.edges.pixels();
        let dst_width = dst_pixmap.width() as usize;
        let dst_pixels = dst_pixmap.pixels_mut();
        for y in 0..dst_rect_height {
            let dst_y = dst_rect_top + y;
            let src_y = y as isize + src_y_offset;
            if src_y < 0 {
                continue;
            }

            let src_y = src_y as usize;
            for x in 0..dst_rect_width {
                let dst_x = dst_rect_left + x;
                let src_x = x as isize + src_x_offset;
                if src_x < 0 {
                    continue;
                }

                let src = src_pixels.get(src_y * src_width + src_x as usize);
                let dst = dst_pixels.get_mut(dst_y * dst_width + dst_x);
                if let (Some(src), Some(dst)) = (src, dst) {
                    *dst = *src;
                }
            }
        }
    }

    fn draw(&self, dst_pixmap: &mut PixmapMut, scale: u32, part_idx: usize) {
        let shadow_size = (SHADOW_SIZE * scale) as usize;
        let visible_border_size = (theme::VISIBLE_BORDER_SIZE * scale) as usize;
        let corner_radius = (theme::CORNER_RADIUS * scale) as usize;
        assert!(corner_radius > visible_border_size);

        let dst_width = dst_pixmap.width() as usize;
        let dst_height = dst_pixmap.height() as usize;
        let edges_half = self.edges.width() as usize / 2;
        match part_idx {
            DecorationParts::TOP => {
                let left_edge_width = edges_half;
                let right_edge_width = edges_half;
                let side_width = dst_width
                    .saturating_sub(left_edge_width)
                    .saturating_sub(right_edge_width);

                self.edges_draw(
                    0,
                    -(visible_border_size as isize),
                    dst_pixmap,
                    0,
                    0,
                    left_edge_width,
                    dst_height,
                );

                self.side_draw(
                    &self.top,
                    true,
                    true,
                    side_width,
                    dst_pixmap,
                    left_edge_width,
                    visible_border_size,
                );

                self.edges_draw(
                    edges_half as isize,
                    -(visible_border_size as isize),
                    dst_pixmap,
                    left_edge_width + side_width,
                    0,
                    right_edge_width,
                    dst_height,
                );
            }
            // The side parts run the shadow under their own visible border
            // column as well as beside it. Upstream stopped one pixel short
            // there, which was invisible while the border was opaque and ran the
            // whole height — but it stops a corner short of the bottom now, so
            // the client can round it, and an unshadowed column is the one place
            // around the window with nothing on it at all: a bright shard poking
            // out of the curve.
            DecorationParts::LEFT => {
                let top_edge_height = corner_radius;
                let bottom_edge_height = corner_radius - visible_border_size;
                let side_height = dst_height
                    .saturating_sub(top_edge_height)
                    .saturating_sub(bottom_edge_height);

                self.edges_draw(0, shadow_size as isize, dst_pixmap, 0, 0, dst_width, top_edge_height);

                // Starting a column in means the strip's own first sample — the
                // deepest one — lands on the border column instead of beside it.
                // What drops off the far end is the tail, which is nothing.
                self.side_draw(
                    &self.side,
                    true,
                    false,
                    side_height,
                    dst_pixmap,
                    visible_border_size,
                    top_edge_height,
                );

                self.edges_draw(
                    0,
                    edges_half as isize,
                    dst_pixmap,
                    0,
                    top_edge_height + side_height,
                    dst_width,
                    bottom_edge_height,
                );
            }
            DecorationParts::RIGHT => {
                let top_edge_height = corner_radius;
                let bottom_edge_height = corner_radius - visible_border_size;
                let side_height = dst_height
                    .saturating_sub(top_edge_height)
                    .saturating_sub(bottom_edge_height);
                // Reaching one column further left means reading one column
                // further left as well, or the whole stretch slides over.
                let src_x = edges_half as isize + corner_radius as isize
                    - visible_border_size as isize;

                self.edges_draw(src_x, shadow_size as isize, dst_pixmap, 0, 0, dst_width, top_edge_height);

                self.side_draw(&self.side, false, false, side_height, dst_pixmap, 0, top_edge_height);

                self.edges_draw(
                    src_x,
                    edges_half as isize,
                    dst_pixmap,
                    0,
                    top_edge_height + side_height,
                    dst_width,
                    bottom_edge_height,
                );
            }
            DecorationParts::BOTTOM => {
                let left_edge_width = edges_half;
                let right_edge_width = edges_half;
                let side_width = dst_width
                    .saturating_sub(left_edge_width)
                    .saturating_sub(right_edge_width);

                self.edges_draw(
                    0,
                    edges_half as isize + (corner_radius - visible_border_size) as isize,
                    dst_pixmap,
                    0,
                    0,
                    left_edge_width,
                    dst_height,
                );

                self.side_draw(
                    &self.bottom,
                    false,
                    true,
                    side_width,
                    dst_pixmap,
                    left_edge_width,
                    visible_border_size,
                );

                self.edges_draw(
                    edges_half as isize,
                    edges_half as isize + (corner_radius - visible_border_size) as isize,
                    dst_pixmap,
                    left_edge_width + side_width,
                    0,
                    right_edge_width,
                    dst_height,
                );
            }
            DecorationParts::HEADER => {
                self.edges_draw(
                    shadow_size as isize,
                    shadow_size as isize,
                    dst_pixmap,
                    0,
                    0,
                    corner_radius,
                    corner_radius,
                );

                self.edges_draw(
                    edges_half as isize,
                    shadow_size as isize,
                    dst_pixmap,
                    dst_width.saturating_sub(corner_radius),
                    0,
                    corner_radius,
                    corner_radius,
                );
            }
            _ => unreachable!(),
        }
    }
}

#[derive(Debug)]
struct CachedPart {
    pixmap: Pixmap,
    scale: u32,
    active: bool,
}

impl CachedPart {
    fn new(
        dst_pixmap: &PixmapRef,
        rendered: &RenderedShadow,
        scale: u32,
        active: bool,
        part_idx: usize,
    ) -> CachedPart {
        #[allow(clippy::unwrap_used)]
        let mut pixmap = Pixmap::new(dst_pixmap.width(), dst_pixmap.height()).unwrap();
        rendered.draw(&mut pixmap.as_mut(), scale, part_idx);

        CachedPart {
            pixmap,
            scale,
            active,
        }
    }

    fn matches(&self, dst_pixmap: &PixmapRef, dst_scale: u32, dst_active: bool) -> bool {
        self.pixmap.width() == dst_pixmap.width()
            && self.pixmap.height() == dst_pixmap.height()
            && self.scale == dst_scale
            && self.active == dst_active
    }

    fn draw(&self, dst_pixmap: &mut PixmapMut) {
        let src_data = self.pixmap.data();
        dst_pixmap.data_mut()[..src_data.len()].copy_from_slice(src_data);
    }
}

#[derive(Default, Debug)]
pub struct Shadow {
    part_cache: [Option<CachedPart>; 5],
    // (scale, active) -> RenderedShadow
    rendered: BTreeMap<(u32, bool), RenderedShadow>,
}

impl Shadow {
    pub fn draw(&mut self, pixmap: &mut PixmapMut, scale: u32, active: bool, part_idx: usize) {
        let cache = &mut self.part_cache[part_idx];

        if let Some(cache_value) = cache {
            if !cache_value.matches(&pixmap.as_ref(), scale, active) {
                *cache = None;
            }
        }

        if cache.is_none() {
            let rendered = self
                .rendered
                .entry((scale, active))
                .or_insert_with(|| RenderedShadow::new(scale, active));

            *cache = Some(CachedPart::new(
                &pixmap.as_ref(),
                rendered,
                scale,
                active,
                part_idx,
            ));
        }

        // We filled the cache above.
        #[allow(clippy::unwrap_used)]
        cache.as_ref().unwrap().draw(pixmap);
    }
}
