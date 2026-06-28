//! End-to-end render tests: feed real text (and SGR colors) through a
//! `ghost_term::Vt`, lay it out with `ghost-render`, shape + rasterize with
//! `ghost-shaper`, and draw it on the GPU — asserting on the read-back pixels
//! and dumping PNGs to eyeball. Runs headless on lavapipe.

use ghost_render::{
    CellMetrics, Layer, RectPx, Scene, SceneId, SceneItem, Selection, Transform, layout_frame,
};
use ghost_renderer::{Rendered, Renderer, Theme, render_frame};
use ghost_term::Vt;

const FIRA: &[u8] = include_bytes!("../../shaper/tests/assets/FiraCode-Regular.ttf");

const METRICS: CellMetrics = CellMetrics {
    advance: 9.0,
    line_height: 18.0,
};

fn write_png(name: &str, img: &Rendered) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(name);
    let file = std::fs::File::create(&path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), img.width, img.height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().expect("png header");
    writer.write_image_data(&img.rgba).expect("png data");
    path
}

fn px(img: &Rendered, x: u32, y: u32) -> [u8; 4] {
    let i = ((y * img.width + x) * 4) as usize;
    [
        img.rgba[i],
        img.rgba[i + 1],
        img.rgba[i + 2],
        img.rgba[i + 3],
    ]
}

/// Count pixels in a cell-column band [x0, x1) over the full row height that
/// satisfy `pred`, plus the total scanned.
fn band<F: Fn([u8; 4]) -> bool>(img: &Rendered, x0: u32, x1: u32, pred: F) -> (usize, usize) {
    let mut hits = 0;
    let mut total = 0;
    for x in x0..x1 {
        for y in 0..18 {
            total += 1;
            if pred(px(img, x, y)) {
                hits += 1;
            }
        }
    }
    (hits, total)
}

fn strong_red(p: [u8; 4]) -> bool {
    p[0] > 90 && p[1] < 60 && p[2] < 60
}
fn strong_blue(p: [u8; 4]) -> bool {
    p[2] > 100 && p[0] < 60 && p[1] < 60
}

#[test]
fn renders_ligature_line_to_image() {
    let font = ghost_shaper::font_from_bytes(FIRA).expect("font");

    // The headline claim: Fira Code substitutes "!=" into a single ligature
    // glyph rather than rendering '!' then '='. Prove it at the shaper level so
    // the pixels below are actually drawing a ligature.
    let shaped: Vec<u16> = ghost_shaper::shape(font, "!=", 15.0)
        .iter()
        .map(|g| g.id)
        .collect();
    let naive = vec![
        ghost_shaper::glyph_id(font, '!'),
        ghost_shaper::glyph_id(font, '='),
    ];
    assert_ne!(
        shaped, naive,
        "Fira Code should substitute != into a ligature"
    );

    let mut vt = Vt::new(40, 3);
    vt.feed_str("fn ok() { a != b && c => d } // -> go");
    let frame = layout_frame(&vt, METRICS);
    let img = render_frame(&frame, font, 15.0, Theme::default());

    assert_eq!(img.width, 360, "40 cols * 9px advance");
    assert_eq!(img.height, 54, "3 rows * 18px line height");
    assert_eq!(img.rgba.len() as u32, img.width * img.height * 4);

    let bg = [16i32, 16, 18];
    let lit = img
        .rgba
        .chunks_exact(4)
        .filter(|p| {
            (i32::from(p[0]) - bg[0]).abs() > 12
                || (i32::from(p[1]) - bg[1]).abs() > 12
                || (i32::from(p[2]) - bg[2]).abs() > 12
        })
        .count();
    assert!(
        lit > 300,
        "expected glyph pixels, only {lit} differ from bg"
    );

    let path = write_png("ghost_ligature_sample.png", &img);
    eprintln!("WROTE PNG: {} (lit={lit})", path.display());
}

