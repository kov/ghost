use crate::graphics::{Image, Placement};
use crate::line::Line;
use crate::parser::{self, DecMode, DynamicColor, Parser, Progress};
use crate::terminal::{ClipboardSelection, Cursor, Terminal};

/// The active mouse-reporting protocol (DEC private modes 1000/1002/1003),
/// which governs whether — and for which events — a frontend should send mouse
/// reports to the child. Independent of the coordinate encoding ([`Vt::mouse_sgr`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseProtocol {
    /// No mouse reporting (the default).
    Off,
    /// 1000 (X11): button press and release only.
    Press,
    /// 1002: press/release plus motion while a button is held (drag).
    ButtonDrag,
    /// 1003: press/release plus all pointer motion.
    AnyMotion,
}

#[derive(Debug)]
pub struct Vt {
    parser: Parser,
    terminal: Terminal,
}

impl Vt {
    pub fn builder() -> Builder {
        Builder::default()
    }

    pub fn new(cols: usize, rows: usize) -> Vt {
        Self::builder().size(cols, rows).build()
    }

    pub fn feed_str(&mut self, s: &str) -> Changes<'_> {
        s.chars()
            .filter_map(|ch| self.parser.feed(ch))
            .for_each(|op| self.terminal.execute(op));

        let lines = self.terminal.changes();
        let scrollback = self.terminal.gc();

        Changes { lines, scrollback }
    }

    pub fn feed(&mut self, input: char) {
        if let Some(op) = self.parser.feed(input) {
            self.terminal.execute(op);
        }
    }

    pub fn size(&self) -> (usize, usize) {
        self.terminal.size()
    }

    pub fn resize(&mut self, cols: usize, rows: usize) -> Changes<'_> {
        self.terminal.resize(cols, rows);

        let lines = self.terminal.changes();
        let scrollback = self.terminal.gc();

        Changes { lines, scrollback }
    }

    pub fn view(&self) -> impl Iterator<Item = &Line> {
        self.terminal.view()
    }

    /// The viewport scrolled `offset` lines up into scrollback. `offset` 0 is
    /// the live viewport (identical to [`view`](Self::view)); it is clamped to
    /// [`scrollback_len`](Self::scrollback_len).
    pub fn view_at(&self, offset: usize) -> impl Iterator<Item = &Line> {
        self.terminal.view_at(offset)
    }

    /// Number of scrollback lines retained above the viewport — the maximum
    /// scroll-up offset. 0 means the viewport already sits at the bottom.
    pub fn scrollback_len(&self) -> usize {
        self.terminal.scrollback_len()
    }

    /// Monotonic count of lines that have ever scrolled off the top of the
    /// viewport into history (including ones since trimmed). Grows by the gross
    /// lines pushed each feed even at the scrollback cap, so a frontend can pin a
    /// scrolled-back view to fixed content across output.
    pub fn lines_scrolled_off(&self) -> usize {
        self.terminal.lines_scrolled_off()
    }

    pub fn lines(&self) -> impl Iterator<Item = &Line> {
        self.terminal.lines()
    }

    pub fn line(&self, n: usize) -> &Line {
        self.terminal.line(n)
    }

    pub fn text(&self) -> Vec<String> {
        self.terminal.text()
    }

    /// The DECRQCRA rectangle checksum for the visible-screen rectangle with
    /// 0-based inclusive bounds `[top..=bottom] x [left..=right]`, ready to
    /// place verbatim in the `DCS Pid ! ~ HHHH ST` reply.
    ///
    /// This follows xterm's algorithm (what esctest and other conformance tools
    /// expect): each cell contributes its character code plus attribute bits
    /// (bold `+0x80`, blink `+0x40`, inverse `+0x20`, underline `+0x10`; ghost
    /// has no invisible/protected bits), a plain unattributed space (`0x20`) is
    /// trimmed unless it is the rectangle's first cell, and the 16-bit sum is
    /// negated — `(0x10000 - sum) & 0xFFFF` — because the default xterm checksum
    /// esctest reads is the negated form (it recovers the sum as
    /// `0x10000 - reply`). A blank cell therefore checksums to `0x20`, which
    /// esctest's empty-cell assertions rely on. Coordinates are clamped to the
    /// screen so an out-of-range request can't panic.
    pub fn rect_checksum(&self, top: usize, left: usize, bottom: usize, right: usize) -> u16 {
        let (cols, rows) = self.terminal.size();
        if cols == 0 || rows == 0 || top >= rows || left >= cols {
            return 0;
        }
        let bottom = bottom.min(rows - 1);
        let right = right.min(cols - 1);
        let mut sum: u32 = 0;
        let mut first = true;
        for r in top..=bottom {
            let cells = self.terminal.line(r).cells();
            for c in left..=right {
                let Some(cell) = cells.get(c) else { continue };
                let ch = cell.char();
                // ghost stores blanks as ' ', but treat a NUL base char as a
                // space too (belt-and-braces with xterm's undrawn-cell handling).
                let mut v = if ch == '\0' { 0x20 } else { ch as u32 };
                let pen = cell.pen();
                if pen.is_bold() {
                    v += 0x80;
                }
                if pen.is_blink() {
                    v += 0x40;
                }
                if pen.is_inverse() {
                    v += 0x20;
                }
                if pen.is_underline() {
                    v += 0x10;
                }
                // A plain unattributed space contributes only as the first cell;
                // interior and trailing blanks are trimmed (as DEC terminals do).
                if first || v != 0x20 {
                    sum = sum.wrapping_add(v);
                }
                first = false;
            }
        }
        (0x10000u32.wrapping_sub(sum) & 0xFFFF) as u16
    }

    /// The URI behind an interned OSC 8 hyperlink id — the id a linked cell
    /// carries in `Pen::link_id`.
    pub fn hyperlink(&self, id: u16) -> Option<&str> {
        self.terminal.hyperlink(id)
    }

    /// Drain the decoded OSC 52 clipboard writes queued while feeding output.
    pub fn take_clipboard_writes(&mut self) -> Vec<(ClipboardSelection, String)> {
        self.terminal.take_clipboard_writes()
    }

    /// The app-set default foreground (OSC 10), if any.
    pub fn dynamic_foreground(&self) -> Option<[u8; 3]> {
        self.terminal.dynamic_color(DynamicColor::Foreground)
    }

    /// The app-set default background (OSC 11), if any.
    pub fn dynamic_background(&self) -> Option<[u8; 3]> {
        self.terminal.dynamic_color(DynamicColor::Background)
    }

    /// The app-set cursor color (OSC 12), if any.
    pub fn dynamic_cursor_color(&self) -> Option<[u8; 3]> {
        self.terminal.dynamic_color(DynamicColor::Cursor)
    }

    /// The app-set color for palette index `i` (OSC 4), if any — `None` leaves the
    /// index to the frontend's theme.
    pub fn palette_color(&self, i: u8) -> Option<[u8; 3]> {
        self.terminal.palette_color(i)
    }

    /// The whole OSC 4 override table, for a renderer resolving indexed colors.
    pub fn palette(&self) -> &[Option<[u8; 3]>; 256] {
        self.terminal.palette()
    }

    /// Whether the app has left the palette untouched (the common case).
    pub fn palette_is_default(&self) -> bool {
        self.terminal.palette_is_default()
    }

    /// The task progress the app last reported (OSC 9;4), if any.
    pub fn progress(&self) -> Option<Progress> {
        self.terminal.progress()
    }

    /// Absolute rows (same space as [`lines_scrolled_off`](Self::lines_scrolled_off))
    /// of OSC 133;A prompt starts still reachable through the retained
    /// scrollback, oldest first.
    pub fn prompt_rows(&self) -> impl Iterator<Item = usize> + '_ {
        self.terminal.prompt_rows()
    }

    /// Whether an OSC 133 shell-integration command is currently running
    /// (`;C` seen, no `;D` yet).
    pub fn command_running(&self) -> bool {
        self.terminal.command_running()
    }

    /// Exit status reported by the most recent OSC 133;D, if it carried one.
    pub fn last_exit_status(&self) -> Option<i32> {
        self.terminal.last_exit_status()
    }

    pub fn title(&self) -> &str {
        self.terminal.title()
    }

    /// How many times the terminal bell (BEL) has rung since creation. A host can
    /// poll this after feeding to detect a ring even with nobody attached.
    pub fn bell_count(&self) -> u64 {
        self.terminal.bell_count()
    }

    /// The cursor position for a cursor-position report, 0-based `(col, row)`,
    /// origin-relative in origin mode. See [`Terminal::cursor_report`].
    pub fn cursor_report(&self) -> (usize, usize) {
        self.terminal.cursor_report()
    }

    pub fn cursor(&self) -> Cursor {
        self.terminal.cursor()
    }

    /// The current left/right margins as 1-based inclusive columns (DECRQSS
    /// DECSLRM report). See [`Terminal::left_right_margins`].
    pub fn left_right_margins(&self) -> (usize, usize) {
        self.terminal.left_right_margins()
    }

    /// The current top/bottom margins as 1-based inclusive rows (DECRQSS DECSTBM
    /// report). See [`Terminal::top_bottom_margins`].
    pub fn top_bottom_margins(&self) -> (usize, usize) {
        self.terminal.top_bottom_margins()
    }

    /// The current pen as a DECRQSS SGR report body (e.g. `"0;1"`). See
    /// [`Terminal::sgr_report`].
    pub fn sgr_report(&self) -> String {
        self.terminal.sgr_report()
    }

    /// The DECSCL conformance level (1–5). See [`Terminal::conformance_level`].
    pub fn conformance_level(&self) -> u8 {
        self.terminal.conformance_level()
    }

    /// The current DECSCA state (0/1) for a DECRQSS `" q` report. See
    /// [`Terminal::decsca_report`].
    pub fn decsca_report(&self) -> u16 {
        self.terminal.decsca_report()
    }

    /// An ANSI mode's state for DECRQM `CSI Ps $ p`. See
    /// [`Terminal::ansi_mode_state`].
    pub fn ansi_mode_state(&self, mode: u16) -> crate::ModeReport {
        self.terminal.ansi_mode_state(mode)
    }

    pub fn cursor_key_app_mode(&self) -> bool {
        self.terminal.cursor_keys_app_mode()
    }

    /// xterm modifyOtherKeys level (0 off, 1, 2) the app negotiated via
    /// `CSI > 4 ; Pv m`. The frontend key encoder reads this to disambiguate
    /// keys (e.g. Ctrl+I from Tab) as `CSI 27 ; mod ; code ~`.
    pub fn modify_other_keys(&self) -> u8 {
        self.terminal.modify_other_keys()
    }

    /// The active kitty keyboard progressive-enhancement flags (0 = legacy), as
    /// negotiated via `CSI > flags u` / `CSI = flags ; mode u`. Supersedes
    /// modifyOtherKeys; the frontend key encoder reads it to pick `CSI u` output.
    pub fn kitty_keyboard_flags(&self) -> u8 {
        self.terminal.kitty_keyboard_flags()
    }

    /// Drain the kitty-graphics acknowledgement bytes the terminal has queued for
    /// the child's input stream (image transfer / query OK and error replies).
    /// Like the cursor and device-attribute queries, these are written back by
    /// the host when detached and by the frontend when attached.
    pub fn take_graphics_responses(&mut self) -> Vec<u8> {
        self.terminal.take_graphics_responses()
    }

    /// A stored kitty-graphics image by id (RGBA8 pixels), for the renderer.
    pub fn graphics_image(&self, id: u32) -> Option<&Image> {
        self.terminal.graphics_image(id)
    }

    /// The number of stored kitty-graphics images.
    pub fn graphics_image_count(&self) -> usize {
        self.terminal.graphics_image_count()
    }

    /// The active kitty-graphics placements the renderer should draw. Each
    /// placement's `row` is an absolute line index; map it to a viewport row with
    /// [`lines_scrolled_off`](Self::lines_scrolled_off).
    pub fn graphics_placements(&self) -> impl Iterator<Item = &Placement> {
        self.terminal.graphics_placements()
    }

    /// The number of active kitty-graphics placements.
    pub fn graphics_placement_count(&self) -> usize {
        self.terminal.graphics_placement_count()
    }

    /// The active mouse-reporting protocol (DEC modes 1000/1002/1003). When more
    /// than one is somehow enabled, the most permissive wins.
    pub fn mouse_protocol(&self) -> MouseProtocol {
        if self.terminal.mode_enabled(DecMode::MouseReportAny) {
            MouseProtocol::AnyMotion
        } else if self.terminal.mode_enabled(DecMode::MouseReportButton) {
            MouseProtocol::ButtonDrag
        } else if self.terminal.mode_enabled(DecMode::MouseReportX11) {
            MouseProtocol::Press
        } else {
            MouseProtocol::Off
        }
    }

    /// Whether SGR extended mouse coordinates (DEC mode 1006) are enabled.
    pub fn mouse_sgr(&self) -> bool {
        self.terminal.mode_enabled(DecMode::MouseSgr)
    }

    /// Whether focus in/out reporting (DEC mode 1004) is enabled.
    pub fn focus_report(&self) -> bool {
        self.terminal.mode_enabled(DecMode::FocusReport)
    }

    /// Whether bracketed paste (DEC mode 2004) is enabled.
    pub fn bracketed_paste(&self) -> bool {
        self.terminal.mode_enabled(DecMode::BracketedPaste)
    }

    /// Whether synchronized output (DEC mode 2026) is active — the app is mid
    /// atomic frame and the frontend should hold presentation until the mode
    /// resets (with a timeout guarding against an app that never resets it).
    pub fn synchronized_output(&self) -> bool {
        self.terminal.mode_enabled(DecMode::SynchronizedOutput)
    }

    /// The DECRPM report for a DEC private mode by raw number. See
    /// [`Terminal::mode_state`].
    pub fn dec_mode_state(&self, mode: u16) -> crate::ModeReport {
        self.terminal.mode_state(mode)
    }

    pub fn dump(&self) -> String {
        let funs = self.terminal.dump();
        let mut seq = parser::dump(&funs);

        seq.push_str(&self.terminal.dump_graphics());
        seq.push_str(&self.parser.dump());

        seq
    }

    /// Like [`dump`](Self::dump), but also replays the retained scrollback above
    /// the viewport, so a terminal fed this sequence regains the scrolled-off
    /// history in its own scrollback.
    pub fn dump_with_scrollback(&self) -> String {
        let funs = self.terminal.dump_with_scrollback();
        let mut seq = parser::dump(&funs);

        seq.push_str(&self.terminal.dump_graphics());
        seq.push_str(&self.parser.dump());

        seq
    }

    /// Like [`dump_with_scrollback`](Self::dump_with_scrollback) but omits the
    /// image transmit escapes, keeping the graphics placements. For the
    /// recording's content-addressed dedup: the image bytes are stored once
    /// out-of-band (see [`graphics_images`](Self::graphics_images)) and the reader
    /// reconstructs the transmits, so they need not be re-inlined every checkpoint.
    pub fn dump_with_scrollback_without_images(&self) -> String {
        let funs = self.terminal.dump_with_scrollback();
        let mut seq = parser::dump(&funs);

        seq.push_str(&self.terminal.dump_graphics_placements());
        seq.push_str(&self.parser.dump());

        seq
    }

    /// The stored kitty-graphics images, for the recording's content-addressed
    /// dedup. Pair with [`dump_with_scrollback_without_images`](Self::dump_with_scrollback_without_images).
    pub fn graphics_images(&self) -> impl Iterator<Item = &Image> {
        self.terminal.graphics_images()
    }
}

pub struct Builder {
    size: (usize, usize),
    scrollback_limit: Option<usize>,
}

impl Builder {
    pub fn size(&mut self, cols: usize, rows: usize) -> &mut Self {
        self.size = (cols, rows);

        self
    }

    pub fn scrollback_limit(&mut self, limit: usize) -> &mut Self {
        self.scrollback_limit = Some(limit);

        self
    }

    pub fn build(&self) -> Vt {
        Vt {
            parser: Parser::new(),
            terminal: Terminal::new(self.size, self.scrollback_limit),
        }
    }
}

impl Default for Builder {
    fn default() -> Self {
        Builder {
            size: (80, 24),
            scrollback_limit: None,
        }
    }
}

pub struct Changes<'a> {
    pub lines: Vec<usize>,
    pub scrollback: Box<dyn Iterator<Item = Line> + 'a>,
}

#[cfg(test)]
mod tests {
    use super::Vt;
    use proptest::prelude::*;
    use std::env;
    use std::fs;

    /// The first `n` cells of a physical grid row as a string (unlike `text()`,
    /// which joins wrapped rows into logical lines).
    fn row_cells(vt: &Vt, row: usize, n: usize) -> String {
        vt.line(row).cells()[..n].iter().map(|c| c.char()).collect()
    }

