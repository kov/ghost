//! End-to-end render test: feed real text through a `ghost_term::Vt`, lay it
//! out with `ghost-render`, shape + rasterize with `ghost-shaper`, and draw it
//! on the GPU — then assert the image isn't blank and dump a PNG to eyeball the
//! ligatures. Runs headless on lavapipe.

use ghost_render::{CellMetrics, layout_frame};
use ghost_renderer::{Rendered, render_frame};
use ghost_term::Vt;

const FIRA: &[u8] = include_bytes!("../../shaper/tests/assets/FiraCode-Regular.ttf");

fn write_png(path: &std::path::Path, img: &Rendered) {
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), img.width, img.height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().expect("png header");
    writer.write_image_data(&img.rgba).expect("png data");
}

#[test]
fn renders_ligature_line_to_image() {
    let mut vt = Vt::new(40, 3);
    // != && => -> are all Fira Code ligatures.
    vt.feed_str("fn ok() { a != b && c => d } // -> go");

    let frame = layout_frame(
        &vt,
        CellMetrics {
            advance: 9.0,
            line_height: 18.0,
        },
    );
    let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
    let img = render_frame(&frame, font, 15.0, [0.06, 0.07, 0.09, 1.0]);

    assert_eq!(img.width, 360, "40 cols * 9px advance");
    assert_eq!(img.height, 54, "3 rows * 18px line height");
    assert_eq!(img.rgba.len() as u32, img.width * img.height * 4);

    // Something legible was drawn: many pixels differ from the dark background.
    let bg = [15i32, 18, 23];
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

    let sum: u64 = img.rgba.iter().map(|&b| u64::from(b)).sum();
    let path = std::env::temp_dir().join("ghost_ligature_sample.png");
    write_png(&path, &img);
    eprintln!("WROTE PNG: {} (lit={lit}, sum={sum})", path.display());
}
