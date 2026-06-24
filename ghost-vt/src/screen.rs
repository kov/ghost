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

pub struct Screen {
    vt: Vt,
    cols: u16,
    rows: u16,
    /// Incomplete trailing UTF-8 bytes carried over from the previous feed.
    pending: Vec<u8>,
    /// Reused scratch for the viewport rows the last [`feed`](Screen::feed)
    /// changed, so reporting damage costs no per-feed allocation.
    dirty_rows: Vec<usize>,
}

impl Screen {
    pub fn new(cols: u16, rows: u16, scrollback_limit: usize) -> Self {
        let vt = Vt::builder()
            .size(cols as usize, rows as usize)
            .scrollback_limit(scrollback_limit)
            .build();
        Screen {
            vt,
            cols,
            rows,
            pending: Vec::new(),
            dirty_rows: Vec::new(),
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
    /// Returns the viewport rows this feed changed (sorted, deduplicated). The
    /// contract is **"these rows definitely changed"**, not "only these changed":
    /// scrolling, full clears, alt-screen switches and reflow conservatively
    /// report the whole viewport. It is a damage hint — useful to skip work when
    /// it is *empty or small*, never to assume an unlisted row is untouched. An
    /// empty slice means nothing in the viewport changed (e.g. a query that only
    /// produced a reply, or bytes held back as an incomplete tail).
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
        &self.dirty_rows
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.cols = cols;
        self.rows = rows;
        self.vt.resize(cols as usize, rows as usize);
    }

    /// Current terminal size as `(cols, rows)`.
    pub fn dimensions(&self) -> (u16, u16) {
        (self.cols, self.rows)
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

    /// Cursor position as 1-based `(col, row)` — the form a cursor-position
    /// report (CPR) carries. `avt` tracks the cursor 0-based.
    pub fn cursor(&self) -> (u16, u16) {
        let c = self.vt.cursor();
        (
            (c.col as u16).saturating_add(1),
            (c.row as u16).saturating_add(1),
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