#[test]
fn resolves_ansi_colors_and_backgrounds() {
    // Hide the cursor (?25l) so a cursor block can't perturb the sampled bands,
    // then: "AB" red fg, a blank, "CD" on a blue background.
    let mut vt = Vt::new(40, 1);
    vt.feed_str("\x1b[?25l\x1b[31mAB\x1b[0m \x1b[44mCD\x1b[0m");

    let frame = layout_frame(&vt, METRICS);
    let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
    let img = render_frame(&frame, font, 15.0, Theme::default());
    let path = write_png("ghost_color_sample.png", &img);
    eprintln!("WROTE PNG: {}", path.display());

    // Red foreground (cols 0..1 -> x 0..18): strokes over the dark bg, so red
    // pixels are present but it is NOT a solid fill.
    let (red_hits, red_total) = band(&img, 0, 18, strong_red);
    assert!(red_hits > 15, "expected red fg strokes, got {red_hits}");
    assert!(
        red_hits * 2 < red_total,
        "red fg should be strokes, not a fill ({red_hits}/{red_total})"
    );

    // Blue background (cols 3..4 -> x 27..45): a filled rect, so most pixels are
    // strongly blue even with light glyphs on top.
    let (blue_hits, blue_total) = band(&img, 27, 45, strong_blue);
    assert!(
        blue_hits * 2 > blue_total,
        "blue bg cell should be mostly filled ({blue_hits}/{blue_total})"
    );

    // The blank column between them (col 2 -> x 18..27) must carry no color:
    // catches fg/bg bleed into neighbouring cells.
    let (red_bleed, _) = band(&img, 18, 27, strong_red);
    let (blue_bleed, _) = band(&img, 18, 27, strong_blue);
    assert_eq!(red_bleed, 0, "red bled into the blank column");
    assert_eq!(blue_bleed, 0, "blue bled into the blank column");
}

#[test]
fn highlights_a_selection_band() {
    // Hide the cursor, print "hello world", and select "hello" (cols 0..=4).
    let mut vt = Vt::new(40, 1);
    vt.feed_str("\x1b[?25lhello world");
    let frame = layout_frame(&vt, METRICS);
    let font = ghost_shaper::font_from_bytes(FIRA).expect("font");

    let mut renderer = Renderer::headless(Theme::default());
    renderer.set_selection(Some(Selection::new((0, 0), (0, 4))));
    let img = renderer.render_offscreen(&frame, font, 15.0);
    let path = write_png("ghost_selection_sample.png", &img);
    eprintln!("WROTE PNG: {}", path.display());

    // The selection tint is bluish and well above the near-neutral background;
    // glyphs drawn on top read as light gray (blue ~= red), so this predicate
    // catches the tint fill but not the glyphs or the bg.
    let tinted = |p: [u8; 4]| p[2] > 45 && i32::from(p[2]) > i32::from(p[0]) + 8;

    // Selected cells 0..=4 (x 0..45) are mostly filled with the tint.
    let (hits, total) = band(&img, 0, 45, tinted);
    assert!(
        hits * 2 > total,
        "selected band should be mostly tinted ({hits}/{total})"
    );

    // "world" (cols 6..=10, x 54..99) is outside the selection — no tint.
    let (bleed, _) = band(&img, 54, 99, tinted);
    assert_eq!(bleed, 0, "selection tint bled past its range ({bleed})");
}

#[test]
fn draws_block_cursor_at_prompt_position() {
    // After "hi" the cursor sits on a trailing blank cell (col 2) — the usual
    // prompt position. The block cursor must still be drawn there.
    let mut vt = Vt::new(10, 1);
    vt.feed_str("hi");
    let frame = layout_frame(&vt, METRICS);
    let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
    let img = render_frame(&frame, font, 15.0, Theme::default());

    // The cursor block fills col 2 (x 18..27) in the (light) foreground color.
    let (light, total) = band(&img, 18, 27, |p| p[0] > 180 && p[1] > 180 && p[2] > 180);
    assert!(
        light * 2 > total,
        "cursor block should fill the cell at col 2 ({light}/{total})"
    );
}

/// Pixels in a sub-rectangle [x0,x1)×[y0,y1) satisfying `pred`, plus the total.
fn rect<F: Fn([u8; 4]) -> bool>(
    img: &Rendered,
    x0: u32,
    x1: u32,
    y0: u32,
    y1: u32,
    pred: F,
) -> (usize, usize) {
    let mut hits = 0;
    let mut total = 0;
    for x in x0..x1 {
        for y in y0..y1 {
            total += 1;
            if pred(px(img, x, y)) {
                hits += 1;
            }
        }
    }
    (hits, total)
}

fn light(p: [u8; 4]) -> bool {
    p[0] > 180 && p[1] > 180 && p[2] > 180
}

