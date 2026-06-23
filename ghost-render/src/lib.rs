//! Pure layout model: turn a `vt` terminal grid into a drawable [`Frame`].
//!
//! This is the testable heart of ghost's renderer. It owns no GPU, no window,
//! and no font — it consumes the styled cell grid that [`ghost_term::Vt`] already
//! produces and emits a description of *what to draw where*, in both cell and
//! pixel coordinates. A later stage shapes and rasterizes the runs; this stage
//! decides their boundaries, and that's where the correctness that matters
//! (ligature grouping, wide-cell width, cursor isolation) lives — all of it
//! `assert_eq!`-checkable without a display.
//!
//! The only "font" input is [`CellMetrics`]: a monospace cell box. Pixel
//! positions are pure arithmetic from it, so even geometry is unit-testable.

use ghost_term::{Color, Line, Pen, Vt};

pub mod scene;
pub use scene::{BadgeKind, Layer, RectPx, Rgba, Scene, SceneId, SceneItem};

/// The monospace cell box. The sole metric input the layout needs; pixel
/// coordinates derive from it by multiplication.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellMetrics {
    /// Horizontal advance of one cell, in pixels.
    pub advance: f32,
    /// Height of one row, in pixels.
    pub line_height: f32,
}

/// A run's resolved visual attributes. Palette resolution (indexed→RGB,
/// default fg/bg, inverse swap) happens downstream; here we carry the pen's
/// logical state verbatim so the layout stays free of theme decisions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub faint: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub blink: bool,
    pub inverse: bool,
}

impl Style {
    /// Read a [`Pen`]'s colors and attributes into a [`Style`].
    pub fn from_pen(pen: &Pen) -> Self {
        Style {
            fg: pen.foreground(),
            bg: pen.background(),
            bold: pen.is_bold(),
            faint: pen.is_faint(),
            italic: pen.is_italic(),
            underline: pen.is_underline(),
            strikethrough: pen.is_strikethrough(),
            blink: pen.is_blink(),
            inverse: pen.is_inverse(),
        }
    }
}

/// A maximal sequence of cells eligible to be shaped together: contiguous
/// columns, one identical [`Style`], and never spanning the cursor cell. This
/// is the unit a shaper turns into glyphs — so a ligature can only form within
/// a run, never across a style boundary or through the cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    /// Column where the run starts (0-based).
    pub start_col: usize,
    /// Total columns the run spans, counting a wide cell as 2.
    pub width_cols: usize,
    /// The run's characters; the tail half of a wide cell is omitted.
    pub text: String,
    pub style: Style,
}

impl Run {
    /// Left edge of the run in pixels.
    pub fn pixel_x(&self, metrics: CellMetrics) -> f32 {
        self.start_col as f32 * metrics.advance
    }

    /// Width of the run in pixels.
    pub fn pixel_width(&self, metrics: CellMetrics) -> f32 {
        self.width_cols as f32 * metrics.advance
    }
}

/// One laid-out row: its runs, left to right. Empty when the row draws nothing.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct RowLayout {
    pub runs: Vec<Run>,
}

/// Where the cursor sits, in cell coordinates (viewport-relative).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorLayout {
    pub col: usize,
    pub row: usize,
}

/// A normalized linear text selection over the viewport grid, in 0-based
/// `(row, col)` cell coordinates (matching [`CursorLayout`]). `start` is the
/// earlier cell in reading order; both endpoints are inclusive.
///
/// This is a pure layout fact — the renderer turns it into highlight rectangles
/// and a copy consumer turns it into text — so it lives here, with the other
/// cell-coordinate types, and is exhaustively testable without a display.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    /// Earlier endpoint in reading order, `(row, col)`, inclusive.
    pub start: (usize, usize),
    /// Later endpoint in reading order, `(row, col)`, inclusive.
    pub end: (usize, usize),
}

impl Selection {
    /// Build a normalized selection from an anchor and the active cell, given in
    /// either order; both are `(row, col)` and inclusive.
    pub fn new(anchor: (usize, usize), active: (usize, usize)) -> Self {
        let (start, end) = if anchor <= active {
            (anchor, active)
        } else {
            (active, anchor)
        };
        Selection { start, end }
    }

    /// The half-open column interval `[c0, c1)` selected on `row`, or `None` if
    /// the row lies outside the selection. `cols` is the grid width: the first
    /// and interior rows of a multi-row selection extend to end-of-line.
    pub fn row_span(&self, row: usize, cols: usize) -> Option<(usize, usize)> {
        if row < self.start.0 || row > self.end.0 {
            return None;
        }
        let c0 = if row == self.start.0 { self.start.1 } else { 0 };
        let c1 = if row == self.end.0 {
            self.end.1 + 1
        } else {
            cols
        };
        let c0 = c0.min(cols);
        let c1 = c1.min(cols);
        (c0 < c1).then_some((c0, c1))
    }
}

