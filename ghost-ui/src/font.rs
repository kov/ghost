//! Resolve the configured font family into a [`FontSet`] — up to four real faces
//! (regular, bold, italic, bold-italic) — plus the cell metrics its regular face
//! implies at the chosen size.
//!
//! An unset `[font] family` uses the bundled Fira Code as a single face (bold and
//! italic synthesized). A named family is resolved through fontconfig (Linux only —
//! fontconfig is the Linux font database): we resolve each style and keep only the
//! faces fontconfig returns a *distinct* file for, because it always returns
//! something (falling back to a nearby face when the exact style is absent). Each
//! kept face's file is read and leaked to `'static`, exactly as the bundled bytes
//! are, so the per-window `FontSet` borrows them for the whole run. Any failure —
//! fontconfig unavailable, no match, unreadable file, non-Linux — falls back to the
//! bundle with a log, never fatal.

use ghost_render::CellMetrics;
use ghost_shaper::{FontRef, FontSet};

/// The bundled default face: Fira Code Regular (SIL OFL-1.1), which carries the
/// programming ligatures. Same asset the shaper's tests use.
const FIRA: &[u8] = include_bytes!("../../ghost-shaper/tests/assets/FiraCode-Regular.ttf");

/// The resolved faces for the run, the base glyph size, and the whole-pixel cell
/// metrics derived from the regular face at that size. Built once at launch and
/// shared by every window.
pub struct FontSetup {
    pub fonts: FontSet<'static>,
    pub size: f32,
    pub metrics: CellMetrics,
}

impl FontSetup {
    /// Resolve `family` (fontconfig name, or `None` for the bundle) at `size` px.
    pub fn resolve(family: Option<&str>, size: f32) -> Self {
        let fonts = resolve_faces(family);
        let metrics = ghost_shaper::cell_metrics(fonts.regular, size);
        FontSetup {
            fonts,
            size,
            metrics,
        }
    }
}

/// The bundled Fira Code as a single face (bold/italic synthesized).
fn bundled() -> FontSet<'static> {
    FontSet::single(ghost_shaper::font_from_bytes(FIRA).expect("bundled Fira Code parses"))
}

/// Read a font file and leak its bytes to `'static`, then parse the `idx`-th face.
fn load(path: &std::path::Path, idx: usize) -> Option<FontRef<'static>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("ghost-ui: reading {}: {e}", path.display());
            return None;
        }
    };
    let leaked: &'static [u8] = Box::leak(bytes.into_boxed_slice());
    ghost_shaper::font_from_index(leaked, idx)
}

#[cfg(not(target_os = "linux"))]
fn resolve_faces(family: Option<&str>) -> FontSet<'static> {
    if family.is_some() {
        eprintln!("ghost-ui: [font] family needs fontconfig (Linux only); using bundled Fira Code");
    }
    bundled()
}

#[cfg(target_os = "linux")]
fn resolve_faces(family: Option<&str>) -> FontSet<'static> {
    use std::path::PathBuf;

    let Some(family) = family else {
        return bundled();
    };
    let Some(fc) = fontconfig::Fontconfig::new() else {
        eprintln!("ghost-ui: fontconfig unavailable; using bundled Fira Code");
        return bundled();
    };

    // (file, face-index) for `family` in `style`, or None if fontconfig can't match.
    let query = |style: Option<&str>| -> Option<(PathBuf, usize)> {
        let font = fc.find(family, style).ok()?;
        Some((font.path, font.index.unwrap_or(0) as usize))
    };

    // Regular is the yardstick; without it, fall back to the bundle entirely.
    let Some((reg_path, reg_idx)) = query(None) else {
        eprintln!("ghost-ui: no font for family {family:?}; using bundled Fira Code");
        return bundled();
    };
    let Some(regular) = load(&reg_path, reg_idx) else {
        return bundled();
    };

    // Each style slot tries its aliases in order and takes the first file that differs
    // from every face already chosen — fontconfig always returns *something*, so a hit
    // equal to a reused path means it fell back to that face and this style is really
    // absent (`FontSet::face` will synthesize it). Slanted faces are styled "Italic" by
    // some families and "Oblique" by others (e.g. DejaVu), so we accept either.
    let resolve_slot = |styles: &[&str], reused: &[&PathBuf]| -> Option<(PathBuf, usize)> {
        styles.iter().find_map(|style| {
            let hit = query(Some(style))?;
            reused.iter().all(|p| **p != hit.0).then_some(hit)
        })
    };

    let bold_q = resolve_slot(&["Bold"], &[&reg_path]);
    let italic_q = resolve_slot(&["Italic", "Oblique"], &[&reg_path]);
    // Bold-italic must differ from regular AND from whatever bold/italic resolved to.
    let mut bi_reused = vec![&reg_path];
    if let Some((p, _)) = &bold_q {
        bi_reused.push(p);
    }
    if let Some((p, _)) = &italic_q {
        bi_reused.push(p);
    }
    let bold_italic_q = resolve_slot(&["Bold Italic", "Bold Oblique"], &bi_reused);

    let bold = bold_q.and_then(|(p, i)| load(&p, i));
    let italic = italic_q.and_then(|(p, i)| load(&p, i));
    let bold_italic = bold_italic_q.and_then(|(p, i)| load(&p, i));

    // A per-face summary: "real" = a distinct file fontconfig found, "synth" = none, so
    // the style is synthesized from the nearest face.
    let tag = |f: &Option<FontRef<'static>>| if f.is_some() { "real" } else { "synth" };
    eprintln!(
        "ghost-ui: font family {family:?} -> {} (bold {}, italic {}, bold-italic {})",
        reg_path.display(),
        tag(&bold),
        tag(&italic),
        tag(&bold_italic),
    );

    FontSet {
        regular,
        bold,
        italic,
        bold_italic,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_family_is_a_single_bundled_face() {
        let setup = FontSetup::resolve(None, 15.0);
        // The bundled Fira Code at 15px is the historic 9x18 cell.
        assert_eq!(setup.metrics.advance, 9.0);
        assert_eq!(setup.metrics.line_height, 18.0);
        assert_eq!(setup.size, 15.0);
        // Single face: bold/italic are synthesized, so no real slots.
        assert!(setup.fonts.bold.is_none());
        assert!(setup.fonts.italic.is_none());
        assert!(setup.fonts.bold_italic.is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn resolving_a_family_yields_a_usable_regular_face() {
        // fontconfig always matches something (a real family or its own fallback), so
        // this exercises the lookup → load → metrics path and must yield a usable
        // regular face at whole-pixel cell dimensions — never a panic.
        let setup = FontSetup::resolve(Some("monospace"), 15.0);
        assert!(setup.metrics.advance > 0.0 && setup.metrics.advance.fract() == 0.0);
        assert!(setup.metrics.line_height > 0.0);
        assert_eq!(setup.size, 15.0);
    }
}
