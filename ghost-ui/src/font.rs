//! Resolve the configured font to bytes the shaper can parse, and the cell metrics
//! that face implies at the chosen size.
//!
//! An unset `[font] family` uses the bundled Fira Code (ligatures included). A named
//! family is resolved through fontconfig (Linux only — fontconfig is the Linux font
//! database); the match's file is read and leaked to `'static` so the one `FontRef`
//! per window can borrow it for the whole run, exactly as the bundled bytes are. Any
//! failure — fontconfig unavailable, no match, unreadable file, non-Linux — falls
//! back to the bundle with a log, never fatal.

use ghost_render::CellMetrics;

/// The bundled default face: Fira Code Regular (SIL OFL-1.1), which carries the
/// programming ligatures. Same asset the shaper's tests use.
const FIRA: &[u8] = include_bytes!("../../ghost-shaper/tests/assets/FiraCode-Regular.ttf");

/// The resolved font for the run: static bytes (bundled or leaked-from-disk), the
/// base glyph size, and the whole-pixel cell metrics derived from that face at that
/// size. Built once at launch and shared by every window.
pub struct FontSetup {
    pub bytes: &'static [u8],
    pub size: f32,
    pub metrics: CellMetrics,
}

impl FontSetup {
    /// Resolve `family` (fontconfig name, or `None` for the bundle) at `size` px.
    pub fn resolve(family: Option<&str>, size: f32) -> Self {
        let bytes = resolve_bytes(family);
        let font = ghost_shaper::font_from_bytes(bytes).expect("resolved font parses");
        let metrics = ghost_shaper::cell_metrics(font, size);
        FontSetup {
            bytes,
            size,
            metrics,
        }
    }
}

fn resolve_bytes(family: Option<&str>) -> &'static [u8] {
    let Some(family) = family else {
        return FIRA;
    };
    match load_family(family) {
        Some(bytes) => Box::leak(bytes.into_boxed_slice()),
        None => FIRA,
    }
}

#[cfg(target_os = "linux")]
fn load_family(family: &str) -> Option<Vec<u8>> {
    let Some(fc) = fontconfig::Fontconfig::new() else {
        eprintln!("ghost-ui: fontconfig unavailable; using bundled Fira Code");
        return None;
    };
    let font = match fc.find(family, None) {
        Ok(font) => font,
        Err(e) => {
            eprintln!("ghost-ui: no font for family {family:?} ({e}); using bundled Fira Code");
            return None;
        }
    };
    match std::fs::read(&font.path) {
        Ok(bytes) => {
            eprintln!(
                "ghost-ui: font family {family:?} -> {} ({})",
                font.name,
                font.path.display()
            );
            Some(bytes)
        }
        Err(e) => {
            eprintln!(
                "ghost-ui: reading {}: {e}; using bundled Fira Code",
                font.path.display()
            );
            None
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn load_family(_family: &str) -> Option<Vec<u8>> {
    eprintln!("ghost-ui: [font] family needs fontconfig (Linux only); using bundled Fira Code");
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_family_uses_the_bundled_font() {
        let setup = FontSetup::resolve(None, 15.0);
        assert_eq!(
            setup.bytes.len(),
            FIRA.len(),
            "unset family is the bundled Fira"
        );
        // The bundled Fira Code at 15px is the historic 9x18 cell.
        assert_eq!(setup.metrics.advance, 9.0);
        assert_eq!(setup.metrics.line_height, 18.0);
        assert_eq!(setup.size, 15.0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn resolving_a_family_yields_a_usable_font() {
        // fontconfig always matches something (a real family or its own fallback), so
        // this exercises the lookup → load → metrics path and must yield a usable face
        // at whole-pixel cell dimensions — never a panic.
        let setup = FontSetup::resolve(Some("monospace"), 15.0);
        assert!(setup.metrics.advance > 0.0 && setup.metrics.advance.fract() == 0.0);
        assert!(setup.metrics.line_height > 0.0);
        assert_eq!(setup.size, 15.0);
    }
}
