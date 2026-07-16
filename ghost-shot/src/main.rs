//! `ghost-shot` — render a ghost-ui scene to a PNG, headlessly, so we can *look*
//! at what the renderer draws without launching the windowed app.
//!
//! It drives the real models (`FleetModel`, `RootModel`, `TerminalModel`) with
//! synthetic-but-representative sessions, asks them for a `Scene`, and rasterizes
//! it through the same `ghost-renderer` the app uses (software adapter). This is
//! the first-class visual-debugging path: change the UI, run the tool, eyeball
//! the image.
//!
//! Usage: `ghost-shot <fleet|single> [out.png]` (default `ghost-shot.png`), or one
//! of the headless performance benchmarks: `ghost-shot bench [tiles] [frames]`
//! (fleet arrow-nav) / `bench resize [tiles] [steps]` (window resize) / `bench
//! single [WxH] [scale] [frames]` (the maximized single view, e.g. 4K).

use std::collections::HashSet;

use ghost_render::CellMetrics;
use ghost_renderer::{Damage, Rendered, Renderer, SceneCache, Theme};
use ghost_ui_core::{
    FleetModel, Key, KeyEventKind, Mods, NamedKey, RootModel, Sessions, TerminalModel, UiEvent,
};
use ghost_vt::session::SessionInfo;

/// A bundled font so the tool needs no system font lookup.
const FIRA: &[u8] = include_bytes!("../../ghost-shaper/tests/assets/FiraCode-Regular.ttf");

/// Proven metrics/size pairing from the renderer's golden tests (FiraCode at
/// `SIZE_PX` advances ~9px and is ~18px tall).
const METRICS: CellMetrics = CellMetrics {
    advance: 9.0,
    line_height: 18.0,
};
const SIZE_PX: f32 = 15.0;

fn main() {
    let mut args = std::env::args().skip(1);
    let which = args.next().unwrap_or_else(|| "fleet".to_string());

    if which == "bench" {
        // `bench [tiles] [frames]` runs the arrow-nav benchmark (back-compat);
        // `bench resize [tiles] [steps]` runs the window-resize benchmark.
        let mode = args.next().unwrap_or_default();
        if mode == "resize" {
            let tiles = args.next().and_then(|s| s.parse().ok()).unwrap_or(6);
            let steps = args.next().and_then(|s| s.parse().ok()).unwrap_or(120);
            bench_resize(tiles, steps);
        } else if mode == "single" {
            // `bench single [WxH] [scale] [frames]` — the maximized 4K single view.
            let size = args
                .next()
                .and_then(|s| parse_size(&s))
                .unwrap_or((3840, 2160));
            let scale = args.next().and_then(|s| s.parse().ok()).unwrap_or(2.0);
            let frames = args.next().and_then(|s| s.parse().ok()).unwrap_or(120);
            bench_single(size, scale, frames);
        } else if mode == "type" {
            // `bench type [WxH] [scale] [frames]` — typing into the single view (one
            // row changes per frame): full redraw vs damage-aware partial redraw.
            let size = args
                .next()
                .and_then(|s| parse_size(&s))
                .unwrap_or((3840, 2160));
            let scale = args.next().and_then(|s| s.parse().ok()).unwrap_or(2.0);
            let frames = args.next().and_then(|s| s.parse().ok()).unwrap_or(200);
            bench_type(size, scale, frames);
        } else if mode == "dive" {
            // `bench dive [WxH] [scale] [sessions]` — the single<->fleet zoom animation
            // with `sessions` tiles (first half attached): full-surface repaint per frame.
            let size = args
                .next()
                .and_then(|s| parse_size(&s))
                .unwrap_or((3840, 2160));
            let scale = args.next().and_then(|s| s.parse().ok()).unwrap_or(2.0);
            let sessions = args.next().and_then(|s| s.parse().ok()).unwrap_or(6);
            bench_dive(size, scale, sessions);
        } else {
            let tiles = mode.parse().unwrap_or(6);
            let frames = args.next().and_then(|s| s.parse().ok()).unwrap_or(600);
            bench(tiles, frames);
        }
        return;
    }

    if which == "calib-tui" {
        // Take over the real terminal and draw the calibration pattern — run it as a
        // ghost session, then eyeball its tile in the fleet.
        calib_tui();
        return;
    }

    if which == "calib" {
        // `calib [WxH] [N] [out.png]` — render the fleet with N calibration sessions
        // so preview position, scale, aspect and grid density can be checked at any
        // window size and session count.
        let (mut size, mut count, mut out) = (
            (720u32, 432u32),
            12usize,
            "ghost-shot-calib.png".to_string(),
        );
        for a in args {
            if let Some(s) = parse_size(&a) {
                size = s;
            } else if let Ok(n) = a.parse::<usize>() {
                count = n;
            } else {
                out = a;
            }
        }
        let scene = calib_scene(size, count);
        let font = ghost_shaper::font_from_bytes(FIRA).expect("bundled font loads");
        let mut renderer = Renderer::headless(Theme::default());
        let img = renderer.render_offscreen_scene(&scene, font, SIZE_PX);
        img.save_png(&out).expect("write png");
        println!("wrote calib fleet ({}x{}) to {out}", size.0, size.1);
        return;
    }

    if which == "zoom" {
        // `zoom [in|out] [count] [prefix]` — render a CONTACT SHEET of the fleet dive:
        // one tile per progress step, every 5 %, so the whole camera motion can be
        // eyeballed at once. The dive is into/out of the *second* session. With no
        // direction both sheets are written (`{prefix}-in.png`, `{prefix}-out.png`);
        // `count` sessions (default 2). A thin white bar across each tile's top marks
        // progress (0 → 100 %), read left-to-right, top-to-bottom.
        let mut rest: Vec<String> = args.collect();
        let dir = rest
            .first()
            .filter(|s| *s == "in" || *s == "out")
            .cloned()
            .inspect(|_| {
                rest.remove(0);
            });
        let count = rest
            .iter()
            .find_map(|s| s.parse::<usize>().ok())
            .unwrap_or(2);
        let prefix = rest
            .iter()
            .find(|s| s.parse::<usize>().is_err())
            .cloned()
            .unwrap_or_else(|| "dive".to_string());
        for d in ["in", "out"] {
            if dir.as_deref().is_none_or(|want| want == d) {
                zoom_contact_sheet(d, count, &format!("{prefix}-{d}.png"));
            }
        }
        return;
    }

    if which == "frame" {
        // `frame <in|out> <pct> [count] [out.png]` — one full-resolution dive frame at
        // `pct` %, for inspecting detail the downscaled contact sheet can't show.
        let dir = args.next().unwrap_or_else(|| "in".to_string());
        let pct = args
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(50);
        let mut count = 2usize;
        let mut out = format!("dive-{dir}-{pct:03}.png");
        for a in args {
            if let Ok(n) = a.parse::<usize>() {
                count = n;
            } else {
                out = a;
            }
        }
        let (scene, w, h) = dive_frame_scene(&dir, count, pct);
        let font = ghost_shaper::font_from_bytes(FIRA).expect("bundled font loads");
        let mut renderer = Renderer::headless(Theme::default());
        let img = renderer.render_offscreen_scene(&scene, font, SIZE_PX);
        img.save_png(&out).expect("write png");
        println!("wrote {dir}-dive frame at {pct}% ({w}x{h}) to {out}");
        return;
    }

    let out = args.next().unwrap_or_else(|| "ghost-shot.png".to_string());
    let (scene, w, h) = match which.as_str() {
        "fleet" => fleet_scene(false),
        "fleet-revealed" => fleet_scene(true),
        "single" => single_scene(),
        other => {
            eprintln!(
                "unknown scene '{other}' (expected: fleet | fleet-revealed | single | zoom | bench | calib | calib-tui)"
            );
            std::process::exit(2);
        }
    };

    let font = ghost_shaper::font_from_bytes(FIRA).expect("bundled font loads");
    let mut renderer = Renderer::headless(Theme::default());
    let img = renderer.render_offscreen_scene(&scene, font, SIZE_PX);
    img.save_png(&out).expect("write png");
    println!("wrote {which} scene ({w}x{h}) to {out}");
}