/// A full frame ready to draw: the laid-out viewport plus the cursor.
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub cols: usize,
    pub rows: usize,
    pub metrics: CellMetrics,
    pub rows_layout: Vec<RowLayout>,
    /// `None` when the cursor is hidden (DECTCEM `?25l`).
    pub cursor: Option<CursorLayout>,
}

/// Lay out a single line into style- and cursor-delimited [`Run`]s.
///
/// `cursor_col` is the cursor's column on this row, or `None` if the (visible)
/// cursor is elsewhere; the cursor cell becomes its own run so ligatures never
/// render through it.
pub fn layout_row(line: &Line, cursor_col: Option<usize>) -> RowLayout {
    let cells = line.cells();
    // Trailing default cells (blank space, default pen) draw nothing; stop at
    // the last cell that has something to show.
    let mut end = cells
        .iter()
        .rposition(|c| !c.is_default())
        .map_or(0, |i| i + 1);
    // ...but a visible cursor on a trailing blank (the usual prompt position)
    // must still produce a run so the renderer can draw the cursor block there.
    if let Some(cc) = cursor_col {
        end = end.max(cc + 1).min(cells.len());
    }

    let mut runs: Vec<Run> = Vec::new();
    let mut cur: Option<Run> = None;
    // The cell after the cursor must also break, so the cursor cell is isolated
    // on both sides (a ligature can't reach through it).
    let mut prev_was_cursor = false;

    for (col, cell) in cells[..end].iter().enumerate() {
        let width = cell.width() as usize;
        if width == 0 {
            continue; // the tail half of a wide cell — its head already counted it
        }
        let style = Style::from_pen(cell.pen());
        let is_cursor = cursor_col == Some(col);
        let start_new = match &cur {
            None => true,
            Some(run) => is_cursor || prev_was_cursor || run.style != style,
        };

        if start_new {
            if let Some(run) = cur.take() {
                runs.push(run);
            }
            cur = Some(Run {
                start_col: col,
                width_cols: width,
                text: cell.char().to_string(),
                style,
            });
        } else {
            let run = cur.as_mut().expect("non-start implies an open run");
            run.width_cols += width;
            run.text.push(cell.char());
        }
        prev_was_cursor = is_cursor;
    }

    if let Some(run) = cur.take() {
        runs.push(run);
    }
    RowLayout { runs }
}

