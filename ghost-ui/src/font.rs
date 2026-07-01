//! Resolve the configured font family into a [`FontSet`] — up to four real faces
//! (regular, bold, italic, bold-italic) — plus the cell metrics its regular face
//! implies at the chosen size.
//!
//! An unset `[font] family` uses the bundled Fira Code as a single face (bold and
//! italic synthesized). A named family is resolved through the platform's font
//! database — fontconfig on Linux, CoreText on macOS — into up to four real faces.
//! Both backends resolve each style and keep only the ones that map to a *distinct*
//! face, because both hand back *something* even for an absent style (a nearby face);
//! a repeat means "synthesize this one". They converge on a common shape
//! ([`ResolvedFaces`]): a `(file, index-in-file)` per slot, which [`load_and_assemble`]
//! reads, leaks to `'static` (exactly as the bundled bytes are, so the per-window
//! `FontSet` borrows them for the whole run), and turns into a `FontSet`. Any failure
//! — backend unavailable, no match, unreadable file, unsupported OS — falls back to
//! the bundle with a log, never fatal.

use ghost_render::CellMetrics;
use ghost_shaper::{FontRef, FontSet};
use std::collections::HashMap;
// File paths only appear in the family-resolution and fallback paths (Linux/macOS).
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::path::PathBuf;

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
/// Only the family-resolution paths use this, so it is gated to those targets to stay
/// `-D dead_code` clean where there is no resolution (e.g. Windows falls back to the
/// bundle without ever reading a file).
#[cfg(any(target_os = "linux", target_os = "macos"))]
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

/// The face files resolved for a family: the mandatory regular face plus the
/// bold/italic/bold-italic slots that resolved to a *distinct* real face (a repeat
/// means the style is absent and gets synthesized). Each entry is a file path and the
/// face index within it — 0 unless the file is a `.ttc` collection. Produced per
/// platform (fontconfig / CoreText) and consumed by [`load_and_assemble`].
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct ResolvedFaces {
    regular: (PathBuf, usize),
    bold: Option<(PathBuf, usize)>,
    italic: Option<(PathBuf, usize)>,
    bold_italic: Option<(PathBuf, usize)>,
}

/// Read the resolved face files into a [`FontSet`], logging a per-style real/synth
/// summary. Falls back to the bundled face if even the regular face can't be read.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn load_and_assemble(family: &str, faces: ResolvedFaces) -> FontSet<'static> {
    let Some(regular) = load(&faces.regular.0, faces.regular.1) else {
        return bundled();
    };
    let slot = |s: Option<(PathBuf, usize)>| s.and_then(|(p, i)| load(&p, i));
    let bold = slot(faces.bold);
    let italic = slot(faces.italic);
    let bold_italic = slot(faces.bold_italic);

    // "real" = a distinct face the backend found; "synth" = none, so the style is
    // synthesized from the nearest face we do have.
    let tag = |f: &Option<FontRef<'static>>| if f.is_some() { "real" } else { "synth" };
    eprintln!(
        "ghost-ui: font family {family:?} -> {} (bold {}, italic {}, bold-italic {})",
        faces.regular.0.display(),
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

// No fontconfig, no CoreText (Windows, the BSDs, …): a configured family can't be
// resolved, so serve the bundled face.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn resolve_faces(family: Option<&str>) -> FontSet<'static> {
    if family.is_some() {
        eprintln!(
            "ghost-ui: [font] family resolution is unsupported on this OS; using bundled Fira Code"
        );
    }
    bundled()
}

#[cfg(target_os = "linux")]
fn resolve_faces(family: Option<&str>) -> FontSet<'static> {
    let Some(family) = family else {
        return bundled();
    };
    match fontconfig_faces(family) {
        Some(faces) => load_and_assemble(family, faces),
        None => {
            eprintln!("ghost-ui: no font for family {family:?}; using bundled Fira Code");
            bundled()
        }
    }
}

/// Resolve `family` to face files through fontconfig, the Linux font database.
#[cfg(target_os = "linux")]
fn fontconfig_faces(family: &str) -> Option<ResolvedFaces> {
    let fc = fontconfig::Fontconfig::new().or_else(|| {
        eprintln!("ghost-ui: fontconfig unavailable; using bundled Fira Code");
        None
    })?;

    // (file, face-index) for `family` in `style`, or None if fontconfig can't match.
    let query = |style: Option<&str>| -> Option<(PathBuf, usize)> {
        let font = fc.find(family, style).ok()?;
        Some((font.path, font.index.unwrap_or(0) as usize))
    };

    // Regular is the yardstick; without it, there is nothing to resolve against.
    let (reg_path, reg_idx) = query(None)?;

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

    let bold = resolve_slot(&["Bold"], &[&reg_path]);
    let italic = resolve_slot(&["Italic", "Oblique"], &[&reg_path]);
    // Bold-italic must differ from regular AND from whatever bold/italic resolved to.
    let mut bi_reused = vec![&reg_path];
    if let Some((p, _)) = &bold {
        bi_reused.push(p);
    }
    if let Some((p, _)) = &italic {
        bi_reused.push(p);
    }
    let bold_italic = resolve_slot(&["Bold Italic", "Bold Oblique"], &bi_reused);

    Some(ResolvedFaces {
        regular: (reg_path, reg_idx),
        bold,
        italic,
        bold_italic,
    })
}

