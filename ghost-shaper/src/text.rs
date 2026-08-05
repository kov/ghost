//! Paint a *proportional* string into a CPU bitmap.
//!
//! The rest of this crate serves the terminal grid, where layout is decided
//! before shaping: one cell per column, one face for the whole run, no
//! positioning offsets. Window chrome is the other kind of text — a title is a
//! sentence in whatever scripts the user's shell chose to put there — and it
//! needs the three things a grid never does:
//!
//!  * **per-character fallback inside one string**, so a title that mixes
//!    scripts doesn't lose the half the configured face lacks,
//!  * **shaped positioning**, so a combining mark lands on its base instead of
//!    taking an advance of its own, and
//!  * **color glyphs**, so an emoji in a title is an emoji and not a hole.
//!
//! Output is premultiplied RGBA — what `tiny_skia::Pixmap` and the compositor
//! both want — laid out with the baseline at a stable height, so a title
//! without descenders doesn't sit differently from one with them.

use crate::{
    ColorGlyphBitmap, Fallback, FontRef, FontSet, GlyphBitmap, ShapedGlyph, Synthesis, covers,
    has_color_glyphs, rasterize_color,
};
use swash::scale::image::Content;
use swash::scale::{Render, ScaleContext, Source};
use swash::shape::ShapeContext;
use swash::zeno::Format;

/// How the faces should be realized.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TextStyle {
    /// Variable-font `wght` axis value, 100–900 — for a weight the family
    /// expresses as a variation rather than a separate face (GNOME's
    /// `Cantarell Bold 11` against a family that ships one variable file).
    /// A static face has no such axis and ignores it.
    pub weight: Option<f32>,
}

impl TextStyle {
    /// The variation settings to build a shaper or scaler with.
    fn variations(&self) -> Vec<(&'static str, f32)> {
        self.weight.map(|w| ("wght", w)).into_iter().collect()
    }
}

/// A painted run of text: premultiplied RGBA, row-major, `width * height * 4`
/// bytes, plus the row the glyphs' baseline sits on (chrome that wants to align
/// text against other elements uses it; centering the whole bitmap ignores it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextImage {
    pub width: u32,
    pub height: u32,
    pub baseline: i32,
    /// Row-major premultiplied RGBA, `width * height * 4` bytes.
    pub rgba: Vec<u8>,
}

/// Paint `text` at `size_px` in `color` (straight-alpha RGBA, each channel
/// `0.0..=1.0`), drawing each character from the face that actually covers it:
/// `fonts`' regular face where it can, otherwise whatever `fallback` resolves.
/// Color glyphs (emoji) paint in their own colors, taking only `color`'s alpha.
///
/// `None` when the result would have no ink at all — an empty or all-blank
/// title has no bitmap to hand anyone, rather than a zero-sized one.
///
/// Known edge: a combining mark is only guaranteed to shape with its base when
/// one face covers both. When the base comes from a fallback face and the mark
/// does not (or vice versa) they land in separate runs and the mark is
/// positioned on its own.
pub fn paint_text(
    fonts: FontSet<'_>,
    fallback: &mut dyn Fallback,
    text: &str,
    size_px: f32,
    color: [f32; 4],
    style: TextStyle,
) -> Option<TextImage> {
    let (primary, synth) = fonts.face(false, false);
    let placed = place(primary, synth, fallback, text, size_px, style);

    // The em box fixes the vertical frame (and the horizontal one for a run
    // whose glyphs all sit inside their advances), so the same title painted
    // twice lands the same way. Ink that escapes it — an accent above the
    // ascent, an italic's overhang — grows the canvas rather than being cut.
    let m = primary.metrics(&[]).scale(size_px);
    let mut left = 0i32;
    let mut right = placed.advance.ceil() as i32;
    let mut top = -(m.ascent.ceil() as i32);
    let mut bottom = m.descent.ceil() as i32;
    let mut any_ink = false;
    for g in &placed.glyphs {
        let (w, h) = g.size();
        if w == 0 || h == 0 {
            continue;
        }
        any_ink = true;
        left = left.min(g.x);
        top = top.min(g.y);
        right = right.max(g.x + w as i32);
        bottom = bottom.max(g.y + h as i32);
    }
    if !any_ink {
        return None;
    }

    let width = (right - left).max(1) as u32;
    let height = (bottom - top).max(1) as u32;
    let mut img = TextImage {
        width,
        height,
        baseline: -top,
        rgba: vec![0; (width as usize) * (height as usize) * 4],
    };
    for g in &placed.glyphs {
        g.blend_into(&mut img, g.x - left, g.y - top, color);
    }
    Some(img)
}