    /// The 8×8 block esctest's rectangle tests prepare (`abcdefgh` / `ijklmnop` /
    /// …), on a 10-row grid so the last line feed doesn't scroll it.
    fn rect_vt() -> Vt {
        let mut vt = Vt::new(8, 10);
        for line in [
            "abcdefgh", "ijklmnop", "qrstuvwx", "yz012345", "ABCDEFGH", "IJKLMNOP", "QRSTUVWX",
            "YZ6789!@",
        ] {
            vt.feed_str(line);
            vt.feed_str("\r\n");
        }
        vt
    }

    /// The eight rows [`rect_vt`] wrote, as strings.
    fn rect_rows(vt: &Vt) -> Vec<String> {
        (0..8).map(|r| row_cells(vt, r, 8)).collect()
    }

    /// Write the esctest region grid (abcde / fghij / …) with its top-left corner
    /// at 1-based `(col, row)` — how the DECFI/DECBI tests lay out their screen.
    fn grid5_at(vt: &mut Vt, col: usize, row: usize) {
        for (i, s) in ["abcde", "fghij", "klmno", "pqrst", "uvwxy"]
            .iter()
            .enumerate()
        {
            vt.feed_str(&format!("\x1b[{};{col}H{s}", row + i));
        }
    }

    /// Fill a 5×5 vt with the esctest region grid (abcde / fghij / …).
    fn grid5(vt: &mut Vt) {
        for (r, s) in ["abcde", "fghij", "klmno", "pqrst", "uvwxy"]
            .iter()
            .enumerate()
        {
            vt.feed_str(&format!("\x1b[{};1H{s}", r + 1));
        }
    }

    #[test]
    fn autowrap_respects_the_right_margin() {
        let mut vt = Vt::new(10, 5);
        vt.feed_str("\x1b[?69h"); // DECLRMM on
        vt.feed_str("\x1b[2;4s"); // DECSLRM: left col 2, right col 4
        vt.feed_str("\x1b[1;1H"); // CUP(1,1) — absolute top-left
        vt.feed_str("abcdefgh");

        // Text flows to the right margin (col 4), then wraps to the left margin
        // (col 2) on the next line — exactly esctest's test_DECSET_DECLRMM.
        assert_eq!(
            row_cells(&vt, 0, 4),
            "abcd",
            "row 1 fills to the right margin"
        );
        assert_eq!(
            row_cells(&vt, 1, 4),
            " efg",
            "row 2 resumes at the left margin"
        );
        assert_eq!(
            row_cells(&vt, 2, 3),
            " h ",
            "row 3 continues at the left margin"
        );
    }

    #[test]
    fn autowrap_at_the_screen_edge_is_unchanged() {
        let mut vt = Vt::new(4, 3);
        vt.feed_str("abcdef"); // no margins: wrap at the screen edge
        assert_eq!(row_cells(&vt, 0, 4), "abcd");
        assert_eq!(row_cells(&vt, 1, 4), "ef  ");
    }

    #[test]
    fn a_cursor_move_cancels_a_pending_wrap() {
        let mut vt = Vt::new(4, 2);
        vt.feed_str("abcd"); // cursor now pending-wrap at the last column
        vt.feed_str("\x1b[1;1H"); // move home cancels the pending wrap
        vt.feed_str("X"); // must overwrite at col 1, not wrap to row 2
        assert_eq!(row_cells(&vt, 0, 4), "Xbcd");
        assert_eq!(row_cells(&vt, 1, 4), "    ");
    }

    #[test]
    fn a_tab_stops_at_the_right_margin() {
        // esctest test_DECSET_DECAWM_NoLineWrapOnTabWithLeftRightMargin: inside a
        // left/right-margin box a tab stops at the right margin rather than
        // jumping to the next tab stop beyond it.
        let mut vt = Vt::new(80, 24);
        vt.feed_str("\x1b[?69h"); // DECLRMM on
        vt.feed_str("\x1b[10;20s"); // DECSLRM: left col 10, right col 20
        vt.feed_str("\x1b[1;1H"); // home (absolute col 0)

        vt.feed_str("\t");
        assert_eq!(vt.cursor().col, 8, "first tab -> next stop");
        vt.feed_str("\t");
        assert_eq!(vt.cursor().col, 16, "second tab -> next stop");
        vt.feed_str("\t");
        assert_eq!(
            vt.cursor().col,
            19,
            "tab clamps to the right margin (col 20)"
        );
        vt.feed_str("\t");
        assert_eq!(vt.cursor().col, 19, "further tabs stay at the right margin");
    }

    #[test]
    fn a_zero_width_glyph_consumes_a_pending_wrap() {
        // ghost prints even zero-width marks as their own cell (no combining
        // support), so one arriving with a wrap pending must wrap to the next
        // line rather than overwrite the last column. ghost's own dump relies on
        // this to round-trip a full wrapped row (regression from the slice-2
        // pending_wrap change; shrunk from prop_dump).
        let mut vt = Vt::new(10, 5);
        vt.feed_str("     ⺀⺀ "); // fills the row: cursor parks on col 9, wrap pending
        vt.feed_str("\u{fe00}"); // VS-15, width 0 — must wrap to row 1, col 0
        assert_eq!(
            vt.line(0).cells()[9].char(),
            ' ',
            "row 0's last column is intact"
        );
        assert_eq!(
            vt.line(1).cells()[0].char(),
            '\u{fe00}',
            "the mark wrapped down"
        );
        assert_eq!((vt.cursor().col, vt.cursor().row), (1, 1));
    }

    #[test]
    fn su_scrolls_only_the_left_right_margin_box() {
        // esctest test_SU_RespectsLeftRightScrollRegion on a 5x5 grid: SU scrolls
        // only the boxed columns, leaving cells outside [left,right] in place.
        let mut vt = Vt::new(5, 5);
        for (r, s) in ["abcde", "fghij", "klmno", "pqrst", "uvwxy"]
            .iter()
            .enumerate()
        {
            vt.feed_str(&format!("\x1b[{};1H{s}", r + 1));
        }
        vt.feed_str("\x1b[?69h"); // DECLRMM on
        vt.feed_str("\x1b[2;4s"); // DECSLRM: left col 2, right col 4
        vt.feed_str("\x1b[3;2H"); // cursor inside the box
        vt.feed_str("\x1b[2S"); // SU 2

        assert_eq!(row_cells(&vt, 0, 5), "almne");
        assert_eq!(row_cells(&vt, 1, 5), "fqrsj");
        assert_eq!(row_cells(&vt, 2, 5), "kvwxo");
        assert_eq!(row_cells(&vt, 3, 5), "p   t");
        assert_eq!(row_cells(&vt, 4, 5), "u   y");
    }

    #[test]
    fn sd_scrolls_only_the_left_right_margin_box() {
        // SU's counterpart: SD moves the boxed columns down, outside columns stay.
        let mut vt = Vt::new(5, 5);
        for (r, s) in ["abcde", "fghij", "klmno", "pqrst", "uvwxy"]
            .iter()
            .enumerate()
        {
            vt.feed_str(&format!("\x1b[{};1H{s}", r + 1));
        }
        vt.feed_str("\x1b[?69h"); // DECLRMM on
        vt.feed_str("\x1b[2;4s"); // DECSLRM: left col 2, right col 4
        vt.feed_str("\x1b[3;2H"); // cursor inside the box
        vt.feed_str("\x1b[2T"); // SD 2

        assert_eq!(row_cells(&vt, 0, 5), "a   e");
        assert_eq!(row_cells(&vt, 1, 5), "f   j");
        assert_eq!(row_cells(&vt, 2, 5), "kbcdo");
        assert_eq!(row_cells(&vt, 3, 5), "pghit");
        assert_eq!(row_cells(&vt, 4, 5), "ulmny");
    }

    #[test]
    fn a_line_feed_outside_the_box_is_inert_at_the_bottom_margin() {
        // esctest test_LF_MovesDoesNotScrollOutsideLeftRight: at the bottom margin
        // but outside the L/R box a line feed neither scrolls nor moves down.
        let mut vt = Vt::new(10, 8);
        vt.feed_str("\x1b[2;5r"); // DECSTBM rows 2..5 (bottom_margin = row 4)
        vt.feed_str("\x1b[?69h");
        vt.feed_str("\x1b[2;5s"); // DECSLRM cols 2..5 (box cols 1..=4)
        vt.feed_str("\x1b[5;3Hx"); // 'x' at row 4, col 2 (inside the box)

        vt.feed_str("\x1b[5;6H\n"); // cursor right of the box, at the bottom margin
        assert_eq!(
            (vt.cursor().col, vt.cursor().row),
            (5, 4),
            "frozen right of box"
        );
        assert_eq!(vt.line(4).cells()[2].char(), 'x', "no scroll");

        vt.feed_str("\x1b[5;1H\n"); // cursor left of the box, at the bottom margin
        assert_eq!(
            (vt.cursor().col, vt.cursor().row),
            (0, 4),
            "frozen left of box"
        );
        assert_eq!(vt.line(4).cells()[2].char(), 'x', "still no scroll");

        vt.feed_str("\x1b[4;6H\n"); // above the bottom margin: may still move down
        assert_eq!(
            (vt.cursor().col, vt.cursor().row),
            (5, 4),
            "moves down, no scroll"
        );
    }

    #[test]
    fn a_reverse_index_outside_the_box_is_inert_at_the_top_margin() {
        // esctest test_RI_MovesDoesNotScrollOutsideLeftRight.
        let mut vt = Vt::new(10, 8);
        vt.feed_str("\x1b[2;5r"); // DECSTBM rows 2..5 (top_margin = row 1)
        vt.feed_str("\x1b[?69h");
        vt.feed_str("\x1b[2;5s"); // DECSLRM cols 2..5 (box cols 1..=4)
        vt.feed_str("\x1b[5;3Hx"); // 'x' at row 4, col 2 (inside the box)

        vt.feed_str("\x1b[2;6H\x1bM"); // RI right of the box, at the top margin
        assert_eq!(
            (vt.cursor().col, vt.cursor().row),
            (5, 1),
            "frozen right of box"
        );
        assert_eq!(vt.line(4).cells()[2].char(), 'x', "no scroll");
    }

    #[test]
    fn autowrap_inside_a_box_does_not_mark_the_row_wrapped() {
        // A soft wrap at the right margin isn't a logical-line continuation, so
        // the row must not get the `wrapped` flag (which would fuse it with the
        // next row in text()/reflow).
        let mut vt = Vt::new(10, 5);
        vt.feed_str("\x1b[?69h");
        vt.feed_str("\x1b[2;4s"); // box cols 2..4 (left_margin 1, right_margin 3)
        vt.feed_str("\x1b[1;2H"); // cursor at the left margin, row 0
        vt.feed_str("abcdef"); // wraps within the box
        assert!(
            !vt.line(0).wrapped,
            "an in-box wrap must not set the wrapped flag"
        );
    }

    #[test]
    fn ich_inserts_within_the_right_margin() {
        // esctest test_ICH_ScrollOffRightMarginInScrollRegion: ICH shifts right
        // only up to the right margin, dropping the cell there; cells outside the
        // box are untouched.
        let mut vt = Vt::new(10, 3);
        vt.feed_str("abcdefg");
        vt.feed_str("\x1b[?69h\x1b[2;5s"); // box cols 2..5 (left 1, right 4)
        vt.feed_str("\x1b[1;3H"); // cursor at col 2 (inside the box)
        vt.feed_str("\x1b[1@"); // ICH 1
        assert_eq!(row_cells(&vt, 0, 7), "ab cdfg");
    }

    #[test]
    fn ich_outside_the_box_is_a_no_op() {
        // esctest test_ICH_IsNoOpWhenCursorBeginsOutsideScrollRegion.
        let mut vt = Vt::new(10, 3);
        vt.feed_str("abcdefg");
        vt.feed_str("\x1b[?69h\x1b[2;5s"); // box cols 2..5
        vt.feed_str("\x1b[1;1H"); // cursor left of the box
        vt.feed_str("\x1b[10@"); // ICH 10
        assert_eq!(row_cells(&vt, 0, 7), "abcdefg");
    }

    #[test]
    fn dch_deletes_within_the_right_margin() {
        // esctest test_DCH_RespectsMargins: DCH pulls left only within the box,
        // blank-filling at the right margin; cells outside the box are untouched.
        let mut vt = Vt::new(10, 3);
        vt.feed_str("abcde");
        vt.feed_str("\x1b[?69h\x1b[2;4s"); // box cols 2..4 (left 1, right 3)
        vt.feed_str("\x1b[1;3H"); // cursor at col 2 (inside the box)
        vt.feed_str("\x1b[1P"); // DCH 1
        assert_eq!(row_cells(&vt, 0, 5), "abd e");
    }

    #[test]
    fn dch_outside_the_box_is_a_no_op() {
        // esctest test_DCH_DoesNothingOutsideLeftRightMargin.
        let mut vt = Vt::new(10, 3);
        vt.feed_str("abcde");
        vt.feed_str("\x1b[?69h\x1b[2;4s"); // box cols 2..4
        vt.feed_str("\x1b[1;1H"); // cursor left of the box
        vt.feed_str("\x1b[99P"); // DCH 99
        assert_eq!(row_cells(&vt, 0, 5), "abcde");
    }

    #[test]
    fn dl_deletes_lines_within_the_box() {
        // esctest test_DL_InLeftRightScrollRegion: DL pulls lines up only within
        // the boxed columns, leaving cells outside the box in place.
        let mut vt = Vt::new(5, 5);
        grid5(&mut vt);
        vt.feed_str("\x1b[?69h\x1b[2;4s"); // box cols 2..4 (left 1, right 3)
        vt.feed_str("\x1b[2;3H"); // cursor row 1, col 2 (inside the box)
        vt.feed_str("\x1b[1M"); // DL 1
        assert_eq!(row_cells(&vt, 0, 5), "abcde");
        assert_eq!(row_cells(&vt, 1, 5), "flmnj");
        assert_eq!(row_cells(&vt, 2, 5), "kqrso");
        assert_eq!(row_cells(&vt, 3, 5), "pvwxt");
        assert_eq!(row_cells(&vt, 4, 5), "u   y");
    }

    #[test]
    fn il_inserts_lines_within_the_box() {
        // DL's counterpart: IL pushes boxed lines down, outside columns stay.
        let mut vt = Vt::new(5, 5);
        grid5(&mut vt);
        vt.feed_str("\x1b[?69h\x1b[2;4s"); // box cols 2..4
        vt.feed_str("\x1b[2;3H"); // cursor row 1, col 2 (inside the box)
        vt.feed_str("\x1b[1L"); // IL 1
        assert_eq!(row_cells(&vt, 0, 5), "abcde");
        assert_eq!(row_cells(&vt, 1, 5), "f   j");
        assert_eq!(row_cells(&vt, 2, 5), "kghio");
        assert_eq!(row_cells(&vt, 3, 5), "plmnt");
        assert_eq!(row_cells(&vt, 4, 5), "uqrsy");
    }

    #[test]
    fn dl_outside_the_box_is_a_no_op() {
        // esctest test_DL_OutsideLeftRightScrollRegion.
        let mut vt = Vt::new(5, 5);
        grid5(&mut vt);
        vt.feed_str("\x1b[?69h\x1b[2;4s"); // box cols 2..4
        vt.feed_str("\x1b[2;1H"); // cursor left of the box
        vt.feed_str("\x1b[1M"); // DL
        for (r, s) in ["abcde", "fghij", "klmno", "pqrst", "uvwxy"]
            .iter()
            .enumerate()
        {
            assert_eq!(row_cells(&vt, r, 5), *s);
        }
    }

    #[test]
    fn dl_above_the_scroll_region_is_a_no_op() {
        // esctest test_DL_OutsideScrollRegion: DL is inert when the cursor is
        // outside the DECSTBM region (previously it scrolled anyway).
        let mut vt = Vt::new(5, 5);
        grid5(&mut vt);
        vt.feed_str("\x1b[2;4r"); // DECSTBM rows 2..4 (top_margin = row 1)
        vt.feed_str("\x1b[1;3H"); // cursor at row 0, above the top margin
        vt.feed_str("\x1b[1M"); // DL
        for (r, s) in ["abcde", "fghij", "klmno", "pqrst", "uvwxy"]
            .iter()
            .enumerate()
        {
            assert_eq!(row_cells(&vt, r, 5), *s);
        }
    }

    #[test]
    fn decic_inserts_columns_across_the_region() {
        // esctest test_DECIC_ExplicitParam: DECIC inserts blank columns at the
        // cursor column across every row of the vertical region.
        let mut vt = Vt::new(10, 3);
        vt.feed_str("\x1b[1;1Habcdefg\x1b[2;1HABCDEFG\x1b[3;1Hzyxwvut");
        vt.feed_str("\x1b[2;2H"); // cursor row 1, col 1
        vt.feed_str("\x1b['}"); // DECIC 1
        assert_eq!(row_cells(&vt, 0, 8), "a bcdefg");
        assert_eq!(row_cells(&vt, 1, 8), "A BCDEFG");
        assert_eq!(row_cells(&vt, 2, 8), "z yxwvut");
    }