#[cfg(target_os = "macos")]
fn resolve_faces(family: Option<&str>) -> FontSet<'static> {
    let Some(family) = family else {
        return bundled();
    };
    match coretext_faces(family) {
        Some(faces) => load_and_assemble(family, faces),
        None => {
            eprintln!("ghost-ui: no font for family {family:?}; using bundled Fira Code");
            bundled()
        }
    }
}

/// The point size CoreText realizes descriptors at. Only the resolved file URL and
/// face names matter here, never the size, so any nonzero value works.
#[cfg(target_os = "macos")]
const CORETEXT_RESOLVE_PT: f64 = 16.0;

/// Resolve `family` to face files through CoreText, macOS's font database. `None`
/// when the family isn't installed — CoreText substitutes a system font for an
/// unknown name, so we verify the realized family — or has no readable file.
#[cfg(target_os = "macos")]
fn coretext_faces(family: &str) -> Option<ResolvedFaces> {
    use core_text::font;
    use core_text::font_descriptor::{kCTFontBoldTrait, kCTFontItalicTrait};

    // A face's (file, PostScript name, index-in-file). CoreText hands us the file URL
    // and the matched face's PostScript name; the shaper recovers the face's index
    // within a `.ttc` from that name (macOS system fonts are collections). `None` if
    // the font has no on-disk file (e.g. a system font baked into the shared cache).
    let locate = |f: &font::CTFont| -> Option<(PathBuf, String, usize)> {
        let path = f.copy_descriptor().font_path()?;
        let ps = f.postscript_name();
        let idx = ghost_shaper::face_index_by_postscript(&std::fs::read(&path).ok()?, &ps)?;
        Some((path, ps, idx))
    };

    // CTFontCreateWithName substitutes a system font for an unknown family, so trust
    // the result only when the realized family actually matches what was asked for.
    let base = font::new_from_name(family, CORETEXT_RESOLVE_PT).ok()?;
    if !base.family_name().eq_ignore_ascii_case(family) {
        return None;
    }
    let (reg_path, reg_ps, reg_idx) = locate(&base)?;

    // Ask CoreText for each styled variant. It returns a distinct real face when the
    // family ships one, and either nothing or the same face when it doesn't — in which
    // case the style is synthesized. This mirrors the Linux distinct-file check, keyed
    // on the PostScript name: a `.ttc` shares one file across styles, so the *name*,
    // not the path, is a face's identity.
    let styled = |value| locate(&base.clone_with_symbolic_traits(value, value)?);
    let bold_s = styled(kCTFontBoldTrait);
    let italic_s = styled(kCTFontItalicTrait);
    let bold_italic_s = styled(kCTFontBoldTrait | kCTFontItalicTrait);

    // Keep a styled slot only when its PostScript name differs from every face already
    // chosen (an equal name is CoreText handing back a face we already have).
    let distinct = |cand: &Option<(PathBuf, String, usize)>, taken: &[&str]| {
        cand.as_ref()
            .filter(|(_, ps, _)| !taken.contains(&ps.as_str()))
            .map(|(p, _, i)| (p.clone(), *i))
    };
    let bold = distinct(&bold_s, &[reg_ps.as_str()]);
    let italic = distinct(&italic_s, &[reg_ps.as_str()]);
    // Bold-italic must differ from regular AND from whatever bold/italic resolved to.
    let mut bi_taken = vec![reg_ps.as_str()];
    if let Some((_, ps, _)) = &bold_s {
        bi_taken.push(ps);
    }
    if let Some((_, ps, _)) = &italic_s {
        bi_taken.push(ps);
    }
    let bold_italic = distinct(&bold_italic_s, &bi_taken);

    Some(ResolvedFaces {
        regular: (reg_path, reg_idx),
        bold,
        italic,
        bold_italic,
    })
}

