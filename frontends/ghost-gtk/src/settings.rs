//! User settings for `ghost-gtk`: a small, hand-editable TOML file plus the
//! built-in color schemes and the pure helpers (zoom stepping, cols×rows →
//! pixels) that the GUI applies. The GTK/VTE application lives in [`apply`]; the
//! data and math here are deliberately GTK-free so they can be unit-tested.

use std::path::{Path, PathBuf};

use gtk4::{gdk, pango};
use serde::{Deserialize, Serialize};
use vte4::Terminal;
use vte4::prelude::*;

/// Scheme selected when none is configured or the configured id is unknown.
pub const DEFAULT_SCHEME: &str = "gnome-dark";

/// Zoom (VTE font-scale) bounds and step for the Cmd/Ctrl +/- actions.
pub const ZOOM_MIN: f64 = 0.5;
pub const ZOOM_MAX: f64 = 3.0;
const ZOOM_STEP: f64 = 0.1;

// --- the settings document --------------------------------------------------

fn default_family() -> String {
    "Monospace".into()
}
fn default_size() -> f64 {
    12.0
}
fn default_scheme() -> String {
    DEFAULT_SCHEME.into()
}
fn default_columns() -> u16 {
    80
}
fn default_rows() -> u16 {
    24
}
fn default_scale() -> f64 {
    1.0
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FontCfg {
    /// Pango family; empty means the system default monospace.
    #[serde(default = "default_family")]
    pub family: String,
    /// Point size (zoom multiplies this live without changing it).
    #[serde(default = "default_size")]
    pub size: f64,
}
impl Default for FontCfg {
    fn default() -> Self {
        Self {
            family: default_family(),
            size: default_size(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ColorsCfg {
    /// Built-in scheme id (see [`SCHEMES`]).
    #[serde(default = "default_scheme")]
    pub scheme: String,
}
impl Default for ColorsCfg {
    fn default() -> Self {
        Self {
            scheme: default_scheme(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowCfg {
    #[serde(default = "default_columns")]
    pub columns: u16,
    #[serde(default = "default_rows")]
    pub rows: u16,
    /// 0.0 opaque … 1.0 fully transparent.
    #[serde(default)]
    pub transparency: f64,
}
impl Default for WindowCfg {
    fn default() -> Self {
        Self {
            columns: default_columns(),
            rows: default_rows(),
            transparency: 0.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ZoomCfg {
    /// Persisted VTE font-scale, clamped to [`ZOOM_MIN`]..=[`ZOOM_MAX`].
    #[serde(default = "default_scale")]
    pub scale: f64,
}
impl Default for ZoomCfg {
    fn default() -> Self {
        Self {
            scale: default_scale(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub font: FontCfg,
    pub colors: ColorsCfg,
    pub window: WindowCfg,
    pub zoom: ZoomCfg,
}

impl Settings {
    /// The on-disk config path: `$XDG_CONFIG_HOME/ghost/gtk.toml`.
    pub fn path() -> PathBuf {
        ghost_vt::paths::config_dir().join("gtk.toml")
    }

    /// Load from the default path, or defaults if it's missing/unreadable.
    pub fn load() -> Self {
        Self::load_from(&Self::path())
    }

    /// Save to the default path (creating the config dir).
    pub fn save(&self) -> std::io::Result<()> {
        self.save_to(&Self::path())
    }

    /// Load from `path`. A missing file, or any parse error, yields defaults
    /// (the latter logged) — settings are best-effort and never fatal. Values
    /// are normalized so the rest of the app can trust them.
    pub fn load_from(path: &Path) -> Self {
        let mut s = match std::fs::read_to_string(path) {
            Ok(txt) => toml::from_str(&txt).unwrap_or_else(|e| {
                eprintln!("ghost-gtk: ignoring invalid {}: {e}", path.display());
                Settings::default()
            }),
            Err(_) => Settings::default(),
        };
        s.normalize();
        s
    }

    /// Serialize to `path` as pretty TOML, creating the parent directory.
    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let txt = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, txt)
    }

    /// Clamp hand-edited values into supported ranges.
    fn normalize(&mut self) {
        self.window.transparency = self.window.transparency.clamp(0.0, 1.0);
        self.zoom.scale = self.zoom.scale.clamp(ZOOM_MIN, ZOOM_MAX);
        if self.font.size.is_nan() || self.font.size <= 0.0 {
            self.font.size = default_size();
        }
        self.window.columns = self.window.columns.max(1);
        self.window.rows = self.window.rows.max(1);
    }
}

// --- zoom + geometry math ---------------------------------------------------

fn step_zoom(scale: f64, delta: f64) -> f64 {
    // Round to one decimal so repeated steps stay on clean tenths (no drift).
    let next = ((scale + delta) * 10.0).round() / 10.0;
    next.clamp(ZOOM_MIN, ZOOM_MAX)
}

/// One zoom step larger, clamped to [`ZOOM_MAX`].
pub fn zoom_in(scale: f64) -> f64 {
    step_zoom(scale, ZOOM_STEP)
}

/// One zoom step smaller, clamped to [`ZOOM_MIN`].
pub fn zoom_out(scale: f64) -> f64 {
    step_zoom(scale, -ZOOM_STEP)
}

/// Pixels for a `cols`×`rows` grid given a cell's width/height — the terminal
/// area only; the caller adds chrome (header bar) before sizing the window.
pub fn window_pixels(cols: u16, rows: u16, char_w: i32, char_h: i32) -> (i32, i32) {
    (cols as i32 * char_w, rows as i32 * char_h)
}

// Non-linear transparency slider: the first 50% of travel covers only the first
// 30% of the transparency range, giving fine control over low transparency; the
// remaining 50% covers the upper 70%. The breakpoint is (slider 0.5, transp 0.3).
const SLIDER_BREAK: f64 = 0.5;
const TRANSP_BREAK: f64 = 0.3;

/// Map a slider position (0..=1) to a transparency value (0..=1).
pub fn slider_to_transparency(pos: f64) -> f64 {
    let pos = pos.clamp(0.0, 1.0);
    if pos <= SLIDER_BREAK {
        pos * (TRANSP_BREAK / SLIDER_BREAK)
    } else {
        TRANSP_BREAK + (pos - SLIDER_BREAK) * ((1.0 - TRANSP_BREAK) / (1.0 - SLIDER_BREAK))
    }
}

/// Inverse of [`slider_to_transparency`]: transparency (0..=1) → slider position.
pub fn transparency_to_slider(transparency: f64) -> f64 {
    let t = transparency.clamp(0.0, 1.0);
    if t <= TRANSP_BREAK {
        t * (SLIDER_BREAK / TRANSP_BREAK)
    } else {
        SLIDER_BREAK + (t - TRANSP_BREAK) * ((1.0 - SLIDER_BREAK) / (1.0 - TRANSP_BREAK))
    }
}

// --- color schemes ----------------------------------------------------------

/// A 24-bit color (no alpha; transparency is applied to the background only).
#[derive(Clone, Copy)]
pub struct Rgb(pub u8, pub u8, pub u8);

/// A named palette: default fg/bg plus the 16 ANSI colors.
pub struct Scheme {
    pub id: &'static str,
    pub name: &'static str,
    pub fg: Rgb,
    pub bg: Rgb,
    pub palette: [Rgb; 16],
}

/// Resolve a scheme id, falling back to [`DEFAULT_SCHEME`] for unknown ids.
pub fn scheme_by_id(id: &str) -> &'static Scheme {
    SCHEMES
        .iter()
        .find(|s| s.id == id)
        .or_else(|| SCHEMES.iter().find(|s| s.id == DEFAULT_SCHEME))
        .expect("DEFAULT_SCHEME is always present in SCHEMES")
}

// Shared palettes: GNOME, Tango, Solarized, and the VGA/Linux console.
// GNOME tracks current gnome-terminal (master): a neutral #1e1e1e black/bg
// replacing the older blue-tinted #171421, plus tamer 7/8/10.
const GNOME_PALETTE: [Rgb; 16] = [
    Rgb(0x1e, 0x1e, 0x1e),
    Rgb(0xc0, 0x1c, 0x28),
    Rgb(0x26, 0xa2, 0x69),
    Rgb(0xa2, 0x73, 0x4c),
    Rgb(0x12, 0x48, 0x8b),
    Rgb(0xa3, 0x47, 0xba),
    Rgb(0x2a, 0xa1, 0xb3),
    Rgb(0xcf, 0xcf, 0xcf),
    Rgb(0x5d, 0x5d, 0x5d),
    Rgb(0xf6, 0x61, 0x51),
    Rgb(0x33, 0xd1, 0x7a),
    Rgb(0xe9, 0xad, 0x0c),
    Rgb(0x2a, 0x7b, 0xde),
    Rgb(0xc0, 0x61, 0xcb),
    Rgb(0x33, 0xc7, 0xde),
    Rgb(0xff, 0xff, 0xff),
];
const TANGO_PALETTE: [Rgb; 16] = [
    Rgb(0x2e, 0x34, 0x36),
    Rgb(0xcc, 0x00, 0x00),
    Rgb(0x4e, 0x9a, 0x06),
    Rgb(0xc4, 0xa0, 0x00),
    Rgb(0x34, 0x65, 0xa4),
    Rgb(0x75, 0x50, 0x7b),
    Rgb(0x06, 0x98, 0x9a),
    Rgb(0xd3, 0xd7, 0xcf),
    Rgb(0x55, 0x57, 0x53),
    Rgb(0xef, 0x29, 0x29),
    Rgb(0x8a, 0xe2, 0x34),
    Rgb(0xfc, 0xe9, 0x4f),
    Rgb(0x72, 0x9f, 0xcf),
    Rgb(0xad, 0x7f, 0xa8),
    Rgb(0x34, 0xe2, 0xe2),
    Rgb(0xee, 0xee, 0xec),
];
const SOLARIZED_PALETTE: [Rgb; 16] = [
    Rgb(0x07, 0x36, 0x42),
    Rgb(0xdc, 0x32, 0x2f),
    Rgb(0x85, 0x99, 0x00),
    Rgb(0xb5, 0x89, 0x00),
    Rgb(0x26, 0x8b, 0xd2),
    Rgb(0xd3, 0x36, 0x82),
    Rgb(0x2a, 0xa1, 0x98),
    Rgb(0xee, 0xe8, 0xd5),
    Rgb(0x00, 0x2b, 0x36),
    Rgb(0xcb, 0x4b, 0x16),
    Rgb(0x58, 0x6e, 0x75),
    Rgb(0x65, 0x7b, 0x83),
    Rgb(0x83, 0x94, 0x96),
    Rgb(0x6c, 0x71, 0xc4),
    Rgb(0x93, 0xa1, 0xa1),
    Rgb(0xfd, 0xf6, 0xe3),
];
const LINUX_PALETTE: [Rgb; 16] = [
    Rgb(0x00, 0x00, 0x00),
    Rgb(0xaa, 0x00, 0x00),
    Rgb(0x00, 0xaa, 0x00),
    Rgb(0xaa, 0x55, 0x00),
    Rgb(0x00, 0x00, 0xaa),
    Rgb(0xaa, 0x00, 0xaa),
    Rgb(0x00, 0xaa, 0xaa),
    Rgb(0xaa, 0xaa, 0xaa),
    Rgb(0x55, 0x55, 0x55),
    Rgb(0xff, 0x55, 0x55),
    Rgb(0x55, 0xff, 0x55),
    Rgb(0xff, 0xff, 0x55),
    Rgb(0x55, 0x55, 0xff),
    Rgb(0xff, 0x55, 0xff),
    Rgb(0x55, 0xff, 0xff),
    Rgb(0xff, 0xff, 0xff),
];

/// The built-in schemes, in display order. Keep ids stable: they're persisted.
pub const SCHEMES: &[Scheme] = &[
    Scheme {
        id: "gnome-dark",
        name: "GNOME dark",
        fg: Rgb(0xff, 0xff, 0xff),
        bg: Rgb(0x1e, 0x1e, 0x1e),
        palette: GNOME_PALETTE,
    },
    Scheme {
        id: "gnome-light",
        name: "GNOME light",
        fg: Rgb(0x1e, 0x1e, 0x1e),
        bg: Rgb(0xff, 0xff, 0xff),
        palette: GNOME_PALETTE,
    },
    Scheme {
        id: "tango-dark",
        name: "Tango dark",
        fg: Rgb(0xd3, 0xd7, 0xcf),
        bg: Rgb(0x2e, 0x34, 0x36),
        palette: TANGO_PALETTE,
    },
    Scheme {
        id: "tango-light",
        name: "Tango light",
        fg: Rgb(0x2e, 0x34, 0x36),
        bg: Rgb(0xee, 0xee, 0xec),
        palette: TANGO_PALETTE,
    },
    Scheme {
        id: "solarized-dark",
        name: "Solarized dark",
        fg: Rgb(0x83, 0x94, 0x96),
        bg: Rgb(0x00, 0x2b, 0x36),
        palette: SOLARIZED_PALETTE,
    },
    Scheme {
        id: "solarized-light",
        name: "Solarized light",
        fg: Rgb(0x65, 0x7b, 0x83),
        bg: Rgb(0xfd, 0xf6, 0xe3),
        palette: SOLARIZED_PALETTE,
    },
    Scheme {
        id: "linux-console",
        name: "Linux console",
        fg: Rgb(0xff, 0xff, 0xff),
        bg: Rgb(0x00, 0x00, 0x00),
        palette: LINUX_PALETTE,
    },
];

// --- applying to a VTE terminal (GTK side; not unit-tested) -----------------

fn rgba(c: Rgb, alpha: f32) -> gdk::RGBA {
    gdk::RGBA::new(
        c.0 as f32 / 255.0,
        c.1 as f32 / 255.0,
        c.2 as f32 / 255.0,
        alpha,
    )
}

/// Apply the current settings (font, zoom, scheme, transparency) to one VTE
/// terminal. Called on every open terminal when settings change.
pub fn apply(settings: &Settings, term: &Terminal) {
    let mut fd = pango::FontDescription::new();
    if !settings.font.family.is_empty() {
        fd.set_family(&settings.font.family);
    }
    fd.set_size((settings.font.size * pango::SCALE as f64).round() as i32);
    term.set_font(Some(&fd));
    term.set_font_scale(settings.zoom.scale);

    let scheme = scheme_by_id(&settings.colors.scheme);
    // The background's alpha is the terminal's translucency: drawn over the
    // window's dropped background (see `update_window_chrome`) it reveals the
    // desktop. VTE paints the background by default, so no extra setup is needed.
    let bg_alpha = (1.0 - settings.window.transparency).clamp(0.0, 1.0) as f32;
    let palette: Vec<gdk::RGBA> = scheme.palette.iter().map(|c| rgba(*c, 1.0)).collect();
    let palette: Vec<&gdk::RGBA> = palette.iter().collect();
    term.set_colors(
        Some(&rgba(scheme.fg, 1.0)),
        Some(&rgba(scheme.bg, bg_alpha)),
        &palette,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn defaults_when_file_missing() {
        let dir = tmp();
        let s = Settings::load_from(&dir.path().join("absent.toml"));
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn round_trips_through_toml() {
        let dir = tmp();
        let path = dir.path().join("gtk.toml");
        let s = Settings {
            font: FontCfg {
                family: "JetBrains Mono".into(),
                size: 14.0,
            },
            colors: ColorsCfg {
                scheme: "solarized-dark".into(),
            },
            window: WindowCfg {
                columns: 120,
                rows: 40,
                transparency: 0.15,
            },
            zoom: ZoomCfg { scale: 1.3 },
        };
        s.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path), s);
    }

    #[test]
    fn malformed_toml_falls_back_to_defaults() {
        let dir = tmp();
        let path = dir.path().join("gtk.toml");
        std::fs::write(&path, "not = = valid [[[ toml").unwrap();
        assert_eq!(Settings::load_from(&path), Settings::default());
    }

    #[test]
    fn partial_toml_keeps_other_defaults() {
        let dir = tmp();
        let path = dir.path().join("gtk.toml");
        std::fs::write(&path, "[colors]\nscheme = \"tango-dark\"\n").unwrap();
        let s = Settings::load_from(&path);
        assert_eq!(s.colors.scheme, "tango-dark");
        // Everything outside [colors] keeps its default.
        assert_eq!(s.font.size, Settings::default().font.size);
        assert_eq!(s.window.columns, Settings::default().window.columns);
    }

    #[test]
    fn unknown_scheme_falls_back_to_default() {
        assert_eq!(scheme_by_id("does-not-exist").id, DEFAULT_SCHEME);
        assert_eq!(scheme_by_id("tango-dark").id, "tango-dark");
    }

    #[test]
    fn schemes_have_unique_ids_and_full_palettes() {
        let mut ids = std::collections::HashSet::new();
        for s in SCHEMES {
            assert!(ids.insert(s.id), "duplicate scheme id {}", s.id);
            assert_eq!(s.palette.len(), 16, "{} palette", s.id);
        }
        assert!(ids.contains(&DEFAULT_SCHEME), "default scheme must exist");
    }

    #[test]
    fn out_of_range_values_are_clamped_on_load() {
        let dir = tmp();
        let path = dir.path().join("gtk.toml");
        std::fs::write(
            &path,
            "[window]\ntransparency = 5.0\n[zoom]\nscale = 99.0\n[font]\nsize = -3.0\n",
        )
        .unwrap();
        let s = Settings::load_from(&path);
        assert!((0.0..=1.0).contains(&s.window.transparency));
        assert_eq!(s.zoom.scale, ZOOM_MAX);
        assert!(s.font.size > 0.0);
    }

    #[test]
    fn zoom_steps_and_clamps() {
        assert!((zoom_in(1.0) - 1.1).abs() < 1e-9);
        assert!((zoom_out(1.0) - 0.9).abs() < 1e-9);

        let mut z = 1.0;
        for _ in 0..100 {
            z = zoom_in(z);
        }
        assert_eq!(z, ZOOM_MAX);

        let mut z = 1.0;
        for _ in 0..100 {
            z = zoom_out(z);
        }
        assert_eq!(z, ZOOM_MIN);
    }

    #[test]
    fn transparency_slider_is_piecewise_linear() {
        // First half of slider travel covers only the first 30% of transparency,
        // for fine control over low transparency.
        assert!((slider_to_transparency(0.0)).abs() < 1e-9);
        assert!((slider_to_transparency(0.5) - 0.3).abs() < 1e-9);
        assert!((slider_to_transparency(1.0) - 1.0).abs() < 1e-9);
        // Quarter of the slider → 0.25 * 0.6 = 0.15.
        assert!((slider_to_transparency(0.25) - 0.15).abs() < 1e-9);

        // The inverse round-trips both directions.
        for t in [0.0, 0.1, 0.3, 0.5, 0.6, 0.9, 1.0] {
            let s = transparency_to_slider(t);
            assert!((slider_to_transparency(s) - t).abs() < 1e-9, "t={t}");
        }
        // Out-of-range inputs are clamped.
        assert_eq!(slider_to_transparency(2.0), 1.0);
        assert_eq!(transparency_to_slider(-1.0), 0.0);
    }

    #[test]
    fn window_pixels_is_cells_times_metrics() {
        assert_eq!(window_pixels(80, 24, 8, 16), (640, 384));
        assert_eq!(window_pixels(120, 40, 9, 18), (1080, 720));
    }
}