    #[test]
    fn decdc_deletes_columns_across_the_region() {
        // DECIC's counterpart.
        let mut vt = Vt::new(10, 3);
        vt.feed_str("\x1b[1;1Habcdefg\x1b[2;1HABCDEFG");
        vt.feed_str("\x1b[2;2H"); // cursor row 1, col 1
        vt.feed_str("\x1b['~"); // DECDC 1
        assert_eq!(row_cells(&vt, 0, 7), "acdefg ");
        assert_eq!(row_cells(&vt, 1, 7), "ACDEFG ");
    }

    #[test]
    fn decic_only_affects_the_scroll_region() {
        // esctest test_DECIC_CursorWithinTopBottom: rows outside DECSTBM untouched.
        let mut vt = Vt::new(10, 4);
        vt.feed_str("\x1b[1;1Habcdefg\x1b[2;1HABCDEFG");
        vt.feed_str("\x1b[3;1Hzyxwvut\x1b[4;1HZYXWVUT");
        vt.feed_str("\x1b[2;3r"); // DECSTBM rows 2..3 (region rows 1-2)
        vt.feed_str("\x1b[2;2H"); // cursor row 1, col 1 (inside)
        vt.feed_str("\x1b[2'}"); // DECIC 2
        assert_eq!(row_cells(&vt, 0, 7), "abcdefg"); // above: untouched
        assert_eq!(row_cells(&vt, 1, 9), "A  BCDEFG");
        assert_eq!(row_cells(&vt, 2, 9), "z  yxwvut");
        assert_eq!(row_cells(&vt, 3, 7), "ZYXWVUT"); // below: untouched
    }

    #[test]
    fn decic_outside_the_box_is_a_no_op() {
        // esctest test_DECIC_IsNoOpWhenCursorBeginsOutsideScrollRegion.
        let mut vt = Vt::new(10, 2);
        vt.feed_str("\x1b[1;1Habcdefg");
        vt.feed_str("\x1b[?69h\x1b[2;5s"); // box cols 2..5
        vt.feed_str("\x1b[1;1H"); // cursor col 0, outside the box
        vt.feed_str("\x1b[10'}"); // DECIC 10
        assert_eq!(row_cells(&vt, 0, 7), "abcdefg");
    }

    #[test]
    fn decrqm_reports_left_right_margin_mode_state() {
        // DECRQM (`CSI ? 69 $ p`) must reflect DECLRMM's real state, which lives
        // in its own field rather than the tracked-modes set.
        use crate::ModeReport::{Reset, Set};
        let mut vt = Vt::new(80, 24);
        assert_eq!(vt.dec_mode_state(69), Reset, "DECLRMM starts reset");
        vt.feed_str("\x1b[?69h");
        assert_eq!(vt.dec_mode_state(69), Set, "DECLRMM reported set");
        vt.feed_str("\x1b[?69l");
        assert_eq!(vt.dec_mode_state(69), Reset, "DECLRMM reported reset again");
    }

    #[test]
    fn sgr_report_serializes_the_current_pen() {
        // The DECRQSS SGR body is the current pen's op list, always led by a `0`
        // reset — e.g. bold on defaults is `0;1`, and a blank pen is just `0`.
        let mut vt = Vt::new(80, 24);
        assert_eq!(vt.sgr_report(), "0", "default pen reports only the reset");
        vt.feed_str("\x1b[1m");
        assert_eq!(vt.sgr_report(), "0;1", "bold reports 0;1");
        vt.feed_str("\x1b[3;4m"); // italic + underline (bold still set)
        assert_eq!(vt.sgr_report(), "0;1;3;4");
        vt.feed_str("\x1b[m"); // reset back to defaults
        assert_eq!(vt.sgr_report(), "0");
    }

    #[test]
    fn plain_ed_spares_iso_protection_but_not_dec() {
        // A plain ED spares SPA/EPA (ISO) guarded cells but erases DECSCA (DEC)
        // protected ones — matching xterm's ED_respectsISOProtection and
        // ED_should_not_respect_DECSCA.
        let mut vt = Vt::new(10, 2);
        // "ab", then an ISO-guarded "c" (ESC V = SPA, ESC W = EPA).
        vt.feed_str("ab\x1bVc\x1bW");
        vt.feed_str("\x1b[1;1H\x1b[0J"); // home, ED below
        assert_eq!(row_cells(&vt, 0, 3), "  c", "ISO-guarded c is spared");

        // Now with DEC protection (DECSCA), a plain ED erases everything.
        let mut vt = Vt::new(10, 2);
        vt.feed_str("\x1b[1\"qabc\x1b[0\"q"); // DECSCA 1, "abc", DECSCA 0
        vt.feed_str("\x1b[1;1H\x1b[0J");
        assert_eq!(
            row_cells(&vt, 0, 3),
            "   ",
            "DEC-protected cells still erased"
        );
    }

    #[test]
    fn selective_erase_spares_dec_protection() {
        // DECSED / DECSEL spare DECSCA-protected cells and erase the rest.
        let mut vt = Vt::new(10, 2);
        // Protected "ab", then unprotected "cd".
        vt.feed_str("\x1b[1\"qab\x1b[0\"qcd");
        vt.feed_str("\x1b[1;1H\x1b[?0K"); // home, DECSEL to end of line
        assert_eq!(row_cells(&vt, 0, 4), "ab  ", "DECSEL spares protected a,b");

        // DECSED (display) likewise spares protected cells.
        let mut vt = Vt::new(10, 2);
        vt.feed_str("\x1b[1\"qab\x1b[0\"qcd");
        vt.feed_str("\x1b[1;1H\x1b[?2J"); // DECSED whole display
        assert_eq!(row_cells(&vt, 0, 4), "ab  ", "DECSED spares protected a,b");
    }

    #[test]
    fn decsera_erases_a_rectangle_sparing_only_dec_protection() {
        let mut vt = Vt::new(10, 4);
        // Row 1: DEC-protected "AB"; row 2: unprotected "cd"; both at col 1.
        vt.feed_str("\x1b[1\"qAB\x1b[0\"q");
        vt.feed_str("\x1b[2;1Hcd");
        // Erase the 2x2 rect covering both rows, cols 1-2.
        vt.feed_str("\x1b[1;1;2;2$\x7b"); // 0x7b = '{'
        assert_eq!(row_cells(&vt, 0, 2), "AB", "DEC-protected row spared");
        assert_eq!(row_cells(&vt, 1, 2), "  ", "unprotected row erased");

        // DECSERA erases ISO-guarded cells (unlike DECSED/DECSEL).
        let mut vt = Vt::new(10, 2);
        vt.feed_str("a\x1bVb\x1bW"); // "a", ISO-guarded "b"
        vt.feed_str("\x1b[1;1;1;2$\x7b");
        assert_eq!(
            row_cells(&vt, 0, 2),
            "  ",
            "ISO guard does not stop DECSERA"
        );
    }

    #[test]
    fn decera_erases_a_rectangle_and_ignores_the_margins() {
        let mut vt = rect_vt();
        // Margins on both axes, origin mode off: the rectangle's coordinates are
        // absolute, so the erase reaches straight through them (esctest
        // test_DECERA_ignoresMargins).
        vt.feed_str("\x1b[?69h\x1b[3;6s\x1b[3;6r");
        vt.feed_str("\x1b[4;4H"); // park the cursor
        vt.feed_str("\x1b[5;5;7;7$z");
        assert_eq!(
            rect_rows(&vt),
            [
                "abcdefgh", "ijklmnop", "qrstuvwx", "yz012345", "ABCD   H", "IJKL   P", "QRST   X",
                "YZ6789!@"
            ]
        );
        let c = vt.cursor();
        assert_eq!((c.col, c.row), (3, 3), "DECERA does not move the cursor");

        // An inverted rectangle does nothing, and an oversized one clips.
        let mut vt = rect_vt();
        vt.feed_str("\x1b[5;5;4;4$z");
        assert_eq!(
            rect_rows(&vt)[4],
            "ABCDEFGH",
            "inverted rectangle is a no-op"
        );
        vt.feed_str("\x1b[8;8;99;99$z");
        assert_eq!(rect_rows(&vt)[7], "YZ6789! ", "clipped to the screen");
    }

    #[test]
    fn decfra_fills_a_rectangle_with_a_character() {
        let mut vt = rect_vt();
        vt.feed_str("\x1b[37;5;5;7;7$x"); // 37 = '%'
        assert_eq!(
            rect_rows(&vt),
            [
                "abcdefgh", "ijklmnop", "qrstuvwx", "yz012345", "ABCD%%%H", "IJKL%%%P", "QRST%%%X",
                "YZ6789!@"
            ]
        );
        // A fill character outside the printable ranges (here BEL) is ignored, not
        // printed — DEC restricts Pch to 32..126 and 160..255.
        vt.feed_str("\x1b[7;1;1;1;1$x");
        assert_eq!(rect_rows(&vt)[0], "abcdefgh");
        // Omitted bounds default to the whole addressable region.
        vt.feed_str("\x1b[46$x"); // '.'
        assert_eq!(rect_rows(&vt)[0], "........");
        assert_eq!(rect_rows(&vt)[7], "........");
    }

    #[test]
    fn deccra_copies_a_rectangle_source_and_destination_may_overlap() {
        // Non-overlapping: rows 2-4 x cols 2-4 land at (5,5).
        let mut vt = rect_vt();
        vt.feed_str("\x1b[4;4H");
        vt.feed_str("\x1b[2;2;4;4;1;5;5;1$v");
        assert_eq!(
            rect_rows(&vt),
            [
                "abcdefgh", "ijklmnop", "qrstuvwx", "yz012345", "ABCDjklH", "IJKLrstP", "QRSTz01X",
                "YZ6789!@"
            ]
        );
        let c = vt.cursor();
        assert_eq!((c.col, c.row), (3, 3), "DECCRA does not move the cursor");

        // Overlapping: the source is read as it was, not as the copy rewrites it.
        let mut vt = rect_vt();
        vt.feed_str("\x1b[2;2;4;4;1;3;3;1$v");
        assert_eq!(
            rect_rows(&vt),
            [
                "abcdefgh", "ijklmnop", "qrjklvwx", "yzrst345", "ABz01FGH", "IJKLMNOP", "QRSTUVWX",
                "YZ6789!@"
            ]
        );
    }

    #[test]
    fn deccra_defaults_the_destination_to_the_origin_and_clips_it() {
        // Omitted destination = the region's top-left corner.
        let mut vt = rect_vt();
        vt.feed_str("\x1b[2;2;4;4$v");
        assert_eq!(rect_rows(&vt)[..3], ["jkldefgh", "rstlmnop", "z01tuvwx"]);

        // A destination near the edge takes only the part of the rectangle that
        // fits (esctest test_DECCRA_destinationPartiallyOffscreen): the 3×3 block
        // lands as a 2×2 in the bottom-right corner of the 8×10 grid.
        let mut vt = rect_vt();
        vt.feed_str("\x1b[2;2;4;4;1;9;7;1$v");
        assert_eq!(row_cells(&vt, 8, 8), "      jk");
        assert_eq!(row_cells(&vt, 9, 8), "      rs");
    }

    #[test]
    fn deccra_coordinates_are_origin_relative() {
        let mut vt = rect_vt();
        // Margins on both axes + origin mode: source 1,1-3,3 is really 2,2-4,4 and
        // the destination 4,4 is really 5,5 — the same copy as the absolute case.
        vt.feed_str("\x1b[?69h\x1b[2;8s\x1b[2;8r\x1b[?6h");
        vt.feed_str("\x1b[1;1;3;3;1;4;4;1$v");
        vt.feed_str("\x1b[?6l\x1b[?69l\x1b[r");
        assert_eq!(
            rect_rows(&vt),
            [
                "abcdefgh", "ijklmnop", "qrstuvwx", "yz012345", "ABCDjklH", "IJKLrstP", "QRSTz01X",
                "YZ6789!@"
            ]
        );
    }

    #[test]
    fn decfi_moves_forward_and_scrolls_the_box_at_the_right_margin() {
        let mut vt = Vt::new(10, 8);
        grid5_at(&mut vt, 2, 3); // the esctest grid at col 2, row 3

        // Inside the box, short of the right margin: just a step right.
        vt.feed_str("\x1b[?69h\x1b[3;5s\x1b[4;6r"); // L/R 3-5, T/B 4-6
        vt.feed_str("\x1b[5;4H\x1b9");
        assert_eq!((vt.cursor().col, vt.cursor().row), (4, 4));

        // At the right margin: the box scrolls left, a blank column arrives at the
        // right margin, and the cursor stays put (esctest test_DECFI_Scrolls).
        vt.feed_str("\x1b[5;5H\x1b9");
        assert_eq!((vt.cursor().col, vt.cursor().row), (4, 4), "cursor held");
        assert_eq!(
            (2..7).map(|r| row_cells(&vt, r, 7)).collect::<Vec<_>>(),
            [" abcde ", " fhi j ", " kmn o ", " prs t ", " uvwxy "]
        );

        // Right of the margin the cursor moves on, unconfined — but at the screen's
        // right edge the control is ignored.
        vt.feed_str("\x1b[1;6H\x1b9");
        assert_eq!(vt.cursor().col, 6, "outside the box it just steps right");
        vt.feed_str("\x1b[1;10H\x1b9");
        assert_eq!(vt.cursor().col, 9, "ignored at the screen's right edge");
    }

    #[test]
    fn decbi_moves_back_and_scrolls_the_box_at_the_left_margin() {
        let mut vt = Vt::new(10, 8);
        grid5_at(&mut vt, 2, 3);

        vt.feed_str("\x1b[?69h\x1b[3;5s\x1b[4;6r"); // L/R 3-5, T/B 4-6

        // At the left margin the box scrolls right, blanking the left column; the
        // cursor stays (esctest test_DECBI_Scrolls).
        vt.feed_str("\x1b[5;3H\x1b6");
        assert_eq!((vt.cursor().col, vt.cursor().row), (2, 4), "cursor held");
        assert_eq!(
            (2..7).map(|r| row_cells(&vt, r, 7)).collect::<Vec<_>>(),
            [" abcde ", " f ghj ", " k lmo ", " p qrt ", " uvwxy "]
        );

        // Left of the margin the cursor steps back, and is ignored at column 1.
        vt.feed_str("\x1b[1;2H\x1b6");
        assert_eq!(vt.cursor().col, 0, "outside the box it just steps left");
        vt.feed_str("\x1b[1;1H\x1b6");
        assert_eq!(vt.cursor().col, 0, "ignored at the screen's left edge");
    }

    #[test]
    fn cnl_and_cpl_return_to_the_left_margin() {
        let mut vt = Vt::new(20, 10);
        vt.feed_str("\x1b[2;4r\x1b[?69h\x1b[5;10s"); // T/B 2-4, L/R 5-10

        // Begun inside the region, CNL stops at the bottom margin and lands on the
        // left margin — not column 1 (esctest test_CNL_StopsAtBottomMarginInScrollRegion).
        vt.feed_str("\x1b[3;7H\x1b[99E");
        assert_eq!((vt.cursor().col, vt.cursor().row), (4, 3));

        // Begun below it, CNL runs to the last line, still landing on the margin.
        vt.feed_str("\x1b[6;7H\x1b[99E");
        assert_eq!((vt.cursor().col, vt.cursor().row), (4, 9));

        // CPL is the mirror: up to the top margin, onto the left margin.
        vt.feed_str("\x1b[3;7H\x1b[99F");
        assert_eq!((vt.cursor().col, vt.cursor().row), (4, 1));
    }

    #[test]
    fn decaln_homes_the_cursor_and_clears_the_margins() {
        let mut vt = Vt::new(10, 6);
        vt.feed_str("\x1b[?69h\x1b[2;3s\x1b[4;5r"); // margins on both axes
        vt.feed_str("\x1b[5;5H");
        vt.feed_str("\x1b#8");

        assert_eq!((vt.cursor().col, vt.cursor().row), (0, 0), "cursor homed");
        assert_eq!(row_cells(&vt, 0, 10), "EEEEEEEEEE");
        assert_eq!(row_cells(&vt, 5, 10), "EEEEEEEEEE");

        // The margins are gone, so the cursor can cross where they were.
        vt.feed_str("\x1b[4;2H\x1b[A"); // CUU from the old top margin
        assert_eq!(vt.cursor().row, 2, "passed the old top margin");
        vt.feed_str("\x1b[5;2H\x1b[B"); // CUD from the old bottom margin
        assert_eq!(vt.cursor().row, 5, "passed the old bottom margin");
        vt.feed_str("\x1b[1;2H\x1b[D"); // CUB from the old left margin
        assert_eq!(vt.cursor().col, 0, "passed the old left margin");
    }

    #[test]
    fn decsca_survives_a_dump() {
        // A DECSCA-protected run replays guarded after a dump/reload.
        let mut vt = Vt::new(10, 2);
        vt.feed_str("\x1b[1\"qab\x1b[0\"qcd");
        let mut reloaded = Vt::new(10, 2);
        reloaded.feed_str(&vt.dump());
        // In the reloaded terminal, a DECSEL still spares the protected a,b.
        reloaded.feed_str("\x1b[1;1H\x1b[?0K");
        assert_eq!(row_cells(&reloaded, 0, 4), "ab  ");
    }

