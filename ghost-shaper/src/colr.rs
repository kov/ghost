//! COLRv1 color-glyph rasterization: [skrifa] traverses the paint graph and
//! this module's [`ColorPainter`] renders the callbacks onto a [tiny-skia]
//! pixmap. This covers the format modern Noto Color Emoji ships as (COLR
//! version 1 with an empty v0 record array), which swash cannot paint; swash
//! keeps covering COLRv0 layers and bitmap strikes (see
//! [`rasterize_color`](crate::rasterize_color)).
//!
//! The paint graph is evaluated in font units; the painter carries the
//! font-units→pixels mapping (scale by `size_px / upem`, y flipped) as the root
//! of its transform stack, so every fill, clip and gradient lands in pixel
//! space through ordinary transform composition.
//!
//! One known approximation, chosen over refusing the glyph outright: sweep
//! gradients (no conic shader in tiny-skia) fill with their middle stop's
//! color. None occur in the bundled test subset; Noto uses them sparingly.

use skrifa::color::{Brush, ColorGlyphFormat, ColorPainter, ColorStop, CompositeMode};
use skrifa::instance::{LocationRef, Size};
use skrifa::outline::{DrawSettings, OutlineGlyphCollection, OutlinePen};
use skrifa::raw::TableProvider;
use skrifa::{GlyphId, MetadataProvider};
use tiny_skia as ts;

use crate::ColorGlyphBitmap;

/// Pixmap dimension cap, as a multiple of the requested size: a glyph's clip
/// box should be about an em; refuse a graph claiming wildly more before
/// allocating for it.
const MAX_EXTENT_FACTOR: f32 = 4.0;

/// Rasterize `glyph` from `font`'s COLRv1 table at `size_px`, if it has a v1
/// paint graph. Output is straight-alpha RGBA (un-premultiplied from the
/// tiny-skia surface) with swash-style pen-relative placement.
pub(crate) fn rasterize_colrv1(
    font: crate::FontRef,
    glyph: u16,
    size_px: f32,
) -> Option<ColorGlyphBitmap> {
    let index = face_index(font.data, font.offset);
    let sk = skrifa::FontRef::from_index(font.data, index).ok()?;
    let gid = GlyphId::new(glyph as u32);
    let color_glyph = sk
        .color_glyphs()
        .get_with_format(gid, ColorGlyphFormat::ColrV1)?;

    // The clip box at the target size, in pixels (y-up, baseline-relative).
    // Fonts aren't required to carry one; fall back to a generous em-sized box.
    let bbox = color_glyph
        .bounding_box(LocationRef::default(), Size::new(size_px))
        .unwrap_or(skrifa::metrics::BoundingBox {
            x_min: -0.25 * size_px,
            y_min: -0.5 * size_px,
            x_max: 1.5 * size_px,
            y_max: 1.25 * size_px,
        });
    let left = bbox.x_min.floor();
    let top = bbox.y_max.ceil();
    let width = (bbox.x_max.ceil() - left) as i32;
    let height = (top - bbox.y_min.floor()) as i32;
    let cap = (size_px * MAX_EXTENT_FACTOR).ceil() as i32;
    if width <= 0 || height <= 0 || width > cap || height > cap {
        return None;
    }
    let (width, height) = (width as u32, height as u32);

    let upem = sk.head().ok()?.units_per_em() as f32;
    if upem <= 0.0 {
        return None;
    }
    let scale = size_px / upem;
    // Font units → pixmap pixels: scale, flip y, shift the clip box to origin.
    let root = ts::Transform::from_row(scale, 0.0, 0.0, -scale, -left, top);

    let mut painter = PixmapPainter {
        outlines: sk.outline_glyphs(),
        palette: palette(&sk),
        transforms: vec![root],
        clips: Vec::new(),
        layers: Vec::new(),
        base: ts::Pixmap::new(width, height)?,
        width,
        height,
    };
    color_glyph
        .paint(LocationRef::default(), &mut painter)
        .ok()?;

    let mut rgba = painter.base.take();
    if rgba.chunks_exact(4).all(|px| px[3] == 0) {
        return None; // painted nothing: treat as "no color form"
    }
    unpremultiply(&mut rgba);
    Some(ColorGlyphBitmap {
        left: left as i32,
        top: top as i32,
        width,
        height,
        rgba,
    })
}

