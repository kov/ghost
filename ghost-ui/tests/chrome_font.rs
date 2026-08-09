//! The font ghost's own window frame draws its title in.
//!
//! The desktop names it as a family and a *style* (`Cantarell Bold 11`), and
//! resolving that pair is the part that has broken before — so these drive
//! [`ghost_ui::font::resolve_face`] and paint with what it returns, which is
//! what the titlebar does.

#![cfg(target_os = "linux")]

use ghost_shaper::{FontSet, TextStyle, paint_text};
use ghost_ui::font::{SystemFallback, resolve_face, style_weight};

/// Ink in "ghost" painted in `family`/`style` through the chrome text path.
/// `None` when the system has no such family — then there is nothing to assert
/// about, only about the machine.
fn ink_for(family: &str, style: Option<&str>) -> Option<usize> {
    let face = resolve_face(family, style)?;
    let image = paint_text(
        FontSet::single(face),
        &mut SystemFallback::new(),
        "ghost",
        20.0,
        [0.0, 0.0, 0.0, 1.0],
        TextStyle {
            weight: style_weight(style),
        },
    )?;
    Some(image.rgba.chunks(4).filter(|p| p[3] > 0).count())
}

#[test]
fn a_bold_titlebar_font_is_heavier_than_a_regular_one() {
    // GNOME's titlebar-font carries a style name, and the frame it replaced
    // honoured it. A family shipping a real bold face is the straightforward
    // half of that.
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
