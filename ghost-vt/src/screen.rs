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

use vt::Vt;

/// Default bound on retained scrollback lines. Keeps host memory bounded; the
/// viewport itself is always reconstructable regardless of this limit.
pub const DEFAULT_SCROLLBACK: usize = 1000;

pub struct Screen {
    vt: Vt,
    /// Incomplete trailing UTF-8 bytes carried over from the previous feed.
    pending: Vec<u8>,
}

impl Screen {
    pub fn new(cols: u16, rows: u16, scrollback_limit: usize) -> Self {
        let vt = Vt::builder()
            .size(cols as usize, rows as usize)
            .scrollback_limit(scrollback_limit)
            .build();
        Screen {
            vt,
            pending: Vec::new(),
        }
    }

    /// Feed raw PTY bytes, decoding as much valid UTF-8 as possible and holding
    /// back only a genuinely incomplete trailing sequence for next time.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(s) => {
                    if !s.is_empty() {
                        self.vt.feed_str(s);
                    }
                    self.pending.clear();
                    break;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    if valid > 0 {
                        // SAFETY: `valid_up_to` guarantees this prefix is UTF-8.
                        let s = unsafe { std::str::from_utf8_unchecked(&self.pending[..valid]) };
                        self.vt.feed_str(s);
                    }
                    match e.error_len() {
                        // Incomplete tail: keep it, wait for the rest.
                        None => {
                            self.pending.drain(..valid);
                            break;
                        }
                        // Invalid byte(s): emit a replacement char and skip them.
                        Some(bad) => {
                            self.vt.feed_str("\u{fffd}");
                            self.pending.drain(..valid + bad);
                        }
                    }
                }
            }
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.vt.resize(cols as usize, rows as usize);
    }

    /// A byte sequence that, sent to a real terminal, clears it and repaints the
    /// current screen state plus the session's bounded scrollback. Clears the
    /// visible screen only (not the client terminal's own scrollback); the
    /// replayed history scrolls in above the viewport.
    pub fn resync(&self) -> Vec<u8> {
        let mut seq = String::from("\x1b[2J\x1b[H");
        seq.push_str(&self.vt.dump_with_scrollback());
        seq.into_bytes()
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
}
