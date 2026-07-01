//! Phase-2 proof. Two things VTE could never give ghost:
//!  1. Shaping applies OpenType ligatures — Fira Code's `!=`, `->`, … are
//!     contextually substituted into different glyphs than the raw characters.
//!  2. Outline rasterization is deterministic (pure-Rust, hint-free), so a
//!     glyph's coverage is reproducible — the basis for cross-platform goldens.
//!
//! Fixture: bundled Fira Code (`assets/FiraCode-Regular.ttf`, SIL OFL-1.1; see
//! `assets/FiraCode-LICENSE-OFL.txt`), a fixed ligature-bearing font so these
//! never depend on whatever fonts a machine happens to have installed.

use ghost_shaper::{FontRef, Synthesis, font_from_bytes, glyph_id, rasterize, shape};

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
    let bmp = rasterize(font(), a, 32.0, Synthesis::default()).expect("'A' has an outline");
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
    let roman = rasterize(font(), a, 32.0, Synthesis::default()).expect("'A' has an outline");
    let italic = rasterize(
        font(),
        a,
        32.0,
        Synthesis {
            italic: true,
            bold: false,
        },
    )
    .expect("italic 'A' has an outline");
    assert_eq!(
        italic.height, roman.height,
        "a horizontal shear leaves height unchanged"
    );
    assert_ne!(
        italic.coverage, roman.coverage,
        "the shear must move ink, so the raster differs from the upright glyph"
    );
}

#[test]
fn faux_bold_thickens_the_glyph() {
    // Emboldening dilates the outline, so the bold raster covers strictly more
    // ink (a higher total alpha) than the regular glyph at the same size.
    let a = glyph_id(font(), 'A');
    let roman = rasterize(font(), a, 32.0, Synthesis::default()).expect("'A' has an outline");
    let bold = rasterize(
        font(),
        a,
        32.0,
        Synthesis {
            italic: false,
            bold: true,
        },
    )
    .expect("bold 'A' has an outline");
    let ink = |b: &ghost_shaper::GlyphBitmap| b.coverage.iter().map(|&p| u64::from(p)).sum::<u64>();
    assert!(
        ink(&bold) > ink(&roman),
        "embolden must add ink: bold {} vs roman {}",
        ink(&bold),
        ink(&roman)
    );
}

#[test]
fn cell_metrics_match_the_historic_fira_15px_grid() {
    // The renderer shipped a hardcoded 9x18 cell for Fira Code at 15px; deriving the
    // cell from the font must reproduce it exactly, so making the font/size
    // configurable doesn't shift the default layout.
    let m = ghost_shaper::cell_metrics(font(), 15.0);
    assert_eq!(m.advance, 9.0);
    assert_eq!(m.line_height, 18.0);
}

#[test]
fn cell_metrics_scale_with_size() {
    // Sanity for other sizes: a positive whole-pixel cell that grows with the font.
    let small = ghost_shaper::cell_metrics(font(), 12.0);
    let large = ghost_shaper::cell_metrics(font(), 24.0);
    for m in [small, large] {
        assert!(m.advance > 0.0 && m.advance.fract() == 0.0);
        assert!(m.line_height > 0.0 && m.line_height.fract() == 0.0);
        assert!(
            m.line_height > m.advance,
            "a cell is taller than it is wide"
        );
    }
    assert!(large.advance > small.advance);
    assert!(large.line_height > small.line_height);
}
