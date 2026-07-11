//! End-to-end render tests: feed real text (and SGR colors) through a
//! `ghost_term::Vt`, lay it out with `ghost-render`, shape + rasterize with
//! `ghost-shaper`, and draw it on the GPU — asserting on the read-back pixels
//! and dumping PNGs to eyeball. Runs headless on lavapipe.
//!
//! Linux-only: the GPU path needs a software adapter (lavapipe), absent on macOS,
//! so gate the whole target out to keep a bare-macOS `cargo test` green.
#![cfg(target_os = "linux")]

use ghost_render::{
    CellMetrics, Layer, RectPx, Scene, SceneId, SceneItem, Selection, TermDamage, Transform,
    layout_frame, session_key,
};
use ghost_renderer::{Rendered, Renderer, Theme, render_frame};
use ghost_term::Vt;

const FIRA: &[u8] = include_bytes!("../../ghost-shaper/tests/assets/FiraCode-Regular.ttf");

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
fn app_set_dynamic_colors_change_the_rendered_pixels() {
    let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
    // OSC 10/11/12: red default fg, navy default bg, green cursor. An inverse
    // (SGR 7) blank cell paints a quad in the *default foreground*, so the fg
    // override is observable without depending on glyph coverage; the block
    // cursor lands on the next cell; everything else shows the default bg.
    let mut vt = Vt::new(10, 2);
    vt.feed_str("\x1b]10;#ff0000\x07\x1b]11;#000080\x07\x1b]12;#00ff00\x07");
    vt.feed_str("\x1b[7m \x1b[27m");
    let frame = layout_frame(&vt, METRICS);
    let img = render_frame(&frame, font, 15.0, Theme::default());

    // Cell (0,0): inverse blank = app-set foreground.
    assert!(
        strong_red(px(&img, 4, 4)),
        "inverse cell not app-fg: {:?}",
        px(&img, 4, 4)
    );
    // Cell (0,1): the block cursor, filled with the app-set cursor color.
    let cursor = px(&img, 13, 9);
    assert!(
        cursor[1] > 100 && cursor[0] < 60 && cursor[2] < 60,
        "cursor not app-colored: {cursor:?}"
    );
    // A far default cell (row 1): the app-set background, not the theme's.
    assert_eq!(
        px(&img, 80, 27),
        [0x00, 0x00, 0x80, 0xff],
        "default bg not overridden"
    );
}

#[test]
fn app_set_bg_is_exact_on_translucent_themes_across_render_paths() {
    // bg_alpha < 1 regression: the scene path used to alpha-blend the app-set
    // bg (double-premultiplied) over the configured theme's clear, so the
    // window background visibly shifted whenever chrome (a hover underline)
    // joined the scene, and a band-updated row came out darker than the clear.
    let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
    let mut vt = Vt::new(10, 2);
    vt.feed_str("\x1b[?25l\x1b]11;#ffffff\x07hi");
    let frame = layout_frame(&vt, METRICS);
    let theme = Theme {
        bg_alpha: 0.5,
        ..Theme::default()
    };

    // Frame path: premultiplied app bg at the theme's alpha = 255·0.5.
    let by_frame = render_frame(&frame, font, 15.0, theme);
    assert_eq!(px(&by_frame, 80, 27), [128, 128, 128, 128]);

    // Scene path (inline full-window terminal): byte-identical to the frame path.
    let term = SceneItem::Terminal {
        id: SceneId::Root,
        session: session_key("s"),
        rect: RectPx {
            x: 0.0,
            y: 0.0,
            w: 90.0,
            h: 36.0,
        },
        frame: std::rc::Rc::new(frame.clone()),
        selection: None,
        dim: false,
        damage: TermDamage::All,
    };
    let mut scene = Scene::new((90, 36));
    scene.layers.push(Layer::new(0, vec![term]));
    let mut r = Renderer::headless(theme);
    let lone = r.render_offscreen_scene(&scene, font, 15.0);
    assert_eq!(
        lone.rgba, by_frame.rgba,
        "translucent scene path diverged from the frame path"
    );

    // With a chrome Rect riding along (a hyperlink hover underline), the
    // terminal background must not shift toward the theme or gain opacity.
    scene.layers[0].items.push(SceneItem::Rect {
        id: SceneId::Root,
        rect: RectPx {
            x: 0.0,
            y: 16.0,
            w: 18.0,
            h: 1.5,
        },
        color: [1.0, 1.0, 1.0, 0.9],
        radius: 0.0,
    });
    let mut r = Renderer::headless(theme);
    let hovered = r.render_offscreen_scene(&scene, font, 15.0);
    assert_eq!(
        px(&hovered, 80, 27),
        [128, 128, 128, 128],
        "bg shifted when chrome joined the scene"
    );
}