/// The face index within `data` whose table directory sits at `offset` — how a
/// swash [`FontRef`](crate::FontRef) (which records the offset) is mapped back
/// to the index skrifa wants. A non-collection file is always index 0.
fn face_index(data: &[u8], offset: u32) -> u32 {
    if data.get(..4) != Some(b"ttcf") {
        return 0;
    }
    let count = data
        .get(8..12)
        .map(|b| u32::from_be_bytes(b.try_into().unwrap()))
        .unwrap_or(0);
    (0..count)
        .find(|i| {
            let at = 12 + *i as usize * 4;
            data.get(at..at + 4)
                .map(|b| u32::from_be_bytes(b.try_into().unwrap()))
                == Some(offset)
        })
        .unwrap_or(0)
}

/// Palette 0's colors as straight RGBA (CPAL stores BGRA records).
fn palette(font: &skrifa::FontRef) -> Vec<[u8; 4]> {
    let Ok(cpal) = font.cpal() else {
        return Vec::new();
    };
    let Some(Ok(records)) = cpal.color_records_array() else {
        return Vec::new();
    };
    let start = cpal
        .color_record_indices()
        .first()
        .map(|i| i.get() as usize)
        .unwrap_or(0);
    records
        .iter()
        .skip(start)
        .take(cpal.num_palette_entries() as usize)
        .map(|r| [r.red, r.green, r.blue, r.alpha])
        .collect()
}

/// Convert tiny-skia's premultiplied surface bytes to straight alpha, the
/// [`ColorGlyphBitmap`] contract (rounding to nearest).
fn unpremultiply(rgba: &mut [u8]) {
    for px in rgba.chunks_exact_mut(4) {
        let a = px[3] as u32;
        if a != 0 && a != 255 {
            for c in &mut px[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
}

/// The [`ColorPainter`] that renders skrifa's paint-graph callbacks onto
/// tiny-skia pixmaps: a transform stack rooted at the font→pixel mapping, a
/// clip stack of intersected coverage masks, and a layer stack of offscreen
/// pixmaps composited on pop with their COLR composite mode.
struct PixmapPainter<'a> {
    outlines: OutlineGlyphCollection<'a>,
    palette: Vec<[u8; 4]>,
    transforms: Vec<ts::Transform>,
    clips: Vec<ts::Mask>,
    layers: Vec<(ts::Pixmap, CompositeMode)>,
    base: ts::Pixmap,
    width: u32,
    height: u32,
}

impl PixmapPainter<'_> {
    fn current(&self) -> ts::Transform {
        *self
            .transforms
            .last()
            .expect("root transform always present")
    }

    /// A glyph's outline as a tiny-skia path in font units (the current
    /// transform is applied at draw time).
    fn glyph_path(&self, glyph_id: GlyphId) -> Option<ts::Path> {
        let outline = self.outlines.get(glyph_id)?;
        let mut pen = PathPen(ts::PathBuilder::new());
        outline
            .draw(
                DrawSettings::unhinted(Size::unscaled(), LocationRef::default()),
                &mut pen,
            )
            .ok()?;
        pen.0.finish()
    }

    /// Resolve a CPAL palette index (`0xFFFF` = current text color, fixed to
    /// black — see [`rasterize_color`](crate::rasterize_color)) with an extra
    /// alpha multiplier.
    fn color(&self, palette_index: u16, alpha: f32) -> ts::Color {
        let [r, g, b, a] = self
            .palette
            .get(palette_index as usize)
            .copied()
            .unwrap_or([0, 0, 0, 255]);
        let a = (a as f32 / 255.0) * alpha.clamp(0.0, 1.0);
        ts::Color::from_rgba(
            r as f32 / 255.0,
            g as f32 / 255.0,
            b as f32 / 255.0,
            a.clamp(0.0, 1.0),
        )
        .unwrap_or(ts::Color::BLACK)
    }

    /// Gradient stops resolved to colors and sorted by offset (COLR does not
    /// guarantee order; tiny-skia requires it).
    fn stops(&self, stops: &[ColorStop]) -> Vec<(f32, ts::Color)> {
        let mut out: Vec<(f32, ts::Color)> = stops
            .iter()
            .map(|s| {
                (
                    s.offset.clamp(0.0, 1.0),
                    self.color(s.palette_index, s.alpha),
                )
            })
            .collect();
        out.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        out
    }

    /// Build the brush's shader in pixel space: gradient geometry arrives in
    /// the paint node's coordinate system, so the shader transform is the
    /// current stack composed with the brush's own transform.
    fn shader(
        &self,
        brush: &Brush<'_>,
        brush_transform: Option<skrifa::color::Transform>,
    ) -> Option<ts::Shader<'static>> {
        let t = match brush_transform {
            Some(bt) => self.current().pre_concat(convert(bt)),
            None => self.current(),
        };
        let point = |p: skrifa::raw::types::Point<f32>| ts::Point::from_xy(p.x, p.y);
        match brush {
            Brush::Solid {
                palette_index,
                alpha,
            } => Some(ts::Shader::SolidColor(self.color(*palette_index, *alpha))),
            Brush::LinearGradient {
                p0,
                p1,
                color_stops,
                extend,
            } => ts::LinearGradient::new(
                point(*p0),
                point(*p1),
                gradient_stops(self.stops(color_stops)),
                spread(*extend),
                t,
            ),
            Brush::RadialGradient {
                c0,
                r0,
                c1,
                r1,
                color_stops,
                extend,
            } => ts::RadialGradient::new(
                point(*c0),
                *r0,
                point(*c1),
                *r1,
                gradient_stops(self.stops(color_stops)),
                spread(*extend),
                t,
            ),
            Brush::SweepGradient { color_stops, .. } => {
                // No conic shader in tiny-skia: fill with the middle stop.
                let stops = self.stops(color_stops);
                let mid = stops.get(stops.len() / 2)?;
                Some(ts::Shader::SolidColor(mid.1))
            }
        }
    }

    /// The pixmap draws currently land on: the innermost open layer, else the
    /// final surface. Field-projected so the clip stack stays borrowable.
    fn target_and_clip(&mut self) -> (&mut ts::Pixmap, Option<&ts::Mask>) {
        let target = match self.layers.last_mut() {
            Some((pixmap, _)) => pixmap,
            None => &mut self.base,
        };
        (target, self.clips.last())
    }
}

