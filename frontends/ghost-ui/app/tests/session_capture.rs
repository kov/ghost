//! End-to-end: run the ghost-ui binary in headless capture mode, which spawns a
//! real ghost session, attaches as a client, streams its output into a local
//! Screen, and renders it offscreen to a PNG. Asserts the session's output
//! reached our screen and that a non-blank image was produced — exercising the
//! whole spawn → attach → pump → feed → layout → render path.

use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_ghost-ui");
const LAVAPIPE: &str = "/usr/share/vulkan/icd.d/lvp_icd.aarch64.json";

#[test]
fn captures_a_real_session_to_a_png() {
    let tmp = tempfile::tempdir().unwrap();
    let xdg = tmp.path().join("run");
    std::fs::create_dir_all(&xdg).unwrap();
    let png = tmp.path().join("out.png");

    let mut cmd = Command::new(BIN);
    cmd.env("XDG_RUNTIME_DIR", &xdg)
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
