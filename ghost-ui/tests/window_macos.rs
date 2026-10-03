//! End-to-end check of how a translucent macOS window is configured.
//!
//! A native window's state can't be read from the test process, so the binary's
//! `GHOST_WINDOW_DUMP` mode opens one translucent window the way the app does,
//! prints the compositing-relevant NSWindow state, and exits; here we assert it.
//! macOS-only — the failure it guards is a WindowServer behaviour.
#![cfg(target_os = "macos")]

use std::process::Command;

const GHOST: &str = env!("CARGO_BIN_EXE_ghost");

fn dump() -> String {
    let out = Command::new(GHOST)
        .env("GHOST_WINDOW_DUMP", "1")
        .output()
        .expect("run ghost");
    assert!(
        out.status.success(),
        "ghost exited non-zero: {:?}",
        out.status
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn field(dump: &str, key: &str) -> String {
    dump.lines()
        .find_map(|l| l.strip_prefix(&format!("{key}=")))
        .unwrap_or_else(|| panic!("no {key} in:\n{dump}"))
        .to_string()
}

/// A translucent window's NSWindow background must never have alpha 0.
///
/// With a zero-alpha background — `clearColor`, which is what winit installs for
/// any `with_transparent` window, or any colour at alpha 0 — WindowServer
/// recomposites the window CONTINUOUSLY for as long as it exists, even while it
/// draws nothing at all. Measured on an idle window that rendered a single
/// frame: roughly double the machine's idle GPU utilisation, and in Quartz Debug
/// a window that never stops flashing. Any non-zero alpha avoids it entirely;
/// the value is imperceptible on top of the theme's own translucency.
///
/// Bisected down to a bare AppKit window with no winit, no wgpu and no Metal
/// layer, which reproduced it from `backgroundColor` alone — and an otherwise
/// identical window at alpha 0.001 did not. kitty carries the same workaround
/// (`glfw/cocoa_window.m`: `colorWithWhite:0 alpha:0.001`), which is why a
/// translucent kitty window costs a fraction of what ours did.
#[test]
fn a_translucent_window_never_gets_a_zero_alpha_background() {
    let dump = dump();

    // The window really is translucent — otherwise the assertion below would
    // pass vacuously on an opaque window that never had the problem.
    assert_eq!(
        field(&dump, "opaque"),
        "false",
        "the probe must open a TRANSLUCENT window, else it asserts nothing:\n{dump}"
    );

    let alpha: f64 = field(&dump, "bg_alpha")
        .parse()
        .unwrap_or_else(|e| panic!("bg_alpha not a number ({e}):\n{dump}"));
    assert!(
        alpha > 0.0,
        "a translucent window's background alpha is {alpha}: a zero-alpha \
         background makes WindowServer recomposite the window forever, burning \
         GPU while ghost draws nothing"
    );
}

fn points(dump: &str, key: &str) -> f64 {
    field(dump, key)
        .parse()
        .unwrap_or_else(|e| panic!("{key} not a number ({e}):\n{dump}"))
}

/// ghost draws its own titlebar on macOS too — the bar is where a window says
/// what the desktop's frame has nowhere to put (the freeze notice) — but the
/// traffic lights stay AppKit's: they are what VoiceOver, AX automation and the
/// tiling tools find a window by, and they bring zoom, the tiling popover and
/// fullscreen with them. So the window keeps its native frame and lets our
/// surface run up under a transparent titlebar, and the lights sit on our bar.
#[test]
fn the_traffic_lights_stay_native_on_a_bar_of_our_own() {
    let dump = dump();

    // Our surface reaches the top of the window, under a titlebar that draws
    // neither a background nor the title — both are the bar's to draw.
    assert_eq!(field(&dump, "fullsize_content"), "true", "{dump}");
    assert_eq!(field(&dump, "titlebar_transparent"), "true", "{dump}");
    assert_eq!(field(&dump, "title_hidden"), "true", "{dump}");

    // The lights are still AppKit's own, and still there.
    for b in ["close", "miniaturize", "zoom"] {
        assert_eq!(
            field(&dump, &format!("{b}_hidden")),
            "false",
            "the {b} button must stay visible:\n{dump}"
        );
    }
}

/// The bar is exactly as tall as the titlebar AppKit lays the lights out in,
/// so they sit centred on it rather than hugging its top — a taller bar is the
/// tell of a frame drawn without asking the platform.
#[test]
fn the_bar_is_the_height_the_traffic_lights_are_centred_in() {
    let dump = dump();
    let bar = points(&dump, "bar_pt");
    let native = points(&dump, "titlebar_pt");
    assert!(native > 0.0, "AppKit reports no titlebar at all:\n{dump}");
    assert_eq!(
        bar, native,
        "our bar must be the native titlebar's height:\n{dump}"
    );
    // Before the window exists, the height the app opens it with to make room
    // for the bar must agree with the one the window then reports.
    assert_eq!(
        points(&dump, "bar_pt_before_window"),
        native,
        "the window opens a bar of a different height than it gets:\n{dump}"
    );
    let centre = points(&dump, "zoom_mid_y");
    assert!(
        (centre - bar / 2.0).abs() <= 1.0,
        "the lights' centre ({centre}pt from the top) must be the bar's ({}):\n{dump}",
        bar / 2.0
    );
}

/// What the bar keeps clear for the lights covers all three of them, so no
/// title or notice we draw ever runs under one.
#[test]
fn the_bar_keeps_the_traffic_lights_clear() {
    let dump = dump();
    let controls = points(&dump, "controls_pt");
    let zoom_right = points(&dump, "zoom_max_x");
    assert!(
        controls >= zoom_right,
        "the bar keeps {controls}pt clear, but the zoom button reaches {zoom_right}pt:\n{dump}"
    );
    // ...and not a whole window's worth: the title still has room.
    assert!(
        controls < 120.0,
        "{controls}pt is far more than the lights:\n{dump}"
    );
}
