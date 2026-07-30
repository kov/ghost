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