/// Runtime per-character font fallback backed by the platform font database. When the
/// configured family has no glyph for a character (`.notdef`), the renderer asks this
/// for a font that *does* cover it — so symbols, box-drawing, arrows, etc. outside the
/// primary font render instead of the tofu box. Each character's lookup is cached, and
/// loaded files are deduplicated, so a screen full of the same symbol queries the font
/// DB (and reads a file) only once. Faces are read and leaked to `'static`, exactly as
/// the family faces are.
///
/// Linux resolves through fontconfig (a charset match). macOS/other platforms have no
/// resolver yet, so `face_for` returns `None` there and the `.notdef` shows as before —
/// a deliberate v1 scope (see the follow-up note on CoreText fallback + colour emoji).
pub struct SystemFallback {
    /// char → the face that covers it (`None` = looked up, nothing found: don't re-query).
    cache: HashMap<char, Option<FontRef<'static>>>,
    /// (file, face-index) → the leaked face, so two chars from the same fallback file
    /// share one `FontRef` instead of leaking the bytes twice.
    #[cfg(target_os = "linux")]
    files: HashMap<(PathBuf, usize), Option<FontRef<'static>>>,
    #[cfg(target_os = "linux")]
    fc: Option<fontconfig::Fontconfig>,
}

impl Default for SystemFallback {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemFallback {
    pub fn new() -> Self {
        SystemFallback {
            cache: HashMap::new(),
            #[cfg(target_os = "linux")]
            files: HashMap::new(),
            #[cfg(target_os = "linux")]
            fc: fontconfig::Fontconfig::new(),
        }
    }

    /// Resolve a face covering `ch` (uncached). Linux: a fontconfig charset match,
    /// deduplicating the loaded file. Elsewhere: none yet.
    #[cfg(target_os = "linux")]
    fn resolve(&mut self, ch: char) -> Option<FontRef<'static>> {
        let key = self.query_fontconfig(ch)?;
        if let Some(hit) = self.files.get(&key) {
            return *hit;
        }
        let face = load(&key.0, key.1);
        self.files.insert(key, face);
        face
    }

    /// The (file, face-index) fontconfig picks as the best match that covers `ch`.
    /// fontconfig may still hand back a non-covering font when nothing covers it; the
    /// renderer re-checks coverage before using the glyph, so a miss degrades to
    /// `.notdef` rather than a wrong glyph.
    #[cfg(target_os = "linux")]
    fn query_fontconfig(&self, ch: char) -> Option<(PathBuf, usize)> {
        let fc = self.fc.as_ref()?;
        let mut pat = fontconfig::Pattern::new(fc).ok()?;
        let mut charset = fontconfig::CharSet::new(fc).ok()?;
        charset.add_char(ch).ok()?;
        pat.add_charset(charset).ok()?;
        let matched = pat.font_match().ok()?;
        let path = PathBuf::from(matched.filename().ok()?);
        let idx = matched.face_index().unwrap_or(0).max(0) as usize;
        Some((path, idx))
    }

    #[cfg(not(target_os = "linux"))]
    fn resolve(&mut self, _ch: char) -> Option<FontRef<'static>> {
        None
    }
}

impl ghost_shaper::Fallback for SystemFallback {
    fn face_for(&mut self, ch: char) -> Option<FontRef<'static>> {
        if let Some(hit) = self.cache.get(&ch) {
            return *hit;
        }
        let face = self.resolve(ch);
        self.cache.insert(ch, face);
        face
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

    #[cfg(target_os = "linux")]
    #[test]
    fn system_fallback_finds_a_face_covering_a_symbol_outside_coding_fonts() {
        use ghost_shaper::Fallback;
        // ★ (U+2605) is absent from Fira Code but present in DejaVu/Symbola, near-
        // universal on a desktop. The resolver must return a face that ACTUALLY covers
        // it (fontconfig can hand back a non-covering best-effort match, which we reject
        // downstream), and the second lookup must hit the cache — the same face.
        let mut fb = SystemFallback::new();
        let face = fb.face_for('★').expect("a system font should cover ★");
        assert!(
            ghost_shaper::covers(face, '★'),
            "the resolved fallback face must actually have ★"
        );
        let again = fb.face_for('★').expect("cached lookup");
        assert_eq!(
            face.key.value(),
            again.key.value(),
            "a repeat lookup must return the cached face, not re-resolve"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn resolving_a_system_family_yields_real_bold_and_italic() {
        // Menlo ships on every macOS as a single `.ttc` carrying distinct Regular,
        // Bold, Italic, and Bold-Italic faces. CoreText resolution must return each
        // as a *real* face (not a synthesized one), which exercises the whole path:
        // family+trait lookup → file URL → recovering the right index within the
        // collection → load → metrics. A bundled-only fallback would leave the
        // bold/italic slots `None`, so this pins that macOS gets real faces.
        let setup = FontSetup::resolve(Some("Menlo"), 15.0);
        assert!(
            setup.fonts.bold.is_some(),
            "Menlo bold should be a real face"
        );
        assert!(
            setup.fonts.italic.is_some(),
            "Menlo italic should be a real face"
        );
        assert!(
            setup.fonts.bold_italic.is_some(),
            "Menlo bold-italic should be a real face"
        );
        assert!(setup.metrics.advance > 0.0 && setup.metrics.advance.fract() == 0.0);
        assert!(setup.metrics.line_height > 0.0);
        assert_eq!(setup.size, 15.0);
    }
}