    #[test]
    fn decrqm_tracks_inert_dec_modes() {
        // Modes ghost does not act on (DECBKM, DECNKM, DECSCLM, …) still round-trip
        // their DECRQM set/reset bit rather than reporting 0/unrecognized.
        use crate::ModeReport::{Reset, Set};
        let mut vt = Vt::new(80, 24);
        for mode in [4, 5, 18, 19, 42, 66, 67] {
            assert_eq!(vt.dec_mode_state(mode), Reset, "?{mode} starts reset");
            vt.feed_str(&format!("\x1b[?{mode}h"));
            assert_eq!(vt.dec_mode_state(mode), Set, "?{mode} reported set");
            vt.feed_str(&format!("\x1b[?{mode}l"));
            assert_eq!(vt.dec_mode_state(mode), Reset, "?{mode} reported reset");
        }
    }

    #[test]
    fn decrqm_tracks_inert_ansi_modes() {
        // KAM (2) and SRM (12) are inert but round-trip their set/reset bit.
        use crate::ModeReport::{Reset, Set};
        let mut vt = Vt::new(80, 24);
        for mode in [2, 12] {
            assert_eq!(vt.ansi_mode_state(mode), Reset, "mode {mode} starts reset");
            vt.feed_str(&format!("\x1b[{mode}h"));
            assert_eq!(vt.ansi_mode_state(mode), Set, "mode {mode} reported set");
            vt.feed_str(&format!("\x1b[{mode}l"));
            assert_eq!(
                vt.ansi_mode_state(mode),
                Reset,
                "mode {mode} reported reset"
            );
        }
    }

    #[test]
    fn decrqm_reports_permanently_reset_legacy_modes() {
        // Legacy modes recognized by name but never implemented report `4`
        // (permanently reset): DECHCCM (?60), and the ANSI graphic/format modes.
        use crate::ModeReport::{PermanentlyReset, Unrecognized};
        let vt = Vt::new(80, 24);
        assert_eq!(vt.dec_mode_state(60), PermanentlyReset, "DECHCCM");
        for mode in [1, 5, 7, 10, 11, 13, 14, 15, 16, 17, 18, 19] {
            assert_eq!(
                vt.ansi_mode_state(mode),
                PermanentlyReset,
                "ANSI mode {mode} is permanently reset"
            );
        }
        // A truly unknown mode is still unrecognized.
        assert_eq!(vt.dec_mode_state(9999), Unrecognized);
        assert_eq!(vt.ansi_mode_state(99), Unrecognized);
    }

    #[test]
    fn decrqm_deccolm_bit_tracks_the_switch_not_the_column_count() {
        // DECCOLM's DECRQM bit follows the mode switch even if the grid is later
        // resized back (an attached GUI reconciles to its window size). Gated by
        // Allow80To132, matching the physical switch.
        use crate::ModeReport::{Reset, Set};
        let mut vt = Vt::new(80, 24);
        assert_eq!(vt.dec_mode_state(3), Reset, "DECCOLM starts reset");
        // Without ?40 the switch is inert, so the bit stays reset.
        vt.feed_str("\x1b[?3h");
        assert_eq!(vt.dec_mode_state(3), Reset, "inert without Allow80To132");
        // With ?40 on, DECSET 3 sets the bit; a later resize back to 80 leaves it.
        vt.feed_str("\x1b[?40h\x1b[?3h");
        assert_eq!(vt.dec_mode_state(3), Set, "set after the real switch");
        vt.resize(80, 24);
        assert_eq!(vt.dec_mode_state(3), Set, "bit survives a grid resize");
        vt.feed_str("\x1b[?3l");
        assert_eq!(vt.dec_mode_state(3), Reset, "reset by DECRESET 3");
    }

    #[test]
    fn a_hard_reset_leaves_132_column_mode_but_keeps_a_window_sized_grid() {
        // esctest test_RIS_ResetDECCOLM. The grid normally belongs to the window,
        // so a hard reset must not resize it...
        let mut vt = Vt::new(100, 24);
        vt.feed_str("\x1bc");
        assert_eq!(vt.size(), (100, 24), "RIS left the window's grid alone");
        // ...but 132-column mode is a width the *program* asked for (DECCOLM, gated
        // on Allow80To132), so a hard reset takes it back to 80.
        vt.feed_str("\x1b[?40h\x1b[?3h");
        assert_eq!(vt.size(), (132, 24));
        vt.feed_str("\x1bc");
        assert_eq!(vt.size(), (80, 24), "RIS left 132-column mode");
        assert_eq!(vt.dec_mode_state(3), crate::ModeReport::Reset);
        // And the restored 80 columns are a plain grid again: another RIS keeps it.
        vt.feed_str("\x1bc");
        assert_eq!(vt.size(), (80, 24));
    }

    #[test]
    fn decscl_gates_declrmm_below_conformance_level_4() {
        // esctest test_DSCSCL_Level3: at level 3 DECLRMM (?69) is inert, so DECSLRM
        // sets no margins and text past the intended right margin does not wrap.
        use crate::ModeReport::{Reset, Set};
        let mut vt = Vt::new(80, 25);
        vt.feed_str("\x1b[63\"p"); // DECSCL level 3
        vt.feed_str("\x1b[?69h"); // DECLRMM — ignored at level 3
        assert_eq!(vt.dec_mode_state(69), Reset, "?69 stays off at level 3");
        vt.feed_str("\x1b[5;6s"); // DECSLRM — treated as SCOSC, no margins
        vt.feed_str("\x1b[1;5Habc"); // write from col 4
        assert_eq!(
            vt.cursor().col,
            7,
            "no right margin, cursor advances to col 7"
        );

        // At level 4 the same sequence enables the margins and stops the wrap.
        vt.feed_str("\x1b[64\"p"); // DECSCL level 4 (hard reset)
        vt.feed_str("\x1b[?69h");
        assert_eq!(vt.dec_mode_state(69), Set, "?69 works at level 4");
    }

    #[test]
    fn decscl_gates_decncsm_below_conformance_level_5() {
        // DECNCSM (?95) is a VT500 feature: settable only at level 5.
        use crate::ModeReport::{Reset, Set};
        let mut vt = Vt::new(80, 25);
        vt.feed_str("\x1b[64\"p\x1b[?95h"); // level 4 — ?95 ignored
        assert_eq!(vt.dec_mode_state(95), Reset, "?95 stays off at level 4");
        vt.feed_str("\x1b[65\"p\x1b[?95h"); // level 5 — ?95 settable
        assert_eq!(vt.dec_mode_state(95), Set, "?95 works at level 5");
    }

    #[test]
    fn decscl_hard_resets_and_reports_ansi_mode_by_level() {
        // DECSCL performs a hard reset (insert mode cleared) and drives the
        // conformance level the query layer reads for ANSI-mode DECRQM.
        use crate::ModeReport::{Reset, Set};
        let mut vt = Vt::new(80, 25);
        vt.feed_str("\x1b[4h"); // IRM (insert mode) on
        assert_eq!(vt.ansi_mode_state(4), Set);
        vt.feed_str("\x1b[62\"p"); // DECSCL level 2 — hard reset
        assert_eq!(
            vt.ansi_mode_state(4),
            Reset,
            "hard reset cleared insert mode"
        );
        assert_eq!(vt.conformance_level(), 2);
    }

    #[test]
    fn deccolm_resizes_clears_and_homes_when_allowed() {
        // esctest test_DECSET_DECCOLM: with Allow80To132 on, DECCOLM switches to
        // 132 columns, clears the screen, and homes the cursor.
        let mut vt = Vt::new(80, 25);
        vt.feed_str("\x1b[?40h"); // Allow80To132
        vt.feed_str("\x1b[5;5Hxyz"); // 'xyz' at row 4
        vt.feed_str("\x1b[6;11r\x1b[?69h\x1b[3;10s"); // top/bottom + L/R margins
        vt.feed_str("\x1b[?3h"); // DECCOLM -> 132
        assert_eq!(vt.size().0, 132, "resized to 132 columns");
        assert_eq!((vt.cursor().col, vt.cursor().row), (0, 0), "cursor homed");
        assert_eq!(row_cells(&vt, 4, 8).trim(), "", "screen cleared");
        assert_eq!(
            vt.dec_mode_state(69),
            crate::ModeReport::Reset,
            "DECLRMM reset"
        );
        // The top/bottom region must reset too (it survived a col-only resize),
        // so line feeds walk down the full screen rather than scrolling a stale
        // 2-row DECSTBM box.
        vt.feed_str("\r\nHello\r\nWorld");
        assert_eq!(&row_cells(&vt, 1, 5), "Hello", "second row after home");
        assert_eq!(&row_cells(&vt, 2, 5), "World", "third row, not scrolled");
    }

    #[test]
    fn deccolm_is_inert_without_allow_80_to_132() {
        // esctest test_DECSET_Allow80To132: DECCOLM only has an effect while ?40
        // is on; toggling ?3 without it leaves the width unchanged.
        let mut vt = Vt::new(80, 25);
        vt.feed_str("\x1b[?3h"); // no ?40
        assert_eq!(vt.size().0, 80, "inert without Allow80To132");
        vt.feed_str("\x1b[?40h\x1b[?3h"); // allow, then enter 132
        assert_eq!(vt.size().0, 132);
        vt.feed_str("\x1b[?40l\x1b[?3l"); // disallow, then try to leave — inert
        assert_eq!(vt.size().0, 132, "132->80 also needs ?40");
    }

    #[test]
    fn deccolm_preserves_the_screen_under_decncsm() {
        // With DECNCSM (?95, level 5) set, a column change keeps the screen.
        let mut vt = Vt::new(80, 25);
        vt.feed_str("\x1b[65\"p"); // DECSCL level 5 (so ?95 is settable)
        vt.feed_str("\x1b[?40h\x1b[?95h"); // Allow80To132 + DECNCSM
        vt.feed_str("\x1b[1;1H1"); // '1' at the origin
        vt.feed_str("\x1b[?3h"); // DECCOLM -> 132, but no clear
        assert_eq!(vt.size().0, 132);
        assert_eq!(&row_cells(&vt, 0, 1), "1", "DECNCSM kept the content");
    }

    #[test]
    fn left_right_margins_drive_origin_relative_cursor() {
        let mut vt = Vt::new(80, 25);
        vt.feed_str("\x1b[6;11r"); // DECSTBM: top row 6, bottom 11
        vt.feed_str("\x1b[?69h"); // DECLRMM on
        vt.feed_str("\x1b[5;10s"); // DECSLRM: left col 5, right col 10
        vt.feed_str("\x1b[?6h"); // origin mode on
        vt.feed_str("\x1b[1;1H"); // CUP(1,1) -> origin corner (col5,row6)

        // Absolute landing: 0-based (col4, row5).
        let c = vt.cursor();
        assert_eq!(
            (c.col, c.row),
            (4, 5),
            "origin CUP lands at the margin corner"
        );
        // CPR reports origin-relative (1,1).
        assert_eq!(
            vt.cursor_report(),
            (0, 0),
            "CPR is origin-relative (0-based)"
        );

        // The write lands at the corner; DECRQCRA reads 'X' there, blank elsewhere.
        vt.feed_str("X");
        let neg_x = 0u16.wrapping_sub(u16::from(b'X'));
        assert_eq!(
            vt.rect_checksum(5, 4, 5, 4),
            neg_x,
            "X at absolute (col5,row6)"
        );
        assert_eq!(
            vt.rect_checksum(5, 0, 5, 0),
            0xFFE0,
            "col1 of that row is blank"
        );

        // `CSI s` with the mode on resets margins to full; disabling ?69 too.
        vt.feed_str("\x1b[?6l"); // origin off so addressing is absolute again
        vt.feed_str("\x1b[?69l"); // DECLRMM off -> margins reset to full
        vt.feed_str("\x1b[1;1H\x1b[?6h\x1b[1;1H"); // origin on, home
        assert_eq!(
            vt.cursor().col,
            0,
            "no left margin after ?69l, home is col 1"
        );
    }

    #[test]
    fn cuf_stops_at_the_right_margin_inside_the_box() {
        // esctest test_CUF_StopsAtRightMarginInScrollRegion: a cursor at or left of
        // the right margin can't be moved past it, even without origin mode.
        let mut vt = Vt::new(80, 25);
        vt.feed_str("\x1b[?69h\x1b[5;10s"); // box cols 4..=9 (0-based)
        vt.feed_str("\x1b[3;7H"); // absolute col 6, inside the box
        vt.feed_str("\x1b[80C"); // CUF by a lot
        assert_eq!(vt.cursor().col, 9, "stops at the right margin");
    }

    #[test]
    fn cuf_stops_at_the_screen_edge_when_right_of_the_box() {
        // test_CUF_StopsAtRightEdgeWhenBegunRightOfScrollRegion: begun right of the
        // margin, CUF runs to the screen edge, not the margin.
        let mut vt = Vt::new(80, 25);
        vt.feed_str("\x1b[?69h\x1b[5;10s");
        vt.feed_str("\x1b[3;12H"); // absolute col 11, right of the box
        vt.feed_str("\x1b[80C");
        assert_eq!(vt.cursor().col, 79, "stops at the screen edge");
    }

    #[test]
    fn cub_stops_at_the_left_margin_inside_the_box() {
        // esctest test_CUB_StopsAtLeftMarginInScrollRegion.
        let mut vt = Vt::new(80, 25);
        vt.feed_str("\x1b[?69h\x1b[5;10s");
        vt.feed_str("\x1b[3;7H"); // absolute col 6, inside the box
        vt.feed_str("\x1b[99D"); // CUB by a lot
        assert_eq!(vt.cursor().col, 4, "stops at the left margin");
    }

    #[test]
    fn cub_stops_at_the_screen_edge_when_left_of_the_box() {
        // test_CUB_StopsAtLeftEdgeWhenBegunLeftOfScrollRegion.
        let mut vt = Vt::new(80, 25);
        vt.feed_str("\x1b[?69h\x1b[5;10s");
        vt.feed_str("\x1b[3;4H"); // absolute col 3, left of the box
        vt.feed_str("\x1b[99D");
        assert_eq!(vt.cursor().col, 0, "stops at the screen edge");
    }

    #[test]
    fn reverse_wrap_wraps_to_the_previous_rows_right_edge() {
        // esctest test_BS_WrapsInWraparoundMode: with ?7 + ?45, a backspace at the
        // left edge wraps up to the last column of the row above.
        let mut vt = Vt::new(80, 25);
        vt.feed_str("\x1b[?7h\x1b[?45h"); // DECAWM + reverse-wrap
        vt.feed_str("\x1b[3;1H"); // row 2, col 0
        vt.feed_str("\x08"); // BS
        assert_eq!((vt.cursor().col, vt.cursor().row), (79, 1));
    }

    #[test]
    fn reverse_wrap_requires_both_decawm_and_mode_45() {
        // test_BS_ReverseWrapRequiresDECAWM / test_BS_NoWrapByDefault: neither mode
        // alone wraps.
        let mut vt = Vt::new(80, 25);
        vt.feed_str("\x1b[?7l\x1b[?45h"); // reverse-wrap on, DECAWM off
        vt.feed_str("\x1b[3;1H\x08");
        assert_eq!(
            (vt.cursor().col, vt.cursor().row),
            (0, 2),
            "no wrap without DECAWM"
        );

        vt.feed_str("\x1b[?7h\x1b[?45l"); // DECAWM on, reverse-wrap off
        vt.feed_str("\x1b[3;1H\x08");
        assert_eq!(
            (vt.cursor().col, vt.cursor().row),
            (0, 2),
            "no wrap without ?45"
        );
    }

    #[test]
    fn reverse_wrap_lands_on_the_right_margin_inside_a_box() {
        // test_BS_ReverseWrapWithLeftRight: from the left margin, wrap to the right
        // margin of the row above.
        let mut vt = Vt::new(80, 25);
        vt.feed_str("\x1b[?7h\x1b[?45h\x1b[?69h\x1b[5;10s"); // box cols 4..=9
        vt.feed_str("\x1b[3;5H"); // absolute col 4 (the left margin), row 2
        vt.feed_str("\x08");
        assert_eq!((vt.cursor().col, vt.cursor().row), (9, 1));
    }

    #[test]
    fn reverse_wrap_from_left_of_the_box_lands_on_the_right_margin() {
        // test_BS_ReversewrapFromLeftEdgeToRightMargin: begun at the screen's left
        // edge (left of the left margin), a backspace still wraps to the right
        // margin of the row above.
        let mut vt = Vt::new(80, 25);
        vt.feed_str("\x1b[?7h\x1b[?45h\x1b[?69h\x1b[5;10s");
        vt.feed_str("\x1b[3;1H"); // absolute col 0, left of the box
        vt.feed_str("\x08");
        assert_eq!((vt.cursor().col, vt.cursor().row), (9, 1));
    }

