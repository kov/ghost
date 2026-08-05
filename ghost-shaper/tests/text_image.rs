//! Painting a *proportional* string into a CPU bitmap — the path window chrome
//! draws its text through (the titlebar, today via the CSD frame).
//!
//! Everything the terminal grid never needed lives here: per-character font
//! fallback inside one string, real shaping so combining marks land on their
//! base instead of consuming their own cell, and color glyphs for emoji. The
//! assertions are on the *painted pixels*, because that is the thing that was
//! wrong on screen.
//!
//! Fixtures (see `assets/README.md`): Fira Code (no ★), DejaVu Sans Mono (has
//! ★, and carries combining diacritics), and a two-glyph COLRv1 Noto Color
//! Emoji subset.

use ghost_shaper::{
    Fallback, FontRef, FontSet, Synthesis, TextImage, TextStyle, covers, font_from_bytes, glyph_id,
    paint_text, rasterize,
};

const FIRA: &[u8] = include_bytes!("assets/FiraCode-Regular.ttf");
const DEJAVU: &[u8] = include_bytes!("assets/DejaVuSansMono.ttf");
const EMOJI: &[u8] = include_bytes!("assets/NotoColorEmoji-COLRv1-subset.ttf");

const SIZE: f32 = 20.0;
const BLACK: [f32; 4] = [0.0, 0.0, 0.0, 1.0];
/// No variable-font axis settings: the fixtures are all static faces.
const PLAIN: TextStyle = TextStyle { weight: None };

fn fira() -> FontRef<'static> {
    font_from_bytes(FIRA).expect("parse bundled Fira Code")
}

fn dejavu() -> FontRef<'static> {
    font_from_bytes(DEJAVU).expect("parse bundled DejaVu Sans Mono")
}

fn emoji() -> FontRef<'static> {
    font_from_bytes(EMOJI).expect("parse bundled Noto Color Emoji subset")
}

/// The stand-in for the platform font database: hands back the first bundled
/// face that actually covers the character, exactly as fontconfig/CoreText do
/// at runtime — without depending on what fonts this machine has.
struct Bundled(Vec<FontRef<'static>>);

impl Fallback for Bundled {
    fn face_for(&mut self, ch: char) -> Option<FontRef<'static>> {
        self.0.iter().copied().find(|f| covers(*f, ch))
    }
}

/// A fallback that never resolves anything — the pre-fix behaviour, where an
/// uncovered char could only ever be the primary face's `.notdef`.
struct NoFallback;

impl Fallback for NoFallback {
    fn face_for(&mut self, _ch: char) -> Option<FontRef<'static>> {
        None
    }
}

/// Straight-alpha channels of one pixel, un-premultiplying so a color test can
/// talk about hue without the alpha folded in.
fn pixel(img: &TextImage, x: u32, y: u32) -> (u8, u8, u8, u8) {
    let i = ((y * img.width + x) * 4) as usize;
    let p = &img.rgba[i..i + 4];
    let a = p[3];
    if a == 0 {
        return (0, 0, 0, 0);
    }
    let un = |c: u8| ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8;
    (un(p[0]), un(p[1]), un(p[2]), a)
}

/// Every pixel with any ink in it.
fn inked(img: &TextImage) -> Vec<(u32, u32)> {
    (0..img.height)
        .flat_map(|y| (0..img.width).map(move |x| (x, y)))
        .filter(|&(x, y)| pixel(img, x, y).3 > 0)
        .collect()
}

fn ink_count(img: &TextImage) -> usize {
    inked(img).len()
}

/// The topmost row carrying ink.
fn ink_top(img: &TextImage) -> u32 {
    inked(img).iter().map(|&(_, y)| y).min().expect("some ink")
}

#[test]
fn a_char_the_primary_face_lacks_is_drawn_from_the_fallback_face() {
    // ★ is absent from Fira Code. Painted through a fallback, the title must
    // carry DejaVu's real star — as much ink as rasterizing that glyph
    // directly — not the primary's `.notdef`.
    let mut fb = Bundled(vec![dejavu()]);
    let img = paint_text(FontSet::single(fira()), &mut fb, "★", SIZE, BLACK, PLAIN)
        .expect("a star paints to something");

    let star = rasterize(
        dejavu(),
        glyph_id(dejavu(), '★'),
        SIZE,
        Synthesis::default(),
    )
    .expect("DejaVu's star has an outline");
    let direct = star.coverage.iter().filter(|&&c| c > 0).count();

    assert_eq!(
        ink_count(&img),
        direct,
        "the star must come from the fallback face, glyph-for-glyph"
    );
}