/// Benchmark the real fleet hot path through `RootModel`, mirroring the user
/// flow: open a fresh window (fleet, owning nothing) → attach `tiles` sessions
/// (each becomes the foreground briefly and prints a full screen) → F9 into the
/// fleet → then `frames` rounds of arrow-nav between the tiles (move focus →
/// rebuild scene → render). Reports model vs. render time so `perf record` can
/// attribute the cost. This is exactly what holding an arrow key in the fleet does.
fn bench(tiles: usize, frames: usize) {
    use std::time::Instant;

    let size = (1400u32, 900u32);
    let key = |k: NamedKey| UiEvent::Key {
        key: Key::Named(k),
        mods: Mods::NONE,
        kind: KeyEventKind::Press,
        alts: None,
    };

    // A freshly-opened window starts in the fleet overview, owning nothing.
    let (mut root, mut states, _) = RootModel::fleet(METRICS, size, 1.0);
    root.update(
        &mut states,
        UiEvent::Resize {
            w_px: size.0,
            h_px: size.1,
            scale: 1.0,
        },
    );
    // Attach `tiles` sessions the way the shell does (spawn / take-over reply with
    // AdoptSession), each producing a full screen of output.
    for i in 0..tiles {
        let name = format!("s{i}");
        root.update(&mut states, UiEvent::AdoptSession(name.clone()));
        root.update(
            &mut states,
            UiEvent::SessionData {
                name,
                bytes: dense_screen().into_bytes(),
                ended: false,
            },
        );
    }
    // Open the fleet overview (F9).
    root.update(&mut states, key(NamedKey::F9));

    let font = ghost_shaper::font_from_bytes(FIRA).expect("bundled font loads");
    let mut renderer = Renderer::headless(Theme::default());
    let px = |r: &RootModel| SIZE_PX * r.render_scale();

    // Warm up (first frame builds the glyph atlas + shaping/frame caches).
    let _ = renderer.render_offscreen_scene(&root.view(&states), font, px(&root));

    let dirs = [
        NamedKey::ArrowDown,
        NamedKey::ArrowRight,
        NamedKey::ArrowUp,
        NamedKey::ArrowLeft,
    ];
    let (mut model_ns, mut render_ns) = (0u128, 0u128);
    for i in 0..frames {
        let nav = Instant::now();
        root.update(&mut states, key(dirs[i % dirs.len()]));
        let scene = root.view(&states);
        model_ns += nav.elapsed().as_nanos();

        let r = Instant::now();
        let _ = renderer.render_offscreen_scene(&scene, font, px(&root));
        render_ns += r.elapsed().as_nanos();
    }

    let per = |ns: u128| (ns as f64) / (frames as f64) / 1.0e6;
    println!("bench: {tiles} attached sessions, fleet open, {frames} arrow-nav frames");
    println!("  model  (update + view): {:.3} ms/frame", per(model_ns));
    println!("  render (build + raster): {:.3} ms/frame", per(render_ns));
    println!("  total: {:.3} ms/frame", per(model_ns + render_ns));
}

/// Benchmark the window-resize hot path in the fleet view, the case the shell's
/// resize coalescing targets. Sets up `tiles` attached sessions in the fleet, then
/// sweeps the window size over `steps` (a grab-shrink-grow drag) two ways: relayout
/// plus raster every step (what dragging cost before coalescing), versus capture
/// once then stretch-blit every step (the per-step cost now). The first is O(tiles)
/// per step (every preview re-renders at the new size); the second is a single
/// textured quad, flat in tile count. The live shell still does one real relayout
/// when the drag settles (and ~every 250 ms during a long drag), so the effective
/// win is close to this per-step ratio.
fn bench_resize(tiles: usize, steps: usize) {
    use std::time::Instant;

    let key = |k: NamedKey| UiEvent::Key {
        key: Key::Named(k),
        mods: Mods::NONE,
        kind: KeyEventKind::Press,
        alts: None,
    };

    let base = (1400u32, 900u32);
    let (mut root, mut states, _) = RootModel::fleet(METRICS, base, 1.0);
    root.update(
        &mut states,
        UiEvent::Resize {
            w_px: base.0,
            h_px: base.1,
            scale: 1.0,
        },
    );
    for i in 0..tiles {
        let name = format!("s{i}");
        root.update(&mut states, UiEvent::AdoptSession(name.clone()));
        root.update(
            &mut states,
            UiEvent::SessionData {
                name,
                bytes: dense_screen().into_bytes(),
                ended: false,
            },
        );
    }
    root.update(&mut states, key(NamedKey::F9));

    let font = ghost_shaper::font_from_bytes(FIRA).expect("bundled font loads");
    let mut renderer = Renderer::headless(Theme::default());
    let px = |r: &RootModel| SIZE_PX * r.render_scale();

    // A grab-shrink-grow drag: width 1400 → 900 → 1400, height tracking the aspect.
    let sizes: Vec<(u32, u32)> = (0..steps)
        .map(|i| {
            let t = i as f32 / steps as f32;
            let w = (1400.0 - 500.0 * (std::f32::consts::PI * t).sin()).round() as u32;
            let h = (w as f32 * 9.0 / 14.0).round() as u32;
            (w.max(200), h.max(200))
        })
        .collect();

    // Warm caches at the base size.
    let _ = renderer.render_offscreen_scene(&root.view(&states), font, px(&root));

    // Old behaviour: every drag step relayouts the model and re-rasters every tile.
    let mut relayout_ns = 0u128;
    for &(w, h) in &sizes {
        let t = Instant::now();
        root.update(
            &mut states,
            UiEvent::Resize {
                w_px: w,
                h_px: h,
                scale: 1.0,
            },
        );
        let scene = root.view(&states);
        let _ = renderer.render_offscreen_scene(&scene, font, px(&root));
        relayout_ns += t.elapsed().as_nanos();
    }

    // New behaviour: capture once at the gesture start, then stretch-blit each step.
    root.update(
        &mut states,
        UiEvent::Resize {
            w_px: base.0,
            h_px: base.1,
            scale: 1.0,
        },
    );
    renderer.capture_snapshot(&root.view(&states), font, px(&root));
    let mut blit_ns = 0u128;
    for &(w, h) in &sizes {
        let t = Instant::now();
        let _ = renderer.blit_snapshot_offscreen(w, h);
        blit_ns += t.elapsed().as_nanos();
    }
    renderer.clear_snapshot();

    let per = |ns: u128| (ns as f64) / (steps as f64) / 1.0e6;
    println!("bench resize: {tiles} attached sessions, fleet open, {steps} drag steps");
    println!(
        "  relayout+raster per step (old): {:.3} ms",
        per(relayout_ns)
    );
    println!("  stretch-blit per step (new):    {:.3} ms", per(blit_ns));
    println!(
        "  per-step speedup: {:.1}x",
        relayout_ns as f64 / blit_ns.max(1) as f64
    );
}