#[test]
fn banded_updates_match_the_full_raster_on_translucent_themes() {
    // A band update erases with a replace quad; its color must come out equal
    // to the pass clear (straight-alpha instance color, premultiplied once by
    // the shader) or every typed-on row turns into a darker stripe when
    // bg_alpha < 1.
    let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
    let theme = Theme {
        bg_alpha: 0.5,
        ..Theme::default()
    };
    let mut vt = Vt::new(10, 2);
    vt.feed_str("\x1b[?25l\x1b]11;#ffffff\x07hi");

    let scene_of = |vt: &Vt, damage: TermDamage| {
        let mut scene = Scene::new((90, 36));
        scene.layers.push(Layer::new(
            0,
            vec![SceneItem::Terminal {
                id: SceneId::Root,
                session: session_key("s"),
                rect: RectPx {
                    x: 0.0,
                    y: 0.0,
                    w: 90.0,
                    h: 36.0,
                },
                frame: std::rc::Rc::new(layout_frame(vt, METRICS)),
                selection: None,
                dim: false,
                damage,
            }],
        ));
        scene
    };

    // Full raster, then a one-row band update after new output on row 0.
    let mut r = Renderer::headless(theme);
    let _ = r.present_offscreen(&scene_of(&vt, TermDamage::All), font, 15.0);
    vt.feed_str("\x1b[1;1Hho");
    let banded = r.present_offscreen(
        &scene_of(&vt, TermDamage::Rows { lo: 0, hi: 0 }),
        font,
        15.0,
    );
    assert_eq!(r.surface_band_updates(), 1, "band path not exercised");

    // A fresh full raster of the same content is the reference.
    let mut fresh = Renderer::headless(theme);
    let full = fresh.present_offscreen(&scene_of(&vt, TermDamage::All), font, 15.0);
    assert_eq!(
        banded.rgba, full.rgba,
        "band-updated pixels diverge from a full raster"
    );
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
fn a_translucent_lone_terminal_composites_see_through() {
    // The test above drives the INLINE path (`render_frame`). The live single-terminal
    // WINDOW instead composites the session's premultiplied Surface over a TRANSPARENT
    // swapchain clear (`present_scene` -> `encode_surface_blit`) — a different code path
    // with its own clear. A half-opaque theme must stay see-through through it too:
    // blank default-background pixels keep ~half alpha (the Surface's own translucent
    // clear, blitted 1:1 over transparent), while an SGR-coloured cell stays opaque.
    // This guards the exact path a user's translucent terminal is drawn by.
    let mut vt = Vt::new(40, 1);
    vt.feed_str("\x1b[?25l\x1b[44mA");
    let frame = layout_frame(&vt, METRICS);
    let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
    let (w, h) = (40 * 9, 18); // 40 cols * 9px advance, 1 row * 18px line height
    let scene = Scene {
        size_px: (w, h),
        layers: vec![Layer::new(
            0,
            vec![SceneItem::Terminal {
                id: SceneId::Root,
                session: session_key("a"),
                rect: RectPx {
                    x: 0.0,
                    y: 0.0,
                    w: w as f32,
                    h: h as f32,
                },
                frame: std::rc::Rc::new(frame),
                selection: None,
                dim: false,
                damage: TermDamage::All,
            }],
        )],
    };
    let theme = Theme {
        bg_alpha: 0.5,
        ..Theme::default()
    };
    let img = Renderer::headless(theme).present_offscreen(&scene, font, 15.0);

    // A blank far-right cell (col ~10, x=94) carries only the composited clear: ~half
    // alpha, proving the Surface's translucency survived the blit onto a transparent
    // swapchain rather than being flattened opaque.
    let blank = px(&img, 94, 9);
    assert!(
        (100..=160).contains(&(blank[3] as u32)),
        "live composite should keep the default background ~half transparent, got alpha {}",
        blank[3]
    );
    // The blue-background cell (col 0) stays fully opaque.
    let colored = px(&img, 4, 9);
    assert_eq!(
        colored[3], 255,
        "an SGR background must stay opaque through the surface path, got alpha {}",
        colored[3]
    );
}

/// The blank default-background strip on row 9 (x past the col-0 glyph/blue cell)
/// used to probe the frost fill. Shared by the frost tests below.
const FROST_STRIP: std::ops::Range<u32> = 20..350;

fn frost_scene(frame: &std::rc::Rc<ghost_render::Frame>, w: u32, h: u32) -> Scene {
    Scene {
        size_px: (w, h),
        layers: vec![Layer::new(
            0,
            vec![SceneItem::Terminal {
                id: SceneId::Root,
                session: session_key("a"),
                rect: RectPx {
                    x: 0.0,
                    y: 0.0,
                    w: w as f32,
                    h: h as f32,
                },
                frame: frame.clone(),
                selection: None,
                dim: false,
                damage: TermDamage::All,
            }],
        )],
    }
}

fn strip_mean(img: &Rendered, chan: usize) -> f32 {
    let (mut sum, mut n) = (0u32, 0u32);
    for x in FROST_STRIP {
        sum += px(img, x, 9)[chan] as u32;
        n += 1;
    }
    sum as f32 / n as f32
}

fn strip_alpha_range(img: &Rendered) -> u32 {
    let (mut lo, mut hi) = (255u32, 0u32);
    for x in FROST_STRIP {
        let a = px(img, x, 9)[3] as u32;
        lo = lo.min(a);
        hi = hi.max(a);
    }
    hi - lo
}

#[test]
fn frost_glazes_the_translucent_background_as_smooth_tinted_glass() {
    // Frost v2 is a smooth flat glass FILL (density) — it raises the see-through
    // default background toward opaque UNIFORMLY, tinted by the theme (dark scheme
    // -> dark glass), with only a subtle grain riding on top. That's the opposite
    // of the old per-pixel static, and it's what lets a compositor-less frost read
    // as glass: it dims/obscures the sharp backdrop instead of just speckling it.
    // Opaque SGR cells stay byte-identical (dest-over, strictly under the frame).
    let mut vt = Vt::new(40, 1);
    vt.feed_str("\x1b[?25l\x1b[44mA"); // hide cursor; "A" on a blue bg in col 0
    let frame = std::rc::Rc::new(layout_frame(&vt, METRICS));
    let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
    let (w, h) = (40 * 9, 18);
    let scene = frost_scene(&frame, w, h);

    let translucent = Theme {
        bg_alpha: 0.5,
        ..Theme::default()
    };
    let frosted = Theme {
        bg_alpha: 0.5,
        frost: 0.55,
        ..Theme::default()
    };
    let plain_img = Renderer::headless(translucent).present_offscreen(&scene, font, 15.0);
    let frost_img = Renderer::headless(frosted).present_offscreen(&scene, font, 15.0);
    write_png("frost.png", &frost_img);

    // Density: the flat glass fill raises the background well toward opaque. At
    // bg_alpha 0.5 + frost 0.55 the effective alpha is ~0.5 + 0.55*0.5 ≈ 0.78.
    let plain_a = strip_mean(&plain_img, 3); // ~128
    let frost_a = strip_mean(&frost_img, 3); // ~198
    assert!(
        frost_a > plain_a + 50.0,
        "frost should fill the background toward opaque (plain {plain_a:.0} -> frost {frost_a:.0})"
    );

    // Smooth, not static: unlike the old grain (range > 40), the fill is nearly
    // flat — a subtle grain rides on it but doesn't dominate.
    let range = strip_alpha_range(&frost_img);
    assert!(
        (2..25).contains(&range),
        "frost v2 is a smooth fill with only a subtle grain, got alpha range {range}"
    );

    // Tint tracks the (dark) theme: the glass is dark, NOT the old milky-white
    // (which would push mean red well above 100 over this dark background).
    let frost_r = strip_mean(&frost_img, 0);
    assert!(
        frost_r < 80.0,
        "a dark theme should give dark glass, got mean red {frost_r:.0}"
    );

    // An opaque SGR cell (col 0, blue bg) is byte-identical with and without frost —
    // frost composites strictly under the frame, so a fully-opaque pixel is untouched.
    assert_eq!(
        px(&plain_img, 4, 9),
        px(&frost_img, 4, 9),
        "frost must not touch an opaque SGR background"
    );

    // The noise is a fixed seed: two renders of the same frosted theme are identical.
    let frost_img2 = Renderer::headless(frosted).present_offscreen(&scene, font, 15.0);
    assert_eq!(
        frost_img.rgba, frost_img2.rgba,
        "frost must be deterministic (fixed noise seed)"
    );

    // Frost is inert behind a fully-opaque background (nothing shows through to
    // frost): an opaque theme renders identically whether frost is set or not.
    let opaque = Theme::default();
    let opaque_frost = Theme {
        frost: 0.55,
        ..Theme::default()
    };
    let opaque_img = Renderer::headless(opaque).present_offscreen(&scene, font, 15.0);
    let opaque_frost_img = Renderer::headless(opaque_frost).present_offscreen(&scene, font, 15.0);
    assert_eq!(
        opaque_img.rgba, opaque_frost_img.rgba,
        "frost behind an opaque background must be a no-op"
    );
}

#[test]
fn frost_tint_follows_the_theme_background() {
    // v2's default tint is the theme background nudged toward white, so a dark
    // scheme gets dark glass and a light scheme gets light glass — the opposite of
    // the old fixed milk-white, which lightened every scheme the same way.
    let mut vt = Vt::new(40, 1);
    vt.feed_str("\x1b[?25l "); // hide cursor; a blank cell (default bg)
    let frame = std::rc::Rc::new(layout_frame(&vt, METRICS));
    let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
    let (w, h) = (40 * 9, 18);
    let scene = frost_scene(&frame, w, h);

    // Default (dark) bg vs a light bg; frost_tint left None so the renderer derives
    // each from its own theme background.
    let dark = Theme {
        bg_alpha: 0.5,
        frost: 0.6,
        ..Theme::default()
    };
    let light = Theme {
        bg: [0xf0, 0xf0, 0xf2],
        bg_alpha: 0.5,
        frost: 0.6,
        ..Theme::default()
    };
    let dark_img = Renderer::headless(dark).present_offscreen(&scene, font, 15.0);
    let light_img = Renderer::headless(light).present_offscreen(&scene, font, 15.0);

    let dark_r = strip_mean(&dark_img, 0);
    let light_r = strip_mean(&light_img, 0);
    assert!(
        dark_r < 80.0,
        "dark scheme -> dark glass, got mean red {dark_r:.0}"
    );
    assert!(
        light_r > 180.0,
        "light scheme -> light glass, got mean red {light_r:.0}"
    );
}

#[test]
fn frost_survives_the_resize_snapshot_blit() {
    // During an interactive resize the window stretch-blits a captured snapshot
    // instead of re-rendering (see a_resize_blit_scales_the_snapshot_without_reshaping).
    // The frost is applied fresh at surface resolution after that blit, so the
    // grain stays visible (and crisp, not stretched) through a drag rather than
    // popping out until the resize commits.
    let mut vt = Vt::new(40, 1);
    vt.feed_str("\x1b[?25l\x1b[44mA"); // hide cursor; "A" on a blue bg in col 0
    let frame = std::rc::Rc::new(layout_frame(&vt, METRICS));
    let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
    let (w, h) = (40 * 9, 18);
    let scene = Scene {
        size_px: (w, h),
        layers: vec![Layer::new(
            0,
            vec![SceneItem::Terminal {
                id: SceneId::Root,
                session: session_key("a"),
                rect: RectPx {
                    x: 0.0,
                    y: 0.0,
                    w: w as f32,
                    h: h as f32,
                },
                frame: frame.clone(),
                selection: None,
                dim: false,
                damage: TermDamage::All,
            }],
        )],
    };
    let plain = Theme {
        bg_alpha: 0.5,
        ..Theme::default()
    };
    let frosted = Theme {
        bg_alpha: 0.5,
        frost: 0.55,
        ..Theme::default()
    };

    let mut pr = Renderer::headless(plain);
    pr.capture_snapshot(&scene, font, 15.0);
    let plain_img = pr.blit_snapshot_offscreen(w, h);

    let mut r = Renderer::headless(frosted);
    r.capture_snapshot(&scene, font, 15.0);
    // Blit the snapshot 1:1 (the resize path also stretches, but the frost is
    // applied at the blit target's resolution either way).
    let img = r.blit_snapshot_offscreen(w, h);
    // The glass fill reaches the snapshot-blitted background too: it's raised well
    // toward opaque, not left at the plain translucent alpha.
    let (plain_a, frost_a) = (strip_mean(&plain_img, 3), strip_mean(&img, 3));
    assert!(
        frost_a > plain_a + 50.0,
        "frost should fill the snapshot-blitted background too (plain {plain_a:.0} -> frost {frost_a:.0})"
    );
}

#[test]
fn set_theme_invalidates_cached_surfaces_and_recomposites() {
    // A live theme change (config reload) must drop cached surfaces: they baked
    // the old bg_alpha/palette under an (session, size) key a theme swap doesn't
    // change, so a stale cache hit would blit the OLD theme. Re-raster at the new
    // theme instead, and let the new opacity reach the composited pixels.
    let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
    let frame = {
        let mut vt = Vt::new(80, 24); // 720x432 px at METRICS
        vt.feed_str("surface content => fn main() { let answer = 42; }");
        layout_frame(&vt, METRICS)
    };
    // Two downscaled tiles (0.5x) — the size that goes through the surface cache.
    let tile = |id, x: f32| SceneItem::Terminal {
        id,
        session: session_key(&format!("{id:?}")),
        rect: RectPx {
            x,
            y: 0.0,
            w: 360.0,
            h: 216.0,
        },
        frame: std::rc::Rc::new(frame.clone()),
        selection: None,
        dim: false,
        damage: TermDamage::All,
    };
    let mut scene = Scene::new((800, 240));
    scene.layers.push(Layer::new(
        0,
        vec![tile(SceneId::Tile(0), 0.0), tile(SceneId::Tile(1), 380.0)],
    ));

    let mut r = Renderer::headless(Theme::default()); // opaque
    let first = r.render_offscreen_scene(&scene, font, 15.0);
    assert_eq!(
        r.surface_renders(),
        2,
        "the first paint renders both surface textures"
    );
    // A blank scene pixel (right of both tiles) carries the opaque theme clear.
    assert_eq!(
        px(&first, 770, 120)[3],
        255,
        "precondition: the opaque theme clears fully opaque"
    );

    // Reload to a translucent theme.
    r.set_theme(Theme {
        bg_alpha: 0.5,
        ..Theme::default()
    });
    let second = r.render_offscreen_scene(&scene, font, 15.0);
    assert_eq!(
        r.surface_renders(),
        4,
        "set_theme must drop the cache so both surfaces re-raster at the new theme"
    );
    // The blank clear is now translucent — the reloaded opacity reached the pixels.
    let a = px(&second, 770, 120)[3] as u32;
    assert!(
        (100..=160).contains(&a),
        "the reloaded opacity should composite see-through, got alpha {a}"
    );
}

#[test]
fn an_uncovered_glyph_falls_back_to_a_face_that_has_it() {
    // Fira Code has no ★ (U+2605): shaping it yields .notdef, drawn as the tofu box.
    // With a fallback resolver that points ★ at DejaVu Sans Mono (which covers it), the
    // renderer must draw DejaVu's real star instead — byte-identical to rendering the
    // same line with DejaVu as the primary font, and DIFFERENT from Fira's notdef box.
    const DEJAVU: &[u8] = include_bytes!("../../ghost-shaper/tests/assets/DejaVuSansMono.ttf");
    let fira = ghost_shaper::font_from_bytes(FIRA).expect("fira");
    let dejavu = ghost_shaper::font_from_bytes(DEJAVU).expect("dejavu");
    assert_eq!(
        ghost_shaper::glyph_id(fira, '★'),
        0,
        "precondition: Fira must lack ★"
    );
    assert_ne!(
        ghost_shaper::glyph_id(dejavu, '★'),
        0,
        "precondition: DejaVu must cover ★"
    );

    // A resolver that sends ★ (and only ★) to DejaVu.
    struct StarToDejaVu;
    impl ghost_shaper::Fallback for StarToDejaVu {
        fn face_for(&mut self, ch: char) -> Option<ghost_shaper::FontRef<'static>> {
            (ch == '★').then(|| ghost_shaper::font_from_bytes(DEJAVU).expect("dejavu"))
        }
    }

    let mut vt = Vt::new(3, 1);
    vt.feed_str("\x1b[?25l★"); // a lone star; hide the cursor so it can't tint the cell
    let frame = layout_frame(&vt, METRICS);

    // Fira alone: the star is .notdef (the tofu box). DejaVu as primary: the reference.
    let notdef = Renderer::headless(Theme::default()).render_offscreen(&frame, fira, 15.0);
    let reference = Renderer::headless(Theme::default()).render_offscreen(&frame, dejavu, 15.0);
    // Fira primary + the fallback: must reproduce DejaVu's star exactly.
    let mut r = Renderer::headless(Theme::default());
    r.set_fallback(Box::new(StarToDejaVu));
    let fallback = r.render_offscreen(&frame, fira, 15.0);

    assert_eq!(
        fallback.rgba, reference.rgba,
        "the fallback must draw DejaVu's ★ exactly, as if DejaVu were the primary font"
    );
    assert_ne!(
        fallback.rgba, notdef.rgba,
        "the fallback ★ must replace Fira's .notdef box, not sit alongside it"
    );
}

