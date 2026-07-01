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
}