    #[test]
    fn reverse_wrap_wraps_around_the_top_of_the_scroll_region() {
        // test_BS_ReverseWrapGoesToBottom: at the top margin, a reverse wrap lands
        // on the bottom margin, staying inside the vertical region.
        let mut vt = Vt::new(80, 25);
        vt.feed_str("\x1b[?7h\x1b[?45h\x1b[2;5r"); // DECSTBM rows 1..=4 (0-based)
        vt.feed_str("\x1b[2;1H"); // row 1 (top margin), col 0
        vt.feed_str("\x08");
        assert_eq!(
            (vt.cursor().col, vt.cursor().row),
            (79, 4),
            "top margin wraps to bottom"
        );
    }

    #[test]
    fn reverse_wrap_counts_across_several_rows() {
        // esctest test_CUB_AfterNoWrappedInlines geometry: 160 backspaces from
        // (col4,row4) on an 80-wide screen walk back two full rows to (col4,row2).
        let mut vt = Vt::new(80, 25);
        vt.feed_str("\x1b[?7h\x1b[?45h");
        vt.feed_str("\x1b[5;5H"); // row 4, col 4
        vt.feed_str("\x1b[160D"); // CUB 160
        assert_eq!((vt.cursor().col, vt.cursor().row), (4, 2));
    }

    #[test]
    fn reverse_wrap_from_a_pending_wrap_stays_on_the_edge_column() {
        // esctest test_BS_ReverseWrapStartingInDoWrapPosition: after filling the
        // last column the cursor is in a pending wrap; under reverse-wrap the next
        // backspace cancels the pending wrap in place (lands on the last column)
        // rather than stepping left, so the following glyph overwrites the last one.
        let mut vt = Vt::new(80, 25);
        vt.feed_str("\x1b[?7h\x1b[?45h");
        vt.feed_str("\x1b[1;79H"); // col 78, row 0
        vt.feed_str("ab"); // 'a'@78, 'b'@79, then pending wrap
        vt.feed_str("\x08"); // BS: cancels the pending wrap, stays on col 79
        assert_eq!((vt.cursor().col, vt.cursor().row), (79, 0));
        vt.feed_str("X"); // overwrites 'b'
        let row = row_cells(&vt, 0, 80);
        assert_eq!(&row[78..80], "aX", "'a' kept, 'X' overwrote 'b'");
    }

    #[test]
    fn reverse_wrap_is_suppressed_right_after_a_line_feed() {
        // esctest test_BS_InitialReverseWraparound: a line feed lands the cursor at
        // a fresh line start; the very next BS must NOT reverse-wrap to the row
        // above. Any other operation in between re-enables the wrap.
        let mut vt = Vt::new(80, 25);
        vt.feed_str("\x1b[?7h\x1b[?45h"); // DECAWM + reverse-wrap
        vt.feed_str("\x1b[1;1H\x1bE"); // home, then NEL -> row 1, col 0
        vt.feed_str("\x08"); // BS right after the line feed: no wrap
        assert_eq!(
            (vt.cursor().col, vt.cursor().row),
            (0, 1),
            "BS immediately after NEL stays put"
        );
        // A BS after a plain LF is likewise suppressed.
        vt.feed_str("\x1b[1;1H\n\x08");
        assert_eq!(
            (vt.cursor().col, vt.cursor().row),
            (0, 1),
            "BS after LF stays put"
        );
        // But once the cursor is repositioned by anything else, BS wraps again.
        vt.feed_str("\x1b[3;1H\x08"); // CUP to row 2 col 0, then BS
        assert_eq!(
            (vt.cursor().col, vt.cursor().row),
            (79, 1),
            "BS after a CUP wraps to the row above"
        );
    }

    #[test]
    fn decrqm_reports_reverse_wrap_mode_state() {
        use crate::ModeReport::{Reset, Set};
        let mut vt = Vt::new(80, 25);
        assert_eq!(vt.dec_mode_state(45), Reset, "?45 starts reset");
        vt.feed_str("\x1b[?45h");
        assert_eq!(vt.dec_mode_state(45), Set, "?45 reported set");
        vt.feed_str("\x1b[?45l");
        assert_eq!(vt.dec_mode_state(45), Reset, "?45 reported reset");
    }

    #[test]
    fn rect_checksum_matches_xterm_decrqcra() {
        let mut vt = Vt::new(10, 2);
        vt.feed_str("A B"); // row 0: 'A', ' ', 'B', then blanks

        // Single cells: character code, negated 16-bit (xterm's default form,
        // which esctest un-negates as 0x10000 - reply).
        assert_eq!(vt.rect_checksum(0, 0, 0, 0), 0xFFBF); // 'A' = 65
        assert_eq!(vt.rect_checksum(0, 2, 0, 2), 0xFFBE); // 'B' = 66
                                                          // A lone blank still counts (it is the rect's first cell): 0x20 = 32.
        assert_eq!(vt.rect_checksum(0, 1, 0, 1), 0xFFE0);

        // Multi-cell: interior/trailing plain spaces are trimmed, but a leading
        // one (the first cell) is not.
        assert_eq!(vt.rect_checksum(0, 0, 0, 2), 0xFF7D); // 'A' + 'B' = 131
        assert_eq!(vt.rect_checksum(0, 0, 0, 1), 0xFFBF); // 'A' + trimmed space
        assert_eq!(vt.rect_checksum(0, 3, 0, 4), 0xFFE0); // space + trimmed space

        // Out-of-range coordinates clamp to the screen rather than panicking.
        assert_eq!(vt.rect_checksum(0, 0, 99, 0), vt.rect_checksum(0, 0, 1, 0));
    }

    #[test]
    fn rect_checksum_adds_attribute_bits_like_xterm() {
        let mut vt = Vt::new(10, 1);
        vt.feed_str("\x1b[1mA\x1b[0m"); // bold 'A' = 65 + 0x80 = 193
        assert_eq!(vt.rect_checksum(0, 0, 0, 0), 0xFF3F);

        let mut vt = Vt::new(10, 1);
        vt.feed_str("\x1b[7mA\x1b[0m"); // inverse 'A' = 65 + 0x20 = 97
        assert_eq!(vt.rect_checksum(0, 0, 0, 0), 0xFF9F);

        let mut vt = Vt::new(10, 1);
        vt.feed_str("\x1b[4mA\x1b[0m"); // underline 'A' = 65 + 0x10 = 81
        assert_eq!(vt.rect_checksum(0, 0, 0, 0), 0xFFAF);
    }

    #[test]
    fn feed_str_returns_changed_lines() {
        let mut vt = Vt::builder().size(2, 2).build();

        vt.feed_str("");

        let (lines, scrollback) = {
            let changes = vt.feed_str("aa\r\nbb\r\ncc");

            let scrollback = changes
                .scrollback
                .map(|line| line.text())
                .collect::<Vec<_>>();

            (changes.lines, scrollback)
        };

        assert_eq!(lines, vec![0, 1]);
        assert_eq!(scrollback, Vec::<String>::new());
    }

    #[test]
    fn feed_str_updates_accessors() {
        let mut vt = Vt::builder().size(2, 2).build();

        vt.feed_str("");
        vt.feed_str("aa\r\nbb\r\ncc");

        assert_eq!(vt.size(), (2, 2));
        // "cc" fills the 2-col bottom line; the cursor parks on the last column
        // with the wrap deferred rather than sitting past the edge.
        assert_eq!(vt.cursor(), (1, 1));

        assert_eq!(
            vt.text(),
            vec!["aa".to_owned(), "bb".to_owned(), "cc".to_owned()]
        );

        assert_eq!(vt.view().count(), 2);
        assert!(vt.lines().count() >= 2);
        assert_eq!(vt.line(0).chars().take(2).collect::<String>(), "bb");
    }

    #[test]
    fn unicode_placeholder_consumes_trailing_diacritics() {
        use crate::Color;
        // kitty Unicode placeholder: U+10EEEE rides in a cell with the image id in
        // the foreground colour, followed by zero-width combining diacritics (here
        // U+0305) that encode the image row/col. The diacritics must not occupy
        // cells or shove later text — the renderer resolves row/col from position.
        let mut vt = Vt::builder().size(5, 1).build();
        vt.feed_str("\x1b[38;2;0;0;5m\u{10eeee}\u{0305}\u{0305}X");

        let cells = vt.line(0).cells();
        assert_eq!(cells[0].char(), '\u{10eeee}', "placeholder occupies cell 0");
        assert_eq!(
            cells[0].pen().foreground(),
            Some(Color::rgb(0, 0, 5)),
            "the image id rides in the foreground colour"
        );
        assert_eq!(
            cells[1].char(),
            'X',
            "the diacritics were consumed, so X lands in cell 1"
        );
        // Cursor advanced past the placeholder and X only, not the diacritics.
        assert_eq!(vt.cursor(), (2, 0));
    }

    #[test]
    fn dump_re_transmits_stored_images_for_reattach() {
        // A reattaching (or replaying-from-checkpoint) client is fed dump(); the
        // stored images must come back so placeholder cells — which carry only the
        // id — can resolve to pixels again.
        let mut vt = Vt::new(10, 3);
        vt.feed_str("\x1b_Gi=7,a=t,f=24,s=2,v=1;/wAAAP8A\x1b\\");
        assert_eq!(vt.graphics_image_count(), 1);
        let pixels = vt.graphics_image(7).expect("stored").pixels.clone();

        let dump = vt.dump();
        let mut fresh = Vt::new(10, 3);
        fresh.feed_str(&dump);

        let restored = fresh
            .graphics_image(7)
            .expect("image re-transmitted in dump");
        assert_eq!((restored.width, restored.height), (2, 1));
        assert_eq!(restored.pixels, pixels);
    }

    #[test]
    fn placeholder_displayed_image_survives_eviction_pressure() {
        use base64::Engine;

        // Each 750-px RGB image stores 3000 RGBA bytes; the cfg(test) store cap is
        // 8 KiB, so a third image forces eviction.
        let px = base64::engine::general_purpose::STANDARD.encode(vec![0u8; 750 * 3]);
        let transmit = |id: u32| format!("\u{1b}_Gi={id},a=t,f=24,s=750,v=1;{px}\u{1b}\\");

        let mut vt = Vt::new(20, 5);
        // Image 1 is the OLDEST, but it's on screen via Unicode-placeholder cells
        // (fg rgb(0,0,1) packs image id 1) rather than a placement.
        vt.feed_str(&transmit(1));
        vt.feed_str("\u{1b}[38;2;0;0;1m\u{10eeee}\u{10eeee}");
        // Image 2 is newer and not displayed.
        vt.feed_str(&transmit(2));
        // Storing image 3 exceeds the cap. The LRU image is #1, but it's visibly on
        // screen, so #2 (unreferenced) must be evicted instead.
        vt.feed_str(&transmit(3));

        assert!(
            vt.graphics_image(1).is_some(),
            "an on-screen placeholder image must not be evicted"
        );
        assert!(
            vt.graphics_image(2).is_none(),
            "the unreferenced image is evicted instead"
        );
        assert!(vt.graphics_image(3).is_some());
    }

    #[test]
    fn transmit_free_dump_plus_reconstructed_transmits_restores_everything() {
        // The recording dedup stores image bytes out-of-band: feeding the
        // transmit-free dump preceded by transmits reconstructed from those bytes
        // must restore images and placements exactly like the full dump.
        let mut vt = Vt::new(10, 3);
        vt.feed_str("\x1b_Gi=5,a=T,f=24,s=2,v=1,c=2,r=1;/wAAAP8A\x1b\\");

        let mut imgs: Vec<_> = vt
            .graphics_images()
            .map(|i| (i.id, i.width, i.height, i.pixels.clone()))
            .collect();
        imgs.sort_by_key(|i| i.0);
        let mut dump = String::new();
        for (id, w, h, px) in &imgs {
            dump.push_str(&crate::encode_transmit(*id, *w, *h, px));
        }
        dump.push_str(&vt.dump_with_scrollback_without_images());

        let mut fresh = Vt::new(10, 3);
        fresh.feed_str(&dump);
        assert_eq!(fresh.graphics_image_count(), 1);
        assert_eq!(fresh.graphics_placement_count(), 1);
        let p = fresh
            .graphics_placements()
            .next()
            .expect("placement restored");
        assert_eq!((p.col, p.cols, p.rows), (0, 2, 1));
    }

    #[test]
    fn dump_re_places_direct_placements_for_reattach() {
        // A directly-placed image (a=T) must survive a reattach: both its bytes
        // and its on-screen placement come back from the dump.
        let mut vt = Vt::new(10, 3);
        vt.feed_str("\x1b_Gi=5,a=T,f=24,s=2,v=1,c=2,r=1;/wAAAP8A\x1b\\");
        assert_eq!(vt.graphics_placement_count(), 1);

        let dump = vt.dump();
        let mut fresh = Vt::new(10, 3);
        fresh.feed_str(&dump);

        assert_eq!(fresh.graphics_image_count(), 1);
        assert_eq!(fresh.graphics_placement_count(), 1);
        let p = fresh
            .graphics_placements()
            .next()
            .expect("placement restored");
        assert_eq!((p.col, p.cols, p.rows), (0, 2, 1));
    }

    #[test]
    fn placeholder_run_ends_at_a_cursor_move() {
        // The placeholder run only spans the characters that immediately follow the
        // placeholder cell; a cursor move ends it, so a later combining mark is
        // handled normally (the emulator's usual leading-mark behaviour) rather
        // than being silently eaten as if it were one of the placeholder's.
        let mut vt = Vt::builder().size(8, 1).build();
        // Placeholder at col 0, move to column 5 (CHA, 1-based), then a combining
        // mark and 'X'.
        vt.feed_str("\x1b[38;2;0;0;5m\u{10eeee}\x1b[5G\u{0305}X");

        let cells = vt.line(0).cells();
        assert_eq!(cells[0].char(), '\u{10eeee}');
        // The mark was NOT consumed: it sits in its own cell at column 4 and 'X'
        // follows at column 5.
        assert_eq!(cells[4].char(), '\u{0305}');
        assert_eq!(cells[5].char(), 'X');
    }

    #[test]
    fn adjacent_placeholders_each_occupy_a_cell() {
        // A multi-cell image is a grid of placeholder cells, each (optionally)
        // followed by its own diacritics. Consecutive placeholders must each get
        // their own cell — the placeholder is never consumed as a trailing mark.
        let mut vt = Vt::builder().size(5, 1).build();
        vt.feed_str("\u{10eeee}\u{0305}\u{10eeee}\u{0305}\u{10eeee}");

        let cells = vt.line(0).cells();
        assert_eq!(cells[0].char(), '\u{10eeee}');
        assert_eq!(cells[1].char(), '\u{10eeee}');
        assert_eq!(cells[2].char(), '\u{10eeee}');
        assert_eq!(
            vt.cursor(),
            (3, 0),
            "three placeholders, diacritics consumed"
        );
    }

    #[test]
    fn feed_str_returns_trimmed_scrollback() {
        let mut vt = Vt::builder().size(2, 2).scrollback_limit(0).build();

        vt.feed_str("");

        let scrollback = {
            let changes = vt.feed_str("aa\r\nbb\r\ncc");

            changes
                .scrollback
                .map(|line| line.text())
                .collect::<Vec<_>>()
        };

        assert_eq!(scrollback, vec!["aa".to_owned()]);
        assert_eq!(vt.text(), vec!["bb".to_owned(), "cc".to_owned()]);
    }

    #[test]
    fn view_at_scrolls_into_scrollback() {
        let mut vt = Vt::builder().size(2, 2).build();
        vt.feed_str("aa\r\nbb\r\ncc"); // scrollback ["aa"], view ["bb","cc"]
        assert_eq!(vt.scrollback_len(), 1);
        // offset 0 is the live viewport.
        let live: Vec<String> = vt.view_at(0).map(|l| l.text()).collect();
        assert_eq!(live, vec!["bb".to_string(), "cc".to_string()]);
        // offset 1 brings the scrollback line onto the top row.
        let up: Vec<String> = vt.view_at(1).map(|l| l.text()).collect();
        assert_eq!(up, vec!["aa".to_string(), "bb".to_string()]);
        // Offsets past the retained history clamp to the oldest window.
        let clamped: Vec<String> = vt.view_at(99).map(|l| l.text()).collect();
        assert_eq!(clamped, up);
    }

    #[test]
    fn lines_scrolled_off_is_monotonic_across_trimming() {
        // 2 rows + a 3-line scrollback cap: at most 5 lines are ever retained.
        let mut vt = Vt::builder().size(2, 2).scrollback_limit(3).build();
        vt.feed_str("a\r\nb\r\nc\r\nd\r\ne\r\nf\r\ng"); // 7 lines a..g
        assert_eq!(vt.scrollback_len(), 3, "scrollback is capped");
        // a..e (5 lines) have scrolled off the top, though only 3 are retained.
        assert_eq!(vt.lines_scrolled_off(), 5);
        // Further output keeps the count climbing even though the length is pinned.
        vt.feed_str("\r\nh\r\ni");
        assert_eq!(vt.scrollback_len(), 3);
        assert_eq!(vt.lines_scrolled_off(), 7);
    }

