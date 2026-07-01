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

use ghost_render::{CellMetrics, Run};
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
    font_from_index(data, 0)
}

/// Parse the `index`-th face from raw bytes — for a `.ttc` collection (or any file
/// fontconfig reports with a face index other than 0).
pub fn font_from_index(data: &[u8], index: usize) -> Option<FontRef<'_>> {
    FontRef::from_index(data, index)
}

/// A face's PostScript name (OpenType name id 6), if it declares one.
fn postscript_name(font: FontRef) -> Option<String> {
    font.localized_strings()
        .find(|s| s.id() == swash::StringId::PostScript)
        .map(|s| s.to_string())
}

/// Within a font file that may be a `.ttc` collection, the index of the face whose
/// PostScript name is `postscript`. macOS's CoreText resolves a family+style to a
/// file URL plus the matched face's PostScript name, but not its index within a
/// collection (Menlo, SF Mono, … all ship as collections); this recovers that index
/// so the exact face is loaded. `None` if no face in the file matches.
pub fn face_index_by_postscript(data: &[u8], postscript: &str) -> Option<usize> {
    (0usize..)
        .map_while(|i| FontRef::from_index(data, i).map(|f| (i, f)))
        .find(|(_, f)| postscript_name(*f).as_deref() == Some(postscript))
        .map(|(i, _)| i)
}

/// A font family's faces for the four terminal styles. `regular` always exists; the
/// bold/italic/bold-italic slots are `Some` only when a real face was found, so a
/// missing style is synthesized from the nearest face we do have. `Copy` — it is
/// just up to four [`FontRef`]s (each a thin borrow of already-loaded bytes).
#[derive(Clone, Copy)]
pub struct FontSet<'a> {
    pub regular: FontRef<'a>,
    pub bold: Option<FontRef<'a>>,
    pub italic: Option<FontRef<'a>>,
    pub bold_italic: Option<FontRef<'a>>,
}

impl<'a> FontSet<'a> {
    /// A single-face set: every style renders from `regular`, synthesizing bold and
    /// italic as needed — the bundled-font / no-real-faces path.
    pub fn single(regular: FontRef<'a>) -> Self {
        FontSet {
            regular,
            bold: None,
            italic: None,
            bold_italic: None,
        }
    }

    /// The face to shape and rasterize a run of style `(bold, italic)` with, plus the
    /// residual [`Synthesis`] to apply on top — whatever the chosen face does not
    /// itself provide. Prefers an exact face; then a face covering one axis (real
    /// bold with a synthetic oblique, or real italic with a synthetic weight); then
    /// regular with both synthesized. So a family that ships only Regular and Bold
    /// (e.g. Fira Code) gets a real bold weight and a synthesized oblique for its
    /// italics, automatically.
    pub fn face(&self, bold: bool, italic: bool) -> (FontRef<'a>, Synthesis) {
        let synth = |bold, italic| Synthesis { italic, bold };
        match (bold, italic) {
            (false, false) => (self.regular, Synthesis::default()),
            (true, false) => match self.bold {
                Some(f) => (f, Synthesis::default()),
                None => (self.regular, synth(true, false)),
            },
            (false, true) => match self.italic {
                Some(f) => (f, Synthesis::default()),
                None => (self.regular, synth(false, true)),
            },
            (true, true) => {
                if let Some(f) = self.bold_italic {
                    (f, Synthesis::default())
                } else if let Some(f) = self.bold {
                    (f, synth(false, true)) // real bold weight, synthetic oblique
                } else if let Some(f) = self.italic {
                    (f, synth(true, false)) // real italic shapes, synthetic weight
                } else {
                    (self.regular, synth(true, true))
                }
            }
        }
    }
}

impl<'a> From<FontRef<'a>> for FontSet<'a> {
    fn from(regular: FontRef<'a>) -> Self {
        FontSet::single(regular)
    }
}

/// The glyph id a single character maps to via the font's cmap, with no shaping
/// applied — the "naive" id, useful as a baseline against shaped output.
pub fn glyph_id(font: FontRef, ch: char) -> u16 {
    font.charmap().map(ch)
}

/// The terminal cell size for `font` at `size_px`: the monospace advance and the
/// line height, both rounded to whole pixels so the grid stays crisp. The advance is
/// a representative glyph's shaped advance (every glyph in a monospace face shares
/// it); the line height is the font's own ascent + descent + line gap. This is what
/// makes a configurable font/size correct — a different face or size has different
/// cell dimensions, and glyphs must sit on cells sized from the same font.
pub fn cell_metrics(font: FontRef, size_px: f32) -> CellMetrics {
    let advance = shape(font, "M", size_px)
        .first()
        .map(|g| g.advance)
        .filter(|a| *a > 0.0)
        .unwrap_or(size_px * 0.6)
        .round();
    let m = font.metrics(&[]).scale(size_px);
    let line_height = (m.ascent + m.descent + m.leading).round();
    CellMetrics {
        advance,
        line_height,
    }
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

/// Synthetic face styling applied at raster time when we lack dedicated bold /
/// italic faces (we ship a single regular face): an oblique shear for italics
/// and outline dilation for bold. Doubles as a glyph-cache key discriminator.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Synthesis {
    pub italic: bool,
    pub bold: bool,
}

/// Synthetic-oblique shear for faux italics. ~14° leans the top to the right,
/// the usual range for synthesized italics.
const FAUX_ITALIC_DEGREES: f32 = 14.0;

/// Faux-bold outline dilation, as a fraction of the em — the stroke grows by
/// `size_px * this` in each direction, heavier ink without a bold face.
const FAUX_BOLD_FACTOR: f32 = 0.04;

/// Rasterize a glyph to an alpha-coverage bitmap with hinting **off**, so the
/// output is bit-identical across platforms — the basis for deterministic glyph
/// goldens. `synth` applies a synthetic oblique shear and/or outline dilation
/// (faux italic / bold, since we ship a single regular face). Returns `None` for
/// a glyph with no outline (e.g. a space).
pub fn rasterize(font: FontRef, glyph: u16, size_px: f32, synth: Synthesis) -> Option<GlyphBitmap> {
    let mut ctx = ScaleContext::new();
    let mut scaler = ctx.builder(font).size(size_px).hint(false).build();
    let mut render = Render::new(&[Source::Outline]);
    render.format(Format::Alpha);
    if synth.bold {
        // Dilate the outline so the strokes thicken (swash emboldens before it
        // transforms, so this composes correctly with the italic shear).
        render.embolden(size_px * FAUX_BOLD_FACTOR);
    }
    if synth.italic {
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

#[cfg(test)]
mod tests {
    use super::*;

    // The bundled Fira Code fixture: a single face at index 0 (see the tests dir).
    const FIRA: &[u8] = include_bytes!("../tests/assets/FiraCode-Regular.ttf");

    #[test]
    fn finds_a_face_by_its_postscript_name() {
        // Read the fixture's own PostScript name, then look it back up: a single-face
        // file resolves to index 0, and an unknown name resolves to nothing. This is
        // the lookup CoreText leans on to recover a face's index inside a `.ttc`.
        let name = postscript_name(font_from_bytes(FIRA).unwrap()).expect("Fira has a PS name");
        assert_eq!(face_index_by_postscript(FIRA, &name), Some(0));
        assert_eq!(face_index_by_postscript(FIRA, "No-Such-Face"), None);
    }
}