/// One glyph of a laid-out string: which face it must be drawn from, which
/// glyph, and where its pen sits relative to the run's origin (x rightwards
/// from the start, y *up* from the baseline, as shaping reports it).
///
/// This is the seam between laying text out and putting it on a surface. The
/// layout — per-character fallback, shaped positions, the synthesis a face
/// needs — is subtle enough that it must exist once; what a caller does with
/// the placements is not. [`paint_text`] rasterizes them into a CPU bitmap; a
/// GPU renderer instead looks each up in its glyph atlas.
#[derive(Clone, Copy)]
pub struct PlacedGlyph<'a> {
    pub face: FontRef<'a>,
    pub id: u16,
    pub x: f32,
    pub y: f32,
    /// What the chosen face does not itself provide (faux bold/italic).
    pub synth: Synthesis,
}

/// A laid-out string: where each glyph goes, how far the run advances, and the
/// vertical extent of the face it was laid out in (so a caller can centre it
/// without measuring ink, which would make the text jump as it changed).
#[derive(Clone)]
pub struct TextLayout<'a> {
    pub glyphs: Vec<PlacedGlyph<'a>>,
    pub advance: f32,
    pub ascent: f32,
    pub descent: f32,
}

/// Lay `text` out at `size_px`, drawing each character from the face that
/// actually covers it — `fonts`' regular face where it can, otherwise whatever
/// `fallback` resolves — with shaping's own positioning within each run.
///
/// The layout half of [`paint_text`]; see [`PlacedGlyph`] for why it is public.
pub fn layout_text<'a>(
    fonts: FontSet<'a>,
    fallback: &mut dyn Fallback,
    text: &str,
    size_px: f32,
    style: TextStyle,
) -> TextLayout<'a> {
    let (primary, synth) = fonts.face(false, false);
    let mut glyphs = Vec::new();
    let mut pen = 0.0f32;
    for run in segment(primary, fallback, text) {
        // A fallback face is used as it is: it was chosen for coverage, and
        // synthesizing weight onto someone else's face is worse than not. The
        // weight axis still applies — a fallback that has one should match the
        // text it is standing in for, and one that hasn't ignores it.
        let synth = if run.is_primary {
            synth
        } else {
            Synthesis::default()
        };
        for g in shape_varied(run.face, run.text, size_px, style) {
            glyphs.push(PlacedGlyph {
                face: run.face,
                id: g.id,
                x: pen + g.x,
                y: g.y,
                synth,
            });
            pen += g.advance;
        }
    }
    let m = primary.metrics(&[]).scale(size_px);
    TextLayout {
        glyphs,
        advance: pen,
        ascent: m.ascent,
        descent: m.descent,
    }
}

/// One rasterized glyph and where its top-left pixel goes, in a coordinate
/// space whose origin is the pen start on the baseline (y-down).
struct Placed {
    x: i32,
    y: i32,
    bitmap: Raster,
}

/// A glyph's rasterized form: a coverage mask tinted with the text color, or a
/// color bitmap that carries its own.
enum Raster {
    Mask(GlyphBitmap),
    Color(ColorGlyphBitmap),
}

