//! ghost-shaper — turn `ghost-render` runs into positioned glyphs and rasterize
//! them with [swash] (pure-Rust OpenType shaping + outline rasterization).
//!
//! Two properties matter here, and both are things the old VTE frontend could
//! never give ghost:
//!  * **Shaping applies OpenType features**, so programming ligatures (Fira
//!    Code's `!=`, `=>`, …) are substituted at this layer — see the tests.
//!  * **Rasterization is hint-free outline rendering** in pure Rust, so a
//!    glyph's coverage bitmap is identical across platforms, which is what makes
//!    glyph golden tests deterministic in CI.
//!
//! [swash]: https://docs.rs/swash

use ghost_render::Run;
use swash::scale::{Render, ScaleContext, Source};
use swash::shape::ShapeContext;
use swash::zeno::{Angle, Format, Transform};

pub use swash::FontRef;

/// A glyph produced by shaping: the resolved glyph id, its horizontal advance
/// in pixels, and the byte offset of the source cluster it came from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShapedGlyph {
    pub id: u16,
    pub advance: f32,
    pub cluster: u32,
}

/// An 8-bit alpha-coverage bitmap for one glyph, positioned relative to the pen
/// origin (`left`/`top`, y-up).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlyphBitmap {
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    /// Row-major coverage, `width * height` bytes.
    pub coverage: Vec<u8>,
}

/// Parse a font face (index 0) from its raw bytes.
pub fn font_from_bytes(data: &[u8]) -> Option<FontRef<'_>> {
    FontRef::from_index(data, 0)
}

/// The glyph id a single character maps to via the font's cmap, with no shaping
/// applied — the "naive" id, useful as a baseline against shaped output.
pub fn glyph_id(font: FontRef, ch: char) -> u16 {
    font.charmap().map(ch)
}

/// Shape `text` at `size_px`, applying the font's OpenType features (including
/// contextual ligatures via `calt`/`liga`).
pub fn shape(font: FontRef, text: &str, size_px: f32) -> Vec<ShapedGlyph> {
    let mut ctx = ShapeContext::new();
    let mut shaper = ctx
        .builder(font)
        .size(size_px)
        .features(&[("calt", 1), ("liga", 1)])
        .build();
    shaper.add_str(text);

    let mut out = Vec::new();
    shaper.shape_with(|cluster| {
        let source = cluster.source.start;
        for glyph in cluster.glyphs {
            out.push(ShapedGlyph {
                id: glyph.id,
                advance: glyph.advance,
                cluster: source,
            });
        }
    });
    out
}

/// Shape one laid-out [`Run`]'s text at `size_px`.
pub fn shape_run(font: FontRef, run: &Run, size_px: f32) -> Vec<ShapedGlyph> {
    shape(font, &run.text, size_px)
}

/// Synthetic-oblique shear for faux italics, applied to the glyph outline when a
/// dedicated italic face isn't available. ~14° leans the top to the right, the
/// usual range for synthesized italics.
const FAUX_ITALIC_DEGREES: f32 = 14.0;

/// Rasterize a glyph to an alpha-coverage bitmap with hinting **off**, so the
/// output is bit-identical across platforms — the basis for deterministic
/// glyph goldens. With `italic`, a synthetic oblique shear is applied to the
/// outline (faux italics, since we ship a single regular face). Returns `None`
/// for a glyph with no outline (e.g. a space).
pub fn rasterize(font: FontRef, glyph: u16, size_px: f32, italic: bool) -> Option<GlyphBitmap> {
    let mut ctx = ScaleContext::new();
    let mut scaler = ctx.builder(font).size(size_px).hint(false).build();
    let mut render = Render::new(&[Source::Outline]);
    render.format(Format::Alpha);
    if italic {
        // x' = x + y·tan(θ): outline points above the baseline shift right.
        render.transform(Some(Transform::skew(
            Angle::from_degrees(FAUX_ITALIC_DEGREES),
            Angle::ZERO,
        )));
    }
    let image = render.render(&mut scaler, glyph)?;
    Some(GlyphBitmap {
        left: image.placement.left,
        top: image.placement.top,
        width: image.placement.width,
        height: image.placement.height,
        coverage: image.data,
    })
}