    #[test]
    fn resize_returns_changed_lines() {
        let mut vt = Vt::new(4, 2);

        vt.feed_str("");

        let (lines, scrollback_count) = {
            let changes = vt.resize(4, 3);

            (changes.lines, changes.scrollback.count())
        };

        assert_eq!(lines, vec![0, 1, 2]);
        assert_eq!(scrollback_count, 0);
    }

    #[test]
    fn resize_updates_size_accessor() {
        let mut vt = Vt::new(4, 2);

        vt.resize(4, 3);

        assert_eq!(vt.size(), (4, 3));
    }

    #[test]
    fn dump_initial() {
        let vt1 = Vt::new(10, 4);
        let mut vt2 = Vt::new(10, 4);

        vt2.feed_str(&vt1.dump());

        assert_vts_eq(&vt1, &vt2);
    }

    #[test]
    fn dump_modified() {
        let mut vt1 = Vt::new(10, 4);
        let mut vt2 = Vt::new(10, 4);

        vt1.feed_str("hello\n\rworld 日\u{9b}5W\u{9b}7`\u{1b}[W\u{9b}?6h");
        vt1.feed_str("\u{9b}2;4r\u{9b}1;5H\x1b[1;31;41m\u{9b}?25l\u{9b}4h");
        vt1.feed_str("\u{9b}?7l\u{9b}20h\u{9b}\u{3a}\x1b(0\x1b)0\u{0e}");

        vt2.feed_str(&vt1.dump());

        assert_vts_eq(&vt1, &vt2);
    }

    #[test]
    fn exposes_input_relevant_modes() {
        use super::MouseProtocol;
        let mut vt = Vt::new(20, 5);
        assert_eq!(vt.mouse_protocol(), MouseProtocol::Off);
        assert!(!vt.mouse_sgr());
        assert!(!vt.focus_report());
        assert!(!vt.bracketed_paste());

        // An app turns on X11 mouse, SGR coords, focus, and bracketed paste.
        vt.feed_str("\x1b[?1000h\x1b[?1006h\x1b[?1004h\x1b[?2004h");
        assert_eq!(vt.mouse_protocol(), MouseProtocol::Press);
        assert!(vt.mouse_sgr());
        assert!(vt.focus_report());
        assert!(vt.bracketed_paste());

        // Any-motion (1003) wins over X11 (1000); button-event (1002) sits between.
        vt.feed_str("\x1b[?1003h");
        assert_eq!(vt.mouse_protocol(), MouseProtocol::AnyMotion);
        vt.feed_str("\x1b[?1003l");
        assert_eq!(vt.mouse_protocol(), MouseProtocol::Press);
        vt.feed_str("\x1b[?1002h");
        assert_eq!(vt.mouse_protocol(), MouseProtocol::ButtonDrag);

        // Everything off again.
        vt.feed_str("\x1b[?1000l\x1b[?1002l\x1b[?1006l\x1b[?1004l\x1b[?2004l");
        assert_eq!(vt.mouse_protocol(), MouseProtocol::Off);
        assert!(!vt.mouse_sgr());
        assert!(!vt.focus_report());
        assert!(!vt.bracketed_paste());
    }

    #[test]
    fn hyperlinks_attach_to_printed_cells() {
        let mut vt = Vt::new(40, 5);
        vt.feed_str("\x1b]8;;https://example.com/a\x1b\\LINK\x1b]8;;\x1b\\!");

        let line = vt.line(0);
        let id = line[0].pen().link_id().expect("first cell is linked");
        assert_eq!(vt.hyperlink(id), Some("https://example.com/a"));
        assert_eq!(vt.line(0)[3].pen().link_id(), Some(id), "whole run linked");
        assert_eq!(vt.line(0)[4].pen().link_id(), None, "closed before '!'");

        // BEL-terminated form, params (id=…) accepted and ignored; the same
        // URI interns to the same id.
        vt.feed_str("\x1b]8;id=x;https://example.com/a\x07S");
        assert_eq!(vt.line(0)[5].pen().link_id(), Some(id));
        // A different URI gets its own id.
        vt.feed_str("\x1b]8;;https://other.example\x1b\\O");
        let other = vt.line(0)[6].pen().link_id().expect("linked");
        assert_ne!(other, id);
        assert_eq!(vt.hyperlink(other), Some("https://other.example"));

        // Unknown ids resolve to nothing.
        assert_eq!(vt.hyperlink(9999), None);
    }

    #[test]
    fn hyperlinks_survive_a_dump_roundtrip() {
        let mut vt = Vt::new(40, 5);
        // A styled link, plain text, then a second link left open (the live
        // pen carries it at dump time).
        vt.feed_str("\x1b[31m\x1b]8;;https://a.example\x1b\\AA\x1b]8;;\x1b\\bb");
        vt.feed_str("\x1b]8;;https://b.example\x1b\\CC");

        let mut vt2 = Vt::new(40, 5);
        vt2.feed_str(&vt.dump());

        let resolve = |vt: &Vt, i: usize| {
            vt.line(0)[i]
                .pen()
                .link_id()
                .and_then(|id| vt.hyperlink(id).map(str::to_owned))
        };
        for i in 0..6 {
            assert_eq!(resolve(&vt, i), resolve(&vt2, i), "cell {i}");
            assert_eq!(
                vt.line(0)[i].pen().foreground(),
                vt2.line(0)[i].pen().foreground(),
                "cell {i} style intact"
            );
        }
        // The live pen still carries the open link: new output stays linked.
        vt2.feed_str("X");
        assert_eq!(resolve(&vt2, 6).as_deref(), Some("https://b.example"));
    }

    #[test]
    fn erased_cells_do_not_carry_the_open_hyperlink() {
        let mut vt = Vt::new(20, 5);
        vt.feed_str("\x1b]8;;https://a.example\x1b\\AB");
        // Erase to end of line while the link is open: blanks are not links
        // (clicking empty space should never open anything).
        vt.feed_str("\x1b[K");
        assert_eq!(vt.line(0)[5].pen().link_id(), None);
        // But printed cells keep linking.
        vt.feed_str("C");
        assert!(vt.line(0)[2].pen().link_id().is_some());
    }

    #[test]
    fn osc133_prompt_marks_record_absolute_rows() {
        let mut vt = Vt::new(20, 5);
        vt.feed_str("\x1b]133;A\x07$ one\r\n");
        vt.feed_str("out\r\nout\r\n");
        // ST-terminated, and kitty-style params after the letter are accepted.
        vt.feed_str("\x1b]133;A;k=s\x1b\\$ two\r\n");
        assert_eq!(vt.prompt_rows().collect::<Vec<_>>(), [0, 3]);

        // A duplicate mark on the same row records once.
        vt.feed_str("\x1b]133;A\x07\x1b]133;A\x07$ dup");
        assert_eq!(vt.prompt_rows().collect::<Vec<_>>(), [0, 3, 4]);

        // Rows are absolute: scrolling doesn't move recorded marks.
        vt.feed_str("\r\nx\r\nx\r\nx\r\n");
        assert_eq!(vt.prompt_rows().collect::<Vec<_>>(), [0, 3, 4]);
    }

    #[test]
    fn osc133_marks_beyond_retained_history_are_pruned() {
        let mut vt = Vt::builder().size(10, 3).scrollback_limit(2).build();
        vt.feed_str("\x1b]133;A\x07$ old\r\n");
        // Push the mark far past the 2-line scrollback.
        for _ in 0..10 {
            vt.feed_str("x\r\n");
        }
        assert_eq!(vt.prompt_rows().count(), 0, "unreachable mark pruned");
    }

    #[test]
    fn osc133_tracks_command_exit() {
        let mut vt = Vt::new(20, 5);
        assert!(!vt.command_running());
        assert_eq!(vt.last_exit_status(), None);

        vt.feed_str("\x1b]133;A\x07$ \x1b]133;B\x07make\x1b]133;C\x07");
        assert!(vt.command_running(), "output began (C): command runs");
        vt.feed_str("building...\r\n\x1b]133;D;2\x07");
        assert!(!vt.command_running());
        assert_eq!(vt.last_exit_status(), Some(2));

        // A bare D (no code) still ends the command.
        vt.feed_str("\x1b]133;C\x07\x1b]133;D\x07");
        assert!(!vt.command_running());
        assert_eq!(vt.last_exit_status(), None);
    }

    #[test]
    fn prompt_marks_survive_a_dump_roundtrip() {
        let mut vt = Vt::builder().size(20, 4).scrollback_limit(50).build();
        vt.feed_str("\x1b]133;A\x07$ one\r\nout\r\nout\r\n");
        vt.feed_str("\x1b]133;A\x07$ two\r\nmore\r\n");

        let mut vt2 = Vt::builder().size(20, 4).scrollback_limit(50).build();
        vt2.feed_str(&vt.dump_with_scrollback());

        // The two terminals may number history differently (trimming), so
        // compare each mark's depth below the live viewport top.
        let depths = |vt: &Vt| {
            vt.prompt_rows()
                .map(|r| vt.lines_scrolled_off() as i64 - r as i64)
                .collect::<Vec<_>>()
        };
        assert_eq!(depths(&vt), depths(&vt2));

        // Reset clears marks and command state.
        vt.feed_str("\x1bc");
        assert_eq!(vt.prompt_rows().count(), 0);
    }

    #[test]
    fn osc52_queues_clipboard_writes() {
        use super::ClipboardSelection::{Clipboard, Primary};
        let mut vt = Vt::new(20, 5);
        assert!(vt.take_clipboard_writes().is_empty());

        // BEL- and ST-terminated; base64 payloads decode to the text.
        vt.feed_str("\x1b]52;c;aGVsbG8=\x07"); // "hello"
        vt.feed_str("\x1b]52;p;cHJpbWFyeQ==\x1b\\"); // "primary"
                                                     // An empty selection defaults to the clipboard.
        vt.feed_str("\x1b]52;;Zm9v\x07"); // "foo"
        let writes = vt.take_clipboard_writes();
        assert_eq!(
            writes,
            [
                (Clipboard, "hello".to_string()),
                (Primary, "primary".to_string()),
                (Clipboard, "foo".to_string()),
            ]
        );
        assert!(vt.take_clipboard_writes().is_empty(), "drained once");

        // Ignored: the query form (clipboard *read* is a privacy hole),
        // invalid base64, and payloads that aren't UTF-8 text.
        vt.feed_str("\x1b]52;c;?\x07");
        vt.feed_str("\x1b]52;c;!not-base64!\x07");
        vt.feed_str("\x1b]52;c;/w==\x07"); // a lone 0xFF byte
        assert!(vt.take_clipboard_writes().is_empty());
    }

    #[test]
    fn osc52_targets_both_selections_when_asked() {
        use super::ClipboardSelection::{Clipboard, Primary};
        let mut vt = Vt::new(20, 5);
        // Pc lists targets; "pc" writes primary and clipboard alike.
        vt.feed_str("\x1b]52;pc;Ym90aA==\x07"); // "both"
        assert_eq!(
            vt.take_clipboard_writes(),
            [
                (Primary, "both".to_string()),
                (Clipboard, "both".to_string())
            ]
        );
    }

    #[test]
    fn reset_clears_hyperlinks() {
        let mut vt = Vt::new(20, 5);
        vt.feed_str("\x1b]8;;https://a.example\x1b\\L");
        let id = vt.line(0)[0].pen().link_id().unwrap();
        vt.feed_str("\x1bc");
        assert_eq!(vt.hyperlink(id), None, "table cleared by RIS");
        vt.feed_str("P");
        assert_eq!(vt.line(0)[0].pen().link_id(), None, "pen link cleared");
    }

    #[test]
    fn tracks_synchronized_output_mode() {
        let mut vt = Vt::new(20, 5);
        assert!(!vt.synchronized_output());

        vt.feed_str("\x1b[?2026h");
        assert!(vt.synchronized_output());
        // Content keeps applying while sync is on; holding presentation is the
        // frontend's business, not the emulator's.
        vt.feed_str("hello");
        assert_eq!(vt.text()[0].trim_end(), "hello");
        vt.feed_str("\x1b[?2026l");
        assert!(!vt.synchronized_output());

        // Transient frame marker: a state dump must never re-emit it (a
        // reattaching frontend would start with presentation held), and a full
        // reset clears it.
        vt.feed_str("\x1b[?2026h");
        assert!(!vt.dump().contains("2026"), "dump leaked mode 2026");
        vt.feed_str("\x1bc");
        assert!(!vt.synchronized_output());
    }

    #[test]
    fn osc_4_sets_the_indexed_palette_and_osc_104_resets_it() {
        let mut vt = Vt::new(20, 5);
        assert_eq!(vt.palette_color(1), None, "the palette starts untouched");

        // A single index, then several pairs in one OSC (xterm allows both).
        vt.feed_str("\x1b]4;1;rgb:f0f0/0000/0000\x07");
        assert_eq!(vt.palette_color(1), Some([0xf0, 0x00, 0x00]));
        vt.feed_str("\x1b]4;2;#00ff00;255;rgb:aaaa/bbbb/cccc\x1b\\");
        assert_eq!(vt.palette_color(2), Some([0x00, 0xff, 0x00]));
        assert_eq!(vt.palette_color(255), Some([0xaa, 0xbb, 0xcc]));

        // A query pair sets nothing (the host answers it), and neither an
        // unparseable spec nor an out-of-range index disturbs its neighbours.
        vt.feed_str("\x1b]4;1;?\x07\x1b]4;1;bogus\x07\x1b]4;999;#fff\x07");
        assert_eq!(vt.palette_color(1), Some([0xf0, 0x00, 0x00]));

        // A mixed set/query OSC still applies its set.
        vt.feed_str("\x1b]4;1;?;3;#0000ff\x07");
        assert_eq!(vt.palette_color(3), Some([0x00, 0x00, 0xff]));

        // OSC 104 with indices resets those; with none, the whole palette.
        vt.feed_str("\x1b]104;1;2\x07");
        assert_eq!(vt.palette_color(1), None);
        assert_eq!(vt.palette_color(2), None);
        assert_eq!(vt.palette_color(3), Some([0x00, 0x00, 0xff]));
        vt.feed_str("\x1b]104\x07");
        assert_eq!(vt.palette_color(3), None);
        assert_eq!(vt.palette_color(255), None);
        assert!(vt.palette_is_default());
    }

    #[test]
    fn a_palette_override_survives_a_dump_and_a_hard_reset_clears_it() {
        let mut vt = Vt::new(20, 5);
        vt.feed_str("\x1b]4;1;#ff0000;200;#00ff00\x07");

        let mut reloaded = Vt::new(20, 5);
        reloaded.feed_str(&vt.dump());
        assert_eq!(reloaded.palette_color(1), Some([0xff, 0x00, 0x00]));
        assert_eq!(reloaded.palette_color(200), Some([0x00, 0xff, 0x00]));

        // RIS takes the palette back to the theme's.
        vt.feed_str("\x1bc");
        assert!(vt.palette_is_default());
    }

    #[test]
    fn osc_dynamic_colors_set_and_reset() {
        let mut vt = Vt::new(20, 5);
        assert_eq!(vt.dynamic_foreground(), None);
        assert_eq!(vt.dynamic_background(), None);
        assert_eq!(vt.dynamic_cursor_color(), None);

        // rgb:/ # spec forms, BEL- or ST-terminated. rgb: components scale
        // by digit count (X11); # forms are left-justified, high byte kept.
        vt.feed_str("\x1b]10;rgb:ff/80/00\x07");
        assert_eq!(vt.dynamic_foreground(), Some([0xff, 0x80, 0x00]));
        vt.feed_str("\x1b]11;#102030\x1b\\");
        assert_eq!(vt.dynamic_background(), Some([0x10, 0x20, 0x30]));
        vt.feed_str("\x1b]12;rgb:aaaa/bbbb/cccc\x07");
        assert_eq!(vt.dynamic_cursor_color(), Some([0xaa, 0xbb, 0xcc]));
        vt.feed_str("\x1b]10;rgb:a/b/c\x07"); // 1-digit: v * 255/15
        assert_eq!(vt.dynamic_foreground(), Some([0xaa, 0xbb, 0xcc]));
        vt.feed_str("\x1b]10;#abc\x07"); // left-justified nibbles
        assert_eq!(vt.dynamic_foreground(), Some([0xa0, 0xb0, 0xc0]));

        // The query form is the host's to answer, not a set; unparseable
        // specs are ignored.
        vt.feed_str("\x1b]11;?\x07");
        vt.feed_str("\x1b]11;bogus\x07");
        assert_eq!(vt.dynamic_background(), Some([0x10, 0x20, 0x30]));

        // OSC 110/111/112 reset each override to the theme default.
        vt.feed_str("\x1b]110\x07\x1b]111\x07\x1b]112\x07");
        assert_eq!(vt.dynamic_foreground(), None);
        assert_eq!(vt.dynamic_background(), None);
        assert_eq!(vt.dynamic_cursor_color(), None);
    }

