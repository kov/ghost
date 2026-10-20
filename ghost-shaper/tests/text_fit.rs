//! Fitting a proportional string into a box: chrome text that is too wide
//! keeps its *start* and ends in an ellipsis, rather than running off under
//! whatever sits next to it.

use ghost_shaper::{FontRef, FontSet, TextStyle, fit_text, font_from_bytes, glyph_id, layout_text};

const FIRA: &[u8] = include_bytes!("assets/FiraCode-Regular.ttf");
const SIZE: f32 = 20.0;
const PLAIN: TextStyle = TextStyle { weight: None };

fn fira() -> FontRef<'static> {
    font_from_bytes(FIRA).expect("parse bundled Fira Code")
}

struct NoFallback;

impl ghost_shaper::Fallback for NoFallback {
    fn face_for(&mut self, _ch: char) -> Option<FontRef<'static>> {
        None
    }
}

/// The glyph ids of a layout, in order.
fn ids(text: &str, max_w: f32) -> (Vec<u16>, f32) {
    let laid = fit_text(
        FontSet::single(fira()),
        &mut NoFallback,
        text,
        SIZE,
        PLAIN,
        max_w,
    );
    (laid.glyphs.iter().map(|g| g.id).collect(), laid.advance)
}

fn ids_of(text: &str) -> Vec<u16> {
    text.chars().map(|c| glyph_id(fira(), c)).collect()
}

/// Fira Code is monospaced: every glyph advances the same.
fn advance() -> f32 {
    layout_text(FontSet::single(fira()), &mut NoFallback, "x", SIZE, PLAIN).advance
}

#[test]
fn text_that_fits_is_laid_out_whole() {
    let (got, w) = ids("answer: ship it?", 1000.0);
    assert_eq!(got, ids_of("answer: ship it?"));
    assert!((w - 16.0 * advance()).abs() < 0.01);
}

#[test]
fn text_too_wide_keeps_its_start_and_ends_in_an_ellipsis() {
    // Room for ten glyphs: nine of the text, then the ellipsis.
    let (got, w) = ids(
        "answer: which crate should go first?",
        10.0 * advance() + 0.5,
    );
    assert_eq!(got, ids_of("answer: w\u{2026}"));
    assert!(w <= 10.0 * advance() + 0.5);
    // Room for nine: the cut falls just after the space, which goes too.
    let (got, _) = ids(
        "answer: which crate should go first?",
        9.0 * advance() + 0.5,
    );
    assert_eq!(
        got,
        ids_of("answer:\u{2026}"),
        "the space before the cut is dropped"
    );
    let (got, _) = ids(
        "answer: which crate should go first?",
        12.0 * advance() + 0.5,
    );
    assert_eq!(
        got,
        ids_of("answer: whi\u{2026}"),
        "as much of the start as fits"
    );
}

#[test]
fn a_box_too_narrow_for_the_ellipsis_draws_nothing() {
    let (got, w) = ids("answer: ship it?", advance() * 0.5);
    assert!(got.is_empty());
    assert_eq!(w, 0.0);
    let (got, _) = ids("answer: ship it?", advance() + 0.5);
    assert_eq!(got, ids_of("\u{2026}"));
}
