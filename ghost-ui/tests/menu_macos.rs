//! End-to-end check of the native macOS menu bar.
//!
//! A native menu can't be clicked under the test sandbox, so the binary's
//! `GHOST_MENU_DUMP` mode installs the real menu against a running
//! `NSApplication` (no window, no session) and prints its structure; here we
//! assert that structure. macOS-only — there is no menu bar elsewhere.
#![cfg(target_os = "macos")]

use std::process::Command;

const GHOST: &str = env!("CARGO_BIN_EXE_ghost");

#[test]
fn menu_dump_lists_the_expected_native_menu_bar() {
    let out = Command::new(GHOST)
        .env("GHOST_MENU_DUMP", "1")
        .output()
        .expect("run ghost");
    assert!(
        out.status.success(),
        "ghost exited non-zero: {:?}",
        out.status
    );
    let dump = String::from_utf8_lossy(&out.stdout);

    // winit's App submenu survives our append (Quit still terminates the app).
    assert!(
        dump.contains("Quit ghost\tkey=q\taction=terminate:"),
        "{dump}"
    );

    // File: our window/session items route to the app; Close uses AppKit's own
    // performClose:, so it flows through the same "close = detach" path as Cmd-W.
    for line in [
        "New Window\tkey=n\taction=ghostMenuAction:",
        "New Session\tkey=t\taction=ghostMenuAction:",
        "Close Window\tkey=w\taction=performClose:",
    ] {
        assert!(dump.contains(line), "missing {line:?} in:\n{dump}");
    }

    // Edit: Copy/Paste route through the app so they mirror the terminal's own
    // selection/clipboard handling rather than AppKit's inert copy:/paste:.
    assert!(
        dump.contains("Copy\tkey=c\taction=ghostMenuAction:"),
        "{dump}"
    );
    assert!(
        dump.contains("Paste\tkey=v\taction=ghostMenuAction:"),
        "{dump}"
    );
    // Emoji & Symbols opens the system Character Viewer via AppKit's own
    // selector, bound to Ctrl-Cmd-Space: the chord is NOT a global hotkey — in
    // apps where it works it is the key equivalent of their (usually AppKit
    // auto-added) Edit-menu item, so ours must carry it too. Insertion then
    // arrives through the IME commit path.
    assert!(
        dump.contains("Emoji & Symbols\tkey= \taction=orderFrontCharacterPalette:\tmods=ctrl-cmd"),
        "{dump}"
    );

    // View: font zoom routes to the app.
    assert!(
        dump.contains("Zoom In\tkey==\taction=ghostMenuAction:"),
        "{dump}"
    );
    assert!(
        dump.contains("Actual Size\tkey=0\taction=ghostMenuAction:"),
        "{dump}"
    );

    // Window: native AppKit items; setWindowsMenu additionally hands AppKit the
    // window list and the Cmd-` cycling.
    assert!(
        dump.contains("Minimize\tkey=m\taction=performMiniaturize:"),
        "{dump}"
    );

    // The four ghost submenus exist as top-level entries in the bar.
    for submenu in ["File", "Edit", "View", "Window"] {
        assert!(
            dump.lines()
                .any(|l| l == format!("MENU\t{submenu}\tkey=\taction=submenuAction:")),
            "missing top-level {submenu} submenu in:\n{dump}"
        );
    }

    // Dock: the icon's right-click menu carries a New Window that routes to the app
    // (so it works with no window focused), verified by asking the delegate exactly
    // as AppKit would. Its key equivalent is empty — a Dock menu has no chords —
    // which also distinguishes it from File > New Window (Cmd-N).
    assert!(dump.contains("DOCK"), "no Dock menu section in:\n{dump}");
    assert!(
        dump.contains("New Window\tkey=\taction=ghostMenuAction:"),
        "Dock menu missing a routed New Window in:\n{dump}"
    );
}
