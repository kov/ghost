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

use std::collections::{HashMap, HashSet};

pub use ghost_term::CursorShape;
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

/// Where the cursor sits, in cell coordinates (viewport-relative), and the
/// shape to draw it as (DECSCUSR).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorLayout {
    pub col: usize,
    pub row: usize,
    pub shape: CursorShape,
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

/// A kitty-graphics image to draw, resolved to viewport cell coordinates and
/// pixel-free — a *handle* (`image_id`) the renderer resolves to a cached
/// texture, never the pixels themselves (so [`Frame`] stays cheap to `Clone` and
/// compare). The cell rect is the image's footprint; `row` is viewport-relative
/// and may be negative when the image's top has scrolled above the viewport (the
/// renderer clips it). Emitted in ascending `z` so painter's order is `z`-order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ImagePlacement {
    pub image_id: u32,
    /// Top-left cell: `col` is 0-based from the left; `row` is viewport-relative
    /// (0 = top visible row), negative when the image starts above the viewport.
    pub col: usize,
    pub row: isize,
    /// Footprint in cells — the explicit `c`/`r`, or derived from the image's
    /// pixel size and the cell box when the client left sizing to the terminal.
    pub cols: usize,
    pub rows: usize,
    pub z: i32,
    /// Source sub-rectangle of the image to draw, as `[u0, v0, u1, v1]` in 0..1.
    /// `[0, 0, 1, 1]` is the whole image (direct placements); Unicode-placeholder
    /// cells use a per-cell slice so each cell shows its part of the image.
    pub uv: [f32; 4],
}

