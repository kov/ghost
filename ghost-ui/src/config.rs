//! File-only UI configuration: a small, hand-editable TOML read once at launch
//! from `$XDG_CONFIG_HOME/ghost/ui.toml`. It selects a color scheme (`[colors]`),
//! a persisted font zoom (`[zoom]`), the background opacity, compositor blur and
//! self-drawn frost, initial grid size, and inner padding (`[window]`), the base
//! font size + family (`[font]`), and how the macOS Option key behaves
//! (`[input] option_as_meta`).
//!
//! Only [`load`](UiConfig::load) touches the filesystem; the scheme/theme mapping
//! is pure and unit-tested. Scheme ids are inherited from the retired ghost-gtk
//! frontend (kept stable because they're persisted). Unknown sections/fields are
//! ignored, so a file that carries settings a newer ghost added still loads here.

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

/// The built-in base glyph size (px) used when `[font] size` is unset. 15px is the
/// size the renderer's original hardcoded 9x18 Fira Code cell was measured at.
pub const DEFAULT_FONT_PX: f32 = 15.0;

/// Sane bounds for a configured font size; a value outside these clamps in.
const MIN_FONT_PX: f32 = 6.0;
const MAX_FONT_PX: f32 = 120.0;

/// The initial window grid when `[window] columns`/`rows` are unset — the historic
/// 80x24 the window opened at before it was configurable.
const DEFAULT_COLUMNS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;
/// Inner padding (logical px, per side) between the terminal grid and the window
/// edges when `[window] padding` is unset — a small, DPI-scaled breathing room so
/// the bottom line doesn't crowd the window's rounded corners. Filled with the
/// terminal background, so it blends rather than framing the content.
const DEFAULT_PADDING: f32 = 4.0;
/// Upper bound for the configured padding (logical px per side); a larger value
/// clamps in so a typo can't swallow the whole window.
const MAX_PADDING: f32 = 200.0;
/// Sane bounds for the configured initial grid; a value outside these clamps in
/// (never 0 — a zero-size grid has no cells — and not so large it asks the
/// compositor for an absurd window).
const MIN_GRID: u16 = 1;
const MAX_COLUMNS: u16 = 1000;
const MAX_ROWS: u16 = 1000;

/// The parsed `ui.toml`. Sections we don't read yet are ignored by serde.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    colors: Colors,
    zoom: Zoom,
    window: Window,
    font: Font,
    input: Input,
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
    /// Request compositor backdrop-blur behind the translucent background
    /// ("frosted glass"). Honoured on KDE/KWin (and other `org_kde_kwin_blur`
    /// compositors) and macOS; a no-op elsewhere. Only meaningful when `opacity`
    /// is below 1.0. Off by default.
    blur: bool,
    /// Frosted-glass density, 0.0..=1.0 (clamped on apply). Above 0, a smooth
    /// tinted glass fill is rendered into the see-through default-background areas
    /// — a self-drawn frosting that shows even where the compositor can't `blur`,
    /// dimming what's behind so it reads as glass. Only meaningful when `opacity`
    /// is below 1.0. Off (0.0) by default.
    frost: f32,
    /// Frost glass colour as a hex string (`"#rrggbb"`). Overrides the default,
    /// which derives the tint from the scheme background (dark scheme → dark glass).
    /// Only meaningful when `frost` is above 0. Unset by default.
    frost_tint: Option<String>,
    /// Initial window grid in character cells (clamped on apply). The window opens
    /// sized to hold this many columns/rows at the base font; it can be resized after.
    columns: u16,
    rows: u16,
    /// Inner padding in logical px per side between the terminal grid and the window
    /// edges (clamped on apply). DPI-scaled, filled with the terminal background.
    padding: f32,
}