/// A screen filled with a solid truecolor background — a flat block that makes a
/// tile instantly identifiable (and its extent obvious) in a dive frame.
fn solid_screen(r: u8, g: u8, b: u8) -> String {
    let mut s = String::from("\x1b[2J\x1b[H");
    for _ in 0..60 {
        s.push_str(&format!("\x1b[48;2;{r};{g};{b}m"));
        s.push_str(&" ".repeat(170));
        s.push_str("\x1b[0m\r\n");
    }
    s
}

/// A full 80×24 screen of varied, coloured glyphs — a dense-ish preview.
fn dense_screen() -> String {
    let mut s = String::new();
    for row in 0..24 {
        s.push_str(&format!("\x1b[38;5;{}m", 16 + (row % 200)));
        for col in 0..80 {
            s.push(char::from(b'!' + ((row * 7 + col * 3) % 90) as u8));
        }
        s.push_str("\r\n");
    }
    s
}

/// A full `cols`×`rows` screen of varied coloured glyphs — the dense-content case
/// for the single-view benchmark (every cell carries a glyph and a colour run).
fn dense_screen_sized(cols: usize, rows: usize) -> String {
    let mut s = String::new();
    for row in 0..rows {
        s.push_str(&format!("\x1b[38;5;{}m", 16 + (row % 200)));
        for col in 0..cols {
            s.push(char::from(b'!' + ((row * 7 + col * 3) % 90) as u8));
        }
        if row + 1 < rows {
            s.push_str("\r\n");
        }
    }
    s
}

/// Benchmark the single (one-terminal) view at a given window size — the maximized
/// 4K case the user hits. Fills the whole grid with dense coloured text, then feeds
/// one fresh line per frame (the active-output case: the screen scrolls, so every
/// row changes) and renders, separating model time from render time. On lavapipe the
/// render figure is dominated by software rasterization of the full surface.
fn bench_single(size: (u32, u32), scale: f32, frames: usize) {
    use std::time::Instant;

    let name = "bench";
    let model = TerminalModel::new(name.to_string(), 1, 1, METRICS);
    let (mut root, mut states) = RootModel::single(model, METRICS, size);
    root.update(
        &mut states,
        UiEvent::Resize {
            w_px: size.0,
            h_px: size.1,
            scale: scale as f64,
        },
    );
    let cols = (size.0 as f32 / (METRICS.advance * scale)).floor().max(1.0) as usize;
    let rows = (size.1 as f32 / (METRICS.line_height * scale))
        .floor()
        .max(1.0) as usize;
    root.update(
        &mut states,
        UiEvent::SessionData {
            name: name.to_string(),
            bytes: dense_screen_sized(cols, rows).into_bytes(),
            ended: false,
        },
    );

    let font = ghost_shaper::font_from_bytes(FIRA).expect("bundled font loads");
    let mut renderer = Renderer::headless(Theme::default());
    let px = SIZE_PX * root.render_scale();

    // Warm up (first frame builds the glyph atlas + shaping/frame caches).
    let _ = renderer.render_offscreen_scene(&root.view(&states), font, px);

    let (mut model_ns, mut render_ns) = (0u128, 0u128);
    for i in 0..frames {
        let line = format!(
            "\x1b[38;5;{}mframe {i}: the quick brown fox jumps over the lazy dog\r\n",
            16 + (i % 200)
        );
        let m = Instant::now();
        root.update(
            &mut states,
            UiEvent::SessionData {
                name: name.to_string(),
                bytes: line.into_bytes(),
                ended: false,
            },
        );
        let scene = root.view(&states);
        model_ns += m.elapsed().as_nanos();

        let r = Instant::now();
        let _ = renderer.render_offscreen_scene(&scene, font, px);
        render_ns += r.elapsed().as_nanos();
    }

    let per = |ns: u128| (ns as f64) / (frames as f64) / 1.0e6;
    println!(
        "bench single: {}x{} @ {scale}x ({cols}x{rows} grid), {frames} frames of scrolling output",
        size.0, size.1
    );
    println!(
        "  model  (update + view):          {:.3} ms/frame",
        per(model_ns)
    );
    println!(
        "  render (build+raster+alloc+read): {:.3} ms/frame",
        per(render_ns)
    );
    println!(
        "  total:                            {:.3} ms/frame",
        per(model_ns + render_ns)
    );
}