#[test]
fn a_color_emoji_renders_in_color_not_tinted() {
    // Fira Code has no 🤪; the fallback resolves it to the bundled Noto Color
    // Emoji COLRv1 subset. The glyph must render with the emoji's own palette
    // (many hues, notably the yellow face) — not as a coverage mask tinted
    // with the cell's near-gray foreground, which is what a mask-only glyph
    // path produces.
    const NOTO: &[u8] =
        include_bytes!("../../ghost-shaper/tests/assets/NotoColorEmoji-COLRv1-subset.ttf");
    let fira = ghost_shaper::font_from_bytes(FIRA).expect("fira");
    assert_eq!(
        ghost_shaper::glyph_id(fira, '\u{1F92A}'),
        0,
        "precondition: Fira must lack the emoji"
    );

    struct EmojiFallback;
    impl ghost_shaper::Fallback for EmojiFallback {
        fn face_for(&mut self, ch: char) -> Option<ghost_shaper::FontRef<'static>> {
            (ch == '\u{1F92A}').then(|| ghost_shaper::font_from_bytes(NOTO).expect("noto subset"))
        }
    }

    let mut vt = Vt::new(4, 1);
    vt.feed_str("\x1b[?25l🤪");
    let frame = layout_frame(&vt, METRICS);
    let mut r = Renderer::headless(Theme::default());
    r.set_fallback(Box::new(EmojiFallback));
    let img = r.render_offscreen(&frame, fira, 15.0);

    // Scan the emoji's two-cell box. A tinted mask stays achromatic (fg and bg
    // are both near-gray), so saturation is the discriminator, not pixel count.
    let (box_w, box_h) = (2.0 * METRICS.advance, METRICS.line_height);
    let mut saturated = 0u32;
    let mut yellow = false;
    for y in 0..box_h as u32 {
        for x in 0..box_w as u32 {
            let [red, g, b, _] = px(&img, x, y);
            let spread = red.max(g).max(b) - red.min(g).min(b);
            if spread > 40 {
                saturated += 1;
            }
            if red > 180 && g > 120 && b < 100 {
                yellow = true;
            }
        }
    }
    assert!(
        saturated > 20,
        "a color emoji must paint saturated pixels, found {saturated}"
    );
    assert!(yellow, "the zany face's yellow head must be present");
}