impl ColorPainter for PixmapPainter<'_> {
    fn push_transform(&mut self, transform: skrifa::color::Transform) {
        let next = self.current().pre_concat(convert(transform));
        self.transforms.push(next);
    }

    fn pop_transform(&mut self) {
        if self.transforms.len() > 1 {
            self.transforms.pop();
        }
    }

    fn push_clip_glyph(&mut self, glyph_id: GlyphId) {
        let path = self.glyph_path(glyph_id);
        self.push_clip_path(path);
    }

    fn push_clip_box(&mut self, clip_box: skrifa::raw::types::BoundingBox<f32>) {
        let rect = ts::Rect::from_ltrb(
            clip_box.x_min,
            clip_box.y_min,
            clip_box.x_max,
            clip_box.y_max,
        );
        self.push_clip_path(rect.map(ts::PathBuilder::from_rect));
    }

    fn pop_clip(&mut self) {
        self.clips.pop();
    }

    fn fill(&mut self, brush: Brush<'_>) {
        let Some(shader) = self.shader(&brush, None) else {
            return;
        };
        let paint = ts::Paint {
            shader,
            anti_alias: true,
            ..Default::default()
        };
        let Some(rect) = ts::Rect::from_xywh(0.0, 0.0, self.width as f32, self.height as f32)
        else {
            return;
        };
        let (target, clip) = self.target_and_clip();
        target.fill_rect(rect, &paint, ts::Transform::identity(), clip);
    }

    fn fill_glyph(
        &mut self,
        glyph_id: GlyphId,
        brush_transform: Option<skrifa::color::Transform>,
        brush: Brush<'_>,
    ) {
        // The common PaintGlyph{fill} shape, drawn directly against the glyph
        // path instead of materializing a one-shot clip mask.
        let Some(path) = self.glyph_path(glyph_id) else {
            return;
        };
        let Some(shader) = self.shader(&brush, brush_transform) else {
            return;
        };
        let paint = ts::Paint {
            shader,
            anti_alias: true,
            ..Default::default()
        };
        let transform = self.current();
        let (target, clip) = self.target_and_clip();
        target.fill_path(&path, &paint, ts::FillRule::Winding, transform, clip);
    }

    fn push_layer(&mut self, composite_mode: CompositeMode) {
        // Must always push: a skipped layer would unbalance the matching
        // pop_layer and composite an outer layer too early. The dims are
        // nonzero and capped (see MAX_EXTENT_FACTOR), so this cannot fail.
        let pixmap = ts::Pixmap::new(self.width, self.height).expect("nonzero, capped dims");
        self.layers.push((pixmap, composite_mode));
    }

    fn pop_layer(&mut self) {
        let Some((pixmap, mode)) = self.layers.pop() else {
            return;
        };
        let paint = ts::PixmapPaint {
            blend_mode: blend(mode),
            ..Default::default()
        };
        let (target, _) = self.target_and_clip();
        target.draw_pixmap(
            0,
            0,
            pixmap.as_ref(),
            &paint,
            ts::Transform::identity(),
            None,
        );
    }
}

