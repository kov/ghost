//! Phase-2 proof. Two things VTE could never give ghost:
//!  1. Shaping applies OpenType ligatures — Fira Code's `!=`, `->`, … are
//!     contextually substituted into different glyphs than the raw characters.
//!  2. Outline rasterization is deterministic (pure-Rust, hint-free), so a
//!     glyph's coverage is reproducible — the basis for cross-platform goldens.
//!
//! Fixture: bundled Fira Code (`assets/FiraCode-Regular.ttf`, SIL OFL-1.1; see
//! `assets/FiraCode-LICENSE-OFL.txt`), a fixed ligature-bearing font so these
//! never depend on whatever fonts a machine happens to have installed.

use ghost_shaper::{
    FontRef, FontSet, Synthesis, font_from_bytes, glyph_id, rasterize, rasterize_color, shape,
};

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

// Each `font_from_bytes` call mints a distinct swash cache key, so building several
// FontRefs from the same bytes gives us identifiable "faces" to prove which slot
// `FontSet::face` picks — without needing several real font files.
fn pick(set: &FontSet, bold: bool, italic: bool) -> (u64, Synthesis) {
    let (f, synth) = set.face(bold, italic);
    (f.key.value(), synth)
}

#[test]
fn single_face_set_synthesizes_bold_and_italic() {
    let reg = font_from_bytes(FIRA).unwrap();
    let rk = reg.key.value();
    let set = FontSet::single(reg);
    assert_eq!(pick(&set, false, false), (rk, Synthesis::default()));
    assert_eq!(
        pick(&set, true, false),
        (
            rk,
            Synthesis {
                bold: true,
                italic: false
            }
        )
    );
    assert_eq!(
        pick(&set, false, true),
        (
            rk,
            Synthesis {
                bold: false,
                italic: true
            }
        )
    );
    assert_eq!(
        pick(&set, true, true),
        (
            rk,
            Synthesis {
                bold: true,
                italic: true
            }
        )
    );
    // `From<FontRef>` is the same single-face set.
    let from: FontSet = reg.into();
    assert_eq!(
        from.face(true, true).1,
        Synthesis {
            bold: true,
            italic: true
        }
    );
}

#[test]
fn a_family_with_only_bold_uses_it_and_synthesizes_the_oblique() {
    // The Fira Code case: real Regular + Bold, no italic face.
    let reg = font_from_bytes(FIRA).unwrap();
    let bold = font_from_bytes(FIRA).unwrap();
    let (rk, bk) = (reg.key.value(), bold.key.value());
    assert_ne!(rk, bk);
    let set = FontSet {
        regular: reg,
        bold: Some(bold),
        italic: None,
        bold_italic: None,
    };
    assert_eq!(pick(&set, false, false), (rk, Synthesis::default()));
    assert_eq!(pick(&set, true, false), (bk, Synthesis::default())); // real bold
    // Italic falls back to Regular + synthetic oblique.
    assert_eq!(
        pick(&set, false, true),
        (
            rk,
            Synthesis {
                bold: false,
                italic: true
            }
        )
    );
    // Bold-italic uses the real bold weight with a synthetic oblique on top.
    assert_eq!(
        pick(&set, true, true),
        (
            bk,
            Synthesis {
                bold: false,
                italic: true
            }
        )
    );
}

#[test]
fn a_full_family_uses_the_exact_face_with_no_synthesis() {
    let reg = font_from_bytes(FIRA).unwrap();
    let bold = font_from_bytes(FIRA).unwrap();
    let ital = font_from_bytes(FIRA).unwrap();
    let bi = font_from_bytes(FIRA).unwrap();
    let set = FontSet {
        regular: reg,
        bold: Some(bold),
        italic: Some(ital),
        bold_italic: Some(bi),
    };
    assert_eq!(
        pick(&set, true, false),
        (bold.key.value(), Synthesis::default())
    );
    assert_eq!(
        pick(&set, false, true),
        (ital.key.value(), Synthesis::default())
    );
    assert_eq!(
        pick(&set, true, true),
        (bi.key.value(), Synthesis::default())
    );
}