struct Layout {
    glyphs: Vec<Placed>,
    advance: f32,
}

impl Placed {
    fn size(&self) -> (u32, u32) {
        match &self.bitmap {
            Raster::Mask(b) => (b.width, b.height),
            Raster::Color(b) => (b.width, b.height),
        }
    }

    /// Source-over this glyph onto `img` at `(dx, dy)`, premultiplying as it
    /// goes. A mask takes `color`; a color bitmap keeps its own and takes only
    /// the alpha.
    fn blend_into(&self, img: &mut TextImage, dx: i32, dy: i32, color: [f32; 4]) {
        let (w, h) = self.size();
        let alpha = color[3].clamp(0.0, 1.0);
        for row in 0..h {
            for col in 0..w {
                let (src, sa) = match &self.bitmap {
                    Raster::Mask(b) => {
                        let cov = b.coverage[(row * w + col) as usize] as f32 / 255.0;
                        let a = cov * alpha;
                        ([color[0], color[1], color[2]], a)
                    }
                    Raster::Color(b) => {
                        let p = ((row * w + col) * 4) as usize;
                        let c = |i: usize| b.rgba[p + i] as f32 / 255.0;
                        ([c(0), c(1), c(2)], c(3) * alpha)
                    }
                };
                if sa <= 0.0 {
                    continue;
                }
                let (x, y) = (dx + col as i32, dy + row as i32);
                if x < 0 || y < 0 || x >= img.width as i32 || y >= img.height as i32 {
                    continue;
                }
                let i = ((y as u32 * img.width + x as u32) * 4) as usize;
                let dst = &mut img.rgba[i..i + 4];
                let da = dst[3] as f32 / 255.0;
                let out_a = sa + da * (1.0 - sa);
                let to_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
                for ch in 0..3 {
                    // Both sides premultiplied: src contributes `c * sa`, the
                    // destination is already premultiplied so it only fades.
                    let d = dst[ch] as f32 / 255.0;
                    dst[ch] = to_u8(src[ch] * sa + d * (1.0 - sa));
                }
                dst[3] = to_u8(out_a);
            }
        }
    }
}

/// Rasterize every glyph of a laid-out `text`, keeping each one's pen position.
fn place(
    primary: FontRef<'_>,
    synth: Synthesis,
    fallback: &mut dyn Fallback,
    text: &str,
    size_px: f32,
    style: TextStyle,
) -> Layout {
    let laid = layout_text(
        FontSet {
            regular: primary,
            // The synthesis the caller resolved is carried per-glyph by the
            // layout; a bare regular slot is all it needs to segment runs.
            bold: None,
            italic: None,
            bold_italic: None,
        },
        fallback,
        text,
        size_px,
        style,
    );
    let mut glyphs = Vec::new();
    for g in &laid.glyphs {
        // The layout gives a fallback face no synthesis; the primary keeps the
        // caller's, since `FontSet::single` above cannot carry it.
        let synth = if same_face(g.face, primary) {
            synth
        } else {
            g.synth
        };
        let color_capable = has_color_glyphs(g.face);
        let raster = color_capable
            .then(|| rasterize_color(g.face, g.id, size_px))
            .flatten()
            .map(Raster::Color)
            .or_else(|| rasterize_varied(g.face, g.id, size_px, synth, style).map(Raster::Mask));
        if let Some(bitmap) = raster {
            let (left, top) = match &bitmap {
                Raster::Mask(b) => (b.left, b.top),
                Raster::Color(b) => (b.left, b.top),
            };
            glyphs.push(Placed {
                // `left`/`top` are y-up from the pen origin; the canvas is
                // y-down from the baseline.
                x: (g.x + left as f32).round() as i32,
                y: (-g.y - top as f32).round() as i32,
                bitmap,
            });
        }
    }
    Layout {
        glyphs,
        advance: laid.advance,
    }
}