impl PixmapPainter<'_> {
    /// Push the intersection of the current clip with `path` (in font units,
    /// mapped through the current transform). `None` — an empty outline —
    /// clips everything out.
    fn push_clip_path(&mut self, path: Option<ts::Path>) {
        let mut mask = match self.clips.last() {
            Some(m) => m.clone(),
            None => full_mask(self.width, self.height),
        };
        match path {
            Some(p) => mask.intersect_path(&p, ts::FillRule::Winding, true, self.current()),
            None => mask = ts::Mask::new(self.width, self.height).expect("nonzero dims"),
        }
        self.clips.push(mask);
    }
}

fn gradient_stops(stops: Vec<(f32, ts::Color)>) -> Vec<ts::GradientStop> {
    stops
        .into_iter()
        .map(|(offset, color)| ts::GradientStop::new(offset, color))
        .collect()
}

/// An all-covering mask, the identity for clip intersection.
fn full_mask(width: u32, height: u32) -> ts::Mask {
    let size = ts::IntSize::from_wh(width, height).expect("nonzero dims");
    ts::Mask::from_vec(vec![u8::MAX; (width * height) as usize], size).expect("sized to fit")
}

/// skrifa's affine (column-major names xx/yx/xy/yy/dx/dy) as tiny-skia's.
fn convert(t: skrifa::color::Transform) -> ts::Transform {
    ts::Transform::from_row(t.xx, t.yx, t.xy, t.yy, t.dx, t.dy)
}

fn spread(extend: skrifa::color::Extend) -> ts::SpreadMode {
    match extend {
        skrifa::color::Extend::Repeat => ts::SpreadMode::Repeat,
        skrifa::color::Extend::Reflect => ts::SpreadMode::Reflect,
        // Pad is the spec default; unknown values read as Pad too.
        _ => ts::SpreadMode::Pad,
    }
}

fn blend(mode: CompositeMode) -> ts::BlendMode {
    use CompositeMode as C;
    use ts::BlendMode as B;
    match mode {
        C::Clear => B::Clear,
        C::Src => B::Source,
        C::Dest => B::Destination,
        C::SrcOver => B::SourceOver,
        C::DestOver => B::DestinationOver,
        C::SrcIn => B::SourceIn,
        C::DestIn => B::DestinationIn,
        C::SrcOut => B::SourceOut,
        C::DestOut => B::DestinationOut,
        C::SrcAtop => B::SourceAtop,
        C::DestAtop => B::DestinationAtop,
        C::Xor => B::Xor,
        C::Plus => B::Plus,
        C::Screen => B::Screen,
        C::Overlay => B::Overlay,
        C::Darken => B::Darken,
        C::Lighten => B::Lighten,
        C::ColorDodge => B::ColorDodge,
        C::ColorBurn => B::ColorBurn,
        C::HardLight => B::HardLight,
        C::SoftLight => B::SoftLight,
        C::Difference => B::Difference,
        C::Exclusion => B::Exclusion,
        C::Multiply => B::Multiply,
        C::HslHue => B::Hue,
        C::HslSaturation => B::Saturation,
        C::HslColor => B::Color,
        C::HslLuminosity => B::Luminosity,
        _ => B::SourceOver,
    }
}

/// Adapts skrifa's outline callbacks onto a tiny-skia path builder.
struct PathPen(ts::PathBuilder);

impl OutlinePen for PathPen {
    fn move_to(&mut self, x: f32, y: f32) {
        self.0.move_to(x, y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.0.line_to(x, y);
    }

    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        self.0.quad_to(cx0, cy0, x, y);
    }

    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        self.0.cubic_to(cx0, cy0, cx1, cy1, x, y);
    }

    fn close(&mut self) {
        self.0.close();
    }
}
