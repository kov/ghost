//! Draw the window title with ghost's own text stack.
//!
//! On Linux the titlebar belongs to the CSD frame winit draws (GNOME offers no
//! server-side decorations), and that frame's built-in text renderer takes one
//! font face, lays characters out one at a time with no shaping, and can only
//! draw outlines. So a title with a character that face lacks — CJK, Cyrillic,
//! box-drawing, an arrow, an emoji — rendered as `.notdef` or as nothing, and a
//! combining accent walked off its base.
//!
//! ghost already resolves fonts against the platform database, falls back per
//! character, shapes, and rasterizes color glyphs, because the terminal grid
//! needs all of it. This installs that stack into the frame's renderer seam
//! ([`sctk_adwaita::title::set_title_renderer`]), so the titlebar draws what the
//! terminal would.

use crate::font::{SystemFallback, resolve_face, style_weight};
use ghost_shaper::{FontRef, FontSet, TextStyle, paint_text};
use sctk_adwaita::title::{TitleFont, TitleImage, TitleRenderer};
use std::collections::HashMap;

/// Install ghost's text stack as the CSD frame's title renderer. Idempotent,
/// and must run before any window is created — a frame builds its renderer once
/// and keeps it. Only meaningful on Linux; elsewhere the platform draws its own
/// titlebar and this is never consulted.
pub fn install() {
    sctk_adwaita::title::set_title_renderer(renderer);
}

/// A fresh title renderer — what [`install`] hands the frame, exposed so a test
/// can drive it against a font of its choosing rather than the desktop's.
pub fn renderer() -> Box<dyn TitleRenderer> {
    Box::new(ChromeText::default())
}

/// Ghost's title-text renderer: the configured desktop title font resolved
/// through the platform font database, with per-character fallback for whatever
/// it doesn't cover.
#[derive(Default)]
struct ChromeText {
    /// (family, style) → the resolved face. The frame re-renders on every
    /// focus change and scale change, so resolving per render would shell out
    /// to the font database (and leak a face) each time.
    faces: HashMap<(String, Option<String>), Option<FontRef<'static>>>,
    fallback: SystemFallback,
}

impl ChromeText {
    /// The face for `font`, resolved once and cached. `None` when the platform
    /// has no font database or no match — the caller then draws nothing, which
    /// is what the frame did before this existed.
    fn face(&mut self, font: &TitleFont) -> Option<FontRef<'static>> {
        let key = (font.name.clone(), font.style.clone());
        if let Some(hit) = self.faces.get(&key) {
            return *hit;
        }
        let face = resolve_face(&font.name, font.style.as_deref());
        if face.is_none() {
            eprintln!(
                "ghost-ui: no font for titlebar family {:?}; the title will not be drawn",
                font.name
            );
        }
        self.faces.insert(key, face);
        face
    }
}

impl TitleRenderer for ChromeText {
    fn render(
        &mut self,
        title: &str,
        font: &TitleFont,
        size_px: f32,
        color: [f32; 4],
    ) -> Option<TitleImage> {
        // The desktop's style (`Cantarell Bold 11`) is resolved into the face
        // itself where the family ships a separate one; where it only offers
        // the weight as a variation axis, the style names it instead.
        let face = self.face(font)?;
        let style = TextStyle {
            weight: style_weight(font.style.as_deref()),
        };
        let image = paint_text(
            FontSet::single(face),
            &mut self.fallback,
            title,
            size_px,
            color,
            style,
        )?;
        Some(TitleImage {
            width: image.width,
            height: image.height,
            rgba: image.rgba,
        })
    }
}
