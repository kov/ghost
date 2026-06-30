//! End-to-end: run the `ghost` binary in headless capture mode, which spawns a
//! real ghost session, attaches as a client, streams its output into a local
//! Screen, and renders it offscreen to a PNG. Asserts the session's output
//! reached our screen and that a non-blank image was produced — exercising the
//! whole spawn → attach → pump → feed → layout → render path.

use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_ghost");
const LAVAPIPE: &str = "/usr/share/vulkan/icd.d/lvp_icd.aarch64.json";

#[test]
fn captures_a_real_session_to_a_png() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path().join("run");
    std::fs::create_dir_all(&xdg).unwrap();
    let png = tmp.path().join("out.png");

    let mut cmd = Command::new(BIN);
    cmd.env("XDG_RUNTIME_DIR", &xdg)
        // Isolate config so the capture doesn't read the developer's ui.toml.
        .env("XDG_CONFIG_HOME", tmp.path())
        .env("GHOST_CAPTURE", &png)
        .env("GHOST_CMD", "printf 'CAPTURE-MARKER-7\\n'");
    // Pin the software adapter so the offscreen render is reproducible headless.
    if Path::new(LAVAPIPE).exists() {
        cmd.env("VK_ICD_FILENAMES", LAVAPIPE);
    }
    let out = cmd.output().expect("run ghost-ui");
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(out.status.success(), "ghost-ui capture failed:\n{stderr}");
    // The spawned session's output round-tripped into our local screen.
    assert!(
        stderr.contains("CAPTURE-MARKER-7"),
        "marker not found on captured screen:\n{stderr}"
    );

    // A non-blank PNG was written.
    let bytes = std::fs::read(&png).expect("png written");
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().expect("png header");
    let mut buf = vec![0u8; reader.output_buffer_size().expect("png buffer size")];
    reader.next_frame(&mut buf).expect("png frame");
    // Theme background is ~[16,16,18]; text pixels are much brighter.
    let lit = buf
        .chunks_exact(4)
        .filter(|p| p[0] > 40 || p[1] > 40 || p[2] > 40)
        .count();
    assert!(lit > 50, "rendered image looks blank ({lit} lit pixels)");
}

#[test]
fn feeds_input_and_sees_it_echoed() {
    // Drive the input path end to end: the binary attaches to a `cat` session,
    // sends bytes via SessionView::send_input, and `cat` (plus the PTY echo)
    // round-trips them onto the screen.
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path().join("run");
    std::fs::create_dir_all(&xdg).unwrap();
    let png = tmp.path().join("out.png");

    let mut cmd = Command::new(BIN);
    cmd.env("XDG_RUNTIME_DIR", &xdg)
        .env("XDG_CONFIG_HOME", tmp.path())
        .env("GHOST_CAPTURE", &png)
        .env("GHOST_CMD", "cat")
        .env("GHOST_FEED", "ROUNDTRIP-42\r");
    if Path::new(LAVAPIPE).exists() {
        cmd.env("VK_ICD_FILENAMES", LAVAPIPE);
    }
    let out = cmd.output().expect("run ghost-ui");
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(out.status.success(), "ghost-ui capture failed:\n{stderr}");
    assert!(
        stderr.contains("ROUNDTRIP-42"),
        "fed input was not echoed onto the screen:\n{stderr}"
    );
}

#[test]
fn applies_color_scheme_from_config() {
    // A ui.toml selecting solarized-dark must paint the rendered background with
    // that scheme's bg (#002b36), proving config -> Theme -> renderer end to end.
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path().join("run");
    let cfg = tmp.path().join("config");
    std::fs::create_dir_all(&xdg).unwrap();
    std::fs::create_dir_all(cfg.join("ghost")).unwrap();
    std::fs::write(
        cfg.join("ghost").join("ui.toml"),
        "[colors]\nscheme = \"solarized-dark\"\n",
    )
    .unwrap();
    let png = tmp.path().join("out.png");

    let mut cmd = Command::new(BIN);
    cmd.env("XDG_RUNTIME_DIR", &xdg)
        .env("XDG_CONFIG_HOME", &cfg)
        .env("GHOST_CAPTURE", &png)
        .env("GHOST_CMD", "printf 'hi\\n'");
    if Path::new(LAVAPIPE).exists() {
        cmd.env("VK_ICD_FILENAMES", LAVAPIPE);
    }
    let out = cmd.output().expect("run ghost-ui");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "ghost-ui capture failed:\n{stderr}");

    let bytes = std::fs::read(&png).expect("png written");
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().expect("png header");
    let mut buf = vec![0u8; reader.output_buffer_size().expect("png buffer size")];
    reader.next_frame(&mut buf).expect("png frame");
    // Solarized-dark background is #002b36 = [0,43,54]; most of the (mostly
    // blank) screen should be exactly that, and never the default [16,16,18].
    let solar = buf
        .chunks_exact(4)
        .filter(|p| p[0] < 12 && (40..56).contains(&p[1]) && (48..64).contains(&p[2]))
        .count();
    let total = buf.len() / 4;
    assert!(
        solar > total / 2,
        "expected the solarized-dark background, only {solar}/{total} pixels matched"
    );
}

#[test]
fn applies_window_opacity_from_config() {
    // A ui.toml setting [window] opacity makes the default background translucent:
    // the captured PNG's blank area carries ~half alpha, not a solid 255. Proves
    // config -> Theme.bg_alpha -> the premultiplied clear, end to end.
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path().join("run");
    let cfg = tmp.path().join("config");
    std::fs::create_dir_all(&xdg).unwrap();
    std::fs::create_dir_all(cfg.join("ghost")).unwrap();
    std::fs::write(
        cfg.join("ghost").join("ui.toml"),
        "[window]\nopacity = 0.5\n",
    )
    .unwrap();
    let png = tmp.path().join("out.png");

    let mut cmd = Command::new(BIN);
    cmd.env("XDG_RUNTIME_DIR", &xdg)
        .env("XDG_CONFIG_HOME", &cfg)
        .env("GHOST_CAPTURE", &png)
        .env("GHOST_CMD", "printf 'hi\\n'");
    if Path::new(LAVAPIPE).exists() {
        cmd.env("VK_ICD_FILENAMES", LAVAPIPE);
    }
    let out = cmd.output().expect("run ghost-ui");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "ghost-ui capture failed:\n{stderr}");

    let bytes = std::fs::read(&png).expect("png written");
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let mut reader = decoder.read_info().expect("png header");
    let mut buf = vec![0u8; reader.output_buffer_size().expect("png buffer size")];
    reader.next_frame(&mut buf).expect("png frame");
    let total = buf.len() / 4;
    // The mostly-blank screen is the default background (#101012) at 0.5 opacity.
    // Most pixels must carry alpha ~128 (half of 255), AND straight (not
    // premultiplied) RGB ~16 — a premultiplied PNG would store ~8 and composite
    // too dark in any viewer.
    let translucent_bg = buf
        .chunks_exact(4)
        .filter(|p| (12..=20).contains(&p[0]) && (110..=145).contains(&p[3]))
        .count();
    assert!(
        translucent_bg > total / 2,
        "expected a half-transparent, straight-alpha background, only {translucent_bg}/{total} matched"
    );
}
