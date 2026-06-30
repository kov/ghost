//! File-only UI configuration: a small, hand-editable TOML read once at launch
//! from `$XDG_CONFIG_HOME/ghost/ui.toml`. It selects a color scheme (`[colors]`),
//! a persisted font zoom (`[zoom]`), and the background opacity (`[window]`).
//!
//! Only [`load`](UiConfig::load) touches the filesystem; the scheme/theme mapping
//! is pure and unit-tested. Scheme ids are inherited from the retired ghost-gtk
//! frontend (kept stable because they're persisted). Unknown sections/fields are
//! ignored, so a file that also carries (not-yet-read) `[font]` settings still loads.

use ghost_renderer::Theme;
use serde::Deserialize;

/// A built-in color scheme: foreground/background plus the 16 base ANSI colors.
struct Scheme {
    id: &'static str,
    fg: [u8; 3],
    bg: [u8; 3],
    palette: [[u8; 3]; 16],
}

// Palettes copied verbatim from the retired ghost-gtk frontend, so these schemes
// render identically to how they did there.
#[rustfmt::skip]
const GNOME_PALETTE: [[u8; 3]; 16] = [
    [0x1e, 0x1e, 0x1e], [0xc0, 0x1c, 0x28], [0x26, 0xa2, 0x69], [0xa2, 0x73, 0x4c],
    [0x12, 0x48, 0x8b], [0xa3, 0x47, 0xba], [0x2a, 0xa1, 0xb3], [0xcf, 0xcf, 0xcf],
    [0x5d, 0x5d, 0x5d], [0xf6, 0x61, 0x51], [0x33, 0xd1, 0x7a], [0xe9, 0xad, 0x0c],
    [0x2a, 0x7b, 0xde], [0xc0, 0x61, 0xcb], [0x33, 0xc7, 0xde], [0xff, 0xff, 0xff],
];
#[rustfmt::skip]
const TANGO_PALETTE: [[u8; 3]; 16] = [
    [0x2e, 0x34, 0x36], [0xcc, 0x00, 0x00], [0x4e, 0x9a, 0x06], [0xc4, 0xa0, 0x00],
    [0x34, 0x65, 0xa4], [0x75, 0x50, 0x7b], [0x06, 0x98, 0x9a], [0xd3, 0xd7, 0xcf],
    [0x55, 0x57, 0x53], [0xef, 0x29, 0x29], [0x8a, 0xe2, 0x34], [0xfc, 0xe9, 0x4f],
    [0x72, 0x9f, 0xcf], [0xad, 0x7f, 0xa8], [0x34, 0xe2, 0xe2], [0xee, 0xee, 0xec],
];
#[rustfmt::skip]
const SOLARIZED_PALETTE: [[u8; 3]; 16] = [
    [0x07, 0x36, 0x42], [0xdc, 0x32, 0x2f], [0x85, 0x99, 0x00], [0xb5, 0x89, 0x00],
    [0x26, 0x8b, 0xd2], [0xd3, 0x36, 0x82], [0x2a, 0xa1, 0x98], [0xee, 0xe8, 0xd5],
    [0x00, 0x2b, 0x36], [0xcb, 0x4b, 0x16], [0x58, 0x6e, 0x75], [0x65, 0x7b, 0x83],
    [0x83, 0x94, 0x96], [0x6c, 0x71, 0xc4], [0x93, 0xa1, 0xa1], [0xfd, 0xf6, 0xe3],
];
#[rustfmt::skip]
const LINUX_PALETTE: [[u8; 3]; 16] = [
    [0x00, 0x00, 0x00], [0xaa, 0x00, 0x00], [0x00, 0xaa, 0x00], [0xaa, 0x55, 0x00],
    [0x00, 0x00, 0xaa], [0xaa, 0x00, 0xaa], [0x00, 0xaa, 0xaa], [0xaa, 0xaa, 0xaa],
    [0x55, 0x55, 0x55], [0xff, 0x55, 0x55], [0x55, 0xff, 0x55], [0xff, 0xff, 0x55],
    [0x55, 0x55, 0xff], [0xff, 0x55, 0xff], [0x55, 0xff, 0xff], [0xff, 0xff, 0xff],
];