    #[test]
    fn dynamic_colors_survive_a_dump_roundtrip() {
        let mut vt = Vt::new(20, 5);
        vt.feed_str("\x1b]10;rgb:d8/db/e0\x07\x1b]11;#101012\x07\x1b]12;#ff0000\x07");

        let mut vt2 = Vt::new(20, 5);
        vt2.feed_str(&vt.dump());
        assert_eq!(vt2.dynamic_foreground(), Some([0xd8, 0xdb, 0xe0]));
        assert_eq!(vt2.dynamic_background(), Some([0x10, 0x10, 0x12]));
        assert_eq!(vt2.dynamic_cursor_color(), Some([0xff, 0x00, 0x00]));

        // RIS drops the overrides.
        vt.feed_str("\x1bc");
        assert_eq!(vt.dynamic_foreground(), None);
        assert_eq!(vt.dynamic_background(), None);
        assert_eq!(vt.dynamic_cursor_color(), None);
    }

    #[test]
    fn osc_9_4_tracks_task_progress() {
        use super::Progress::*;
        let mut vt = Vt::new(20, 5);
        assert_eq!(vt.progress(), None);

        vt.feed_str("\x1b]9;4;1;42\x07");
        assert_eq!(vt.progress(), Some(Normal(42)));
        vt.feed_str("\x1b]9;4;2;90\x1b\\");
        assert_eq!(vt.progress(), Some(Error(90)));
        vt.feed_str("\x1b]9;4;3\x07");
        assert_eq!(vt.progress(), Some(Indeterminate));
        vt.feed_str("\x1b]9;4;4;10\x07");
        assert_eq!(vt.progress(), Some(Paused(10)));
        // Out-of-range percentages clamp.
        vt.feed_str("\x1b]9;4;1;150\x07");
        assert_eq!(vt.progress(), Some(Normal(100)));
        // st=0 removes the progress indication.
        vt.feed_str("\x1b]9;4;0\x07");
        assert_eq!(vt.progress(), None);

        // Unknown states and the other OSC 9 sub-commands are ignored.
        vt.feed_str("\x1b]9;4;7;10\x07");
        vt.feed_str("\x1b]9;notification text\x07");
        assert_eq!(vt.progress(), None);
    }

    #[test]
    fn progress_survives_a_dump_roundtrip_and_ris_clears_it() {
        let mut vt = Vt::new(20, 5);
        vt.feed_str("\x1b]9;4;1;42\x07");

        let mut vt2 = Vt::new(20, 5);
        vt2.feed_str(&vt.dump());
        assert_eq!(vt2.progress(), Some(super::Progress::Normal(42)));

        vt.feed_str("\x1bc");
        assert_eq!(vt.progress(), None);
    }

    #[test]
    fn tracks_color_scheme_report_mode_2031() {
        use crate::ModeReport::{Reset, Set};
        let mut vt = Vt::new(20, 5);
        assert_eq!(vt.dec_mode_state(2031), Reset);
        vt.feed_str("\x1b[?2031h");
        assert_eq!(vt.dec_mode_state(2031), Set);
        assert!(
            vt.dump().contains("\x1b[?2031h"),
            "the subscription must survive a resync"
        );
        vt.feed_str("\x1b[?2031l");
        assert_eq!(vt.dec_mode_state(2031), Reset);
    }

    #[test]
    fn dump_restores_non_display_modes() {
        let mut vt = Vt::new(20, 5);
        // An app enables mouse tracking, SGR coordinates, focus, and paste.
        vt.feed_str("\x1b[?1000h\x1b[?1006h\x1b[?1004h\x1b[?2004h");
        let dump = vt.dump();
        for seq in ["\x1b[?1000h", "\x1b[?1006h", "\x1b[?1004h", "\x1b[?2004h"] {
            assert!(dump.contains(seq), "dump missing {seq:?}: {dump:?}");
        }

        // Disabling a mode drops it from the dump.
        vt.feed_str("\x1b[?1000l");
        assert!(!vt.dump().contains("\x1b[?1000h"));

        // And the dump round-trips into an equivalent terminal.
        let mut vt2 = Vt::new(20, 5);
        vt2.feed_str(&vt.dump());
        assert_vts_eq(&vt, &vt2);
    }

    #[test]
    fn dump_restores_window_title() {
        let mut vt = Vt::new(20, 5);
        vt.feed_str("\x1b]2;my session\x07");
        let dump = vt.dump();
        assert!(
            dump.contains("\x1b]2;my session\x07"),
            "dump missing title: {dump:?}"
        );

        // The dump round-trips into an equivalent terminal.
        let mut vt2 = Vt::new(20, 5);
        vt2.feed_str(&vt.dump());
        assert_vts_eq(&vt, &vt2);

        // A later title replaces the earlier one.
        vt.feed_str("\x1b]0;renamed\x07");
        let dump = vt.dump();
        assert!(dump.contains("\x1b]2;renamed\x07"));
        assert!(!dump.contains("my session"));
    }

    #[test]
    fn dump_with_file() {
        if let Ok((w, h, input, step)) = setup_dump_with_file() {
            let mut vt1 = Vt::new(w, h);

            let mut s = 0;

            for c in input.chars().take(1_000_000) {
                vt1.feed(c);

                if s == 0 {
                    let d = vt1.dump();
                    let mut vt2 = Vt::new(w, h);

                    vt2.feed_str(&d);

                    assert_vts_eq(&vt1, &vt2);
                }

                s = (s + 1) % step;
            }
        }
    }

    fn gen_input(max_len: usize) -> impl Strategy<Value = Vec<char>> {
        prop::collection::vec(
            prop_oneof![gen_ctl_seq(), gen_esc_seq(), gen_csi_seq(), gen_text()],
            1..=max_len,
        )
        .prop_map(flatten)
    }

    fn gen_ctl_seq() -> impl Strategy<Value = Vec<char>> {
        let ctl_chars = vec![0x00..0x18, 0x19..0x1a, 0x1c..0x20];

        prop::sample::select(flatten(ctl_chars)).prop_map(|v: u8| vec![v as char])
    }

    fn gen_esc_seq() -> impl Strategy<Value = Vec<char>> {
        (
            prop::collection::vec(gen_esc_intermediate(), 0..=2),
            gen_esc_finalizer(),
        )
            .prop_map(|(inters, fin)| flatten(vec![vec!['\x1b'], inters, vec![fin]]))
    }

    fn gen_csi_seq() -> impl Strategy<Value = Vec<char>> {
        prop_oneof![
            gen_csi_sgr_seq(),
            gen_csi_sm_seq(),
            gen_csi_rm_seq(),
            gen_csi_any_seq(),
        ]
    }

    fn gen_text() -> impl Strategy<Value = Vec<char>> {
        prop::collection::vec(gen_char(), 1..10)
    }

    fn gen_esc_intermediate() -> impl Strategy<Value = char> {
        (0x20..0x30u8).prop_map(|v| v as char)
    }

    fn gen_esc_finalizer() -> impl Strategy<Value = char> {
        let finalizers = vec![
            0x30..0x50,
            0x51..0x58,
            0x59..0x5a,
            0x5a..0x5b,
            0x5c..0x5d,
            0x60..0x7f,
        ];

        prop::sample::select(flatten(finalizers)).prop_map(|v: u8| v as char)
    }

    fn gen_csi_sgr_seq() -> impl Strategy<Value = Vec<char>> {
        gen_csi_params().prop_map(|params| flatten(vec![vec!['\x1b', '['], params, vec!['m']]))
    }

    fn gen_csi_sm_seq() -> impl Strategy<Value = Vec<char>> {
        (gen_csi_intermediate(), gen_csi_sm_rm_param()).prop_map(|(inters, params)| {
            flatten(vec![vec!['\x1b', '['], inters, params, vec!['h']])
        })
    }

    fn gen_csi_rm_seq() -> impl Strategy<Value = Vec<char>> {
        (gen_csi_intermediate(), gen_csi_sm_rm_param()).prop_map(|(inters, params)| {
            flatten(vec![vec!['\x1b', '['], inters, params, vec!['l']])
        })
    }

    fn gen_csi_any_seq() -> impl Strategy<Value = Vec<char>> {
        (gen_csi_params(), gen_csi_finalizer())
            .prop_map(|(params, fin)| flatten(vec![vec!['\x1b', '['], params, vec![fin]]))
    }

    fn gen_csi_intermediate() -> impl Strategy<Value = Vec<char>> {
        prop::collection::vec(prop::sample::select(vec!['?', '!']), 0..=1)
    }

    fn gen_csi_params() -> impl Strategy<Value = Vec<char>> {
        prop::collection::vec(
            prop_oneof![
                gen_csi_param(),
                gen_csi_param(),
                prop::sample::select(vec![';'])
            ],
            0..=5,
        )
    }

    fn gen_csi_param() -> impl Strategy<Value = char> {
        (0x30..0x3au8).prop_map(|v| v as char)
    }

    fn gen_csi_sm_rm_param() -> impl Strategy<Value = Vec<char>> {
        let modes = vec![1, 4, 6, 7, 20, 25, 47, 1047, 1048, 1049];

        prop_oneof![
            prop::sample::select(modes).prop_map(|n| n.to_string().chars().collect()),
            prop::collection::vec(gen_csi_param(), 1..=4)
        ]
    }

    fn gen_csi_finalizer() -> impl Strategy<Value = char> {
        (0x40..0x7fu8).prop_map(|v| v as char)
    }

    fn gen_char() -> impl Strategy<Value = char> {
        prop_oneof![
            gen_ascii_char(),
            gen_ascii_char(),
            gen_ascii_char(),
            gen_ascii_char(),
            gen_ascii_char(),
            (0x80..=0xd7ffu32).prop_map(|v| char::from_u32(v).unwrap()),
            (0xf900..=0xffffu32).prop_map(|v| char::from_u32(v).unwrap())
        ]
    }

    fn gen_ascii_char() -> impl Strategy<Value = char> {
        (0x20..=0x7fu8).prop_map(|v| v as char)
    }

    fn flatten<T, I: IntoIterator<Item = T>>(seqs: Vec<I>) -> Vec<T> {
        seqs.into_iter().flatten().collect()
    }

    proptest! {
        #[test]
        fn prop_sanity_checks_infinite_scrollback(input in gen_input(25)) {
            let mut vt = Vt::builder().size(10, 5).build();

            vt.feed_str(&(input.into_iter().collect::<String>()));

            vt.terminal.verify();
            assert!(vt.lines().count() >= vt.size().1);
        }

        #[test]
        fn prop_sanity_checks_no_scrollback(input in gen_input(25)) {
            let mut vt = Vt::builder().size(10, 5).scrollback_limit(0).build();

            vt.feed_str(&(input.into_iter().collect::<String>()));

            vt.terminal.verify();
            assert!(vt.lines().count() == vt.size().1);
        }

        #[test]
        fn prop_sanity_checks_fixed_scrollback(input in gen_input(25)) {
            let scrollback_limit = 3;
            let mut vt = Vt::builder().size(10, 5).scrollback_limit(scrollback_limit).build();

            vt.feed_str(&(input.into_iter().collect::<String>()));
            let (_, rows) = vt.size();

            vt.terminal.verify();
            assert!(vt.lines().count() >= rows && vt.lines().count() <= rows + scrollback_limit);
        }

        #[test]
        fn prop_resizing(new_cols in 2..15usize, new_rows in 2..8usize, input1 in gen_input(25), input2 in gen_input(25)) {
            let mut vt = Vt::builder().size(10, 5).build();

            vt.feed_str(&(input1.into_iter().collect::<String>()));
            vt.resize(new_cols, new_rows);
            vt.feed_str(&(input2.into_iter().collect::<String>()));

            vt.terminal.verify();
            assert!(vt.lines().count() >= vt.size().1);
        }

        #[test]
        fn prop_dump(input in gen_input(25)) {
            let mut vt1 = Vt::new(10, 5);
            let mut vt2 = Vt::new(10, 5);

            vt1.feed_str(&(input.into_iter().collect::<String>()));
            vt2.feed_str(&vt1.dump());

            assert_vts_eq(&vt1, &vt2);
        }
    }

    fn setup_dump_with_file() -> Result<(usize, usize, String, usize), env::VarError> {
        let path = env::var("P")?;
        let input = fs::read_to_string(path).unwrap();
        let w: usize = env::var("W").unwrap().parse::<usize>().unwrap();
        let h: usize = env::var("H").unwrap().parse::<usize>().unwrap();

        let step: usize = env::var("S")
            .unwrap_or("1".to_owned())
            .parse::<usize>()
            .unwrap();

        Ok((w, h, input, step))
    }

    fn assert_vts_eq(vt1: &Vt, vt2: &Vt) {
        vt1.parser.assert_eq(&vt2.parser);
        vt1.terminal.assert_eq(&vt2.terminal);
    }

    // kitty graphics protocol — receiving and storing images. The base64 strings
    // below are tiny hand-checkable images:
    //   "/wAAAP8A" = FF 00 00  00 FF 00     (a red then a green RGB pixel)
    //   "AAAA"     = 00 00 00                (one black RGB pixel)
    const RED_GREEN_RGB_B64: &str = "/wAAAP8A";

    #[test]
    fn kitty_graphics_transmit_stores_image_and_acks() {
        let mut vt = Vt::new(20, 5);

        vt.feed_str(&format!(
            "\x1b_Gi=5,a=t,f=24,s=2,v=1;{RED_GREEN_RGB_B64}\x1b\\"
        ));

        let image = vt.graphics_image(5).expect("image stored under id 5");
        assert_eq!((image.width, image.height), (2, 1));
        // RGB expands to RGBA with an opaque alpha.
        assert_eq!(image.pixels, vec![255, 0, 0, 255, 0, 255, 0, 255]);
        assert_eq!(vt.take_graphics_responses(), b"\x1b_Gi=5;OK\x1b\\");
        // Responses are drained, not repeated.
        assert!(vt.take_graphics_responses().is_empty());
    }

    #[test]
    fn kitty_graphics_query_acks_support_without_storing() {
        let mut vt = Vt::new(20, 5);

        // The standard support probe: a 1×1 image with a=q.
        vt.feed_str("\x1b_Gi=31,a=q,f=24,s=1,v=1;AAAA\x1b\\");

        assert_eq!(vt.take_graphics_responses(), b"\x1b_Gi=31;OK\x1b\\");
        assert_eq!(vt.graphics_image_count(), 0);
    }

    #[test]
    fn kitty_graphics_assigns_id_for_image_number_and_echoes_it() {
        let mut vt = Vt::new(20, 5);

        vt.feed_str(&format!(
            "\x1b_GI=7,a=t,f=24,s=2,v=1;{RED_GREEN_RGB_B64}\x1b\\"
        ));

        // The terminal allocates an id and echoes both it and the image number.
        let response = String::from_utf8(vt.take_graphics_responses()).unwrap();
        assert_eq!(response, "\x1b_Gi=1,I=7;OK\x1b\\");
        assert_eq!(vt.graphics_image_count(), 1);
        assert!(vt.graphics_image(1).is_some());
    }

    #[test]
    fn kitty_graphics_refuses_non_direct_transmission() {
        let mut vt = Vt::new(20, 5);

        // t=f (file) is refused: reading the session host's filesystem is a
        // security hazard and meaningless to a remote display.
        vt.feed_str("\x1b_Gi=8,a=t,t=f,f=24,s=1,v=1;AAAA\x1b\\");

        let response = String::from_utf8(vt.take_graphics_responses()).unwrap();
        assert_eq!(
            response,
            "\x1b_Gi=8;ENOTSUPPORTED:only direct transmission is supported\x1b\\"
        );
        assert_eq!(vt.graphics_image_count(), 0);
    }

    #[test]
    fn kitty_graphics_rejects_i_and_i_number_together() {
        let mut vt = Vt::new(20, 5);

        vt.feed_str("\x1b_Gi=1,I=2,a=t,f=24,s=1,v=1;AAAA\x1b\\");

        let response = String::from_utf8(vt.take_graphics_responses()).unwrap();
        assert!(response.contains("EINVAL"), "got {response:?}");
        assert_eq!(vt.graphics_image_count(), 0);
    }

    #[test]
    fn kitty_graphics_rejects_pixel_data_size_mismatch() {
        let mut vt = Vt::new(20, 5);

        // Claims 2×2 RGBA (16 bytes) but sends 3 bytes.
        vt.feed_str("\x1b_Gi=4,a=t,f=32,s=2,v=2;AAAA\x1b\\");

        let response = String::from_utf8(vt.take_graphics_responses()).unwrap();
        assert!(response.contains("EINVAL"), "got {response:?}");
        assert_eq!(vt.graphics_image_count(), 0);
    }

