//! The host's authoritative screen state.
//!
//! A [`Screen`] wraps a headless [`Vt`] fed with every byte the child writes to
//! its PTY, so the host always knows what the terminal *looks like* even when no
//! client is attached. On attach the host asks for a [`resync`](Screen::resync)
//! sequence that repaints a freshly-cleared terminal to that state, after which
//! it streams live bytes verbatim.
//!
//! PTY output is raw bytes, and a read can split a multibyte UTF-8 character
//! across chunk boundaries; [`Screen::feed`] buffers any incomplete trailing
//! sequence so the authoritative state is never corrupted (invalid bytes become
//! U+FFFD, matching `String::from_utf8_lossy`).

use crate::record::{Item, Recording};
use ghost_term::Vt;

/// Default bound on retained scrollback lines. Keeps host memory bounded; the
/// viewport itself is always reconstructable regardless of this limit.
pub const DEFAULT_SCROLLBACK: usize = 1000;

/// The rows the drawn cursor implicitly redrew by *moving* on the last
/// [`feed`](Screen::feed) — as opposed to the cell content [`feed`](Screen::feed)
/// returns. `feed`'s row hint covers only printed glyphs; a bare CUP/CUF, a
/// scroll of the cursor, or a DECTCEM show/hide repositions or repaints the block
/// without touching a line, so a caller that draws a cursor folds these rows into
/// its damage. Both rows are `None` when the cursor did not change; rows are
/// clamped to the current viewport. Whether the cursor is *actually* on screen
/// (e.g. the caller scrolled back into history) is the caller's call — see
/// [`repaint`](CursorDamage::repaint).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CursorDamage {
    /// Row the cursor left, when it was drawn (visible) there.
    pub left: Option<usize>,
    /// Row the cursor entered, when it is drawn (visible) there.
    pub entered: Option<usize>,
    /// Whether the change warrants a repaint at all — true when the block was or
    /// is visible, false for a move that stayed hidden the whole time.
    pub repaint: bool,
}

pub struct Screen {
    vt: Vt,
    cols: u16,
    rows: u16,
    /// Incomplete trailing UTF-8 bytes carried over from the previous feed.
    pending: Vec<u8>,
    /// Reused scratch for the viewport rows the last [`feed`](Screen::feed)
    /// changed, so reporting damage costs no per-feed allocation.
    dirty_rows: Vec<usize>,
    /// The drawn cursor after the previous [`feed`](Screen::feed), for diffing a
    /// bare cursor move (which dirties no content row) into [`cursor_damage`].
    prev_cursor: ghost_term::terminal::Cursor,
    /// The cursor-move damage from the last [`feed`](Screen::feed).
    cursor_damage: CursorDamage,
}

impl Screen {
    pub fn new(cols: u16, rows: u16, scrollback_limit: usize) -> Self {
        let vt = Vt::builder()
            .size(cols as usize, rows as usize)
            .scrollback_limit(scrollback_limit)
            .build();
        let prev_cursor = vt.cursor();
        Screen {
            vt,
            cols,
            rows,
            pending: Vec::new(),
            dirty_rows: Vec::new(),
            prev_cursor,
            cursor_damage: CursorDamage::default(),
        }
    }

    /// Reconstruct a screen by replaying a recording from its most recent
    /// checkpoint (or from the start if it has none). The checkpoint's dump
    /// restores the emulator state; the items after it bring it current. This
    /// is how the archival recording is turned back into live emulator state
    /// (for export, or to bound the recording at a checkpoint).
    pub fn from_recording(rec: &Recording, scrollback_limit: usize) -> Self {
        let (mut screen, rest) = match rec.latest_checkpoint() {
            Some(i) => {
                let Item::Checkpoint {
                    cols, rows, dump, ..
                } = &rec.items[i]
                else {
                    unreachable!("latest_checkpoint indexes a checkpoint");
                };
                let mut screen = Screen::new(*cols, *rows, scrollback_limit);
                screen.feed(dump);
                (screen, &rec.items[i + 1..])
            }
            None => (
                Screen::new(rec.header.cols, rec.header.rows, scrollback_limit),
                &rec.items[..],
            ),
        };
        for item in rest {
            match item {
                Item::Output { data, .. } => {
                    screen.feed(data);
                }
                Item::Resize { cols, rows, .. } => screen.resize(*cols, *rows),
                Item::Checkpoint { .. } => {}
            }
        }
        screen
    }

