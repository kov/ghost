/// The cursor's drawn shape, set by DECSCUSR (`CSI Ps SP q`). Blinking is not
/// modelled — the blinking and steady variants of each shape collapse to one.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Bar,
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct Cursor {
    pub col: usize,
    pub row: usize,
    pub visible: bool,
    pub shape: CursorShape,
}

impl Default for Cursor {
    fn default() -> Self {
        Self {
            col: 0,
            row: 0,
            visible: true,
            shape: CursorShape::Block,
        }
    }
}

impl From<Cursor> for Option<(usize, usize)> {
    fn from(cursor: Cursor) -> Self {
        if cursor.visible {
            Some((cursor.col, cursor.row))
        } else {
            None
        }
    }
}

impl PartialEq<(usize, usize)> for Cursor {
    fn eq(&self, (other_col, other_row): &(usize, usize)) -> bool {
        *other_col == self.col && *other_row == self.row
    }
}