    #[test]
    fn kitty_graphics_reassembles_chunked_transfer() {
        let mut vt = Vt::new(20, 5);

        // The red+green RGB image split across two chunks (each a multiple of 4
        // base64 bytes). The control data rides only the first chunk.
        vt.feed_str("\x1b_Gi=9,a=t,f=24,s=2,v=1,m=1;/wAA\x1b\\");
        // No image and no response until the final chunk arrives.
        assert!(vt.graphics_image(9).is_none());
        assert!(vt.take_graphics_responses().is_empty());

        vt.feed_str("\x1b_Gm=0;AP8A\x1b\\");

        let image = vt.graphics_image(9).expect("assembled image");
        assert_eq!(image.pixels, vec![255, 0, 0, 255, 0, 255, 0, 255]);
        assert_eq!(vt.take_graphics_responses(), b"\x1b_Gi=9;OK\x1b\\");
    }

    #[test]
    fn kitty_graphics_quiet_suppresses_ok_but_not_errors() {
        let mut vt = Vt::new(20, 5);

        // q=1 suppresses the OK acknowledgement.
        vt.feed_str(&format!(
            "\x1b_Gi=5,a=t,q=1,f=24,s=2,v=1;{RED_GREEN_RGB_B64}\x1b\\"
        ));
        assert!(vt.take_graphics_responses().is_empty());
        assert!(vt.graphics_image(5).is_some());

        // q=2 suppresses errors.
        vt.feed_str("\x1b_Gi=6,a=t,q=2,t=f,f=24,s=1,v=1;AAAA\x1b\\");
        assert!(vt.take_graphics_responses().is_empty());

        // q=1 still lets an error through.
        vt.feed_str("\x1b_Gi=6,a=t,q=1,t=f,f=24,s=1,v=1;AAAA\x1b\\");
        let response = String::from_utf8(vt.take_graphics_responses()).unwrap();
        assert!(response.contains("ENOTSUPPORTED"), "got {response:?}");
    }

    #[test]
    fn kitty_graphics_decodes_png() {
        let mut vt = Vt::new(20, 5);

        // A 1×1 opaque-red PNG, base64-encoded (generated with the `png` crate).
        let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR42mP4z8AAAAMBAQD3A0FDAAAAAElFTkSuQmCC";
        vt.feed_str(&format!("\x1b_Gi=3,a=t,f=100;{png_b64}\x1b\\"));

        let image = vt.graphics_image(3).expect("PNG stored");
        assert_eq!((image.width, image.height), (1, 1));
        assert_eq!(image.pixels, vec![255, 0, 0, 255]);
        assert_eq!(vt.take_graphics_responses(), b"\x1b_Gi=3;OK\x1b\\");
    }

    #[test]
    fn kitty_graphics_hard_reset_clears_images() {
        let mut vt = Vt::new(20, 5);

        vt.feed_str(&format!(
            "\x1b_Gi=5,a=t,f=24,s=2,v=1;{RED_GREEN_RGB_B64}\x1b\\"
        ));
        assert_eq!(vt.graphics_image_count(), 1);
        let _ = vt.take_graphics_responses();

        vt.feed_str("\x1bc"); // RIS

        assert_eq!(vt.graphics_image_count(), 0);
    }

    #[test]
    fn kitty_graphics_rejects_oversized_raw_dimensions() {
        let mut vt = Vt::new(20, 5);

        // 100000×100000 RGBA = 40 GB; reject on the declared size before trying to
        // match the (tiny) payload, never allocating.
        vt.feed_str("\x1b_Gi=2,a=t,f=32,s=100000,v=100000;AAAA\x1b\\");

        let response = String::from_utf8(vt.take_graphics_responses()).unwrap();
        assert!(response.contains("EINVAL"), "got {response:?}");
        assert_eq!(vt.graphics_image_count(), 0);
    }

    #[test]
    fn kitty_graphics_rejects_png_declaring_huge_dimensions() {
        let mut vt = Vt::new(20, 5);

        // A 66-byte PNG whose IHDR declares 1000×16_000_000 RGBA (1.6e10 px). The
        // decoder's byte limit covers row buffers, not the output buffer, so we
        // must reject on the declared size or the host allocates ~64 GB and dies.
        let png_b64 = "iVBORw0KGgoAAAANSUhEUgAAA+gA9CQACAYAAADGj1CpAAAACUlEQVR4nGMAAAABAAFe/335AAAAAElFTkSuQmCC";
        vt.feed_str(&format!("\x1b_Gi=2,a=t,f=100;{png_b64}\x1b\\"));

        let response = String::from_utf8(vt.take_graphics_responses()).unwrap();
        assert!(response.contains("EINVAL"), "got {response:?}");
        assert_eq!(vt.graphics_image_count(), 0);
    }

    #[test]
    fn kitty_graphics_store_is_bounded_and_evicts_the_oldest_when_full() {
        let mut vt = Vt::new(20, 5);

        // Transmit more distinct 1×1 images than the store's count budget (1024).
        // The store stays bounded by evicting the least-recently-used image rather
        // than growing, or — as it once did — refusing the newest transfer.
        for id in 1..=1025u32 {
            vt.feed_str(&format!("\x1b_Gi={id},a=t,q=2,f=24,s=1,v=1;AAAA\x1b\\"));
        }
        let _ = vt.take_graphics_responses();

        assert_eq!(vt.graphics_image_count(), 1024, "the store stays bounded");
        assert!(
            vt.graphics_image(1).is_none(),
            "the oldest image was evicted"
        );
        assert!(
            vt.graphics_image(1025).is_some(),
            "the newest image is kept"
        );
    }

    #[test]
    fn kitty_graphics_unkeyed_command_emits_no_response() {
        let mut vt = Vt::new(20, 5);

        // No i and no I: a success stores under a reallocated id but is silent
        // (nothing to match a reply against)…
        vt.feed_str("\x1b_Ga=t,f=24,s=2,v=1;/wAAAP8A\x1b\\");
        assert!(vt.take_graphics_responses().is_empty());
        assert_eq!(vt.graphics_image_count(), 1);

        // …and an unkeyed *error* is silent too (no unmatchable i=0 reply).
        vt.feed_str("\x1b_Ga=t,f=24,s=2,v=2;AAAA\x1b\\");
        assert!(vt.take_graphics_responses().is_empty());

        // i=0 explicitly means "unset", so it behaves like an unkeyed command.
        vt.feed_str("\x1b_Gi=0,a=t,f=24,s=2,v=1;/wAAAP8A\x1b\\");
        assert!(vt.take_graphics_responses().is_empty());
    }

    #[test]
    fn kitty_graphics_quiet_on_a_later_chunk_suppresses_the_response() {
        let mut vt = Vt::new(20, 5);

        // No q on the first chunk; q=1 on the final chunk must still suppress OK.
        vt.feed_str("\x1b_Gi=9,a=t,f=24,s=2,v=1,m=1;/wAA\x1b\\");
        vt.feed_str("\x1b_Gm=0,q=1;AP8A\x1b\\");

        assert!(vt.take_graphics_responses().is_empty());
        assert!(vt.graphics_image(9).is_some());
    }

    #[test]
    fn kitty_graphics_transmit_and_display_creates_a_placement_at_the_cursor() {
        let mut vt = Vt::new(20, 5);

        vt.feed_str("\x1b[3;5H"); // cursor to row 3, col 5 (1-based) => (2, 4)
        vt.feed_str("\x1b_Gi=7,a=T,f=24,s=2,v=1,c=4,r=2,z=1;/wAAAP8A\x1b\\");

        assert!(vt.graphics_image(7).is_some());
        assert_eq!(vt.graphics_placement_count(), 1);
        let p = vt.graphics_placements().next().unwrap().clone();
        assert_eq!((p.image_id, p.placement_id), (7, 0));
        assert_eq!((p.row, p.col), (2, 4));
        assert_eq!((p.cols, p.rows, p.z), (4, 2, 1));
        assert_eq!(vt.take_graphics_responses(), b"\x1b_Gi=7;OK\x1b\\");
    }

    #[test]
    fn kitty_graphics_transmit_only_does_not_create_a_placement() {
        let mut vt = Vt::new(20, 5);

        vt.feed_str("\x1b_Gi=7,a=t,f=24,s=2,v=1;/wAAAP8A\x1b\\");

        assert!(vt.graphics_image(7).is_some());
        assert_eq!(vt.graphics_placement_count(), 0);
    }

    #[test]
    fn kitty_graphics_put_displays_a_stored_image_and_errors_when_missing() {
        let mut vt = Vt::new(20, 5);
        vt.feed_str("\x1b_Gi=7,a=t,f=24,s=2,v=1;/wAAAP8A\x1b\\");
        let _ = vt.take_graphics_responses();

        // a=p displays the already-stored image at placement 3.
        vt.feed_str("\x1b_Gi=7,a=p,p=3;\x1b\\");
        assert_eq!(vt.graphics_placement_count(), 1);
        assert_eq!(vt.graphics_placements().next().unwrap().placement_id, 3);
        assert_eq!(vt.take_graphics_responses(), b"\x1b_Gi=7,p=3;OK\x1b\\");

        // a=p for an unknown image is an ENOENT.
        vt.feed_str("\x1b_Gi=99,a=p;\x1b\\");
        let r = String::from_utf8(vt.take_graphics_responses()).unwrap();
        assert!(r.contains("ENOENT"), "got {r:?}");
        assert_eq!(vt.graphics_placement_count(), 1);
    }

    #[test]
    fn kitty_graphics_placement_anchor_is_absolute_so_it_scrolls_with_content() {
        let mut vt = Vt::new(10, 3);

        // Push lines into scrollback so lines_scrolled_off > 0.
        vt.feed_str("a\r\nb\r\nc\r\nd\r\ne\r\n");
        let expected_row = vt.lines_scrolled_off() + vt.cursor().row;

        vt.feed_str("\x1b_Gi=1,a=T,f=24,s=2,v=1;/wAAAP8A\x1b\\");

        assert_eq!(vt.graphics_placements().next().unwrap().row, expected_row);
    }

    #[test]
    fn kitty_graphics_replacing_a_placement_does_not_duplicate_it() {
        let mut vt = Vt::new(20, 5);
        vt.feed_str("\x1b_Gi=7,a=t,f=24,s=2,v=1;/wAAAP8A\x1b\\");
        let _ = vt.take_graphics_responses();

        // The same (image, placement) pair, placed twice at different anchors,
        // updates in place rather than stacking.
        vt.feed_str("\x1b[1;1H\x1b_Gi=7,a=p,p=2;\x1b\\");
        vt.feed_str("\x1b[2;3H\x1b_Gi=7,a=p,p=2;\x1b\\");

        assert_eq!(vt.graphics_placement_count(), 1);
        let p = vt.graphics_placements().next().unwrap().clone();
        assert_eq!((p.row, p.col), (1, 2)); // the second anchor won
    }

    #[test]
    fn kitty_graphics_delete_by_id_removes_placements_and_uppercase_frees() {
        let mut vt = Vt::new(20, 5);
        vt.feed_str("\x1b_Gi=7,a=T,f=24,s=2,v=1;/wAAAP8A\x1b\\");
        vt.feed_str("\x1b_Gi=8,a=T,f=24,s=2,v=1;/wAAAP8A\x1b\\");
        let _ = vt.take_graphics_responses();
        assert_eq!(vt.graphics_placement_count(), 2);

        // d=i (lowercase) drops image 7's placements but keeps its pixel data.
        vt.feed_str("\x1b_Ga=d,d=i,i=7;\x1b\\");
        assert_eq!(vt.graphics_placement_count(), 1);
        assert!(
            vt.graphics_image(7).is_some(),
            "lowercase d=i keeps the image"
        );

        // d=I (uppercase) drops image 8's placement and frees the image.
        vt.feed_str("\x1b_Ga=d,d=I,i=8;\x1b\\");
        assert_eq!(vt.graphics_placement_count(), 0);
        assert!(
            vt.graphics_image(8).is_none(),
            "uppercase d=I frees the image"
        );
        assert!(vt.graphics_image(7).is_some());

        // Deletes carry no acknowledgement.
        assert!(vt.take_graphics_responses().is_empty());
    }

    #[test]
    fn kitty_graphics_delete_all_clears_placements_and_uppercase_frees_images() {
        let mut vt = Vt::new(20, 5);
        vt.feed_str("\x1b_Gi=7,a=T,f=24,s=2,v=1;/wAAAP8A\x1b\\");
        vt.feed_str("\x1b_Gi=8,a=T,f=24,s=2,v=1;/wAAAP8A\x1b\\");
        let _ = vt.take_graphics_responses();

        // d=a (lowercase) clears placements but keeps the images.
        vt.feed_str("\x1b_Ga=d,d=a;\x1b\\");
        assert_eq!(vt.graphics_placement_count(), 0);
        assert_eq!(vt.graphics_image_count(), 2);

        // d=A frees the images too.
        vt.feed_str("\x1b_Ga=d,d=A;\x1b\\");
        assert_eq!(vt.graphics_image_count(), 0);
    }

    #[test]
    fn kitty_graphics_hard_reset_clears_placements() {
        let mut vt = Vt::new(20, 5);
        vt.feed_str("\x1b_Gi=7,a=T,f=24,s=2,v=1;/wAAAP8A\x1b\\");
        assert_eq!(vt.graphics_placement_count(), 1);
        let _ = vt.take_graphics_responses();

        vt.feed_str("\x1bc"); // RIS

        assert_eq!(vt.graphics_placement_count(), 0);
    }

    #[test]
    fn kitty_graphics_placement_scoped_delete_keeps_image_until_last_placement() {
        let mut vt = Vt::new(20, 5);
        vt.feed_str("\x1b_Gi=7,a=t,f=24,s=2,v=1;/wAAAP8A\x1b\\");
        vt.feed_str("\x1b_Gi=7,a=p,p=1;\x1b\\");
        vt.feed_str("\x1b_Gi=7,a=p,p=2;\x1b\\");
        let _ = vt.take_graphics_responses();
        assert_eq!(vt.graphics_placement_count(), 2);

        // d=I scoped to placement 1 frees only that placement; image 7 stays
        // because placement (7,2) still references it.
        vt.feed_str("\x1b_Ga=d,d=I,i=7,p=1;\x1b\\");
        assert_eq!(vt.graphics_placement_count(), 1);
        assert!(
            vt.graphics_image(7).is_some(),
            "the image must survive while a placement remains"
        );

        // Deleting the last placement (uppercase) now frees the image.
        vt.feed_str("\x1b_Ga=d,d=I,i=7,p=2;\x1b\\");
        assert_eq!(vt.graphics_placement_count(), 0);
        assert!(vt.graphics_image(7).is_none());
    }

    #[test]
    fn kitty_graphics_placements_are_per_screen() {
        let mut vt = Vt::new(20, 5);
        vt.feed_str("\x1b_Gi=1,a=T,f=24,s=2,v=1;/wAAAP8A\x1b\\");
        let _ = vt.take_graphics_responses();
        assert_eq!(vt.graphics_placement_count(), 1);

        // Entering the alternate screen parks the primary placement (not visible).
        vt.feed_str("\x1b[?1049h");
        assert_eq!(vt.graphics_placement_count(), 0);
        // The image itself is global, so it is still stored.
        assert!(vt.graphics_image(1).is_some());

        // A placement made on the alternate screen is independent.
        vt.feed_str("\x1b_Gi=2,a=T,f=24,s=2,v=1;/wAAAP8A\x1b\\");
        let _ = vt.take_graphics_responses();
        assert_eq!(vt.graphics_placement_count(), 1);
        assert_eq!(vt.graphics_placements().next().unwrap().image_id, 2);

        // Leaving it restores the primary placement.
        vt.feed_str("\x1b[?1049l");
        assert_eq!(vt.graphics_placement_count(), 1);
        assert_eq!(vt.graphics_placements().next().unwrap().image_id, 1);
    }

    #[test]
    fn kitty_graphics_records_the_cursor_move_policy() {
        let mut vt = Vt::new(20, 5);

        vt.feed_str("\x1b_Gi=1,a=T,f=24,s=2,v=1,C=1;/wAAAP8A\x1b\\");
        assert!(vt.graphics_placements().next().unwrap().no_cursor_move);

        vt.feed_str("\x1b[2;1H\x1b_Gi=2,a=T,f=24,s=2,v=1;/wAAAP8A\x1b\\");
        let p2 = vt.graphics_placements().find(|p| p.image_id == 2).unwrap();
        assert!(!p2.no_cursor_move);
    }

    #[test]
    fn kitty_graphics_chunked_placement_anchors_to_the_first_chunk() {
        let mut vt = Vt::new(20, 5);
        vt.feed_str("\x1b[2;1H"); // cursor to row 2 (1-based) => row index 1
        vt.feed_str("\x1b_Gi=9,a=T,f=24,s=2,v=1,m=1;/wAA\x1b\\");
        vt.feed_str("\x1b[5;1H"); // move the cursor between chunks => row index 4
        vt.feed_str("\x1b_Gm=0;AP8A\x1b\\");

        // The placement anchors where the transfer began (row 1), not the final
        // chunk's cursor (row 4).
        assert_eq!(vt.graphics_placements().next().unwrap().row, 1);
    }
}