/// Built-in schemes; ids inherited from ghost-gtk. Keep ids stable: they're persisted.
const SCHEMES: &[Scheme] = &[
    Scheme {
        id: "gnome-dark",
        fg: [0xff, 0xff, 0xff],
        bg: [0x1e, 0x1e, 0x1e],
        palette: GNOME_PALETTE,
    },
    Scheme {
        id: "gnome-light",
        fg: [0x1e, 0x1e, 0x1e],
        bg: [0xff, 0xff, 0xff],
        palette: GNOME_PALETTE,
    },
    Scheme {
        id: "tango-dark",
        fg: [0xd3, 0xd7, 0xcf],
        bg: [0x2e, 0x34, 0x36],
        palette: TANGO_PALETTE,
    },
    Scheme {
        id: "tango-light",
        fg: [0x2e, 0x34, 0x36],
        bg: [0xee, 0xee, 0xec],
        palette: TANGO_PALETTE,
    },
    Scheme {
        id: "solarized-dark",
        fg: [0x83, 0x94, 0x96],
        bg: [0x00, 0x2b, 0x36],
        palette: SOLARIZED_PALETTE,
    },
    Scheme {
        id: "solarized-light",
        fg: [0x65, 0x7b, 0x83],
        bg: [0xfd, 0xf6, 0xe3],
        palette: SOLARIZED_PALETTE,
    },
    Scheme {
        id: "linux-console",
        fg: [0xff, 0xff, 0xff],
        bg: [0x00, 0x00, 0x00],
        palette: LINUX_PALETTE,
    },
];

fn scheme_by_id(id: &str) -> Option<&'static Scheme> {
    SCHEMES.iter().find(|s| s.id == id)
}