#[test]
fn scales_a_large_surface_frame_to_fit_its_tile() {
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
            session: 0,
            rect: RectPx {
                x: 0.0,
                y: 0.0,
                w: 45.0,
                h: 36.0,
            },
            frame: std::rc::Rc::new(frame),
            selection: None,
            dim: false,
            damage: TermDamage::All,
        }],
    ));
    let mut renderer = Renderer::headless(Theme::default());
    let img = renderer.render_offscreen_scene(&scene, font, 15.0);
    let path = write_png("ghost_scaled_surface_sample.png", &img);
    eprintln!("WROTE PNG: {}", path.display());

    // The blue marker (frame cell at 81,54) maps to ~(40,27) at 0.5x: inside the
    // tile's lower-right quadrant.
    let (inside, _) = rect(&img, 36, 45, 27, 36, strong_blue);
    assert!(
        inside > 0,
        "scaled surface should bring the bottom-right cell inside the tile"
    );
    // Nothing draws outside the tile rect (x>=45 or y>=36 stays clear).
    let (outside, _) = rect(&img, 45, 90, 0, 72, strong_blue);
    assert_eq!(outside, 0, "surface must stay within its tile ({outside})");
}

#[test]
fn an_unchanged_surface_is_not_re_rasterized() {
    // Two scaled surfaces (a real-size frame shrunk into a small tile). Rendering
    // the scene twice must re-render zero surface textures the second time — the
    // cache turns fleet nav and idle tiles into cheap blits, the whole point of RTT.
    let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
    let frame = {
        let mut vt = Vt::new(80, 24); // 720x432 px at METRICS
        vt.feed_str("surface content => fn main() { let answer = 42; }");
        layout_frame(&vt, METRICS)
    };
    let tile = |id, x: f32| SceneItem::Terminal {
        id,
        session: session_key(&format!("{id:?}")),
        rect: RectPx {
            x,
            y: 0.0,
            w: 360.0,
            h: 216.0,
        }, // 0.5x: contain_scale < 1
        frame: std::rc::Rc::new(frame.clone()),
        selection: None,
        dim: false,
        // Downscaled tile; an unchanged repaint hits the Rc-identity cache first,
        // so this stamp only matters if the cached frame actually changes.
        damage: TermDamage::All,
    };
    let mut scene = Scene::new((800, 240));
    scene.layers.push(Layer::new(
        0,
        vec![tile(SceneId::Tile(0), 0.0), tile(SceneId::Tile(1), 380.0)],
    ));

    let mut r = Renderer::headless(Theme::default());
    let _ = r.render_offscreen_scene(&scene, font, 15.0);
    assert_eq!(
        r.surface_renders(),
        2,
        "the first paint renders both surface textures"
    );
    let _ = r.render_offscreen_scene(&scene, font, 15.0);
    assert_eq!(
        r.surface_renders(),
        2,
        "an unchanged repaint blits the cache, re-rasterizing nothing"
    );
}

