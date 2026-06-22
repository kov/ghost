//! End-to-end render tests: feed real text (and SGR colors) through a
//! `ghost_term::Vt`, lay it out with `ghost-render`, shape + rasterize with
//! `ghost-shaper`, and draw it on the GPU — asserting on the read-back pixels
//! and dumping PNGs to eyeball. Runs headless on lavapipe.

use ghost_render::{CellMetrics, layout_frame};
use ghost_renderer::{Rendered, Theme, render_frame};
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

#[test]
fn renders_ligature_line_to_image() {
    let mut vt = Vt::new(40, 3);
    // != && => -> are all Fira Code ligatures.
    vt.feed_str("fn ok() { a != b && c => d } // -> go");

    let frame = layout_frame(&vt, METRICS);
    let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
    let img = render_frame(&frame, font, 15.0, Theme::default());

    assert_eq!(img.width, 360, "40 cols * 9px advance");
    assert_eq!(img.height, 54, "3 rows * 18px line height");
    assert_eq!(img.rgba.len() as u32, img.width * img.height * 4);

    // Something legible was drawn: many pixels differ from the dark theme bg.
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
    // "AB" in ANSI red fg, then a blank, then "CD" on an ANSI blue background.
    let mut vt = Vt::new(40, 1);
    vt.feed_str("\x1b[31mAB\x1b[0m \x1b[44mCD\x1b[0m");

    let frame = layout_frame(&vt, METRICS);
    let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
    let img = render_frame(&frame, font, 15.0, Theme::default());
    let path = write_png("ghost_color_sample.png", &img);
    eprintln!("WROTE PNG: {}", path.display());

    // Red foreground glyphs (cols 0..1 -> x in 0..18). ANSI red is ~[128,0,0];
    // glyph strokes should show a clearly red pixel.
    let red = (0..18)
        .flat_map(|x| (0..18).map(move |y| (x, y)))
        .map(|(x, y)| px(&img, x, y))
        .any(|p| p[0] > 90 && p[1] < 50 && p[2] < 50);
    assert!(red, "expected red foreground pixels in cols 0..1");

    // Blue background cells (cols 3..4 -> x in 27..45). ANSI blue bg is
    // ~[0,0,128]; the filled background rect should show clearly blue pixels.
    let blue = (27..45)
        .flat_map(|x| (0..18).map(move |y| (x, y)))
        .map(|(x, y)| px(&img, x, y))
        .any(|p| p[2] > 100 && p[0] < 60 && p[1] < 60);
    assert!(blue, "expected blue background pixels in cols 3..4");
}