    /// Feed raw PTY bytes, decoding as much valid UTF-8 as possible and holding
    /// back only a genuinely incomplete trailing sequence for next time.
    ///
    /// Returns the viewport rows whose **cell content** (character, width, or
    /// pen) this feed changed — sorted and deduplicated. The claim is a
    /// **superset**: every row whose cells changed is listed, so a row that is
    /// *not* listed had no cell change and row-banded redraw may leave it in the
    /// surface it already holds. That one-directional guarantee is pinned by
    /// `ghost-term`'s `damage_audit`; over-reporting is fine and deliberate —
    /// scrolling, full clears, alt-screen switches and reflow conservatively
    /// claim the whole viewport.
    ///
    /// **Cell content only.** A change that recolors or repositions without
    /// writing a cell is *not* here — it is handled above this hint: the drawn
    /// cursor moving, hiding, or reshaping (its own diffed channel,
    /// [`cursor_damage`](Self::cursor_damage)), OSC 4/104 palette and OSC
    /// 10/11/12 dynamic colors (they recolor every drawn cell — the view layer
    /// snapshots them and forces a full repaint), and kitty-graphics placement
    /// changes. An empty slice therefore means no viewport **cell** changed, not
    /// that nothing observable did — a query reply, a palette edit, a bare
    /// cursor move, or bytes held back as an incomplete UTF-8 tail all return
    /// empty.
    pub fn feed(&mut self, bytes: &[u8]) -> &[usize] {
        self.dirty_rows.clear();
        self.pending.extend_from_slice(bytes);
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(s) => {
                    if !s.is_empty() {
                        let lines = self.vt.feed_str(s).lines;
                        self.dirty_rows.extend(lines);
                    }
                    self.pending.clear();
                    break;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    if valid > 0 {
                        // SAFETY: `valid_up_to` guarantees this prefix is UTF-8.
                        let s = unsafe { std::str::from_utf8_unchecked(&self.pending[..valid]) };
                        let lines = self.vt.feed_str(s).lines;
                        self.dirty_rows.extend(lines);
                    }
                    match e.error_len() {
                        // Incomplete tail: keep it, wait for the rest.
                        None => {
                            self.pending.drain(..valid);
                            break;
                        }
                        // Invalid byte(s): emit a replacement char and skip them.
                        Some(bad) => {
                            let lines = self.vt.feed_str("\u{fffd}").lines;
                            self.dirty_rows.extend(lines);
                            self.pending.drain(..valid + bad);
                        }
                    }
                }
            }
        }
        self.dirty_rows.sort_unstable();
        self.dirty_rows.dedup();
        // The emulator can resize itself from within the feed (DECCOLM 80↔132),
        // an unusual bottom-up change: keep our own size in step so `dimensions()`
        // (and the CSI 18t reply built from it) and the cursor-damage clamp below
        // reflect it. The change is deterministic from these bytes, so a recording
        // replays it without a separate resize item.
        let (vc, vr) = self.vt.size();
        self.cols = vc as u16;
        self.rows = vr as u16;
        // The drawn cursor is part of the frame but moving it writes no cell, so
        // this feed's content hint above misses it. Diff the cursor (position +
        // visibility + shape) against the last feed and report the row it left
        // and the row it entered as their own damage — clamped to the viewport,
        // since a shrink can strand a stale row past the bottom.
        let now = self.vt.cursor();
        let prev = std::mem::replace(&mut self.prev_cursor, now);
        self.cursor_damage = if now == prev {
            CursorDamage::default()
        } else {
            let max_row = self.rows.saturating_sub(1) as usize;
            CursorDamage {
                left: prev.visible.then(|| prev.row.min(max_row)),
                entered: now.visible.then(|| now.row.min(max_row)),
                repaint: prev.visible || now.visible,
            }
        };
        &self.dirty_rows
    }

    /// The [`CursorDamage`] from the most recent [`feed`](Screen::feed): the rows
    /// a bare cursor move repainted, which the content hint `feed` returns does
    /// not cover. A caller that draws the cursor folds these into its damage
    /// (gating on whether the cursor is on screen — see [`CursorDamage`]).
    pub fn cursor_damage(&self) -> CursorDamage {
        self.cursor_damage
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        self.vt.resize(cols as usize, rows as usize);
        // Re-sync the cursor-damage baseline to the reflowed cursor. A resize forces a
        // full repaint (the drawn cursor lands at its post-reflow position), so the next
        // feed's [`cursor_damage`] must diff against THAT, not the pre-resize cursor —
        // otherwise a resize that moved the cursor leaves a stale baseline, and a later
        // bare cursor move that happens to match it reports no damage and never repaints.
        self.prev_cursor = self.vt.cursor();
    }

    /// Current terminal size as `(cols, rows)`.
    pub fn dimensions(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// What a program on this session's tty may change about the terminal (see
    /// [`ghost_term::policy`]). The session host and an attached frontend each run
    /// one of these over the same bytes: give them the same policy.
    pub fn set_policy(&mut self, policy: ghost_term::TerminalPolicy) {
        self.vt.set_policy(policy);
    }

    /// Borrow the underlying emulator core for read-only consumers such as a
    /// renderer's layout pass (`ghost_render::layout_frame`), which needs the
    /// live styled grid and cursor rather than a text/CPR snapshot.
    pub fn vt(&self) -> &Vt {
        &self.vt
    }

    /// The active kitty keyboard flags — the host answers a `CSI ? u` query with
    /// these while detached (mirroring how it answers DA/cursor/size queries).
    pub fn kitty_keyboard_flags(&self) -> u8 {
        self.vt.kitty_keyboard_flags()
    }

    /// Drain the kitty-graphics acknowledgement bytes queued while feeding child
    /// output (image transfer / query OK and error replies). Unlike DA/cursor
    /// queries these are stateful, so they come from the emulator rather than the
    /// query scanner; the host writes them to the child while detached and
    /// discards them while attached (the outer terminal answers via the pipe).
    pub fn take_graphics_responses(&mut self) -> Vec<u8> {
        self.vt.take_graphics_responses()
    }

    /// Drain the decoded OSC 52 clipboard writes queued while feeding child
    /// output. The attached frontend applies them to the system clipboard; the
    /// detached host drains and discards (nobody's clipboard to write).
    pub fn take_clipboard_writes(&mut self) -> Vec<(ghost_term::ClipboardSelection, String)> {
        self.vt.take_clipboard_writes()
    }

    /// Drain the window ops (XTWINOPS) a program asked for that the emulator can't
    /// carry out — iconify, maximize, full-screen. An attached frontend performs
    /// them; a detached host has no window and drains them into the void.
    pub fn take_window_ops(&mut self) -> Vec<ghost_term::XtwinopsOp> {
        self.vt.take_window_ops()
    }

    /// The colors OSC 10/11/12 (and the `?996` color-scheme) queries answer
    /// with: an app-set dynamic override (OSC 10/11/12 set forms) wins over
    /// `theme`, the frontend's configured scheme.
    pub fn effective_colors(&self, theme: crate::query::ThemeColors) -> crate::query::ThemeColors {
        crate::query::ThemeColors {
            fg: self.vt.dynamic_foreground().unwrap_or(theme.fg),
            bg: self.vt.dynamic_background().unwrap_or(theme.bg),
            cursor: self.vt.dynamic_cursor_color().unwrap_or(theme.cursor),
            // The app's OSC 4 overrides are answered per index (`ReplyCtx::palette`),
            // not folded in here — this is the scheme's own palette.
            ansi: theme.ansi,
        }
    }

    /// Cursor position as 1-based `(col, row)` — the form a cursor-position
    /// report (CPR) carries. `avt` tracks the cursor 0-based.
    pub fn cursor(&self) -> (u16, u16) {
        let c = self.vt.cursor();
        (
            (c.col as u16).saturating_add(1),
            (c.row as u16).saturating_add(1),
        )
    }

    /// Cursor position for a cursor-position report (CPR) as 1-based
    /// `(col, row)`, reported relative to the active margins in origin mode —
    /// what a program querying its cursor reads back. Distinct from
    /// [`Self::cursor`], which is the absolute position for rendering/IME.
    pub fn cursor_report(&self) -> (u16, u16) {
        let (col, row) = self.vt.cursor_report();
        (
            (col as u16).saturating_add(1),
            (row as u16).saturating_add(1),
        )
    }

    /// The emulator state as an extended dump (scrollback + viewport + modes).
    /// Feeding these bytes to a fresh terminal reconstructs the state; this is
    /// what a reattach resync replays.
    pub fn dump(&self) -> Vec<u8> {
        self.vt.dump_with_scrollback().into_bytes()
    }

    /// The emulator dump without image transmit escapes, for a recording
    /// checkpoint: the image bytes are stored once out-of-band (see
    /// [`graphics_images`](Self::graphics_images)) and reconstructed on replay.
    pub fn dump_without_images(&self) -> Vec<u8> {
        self.vt.dump_with_scrollback_without_images().into_bytes()
    }

    /// The stored kitty-graphics images, for a recording checkpoint's
    /// content-addressed dedup (borrowed; pair with
    /// [`dump_without_images`](Self::dump_without_images)).
    pub fn graphics_images(&self) -> Vec<crate::record::CheckpointImage<'_>> {
        self.vt
            .graphics_images()
            .map(|i| crate::record::CheckpointImage {
                id: i.id,
                width: i.width,
                height: i.height,
                pixels: &i.pixels,
            })
            .collect()
    }

    /// A byte sequence that, sent to a real terminal, clears it and repaints the
    /// current screen state plus the session's bounded scrollback. Clears the
    /// visible screen only (not the client terminal's own scrollback); the
    /// replayed history scrolls in above the viewport.
    pub fn resync(&self) -> Vec<u8> {
        let mut seq = Vec::from(b"\x1b[2J\x1b[H".as_slice());
        seq.extend_from_slice(&self.dump());
        seq
    }

    /// The current screen as text lines (scrollback + viewport).
    pub fn text(&self) -> Vec<String> {
        self.vt.text()
    }

    /// The terminal's window title (OSC 0/2), empty if none has been set.
    /// The icon label a program set (OSC 0/1), for the `CSI 20 t` report.
    pub fn icon_title(&self) -> &str {
        self.vt.icon_title()
    }

    pub fn title(&self) -> &str {
        self.vt.title()
    }

    /// How many times the child has rung the terminal bell (BEL) since the
    /// session started. The host polls this after feeding to detect a ring even
    /// when no client is attached.
    pub fn bell_count(&self) -> u64 {
        self.vt.bell_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen_text(s: &Screen) -> Vec<String> {
        s.vt.text()
    }

    #[test]
    fn feeds_plain_text() {
        let mut s = Screen::new(20, 4, 100);
        s.feed(b"hello world");
        assert!(screen_text(&s)[0].starts_with("hello world"));
    }

    #[test]
    fn feed_reports_changed_rows_as_a_damage_hint() {
        let mut s = Screen::new(20, 4, 100);

        // The first feed paints from a fresh terminal, so every row is reported.
        assert_eq!(s.feed(b"first"), &[0, 1, 2, 3]);

        // After that, text on the top row reports only row 0.
        assert_eq!(s.feed(b"more"), &[0]);

        // A query that produces only a reply changes no visible row.
        assert!(s.feed(b"\x1b[6n").is_empty());

        // A full clear conservatively reports the whole viewport — the contract is
        // "these definitely changed", not "only these".
        assert_eq!(s.feed(b"\x1b[2J"), &[0, 1, 2, 3]);

        // An incomplete trailing UTF-8 byte is held back: nothing changed yet.
        assert!(s.feed(&[0xE2]).is_empty());
    }

    #[test]
    fn feed_reports_cursor_moves_as_damage() {
        let mut s = Screen::new(20, 4, 100);
        // Printing advances the cursor within row 0 (and dirties it as content):
        // establish a known baseline before the bare-move cases.
        s.feed(b"x");

        // A bare CUP to row 3 (1-based) prints nothing — no content row is
        // dirtied — but the drawn cursor left row 0 and entered row 2.
        assert!(
            s.feed(b"\x1b[3;1H").is_empty(),
            "a cursor move dirties no content row"
        );
        assert_eq!(
            s.cursor_damage(),
            CursorDamage {
                left: Some(0),
                entered: Some(2),
                repaint: true,
            }
        );

        // Hiding the cursor (DECTCEM) touches no cell: the row it was drawn on
        // repairs, nothing is entered, and a repaint is still warranted.
        assert!(s.feed(b"\x1b[?25l").is_empty());
        assert_eq!(
            s.cursor_damage(),
            CursorDamage {
                left: Some(2),
                entered: None,
                repaint: true,
            }
        );

        // A query that moves nothing reports no cursor damage.
        s.feed(b"\x1b[6n");
        assert_eq!(s.cursor_damage(), CursorDamage::default());
    }

    #[test]
    fn multibyte_split_across_feeds_is_not_corrupted() {
        let mut s = Screen::new(20, 4, 100);
        // "é" is 0xC3 0xA9; split it across two feeds.
        s.feed(&[0xc3]);
        s.feed(&[0xa9]);
        assert!(screen_text(&s)[0].starts_with('é'.to_string().as_str()));
    }

    #[test]
    fn invalid_bytes_become_replacement_char() {
        let mut s = Screen::new(20, 4, 100);
        s.feed(&[b'a', 0xff, b'b']);
        assert_eq!(screen_text(&s)[0].trim_end(), "a\u{fffd}b");
    }

    #[test]
    fn resync_reconstructs_visible_text() {
        let mut s = Screen::new(20, 4, 100);
        s.feed(b"marker-line\r\n");
        let seq = s.resync();
        // Replaying the resync into a fresh terminal reproduces the text.
        let mut replay = Vt::builder().size(20, 4).build();
        replay.feed_str(&String::from_utf8(seq).unwrap());
        assert!(replay.text().iter().any(|l| l.contains("marker-line")));
    }

    #[test]
    fn resync_includes_scrolled_off_lines() {
        // A 4-row screen; print more lines than fit so the first scrolls off.
        let mut s = Screen::new(20, 4, 100);
        for i in 0..12 {
            s.feed(format!("row-{i}\r\n").as_bytes());
        }
        let seq = s.resync();
        // Replay into a terminal that keeps scrollback: the first row, long
        // gone from the viewport, must still be recoverable.
        let mut replay = Vt::builder().size(20, 4).scrollback_limit(100).build();
        replay.feed_str(&String::from_utf8(seq).unwrap());
        assert!(
            replay.lines().any(|l| l.text().contains("row-0")),
            "scrolled-off line not replayed"
        );
    }

    #[test]
    fn resync_restores_mouse_and_paste_modes() {
        // An app (vim/htop-style) enables mouse tracking, SGR coordinates, and
        // bracketed paste. After a detach/reattach these must be restored, or
        // mouse and paste stop working in the reattached terminal.
        let mut s = Screen::new(80, 24, 100);
        s.feed(b"\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?2004h");
        let seq = s.resync();
        let text = String::from_utf8_lossy(&seq);
        for mode in ["\x1b[?1000h", "\x1b[?1002h", "\x1b[?1006h", "\x1b[?2004h"] {
            assert!(text.contains(mode), "resync missing {mode:?}");
        }
    }

    #[test]
    fn vt_accessor_exposes_live_emulator() {
        // A renderer lays out the live grid via `screen.vt()` -> the borrowed
        // emulator must reflect what's been fed and report the right size.
        let mut s = Screen::new(20, 4, 100);
        s.feed(b"live-grid");
        let vt = s.vt();
        assert_eq!(vt.size(), (20, 4));
        assert!(vt.text()[0].starts_with("live-grid"));
    }

    #[test]
    fn counts_ground_bell_but_ignores_osc_terminator_bell() {
        let mut s = Screen::new(20, 4, 100);
        assert_eq!(s.bell_count(), 0);
        s.feed(b"\x07");
        assert_eq!(s.bell_count(), 1, "a lone BEL rings the bell");
        // A BEL that terminates an OSC (title) sequence is not a bell.
        s.feed(b"\x1b]2;title\x07");
        assert_eq!(s.bell_count(), 1, "OSC-terminator BEL must not count");
    }

    #[test]
    fn resync_restores_window_title() {
        // An app sets the window title (OSC 2). After a detach/reattach the
        // title must be restored, or the reattached terminal keeps a stale or
        // default title.
        let mut s = Screen::new(80, 24, 100);
        s.feed(b"\x1b]2;ghost session\x07");
        let seq = s.resync();
        let text = String::from_utf8_lossy(&seq);
        assert!(
            text.contains("\x1b]2;ghost session\x07"),
            "resync missing title: {text:?}"
        );
    }

    #[test]
    fn reconstructs_from_checkpoint_and_bound() {
        use crate::record::{Recorder, read_bytes, truncate_before_latest_checkpoint};

        // Build a recording with a checkpoint partway, plus a resize after it,
        // while tracking the true (live) state for comparison.
        let mut live = Screen::new(20, 5, 1000);
        let mut buf = Vec::new();
        {
            let mut rec = Recorder::new(&mut buf, 20, 5, &[]).unwrap();
            for i in 0..15 {
                let line = format!("line-{i}\r\n");
                rec.output(line.as_bytes()).unwrap();
                live.feed(line.as_bytes());
            }
            let (c, r) = live.dimensions();
            rec.checkpoint(c, r, &live.dump()).unwrap();
            for i in 15..25 {
                let line = format!("line-{i}\r\n");
                rec.output(line.as_bytes()).unwrap();
                live.feed(line.as_bytes());
            }
            rec.resize(30, 5).unwrap();
            live.resize(30, 5);
            for i in 25..30 {
                let line = format!("line-{i}\r\n");
                rec.output(line.as_bytes()).unwrap();
                live.feed(line.as_bytes());
            }
            rec.flush().unwrap();
        }

        // Reconstructing from the full recording (which replays from the latest
        // checkpoint) reproduces the live screen exactly.
        let full = read_bytes(&buf).unwrap();
        let from_full = Screen::from_recording(&full, 1000);
        assert_eq!(from_full.text(), live.text());

        // Bounding the recording at its checkpoint loses no reconstructable
        // state: it yields the same screen.
        let bounded = read_bytes(&truncate_before_latest_checkpoint(&buf).unwrap()).unwrap();
        let from_bounded = Screen::from_recording(&bounded, 1000);
        assert_eq!(from_bounded.text(), live.text());
    }
}
