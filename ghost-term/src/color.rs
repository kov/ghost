use rgb::RGB8;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Color {
    Indexed(u8),
    RGB(RGB8),
}

impl Color {
    pub fn rgb(r: u8, g: u8, b: u8) -> Self {
        Self::RGB(RGB8::new(r, g, b))
    }
}

/// The standard xterm 16-color base palette (indices 0..=15) — the default a
/// color scheme replaces and an OSC 4 query is answered from when neither the
/// scheme nor the app has said otherwise.
#[rustfmt::skip]
pub const ANSI_16: [[u8; 3]; 16] = [
    [0x00, 0x00, 0x00], [0x80, 0x00, 0x00], [0x00, 0x80, 0x00], [0x80, 0x80, 0x00],
    [0x00, 0x00, 0x80], [0x80, 0x00, 0x80], [0x00, 0x80, 0x80], [0xc0, 0xc0, 0xc0],
    [0x80, 0x80, 0x80], [0xff, 0x00, 0x00], [0x00, 0xff, 0x00], [0xff, 0xff, 0x00],
    [0x00, 0x00, 0xff], [0xff, 0x00, 0xff], [0x00, 0xff, 0xff], [0xff, 0xff, 0xff],
];

/// The six channel levels of the 6×6×6 color cube (indices 16..=231).
const CUBE_STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// Resolve an xterm 256-color index to RGB, taking the first 16 from `base` (a
/// scheme's own colors, or [`ANSI_16`]). The cube and the grey ramp above it are
/// scheme-independent — xterm computes them, so schemes don't carry them.
///
/// This is the *default* for an index; an app's OSC 4 override outranks it (see
/// [`crate::Vt::palette`]).
pub fn index_rgb(i: u8, base: &[[u8; 3]; 16]) -> [u8; 3] {
    match i {
        0..=15 => base[i as usize],
        16..=231 => {
            let i = i - 16;
            [
                CUBE_STEPS[(i / 36) as usize],
                CUBE_STEPS[((i / 6) % 6) as usize],
                CUBE_STEPS[(i % 6) as usize],
            ]
        }
        232..=255 => {
            let v = 8 + 10 * (i - 232);
            [v, v, v]
        }
    }
}
