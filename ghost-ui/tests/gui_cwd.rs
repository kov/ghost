//! End-to-end: a GUI (non-CLI) launch starts its session in the right directory.
//!
//! A bundled launch (Finder/launchd `.app`, a Linux desktop file) starts `ghost`
//! at `/`, so new sessions would open in `/`; `main` redirects those to `$HOME`.
//! A launch from a real working directory must be left alone. Both are checked by
//! running capture mode (which spawns a real session) with `pwd -P` as the
//! command and reading the captured screen off stderr.
//!
//! Linux-only: capture mode renders on a software adapter (lavapipe), which macOS
//! lacks, so gate the whole target out — the `home_launch_dir` unit test covers
//! the redirect logic on every platform.
#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_ghost");
const LAVAPIPE: &str = "/usr/share/vulkan/icd.d/lvp_icd.aarch64.json";

/// Run capture mode from `cwd` with `HOME=home` and the session command `pwd -P`,
/// returning the captured screen (stderr) flattened of all whitespace — so a path
/// that wrapped across terminal rows is still matchable as one contiguous string.
fn captured_cwd(cwd: &Path, home: &Path) -> String {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path().join("run");
    std::fs::create_dir_all(&xdg).unwrap();
    let png = tmp.path().join("out.png");

    let mut cmd = Command::new(BIN);
    cmd.current_dir(cwd)
        .env("HOME", home)
        .env("XDG_RUNTIME_DIR", &xdg)
        .env("XDG_CONFIG_HOME", tmp.path())
        .env("GHOST_CAPTURE", &png)
        .env("GHOST_CMD", "pwd -P");
    if Path::new(LAVAPIPE).exists() {
        cmd.env("VK_ICD_FILENAMES", LAVAPIPE);
    }
    let out = cmd.output().expect("run ghost");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "ghost capture failed:\n{stderr}");
    stderr.split_whitespace().collect()
}

/// The physical (symlink-resolved) form of a path, matching what `pwd -P` prints
/// (macOS temp dirs live under a `/var -> /private/var` symlink).
fn physical(p: &Path) -> String {
    p.canonicalize()
        .unwrap()
        .to_string_lossy()
        .split_whitespace()
        .collect()
}

#[test]
fn bundled_launch_at_root_starts_in_home() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    // Launched at `/` (as a bundle is): the session must open in $HOME instead.
    let screen = captured_cwd(Path::new("/"), &home);
    assert!(
        screen.contains(&physical(&home)),
        "session should start in HOME when launched at /, screen was:\n{screen}"
    );
}

#[test]
fn a_real_launch_directory_is_kept() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&work).unwrap();

    // Launched from a real directory (e.g. a terminal): keep it, don't hop to HOME.
    let screen = captured_cwd(&work, &home);
    assert!(
        screen.contains(&physical(&work)),
        "a real launch dir must be kept, screen was:\n{screen}"
    );
    assert!(
        !screen.contains(&physical(&home)),
        "must NOT redirect to HOME from a real launch dir, screen was:\n{screen}"
    );
}
