use crate::{parts::DecorationParts, theme};
use std::collections::BTreeMap;
use tiny_skia::{Pixmap, PixmapMut, PixmapRef, Point, PremultipliedColorU8};

/// How far out of the window the frame reserves room for the shadow.
///
/// The falloff below is spent well inside this — it is under 0.005 by ~25
/// logical pixels — but the margin is also the invisible resize-grab area, so
/// it stays where upstream put it.
pub const SHADOW_SIZE: u32 = 43;

/// One `box-shadow` layer: the window rectangle grown by `spread` and blurred,
/// painted in black at `alpha`.
struct Layer {
    alpha: f32,
    /// Gaussian sigma. CSS defines a blur *radius* as twice this, so a
    /// `14px` blur is `sigma: 7.0`. Zero is a hard edge (no blur).
    sigma: f32,
    spread: f32,
}

/// libadwaita's window shadow, transcribed from its own stylesheet
/// (`/org/gnome/Adwaita/styles/gtk.css`, extracted from `libadwaita-1.so`):
///
/// ```css
/// window.csd          { box-shadow: 0 0 14px 5px rgb(0 0 0/15%),
///                                   0 0  5px 2px rgb(0 0 0/10%),
///                                   0 0  0   1px rgb(0 0 0/ 5%); }
/// window.csd:backdrop { box-shadow: 0 0 14px 5px transparent,
///                                   0 0 10px 5px rgb(0 0 0/ 8%),
///                                   0 0  0   1px rgb(0 0 0/ 5%); }
/// ```
///
/// Note there is no vertical offset — the shadow is the same on all four
/// sides, which is what this frame draws too. (GTK's *built-in* theme does
/// offset its shadow downwards, but a libadwaita app never wears it.)
const ACTIVE_LAYERS: &[Layer] = &[
    Layer {
        alpha: 0.15,
        sigma: 7.0,
        spread: 5.0,
    },
    Layer {
        alpha: 0.10,
        sigma: 2.5,
        spread: 2.0,
    },
    Layer {
        alpha: 0.05,
        sigma: 0.0,
        spread: 1.0,
    },
];
const INACTIVE_LAYERS: &[Layer] = &[
    Layer {
        alpha: 0.08,
        sigma: 5.0,
        spread: 5.0,
    },
    Layer {
        alpha: 0.05,
        sigma: 0.0,
        spread: 1.0,
    },
];