/// The parsed `ui.toml`. Sections we don't read yet are ignored by serde.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    colors: Colors,
    zoom: Zoom,
    window: Window,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Colors {
    /// Scheme id; absent (or unknown) keeps the renderer's built-in default.
    scheme: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct Zoom {
    /// Persisted font zoom; the model clamps it to its own bounds on apply.
    scale: f64,
}

impl Default for Zoom {
    fn default() -> Self {
        Zoom { scale: 1.0 }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct Window {
    /// Background opacity, 0.0..=1.0 (clamped on apply). Only the default
    /// background goes translucent; SGR-coloured cells stay opaque. 1.0 = solid.
    opacity: f32,
}

impl Default for Window {
    fn default() -> Self {
        Window { opacity: 1.0 }
    }
}

impl UiConfig {
    /// Load `$XDG_CONFIG_HOME/ghost/ui.toml`. A missing file yields defaults; a
    /// malformed one is logged and ignored (never fatal).
    pub fn load() -> Self {
        let path = ghost_vt::paths::config_dir().join("ui.toml");
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text).unwrap_or_else(|e| {
                eprintln!("ghost-ui: ignoring {}: {e}", path.display());
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    /// The renderer theme this config selects. An absent or unknown scheme keeps
    /// the renderer's default theme; `[window] opacity` rides on top of either.
    pub fn theme(&self) -> Theme {
        let mut theme = match self.colors.scheme.as_deref() {
            None => Theme::default(),
            Some(id) => match scheme_by_id(id) {
                Some(s) => Theme {
                    fg: s.fg,
                    bg: s.bg,
                    palette: s.palette,
                    ..Theme::default() // keep the default selection tint
                },
                None => {
                    eprintln!("ghost-ui: unknown color scheme {id:?}, using the default");
                    Theme::default()
                }
            },
        };
        // f32::clamp passes NaN through, and a non-finite alpha would poison the
        // GPU clear, so a non-finite opacity falls back to fully opaque.
        theme.bg_alpha = if self.window.opacity.is_finite() {
            self.window.opacity.clamp(0.0, 1.0)
        } else {
            1.0
        };
        theme
    }

    /// The persisted font zoom (raw; the model clamps it to its bounds). 1.0
    /// when unset.
    pub fn zoom(&self) -> f32 {
        self.zoom.scale as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_or_empty_config_keeps_the_default_theme() {
        assert_eq!(UiConfig::default().theme().bg, Theme::default().bg);
        assert_eq!(UiConfig::parse("").unwrap().theme().bg, Theme::default().bg);
    }

    #[test]
    fn known_scheme_sets_fg_bg_and_palette() {
        let c = UiConfig::parse("[colors]\nscheme = \"solarized-dark\"\n").unwrap();
        let t = c.theme();
        assert_eq!(t.bg, [0x00, 0x2b, 0x36]);
        assert_eq!(t.fg, [0x83, 0x94, 0x96]);
        assert_eq!(t.palette[1], [0xdc, 0x32, 0x2f]); // solarized red
        assert_eq!(t.selection, Theme::default().selection); // unchanged
    }

    #[test]
    fn unknown_scheme_falls_back_to_the_default() {
        let c = UiConfig::parse("[colors]\nscheme = \"nope\"\n").unwrap();
        assert_eq!(c.theme().bg, Theme::default().bg);
    }

    #[test]
    fn zoom_scale_parses_and_defaults_to_one() {
        assert_eq!(UiConfig::default().zoom(), 1.0);
        assert_eq!(UiConfig::parse("").unwrap().zoom(), 1.0);
        let c = UiConfig::parse("[zoom]\nscale = 1.5\n").unwrap();
        assert_eq!(c.zoom(), 1.5);
        // A present [zoom] table without scale still defaults to 1.0.
        assert_eq!(UiConfig::parse("[zoom]\n").unwrap().zoom(), 1.0);
    }

    #[test]
    fn window_opacity_parses_clamps_and_defaults_to_one() {
        // Default, empty, and a present-but-empty [window] table are all opaque.
        assert_eq!(UiConfig::default().theme().bg_alpha, 1.0);
        assert_eq!(UiConfig::parse("").unwrap().theme().bg_alpha, 1.0);
        assert_eq!(UiConfig::parse("[window]\n").unwrap().theme().bg_alpha, 1.0);
        // A set value flows to the theme's clear alpha.
        let c = UiConfig::parse("[window]\nopacity = 0.5\n").unwrap();
        assert_eq!(c.theme().bg_alpha, 0.5);
        // Out-of-range opacity clamps into 0.0..=1.0.
        assert_eq!(
            UiConfig::parse("[window]\nopacity = 2.0\n")
                .unwrap()
                .theme()
                .bg_alpha,
            1.0
        );
        assert_eq!(
            UiConfig::parse("[window]\nopacity = -1.0\n")
                .unwrap()
                .theme()
                .bg_alpha,
            0.0
        );
        // Opacity is independent of the chosen colour scheme.
        let c = UiConfig::parse("[colors]\nscheme = \"tango-dark\"\n\n[window]\nopacity = 0.5\n")
            .unwrap();
        assert_eq!(c.theme().bg, [0x2e, 0x34, 0x36]);
        assert_eq!(c.theme().bg_alpha, 0.5);
        // A non-finite opacity (valid TOML, and f32::clamp passes NaN through)
        // must not poison the clear: fall back to fully opaque.
        let c = UiConfig::parse("[window]\nopacity = nan\n").unwrap();
        assert!(c.theme().bg_alpha.is_finite());
        assert_eq!(c.theme().bg_alpha, 1.0);
    }

    #[test]
    fn unread_sections_do_not_break_loading() {
        // Forward-compat: a file carrying settings we don't consume yet must
        // still parse and apply the parts we do.
        let c =
            UiConfig::parse("[font]\nsize = 14.0\n\n[colors]\nscheme = \"tango-dark\"\n").unwrap();
        assert_eq!(c.theme().bg, [0x2e, 0x34, 0x36]);
    }
}
