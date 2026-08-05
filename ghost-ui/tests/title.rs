//! The Linux titlebar draws through the CSD frame (GNOME never offers
//! server-side decorations), whose built-in text renderer has one font, no
//! shaping and no color glyphs — so a title carrying anything outside that
//! font's coverage came out as tofu or as nothing.
//!
//! ghost installs its own renderer into that seam. These drive the frame's real
//! `TitleText` — the same type the frame paints its header from — and assert on
//! the pixels it hands back.

#![cfg(target_os = "linux")]

use ghost_shaper::{Fallback, covers};
use ghost_ui::font::SystemFallback;
use sctk_adwaita::title::{TitleFont, TitleText};
use tiny_skia::{Color, Pixmap};

/// Ink pixels in the painted title.
fn ink(pixmap: &Pixmap) -> usize {
    pixmap.pixels().iter().filter(|p| p.alpha() > 0).count()
}

fn painted(title: &str) -> Option<Pixmap> {
    ghost_ui::title::install();
    let mut text = TitleText::new(Color::BLACK).expect("ghost's title renderer is installed");
    text.update_title(title);
    text.pixmap().cloned()
}

/// Whether this machine has a font covering `ch` at all — without one there is
/// nothing any renderer could draw, and the test would be asserting about the
/// font situation rather than about ghost.
fn system_covers(ch: char) -> bool {
    SystemFallback::new()
        .face_for(ch)
        .is_some_and(|f| covers(f, ch))
}

#[test]
fn a_title_outside_the_ui_fonts_coverage_still_draws() {
    // The reported bug: special characters render poorly in the window title.
    // ★ and Cyrillic are absent from plenty of UI fonts, so they exercise the
    // per-character fallback the frame's own renderer never had.
    if !system_covers('★') || !system_covers('П') {
        eprintln!("skipping: no system font covers the test characters");
        return;
    }

    let latin = painted("ghost").expect("a latin title paints");
    let mixed = painted("ghost ★ Привет").expect("a mixed-script title paints");

    assert!(
        mixed.width() > latin.width(),
        "the non-Latin part of the title must take width ({} vs {})",
        mixed.width(),
        latin.width(),
    );
    assert!(
        ink(&mixed) > ink(&latin),
        "the non-Latin part of the title must draw ink, not tofu-shaped nothing",
    );
}

/// Ink in a title painted directly through ghost's renderer with a chosen font
/// — the desktop's own preference is whatever it is, so a test that cares about
/// the *style* has to name one.
fn ink_for(family: &str, style: Option<&str>) -> Option<usize> {
    let font = TitleFont {
        name: family.into(),
        style: style.map(Into::into),
        pt_size: 11.0,
    };
    ghost_ui::title::renderer()
        .render("ghost", &font, 20.0, [0.0, 0.0, 0.0, 1.0])
        .map(|img| img.rgba.chunks(4).filter(|p| p[3] > 0).count())
}

#[test]
fn a_bold_titlebar_font_is_heavier_than_a_regular_one() {
    // GNOME's titlebar-font is a style name (`Cantarell Bold 11`), and the
    // frame's own renderer honoured it. A family with a real bold face is the
    // straightforward half of that.
    let Some(regular) = ink_for("sans-serif", None) else {
        eprintln!("skipping: no sans-serif font on this system");
        return;
    };
    let bold = ink_for("sans-serif", Some("Bold")).expect("a bold face resolves");
    assert!(
        bold > regular,
        "a Bold titlebar font must paint heavier than a Regular one ({bold} vs {regular} ink)",
    );
}

#[test]
fn a_bold_style_of_a_variable_font_is_heavier_too() {
    // The hard half, and how this broke: for a variable font, fontconfig
    // reports the *named instance* in the high 16 bits of the face index —
    // Adwaita Sans has one file and no separate Bold. Taken literally that
    // index loads nothing at all, and GNOME's default titlebar font vanished.
    let Some(regular) = ink_for("Adwaita Sans", None) else {
        eprintln!("skipping: Adwaita Sans is not installed");
        return;
    };
    let bold = ink_for("Adwaita Sans", Some("Bold")).expect("the bold instance resolves");
    assert!(
        bold > regular,
        "a variable font's Bold instance must paint heavier ({bold} vs {regular} ink)",
    );
}

#[test]
fn an_empty_title_paints_nothing() {
    // The frame centers whatever pixmap it is given; a blank title must yield
    // none at all rather than a zero-sized surface.
    assert!(painted("").is_none());
}

#[test]
fn the_title_is_painted_in_the_frames_font_color() {
    // The frame signals focus by handing us a different font color, so the
    // renderer must actually honour it.
    ghost_ui::title::install();
    let mut text = TitleText::new(Color::from_rgba8(255, 0, 0, 255)).expect("installed");
    text.update_title("ghost");
    let pixmap = text.pixmap().expect("a latin title paints");

    let strongest = pixmap
        .pixels()
        .iter()
        .max_by_key(|p| p.alpha())
        .expect("some ink");
    assert!(
        strongest.red() > 200 && strongest.green() < 40 && strongest.blue() < 40,
        "title text must take the frame's font color, got ({},{},{})",
        strongest.red(),
        strongest.green(),
        strongest.blue(),
    );
}
