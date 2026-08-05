use tiny_skia::{Color, IntSize, Pixmap};

#[cfg(any(feature = "crossfont", feature = "ab_glyph"))]
mod config;
#[cfg(any(feature = "crossfont", feature = "ab_glyph"))]
mod font_preference;

#[cfg(feature = "crossfont")]
mod crossfont_renderer;

#[cfg(all(not(feature = "crossfont"), feature = "ab_glyph"))]
mod ab_glyph_renderer;

#[cfg(all(not(feature = "crossfont"), not(feature = "ab_glyph")))]
mod dumb;

mod hook;

pub use hook::{set_title_renderer, TitleFont, TitleImage, TitleRenderer};

#[derive(Debug)]
pub struct TitleText {
    imp: Imp,
}

#[derive(Debug)]
enum Imp {
    /// A renderer the application installed (see [`set_title_renderer`]).
    Hooked(hook::HookedTitleText),
    Builtin(Builtin),
}

#[cfg(feature = "crossfont")]
type Builtin = crossfont_renderer::CrossfontTitleText;
#[cfg(all(not(feature = "crossfont"), feature = "ab_glyph"))]
type Builtin = ab_glyph_renderer::AbGlyphTitleText;
#[cfg(all(not(feature = "crossfont"), not(feature = "ab_glyph")))]
type Builtin = dumb::DumbTitleText;

impl TitleText {
    pub fn new(color: Color) -> Option<Self> {
        // An installed renderer wins outright: the built-in ones would go and
        // mmap a font of their own just to be ignored.
        if let Some(hooked) = hook::HookedTitleText::new(color) {
            return Some(Self {
                imp: Imp::Hooked(hooked),
            });
        }

        #[cfg(feature = "crossfont")]
        return crossfont_renderer::CrossfontTitleText::new(color)
            .ok()
            .map(|imp| Self {
                imp: Imp::Builtin(imp),
            });

        #[cfg(all(not(feature = "crossfont"), feature = "ab_glyph"))]
        return Some(Self {
            imp: Imp::Builtin(ab_glyph_renderer::AbGlyphTitleText::new(color)),
        });

        #[cfg(all(not(feature = "crossfont"), not(feature = "ab_glyph")))]
        {
            let _ = color;
            return None;
        }
    }

    pub fn update_scale(&mut self, scale: u32) {
        match &mut self.imp {
            Imp::Hooked(imp) => imp.update_scale(scale),
            Imp::Builtin(imp) => imp.update_scale(scale),
        }
    }

    pub fn update_title(&mut self, title: impl Into<String>) {
        match &mut self.imp {
            Imp::Hooked(imp) => imp.update_title(title),
            Imp::Builtin(imp) => imp.update_title(title),
        }
    }

    pub fn update_color(&mut self, color: Color) {
        match &mut self.imp {
            Imp::Hooked(imp) => imp.update_color(color),
            Imp::Builtin(imp) => imp.update_color(color),
        }
    }

    pub fn pixmap(&self) -> Option<&Pixmap> {
        match &self.imp {
            Imp::Hooked(imp) => imp.pixmap(),
            Imp::Builtin(imp) => imp.pixmap(),
        }
    }
}

/// Build a pixmap from premultiplied RGBA, or `None` if the size is degenerate.
fn pixmap_from(image: TitleImage) -> Option<Pixmap> {
    let size = IntSize::from_wh(image.width, image.height)?;
    Pixmap::from_vec(image.rgba, size)
}
