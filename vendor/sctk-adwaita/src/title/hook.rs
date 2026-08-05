//! A seam for the application to draw the title text itself.
//!
//! The built-in renderers rasterize the title from a *single* font face with no
//! shaping (`ab_glyph`) or a fontconfig-matched one (`crossfont`). Neither can
//! fall back per character, so any codepoint the matched face happens to lack —
//! CJK, most symbols, anything outside the Latin range of a UI font — comes out
//! as `.notdef` or nothing at all, and neither draws color glyphs. An
//! application that already owns a real text stack can install it here and get
//! its own titles.
//!
//! The interface is deliberately font-library-agnostic: premultiplied RGBA
//! bytes in, nothing of ours out.

use std::fmt;
use std::sync::OnceLock;
use tiny_skia::{Color, Pixmap};

/// The title font the desktop asked for, as parsed from its configuration
/// (GNOME's `titlebar-font`, e.g. `Cantarell Bold 11`). A renderer resolves
/// this against its own font database.
#[derive(Debug, Clone)]
pub struct TitleFont {
    /// Family name, e.g. `Cantarell`. `sans-serif` when unconfigured.
    pub name: String,
    /// Style, e.g. `Bold`, when the configuration names one.
    pub style: Option<String>,
    /// Size in points, before the surface scale is applied.
    pub pt_size: f32,
}

/// A painted title: premultiplied RGBA, row-major, `width * height * 4` bytes.
pub struct TitleImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// An application-supplied title-text renderer.
pub trait TitleRenderer: Send {
    /// Paint `title` at `size_px` (em size, surface scale already applied) in
    /// `color` (straight-alpha RGBA, each channel `0.0..=1.0`). `None` for a
    /// title with nothing to draw.
    fn render(
        &mut self,
        title: &str,
        font: &TitleFont,
        size_px: f32,
        color: [f32; 4],
    ) -> Option<TitleImage>;
}

static FACTORY: OnceLock<fn() -> Box<dyn TitleRenderer>> = OnceLock::new();

/// Install `factory` as the source of title-text renderers; every frame created
/// afterwards builds its renderer from it instead of using a built-in one.
///
/// Call before creating any window. Only the first call takes effect.
pub fn set_title_renderer(factory: fn() -> Box<dyn TitleRenderer>) {
    let _ = FACTORY.set(factory);
}

/// Points to pixels at the usual 96 dpi, times the surface scale.
fn px_size(pt_size: f32, scale: u32) -> f32 {
    pt_size * (96.0 / 72.0) * scale as f32
}

/// The [`TitleText`](super::TitleText) backend that drives an installed
/// renderer, holding the state it must be re-rendered against.
pub(super) struct HookedTitleText {
    renderer: Box<dyn TitleRenderer>,
    font: TitleFont,
    title: String,
    scale: u32,
    color: Color,
    pixmap: Option<Pixmap>,
}

impl fmt::Debug for HookedTitleText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HookedTitleText")
            .field("font", &self.font)
            .field("title", &self.title)
            .field("scale", &self.scale)
            .finish_non_exhaustive()
    }
}

impl HookedTitleText {
    /// `None` when no renderer was installed — the caller then falls back to a
    /// built-in one.
    pub(super) fn new(color: Color) -> Option<Self> {
        let renderer = (*FACTORY.get()?)();
        Some(Self {
            renderer,
            font: title_font(),
            title: String::new(),
            scale: 1,
            color,
            pixmap: None,
        })
    }

    pub(super) fn update_scale(&mut self, scale: u32) {
        if self.scale != scale {
            self.scale = scale;
            self.rerender();
        }
    }

    pub(super) fn update_title(&mut self, title: impl Into<String>) {
        let title = title.into();
        if self.title != title {
            self.title = title;
            self.rerender();
        }
    }

    pub(super) fn update_color(&mut self, color: Color) {
        if self.color != color {
            self.color = color;
            self.rerender();
        }
    }

    pub(super) fn pixmap(&self) -> Option<&Pixmap> {
        self.pixmap.as_ref()
    }

    fn rerender(&mut self) {
        let color = [
            self.color.red(),
            self.color.green(),
            self.color.blue(),
            self.color.alpha(),
        ];
        let size = px_size(self.font.pt_size, self.scale);
        self.pixmap = self
            .renderer
            .render(&self.title, &self.font, size, color)
            .and_then(super::pixmap_from);
    }
}

/// The desktop's configured title font, or a sensible default.
fn title_font() -> TitleFont {
    #[cfg(any(feature = "crossfont", feature = "ab_glyph"))]
    {
        let pref = super::config::titlebar_font().unwrap_or_default();
        return TitleFont {
            name: pref.name,
            style: pref.style,
            pt_size: pref.pt_size,
        };
    }
    #[cfg(not(any(feature = "crossfont", feature = "ab_glyph")))]
    TitleFont {
        name: "sans-serif".into(),
        style: None,
        pt_size: 10.0,
    }
}