/// Benchmark TYPING into the single view (one row changes per frame — the common
/// interactive case) at `size`. Renders through the headless "present" path
/// (`render_to_cached_target`, no per-frame alloc or readback) so the figure tracks the
/// live cost: the foreground composites through its per-session Surface, which re-rasters
/// only the row that changed and blits the whole Surface.
fn bench_type(size: (u32, u32), scale: f32, frames: usize) {
    use std::time::Instant;

    let name = "bench";
    let cols = (size.0 as f32 / (METRICS.advance * scale)).floor().max(1.0) as usize;
    let rows = (size.1 as f32 / (METRICS.line_height * scale))
        .floor()
        .max(1.0) as usize;
    let mid = rows / 2 + 1; // 1-based row for the CUP escape
    let font = ghost_shaper::font_from_bytes(FIRA).expect("bundled font loads");

    // One char written at (mid, cycling column) per frame — only row `mid` changes.
    let keystroke = |i: usize| format!("\x1b[{};{}Hx", mid, (i % cols) + 1).into_bytes();

    let model = TerminalModel::new(name.to_string(), 1, 1, METRICS);
    let (mut root, mut states) = RootModel::single(model, METRICS, size);
    root.update(
        &mut states,
        UiEvent::Resize {
            w_px: size.0,
            h_px: size.1,
            scale: scale as f64,
        },
    );
    root.update(
        &mut states,
        UiEvent::SessionData {
            name: name.to_string(),
            bytes: dense_screen_sized(cols, rows).into_bytes(),
            ended: false,
        },
    );
    let px = SIZE_PX * root.render_scale();
    let mut renderer = Renderer::headless(Theme::default());
    let mut cache = SceneCache::default();

    let feed = |root: &mut RootModel,
                states: &mut Sessions,
                renderer: &mut Renderer,
                cache: &mut SceneCache,
                i| {
        root.update(
            states,
            UiEvent::SessionData {
                name: name.to_string(),
                bytes: keystroke(i),
                ended: false,
            },
        );
        let scene = root.view(states);
        if cache.damage(&scene, px) == Damage::None {
            return;
        }
        renderer.render_to_cached_target(&scene, font, px);
    };

    // Warm the caches before measuring.
    for i in 0..16 {
        feed(&mut root, &mut states, &mut renderer, &mut cache, i);
    }
    let t = Instant::now();
    for i in 0..frames {
        feed(&mut root, &mut states, &mut renderer, &mut cache, i);
    }
    let ms = (t.elapsed().as_nanos() as f64) / (frames as f64) / 1.0e6;
    println!(
        "bench type: {}x{} @ {scale}x ({cols}x{rows} grid), {frames} typing frames (1 row/frame)",
        size.0, size.1
    );
    println!("  present: {ms:.3} ms/frame  ({:.0} fps)", 1000.0 / ms);
}

/// Benchmark the single↔fleet DIVE animation at a given window size: a fleet of
/// `count` sessions (the first half attached to this window, with live previews; the
/// rest detached/elsewhere as cold cards), dived single→fleet (F9) then fleet→single
/// (select the tile), driven frame by frame at the live ~60fps cadence. Every dive
/// frame carries the camera transform, so the damage detector classifies it `Full` —
/// a whole-surface repaint each frame — which is the cost this measures. Reports
/// render ms/frame (avg + worst) against the 16 ms (60fps) budget; over budget means
/// the dive can't hold 60fps. Runs each dive once to warm the preview/atlas caches,
/// then measures a second pass (the steady cost of a repeat dive).
fn bench_dive(size: (u32, u32), scale: f32, count: usize) {
    use ghost_ui_harness::Harness;
    use std::time::Instant;

    // ~60fps dive cadence, mirroring the core's ANIM_TICK_MS.
    const TICK_MS: u64 = 16;
    let f9 = || UiEvent::Key {
        key: Key::Named(NamedKey::F9),
        mods: Mods::NONE,
        kind: KeyEventKind::Press,
        alts: None,
    };
    let names: Vec<String> = ["edit", "build", "logs", "prod", "test", "docs"]
        .iter()
        .take(count.clamp(1, 6))
        .map(|s| s.to_string())
        .collect();
    let count = names.len();
    let attached_n = count.div_ceil(2); // first half attached to THIS window
    let target = names[0].clone();
    // The host's session list: the first `attached_n` are this window's (attached,
    // live previews below); the rest are detached/elsewhere sessions — cold cards.
    let sessions: Vec<SessionInfo> = names
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let mut si = info(n, i < attached_n, &[], i as i32 + 1);
            si.created_at = Some(i as i64 + 1); // names[0] oldest → stable order
            si
        })
        .collect();

    // Drive the REAL frontend through the harness: synthetic sessions feed in, every
    // attached tile gets a dense live preview, then we foreground the target so the
    // first dive goes OUT. The harness answers `Cmd::ListSessions` from the list, so
    // an F9 toggle completes the grid and launches the dive itself — no hand-rolled
    // reconcile, no second copy of the frame loop. (Offscreen: no swapchain/vsync.)
    let mut h = Harness::fleet(METRICS, size, scale);
    h.set_sessions(sessions);
    for n in &names[..attached_n] {
        h.inject(UiEvent::AdoptSession(n.clone()));
        h.inject(UiEvent::SessionData {
            name: n.clone(),
            bytes: dense_screen().into_bytes(),
            ended: false,
        });
    }
    h.inject(UiEvent::AdoptSession(target.clone())); // land in the single view

    // Drive the in-flight dive to completion, one frame per ~60fps tick, timing the
    // real per-frame work (model `view` + damage + full-surface redraw). Returns
    // (avg ms/frame, worst ms, frames, a clock spaced past this dive for the next).
    let drive = |h: &mut Harness, start: u64| -> (f64, f64, usize, u64) {
        let mut t = start;
        h.advance(t); // fire the launch tick that stamps the dive's start
        let (mut sum_ns, mut max_ns, mut frames) = (0u128, 0u128, 0usize);
        while h.is_animating() {
            let r = Instant::now();
            h.present();
            let e = r.elapsed().as_nanos();
            sum_ns += e;
            max_ns = max_ns.max(e);
            frames += 1;
            t += TICK_MS;
            h.advance(t);
        }
        let f = frames.max(1) as f64;
        (
            sum_ns as f64 / f / 1.0e6,
            max_ns as f64 / 1.0e6,
            frames,
            t + 1_000,
        )
    };

    // Settle the foregrounding dive, then warm the glyph atlas + preview caches with
    // one discarded out-and-back, so the measured pass reflects a repeat dive's cost.
    let mut clock = drive(&mut h, 1_000).3;
    h.inject(f9());
    clock = drive(&mut h, clock).3;
    h.inject(UiEvent::AdoptSession(target.clone()));
    clock = drive(&mut h, clock).3;

    // Measured passes.
    h.inject(f9()); // single -> fleet
    let (ro, xo, fo, c) = drive(&mut h, clock);
    clock = c;
    h.inject(UiEvent::AdoptSession(target)); // fleet -> single
    let (ri, xi, fi, _) = drive(&mut h, clock);

    println!(
        "bench dive: {}x{} @ {scale}x, {count} sessions ({attached_n} attached, {} detached), \
         ~60fps dive (real frontend via ghost-ui-harness, offscreen)",
        size.0,
        size.1,
        count - attached_n,
    );
    let line = |label: &str, ms: f64, max: f64, frames: usize| {
        println!(
            "  {label}: {ms:.3} ms/frame (worst {max:.3}), {frames} frames  ({:.0} fps)",
            1000.0 / ms.max(f64::MIN_POSITIVE)
        );
    };
    line("single -> fleet", ro, xo, fo);
    line("fleet -> single", ri, xi, fi);
    println!(
        "  per-frame = model view + damage + full-surface redraw \
         (offscreen; excludes swapchain present + vsync)"
    );
    println!("  60fps budget = 16.667 ms/frame (over it, the dive can't hold 60fps)");
}

