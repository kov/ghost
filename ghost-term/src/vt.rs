use crate::graphics::{Image, Placement};
use crate::line::Line;
use crate::parser::{self, DecMode, Parser};
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

    /// The URI behind an interned OSC 8 hyperlink id — the id a linked cell
    /// carries in `Pen::link_id`.
    pub fn hyperlink(&self, id: u16) -> Option<&str> {
        self.terminal.hyperlink(id)
    }

    /// Drain the decoded OSC 52 clipboard writes queued while feeding output.
    pub fn take_clipboard_writes(&mut self) -> Vec<(ClipboardSelection, String)> {
        self.terminal.take_clipboard_writes()
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

    pub fn cursor(&self) -> Cursor {
        self.terminal.cursor()
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

    /// The DECRPM state of a DEC private mode by raw number: `Some(true)` set,
    /// `Some(false)` reset, `None` not a mode with queryable state here.
    pub fn dec_mode_state(&self, mode: u16) -> Option<bool> {
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
        assert_eq!(vt.cursor(), (2, 1));

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
