//! The font ghost's own titlebar draws its title in on macOS: the system's
//! window-title font, the one every native window's title is set in.
//!
//! It cannot be asked for by name — the system UI family is dot-prefixed and
//! CoreText hands back Times for a dot name — so it comes from CoreText's UI
//! font role, and its face has to be dug out of a file and drawn through the
//! chrome text path like any other.

#![cfg(target_os = "macos")]

use ghost_shaper::{FontSet, TextStyle, paint_text};
use ghost_ui::desktop::desktop_font;
use ghost_ui::font::{SystemFallback, resolve_title_face, style_weight};

fn ink(weight: Option<f32>) -> usize {
    let face = resolve_title_face().expect("the window-title font resolves to a face");
    let image = paint_text(
        FontSet::single(face),
        &mut SystemFallback::new(),
        "ghost",
        20.0,
        [0.0, 0.0, 0.0, 1.0],
        TextStyle { weight },
    )
    .expect("the title paints");
    image.rgba.chunks(4).filter(|p| p[3] > 0).count()
}

#[test]
fn the_title_is_drawn_in_the_systems_window_title_font() {
    let ui = desktop_font();
    assert!(
        (10.0..=20.0).contains(&ui.pt_size),
        "a window title is {}pt? {ui:?}",
        ui.pt_size
    );
    assert!(
        ink(style_weight(ui.style.as_deref())) > 0,
        "{ui:?} draws nothing"
    );
}

#[test]
fn the_title_carries_the_weight_the_system_sets_it_in() {
    // Native titles are bold. The system face is one variable file, so the
    // weight is only real if the axis is honoured — the same face asked for at
    // the regular weight must come out lighter.
    let ui = desktop_font();
    let weight = style_weight(ui.style.as_deref());
    assert!(
        weight.is_some_and(|w| w > 400.0),
        "the title font should be heavier than regular: {ui:?}"
    );
    assert!(
        ink(weight) > ink(Some(400.0)),
        "{ui:?} is no heavier than regular"
    );
}

#[test]
fn points_are_logical_pixels_on_macos() {
    // A macOS point *is* a logical pixel; the 96 dpi conversion GNOME's sizes
    // need would draw a 13pt title a third too large.
    let ui = desktop_font();
    assert_eq!(ui.px_size(2.0), ui.pt_size * 2.0);
}
