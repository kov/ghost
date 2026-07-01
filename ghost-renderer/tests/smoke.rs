//! Headless GPU smoke test: device on a software adapter, offscreen clear, and
//! pixel readback all work — the foundation for glyph golden tests.
//!
//! Linux-only: it needs a software Vulkan adapter (lavapipe), which is only
//! present on the Linux CI. On macOS there is no fallback adapter, so gate the
//! whole target out to keep a bare-macOS `cargo test` green.
#![cfg(target_os = "linux")]

use ghost_renderer::render_solid;

#[test]
fn clears_to_solid_color_and_reads_back() {
    let img = render_solid(8, 4, [1.0, 0.0, 0.0, 1.0]); // opaque red
    assert_eq!(img.width, 8);
    assert_eq!(img.height, 4);
    assert_eq!(img.rgba.len(), 8 * 4 * 4);
    for px in img.rgba.chunks_exact(4) {
        assert!(
            px[0] > 250 && px[1] < 5 && px[2] < 5 && px[3] > 250,
            "expected opaque red, got {px:?}"
        );
    }
}
