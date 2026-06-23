//! Phase-2 proof. Two things VTE could never give ghost:
//!  1. Shaping applies OpenType ligatures — Fira Code's `!=`, `->`, … are
//!     contextually substituted into different glyphs than the raw characters.
//!  2. Outline rasterization is deterministic (pure-Rust, hint-free), so a
//!     glyph's coverage is reproducible — the basis for cross-platform goldens.
//!
//! Fixture: bundled Fira Code (`assets/FiraCode-Regular.ttf`, SIL OFL-1.1; see
//! `assets/FiraCode-LICENSE-OFL.txt`), a fixed ligature-bearing font so these
//! never depend on whatever fonts a machine happens to have installed.

use ghost_shaper::{FontRef, font_from_bytes, glyph_id, rasterize, shape};

const FIRA: &[u8] = include_bytes!("assets/FiraCode-Regular.ttf");

fn font() -> FontRef<'static> {
    font_from_bytes(FIRA).expect("parse bundled Fira Code")
}

fn shaped_ids(text: &str) -> Vec<u16> {
    shape(font(), text, 16.0).iter().map(|g| g.id).collect()
}

#[test]
fn not_equal_ligature_substitutes_glyphs() {
    let bang = glyph_id(font(), '!');
    let eq = glyph_id(font(), '=');
    assert_ne!(
        shaped_ids("!="),
        vec![bang, eq],
        "Fira Code '!=' must contextually substitute (ligate)"
    );
}

#[test]
fn arrow_ligature_substitutes_glyphs() {
    let minus = glyph_id(font(), '-');
    let gt = glyph_id(font(), '>');
    assert_ne!(
        shaped_ids("->"),
        vec![minus, gt],
        "Fira Code '->' must ligate"
    );
}

#[test]
fn plain_text_is_not_substituted() {
    let a = glyph_id(font(), 'a');
    let b = glyph_id(font(), 'b');
    assert_eq!(shaped_ids("ab"), vec![a, b], "'ab' has no ligature");
}

#[test]
fn rasterize_is_deterministic() {
    let a = glyph_id(font(), 'A');
    let bmp = rasterize(font(), a, 32.0, false).expect("'A' has an outline");
    let sum: u64 = bmp.coverage.iter().map(|&b| u64::from(b)).sum();
    eprintln!(
        "RASTER A@32 => {}x{} left={} top={} len={} sum={}",
        bmp.width,
        bmp.height,
        bmp.left,
        bmp.top,
        bmp.coverage.len(),
        sum
    );
    assert!(bmp.width > 0 && bmp.height > 0 && !bmp.coverage.is_empty());
    // Golden values (locked from the deterministic raster after the first run);
    // a regression or cross-platform divergence trips these.
    assert_eq!((bmp.width, bmp.height), (GOLDEN_WH));
    assert_eq!(sum, GOLDEN_SUM);
}

// Golden values from the deterministic pure-Rust raster of Fira Code 'A' at
// 32px, hint-free. swash produces these bit-for-bit on any platform.
const GOLDEN_WH: (u32, u32) = (20, 23);
const GOLDEN_SUM: u64 = 36031;

#[test]
fn faux_italic_shears_the_glyph() {
    // The synthetic oblique transform shears x by y (x' = x + y·tanθ). Two things
    // are then true for *any* glyph: the vertical extent is untouched (so the
    // bitmap height is identical), and ink above the baseline slides sideways (so
    // the coverage differs from the upright glyph). The bbox *width* is not a
    // reliable probe — shearing a glyph that already has diagonal strokes can
    // narrow it — so we don't assert on it.
    let a = glyph_id(font(), 'A');
    let roman = rasterize(font(), a, 32.0, false).expect("'A' has an outline");
    let italic = rasterize(font(), a, 32.0, true).expect("italic 'A' has an outline");
    assert_eq!(
        italic.height, roman.height,
        "a horizontal shear leaves height unchanged"
    );
    assert_ne!(
        italic.coverage, roman.coverage,
        "the shear must move ink, so the raster differs from the upright glyph"
    );
}