#[test]
fn an_animated_dive_camera_does_not_re_rasterize_surfaces_each_frame() {
    // The single<->fleet dive zooms by animating each layer's camera transform every
    // frame (the world is frozen; only the camera moves). A tile's surface TEXTURE
    // must not depend on the live camera scale — otherwise a continuously-zooming
    // dive re-renders every tile's texture on every frame, the O(sessions x frames)
    // cost that makes the dive sluggish with more than one live surface. Each tile
    // must (re-)rasterize at most once for the whole dive, not once per frame.
    let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
    let frame = {
        let mut vt = Vt::new(80, 24); // 720x432 px native at METRICS
        vt.feed_str("surface content => fn main() { let answer = 42; }");
        layout_frame(&vt, METRICS)
    };
    // Two tiles at a fixed WORLD rect a quarter of native (contain_scale < 1, so the
    // RTT surface path is taken); the camera — not the rect — is what animates.
    let tile = |id, x: f32| SceneItem::Terminal {
        id,
        session: session_key(&format!("{id:?}")),
        rect: RectPx {
            x,
            y: 0.0,
            w: 180.0,
            h: 108.0,
        }, // 0.25x native
        frame: std::rc::Rc::new(frame.clone()),
        selection: None,
        dim: false,
        // The frozen dive world flows through as the same Rc every frame, so the
        // Rc-identity cache — not this stamp — is what avoids the per-frame re-raster.
        damage: TermDamage::All,
    };
    let mut scene = Scene::new((1920, 1080));
    scene.layers.push(Layer::new(
        0,
        vec![tile(SceneId::Tile(0), 0.0), tile(SceneId::Tile(1), 200.0)],
    ));

    let mut r = Renderer::headless(Theme::default());
    let tiles = 2;
    // Drive a zoom: a distinct camera scale every frame, each keeping the on-screen
    // size below the native cap, so size-keyed caching would miss on every frame.
    let frames = 24u32;
    for i in 0..frames {
        let s = 1.0 + (i as f32) * (2.8 / frames as f32); // 1.0 .. ~3.8 (eff < 720 px)
        scene.layers[0].transform = Transform {
            scale: s,
            tx: 0.0,
            ty: 0.0,
        };
        let _ = r.render_offscreen_scene(&scene, font, 15.0);
    }
    assert_eq!(
        r.surface_renders(),
        tiles,
        "each tile's surface must render once for the whole dive, not once per \
         frame: got {} re-renders for {tiles} tiles over {frames} animated frames",
        r.surface_renders(),
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
            session: 0,
            rect: RectPx {
                x: 0.0,
                y: 0.0,
                w: 180.0,
                h: 72.0,
            },
            frame: std::rc::Rc::new(frame),
            selection: None,
            dim: false,
            damage: TermDamage::All,
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
        r.surface_renders(),
        0,
        "a snapshot blit renders no surfaces"
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
    // text re-shape nothing, so fleet navigation and idle surfaces stay cheap.
    let mut vt = Vt::new(40, 6);
    vt.feed_str("fn main() != ok { let x = 1; }\r\nsecond line of text => here");
    let frame = layout_frame(&vt, METRICS);
    let font = ghost_shaper::font_from_bytes(FIRA).expect("font");

    let mut scene = Scene::new((360, 108)); // 40*9 x 6*18
    scene.layers.push(Layer::new(
        0,
        vec![SceneItem::Terminal {
            id: SceneId::Tile(0),
            session: 0,
            rect: RectPx {
                x: 0.0,
                y: 0.0,
                w: 360.0,
                h: 108.0,
            },
            frame: std::rc::Rc::new(frame),
            selection: None,
            dim: false,
            damage: TermDamage::All,
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
fn a_fresh_font_of_the_same_face_reuses_the_shape_cache() {
    // The app rebuilds its `FontRef` from the same embedded bytes every frame, and
    // swash mints a fresh `CacheKey` (a global atomic) on every construction. The
    // shape cache must key on the font's stable *data* identity, not that ephemeral
    // key — otherwise every frame re-shapes every run (the heavy `ls` re-raster),
    // silently defeating the cache no matter how effective it looks in a test that
    // happens to reuse one `FontRef`.
    let text = "fn main() != ok { let x = 1; } => colorized ls here";
    let build = || {
        let mut vt = Vt::new(52, 4);
        vt.feed_str(text);
        std::rc::Rc::new(layout_frame(&vt, METRICS))
    };
    let scene = |frame: std::rc::Rc<ghost_render::Frame>| {
        let mut s = Scene::new((52 * 9, 4 * 18));
        s.layers.push(Layer::new(
            0,
            vec![SceneItem::Terminal {
                id: SceneId::Tile(0),
                session: 0,
                rect: RectPx {
                    x: 0.0,
                    y: 0.0,
                    w: 52.0 * 9.0,
                    h: 4.0 * 18.0,
                },
                frame,
                selection: None,
                dim: false,
                damage: TermDamage::All,
            }],
        ));
        s
    };
    let mut r = Renderer::headless(Theme::default());

    // Frame 1: a freshly-built font shapes the runs.
    let font1 = ghost_shaper::font_from_bytes(FIRA).expect("font");
    let _ = r.render_offscreen_scene(&scene(build()), font1, 15.0);
    let after_first = r.shape_misses();
    assert!(after_first > 0, "the first paint shapes its runs");

    // Frame 2: a DIFFERENT `FontRef` of the SAME face (distinct swash key) over a
    // fresh frame `Rc` must re-shape nothing — the cache keys on the font's data.
    let font2 = ghost_shaper::font_from_bytes(FIRA).expect("font");
    assert_ne!(
        font1.key.value(),
        font2.key.value(),
        "swash mints a fresh key per construction (else the test proves nothing)"
    );
    let _ = r.render_offscreen_scene(&scene(build()), font2, 15.0);
    assert_eq!(
        r.shape_misses(),
        after_first,
        "a fresh FontRef of the same face must reuse the shape cache, re-shaping nothing"
    );
}

#[test]
fn re_rasterizing_seen_text_allocates_no_probe_keys() {
    // Every dive-out rebuilds a tile's frame as a FRESH `Rc`, so the surface cache
    // misses (not ptr-equal) and `build_instances` re-runs in full — the heavy
    // re-raster the animation-warm path pays. Its run texts are already shaped, so
    // that re-raster must not only re-shape nothing but also allocate no owned probe
    // keys: the shape-cache lookup probes by borrowed `&str`. Allocating a `String`
    // per run just to look it up was ~half the heavy raster's CPU.
    let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
    let mut r = Renderer::headless(Theme::default());

    // Enough distinct runs that the first raster does real shaping and keys them.
    let text = "let mut xs = vec![1, 2, 3]; // ls -la /lib64 => many entries here";
    let build = || {
        let mut vt = Vt::new(64, 4);
        vt.feed_str(text);
        std::rc::Rc::new(layout_frame(&vt, METRICS))
    };
    let scene = |frame: std::rc::Rc<ghost_render::Frame>| {
        let mut s = Scene::new((64 * 9, 4 * 18));
        s.layers.push(Layer::new(
            0,
            vec![SceneItem::Terminal {
                id: SceneId::Tile(0),
                session: 0,
                rect: RectPx {
                    x: 0.0,
                    y: 0.0,
                    w: 64.0 * 9.0,
                    h: 4.0 * 18.0,
                },
                frame,
                selection: None,
                dim: false,
                damage: TermDamage::All,
            }],
        ));
        s
    };

    // First raster: shapes the runs and allocates one cache key per distinct run.
    let _ = r.render_offscreen_scene(&scene(build()), font, 15.0);
    let misses = r.shape_misses();
    let allocs = r.shape_key_allocs();
    assert!(
        misses > 0 && allocs > 0,
        "the first raster shapes its runs and keys them (misses={misses}, allocs={allocs})"
    );

    // A distinct `Rc` with identical text forces a surface-cache miss (not ptr-equal)
    // and re-runs `build_instances`, but every run text is already in the shape cache.
    let _ = r.render_offscreen_scene(&scene(build()), font, 15.0);
    assert_eq!(
        r.shape_misses(),
        misses,
        "re-rasterizing seen text re-shapes nothing"
    );
    assert_eq!(
        r.shape_key_allocs(),
        allocs,
        "a shape-cache hit must allocate no owned probe key"
    );
}

#[test]
fn parallel_and_inline_cold_shaping_agree() {
    // Parallel pre-shaping is a pure optimization: a cold frame must shape each
    // distinct run exactly once and render byte-identically whether its runs are
    // shaped in parallel (default) or inline. Dense colorized content (a fresh 2-char
    // colored run per cell) crosses the fan-out threshold.
    let mut s = String::new();
    for row in 0..40usize {
        for col in 0..40usize {
            let a = char::from(b'!' + ((row * 7 + col * 3) % 90) as u8);
            let b = char::from(b'!' + ((row * 11 + col * 5) % 90) as u8);
            s.push_str(&format!("\x1b[38;5;{}m{a}{b}", 16 + ((row + col) % 200)));
        }
        s.push_str("\r\n");
    }
    let mut vt = Vt::new(120, 40);
    vt.feed_str(&s);
    let frame = layout_frame(&vt, METRICS);

    let mut distinct = std::collections::HashSet::new();
    for row in &frame.rows_layout {
        for run in &row.runs {
            distinct.insert(run.text.clone());
        }
    }
    let unique = distinct.len() as u32;
    assert!(
        unique >= 48,
        "need enough runs to hit the parallel path, got {unique}"
    );

    let render = |parallel: bool| {
        let mut r = Renderer::headless(Theme::default());
        r.set_parallel_shaping(parallel);
        let img = r.render_offscreen(&frame, ghost_shaper::font_from_bytes(FIRA).unwrap(), 15.0);
        (img.rgba, r.shape_misses())
    };
    let (par_px, par_miss) = render(true);
    let (ser_px, ser_miss) = render(false);

    assert_eq!(
        par_miss, unique,
        "parallel: each distinct run shaped exactly once"
    );
    assert_eq!(
        ser_miss, unique,
        "inline: each distinct run shaped exactly once"
    );
    assert!(
        par_px == ser_px,
        "parallel and inline shaping must render byte-identically"
    );
}

#[test]
fn warm_repaints_stay_fully_cached() {
    // The general cache-regression guard, expressed on cache_stats(): once content is
    // warm, repainting it — even with a fresh FontRef every frame, exactly as the app
    // does — must be served entirely from the shape and glyph caches. A change that
    // quietly breaks a cache key (as the swash-font-key bug did) surfaces here as
    // misses > 0 while every pixel is still correct, which no golden test would catch.
    let mut r = Renderer::headless(Theme::default());
    let mut vt = Vt::new(60, 5);
    vt.feed_str("fn main() { let xs = vec![1, 2, 3]; } // ls -la /lib64 => entries");
    let frame = layout_frame(&vt, METRICS);

    // Warm the shape + glyph caches.
    let _ = r.render_offscreen(&frame, ghost_shaper::font_from_bytes(FIRA).unwrap(), 15.0);

    let before = r.cache_stats();
    for _ in 0..5 {
        // A fresh FontRef of the same face each frame — the shape the app produces.
        let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
        let _ = r.render_offscreen(&frame, font, 15.0);
    }
    let shape = r.cache_stats().shape.since(before.shape);
    let glyph = r.cache_stats().glyph.since(before.glyph);
    assert!(
        shape.hits > 0 && glyph.hits > 0,
        "the repaints did probe the caches (shape {shape:?}, glyph {glyph:?})"
    );
    assert_eq!(
        (shape.misses, glyph.misses),
        (0, 0),
        "warm repaints must re-shape and re-rasterize nothing — shape hit-rate {:.3}, glyph hit-rate {:.3}",
        shape.hit_rate(),
        glyph.hit_rate(),
    );
    assert_eq!((shape.hit_rate(), glyph.hit_rate()), (1.0, 1.0));
}

#[test]
fn an_idle_tiles_reblit_is_a_surface_cache_hit() {
    // The per-session surface cache: re-presenting an unchanged tile (same frame `Rc`)
    // must blit its cached texture, not re-raster it, and must not evict it. This is the
    // guard against the dive-out eviction regression, where a single-keyed surface cache
    // dropped a session's native tile and re-rastered it on every dive.
    let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
    let mut r = Renderer::headless(Theme::default());
    let frame = {
        let mut vt = Vt::new(40, 6);
        vt.feed_str("idle tile content => stays cached");
        std::rc::Rc::new(layout_frame(&vt, METRICS))
    };
    let mut scene = Scene::new((400, 240));
    scene.layers.push(Layer::new(
        0,
        vec![SceneItem::Terminal {
            id: SceneId::Tile(0),
            session: 7,
            rect: RectPx {
                x: 0.0,
                y: 0.0,
                w: 200.0,
                h: 120.0,
            }, // 0.5x → native-res surface
            frame: frame.clone(),
            selection: None,
            dim: false,
            damage: TermDamage::All,
        }],
    ));

    // First present rasterizes the surface (a miss).
    let _ = r.render_offscreen_scene(&scene, font, 15.0);
    assert_eq!(r.cache_stats().surface.misses, 1);

    let before = r.cache_stats().surface;
    // Re-present the identical scene (same `Rc`): a pure surface hit.
    let _ = r.render_offscreen_scene(&scene, font, 15.0);
    let s = r.cache_stats().surface.since(before);
    assert_eq!(s.misses, 0, "an idle tile must not re-raster its surface");
    assert_eq!(
        s.evictions, 0,
        "an on-screen tile's surface must not be evicted"
    );
    assert!(
        s.hits >= 1,
        "the idle tile blits its cached surface (a hit)"
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

// A screen packed with distinct short colored tokens, mirroring `ls --color`
// (unique filenames in columns): every ~9-cell token is its own SGR-colored run,
// all distinct, so a cold build must shape every one of them. Sizing it to a HiDPI
// 4K grid (213x60 logical cells) reproduces a real full-screen `ls /lib64` at 4K:
// ~1359 distinct cold runs — the heavy first paint the shape cache + parallel
// pre-shaping exist to absorb (a warm re-raster then serves it all from cache).
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

#[test]
fn cold_full_screen_build_stays_within_budget() {
    // Regression guard on the one-time cost this stack targets: cold-building a full
    // 4K screen of dense distinct colored runs (a `ls /lib64` first paint). It times
    // `build_frame_cpu` — layout+shape, no render pass and no readback (the readback
    // render_offscreen adds is a pixel-test need that would only pile ~33MB of copy
    // noise onto this). The build's one GPU touch is cold-glyph atlas uploads, tiny
    // here: the fixture's `fNNNNNN` tokens use ~11 distinct glyphs, so the timed cost
    // is the CPU shaping of the distinct runs. Fresh Renderer per sample = cold cache.
    //
    // The budget is enforced only in release: unoptimized swash shaping runs ~18x
    // slower, so a debug `cargo test` gets a loose catastrophe bound instead of the
    // real number. Measured baselines (4 cores): ~5.0ms release / ~91ms debug; the
    // ceilings are ~1.6x that. The 8ms release ceiling also sits below the inline
    // baseline (~18ms), so this fails too if parallel pre-shaping silently regresses.
    let vt = dense_ls_screen(213, 60); // HiDPI 4K
    let frame = layout_frame(&vt, METRICS);

    let mut distinct = std::collections::HashSet::new();
    for row in &frame.rows_layout {
        for run in &row.runs {
            distinct.insert(run.text.clone());
        }
    }
    let distinct = distinct.len();
    assert!(
        distinct >= 1000,
        "fixture must stay dense to be a meaningful guard, got {distinct} distinct runs"
    );

    // A cold build shapes each distinct run exactly once — so the time below is real
    // shaping work, not a no-op over an already-warm cache.
    let mut probe = Renderer::headless(Theme::default());
    probe.build_frame_cpu(&frame, ghost_shaper::font_from_bytes(FIRA).unwrap(), 15.0);
    assert_eq!(
        probe.shape_misses() as usize,
        distinct,
        "cold build must shape every distinct run once"
    );

    let mut samples: Vec<std::time::Duration> = Vec::new();
    for _ in 0..5 {
        let mut r = Renderer::headless(Theme::default());
        let t = std::time::Instant::now();
        let n = r.build_frame_cpu(&frame, ghost_shaper::font_from_bytes(FIRA).unwrap(), 15.0);
        let e = t.elapsed();
        std::hint::black_box(n);
        samples.push(e);
    }
    samples.sort();
    let median = samples[samples.len() / 2];

    let budget = if cfg!(debug_assertions) {
        std::time::Duration::from_millis(160)
    } else {
        std::time::Duration::from_millis(8)
    };
    assert!(
        median < budget,
        "cold build of {distinct} distinct runs took {median:?} (median of 5), over the \
         {budget:?} budget — cold shaping regressed (parallel pre-shaping off, or per-run \
         shaping got slower)"
    );
}

#[test]
fn no_readback_render_drives_the_full_path() {
    // The no-readback render is the primitive behind the GPU benchmark and any test
    // that drives the real render path to assert on something other than pixels. Here
    // it stands in for that pattern: it runs the whole build → upload → render pass →
    // submit → GPU-wait path (panicking on any GPU/validation error) and reports the
    // target size, which must match both frame_size and the readback path's dims —
    // proving the two share a target and differ only in the pixel copy.
    let vt = dense_ls_screen(80, 24);
    let frame = layout_frame(&vt, METRICS);
    let font = ghost_shaper::font_from_bytes(FIRA).unwrap();

    let mut r = Renderer::headless(Theme::default());
    let (w, h) = r.render_offscreen_no_readback(&frame, font, 15.0);
    assert_eq!(
        (w, h),
        Renderer::frame_size(&frame),
        "no-readback render targets the frame's pixel size"
    );

    let img = r.render_offscreen(&frame, font, 15.0);
    assert_eq!(
        (w, h),
        (img.width, img.height),
        "no-readback and readback render the same target; only the pixel copy differs"
    );
}

#[test]
fn a_real_bold_face_is_used_instead_of_synthesizing_weight() {
    // A bold run rendered through a FontSet with a real bold slot must go to that
    // face, not the emboldened Regular. We can't ship a second face in the test, so
    // the bold slot here is the SAME outline as regular — which means the real-face
    // path emboldens NOTHING, while the single-face path synthesizes the weight. The
    // emboldened render therefore covers strictly more ink, proving the slot is what
    // decided whether synthesis ran.
    let mut vt = Vt::new(20, 1);
    vt.feed_str("\x1b[1mMMMMMMMM"); // a bold run
    let frame = layout_frame(&vt, METRICS);

    let reg = ghost_shaper::font_from_bytes(FIRA).unwrap();
    let with_real_bold = ghost_shaper::FontSet {
        regular: reg,
        bold: Some(ghost_shaper::font_from_bytes(FIRA).unwrap()),
        italic: None,
        bold_italic: None,
    };

    // Single face → the bold run's weight is synthesized (emboldened).
    let synth = Renderer::headless(Theme::default()).render_offscreen(&frame, reg, 15.0);
    // Real bold slot (same outline) → no synthesis, so the strokes stay unthickened.
    let real = Renderer::headless(Theme::default()).render_offscreen(&frame, with_real_bold, 15.0);

    let ink = |img: &Rendered| {
        let bg = px(img, 0, 0);
        (0..img.height * img.width)
            .filter(|i| {
                let (x, y) = (i % img.width, i / img.width);
                px(img, x, y) != bg
            })
            .count()
    };
    assert!(
        ink(&synth) > ink(&real),
        "synthesized bold must add ink over the real (unweighted) face: {} vs {}",
        ink(&synth),
        ink(&real)
    );
}
