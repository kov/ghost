//! The shadow a window casts, as a profile the renderer samples into a LUT.
//!
//! Transcribed from GTK's Adwaita stylesheet (compiled into `libgtk-3.so.0`)
//! and then refitted against a real gnome-terminal window measured pixel by
//! pixel, because the stylesheet arithmetic does not predict what GTK actually
//! draws. It arrived here by way of sctk-adwaita, whose frame we used to wear;
//! we draw the whole frame ourselves now, so the shadow is ours to own — asking
//! a decoration crate what our own window looks like was the wrong way round.
//!
//! What it has to get right, and what the tests pin: the shadow leans
//! *downward*. At the same distance out, the bottom is nearly twice the top,
//! and that asymmetry is the whole difference between a window lit from above
//! and one floating in even light.

/// How far out the falloff runs before there is nothing left of it — under
/// 0.005 by ~25 logical pixels, and pinned to reach nothing by here. A window
/// keeping room for its shadow needs no more than this.
pub const SHADOW_REACH: f32 = 43.0;

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

/// The window shadow GTK's Adwaita casts:
///
/// ```css
/// decoration          { box-shadow: 0 3px 9px 1px rgba(0, 0, 0, 0.5),
///                                   0 0   0   1px rgba(0, 0, 0, 0.75); }
/// decoration:backdrop { box-shadow: 0 3px 9px 1px transparent,
///                                   0 2px 6px 2px rgba(0, 0, 0, 0.2),
///                                   0 0   0   1px rgba(0, 0, 0, 0.75); }
/// ```
///
/// The `0 0 0 1px` layer is the window's outer border, drawn as the edge's
/// outline rather than as a shadow, so only the blurred layers are here.
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

/// The alpha the shadow casts `out` logical pixels beyond the window's edge, in
/// a direction `down` of which points downward: 1 straight down, -1 straight up,
/// 0 out to a side, and the projection in between around a corner.
pub fn edge_alpha(out: f32, down: f32, active: bool) -> f32 {
    let layers = if active {
        ACTIVE_LAYERS
    } else {
        INACTIVE_LAYERS
    };
    // The layers are stacked, so what comes through is what none of them
    // covered.
    1.0 - layers
        .iter()
        .map(|layer| 1.0 - layer_alpha(layer, out, down))
        .product::<f32>()
}

/// The alpha the shadow casts `out` logical pixels beyond a *bottom* corner of
/// the window, along the diagonal such a corner opens up.
///
/// Its own function because rounding a corner opens a notch inside the window
/// rectangle that nothing outside reaches into. Left unpainted that notch is the
/// one place around the window with no shadow at all, and it reads as a bright
/// shard — so whoever rounds the corner fills it, with this, including the
/// downward offset, which at 45° counts for its own share.
pub fn bottom_corner_alpha(out: f32, active: bool) -> f32 {
    edge_alpha(out, std::f32::consts::FRAC_1_SQRT_2, active)
}

/// The alpha a blurred rectangle edge casts `dist` logical pixels outside
/// itself: the Gaussian's tail past that point, with the layer's offset
/// projected onto the direction the point lies in.
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
        0.254_829_6,
        -0.284_496_74,
        1.421_413_7,
        -1.453_152,
        1.061_405_4,
    ];
    const P: f32 = 0.327_591_1;

    let sign = if x < 0.0 { -1.0 } else { 1.0 };
    let x = x.abs();
    let t = 1.0 / (1.0 + P * x);
    let poly = A.iter().rev().fold(0.0, |acc, a| (acc + a) * t);
    sign * (1.0 - poly * (-x * x).exp())
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

    /// An edge: its name, which way is out, and what it was measured casting.
    type Edge = (&'static str, f32, &'static [(f32, f32)]);

    /// Every edge, with which way is out.
    fn measured() -> [Edge; 3] {
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
                let got = edge_alpha(dist, down, true);
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
                edge_alpha(dist, 1.0, true),
                edge_alpha(dist, 0.0, true),
                edge_alpha(dist, -1.0, true),
            );
            assert!(
                below > beside && beside > above,
                "at {dist}px: below {below}, beside {beside}, above {above}",
            );
        }
        // And around a corner it is the projection, not a step: halfway between
        // straight down and straight out lands halfway between their alphas.
        let diagonal = edge_alpha(4.0, std::f32::consts::FRAC_1_SQRT_2, true);
        let (below, beside) = (edge_alpha(4.0, 1.0, true), edge_alpha(4.0, 0.0, true));
        assert!(diagonal > beside && diagonal < below);
    }

    #[test]
    fn the_shadow_is_concentrated_at_the_edge_not_smeared_across_the_margin() {
        // The failure this replaced: a single wide exponential that was
        // lighter than the real thing where the eye reads an edge and heavier
        // everywhere else, which looks like grey haze rather than a shadow.
        assert!(
            edge_alpha(0.5, 1.0, true) > 0.25,
            "too light against the window"
        );

        for down in [1.0, 0.0, -1.0] {
            let mut prev = f32::MAX;
            for step in 0..=(SHADOW_REACH as u32 * 4) {
                let got = edge_alpha(step as f32 / 4.0, down, true);
                assert!(got <= prev, "the falloff rises again at {step}/4 px");
                prev = got;
            }
        }
    }

    #[test]
    fn a_backdrop_window_recedes() {
        for (edge, down, points) in measured() {
            for &(dist, _) in points {
                let (active, backdrop) =
                    (edge_alpha(dist, down, true), edge_alpha(dist, down, false));
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
        // opaque at `SHADOW_REACH` would end in a visible hard cutoff.
        for active in [true, false] {
            let edge = edge_alpha(SHADOW_REACH, 1.0, active);
            assert!(edge <= 0.002, "shadow is {edge} at the margin edge");
        }
    }

    #[test]
    fn the_notch_a_rounded_corner_opens_gets_the_corner_of_the_shadow() {
        // What goes into the notch has to be what the straight edges either
        // side of it are casting: the bottom corner's own diagonal, offset and
        // all, rather than a side value that would step where they meet.
        for out in [0.0, 1.0, 4.0] {
            let (below, beside) = (edge_alpha(out, 1.0, true), edge_alpha(out, 0.0, true));
            let corner = bottom_corner_alpha(out, true);
            assert!(
                corner > beside && corner < below,
                "at {out}px out: corner {corner} should sit between {beside} and {below}"
            );
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