#[test]
fn only_italic_uses_it_and_synthesizes_the_weight() {
    // A family with a real italic but no bold: bold-italic uses the italic face and
    // synthesizes the extra weight.
    let reg = font_from_bytes(FIRA).unwrap();
    let ital = font_from_bytes(FIRA).unwrap();
    let set = FontSet {
        regular: reg,
        bold: None,
        italic: Some(ital),
        bold_italic: None,
    };
    assert_eq!(
        pick(&set, false, true),
        (ital.key.value(), Synthesis::default())
    );
    assert_eq!(
        pick(&set, true, true),
        (
            ital.key.value(),
            Synthesis {
                bold: true,
                italic: false
            }
        )
    );
}

// ---- Color glyph rasterization (emoji) -------------------------------------
//
// Fixture: `assets/NotoColorEmoji-COLRv1-subset.ttf` — Noto Color Emoji
// (COLRv1 build, SIL OFL-1.1; see `assets/NotoColorEmoji-LICENSE-OFL.txt`)
// subset to U+1F92A 🤪 and U+2B50 ⭐ so the paint-graph raster is reproducible
// without depending on system fonts. Its graph exercises layers, glyph clips,
// solid fills, transforms, and linear + radial gradients.

const NOTO_EMOJI: &[u8] = include_bytes!("assets/NotoColorEmoji-COLRv1-subset.ttf");

fn emoji_font() -> FontRef<'static> {
    font_from_bytes(NOTO_EMOJI).expect("parse bundled Noto Color Emoji subset")
}

/// Distinct opaque-ish RGB values in a straight-alpha RGBA bitmap. A colorful
/// emoji has many; anything monochrome collapses to one (plus antialiased
/// edges, which the alpha threshold filters out).
fn distinct_colors(rgba: &[u8]) -> std::collections::HashSet<[u8; 3]> {
    rgba.chunks_exact(4)
        .filter(|px| px[3] > 200)
        .map(|px| [px[0], px[1], px[2]])
        .collect()
}

#[test]
fn colrv1_emoji_rasterizes_in_color() {
    let font = emoji_font();
    let gid = glyph_id(font, '\u{1F92A}');
    assert_ne!(gid, 0, "subset covers the zany face");

    let bmp = rasterize_color(font, gid, 32.0).expect("a COLRv1 glyph rasterizes in color");
    assert!(bmp.width > 0 && bmp.height > 0);
    assert!(
        bmp.width <= 128 && bmp.height <= 128,
        "a 32px raster stays glyph-sized, got {}x{}",
        bmp.width,
        bmp.height
    );
    assert_eq!(bmp.rgba.len(), (bmp.width * bmp.height * 4) as usize);

    let colors = distinct_colors(&bmp.rgba);
    assert!(
        colors.len() >= 4,
        "a color emoji carries several distinct hues, got {}",
        colors.len()
    );
    // The zany face is dominated by the yellow head: some strongly yellow
    // pixel (red and green high, blue well below) must be present.
    assert!(
        colors
            .iter()
            .any(|c| c[0] > 180 && c[1] > 120 && c[2] < 100),
        "expected the yellow face among {colors:?}"
    );
}

#[test]
fn both_subset_emoji_rasterize() {
    let font = emoji_font();
    for (ch, name) in [('\u{1F92A}', "zany face"), ('\u{2B50}', "star")] {
        let gid = glyph_id(font, ch);
        let bmp = rasterize_color(font, gid, 24.0)
            .unwrap_or_else(|| panic!("{name} rasterizes in color"));
        let covered = bmp.rgba.chunks_exact(4).filter(|px| px[3] > 0).count();
        assert!(
            covered > (bmp.width * bmp.height / 4) as usize,
            "{name} paints a substantial part of its box"
        );
    }
}

#[test]
fn color_raster_scales_with_size() {
    let font = emoji_font();
    let gid = glyph_id(font, '\u{2B50}');
    let small = rasterize_color(font, gid, 16.0).expect("16px");
    let large = rasterize_color(font, gid, 64.0).expect("64px");
    assert!(
        large.width >= small.width * 3 && large.height >= small.height * 3,
        "raster tracks the requested size: {}x{} vs {}x{}",
        small.width,
        small.height,
        large.width,
        large.height
    );
}

#[test]
fn color_raster_of_a_text_font_is_none() {
    // Fira Code has no color tables: the color path declines, so the caller
    // falls through to the normal coverage-mask raster.
    let font = font();
    assert!(rasterize_color(font, glyph_id(font, 'A'), 32.0).is_none());
}