#[test]
fn without_a_fallback_the_same_char_is_not_the_real_glyph() {
    // The pre-fix path, pinned so the test above is testing something: with no
    // fallback there is no star to draw — whatever `.notdef` Fira Code has
    // cannot be DejaVu's glyph.
    let mut fb = Bundled(vec![dejavu()]);
    let with = paint_text(FontSet::single(fira()), &mut fb, "★", SIZE, BLACK, PLAIN);
    let without = paint_text(
        FontSet::single(fira()),
        &mut NoFallback,
        "★",
        SIZE,
        BLACK,
        PLAIN,
    );

    let with = ink_count(&with.expect("fallback paints the star"));
    let without = without.map_or(0, |i| ink_count(&i));
    assert_ne!(with, without, "the fallback must change what is drawn");
}

#[test]
fn a_combining_mark_sits_on_its_base_instead_of_taking_its_own_advance() {
    // "e" + U+0301 (combining acute) is one glyph cluster: shaping gives the
    // mark a zero advance and a GPOS offset that lifts it over the base. Drawn
    // char-by-char (no shaping) the accent would instead march forward a full
    // advance and sit on the baseline.
    let mut fb = Bundled(vec![dejavu()]);
    let base =
        paint_text(FontSet::single(dejavu()), &mut fb, "e", SIZE, BLACK, PLAIN).expect("e paints");
    let accented = paint_text(
        FontSet::single(dejavu()),
        &mut fb,
        "e\u{0301}",
        SIZE,
        BLACK,
        PLAIN,
    )
    .expect("é paints");

    assert!(
        accented.width <= base.width + 1,
        "the accent must not claim an advance of its own \
         (base {}px, accented {}px)",
        base.width,
        accented.width,
    );
    assert!(
        ink_top(&accented) < ink_top(&base),
        "the accent must be positioned ABOVE the base's ink \
         (base top {}, accented top {})",
        ink_top(&base),
        ink_top(&accented),
    );
}

#[test]
fn an_emoji_paints_in_its_own_colors_not_the_text_color() {
    // Asked for black text, a COLRv1 emoji must still come out colored — the
    // old outline-only path drew color glyphs as nothing at all.
    let mut fb = Bundled(vec![emoji()]);
    let img = paint_text(
        FontSet::single(fira()),
        &mut fb,
        "\u{1F92A}",
        SIZE,
        BLACK,
        PLAIN,
    )
    .expect("the emoji paints");

    let colored = inked(&img)
        .into_iter()
        .map(|(x, y)| pixel(&img, x, y))
        .filter(|&(r, g, b, _)| r.abs_diff(g) > 8 || g.abs_diff(b) > 8)
        .count();
    assert!(
        colored > 0,
        "a color emoji must paint its own colors, not the requested text color"
    );
}

#[test]
fn outline_glyphs_take_the_requested_text_color() {
    // The titlebar hands us its theme's font color; ordinary text must honour
    // it (the frame's active/inactive states differ only by this).
    let red = [1.0, 0.0, 0.0, 1.0];
    let img = paint_text(
        FontSet::single(fira()),
        &mut NoFallback,
        "A",
        SIZE,
        red,
        PLAIN,
    )
    .expect("an A paints");

    let (r, g, b, _) = inked(&img)
        .into_iter()
        .map(|(x, y)| pixel(&img, x, y))
        .max_by_key(|&(_, _, _, a)| a)
        .expect("some ink");
    assert!(
        r > 200 && g < 40 && b < 40,
        "text must be painted in the requested color, got ({r},{g},{b})"
    );
}

#[test]
fn text_with_no_ink_paints_nothing() {
    // A blank or empty title has no bitmap to hand the frame — `None`, rather
    // than a zero-sized pixmap the compositor would choke on.
    assert!(
        paint_text(
            FontSet::single(fira()),
            &mut NoFallback,
            "",
            SIZE,
            BLACK,
            PLAIN
        )
        .is_none()
    );
    assert!(
        paint_text(
            FontSet::single(fira()),
            &mut NoFallback,
            "   ",
            SIZE,
            BLACK,
            PLAIN
        )
        .is_none()
    );
}

#[test]
fn a_mixed_script_title_keeps_every_run() {
    // The real complaint: a title mixing scripts. Each character that some face
    // covers must contribute ink, so the string cannot silently lose its
    // non-Latin half.
    let mut fb = Bundled(vec![dejavu(), emoji()]);
    let latin = paint_text(
        FontSet::single(fira()),
        &mut fb,
        "ghost",
        SIZE,
        BLACK,
        PLAIN,
    )
    .expect("latin paints");
    let mixed = paint_text(
        FontSet::single(fira()),
        &mut fb,
        "ghost ★ Привет",
        SIZE,
        BLACK,
        PLAIN,
    )
    .expect("mixed script paints");

    assert!(
        mixed.width > latin.width,
        "the non-Latin run must add width, not vanish"
    );
    assert!(
        ink_count(&mixed) > ink_count(&latin),
        "the non-Latin run must add ink, not vanish"
    );
}
