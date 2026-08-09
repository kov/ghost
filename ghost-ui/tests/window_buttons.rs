//! The window buttons ghost draws on its own frame, all the way to pixels.
//!
//! `ghost-ui-core` already checks that exactly one button wears a disc when the
//! pointer is on it; this checks that the disc is *round*. The two halves live
//! in different crates — the chrome that asks and the renderer that draws — so
//! the join is only ever tested here, where they meet.
//!
//! Linux-only, like the other GPU tests: the offscreen path needs lavapipe.
#![cfg(target_os = "linux")]

use ghost_render::{Layer, RectPx, Scene, SceneId, SceneItem};
use ghost_renderer::{Renderer, Theme};
use ghost_ui_core::frame::{ButtonLayout, Titlebar, WindowButton, button_rects, with_titlebar};

const FIRA: &[u8] = include_bytes!("../../ghost-shaper/tests/assets/FiraCode-Regular.ttf");

const W: u32 = 400;
const H: u32 = 120;
const BAR_H: u32 = 35;

#[test]
fn the_hovered_window_button_wears_a_circle_not_a_square() {
    let bar = Titlebar {
        height_px: BAR_H,
        bg: [0.1, 0.1, 0.1, 1.0],
        // White, so the disc — the title colour at 9% — is the only thing that
        // lifts the bar off black, and a lit pixel can only be the disc.
        fg: [1.0, 1.0, 1.0, 1.0],
        title: String::new(),
        notice: None,
        font_px: 15.0,
        buttons: ButtonLayout::parse(":close"),
        hovered: Some(WindowButton::Close),
        pressed: None,
        maximized: false,
        scale: 1.0,
    };
    let mut content = Scene::new((W, H));
    content.layers.push(Layer::new(
        0,
        vec![SceneItem::Rect {
            id: SceneId::Root,
            rect: RectPx {
                x: 0.0,
                y: 0.0,
                w: W as f32,
                h: H as f32,
            },
            color: [0.0, 0.0, 0.0, 1.0],
            radius: 0.0,
        }],
    ));
    let scene = with_titlebar(content, &bar);

    let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
    let img = Renderer::headless(Theme::default()).render_offscreen_scene(&scene, font, 15.0);
    img.save_png(std::env::temp_dir().join("ghost_window_button_hover.png"))
        .expect("png");

    let strip = RectPx {
        x: 0.0,
        y: 0.0,
        w: W as f32,
        h: BAR_H as f32,
    };
    let (_, r) = button_rects(&bar.buttons, strip, 1.0)[0];
    let at = |x: f32, y: f32| {
        let i = ((y.round() as u32 * W + x.round() as u32) * 4) as usize;
        img.rgba[i]
    };
    // The bar itself is 0.1 grey; the disc lifts it by 9% of white. Anything
    // between them is the antialiased rim, which is why this reads well clear
    // of the edge on both sides rather than splitting the difference.
    let bar_grey = at(r.x - 4.0, r.y + r.h * 0.5);
    let lit = |x: f32, y: f32| at(x, y) > bar_grey + 8;

    assert!(
        lit(r.x + r.w * 0.5, r.y + r.h * 0.5),
        "the disc fills the middle of the button"
    );
    assert!(
        lit(r.x + 2.0, r.y + r.h * 0.5),
        "and reaches the button's left edge at its widest"
    );
    for (dx, dy) in [(1.0, 1.0), (r.w - 2.0, 1.0), (1.0, r.h - 2.0)] {
        assert!(
            !lit(r.x + dx, r.y + dy),
            "the square corner at (+{dx}, +{dy}) is outside the circle"
        );
    }
}