/// Whether two `FontRef`s are the same loaded face.
fn same_face(a: FontRef<'_>, b: FontRef<'_>) -> bool {
    a.key.value() == b.key.value()
}

/// [`shape`](crate::shape), with the style's variation settings applied. Chrome
/// text needs them and the terminal grid does not, so this stays local rather
/// than widening the crate's shaping API.
fn shape_varied(font: FontRef<'_>, text: &str, size_px: f32, style: TextStyle) -> Vec<ShapedGlyph> {
    let mut ctx = ShapeContext::new();
    let mut shaper = ctx
        .builder(font)
        .size(size_px)
        .variations(style.variations())
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
                x: glyph.x,
                y: glyph.y,
                cluster: source,
            });
        }
    });
    out
}

/// [`rasterize`](crate::rasterize), with the style's variation settings applied
/// — the raster must be realized at the same axis values it was shaped at.
fn rasterize_varied(
    font: FontRef<'_>,
    glyph: u16,
    size_px: f32,
    synth: Synthesis,
    style: TextStyle,
) -> Option<GlyphBitmap> {
    let mut ctx = ScaleContext::new();
    let mut scaler = ctx
        .builder(font)
        .size(size_px)
        .hint(false)
        .variations(style.variations())
        .build();
    let mut render = Render::new(&[Source::Outline]);
    render.format(Format::Alpha);
    if synth.bold {
        render.embolden(size_px * crate::FAUX_BOLD_FACTOR);
    }
    if synth.italic {
        render.transform(Some(swash::zeno::Transform::skew(
            swash::zeno::Angle::from_degrees(crate::FAUX_ITALIC_DEGREES),
            swash::zeno::Angle::ZERO,
        )));
    }
    let image = render.render(&mut scaler, glyph)?;
    if image.content != Content::Mask {
        return None;
    }
    Some(GlyphBitmap {
        left: image.placement.left,
        top: image.placement.top,
        width: image.placement.width,
        height: image.placement.height,
        coverage: image.data,
    })
}

/// A maximal slice of `text` drawn from one face. The face and the string are
/// borrowed independently — a renderer holds faces for the whole run of the
/// program and lays out whatever string it is handed.
struct Run<'f, 't> {
    face: FontRef<'f>,
    text: &'t str,
    is_primary: bool,
}

/// Split `text` into runs by the face that covers each character. The primary
/// face wins wherever it can, so a fallback never annexes text the configured
/// font could have drawn; a character neither the primary nor the current
/// fallback covers gets its own lookup, and one nothing covers stays on the
/// primary and draws as `.notdef` — the honest "this font has no such glyph".
fn segment<'f, 't>(
    primary: FontRef<'f>,
    fallback: &mut dyn Fallback,
    text: &'t str,
) -> Vec<Run<'f, 't>> {
    let mut runs: Vec<Run<'f, 't>> = Vec::new();
    let mut start = 0usize;
    let mut current: Option<(FontRef<'f>, bool)> = None;
    for (at, ch) in text.char_indices() {
        let face = if covers(primary, ch) {
            Some((primary, true))
        } else {
            // Staying on the current fallback where it covers the character
            // keeps a run of one script in one face, which is what shaping
            // needs to position marks within it.
            match current {
                Some((f, false)) if covers(f, ch) => Some((f, false)),
                _ => fallback.face_for(ch).map(|f| (f, false)),
            }
        };
        let face = face.unwrap_or((primary, true));
        let same = current.is_some_and(|(f, _)| f.key.value() == face.0.key.value());
        if !same {
            if let Some((f, is_primary)) = current {
                runs.push(Run {
                    face: f,
                    text: &text[start..at],
                    is_primary,
                });
            }
            start = at;
            current = Some(face);
        }
    }
    if let Some((f, is_primary)) = current {
        runs.push(Run {
            face: f,
            text: &text[start..],
            is_primary,
        });
    }
    runs
}
