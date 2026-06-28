//! `ghost-shot` — render a ghost-ui scene to a PNG, headlessly, so we can *look*
//! at what the renderer draws without launching the windowed app.
//!
//! It drives the real models (`FleetModel`, `RootModel`, `TerminalModel`) with
//! synthetic-but-representative sessions, asks them for a `Scene`, and rasterizes
//! it through the same `ghost-renderer` the app uses (software adapter). This is
//! the first-class visual-debugging path: change the UI, run the tool, eyeball
//! the image.
//!
//! Usage: `ghost-shot <fleet|single> [out.png]` (default `ghost-shot.png`), or
//! `ghost-shot bench [tiles] [frames]` (arrow-nav) / `ghost-shot bench resize
//! [tiles] [steps]` (window resize) for the headless performance benchmarks.

use std::collections::HashSet;

use ghost_render::CellMetrics;
use ghost_renderer::{Renderer, Theme};
use ghost_ui_core::{
    FleetModel, Key, KeyEventKind, Mods, NamedKey, RootModel, TerminalModel, UiEvent,
};
use ghost_vt::session::SessionInfo;

/// A bundled font so the tool needs no system font lookup.
const FIRA: &[u8] = include_bytes!("../../shaper/tests/assets/FiraCode-Regular.ttf");

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
        // `zoom [out.png] [now_ms]` — render a fleet zoom mid-flight (F9 from the
        // single view), so the camera animation can be eyeballed at any progress.
        // The animation runs ~180ms, so `now_ms` ≈ 90 is mid-zoom; 0 is fully in.
        let out = args
            .next()
            .unwrap_or_else(|| "ghost-shot-zoom.png".to_string());
        let at = args
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(90);
        let (scene, w, h) = zoom_scene(at);
        let font = ghost_shaper::font_from_bytes(FIRA).expect("bundled font loads");
        let mut renderer = Renderer::headless(Theme::default());
        let img = renderer.render_offscreen_scene(&scene, font, SIZE_PX);
        img.save_png(&out).expect("write png");
        println!("wrote zoom frame at now_ms={at} ({w}x{h}) to {out}");
        return;
    }

    let out = args.next().unwrap_or_else(|| "ghost-shot.png".to_string());
    let (scene, w, h) = match which.as_str() {
        "fleet" => fleet_scene(),
        "single" => single_scene(),
        other => {
            eprintln!(
                "unknown scene '{other}' (expected: fleet | single | zoom | bench | calib | calib-tui)"
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
    let (mut root, _) = RootModel::fleet(METRICS, size, 1.0);
    root.update(UiEvent::Resize {
        w_px: size.0,
        h_px: size.1,
        scale: 1.0,
    });
    // Attach `tiles` sessions the way the shell does (spawn / take-over reply with
    // AdoptSession), each producing a full screen of output.
    for i in 0..tiles {
        let name = format!("s{i}");
        root.update(UiEvent::AdoptSession(name.clone()));
        root.update(UiEvent::SessionData {
            name,
            bytes: dense_screen().into_bytes(),
            ended: false,
        });
    }
    // Open the fleet overview (F9).
    root.update(key(NamedKey::F9));

    let font = ghost_shaper::font_from_bytes(FIRA).expect("bundled font loads");
    let mut renderer = Renderer::headless(Theme::default());
    let px = |r: &RootModel| SIZE_PX * r.render_scale();

    // Warm up (first frame builds the glyph atlas + shaping/frame caches).
    let _ = renderer.render_offscreen_scene(&root.view(), font, px(&root));

    let dirs = [
        NamedKey::ArrowDown,
        NamedKey::ArrowRight,
        NamedKey::ArrowUp,
        NamedKey::ArrowLeft,
    ];
    let (mut model_ns, mut render_ns) = (0u128, 0u128);
    for i in 0..frames {
        let nav = Instant::now();
        root.update(key(dirs[i % dirs.len()]));
        let scene = root.view();
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
    let (mut root, _) = RootModel::fleet(METRICS, base, 1.0);
    root.update(UiEvent::Resize {
        w_px: base.0,
        h_px: base.1,
        scale: 1.0,
    });
    for i in 0..tiles {
        let name = format!("s{i}");
        root.update(UiEvent::AdoptSession(name.clone()));
        root.update(UiEvent::SessionData {
            name,
            bytes: dense_screen().into_bytes(),
            ended: false,
        });
    }
    root.update(key(NamedKey::F9));

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
    let _ = renderer.render_offscreen_scene(&root.view(), font, px(&root));

    // Old behaviour: every drag step relayouts the model and re-rasters every tile.
    let mut relayout_ns = 0u128;
    for &(w, h) in &sizes {
        let t = Instant::now();
        root.update(UiEvent::Resize {
            w_px: w,
            h_px: h,
            scale: 1.0,
        });
        let scene = root.view();
        let _ = renderer.render_offscreen_scene(&scene, font, px(&root));
        relayout_ns += t.elapsed().as_nanos();
    }

    // New behaviour: capture once at the gesture start, then stretch-blit each step.
    root.update(UiEvent::Resize {
        w_px: base.0,
        h_px: base.1,
        scale: 1.0,
    });
    renderer.capture_snapshot(&root.view(), font, px(&root));
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
    let primary = TerminalModel::new(names[0].clone(), 80, 24, METRICS);
    let (mut fleet, _) = FleetModel::adopting(primary, Vec::new(), METRICS, size, 1.0, mine);
    let infos: Vec<_> = names
        .iter()
        .enumerate()
        .map(|(i, n)| info(n, true, &[], i as i32 + 1))
        .collect();
    fleet.update(UiEvent::SessionList(infos));
    let cal = calibration_screen(80, 24);
    for n in &names {
        feed(&mut fleet, n, &cal);
    }
    fleet.view()
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

/// A representative fleet overview: two sessions this window drives (live),
/// one held by another window, and one detached — covering all three sections,
/// cards, buttons, the focus border, and scaled live previews.
fn fleet_scene() -> (ghost_render::Scene, u32, u32) {
    let size = (1400u32, 900u32);
    let mine: HashSet<String> = ["edit", "build"].into_iter().map(String::from).collect();

    // The focused/primary tile carries real content via `adopting`.
    let primary = TerminalModel::new("edit".to_string(), 80, 24, METRICS);
    let (mut fleet, _) = FleetModel::adopting(primary, Vec::new(), METRICS, size, 1.0, mine);

    fleet.update(UiEvent::SessionList(vec![
        info("edit", true, &["nvim", "src/fleet.rs"], 4011),
        info("build", true, &[], 4012),
        info("logs", true, &["journalctl", "-f"], 4099), // attached elsewhere
        info("prod", false, &["ssh", "prod-web-1"], 3777), // detached
    ]));

    // Live previews for the sessions this window drives.
    feed(&mut fleet, "edit", EDIT);
    feed(&mut fleet, "build", BUILD);

    let scene = fleet.view();
    (scene, size.0, size.1)
}

/// A fleet zoom mid-flight: open a fleet window, drive several sessions (so the
/// grid has tiles), drop into the single view of one, then press F9 (zoom out) and
/// advance the clock to `at_ms`. The returned scene is the whole fleet world under
/// the partway camera — the spatial dive frozen at one frame.
fn zoom_scene(at_ms: u64) -> (ghost_render::Scene, u32, u32) {
    let size = (1400u32, 900u32);
    let key = |k: NamedKey| UiEvent::Key {
        key: Key::Named(k),
        mods: Mods::NONE,
        kind: KeyEventKind::Press,
        alts: None,
    };
    let (mut root, _) = RootModel::fleet(METRICS, size, 1.0);
    root.update(UiEvent::Resize {
        w_px: size.0,
        h_px: size.1,
        scale: 1.0,
    });
    let names = ["edit", "build", "logs", "prod", "test", "docs"];
    root.update(UiEvent::SessionList(
        names
            .iter()
            .enumerate()
            .map(|(i, n)| info(n, true, &[], i as i32 + 1))
            .collect(),
    ));
    let cal = calibration_screen(80, 24);
    for n in names {
        root.update(UiEvent::AdoptSession(n.to_string()));
        root.update(UiEvent::SessionData {
            name: n.to_string(),
            bytes: cal.clone().into_bytes(),
            ended: false,
        });
    }
    // Now in the single view of `docs`. F9 zooms out into the fleet; stamp the
    // animation start (now_ms 0), then advance to `at_ms`.
    root.update(key(NamedKey::F9));
    root.update(UiEvent::Tick { now_ms: 0 });
    root.update(UiEvent::Tick { now_ms: at_ms });
    (root.view(), size.0, size.1)
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
    let root = RootModel::single(model, METRICS, size);
    (root.view(), size.0, size.1)
}

fn feed(fleet: &mut FleetModel, name: &str, content: &str) {
    fleet.update(UiEvent::SessionData {
        name: name.to_string(),
        bytes: content.as_bytes().to_vec(),
        ended: false,
    });
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
    }
}

const EDIT: &str = "\x1b[1;34m~/ghost/frontends/ghost-ui\x1b[0m\r\n\
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
}