#[test]
fn draws_underline_cursor_along_the_cell_bottom() {
    // DECSCUSR 4 (steady underline): a thin rule on the cell bottom, with the
    // cell otherwise untouched (no block fill).
    let mut vt = Vt::new(10, 1);
    vt.feed_str("hi\x1b[4 q");
    let frame = layout_frame(&vt, METRICS);
    let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
    let img = render_frame(&frame, font, 15.0, Theme::default());

    // The bottom of the cursor cell (col 2, x 18..27) is lit; the upper part is
    // blank (no glyph, no block fill).
    let (bottom, bt) = rect(&img, 18, 27, 16, 18, light);
    let (top, _) = rect(&img, 18, 27, 1, 13, light);
    assert!(
        bottom * 2 > bt,
        "underline cursor should light the cell bottom ({bottom}/{bt})"
    );
    assert_eq!(
        top, 0,
        "underline cursor must not fill the cell body ({top})"
    );
}

#[test]
fn draws_bar_cursor_along_the_cell_leading_edge() {
    // DECSCUSR 6 (steady bar): a thin vertical rule at the cell's left edge.
    let mut vt = Vt::new(10, 1);
    vt.feed_str("hi\x1b[6 q");
    let frame = layout_frame(&vt, METRICS);
    let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
    let img = render_frame(&frame, font, 15.0, Theme::default());

    // The leading edge of the cursor cell (col 2, x 18..) is lit top-to-bottom;
    // the rest of the cell is blank.
    let (edge, et) = rect(&img, 18, 20, 0, 18, light);
    let (body, _) = rect(&img, 22, 27, 0, 18, light);
    assert!(
        edge * 2 > et,
        "bar cursor should light the cell leading edge ({edge}/{et})"
    );
    assert_eq!(body, 0, "bar cursor must not fill the cell body ({body})");
}

#[test]
fn translucent_theme_makes_default_background_see_through() {
    // Hide the cursor, then print "A" on a blue background (col 0). With a
    // half-opaque theme, the default-background area (no quad, just the clear)
    // must read translucent, while the explicitly-coloured cell stays opaque —
    // the standard terminal-transparency behaviour.
    let mut vt = Vt::new(40, 1);
    vt.feed_str("\x1b[?25l\x1b[44mA");
    let frame = layout_frame(&vt, METRICS);
    let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
    let theme = Theme {
        bg_alpha: 0.5,
        ..Theme::default()
    };
    let img = render_frame(&frame, font, 15.0, theme);

    // A blank far-right cell (col 10, x ~94) carries only the clear: ~half alpha.
    let blank = px(&img, 94, 9);
    assert!(
        (100..=160).contains(&(blank[3] as u32)),
        "default background should be ~half transparent, got alpha {}",
        blank[3]
    );
    // The blue-background cell (col 0) stays fully opaque.
    let colored = px(&img, 4, 9);
    assert_eq!(
        colored[3], 255,
        "an SGR background must stay opaque, got alpha {}",
        colored[3]
    );
}

#[test]
fn scales_a_large_preview_frame_to_fit_its_tile() {
    // A real-size session frame drawn into a tile smaller than itself must be
    // scaled to "contain", not clipped. Mark the bottom-right cell blue: at 1:1
    // it lands outside the tile (scissored away); scaled, it appears inside the
    // tile's lower-right.
    let mut vt = Vt::new(10, 4);
    vt.feed_str("\x1b[?25l\x1b[4;10H\x1b[44m \x1b[0m"); // blue bg at row 4, col 10
    let frame = layout_frame(&vt, METRICS);
    // Frame is 90x72 px (10*9 x 4*18).
    let font = ghost_shaper::font_from_bytes(FIRA).expect("font");

    // A 90x72 surface with the frame drawn into the top-left 45x36 tile (0.5x).
    let mut scene = Scene::new((90, 72));
    scene.layers.push(Layer::new(
        0,
        vec![SceneItem::Terminal {
            id: SceneId::Tile(0),
            rect: RectPx {
                x: 0.0,
                y: 0.0,
                w: 45.0,
                h: 36.0,
            },
            frame,
            selection: None,
            dim: false,
        }],
    ));
    let mut renderer = Renderer::headless(Theme::default());
    let img = renderer.render_offscreen_scene(&scene, font, 15.0);
    let path = write_png("ghost_scaled_preview_sample.png", &img);
    eprintln!("WROTE PNG: {}", path.display());

    // The blue marker (frame cell at 81,54) maps to ~(40,27) at 0.5x: inside the
    // tile's lower-right quadrant.
    let (inside, _) = rect(&img, 36, 45, 27, 36, strong_blue);
    assert!(
        inside > 0,
        "scaled preview should bring the bottom-right cell inside the tile"
    );
    // Nothing draws outside the tile rect (x>=45 or y>=36 stays clear).
    let (outside, _) = rect(&img, 45, 90, 0, 72, strong_blue);
    assert_eq!(outside, 0, "preview must stay within its tile ({outside})");
}