// ---- calibration pattern (geometry validation) -------------------------

/// Parse a `WxH` size string (e.g. `1400x900`).
fn parse_size(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.split_once('x')?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

/// Write `text` into the char grid at `(row, col)`, clipped to bounds.
fn put(g: &mut [Vec<char>], row: usize, col: usize, text: &str) {
    if row >= g.len() {
        return;
    }
    for (i, ch) in text.chars().enumerate() {
        if col + i < g[row].len() {
            g[row][col + i] = ch;
        }
    }
}

/// A full-screen calibration pattern for a `cols`×`rows` terminal: a box-drawing
/// border flush to the grid edges, tick marks every 10 columns / 5 rows with an
/// interior dot lattice, labelled corners (TL/TR/BL/BR), a centre crosshair and
/// the size. Rendered into a fleet preview it makes geometry bugs obvious — a
/// correct preview shows the border touching the tile edges with an evenly-spaced,
/// square-celled lattice; a stretched one shows an off-centre border or a lattice
/// whose horizontal and vertical spacing differ.
fn calibration_screen(cols: u16, rows: u16) -> String {
    let (cols, rows) = (cols.max(2) as usize, rows.max(2) as usize);
    let mut g = vec![vec![' '; cols]; rows];

    // Border.
    let last = rows - 1;
    for cell in g[0].iter_mut() {
        *cell = '─';
    }
    for cell in g[last].iter_mut() {
        *cell = '─';
    }
    for row in g.iter_mut() {
        row[0] = '│';
        row[cols - 1] = '│';
    }
    g[0][0] = '┌';
    g[0][cols - 1] = '┐';
    g[rows - 1][0] = '└';
    g[rows - 1][cols - 1] = '┘';

    // Ticks every 10 columns / 5 rows, plus an interior dot at each intersection.
    for x in (10..cols - 1).step_by(10) {
        g[0][x] = '┬';
        g[rows - 1][x] = '┴';
    }
    for y in (5..rows - 1).step_by(5) {
        g[y][0] = '├';
        g[y][cols - 1] = '┤';
        for x in (10..cols - 1).step_by(10) {
            g[y][x] = '·';
        }
    }

    // Corner labels just inside, a centre crosshair, and the size below it.
    put(&mut g, 1, 2, "TL");
    put(&mut g, 1, cols.saturating_sub(4), "TR");
    put(&mut g, rows - 2, 2, "BL");
    put(&mut g, rows - 2, cols.saturating_sub(4), "BR");
    let (cx, cy) = (cols / 2, rows / 2);
    g[cy][cx] = '+';
    let label = format!("{cols}x{rows}");
    put(&mut g, cy + 1, cx.saturating_sub(label.len() / 2), &label);

    // Emit each row at an absolute position (no reliance on scroll), in cyan.
    let mut s = String::from("\x1b[2J\x1b[H");
    for (y, row) in g.iter().enumerate() {
        let line: String = row.iter().collect();
        s.push_str(&format!("\x1b[{};1H\x1b[36m{line}\x1b[0m", y + 1));
    }
    s
}

/// Build a fleet scene whose live tiles show the calibration pattern, at window
/// `size` with `count` sessions — the headless way to validate preview geometry
/// (and how card size adapts to the session count) at any size.
fn calib_scene(size: (u32, u32), count: usize) -> ghost_render::Scene {
    let names: Vec<String> = (0..count.max(1)).map(|i| format!("calib-{i:02}")).collect();
    let mine: HashSet<String> = names.iter().cloned().collect();
    let mut sessions = Sessions::new();
    let primary = TerminalModel::new(names[0].clone(), 80, 24, METRICS);
    let primary_id = names[0].clone();
    let primary_view = sessions.adopt(primary);
    let (mut fleet, _) = FleetModel::adopting(
        &sessions,
        primary_id,
        primary_view,
        Vec::new(),
        METRICS,
        size,
        1.0,
        &mine,
    );
    let infos: Vec<_> = names
        .iter()
        .enumerate()
        .map(|(i, n)| info(n, true, &[], i as i32 + 1))
        .collect();
    fleet.update(&mut sessions, &mine, UiEvent::SessionList(infos));
    let cal = calibration_screen(80, 24);
    for n in &names {
        feed(&mut fleet, &mut sessions, &mine, n, &cal);
    }
    fleet.view(&sessions)
}

/// Take over the real terminal: draw the calibration pattern at the terminal's
/// own size on the alternate screen, wait for Enter, then restore. Run it as a
/// ghost session to validate preview geometry live.
fn calib_tui() {
    use std::io::Write;
    let (cols, rows) = rustix::termios::tcgetwinsize(std::io::stdout())
        .map(|w| (w.ws_col, w.ws_row))
        .unwrap_or((80, 24));
    let (cols, rows) = (cols.max(2), rows.max(2));
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b[?1049h\x1b[?25l"); // alt screen, hide cursor
    let _ = write!(out, "{}", calibration_screen(cols, rows));
    let _ = write!(out, "\x1b[{};3H press Enter to exit ", rows); // over the bottom edge
    let _ = out.flush();
    let mut buf = String::new();
    let _ = std::io::stdin().read_line(&mut buf);
    let _ = write!(out, "\x1b[?25h\x1b[?1049l"); // restore
    let _ = out.flush();
}

/// A representative fleet overview: two sessions this window drives (live, in
/// the window's emphasized group block), one held by another window, and one
/// detached — covering the block, both sections, cards, buttons, the focus
/// border, and scaled live previews.
fn fleet_scene(revealed: bool) -> (ghost_render::Scene, u32, u32) {
    let size = (1400u32, 1200u32);
    let mine: HashSet<String> = ["edit", "build"].into_iter().map(String::from).collect();

    // The focused/primary tile carries real content via `adopting`.
    let mut sessions = Sessions::new();
    let primary = TerminalModel::new("edit".to_string(), 80, 24, METRICS);
    let primary_view = sessions.adopt(primary);
    let (mut fleet, _) = FleetModel::adopting(
        &sessions,
        "edit".to_string(),
        primary_view,
        Vec::new(),
        METRICS,
        size,
        1.0,
        &mine,
    );
    fleet.set_my_group(ghost_ui_core::Group::auto("win-shot-0".to_string(), 0));

    fleet.update(
        &mut sessions,
        &mine,
        UiEvent::SessionList(vec![
            info("edit", true, &["nvim", "src/fleet.rs"], 4011),
            info("build", true, &[], 4012),
            info("logs", true, &["journalctl", "-f"], 4099), // attached elsewhere
            info("prod", false, &["ssh", "prod-web-1"], 3777), // detached
            info("batch", false, &["make", "-j8"], 3120),    // a closed group's member
        ]),
    );

    // Live previews for the sessions this window drives.
    feed(&mut fleet, &mut sessions, &mine, "edit", EDIT);
    feed(&mut fleet, &mut sessions, &mine, "build", BUILD);

    // The detached session is observed, and its mirror has the session's OWN
    // grid — a square-ish 100×50 here — so its card takes that shape instead
    // of the window's aspect.
    fleet.update(
        &mut sessions,
        &mine,
        UiEvent::SessionPush {
            name: "prod".to_string(),
            push: ghost_ui_core::SessionPush::Event(ghost_vt::protocol::SessionEvent::Resized {
                cols: 100,
                rows: 50,
            }),
        },
    );
    feed(
        &mut fleet,
        &mut sessions,
        &mine,
        "prod",
        "$ uptime\r\n 14:02:11 up 41 days,  3:07,  1 user\r\n$ ",
    );

    // The window's own group block also remembers "db", dead: its tile stays
    // in the block, previewing its recording's last screen, offering a
    // relaunch on activation. "logs" belongs to another window's group, so
    // it renders in that group's block below the detached pool. "batch" is
    // the member of a CLOSED group — a windowless, remembered set that
    // renders last, dimmed, reopenable wholesale.
    fleet.set_groups(vec![
        ghost_ui_core::Group {
            id: "win-shot-0".to_string(),
            name: "blue".to_string(),
            color: 0,
            members: vec!["edit".to_string(), "build".to_string(), "db".to_string()],
            connection: None,
        },
        ghost_ui_core::Group {
            id: "win-shot-1".to_string(),
            name: "green".to_string(),
            color: 1,
            members: vec!["logs".to_string()],
            connection: None,
        },
        ghost_ui_core::Group {
            id: "win-shot-2".to_string(),
            name: "purple".to_string(),
            color: 3,
            members: vec!["batch".to_string()],
            connection: None,
        },
    ]);
    fleet.update(
        &mut sessions,
        &mine,
        UiEvent::DeadSessions(vec![ghost_ui_core::DeadSession {
            name: "db".to_string(),
            display_name: String::new(),
            command: vec!["psql".to_string(), "prod".to_string()],
            cwd: Some("~/ops".to_string()),
        }]),
    );
    feed(
        &mut fleet,
        &mut sessions,
        &mine,
        "db",
        "prod=# select count(*) from orders;\r\n count \r\n-------\r\n 42917\r\n(1 row)\r\n\r\nprod=# ",
    );

    // The attached-elsewhere content hides behind its toggle by default;
    // `fleet-revealed` renders the expanded state.
    fleet.set_show_elsewhere(revealed);

    let scene = fleet.view(&sessions);
    (scene, size.0, size.1)
}

/// A fleet dive mid-flight: open a fleet window, drive several sessions (so the
/// grid has tiles) and drop into the single view of one. For `out`, press F9 (dive
/// single → fleet) and advance to `at_ms`. For `in`, additionally settle that dive,
/// then press F9 again (dive fleet → single) and advance to `at_ms`. Either way the
/// returned scene is the whole fleet world under the partway camera.
/// The dive runs this long (mirrors the core's default `ANIM_MS`); the sheet
/// samples across it. The tool drives a default-duration `RootModel`, so this is
/// just the matching step size, not the live `GHOST_ANIM_MS` override.
const DIVE_MS: u64 = 180;

/// Build a `count`-session fleet and kick a dive into/out of the *second* session,
/// returning the model and the `base` time whose first tick stamps the dive's start
/// (tick at `base + DIVE_MS * pct / 100` to land at `pct` %).
fn kicked_dive(dir: &str, count: usize) -> (RootModel, Sessions, u64) {
    let size = (1400u32, 900u32);
    let key = |k: NamedKey| UiEvent::Key {
        key: Key::Named(k),
        mods: Mods::NONE,
        kind: KeyEventKind::Press,
        alts: None,
    };
    let names: Vec<String> = ["edit", "build", "logs", "prod", "test", "docs"]
        .iter()
        .take(count.max(1))
        .map(|s| s.to_string())
        .collect();
    // The "second" session (clamped), the one we dive into and out of.
    let target = names[1.min(names.len() - 1)].clone();

    let (mut root, mut states, _) = RootModel::fleet(METRICS, size, 1.0);
    root.update(
        &mut states,
        UiEvent::Resize {
            w_px: size.0,
            h_px: size.1,
            scale: 1.0,
        },
    );
    // Reconcile WITH creation times (oldest first), as the host does and as a real
    // window has already seen by the time it dives. RootModel caches these across the
    // toggle, so the fleet it rebuilds on F9 is in its final order from the start.
    let reconcile = || {
        UiEvent::SessionList(
            names
                .iter()
                .enumerate()
                .map(|(i, n)| {
                    let mut si = info(n, true, &[], i as i32 + 1);
                    si.created_at = Some(i as i64 + 1); // names[0] oldest
                    si
                })
                .collect(),
        )
    };
    root.update(&mut states, reconcile());
    // Each session gets a distinct solid fill so it's obvious *which* session a dive
    // frames: green = first, red = second, then blue/yellow; a full-grid calibration
    // pattern for any beyond. The border/fill is flush to the window-sized grid, so a
    // tile's extent (and which session it is) reads unambiguously at any zoom.
    for (i, n) in names.iter().enumerate() {
        root.update(&mut states, UiEvent::AdoptSession(n.clone()));
        let content = match i {
            0 => solid_screen(0, 200, 0),
            1 => solid_screen(220, 0, 0),
            2 => solid_screen(0, 80, 255),
            3 => solid_screen(230, 200, 0),
            _ => calibration_screen(155, 50),
        };
        root.update(
            &mut states,
            UiEvent::SessionData {
                name: n.clone(),
                bytes: content.into_bytes(),
                ended: false,
            },
        );
    }
    // Make the target the foreground so a dive-out pulls back from it.
    root.update(&mut states, UiEvent::AdoptSession(target.clone()));

    // `base` is well past the settle ticks so its first tick cleanly stamps the start.
    let base = 10_000u64;
    if dir == "in" {
        root.update(&mut states, key(NamedKey::F9)); // → fleet (dive-out)
        root.update(&mut states, UiEvent::Tick { now_ms: 0 });
        root.update(&mut states, UiEvent::Tick { now_ms: 1_000 }); // settle it
        root.update(&mut states, UiEvent::AdoptSession(target)); // dive into the target tile
    } else {
        root.update(&mut states, key(NamedKey::F9)); // single → fleet (dive-out)
        // The host keeps reconciling mid-dive; with the cache seeded above this is a
        // no-op for ordering, so the dive lands in the same order it animated.
        root.update(&mut states, reconcile());
    }
    (root, states, base)
}

/// Render a single full-resolution dive frame at `pct` %, for inspecting detail the
/// downscaled contact sheet can't show (e.g. the handoff at 0 % / 100 %).
fn dive_frame_scene(dir: &str, count: usize, pct: u64) -> (ghost_render::Scene, u32, u32) {
    let (mut root, mut states, base) = kicked_dive(dir, count);
    // The first tick only stamps the dive's start (elapsed 0); a second advances it.
    root.update(&mut states, UiEvent::Tick { now_ms: base });
    root.update(
        &mut states,
        UiEvent::Tick {
            now_ms: base + DIVE_MS * pct.min(100) / 100,
        },
    );
    (root.view(&states), 1400, 900)
}

/// Render a contact sheet of the fleet dive — one tile per 5 % step — so the whole
/// camera motion can be inspected at once. `out` dives single → fleet from the second
/// session; `in` dives fleet → single into it (the "select a session" gesture).
fn zoom_contact_sheet(dir: &str, count: usize, out_path: &str) {
    let (mut root, mut states, base) = kicked_dive(dir, count);
    let font = ghost_shaper::font_from_bytes(FIRA).expect("bundled font loads");
    let mut renderer = Renderer::headless(Theme::default());
    let pcts: Vec<u64> = (0..=100).step_by(5).collect();
    let frames: Vec<Rendered> = pcts
        .iter()
        .map(|pct| {
            root.update(
                &mut states,
                UiEvent::Tick {
                    now_ms: base + DIVE_MS * pct / 100,
                },
            );
            renderer.render_offscreen_scene(&root.view(&states), font, SIZE_PX)
        })
        .collect();

    let sheet = contact_sheet(&frames, &pcts, 4, 340, 219, 8);
    sheet.save_png(out_path).expect("write png");
    println!(
        "wrote {dir}-dive contact sheet ({} frames, {}x{}) to {out_path}",
        frames.len(),
        sheet.width,
        sheet.height
    );
}

/// Nearest-neighbour downscale of an RGBA image to `tw`×`th`.
fn downscale(src: &Rendered, tw: u32, th: u32) -> Vec<u8> {
    let mut out = vec![0u8; (tw * th * 4) as usize];
    for y in 0..th {
        let sy = (y * src.height / th).min(src.height - 1);
        for x in 0..tw {
            let sx = (x * src.width / tw).min(src.width - 1);
            let si = ((sy * src.width + sx) * 4) as usize;
            let di = ((y * tw + x) * 4) as usize;
            out[di..di + 4].copy_from_slice(&src.rgba[si..si + 4]);
        }
    }
    out
}

/// Tile `frames` into a `cols`-wide grid of `tw`×`th` thumbnails on a dark canvas,
/// each with a thin progress bar across its top marking its percent. Frames read
/// left-to-right, top-to-bottom.
fn contact_sheet(
    frames: &[Rendered],
    pcts: &[u64],
    cols: u32,
    tw: u32,
    th: u32,
    gap: u32,
) -> Rendered {
    let n = frames.len() as u32;
    let rows = n.div_ceil(cols);
    let width = cols * tw + (cols + 1) * gap;
    let height = rows * th + (rows + 1) * gap;
    let mut rgba = vec![0u8; (width * height * 4) as usize];
    // Dark canvas (matches the app background) at full alpha.
    for px in rgba.chunks_exact_mut(4) {
        px.copy_from_slice(&[20, 20, 24, 255]);
    }
    let put = |rgba: &mut [u8], x: u32, y: u32, c: [u8; 4]| {
        let di = ((y * width + x) * 4) as usize;
        rgba[di..di + 4].copy_from_slice(&c);
    };
    for (i, frame) in frames.iter().enumerate() {
        let thumb = downscale(frame, tw, th);
        let (cx, cy) = (i as u32 % cols, i as u32 / cols);
        let (ox, oy) = (gap + cx * (tw + gap), gap + cy * (th + gap));
        for y in 0..th {
            for x in 0..tw {
                let si = ((y * tw + x) * 4) as usize;
                put(
                    &mut rgba,
                    ox + x,
                    oy + y,
                    thumb[si..si + 4].try_into().unwrap(),
                );
            }
        }
        // Progress bar across the top edge: white for the elapsed fraction.
        let filled = tw * pcts[i] as u32 / 100;
        for y in 0..3 {
            for x in 0..tw {
                let c = if x < filled {
                    [235, 235, 245, 255]
                } else {
                    [60, 60, 70, 255]
                };
                put(&mut rgba, ox + x, oy + y, c);
            }
        }
    }
    Rendered {
        width,
        height,
        rgba,
    }
}

/// The single-terminal view, for comparison / regression on the same content.
fn single_scene() -> (ghost_render::Scene, u32, u32) {
    let size = (1100u32, 700u32);
    let mut model = TerminalModel::new("edit".to_string(), 80, 24, METRICS);
    model.update(UiEvent::Resize {
        w_px: size.0,
        h_px: size.1,
        scale: 1.0,
    });
    model.update(UiEvent::SessionData {
        name: "edit".to_string(),
        bytes: EDIT.as_bytes().to_vec(),
        ended: false,
    });
    let (root, states) = RootModel::single(model, METRICS, size);
    (root.view(&states), size.0, size.1)
}

fn feed(
    fleet: &mut FleetModel,
    sessions: &mut Sessions,
    mine: &HashSet<String>,
    name: &str,
    content: &str,
) {
    fleet.update(
        sessions,
        mine,
        UiEvent::SessionData {
            name: name.to_string(),
            bytes: content.as_bytes().to_vec(),
            ended: false,
        },
    );
}

fn info(name: &str, attached: bool, command: &[&str], pid: i32) -> SessionInfo {
    SessionInfo {
        name: name.to_string(),
        pid,
        created_at: None,
        title: name.to_string(),
        command: command.iter().map(|s| s.to_string()).collect(),
        attached,
        bell: false,
        display_name: String::new(),
        cwd: None,
        size: None,
        connection: None,
    }
}

const EDIT: &str = "\x1b[1;34m~/ghost/ghost-ui\x1b[0m\r\n\
\x1b[38;5;240m  1\x1b[0m \x1b[35mfn\x1b[0m \x1b[33mfleet_scene\x1b[0m() -> Scene {\r\n\
\x1b[38;5;240m  2\x1b[0m     \x1b[35mlet\x1b[0m mine = [\x1b[32m\"edit\"\x1b[0m, \x1b[32m\"build\"\x1b[0m];\r\n\
\x1b[38;5;240m  3\x1b[0m     \x1b[35mlet\x1b[0m (fleet, _) = FleetModel::adopting(..);\r\n\
\x1b[38;5;240m  4\x1b[0m     fleet.view()\r\n\
\x1b[38;5;240m  5\x1b[0m }\r\n";

const BUILD: &str = "\x1b[1;32m~/ghost\x1b[0m $ cargo test -p ghost-ui-core\r\n\
\x1b[32m   Compiling\x1b[0m ghost-ui-core v0.1.0\r\n\
\x1b[32m    Finished\x1b[0m test profile in 1.39s\r\n\
\x1b[1mrunning 134 tests\x1b[0m\r\n\
\x1b[32mtest result: ok.\x1b[0m 134 passed; 0 failed\r\n\
$ \x1b[5m_\x1b[0m\r\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_reads_wxh() {
        assert_eq!(parse_size("1400x900"), Some((1400, 900)));
        assert_eq!(parse_size("720x432"), Some((720, 432)));
        assert_eq!(parse_size("nope"), None);
        assert_eq!(parse_size("12xy"), None);
    }

    #[test]
    fn calibration_screen_has_border_corners_and_size() {
        let s = calibration_screen(80, 24);
        for corner in ['┌', '┐', '└', '┘'] {
            assert!(s.contains(corner), "missing corner {corner}");
        }
        assert!(s.contains("80x24"), "missing size label");
        assert!(
            s.contains("TL") && s.contains("BR"),
            "missing corner labels"
        );
    }

    /// ANSI that paints a distinct 2×2-cell colour block in each corner of a
    /// `cols`×`rows` grid (TL red, TR green, BL blue, BR yellow), on a grey fill.
    // Only the GPU dive test uses this; gate it with that test to avoid a
    // dead-code warning on non-Linux where the test is compiled out.
    #[cfg(target_os = "linux")]
    fn corner_markers(cols: u16, rows: u16) -> String {
        use std::fmt::Write;
        let mut s = String::new();
        let _ = write!(s, "\x1b[48;2;30;30;30m\x1b[2J"); // grey fill
        let mut blk = |row: u16, col: u16, r: u8, g: u8, b: u8| {
            for dr in 0..2u16 {
                let _ = write!(s, "\x1b[{};{}H\x1b[48;2;{r};{g};{b}m  ", row + dr, col);
            }
        };
        blk(1, 1, 255, 0, 0); // TL red
        blk(1, cols - 1, 0, 255, 0); // TR green
        blk(rows - 1, 1, 0, 0, 255); // BL blue
        blk(rows - 1, cols - 1, 255, 255, 0); // BR yellow
        let _ = write!(s, "\x1b[0m");
        s
    }

    /// Bounding-box centre of pixels matching `rgb` within `tol`; None if absent.
    #[cfg(target_os = "linux")]
    fn find(img: &Rendered, rgb: [u8; 3], tol: i16) -> Option<(f32, f32)> {
        let (mut minx, mut miny, mut maxx, mut maxy) = (u32::MAX, u32::MAX, 0u32, 0u32);
        let mut n = 0u64;
        for y in 0..img.height {
            for x in 0..img.width {
                let i = ((y * img.width + x) * 4) as usize;
                let p = &img.rgba[i..i + 3];
                if (p[0] as i16 - rgb[0] as i16).abs() <= tol
                    && (p[1] as i16 - rgb[1] as i16).abs() <= tol
                    && (p[2] as i16 - rgb[2] as i16).abs() <= tol
                {
                    minx = minx.min(x);
                    miny = miny.min(y);
                    maxx = maxx.max(x);
                    maxy = maxy.max(y);
                    n += 1;
                }
            }
        }
        (n > 0).then(|| ((minx + maxx) as f32 / 2.0, (miny + maxy) as f32 / 2.0))
    }

    // The dive's full-zoom endpoints (dive-out start, dive-in end) must frame the
    // session exactly like the single view — corner markers land in the same place,
    // none clipped off-screen. GPU test: needs the lavapipe ICD, so it is Linux-CI
    // only — macOS has no software fallback adapter (the sibling parse/layout tests
    // here are pure and stay).
    #[cfg(target_os = "linux")]
    #[test]
    fn dive_full_zoom_aligns_the_session_with_the_single_view() {
        let size = (1400u32, 900u32);
        let font = ghost_shaper::font_from_bytes(FIRA).expect("bundled font loads");
        let mut renderer = Renderer::headless(Theme::default());
        let colors = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [255, 255, 0]];

        // A single session sized to the window; learn its grid from the scene.
        let (mut root, mut states) = RootModel::single(
            TerminalModel::new("m".to_string(), 80, 24, METRICS),
            METRICS,
            size,
        );
        root.update(
            &mut states,
            UiEvent::Resize {
                w_px: size.0,
                h_px: size.1,
                scale: 1.0,
            },
        );
        let (cols, rows) = match root.view(&states).terminals().next().unwrap() {
            ghost_render::SceneItem::Terminal { frame, .. } => {
                (frame.cols as u16, frame.rows as u16)
            }
            _ => unreachable!(),
        };
        root.update(
            &mut states,
            UiEvent::SessionData {
                name: "m".to_string(),
                bytes: corner_markers(cols, rows).into_bytes(),
                ended: false,
            },
        );

        // Reference: the single (full-window) view.
        let single = renderer.render_offscreen_scene(&root.view(&states), font, SIZE_PX);
        let want: Vec<(f32, f32)> = colors
            .iter()
            .enumerate()
            .map(|(i, c)| {
                find(&single, *c, 40).unwrap_or_else(|| panic!("single view missing corner {i}"))
            })
            .collect();

        // Dive-out start: full zoom. Should match the single view.
        let key = UiEvent::Key {
            key: Key::Named(NamedKey::F9),
            mods: Mods::NONE,
            kind: KeyEventKind::Press,
            alts: None,
        };
        root.update(&mut states, key);
        root.update(
            &mut states,
            UiEvent::SessionList(vec![info("m", true, &[], 1)]),
        );
        root.update(&mut states, UiEvent::Tick { now_ms: 10_000 }); // stamp t0 → progress 0 = full zoom
        let dive = renderer.render_offscreen_scene(&root.view(&states), font, SIZE_PX);

        for (i, c) in colors.iter().enumerate() {
            let (wx, wy) = want[i];
            let (dx, dy) = find(&dive, *c, 40)
                .unwrap_or_else(|| panic!("dive lost corner {i} (clipped off-screen?)"));
            assert!(
                (dx - wx).abs() < 2.0 && (dy - wy).abs() < 2.0,
                "corner {i} misaligned: single=({wx:.0},{wy:.0}) dive=({dx:.0},{dy:.0})"
            );
        }
    }
}
