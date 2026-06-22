//! Drives one attached ghost session: streams its output into a local
//! `ghost_vt::screen::Screen`, answers the terminal queries the child emits
//! (the role VTE plays for ghost-gtk), and sends keystrokes / resizes back.
//!
//! The render half lives elsewhere — a caller lays out `self.screen().vt()`
//! with `ghost-render` and draws it. This module is the protocol plumbing.

use std::io;
use std::time::Duration;

use ghost_vt::client::Session;
use ghost_vt::query::QueryScanner;
use ghost_vt::screen::{self, Screen};
use winit::keyboard::{Key, ModifiersState};

use crate::{encode, mouse};

/// Outcome of draining pending session output.
pub struct Pumped {
    /// Output arrived and was fed into the screen — the view should repaint.
    pub dirty: bool,
    /// The child exited or the host closed the connection.
    pub ended: bool,
}

/// An attached session plus the local emulator state mirroring it.
pub struct SessionView {
    session: Session,
    screen: Screen,
    scanner: QueryScanner,
    cols: u16,
    rows: u16,
}

impl SessionView {
    /// Attach (deferred) to a named session and complete the handshake at
    /// `cols`×`rows` — the first resize both promotes us to the display client
    /// and triggers the host's repaint laid out at our real size.
    pub fn attach(name: &str, cols: u16, rows: u16) -> io::Result<Self> {
        Self::from_session(Session::attach_deferred(name)?, cols, rows)
    }

    fn from_session(session: Session, cols: u16, rows: u16) -> io::Result<Self> {
        session.set_read_timeout(Some(Duration::from_millis(1)))?;
        let mut view = SessionView {
            session,
            screen: Screen::new(cols, rows, screen::DEFAULT_SCROLLBACK),
            scanner: QueryScanner::new(),
            cols,
            rows,
        };
        view.session.resize(cols, rows)?; // first resize == attach handshake
        Ok(view)
    }

    pub fn screen(&self) -> &Screen {
        &self.screen
    }

    /// Send raw bytes to the child PTY (paste, IME commits, query replies).
    pub fn send_input(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.session.send_input(bytes)
    }

    /// Encode a pressed key (honoring DECCKM) and send it to the child.
    pub fn key(&mut self, key: &Key, mods: ModifiersState) -> io::Result<()> {
        let app_cursor = self.screen.vt().cursor_key_app_mode();
        if let Some(bytes) = encode::encode(key, mods, app_cursor) {
            self.session.send_input(&bytes)?;
        }
        Ok(())
    }

    /// Report a mouse event to the child, gated on its active mouse mode.
    /// `col`/`row` are 1-based cells; `held` says a button is currently down.
    pub fn mouse(
        &mut self,
        kind: mouse::Kind,
        button: Option<mouse::Button>,
        held: bool,
        col: u16,
        row: u16,
        mods: ModifiersState,
    ) -> io::Result<()> {
        let proto = self.screen.vt().mouse_protocol();
        let sgr = self.screen.vt().mouse_sgr();
        if let Some(bytes) = mouse::encode(proto, sgr, kind, button, held, col, row, mods) {
            self.session.send_input(&bytes)?;
        }
        Ok(())
    }

    /// Report a focus change if the child enabled focus reporting (DEC 1004).
    pub fn focus(&mut self, focused: bool) -> io::Result<()> {
        if self.screen.vt().focus_report() {
            let seq: &[u8] = if focused { b"\x1b[I" } else { b"\x1b[O" };
            self.session.send_input(seq)?;
        }
        Ok(())
    }

    /// Send pasted text, wrapping it in bracketed-paste markers when the child
    /// enabled DEC mode 2004 so it can tell a paste from typing.
    pub fn paste(&mut self, text: &str) -> io::Result<()> {
        let bytes = bracket_paste(text.as_bytes(), self.screen.vt().bracketed_paste());
        self.session.send_input(&bytes)
    }

    /// Tell the host the grid changed (no-op if unchanged or degenerate).
    pub fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        if cols == 0 || rows == 0 || (cols, rows) == (self.cols, self.rows) {
            return Ok(());
        }
        self.cols = cols;
        self.rows = rows;
        self.screen.resize(cols, rows);
        self.session.resize(cols, rows)
    }

    /// Drain up to `max` pending reads, feeding output into the screen and
    /// answering any terminal queries it carried. Bounded so a flood can't
    /// starve the caller's loop.
    pub fn drain(&mut self, max: usize) -> io::Result<Pumped> {
        let mut dirty = false;
        for _ in 0..max {
            let pump = self.session.pump()?;
            if !pump.output.is_empty() {
                dirty = true;
                self.screen.feed(&pump.output);
                let cursor = self.screen.cursor();
                let size = self.screen.dimensions();
                let replies = query_replies(&mut self.scanner, &pump.output, cursor, size);
                if !replies.is_empty() {
                    self.session.send_input(&replies)?;
                }
            }
            if pump.ended {
                return Ok(Pumped { dirty, ended: true });
            }
            if pump.output.is_empty() {
                break;
            }
        }
        Ok(Pumped {
            dirty,
            ended: false,
        })
    }
}

/// Scan child output for terminal queries and build the reply bytes from the
/// given 1-based `(col, row)` cursor and `(cols, rows)` size. Pure, so the query
/// wiring is unit-testable without a live session.
pub fn query_replies(
    scanner: &mut QueryScanner,
    output: &[u8],
    cursor: (u16, u16),
    size: (u16, u16),
) -> Vec<u8> {
    let mut out = Vec::new();
    for query in scanner.scan(output) {
        out.extend_from_slice(&query.reply(cursor, size));
    }
    out
}

/// Wrap pasted bytes in bracketed-paste markers (`ESC[200~` … `ESC[201~`) when
/// the terminal enabled DEC mode 2004; otherwise pass them through unchanged.
/// Pure, so the paste wiring is unit-testable.
pub fn bracket_paste(text: &[u8], bracketed: bool) -> Vec<u8> {
    if !bracketed {
        return text.to_vec();
    }
    let mut out = Vec::with_capacity(text.len() + 12);
    out.extend_from_slice(b"\x1b[200~");
    out.extend_from_slice(text);
    out.extend_from_slice(b"\x1b[201~");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bracketed_paste_wraps_only_when_enabled() {
        assert_eq!(bracket_paste(b"hi", false), b"hi");
        assert_eq!(bracket_paste(b"hi", true), b"\x1b[200~hi\x1b[201~".to_vec());
    }

    #[test]
    fn answers_cursor_position_query() {
        let mut s = QueryScanner::new();
        // CSI 6 n with the cursor at col 3, row 5 -> CSI 5;3 R.
        let reply = query_replies(&mut s, b"\x1b[6n", (3, 5), (80, 24));
        assert_eq!(reply, b"\x1b[5;3R");
    }

    #[test]
    fn answers_primary_device_attributes() {
        let mut s = QueryScanner::new();
        let reply = query_replies(&mut s, b"\x1b[c", (1, 1), (80, 24));
        assert_eq!(reply, b"\x1b[?61;1;21;22;28c");
    }

    #[test]
    fn plain_output_needs_no_reply() {
        let mut s = QueryScanner::new();
        assert!(query_replies(&mut s, b"hello world\r\n", (1, 1), (80, 24)).is_empty());
    }
}