impl Default for Window {
    fn default() -> Self {
        Window {
            opacity: 1.0,
            blur: false,
            frost: 0.0,
            frost_tint: None,
            columns: DEFAULT_COLUMNS,
            rows: DEFAULT_ROWS,
            padding: DEFAULT_PADDING,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct Font {
    /// Base glyph size in px, before zoom/DPI (clamped to a sane range on read).
    size: f32,
    /// fontconfig family name (e.g. "JetBrains Mono"); absent uses the bundled
    /// Fira Code. Resolved through fontconfig at launch — see the binary.
    family: Option<String>,
}

impl Default for Font {
    fn default() -> Self {
        Font {
            size: DEFAULT_FONT_PX,
            family: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct Input {
    /// macOS only: treat the Option (⌥) key as Meta, so Option+key sends an
    /// ESC-prefixed byte (Alt-b word motion, readline Meta bindings, …) instead
    /// of composing an accented character. On by default, matching a terminal's
    /// usual behaviour; set to `false` to type é/ü/£/… via Option. No effect off
    /// macOS, where Alt is already Meta.
    option_as_meta: bool,
}

impl Default for Input {
    fn default() -> Self {
        Input {
            option_as_meta: true,
        }
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

    pub(crate) fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    #[cfg(test)]
    fn frost_tint(&self) -> Option<[u8; 3]> {
        self.window.frost_tint.as_deref().and_then(parse_hex_rgb)
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
        // Frost rides on the same non-finite guard: a NaN must never reach the
        // blend as an intensity, so fall back to off.
        theme.frost = if self.window.frost.is_finite() {
            self.window.frost.clamp(0.0, 1.0)
        } else {
            0.0
        };
        // An explicit tint overrides the theme-derived default; a malformed hex is
        // ignored (`None` → the renderer derives the tint from the background).
        theme.frost_tint = self.window.frost_tint.as_deref().and_then(parse_hex_rgb);
        theme
    }

    /// Whether to request compositor backdrop-blur behind the translucent
    /// background. Off by default; a no-op on compositors without blur support.
    pub fn blur(&self) -> bool {
        self.window.blur
    }

    /// The persisted font zoom (raw; the model clamps it to its bounds). 1.0
    /// when unset.
    pub fn zoom(&self) -> f32 {
        self.zoom.scale as f32
    }

    /// The base glyph size in px, before zoom/DPI. Absent or out-of-range (or
    /// non-finite) falls back to / clamps into a sane range around the built-in.
    pub fn font_size(&self) -> f32 {
        if self.font.size.is_finite() {
            self.font.size.clamp(MIN_FONT_PX, MAX_FONT_PX)
        } else {
            DEFAULT_FONT_PX
        }
    }

    /// The configured fontconfig family name, or `None` to use the bundled font.
    pub fn font_family(&self) -> Option<&str> {
        self.font.family.as_deref()
    }

    /// Whether the macOS Option key acts as Meta (ESC-prefix) rather than
    /// composing accented characters. On by default; see [`Input`].
    pub fn option_as_meta(&self) -> bool {
        self.input.option_as_meta
    }

    /// The initial window grid in cells, clamped to a sane range (never 0). The
    /// window opens sized to hold this many columns/rows at the base font.
    pub fn columns(&self) -> u16 {
        self.window.columns.clamp(MIN_GRID, MAX_COLUMNS)
    }

    pub fn rows(&self) -> u16 {
        self.window.rows.clamp(MIN_GRID, MAX_ROWS)
    }

    /// Inner padding in logical px per side between the terminal grid and the window
    /// edges. Non-finite falls back to the default; otherwise clamped to a sane range
    /// (0 opts out). The shell scales this by the device factor and hands it to the
    /// model, which insets the grid and lets the terminal background fill the border.
    pub fn padding(&self) -> f32 {
        if self.window.padding.is_finite() {
            self.window.padding.clamp(0.0, MAX_PADDING)
        } else {
            DEFAULT_PADDING
        }
    }
}

/// Parse an `#rrggbb` (or bare `rrggbb`) hex colour into RGB bytes. `None` on any
/// malformed input, so a bad `frost_tint` silently falls back to the derived tint.
fn parse_hex_rgb(s: &str) -> Option<[u8; 3]> {
    let h = s.strip_prefix('#').unwrap_or(s);
    if h.len() != 6 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    Some([byte(0)?, byte(2)?, byte(4)?])
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
    fn window_columns_and_rows_parse_default_and_clamp() {
        // Defaults are the historic 80x24 window.
        assert_eq!(
            (UiConfig::default().columns(), UiConfig::default().rows()),
            (80, 24)
        );
        let empty = UiConfig::parse("").unwrap();
        assert_eq!((empty.columns(), empty.rows()), (80, 24));
        // Explicit values are honored (the gtk config's 100x40).
        let c = UiConfig::parse("[window]\ncolumns = 100\nrows = 40\n").unwrap();
        assert_eq!((c.columns(), c.rows()), (100, 40));
        // Zero clamps up to at least one cell (a zero-size grid has none).
        let z = UiConfig::parse("[window]\ncolumns = 0\nrows = 0\n").unwrap();
        assert!(z.columns() >= 1 && z.rows() >= 1);
        // An absurdly large grid clamps down so it can't ask for a monstrous window.
        let big = UiConfig::parse("[window]\ncolumns = 5000\nrows = 5000\n").unwrap();
        assert!(big.columns() <= MAX_COLUMNS && big.rows() <= MAX_ROWS);
        // opacity and the grid coexist in the same [window] table.
        let both = UiConfig::parse("[window]\nopacity = 0.9\ncolumns = 120\n").unwrap();
        assert_eq!(both.columns(), 120);
        assert_eq!(both.theme().bg_alpha, 0.9);
    }

    #[test]
    fn window_padding_parses_defaults_and_clamps() {
        // Default and empty both give the built-in breathing room.
        assert_eq!(UiConfig::default().padding(), DEFAULT_PADDING);
        assert_eq!(UiConfig::parse("").unwrap().padding(), DEFAULT_PADDING);
        // A present [window] table without padding keeps the default.
        assert_eq!(
            UiConfig::parse("[window]\n").unwrap().padding(),
            DEFAULT_PADDING
        );
        // A set value flows through.
        assert_eq!(
            UiConfig::parse("[window]\npadding = 12.0\n")
                .unwrap()
                .padding(),
            12.0
        );
        // Zero is honored (opt out of padding entirely).
        assert_eq!(
            UiConfig::parse("[window]\npadding = 0.0\n")
                .unwrap()
                .padding(),
            0.0
        );
        // Negative clamps to zero; an absurd value clamps to the cap.
        assert_eq!(
            UiConfig::parse("[window]\npadding = -5.0\n")
                .unwrap()
                .padding(),
            0.0
        );
        assert_eq!(
            UiConfig::parse("[window]\npadding = 9999.0\n")
                .unwrap()
                .padding(),
            MAX_PADDING
        );
        // Non-finite falls back to the default.
        let c = UiConfig::parse("[window]\npadding = nan\n").unwrap();
        assert_eq!(c.padding(), DEFAULT_PADDING);
        // Padding coexists with the rest of the [window] table.
        let both =
            UiConfig::parse("[window]\nopacity = 0.9\ncolumns = 120\npadding = 6.0\n").unwrap();
        assert_eq!(both.padding(), 6.0);
        assert_eq!(both.columns(), 120);
        assert_eq!(both.theme().bg_alpha, 0.9);
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
    fn option_as_meta_defaults_on_and_parses() {
        // Default, empty, and a present-but-empty [input] table all keep Option
        // acting as Meta (the terminal-friendly default).
        assert!(UiConfig::default().option_as_meta());
        assert!(UiConfig::parse("").unwrap().option_as_meta());
        assert!(UiConfig::parse("[input]\n").unwrap().option_as_meta());
        // An explicit opt-out lets Option compose accented characters again.
        assert!(
            !UiConfig::parse("[input]\noption_as_meta = false\n")
                .unwrap()
                .option_as_meta()
        );
        assert!(
            UiConfig::parse("[input]\noption_as_meta = true\n")
                .unwrap()
                .option_as_meta()
        );
    }

    #[test]
    fn window_blur_parses_and_defaults_off() {
        // Default, empty, and a present-but-empty [window] table request no blur.
        assert!(!UiConfig::default().blur());
        assert!(!UiConfig::parse("").unwrap().blur());
        assert!(!UiConfig::parse("[window]\n").unwrap().blur());
        // An explicit opt-in asks the compositor for backdrop blur.
        assert!(UiConfig::parse("[window]\nblur = true\n").unwrap().blur());
        assert!(!UiConfig::parse("[window]\nblur = false\n").unwrap().blur());
        // Blur coexists with the rest of the [window] table (it's meaningful
        // only alongside a translucent opacity, but parses independently).
        let both =
            UiConfig::parse("[window]\nopacity = 0.8\nblur = true\ncolumns = 120\n").unwrap();
        assert!(both.blur());
        assert_eq!(both.theme().bg_alpha, 0.8);
        assert_eq!(both.columns(), 120);
    }

    #[test]
    fn window_frost_parses_clamps_and_defaults_off() {
        // Default, empty, and a present-but-empty [window] table have no frost.
        assert_eq!(UiConfig::default().theme().frost, 0.0);
        assert_eq!(UiConfig::parse("").unwrap().theme().frost, 0.0);
        assert_eq!(UiConfig::parse("[window]\n").unwrap().theme().frost, 0.0);
        // A set value flows to the theme's frost intensity.
        assert_eq!(
            UiConfig::parse("[window]\nfrost = 0.3\n")
                .unwrap()
                .theme()
                .frost,
            0.3
        );
        // Out-of-range frost clamps into 0.0..=1.0.
        assert_eq!(
            UiConfig::parse("[window]\nfrost = 2.0\n")
                .unwrap()
                .theme()
                .frost,
            1.0
        );
        assert_eq!(
            UiConfig::parse("[window]\nfrost = -1.0\n")
                .unwrap()
                .theme()
                .frost,
            0.0
        );
        // A non-finite frost (valid TOML) falls back to off, never poisoning the pass.
        let c = UiConfig::parse("[window]\nfrost = nan\n").unwrap();
        assert!(c.theme().frost.is_finite());
        assert_eq!(c.theme().frost, 0.0);
        // Frost coexists with opacity/blur in the same table.
        let all = UiConfig::parse("[window]\nopacity = 0.8\nblur = true\nfrost = 0.2\n").unwrap();
        assert_eq!(all.theme().bg_alpha, 0.8);
        assert!(all.blur());
        assert_eq!(all.theme().frost, 0.2);
    }

    #[test]
    fn window_frost_tint_parses_and_defaults_to_derived() {
        // Unset by default → None, so the renderer derives the tint from the theme.
        assert_eq!(UiConfig::default().frost_tint(), None);
        assert_eq!(UiConfig::parse("[window]\n").unwrap().frost_tint(), None);
        assert_eq!(UiConfig::default().theme().frost_tint, None);
        // A hex string (with or without '#') parses to RGB and reaches the theme.
        let c = UiConfig::parse("[window]\nfrost_tint = \"#1a2b3c\"\n").unwrap();
        assert_eq!(c.frost_tint(), Some([0x1a, 0x2b, 0x3c]));
        assert_eq!(c.theme().frost_tint, Some([0x1a, 0x2b, 0x3c]));
        assert_eq!(
            UiConfig::parse("[window]\nfrost_tint = \"aabbcc\"\n")
                .unwrap()
                .frost_tint(),
            Some([0xaa, 0xbb, 0xcc])
        );
        // Malformed strings are ignored (fall back to the derived tint), never panic.
        for bad in ["#fff", "#12345g", "not-a-color", "#1234567"] {
            let cfg = UiConfig::parse(&format!("[window]\nfrost_tint = \"{bad}\"\n")).unwrap();
            assert_eq!(cfg.theme().frost_tint, None, "{bad:?} should not parse");
        }
    }

    #[test]
    fn unread_sections_do_not_break_loading() {
        // Forward-compat: a file carrying settings we don't consume yet must
        // still parse and apply the parts we do.
        let c = UiConfig::parse(
            "[keybindings]\nquit = \"ctrl+q\"\n\n[colors]\nscheme = \"tango-dark\"\n",
        )
        .unwrap();
        assert_eq!(c.theme().bg, [0x2e, 0x34, 0x36]);
    }

    #[test]
    fn font_size_parses_defaults_and_clamps() {
        assert_eq!(UiConfig::default().font_size(), DEFAULT_FONT_PX);
        assert_eq!(UiConfig::parse("").unwrap().font_size(), DEFAULT_FONT_PX);
        // A present [font] table without size keeps the default.
        assert_eq!(
            UiConfig::parse("[font]\n").unwrap().font_size(),
            DEFAULT_FONT_PX
        );
        // A set value flows through.
        assert_eq!(
            UiConfig::parse("[font]\nsize = 14.0\n")
                .unwrap()
                .font_size(),
            14.0
        );
        // Out-of-range clamps into the sane band; non-finite falls back to default.
        assert_eq!(
            UiConfig::parse("[font]\nsize = 0.0\n").unwrap().font_size(),
            MIN_FONT_PX
        );
        assert_eq!(
            UiConfig::parse("[font]\nsize = 999.0\n")
                .unwrap()
                .font_size(),
            MAX_FONT_PX
        );
        let c = UiConfig::parse("[font]\nsize = nan\n").unwrap();
        assert_eq!(c.font_size(), DEFAULT_FONT_PX);
    }

    #[test]
    fn font_family_parses_and_defaults_to_none() {
        assert_eq!(UiConfig::default().font_family(), None);
        assert_eq!(UiConfig::parse("").unwrap().font_family(), None);
        assert_eq!(UiConfig::parse("[font]\n").unwrap().font_family(), None);
        let c = UiConfig::parse("[font]\nfamily = \"JetBrains Mono\"\n").unwrap();
        assert_eq!(c.font_family(), Some("JetBrains Mono"));
        // Font settings are independent of the rest.
        let c = UiConfig::parse(
            "[font]\nsize = 13.0\nfamily = \"Fira Code\"\n\n[zoom]\nscale = 1.25\n",
        )
        .unwrap();
        assert_eq!(c.font_size(), 13.0);
        assert_eq!(c.font_family(), Some("Fira Code"));
        assert_eq!(c.zoom(), 1.25);
    }
}
