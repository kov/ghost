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

/// The desktop's configured titlebar font: family, style and point size, as
/// GNOME states it (`Adwaita Sans Bold 11`).
#[derive(Debug, Clone, PartialEq)]
pub struct DesktopFont {
    pub family: String,
    pub style: Option<String>,
    pub pt_size: f32,
}

impl Default for DesktopFont {
    /// What to draw with when the desktop has no opinion (or no gsettings).
    fn default() -> Self {
        DesktopFont {
            family: "sans-serif".into(),
            style: None,
            pt_size: 11.0,
        }
    }
}

impl DesktopFont {
    /// Parse GNOME's `titlebar-font` form: a family, then an optional style,
    /// then an optional size — `Cantarell`, `Cantarell 12`, `Cantarell Bold 12`,
    /// `Noto Serif CJK HK Bold 12`. Only the last word can be the size and only
    /// the one before it can be the style, so a multi-word family survives.
    fn parse(spec: &str) -> Option<Self> {
        let spec = spec.trim().trim_matches('\'').trim();
        if spec.is_empty() {
            return None;
        }
        let mut words: Vec<&str> = spec.split_whitespace().collect();
        let pt_size = match words.last().and_then(|w| w.parse::<f32>().ok()) {
            Some(size) if words.len() > 1 => {
                words.pop();
                size
            }
            _ => Self::default().pt_size,
        };
        // A trailing style word, but never the only word — that is the family.
        let style = match words.last() {
            Some(w) if words.len() > 1 && crate::font::style_weight(Some(w)).is_some() => {
                Some(words.pop()?.to_string())
            }
            _ => None,
        };
        Some(DesktopFont {
            family: words.join(" "),
            style,
            pt_size,
        })
    }

    /// The em size in physical pixels at `scale`, from points at the usual 96 dpi.
    pub fn px_size(&self, scale: f32) -> f32 {
        self.pt_size * (96.0 / 72.0) * scale
    }
}

/// The desktop's titlebar font, asked once. `gsettings` is a subprocess, so this
/// must not sit on a resize's path; GNOME does not change it mid-session in
/// practice, and the CSD frame we are replacing reads it once for the same
/// reason.
pub fn desktop_font() -> DesktopFont {
    static FONT: std::sync::OnceLock<DesktopFont> = std::sync::OnceLock::new();
    FONT.get_or_init(|| {
        std::process::Command::new("gsettings")
            .args(["get", "org.gnome.desktop.wm.preferences", "titlebar-font"])
            .output()
            .ok()
            .and_then(|out| String::from_utf8(out.stdout).ok())
            .and_then(|s| DesktopFont::parse(&s))
            .unwrap_or_default()
    })
    .clone()
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_desktop_font_spec_keeps_multi_word_families() {
        // Only the last word can be the size and only the one before it can be a
        // style, so everything left is the family — `Noto Serif CJK HK` is one
        // family, not a family called `Noto` wearing three styles.
        let f = DesktopFont::parse("'Noto Serif CJK HK Bold 12'").expect("parses");
        assert_eq!(f.family, "Noto Serif CJK HK");
        assert_eq!(f.style.as_deref(), Some("Bold"));
        assert_eq!(f.pt_size, 12.0);
    }

    #[test]
    fn a_spec_may_leave_out_the_style_or_the_size() {
        let f = DesktopFont::parse("Cantarell 12").expect("parses");
        assert_eq!((f.family.as_str(), f.style.as_deref()), ("Cantarell", None));
        assert_eq!(f.pt_size, 12.0);

        let f = DesktopFont::parse("Cantarell").expect("parses");
        assert_eq!((f.family.as_str(), f.style.as_deref()), ("Cantarell", None));
        assert_eq!(f.pt_size, DesktopFont::default().pt_size);

        let f = DesktopFont::parse("Adwaita Sans Bold").expect("parses");
        assert_eq!(f.family, "Adwaita Sans");
        assert_eq!(f.style.as_deref(), Some("Bold"));
    }

    #[test]
    fn a_family_named_like_a_style_is_still_a_family() {
        // "Black" is a weight name, but a one-word spec is all family — dropping
        // it would leave us asking for a font with no name at all.
        let f = DesktopFont::parse("Black").expect("parses");
        assert_eq!(f.family, "Black");
        assert_eq!(f.style, None);
    }

    #[test]
    fn an_empty_or_unset_spec_has_no_opinion() {
        assert_eq!(DesktopFont::parse(""), None);
        assert_eq!(DesktopFont::parse("''"), None);
    }

    #[test]
    fn points_become_pixels_at_96dpi_and_scale() {
        let f = DesktopFont {
            pt_size: 12.0,
            ..DesktopFont::default()
        };
        assert_eq!(f.px_size(1.0), 16.0);
        assert_eq!(f.px_size(2.0), 32.0);
    }
}
