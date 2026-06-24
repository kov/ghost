use crate::color::Color;
use crate::pen::Pen;

/// The kitty Unicode-placeholder code point. A cell carrying it is a virtual
/// image placement: the image id is encoded in its foreground colour.
pub(crate) const PLACEHOLDER: char = '\u{10eeee}';

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct Cell(char, Occupancy, Pen);

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum Occupancy {
    Single,
    WideHead,
    WideTail,
}

impl Occupancy {
    pub(crate) fn width(&self) -> u8 {
        match self {
            Occupancy::Single => 1,
            Occupancy::WideHead => 2,
            Occupancy::WideTail => 0,
        }
    }
}

impl Cell {
    pub(crate) fn new(ch: char, occupancy: Occupancy, pen: Pen) -> Self {
        Cell(ch, occupancy, pen)
    }

    pub(crate) fn blank(pen: Pen) -> Self {
        Cell(' ', Occupancy::Single, pen)
    }

    pub fn is_default(&self) -> bool {
        self.0 == ' ' && self.1 == Occupancy::Single && self.2.is_default()
    }

    pub fn char(&self) -> char {
        self.0
    }

    /// Whether this is a kitty Unicode-placeholder cell (it stands in for a slice
    /// of an image rather than drawing its character). The renderer paints the
    /// image over it instead of shaping the glyph.
    pub fn is_placeholder(&self) -> bool {
        self.0 == PLACEHOLDER
    }

    /// For a placeholder cell, the image id encoded in its foreground colour —
    /// the 24-bit RGB value, or a palette index — or `None` if it is not a
    /// placeholder or carries no usable (non-zero) id.
    pub fn placeholder_image_id(&self) -> Option<u32> {
        if !self.is_placeholder() {
            return None;
        }
        let id = match self.2.foreground()? {
            Color::RGB(rgb) => {
                (u32::from(rgb.r) << 16) | (u32::from(rgb.g) << 8) | u32::from(rgb.b)
            }
            Color::Indexed(i) => u32::from(i),
        };
        (id != 0).then_some(id)
    }

    pub(crate) fn occupancy(&self) -> Occupancy {
        self.1
    }

    pub fn width(&self) -> u8 {
        self.1.width()
    }

    pub fn pen(&self) -> &Pen {
        &self.2
    }

    pub(crate) fn set(&mut self, ch: char, occupancy: Occupancy, pen: Pen) {
        self.0 = ch;
        self.1 = occupancy;
        self.2 = pen;
    }
}

impl Default for Cell {
    fn default() -> Self {
        Self::blank(Pen::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_fg(ch: char, fg: Option<Color>) -> Cell {
        let pen = Pen {
            foreground: fg,
            ..Pen::default()
        };
        Cell::new(ch, Occupancy::Single, pen)
    }

    #[test]
    fn placeholder_image_id_reads_the_foreground() {
        // RGB foreground packs the 24-bit image id.
        let c = with_fg(PLACEHOLDER, Some(Color::rgb(0x01, 0x02, 0x03)));
        assert!(c.is_placeholder());
        assert_eq!(c.placeholder_image_id(), Some(0x01_02_03));

        // An indexed foreground is the palette index as the id.
        let c = with_fg(PLACEHOLDER, Some(Color::Indexed(42)));
        assert_eq!(c.placeholder_image_id(), Some(42));
    }

    #[test]
    fn placeholder_image_id_rejects_non_placeholder_and_zero_id() {
        // A normal character is never a placeholder, whatever its colour.
        let c = with_fg('A', Some(Color::rgb(0, 0, 5)));
        assert!(!c.is_placeholder());
        assert_eq!(c.placeholder_image_id(), None);

        // A placeholder with no foreground, or a zero id, has no usable id.
        assert_eq!(with_fg(PLACEHOLDER, None).placeholder_image_id(), None);
        assert_eq!(
            with_fg(PLACEHOLDER, Some(Color::rgb(0, 0, 0))).placeholder_image_id(),
            None
        );
    }
}