#[test]
fn an_unchanged_preview_is_not_re_rasterized() {
    // Two scaled previews (a real-size frame shrunk into a small tile). Rendering
    // the scene twice must re-render zero preview textures the second time — the
    // cache turns fleet nav and idle tiles into cheap blits, the whole point of RTT.
    let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
    let frame = {
        let mut vt = Vt::new(80, 24); // 720x432 px at METRICS
        vt.feed_str("preview content => fn main() { let answer = 42; }");
        layout_frame(&vt, METRICS)
    };
    let tile = |id, x: f32| SceneItem::Terminal {
        id,
        rect: RectPx {
            x,
            y: 0.0,
            w: 360.0,
            h: 216.0,
        }, // 0.5x: preview_scale < 1
        frame: frame.clone(),
        selection: None,
        dim: false,
    };
    let mut scene = Scene::new((800, 240));
    scene.layers.push(Layer::new(
        0,
        vec![tile(SceneId::Tile(0), 0.0), tile(SceneId::Tile(1), 380.0)],
    ));

    let mut r = Renderer::headless(Theme::default());
    let _ = r.render_offscreen_scene(&scene, font, 15.0);
    assert_eq!(
        r.preview_renders(),
        2,
        "the first paint renders both preview textures"
    );
    let _ = r.render_offscreen_scene(&scene, font, 15.0);
    assert_eq!(
        r.preview_renders(),
        2,
        "an unchanged repaint blits the cache, re-rasterizing nothing"
    );
}

#[test]
fn a_resize_blit_scales_the_snapshot_without_reshaping() {
    // During an interactive resize the shell captures the last crisp scene to a
    // texture and stretch-blits it to the changing surface, deferring the real
    // relayout. The blit must reproduce the snapshot (scaled) yet re-shape and
    // re-rasterize NOTHING — that cheapness is the whole reason it exists.
    let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
    let mut r = Renderer::headless(Theme::default());

    // Text (so shaping happens) over a full green backdrop (a deterministic
    // colour to find after the stretch).
    let frame = {
        let mut vt = Vt::new(20, 4); // 180x72 px at METRICS
        vt.feed_str("snapshot text => fn main() {}");
        layout_frame(&vt, METRICS)
    };
    let mut scene = Scene::new((180, 72));
    scene.layers.push(Layer::new(
        0,
        vec![SceneItem::Rect {
            id: SceneId::Root,
            rect: RectPx {
                x: 0.0,
                y: 0.0,
                w: 180.0,
                h: 72.0,
            },
            color: [0.0, 1.0, 0.0, 1.0], // opaque green
            radius: 0.0,
        }],
    ));
    scene.layers.push(Layer::new(
        1,
        vec![SceneItem::Terminal {
            id: SceneId::Root,
            rect: RectPx {
                x: 0.0,
                y: 0.0,
                w: 180.0,
                h: 72.0,
            },
            frame,
            selection: None,
            dim: false,
        }],
    ));

    // Capture the snapshot — one real render, which shapes the text.
    r.capture_snapshot(&scene, font, 15.0);
    assert!(r.has_snapshot(), "a snapshot is held after capture");
    let shapes = r.shape_misses();
    assert!(shapes > 0, "capturing the snapshot shaped its runs");

    // Stretch-blit it to 2x: reshape and re-render nothing.
    let big = r.blit_snapshot_offscreen(360, 144);
    assert_eq!((big.width, big.height), (360, 144));
    assert_eq!(
        r.shape_misses(),
        shapes,
        "a snapshot blit re-shapes nothing"
    );
    assert_eq!(
        r.preview_renders(),
        0,
        "a snapshot blit renders no previews"
    );

    // The green backdrop survives the scaled blit: sample a lower row (the text
    // is on row 0; rows 1..3 are blank, so the rect shows through there).
    let is_green = |p: [u8; 4]| p[0] < 0x40 && p[1] > 0xa0 && p[2] < 0x40;
    assert!(
        is_green(px(&big, 180, 110)),
        "the blit must reproduce the (scaled) snapshot, got {:?}",
        px(&big, 180, 110)
    );

    // A second blit at a different size is also free.
    let _ = r.blit_snapshot_offscreen(270, 108);
    assert_eq!(r.shape_misses(), shapes, "repeated blits stay free");

    // Clearing the snapshot ends the resize path.
    r.clear_snapshot();
    assert!(!r.has_snapshot(), "clear_snapshot drops the held frame");
}

