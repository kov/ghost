//! Headless **default-GPU** render benchmark — the keepable measurement of the real
//! GPU frame cost, distinct from the portable CPU build guard in `render.rs`.
//!
//! It renders a dense full-screen frame on the environment's real GPU (venus on the
//! dev VM, via [`Renderer::headless_default_gpu`], NOT the lavapipe fallback the other
//! tests pin) and times the no-readback path — so the number is GPU frame work, not
//! the ~33MB readback that dominates `render_offscreen` at 4K. It also times the
//! readback path once, so the printout shows exactly how much that copy costs.
//!
//! Gated: it needs a real GPU and force-exits to dodge the venus teardown SIGSEGV (see
//! `venus-teardown-bug`), so a plain `cargo test` skips it. Run it with:
//!
//! ```sh
//! GHOST_GPU_BENCH=1 cargo test -p ghost-renderer --test gpu_bench -- --nocapture
//! ```

use std::time::{Duration, Instant};

use ghost_render::{CellMetrics, layout_frame};
use ghost_renderer::{Renderer, Theme};
use ghost_term::Vt;

const FIRA: &[u8] = include_bytes!("../../ghost-shaper/tests/assets/FiraCode-Regular.ttf");

const METRICS: CellMetrics = CellMetrics {
    advance: 9.0,
    line_height: 18.0,
};

/// A screen packed with distinct short colored tokens, mirroring `ls --color` — every
/// ~9-cell token is its own SGR-colored run, all distinct. (Kept in step with the
/// `dense_ls_screen` fixture in `render.rs`; the two test binaries can't share it.)
fn dense_ls_screen(cols: usize, rows: usize) -> Vt {
    let mut vt = Vt::new(cols, rows);
    let mut s = String::new();
    let mut idx = 0usize;
    for _ in 0..rows {
        let mut c = 0usize;
        while c + 9 <= cols {
            let color = 16 + (idx % 216);
            s.push_str(&format!("\x1b[38;5;{color}mf{idx:06} "));
            idx += 1;
            c += 9;
        }
        s.push_str("\r\n");
    }
    vt.feed_str(&s);
    vt
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

#[test]
fn gpu_render_benchmark() {
    if std::env::var("GHOST_GPU_BENCH").is_err() {
        eprintln!("skipping GPU benchmark (set GHOST_GPU_BENCH=1 to run on the default GPU)");
        return;
    }

    let (mut r, info) = Renderer::headless_default_gpu(Theme::default());
    eprintln!(
        "gpu-bench adapter: {} / {} ({:?})",
        info.name, info.driver, info.device_type
    );
    let font = ghost_shaper::font_from_bytes(FIRA).unwrap();

    for (label, cols, rows) in [("HiDPI-4K", 213usize, 60usize), ("native-4K", 426, 120)] {
        let vt = dense_ls_screen(cols, rows);
        let frame = layout_frame(&vt, METRICS);
        let (w, h) = Renderer::frame_size(&frame);

        // Cold: first render of this content on a fresh renderer (empty caches) — the
        // heavy first paint. Warm: the same frame re-rendered, served from the caches.
        let mut cold_r = Renderer::headless_default_gpu(Theme::default()).0;
        let t = Instant::now();
        let cold_dims = cold_r.render_offscreen_no_readback(&frame, font, 15.0);
        let cold = t.elapsed();
        assert_eq!(cold_dims, (w, h), "cold render targets the frame size");
        drop(cold_r); // this renderer never renders again; force-exit below dodges its drop

        let warm_no_readback = median(
            (0..7)
                .map(|_| {
                    let t = Instant::now();
                    let dims = r.render_offscreen_no_readback(&frame, font, 15.0);
                    let e = t.elapsed();
                    assert_eq!(dims, (w, h));
                    e
                })
                .collect(),
        );
        // The same warm frame WITH the readback, to show the copy's share of the cost.
        let warm_readback = median(
            (0..7)
                .map(|_| {
                    let t = Instant::now();
                    let img = r.render_offscreen(&frame, font, 15.0);
                    let e = t.elapsed();
                    assert_eq!((img.width, img.height), (w, h));
                    e
                })
                .collect(),
        );

        eprintln!(
            "gpu-bench {label} {w}x{h}: cold {cold:?} | warm no-readback {warm_no_readback:?} \
             | warm +readback {warm_readback:?} (readback adds {:?})",
            warm_readback.saturating_sub(warm_no_readback),
        );
    }

    // A real driver (venus) SIGSEGVs at libtest teardown; force-exit on success to skip
    // it. The software fallback (a CPU device) tears down cleanly, so let it report
    // normally. Assertions above panic first, so a real failure still surfaces.
    if info.device_type != wgpu::DeviceType::Cpu {
        std::process::exit(0);
    }
}
