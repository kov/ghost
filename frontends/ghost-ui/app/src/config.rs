//! File-only UI configuration: a small, hand-editable TOML read once at launch
//! from `$XDG_CONFIG_HOME/ghost/ui.toml`. Currently it selects a color scheme.
//!
//! Only [`load`](UiConfig::load) touches the filesystem; the scheme/theme mapping
//! is pure and unit-tested. Scheme ids match ghost-gtk's so the two frontends can
//! eventually share a config. Unknown sections/fields are ignored, so a file that
//! also carries (not-yet-read) `[font]`/`[window]` settings still loads.

use ghost_renderer::Theme;
use serde::Deserialize;

/// A built-in color scheme: foreground/background plus the 16 base ANSI colors.
struct Scheme {
    id: &'static str,
    fg: [u8; 3],
    bg: [u8; 3],
    palette: [[u8; 3]; 16],
}

// Shared palettes, copied verbatim from ghost-gtk (frontends/ghost-gtk/src/
// settings.rs) so a scheme renders identically in both frontends.
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

/// Built-in schemes, ids matching ghost-gtk. Keep ids stable: they're persisted.
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
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Colors {
    /// Scheme id; absent (or unknown) keeps the renderer's built-in default.
    scheme: Option<String>,
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
    /// the renderer's default theme.
    pub fn theme(&self) -> Theme {
        match self.colors.scheme.as_deref() {
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
        }
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
    fn unread_sections_do_not_break_loading() {
        // Forward-compat: a file carrying settings we don't consume yet must
        // still parse and apply the parts we do.
        let c =
            UiConfig::parse("[font]\nsize = 14.0\n\n[colors]\nscheme = \"tango-dark\"\n").unwrap();
        assert_eq!(c.theme().bg, [0x2e, 0x34, 0x36]);
    }
}