#[test]
fn an_identical_repaint_reshapes_nothing() {
    // Shaping dominates per-frame CPU. The cache must make a repaint of unchanged
    // text re-shape nothing, so fleet navigation and idle previews stay cheap.
    let mut vt = Vt::new(40, 6);
    vt.feed_str("fn main() != ok { let x = 1; }\r\nsecond line of text => here");
    let frame = layout_frame(&vt, METRICS);
    let font = ghost_shaper::font_from_bytes(FIRA).expect("font");

    let mut scene = Scene::new((360, 108)); // 40*9 x 6*18
    scene.layers.push(Layer::new(
        0,
        vec![SceneItem::Terminal {
            id: SceneId::Tile(0),
            rect: RectPx {
                x: 0.0,
                y: 0.0,
                w: 360.0,
                h: 108.0,
            },
            frame,
            selection: None,
            dim: false,
        }],
    ));

    let mut r = Renderer::headless(Theme::default());
    let _ = r.render_offscreen_scene(&scene, font, 15.0);
    let after_first = r.shape_misses();
    assert!(after_first > 0, "the first paint shapes its runs");
    let _ = r.render_offscreen_scene(&scene, font, 15.0);
    assert_eq!(
        r.shape_misses(),
        after_first,
        "an identical repaint must re-shape nothing (shaping is cached)"
    );
}

#[test]
fn a_layer_transform_moves_and_scales_its_content() {
    // A red square at layer-space (0,0,20,20) in a layer scaled 2x and shifted by
    // (40,40) must be drawn on screen at (40,40)..(80,80) — the camera the
    // spatial-navigation zoom rides on.
    let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
    let mut scene = Scene::new((100, 100));
    scene.layers.push(
        Layer::new(
            0,
            vec![SceneItem::Rect {
                id: SceneId::Root,
                rect: RectPx {
                    x: 0.0,
                    y: 0.0,
                    w: 20.0,
                    h: 20.0,
                },
                color: [1.0, 0.0, 0.0, 1.0],
                radius: 0.0,
            }],
        )
        .with_transform(Transform {
            scale: 2.0,
            tx: 40.0,
            ty: 40.0,
        }),
    );
    let mut r = Renderer::headless(Theme::default());
    let img = r.render_offscreen_scene(&scene, font, 15.0);
    write_png("ghost_layer_transform.png", &img);

    // Red lands inside the transformed rect...
    assert!(
        strong_red(px(&img, 60, 60)),
        "center of the moved/scaled rect"
    );
    assert!(
        strong_red(px(&img, 42, 42)),
        "near its top-left corner (40,40)"
    );
    assert!(
        strong_red(px(&img, 78, 78)),
        "near its bottom-right corner (80,80)"
    );
    // ...and nowhere near the untransformed (0,0,20,20) location, nor past it.
    assert!(
        !strong_red(px(&img, 10, 10)),
        "untransformed origin is now clear"
    );
    assert!(
        !strong_red(px(&img, 90, 90)),
        "nothing spills past the scaled rect"
    );
}

#[test]
fn a_layer_opacity_fades_its_content() {
    // A fully-opaque red fill in a layer with opacity 0 must vanish (the dark
    // background shows through); at full opacity it paints solid red. This is the
    // alpha the chrome fade rides on.
    let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
    let red = |opacity: f32| {
        let mut scene = Scene::new((40, 40));
        scene.layers.push(
            Layer::new(
                0,
                vec![SceneItem::Rect {
                    id: SceneId::Root,
                    rect: RectPx {
                        x: 0.0,
                        y: 0.0,
                        w: 40.0,
                        h: 40.0,
                    },
                    color: [1.0, 0.0, 0.0, 1.0],
                    radius: 0.0,
                }],
            )
            .with_opacity(opacity),
        );
        scene
    };
    let mut r = Renderer::headless(Theme::default());
    let opaque = r.render_offscreen_scene(&red(1.0), font, 15.0);
    assert!(
        strong_red(px(&opaque, 20, 20)),
        "opacity 1 paints solid red"
    );
    let faded = r.render_offscreen_scene(&red(0.0), font, 15.0);
    assert!(
        !strong_red(px(&faded, 20, 20)),
        "opacity 0 fades the fill away to the background"
    );
}