/// A full frame ready to draw: the laid-out viewport, the cursor, and any images.
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub cols: usize,
    pub rows: usize,
    pub metrics: CellMetrics,
    pub rows_layout: Vec<RowLayout>,
    /// `None` when the cursor is hidden (DECTCEM `?25l`).
    pub cursor: Option<CursorLayout>,
    /// kitty-graphics images overlapping the viewport, in ascending `z`.
    pub images: Vec<ImagePlacement>,
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
        // A kitty Unicode-placeholder cell draws an image slice, not its glyph:
        // end the current run (so a ligature can't reach through it) and leave the
        // column to the image layer. The placement comes from
        // `layout_placeholder_placements`.
        if cell.is_placeholder() {
            if let Some(run) = cur.take() {
                runs.push(run);
            }
            prev_was_cursor = false;
            continue;
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

/// Lay out the visible (live) viewport of `vt` into a [`Frame`].
pub fn layout_frame(vt: &Vt, metrics: CellMetrics) -> Frame {
    layout_frame_at(vt, metrics, 0)
}

/// Lay out the viewport of `vt` scrolled `scroll_offset` lines up into
/// scrollback. `scroll_offset` 0 yields the live viewport (identical to
/// [`layout_frame`]); larger offsets are clamped to the retained history.
///
/// While scrolled into history (`offset > 0`) the live cursor is *not* drawn:
/// the view shows past output, not the edit point, so a cursor block there would
/// be misleading (and the historical row keeps its full run grouping).
pub fn layout_frame_at(vt: &Vt, metrics: CellMetrics, scroll_offset: usize) -> Frame {
    let (cols, rows) = vt.size();
    let offset = scroll_offset.min(vt.scrollback_len());
    let cursor = vt.cursor();
    let cursor_layout = (offset == 0 && cursor.visible).then_some(CursorLayout {
        col: cursor.col,
        row: cursor.row,
        shape: cursor.shape,
    });

    let rows_layout = vt
        .view_at(offset)
        .enumerate()
        .map(|(row, line)| {
            let cursor_col = match cursor_layout {
                Some(c) if c.row == row => Some(c.col),
                _ => None,
            };
            layout_row(line, cursor_col)
        })
        .collect();

    // Both directly-placed images and Unicode-placeholder blocks draw through the
    // same image layer; concatenate, keeping ascending z (placeholders are z 0).
    let mut images = layout_image_placements(vt, metrics, offset, rows);
    images.extend(layout_placeholder_placements(vt, offset, rows));
    images.sort_by_key(|i| i.z);

    Frame {
        cols,
        rows,
        metrics,
        rows_layout,
        cursor: cursor_layout,
        images,
    }
}

/// Resolve `vt`'s kitty-graphics placements into viewport-relative
/// [`ImagePlacement`]s, keeping only those overlapping the visible `rows` (given
/// the scroll `offset`), in ascending `z`.
///
/// Each placement's absolute anchor line maps to a viewport row via the live
/// scroll position; its footprint is the explicit `c`/`r` cells, or — when the
/// client left sizing to the terminal — derived by dividing the image's pixel
/// size by the cell box (the metric the headless `vt` lacks, supplied here).
pub fn layout_image_placements(
    vt: &Vt,
    metrics: CellMetrics,
    offset: usize,
    rows: usize,
) -> Vec<ImagePlacement> {
    // Absolute line index of the top visible row (see `Vt::lines_scrolled_off`).
    let top_abs = vt.lines_scrolled_off() as isize - offset as isize;

    let mut images: Vec<ImagePlacement> = vt
        .graphics_placements()
        .filter_map(|p| {
            let image = vt.graphics_image(p.image_id)?;
            let cell_cols = if p.cols > 0 {
                p.cols as usize
            } else {
                cells_for(image.width as f32, metrics.advance)
            };
            let cell_rows = if p.rows > 0 {
                p.rows as usize
            } else {
                cells_for(image.height as f32, metrics.line_height)
            };
            let row = p.row as isize - top_abs;
            // Keep only placements whose cell span overlaps the viewport.
            if row >= rows as isize || row + cell_rows as isize <= 0 {
                return None;
            }
            Some(ImagePlacement {
                image_id: p.image_id,
                col: p.col,
                row,
                cols: cell_cols,
                rows: cell_rows,
                z: p.z,
                uv: [0.0, 0.0, 1.0, 1.0], // a direct placement draws the whole image
            })
        })
        .collect();

    images.sort_by_key(|i| i.z);
    images
}

/// Resolve kitty Unicode-placeholder cells in the visible viewport into
/// [`ImagePlacement`]s. Placeholder cells (U+10EEEE) carry an image id in their
/// foreground colour and tile the area an image occupies.
///
/// Cells of the same id are grouped into 4-connected components — each is one
/// displayed copy of that image — and every cell of a component emits a 1×1
/// placement sampling the slice of the image it represents (its position within
/// the component's bounding box). So two separate blocks of the same id are two
/// independent copies rather than one image smeared across the gap, and a ragged
/// block paints only its own cells, never the blanks inside its bounding box.
/// Row/column come from cell position, not the kitty diacritics (which the
/// emulator drops); an id with no stored image is skipped (nothing to draw yet).
pub fn layout_placeholder_placements(vt: &Vt, offset: usize, rows: usize) -> Vec<ImagePlacement> {
    // Collect placeholder cells: (row, col) -> image id, in viewport coordinates.
    let mut grid: HashMap<(usize, usize), u32> = HashMap::new();
    for (row, line) in vt.view_at(offset).take(rows).enumerate() {
        for (col, cell) in line.cells().iter().enumerate() {
            if let Some(id) = cell.placeholder_image_id() {
                grid.insert((row, col), id);
            }
        }
    }

    // Deterministic component order: walk cells in row-major order.
    let mut starts: Vec<(usize, usize)> = grid.keys().copied().collect();
    starts.sort_unstable();

    let mut visited: HashSet<(usize, usize)> = HashSet::new();
    let mut out: Vec<ImagePlacement> = Vec::new();
    for &start in &starts {
        if !visited.insert(start) {
            continue;
        }
        let id = grid[&start];
        // Flood-fill the same-id 4-connected component, recording its cells and
        // bounding box.
        let mut members: Vec<(usize, usize)> = Vec::new();
        let (mut r0, mut c0, mut r1, mut c1) = (start.0, start.1, start.0, start.1);
        let mut stack = vec![start];
        while let Some((r, c)) = stack.pop() {
            members.push((r, c));
            r0 = r0.min(r);
            c0 = c0.min(c);
            r1 = r1.max(r);
            c1 = c1.max(c);
            let neighbors = [
                (r.wrapping_sub(1), c),
                (r + 1, c),
                (r, c.wrapping_sub(1)),
                (r, c + 1),
            ];
            for n in neighbors {
                if grid.get(&n) == Some(&id) && visited.insert(n) {
                    stack.push(n);
                }
            }
        }
        if vt.graphics_image(id).is_none() {
            continue; // no stored image -> nothing to draw for this copy
        }
        members.sort_unstable(); // deterministic emission order (row-major)
        let (w, h) = ((c1 - c0 + 1) as f32, (r1 - r0 + 1) as f32);
        for (r, c) in members {
            let (gc, gr) = ((c - c0) as f32, (r - r0) as f32);
            out.push(ImagePlacement {
                image_id: id,
                col: c,
                row: r as isize,
                cols: 1,
                rows: 1,
                z: 0,
                uv: [gc / w, gr / h, (gc + 1.0) / w, (gr + 1.0) / h],
            });
        }
    }
    out
}

/// How many whole cells an image dimension of `px` pixels spans, given a cell
/// edge of `cell` pixels (rounding up; at least one cell).
fn cells_for(px: f32, cell: f32) -> usize {
    if cell <= 0.0 {
        return 1;
    }
    (px / cell).ceil().max(1.0) as usize
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
        assert_eq!(
            f.cursor,
            Some(CursorLayout {
                col: 2,
                row: 0,
                shape: CursorShape::Block,
            })
        );
    }

    #[test]
    fn decscusr_sets_the_cursor_shape_on_the_frame() {
        // The app switches to a bar cursor (DECSCUSR 6), e.g. vim insert mode.
        let v = feed(20, 5, "hi\x1b[6 q");
        let f = layout_frame(&v, M);
        assert_eq!(f.cursor.unwrap().shape, CursorShape::Bar);
        // An underline (4), then back to block (2).
        let v = feed(20, 5, "hi\x1b[4 q");
        assert_eq!(
            layout_frame(&v, M).cursor.unwrap().shape,
            CursorShape::Underline
        );
        let v = feed(20, 5, "hi\x1b[2 q");
        assert_eq!(
            layout_frame(&v, M).cursor.unwrap().shape,
            CursorShape::Block
        );
    }

    #[test]
    fn layout_frame_at_shows_scrollback_and_hides_the_cursor() {
        // 2x2 terminal fed 3 lines, so "aa" lands in scrollback.
        let v = feed(2, 2, "aa\r\nbb\r\ncc");
        // Offset 0 is the live viewport, with a visible cursor — and identical
        // to the plain layout_frame.
        let live = layout_frame_at(&v, M, 0);
        assert_eq!(live.rows_layout[0].runs[0].text, "bb");
        assert!(live.cursor.is_some());
        assert_eq!(live, layout_frame(&v, M));
        // Scrolling up one line brings "aa" to the top and hides the cursor.
        let up = layout_frame_at(&v, M, 1);
        assert_eq!(up.rows_layout[0].runs[0].text, "aa");
        assert_eq!(up.cursor, None, "no live cursor while viewing history");
        // Offsets past the history clamp.
        assert_eq!(layout_frame_at(&v, M, 99), up);
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

    // A 2×1 RGB image (red, green) as the kitty direct-transmission payload.
    const IMG_2X1: &str = "/wAAAP8A";

    #[test]
    fn image_placement_uses_explicit_cell_footprint_at_the_cursor() {
        let v = feed(
            20,
            5,
            &format!("\x1b[2;4H\x1b_Gi=1,a=T,f=24,s=2,v=1,c=3,r=2,z=7;{IMG_2X1}\x1b\\"),
        );

        let f = layout_frame(&v, M);
        assert_eq!(f.images.len(), 1);
        let img = f.images[0];
        assert_eq!(img.image_id, 1);
        assert_eq!((img.col, img.row), (3, 1)); // CUP 2;4 => row 1, col 3 (0-based)
        assert_eq!((img.cols, img.rows), (3, 2)); // explicit c/r
        assert_eq!(img.z, 7);
    }

    #[test]
    fn image_placement_derives_footprint_from_pixels_when_cr_absent() {
        // 20×1 px image (60 zero RGB bytes => 80 base64 'A's), no c/r. With an 8×16
        // cell box: ceil(20/8) = 3 cols, ceil(1/16) = 1 row.
        let payload = "A".repeat(80);
        let v = feed(
            40,
            5,
            &format!("\x1b_Gi=1,a=T,f=24,s=20,v=1;{payload}\x1b\\"),
        );

        let img = layout_frame(&v, M).images[0];
        assert_eq!((img.cols, img.rows), (3, 1));
    }

    #[test]
    fn image_placement_scrolls_with_content_and_culls_offscreen() {
        let mut v = Vt::new(10, 3);
        v.feed_str(&format!("\x1b_Gi=1,a=T,f=24,s=2,v=1;{IMG_2X1}\x1b\\"));
        // Placed at the home row, it sits at viewport row 0.
        assert_eq!(layout_frame(&v, M).images[0].row, 0);

        // Enough output to scroll its (absolute) anchor line above the viewport.
        v.feed_str("a\r\nb\r\nc\r\nd\r\n");
        assert!(
            layout_frame(&v, M).images.is_empty(),
            "an image scrolled above the viewport is culled"
        );
    }

    #[test]
    fn unicode_placeholders_become_per_cell_image_slices() {
        let mut v = Vt::new(10, 3);
        // Transmit (store, don't display) a 2x1 image as id 1.
        v.feed_str(&format!("\x1b_Gi=1,a=t,f=24,s=2,v=1;{IMG_2X1}\x1b\\"));
        // Two placeholder cells on row 0 with foreground = id 1 (RGB 0,0,1).
        v.feed_str("\x1b[38;2;0;0;1m\u{10eeee}\u{10eeee}");

        let f = layout_frame(&v, M);
        // Each cell of the contiguous 2x1 block is its own 1x1 placement sampling
        // its half of the image: left cell -> left half, right cell -> right half.
        assert_eq!(f.images.len(), 2);
        assert_eq!(
            (
                f.images[0].col,
                f.images[0].cols,
                f.images[0].rows,
                f.images[0].uv
            ),
            (0, 1, 1, [0.0, 0.0, 0.5, 1.0])
        );
        assert_eq!(
            (
                f.images[1].col,
                f.images[1].cols,
                f.images[1].rows,
                f.images[1].uv
            ),
            (1, 1, 1, [0.5, 0.0, 1.0, 1.0])
        );
        assert!(f.images.iter().all(|p| p.image_id == 1 && p.row == 0));
        // And the placeholder cells are not laid out as (tofu) text.
        assert!(
            f.rows_layout[0]
                .runs
                .iter()
                .all(|r| !r.text.contains('\u{10eeee}')),
            "placeholder cells draw an image, not their glyph"
        );
    }

    #[test]
    fn non_contiguous_placeholders_of_same_id_are_separate_copies() {
        // The same id shown in two separated cells must be two independent copies,
        // not one image stretched across the gap between them.
        let mut v = Vt::new(10, 3);
        v.feed_str(&format!("\x1b_Gi=1,a=t,f=24,s=2,v=1;{IMG_2X1}\x1b\\"));
        // Placeholder at col 0; move to column 4 (CHA, 1-based); placeholder at col 3.
        v.feed_str("\x1b[38;2;0;0;1m\u{10eeee}\x1b[4G\u{10eeee}");

        let f = layout_frame(&v, M);
        let cols: Vec<usize> = f.images.iter().map(|p| p.col).collect();
        assert_eq!(
            cols,
            vec![0, 3],
            "two copies at their own columns, no gap fill"
        );
        // Each is a standalone 1x1 component, so it shows the whole image.
        assert!(
            f.images
                .iter()
                .all(|p| p.uv == [0.0, 0.0, 1.0, 1.0] && p.cols == 1)
        );
    }

    #[test]
    fn unicode_placeholder_without_a_stored_image_is_skipped() {
        // Placeholder cells referencing an id that was never transmitted produce
        // no placement (nothing to draw), and still aren't laid out as text.
        let mut v = Vt::new(10, 3);
        v.feed_str("\x1b[38;2;0;0;9m\u{10eeee}\u{10eeee}");
        let f = layout_frame(&v, M);
        assert!(f.images.is_empty(), "no stored image -> no placement");
        assert!(
            f.rows_layout
                .first()
                .is_none_or(|r| r.runs.iter().all(|run| !run.text.contains('\u{10eeee}')))
        );
    }

    #[test]
    fn image_placements_are_sorted_by_z() {
        let mut v = Vt::new(20, 5);
        v.feed_str(&format!("\x1b_Gi=1,a=T,f=24,s=2,v=1,z=5;{IMG_2X1}\x1b\\"));
        v.feed_str(&format!("\x1b_Gi=2,a=T,f=24,s=2,v=1,z=-1;{IMG_2X1}\x1b\\"));

        let f = layout_frame(&v, M);
        let order: Vec<(u32, i32)> = f.images.iter().map(|i| (i.image_id, i.z)).collect();
        assert_eq!(order, vec![(2, -1), (1, 5)]); // ascending z = painter order
    }
}