/// The alpha a blurred rectangle edge casts `dist` logical pixels outside
/// itself: the Gaussian's tail past that point.
fn layer_alpha(layer: &Layer, dist: f32) -> f32 {
    if layer.sigma <= 0.0 {
        // An unblurred layer is a hard step at the spread edge.
        return if dist <= layer.spread {
            layer.alpha
        } else {
            0.0
        };
    }
    layer.alpha * normal_cdf((layer.spread - dist) / layer.sigma)
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

/// The alpha this frame's shadow casts `dist` *logical* pixels outside the
/// window, focused or in the backdrop.
///
/// Public because the frame cannot draw everywhere the shadow falls: a client
/// that rounds its own corners opens a notch *inside* the window rectangle, which
/// no decoration subsurface reaches. Left unpainted that notch is the one place
/// around the window with no shadow at all, and it reads as a bright shard. A
/// client in that position paints the notch itself, and asks here so its shadow
/// is the same shadow.
pub fn shadow_alpha(dist: f32, active: bool) -> f32 {
    shadow(dist, 1, active)
}

fn shadow(pixel_dist: f32, scale: u32, active: bool) -> f32 {
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
        .map(|layer| 1.0 - layer_alpha(layer, dist))
        .product::<f32>()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a real libadwaita window casts, read off a screenshot of a focused
    /// `gnome-text-editor` on GNOME at scale 2: its bottom edge over the
    /// wallpaper, with the background taken 300px further out so a wide, faint
    /// tail could not be normalised away. `alpha = 1 - observed / background`,
    /// distances in logical pixels out from the window edge.
    ///
    /// The wallpaper's own texture puts the noise floor around 0.02, so this
    /// pins the shape of the falloff, not its last digit.
    const MEASURED: &[(f32, f32)] = &[
        (0.5, 0.214),
        (1.0, 0.184),
        (2.0, 0.154),
        (4.0, 0.101),
        (8.0, 0.055),
        (12.0, 0.040),
    ];

    #[test]
    fn the_shadow_matches_a_real_libadwaita_window() {
        for &(dist, expected) in MEASURED {
            let got = shadow(dist, 1, true);
            assert!(
                (got - expected).abs() <= 0.03,
                "at {dist} logical px we cast {got}, libadwaita casts {expected}",
            );
        }
    }

    #[test]
    fn the_shadow_is_concentrated_at_the_edge_not_smeared_across_the_margin() {
        // The failure this replaced: a single wide exponential that was
        // lighter than libadwaita where the eye reads an edge and heavier
        // everywhere else, which looks like grey haze rather than a shadow.
        assert!(shadow(0.5, 1, true) > 0.20, "too light against the window");
        assert!(shadow(16.0, 1, true) < 0.02, "still hazy 16px out");

        let mut prev = f32::MAX;
        for step in 0..=(SHADOW_SIZE * 4) {
            let got = shadow(step as f32 / 4.0, 1, true);
            assert!(got <= prev, "the falloff rises again at {step}/4 px");
            prev = got;
        }
    }

    #[test]
    fn a_backdrop_window_recedes() {
        for &(dist, _) in MEASURED {
            let (active, backdrop) = (shadow(dist, 1, true), shadow(dist, 1, false));
            assert!(
                backdrop < active,
                "at {dist} logical px: focused {active}, backdrop {backdrop}",
            );
        }
    }

    #[test]
    fn the_shadow_reaches_nothing_by_the_edge_of_its_margin() {
        // Past the falloff the margin is left transparent; a shadow still
        // opaque at `SHADOW_SIZE` would end in a visible hard cutoff.
        for active in [true, false] {
            let edge = shadow(SHADOW_SIZE as f32, 1, active);
            assert!(edge <= 0.002, "shadow is {edge} at the margin edge");
        }
    }

    #[test]
    fn scaling_stretches_the_falloff_over_more_device_pixels() {
        for &(dist, _) in MEASURED {
            assert_eq!(shadow(dist * 2.0, 2, true), shadow(dist, 1, true));
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
    side: Pixmap,
    edges: Pixmap,
}

impl RenderedShadow {
    fn new(scale: u32, active: bool) -> RenderedShadow {
        let shadow_size = SHADOW_SIZE * scale;
        let corner_radius = theme::CORNER_RADIUS * scale;

        #[allow(clippy::unwrap_used)]
        let mut side = Pixmap::new(shadow_size, 1).unwrap();
        for x in 0..side.width() as usize {
            let alpha = (shadow(x as f32 + 0.5, scale, active) * u8::MAX as f32).round() as u8;

            #[allow(clippy::unwrap_used)]
            let color = PremultipliedColorU8::from_rgba(0, 0, 0, alpha).unwrap();
            side.pixels_mut()[x] = color;
        }

        let edges_size = (corner_radius + shadow_size) * 2;
        #[allow(clippy::unwrap_used)]
        let mut edges = Pixmap::new(edges_size, edges_size).unwrap();
        let edges_middle = Point::from_xy(edges_size as f32 / 2.0, edges_size as f32 / 2.0);
        for y in 0..edges_size as usize {
            let y_pos = y as f32 + 0.5;
            for x in 0..edges_size as usize {
                let dist = edges_middle.distance(Point::from_xy(x as f32 + 0.5, y_pos))
                    - corner_radius as f32;
                let alpha = (shadow(dist, scale, active) * u8::MAX as f32).round() as u8;

                #[allow(clippy::unwrap_used)]
                let color = PremultipliedColorU8::from_rgba(0, 0, 0, alpha).unwrap();
                edges.pixels_mut()[y * edges_size as usize + x] = color;
            }
        }

        RenderedShadow { side, edges }
    }

    fn side_draw(
        &self,
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
                iter_copy(self.side.pixels().iter(), dst);
            }),
            (false, true) => (0..stack).for_each(|i| {
                let dst = dst_pixels
                    .iter_mut()
                    .skip(dst_top * dst_width + dst_left + i)
                    .step_by(dst_width);
                iter_copy(self.side.pixels().iter(), dst);
            }),
            (true, false) => (0..stack).for_each(|i| {
                let dst = dst_pixels
                    .iter_mut()
                    .skip((dst_top + i) * dst_width + dst_left);
                iter_copy(self.side.pixels().iter().rev(), dst);
            }),
            (true, true) => (0..stack).for_each(|i| {
                let dst = dst_pixels
                    .iter_mut()
                    .skip(dst_top * dst_width + dst_left + i)
                    .step_by(dst_width);
                iter_copy(self.side.pixels().iter().rev(), dst);
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

                self.side_draw(false, false, side_height, dst_pixmap, 0, top_edge_height);

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