/// Lay out the visible viewport of `vt` into a [`Frame`].
pub fn layout_frame(vt: &Vt, metrics: CellMetrics) -> Frame {
    let (cols, rows) = vt.size();
    let cursor = vt.cursor();
    let cursor_layout = cursor.visible.then_some(CursorLayout {
        col: cursor.col,
        row: cursor.row,
    });

    let rows_layout = vt
        .view()
        .enumerate()
        .map(|(row, line)| {
            let cursor_col = match cursor_layout {
                Some(c) if c.row == row => Some(c.col),
                _ => None,
            };
            layout_row(line, cursor_col)
        })
        .collect();

    Frame {
        cols,
        rows,
        metrics,
        rows_layout,
        cursor: cursor_layout,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ghost_term::Vt;

    #[test]
    fn selection_row_span_covers_linear_range() {
        // Single row, inclusive columns [2, 5] -> half-open [2, 6).
        let s = Selection::new((0, 2), (0, 5));
        assert_eq!(s.row_span(0, 80), Some((2, 6)));
        assert_eq!(s.row_span(1, 80), None);

        // Endpoints given in either order normalize the same.
        assert_eq!(Selection::new((0, 5), (0, 2)).row_span(0, 80), Some((2, 6)));

        // Multi-row: first row runs to EOL, interior rows are full, the last row
        // runs from column 0 to its (inclusive) endpoint.
        let s = Selection::new((1, 3), (3, 4));
        assert_eq!(s.row_span(0, 10), None);
        assert_eq!(s.row_span(1, 10), Some((3, 10)));
        assert_eq!(s.row_span(2, 10), Some((0, 10)));
        assert_eq!(s.row_span(3, 10), Some((0, 5)));
        assert_eq!(s.row_span(4, 10), None);

        // Columns are clamped to the grid width.
        assert_eq!(
            Selection::new((0, 0), (0, 50)).row_span(0, 10),
            Some((0, 10))
        );
    }

    const M: CellMetrics = CellMetrics {
        advance: 8.0,
        line_height: 16.0,
    };

    /// A fresh terminal fed `s`.
    fn feed(cols: usize, rows: usize, s: &str) -> Vt {
        let mut v = Vt::new(cols, rows);
        v.feed_str(s);
        v
    }

    /// The first viewport row of `v`.
    fn row0(v: &Vt) -> &Line {
        v.view().next().unwrap()
    }

    #[test]
    fn style_reads_pen_attributes() {
        // SGR 1;3;4 = bold, italic, underline.
        let v = feed(10, 1, "\x1b[1;3;4ma");
        let st = Style::from_pen(row0(&v).cells()[0].pen());
        assert!(st.bold && st.italic && st.underline);
        assert!(!st.inverse && !st.strikethrough && !st.blink && !st.faint);
    }

    #[test]
    fn plain_text_is_a_single_run() {
        let v = feed(10, 1, "hello");
        let row = layout_row(row0(&v), None);
        assert_eq!(row.runs.len(), 1, "one style, one run");
        let r = &row.runs[0];
        assert_eq!(r.start_col, 0);
        assert_eq!(r.text, "hello", "trailing blank cells are trimmed");
        assert_eq!(r.width_cols, 5);
    }

    #[test]
    fn blank_row_has_no_runs() {
        let v = feed(10, 1, "");
        assert!(layout_row(row0(&v), None).runs.is_empty());
    }

    #[test]
    fn style_change_splits_runs() {
        let v = feed(10, 1, "a\x1b[1mb");
        let row = layout_row(row0(&v), None);
        assert_eq!(row.runs.len(), 2);
        assert_eq!(row.runs[0].text, "a");
        assert!(!row.runs[0].style.bold);
        assert_eq!(row.runs[1].text, "b");
        assert!(row.runs[1].style.bold);
        assert_eq!(row.runs[1].start_col, 1);
    }

    #[test]
    fn wide_char_counts_two_columns() {
        // '世' is East-Asian wide: one glyph, two cells.
        let v = feed(10, 1, "a世b");
        let row = layout_row(row0(&v), None);
        assert_eq!(row.runs.len(), 1, "uniform style stays one run");
        let r = &row.runs[0];
        assert_eq!(r.text, "a世b", "the wide tail cell contributes no char");
        assert_eq!(r.width_cols, 4, "1 + 2 + 1 columns");
        assert_eq!(r.start_col, 0);
    }

    #[test]
    fn cursor_cell_is_its_own_run() {
        let mut v = feed(10, 1, "abc");
        v.feed_str("\x1b[1;2H"); // move cursor to row 1, col 2 (1-based) => col 1
        let cur = v.cursor();
        assert_eq!((cur.col, cur.row), (1, 0));
        let row = layout_row(row0(&v), Some(cur.col));
        // Same style throughout, but the cursor isolates 'b'.
        assert_eq!(row.runs.len(), 3);
        assert_eq!(row.runs[0].text, "a");
        assert_eq!(row.runs[1].text, "b");
        assert_eq!(row.runs[1].start_col, 1);
        assert_eq!(row.runs[2].text, "c");
        assert_eq!(row.runs[2].start_col, 2);
    }

    #[test]
    fn visible_cursor_on_trailing_blank_is_a_run() {
        // After "hi" the cursor sits on col 2 — a trailing blank (default) cell,
        // the usual shell-prompt position. It must still become its own run so a
        // renderer can draw the cursor block there, even though the trailing-cell
        // trim would otherwise drop it.
        let v = feed(10, 1, "hi");
        let cur = v.cursor();
        assert_eq!((cur.col, cur.row), (2, 0));
        let row = layout_row(row0(&v), Some(cur.col));
        let cursor_run = row.runs.iter().find(|r| r.start_col == 2);
        assert!(
            cursor_run.is_some_and(|r| r.width_cols == 1),
            "cursor cell at col 2 should be its own 1-wide run, got {:?}",
            row.runs
        );
    }

    #[test]
    fn frame_lays_out_view_and_cursor() {
        let v = feed(20, 5, "hi");
        let f = layout_frame(&v, M);
        assert_eq!((f.cols, f.rows), (20, 5));
        assert_eq!(f.rows_layout.len(), 5);
        assert_eq!(f.rows_layout[0].runs[0].text, "hi");
        assert_eq!(f.cursor, Some(CursorLayout { col: 2, row: 0 }));
    }

    #[test]
    fn hidden_cursor_is_none_and_does_not_split() {
        let v = feed(10, 1, "abc\x1b[?25l"); // DECTCEM hide
        let f = layout_frame(&v, M);
        assert_eq!(f.cursor, None);
        assert_eq!(f.rows_layout[0].runs.len(), 1, "no cursor => no split");
        assert_eq!(f.rows_layout[0].runs[0].text, "abc");
    }

    #[test]
    fn run_pixel_geometry() {
        let r = Run {
            start_col: 3,
            width_cols: 4,
            text: "test".into(),
            style: Style::from_pen(&Pen::default()),
        };
        assert_eq!(r.pixel_x(M), 24.0);
        assert_eq!(r.pixel_width(M), 32.0);
    }
}
