//! Terminal-query emulation for detached sessions.
//!
//! A program can ask the terminal about itself — cursor position, device
//! attributes, status, text-area size — and block until it gets a reply. While a
//! client is attached the client (a real terminal, or VTE) answers. With nobody
//! attached, nobody would, and a program that queries on startup (a shell)
//! stalls. The host fills that gap: it scans the child's output for queries and,
//! when detached, replies from its own [`Screen`](crate::screen::Screen) state.
//!
//! [`QueryScanner`] is a deliberately small control-sequence recognizer, kept
//! separate from the `avt` emulator (which we treat as upstream): `avt` discards
//! these queries, and teaching it to generate PTY replies is out of scope for a
//! screen/recording emulator. The scanner only needs to *spot* the handful of
//! query sequences we answer; it carries enough state across calls that a
//! sequence split over two PTY reads is still recognized, and skips OSC/DCS/etc.
//! string payloads so their contents can't be mistaken for a query.
//!
//! Replies mirror what VTE reports (per the user's choice), so a program sees the
//! same answers whether the session is attached or detached.

/// A query the host knows how to answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Query {
    /// `CSI 6 n` — cursor position report. Reply: `CSI row ; col R` (1-based).
    CursorPosition,
    /// `CSI 5 n` — device status report. Reply: `CSI 0 n` (OK).
    DeviceStatus,
    /// `CSI c` / `CSI 0 c` — primary device attributes.
    PrimaryDeviceAttributes,
    /// `CSI > c` / `CSI > 0 c` — secondary device attributes.
    SecondaryDeviceAttributes,
    /// `CSI 18 t` — report text-area size in characters. Reply: `CSI 8 ; rows ; cols t`.
    TextAreaSize,
    /// `CSI ? u` — kitty keyboard protocol flags query. Reply: `CSI ? flags u`
    /// with the terminal's current progressive-enhancement flags. Answering it
    /// (before the DA reply) is how an app detects kitty-keyboard support.
    KittyKeyboardFlags,
}

impl Query {
    /// The reply bytes to write back to the child's PTY. `cursor` is the 1-based
    /// `(col, row)` cursor position, `size` is `(cols, rows)`, and `kitty_flags`
    /// is the current kitty-keyboard flags (only the kitty query uses it).
    ///
    /// The device-attribute strings mirror VTE's non-test replies: DA1 reports a
    /// VT100-level (61) terminal with 132-column mode (1), horizontal scrolling
    /// (21), colour (22) and rectangular editing (28); DA2 reports the same level
    /// with VTE's version encoding as the firmware field.
    pub fn reply(&self, cursor: (u16, u16), size: (u16, u16), kitty_flags: u8) -> Vec<u8> {
        let (col, row) = cursor;
        let (cols, rows) = size;
        match self {
            Query::CursorPosition => format!("\x1b[{row};{col}R").into_bytes(),
            Query::DeviceStatus => b"\x1b[0n".to_vec(),
            Query::PrimaryDeviceAttributes => b"\x1b[?61;1;21;22;28c".to_vec(),
            Query::SecondaryDeviceAttributes => b"\x1b[>61;8400;1c".to_vec(),
            Query::TextAreaSize => format!("\x1b[8;{rows};{cols}t").into_bytes(),
            Query::KittyKeyboardFlags => format!("\x1b[?{kitty_flags}u").into_bytes(),
        }
    }
}

/// Upper bound on the bytes collected for one CSI sequence; well beyond any query
/// we recognize, so a pathological run of parameters can't grow the buffer.
const MAX_CSI_PARAMS: usize = 32;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    /// Normal output.
    Ground,
    /// Saw `ESC`; awaiting the sequence type.
    Esc,
    /// Inside a CSI (`ESC [`), collecting parameter/intermediate bytes.
    Csi,
    /// Inside an OSC string (`ESC ]`), skipping until its terminator.
    Osc,
    /// Saw `ESC` inside an OSC string — maybe the start of an ST (`ESC \`).
    OscEsc,
    /// Inside a DCS/SOS/PM/APC string, skipping until its terminator.
    Str,
    /// Saw `ESC` inside such a string — maybe the start of an ST.
    StrEsc,
}

/// Stateful scanner over a child's PTY output stream. One per session; feed it
/// every chunk in order.
pub struct QueryScanner {
    state: State,
    /// Parameter and intermediate bytes of the CSI currently being collected.
    params: Vec<u8>,
}

impl Default for QueryScanner {
    fn default() -> Self {
        Self::new()
    }
}

impl QueryScanner {
    pub fn new() -> Self {
        QueryScanner {
            state: State::Ground,
            params: Vec::new(),
        }
    }

    /// Feed the next chunk of output; returns the queries it contained, in order.
    pub fn scan(&mut self, bytes: &[u8]) -> Vec<Query> {
        let mut out = Vec::new();
        for &b in bytes {
            match self.state {
                State::Ground => {
                    if b == 0x1b {
                        self.state = State::Esc;
                    }
                }
                State::Esc => match b {
                    b'[' => {
                        self.params.clear();
                        self.state = State::Csi;
                    }
                    b']' => self.state = State::Osc,
                    // DCS, SOS, PM, APC: string controls terminated by ST.
                    b'P' | b'X' | b'^' | b'_' => self.state = State::Str,
                    // ESC ESC: stay primed for the real introducer.
                    0x1b => {}
                    // Any other short escape sequence: nothing we answer.
                    _ => self.state = State::Ground,
                },
                State::Csi => match b {
                    // A new ESC aborts the (malformed) sequence.
                    0x1b => self.state = State::Esc,
                    // Parameter and intermediate bytes.
                    0x20..=0x3f => {
                        if self.params.len() < MAX_CSI_PARAMS {
                            self.params.push(b);
                        }
                    }
                    // Final byte: classify and emit.
                    0x40..=0x7e => {
                        if let Some(q) = classify_csi(&self.params, b) {
                            out.push(q);
                        }
                        self.state = State::Ground;
                    }
                    // C0 controls within a CSI are executed and ignored here.
                    _ => {}
                },
                State::Osc => match b {
                    0x07 => self.state = State::Ground, // BEL terminates
                    0x1b => self.state = State::OscEsc,
                    _ => {}
                },
                State::OscEsc => {
                    // `ESC \` is ST; any other byte is still OSC payload.
                    self.state = if b == b'\\' {
                        State::Ground
                    } else {
                        State::Osc
                    };
                }
                State::Str => match b {
                    0x07 => self.state = State::Ground,
                    0x1b => self.state = State::StrEsc,
                    _ => {}
                },
                State::StrEsc => {
                    self.state = if b == b'\\' {
                        State::Ground
                    } else {
                        State::Str
                    };
                }
            }
        }
        out
    }
}

/// Classify a completed CSI from its parameter/intermediate bytes and final byte.
/// Only the query sequences we answer return `Some`.
fn classify_csi(params: &[u8], final_byte: u8) -> Option<Query> {
    match final_byte {
        // DSR — device status report. The `?`-private DEC variants are left alone.
        b'n' => match params {
            b"6" => Some(Query::CursorPosition),
            b"5" => Some(Query::DeviceStatus),
            _ => None,
        },
        // DA — device attributes. `>` marks the secondary request.
        b'c' => match params {
            b"" | b"0" => Some(Query::PrimaryDeviceAttributes),
            b">" | b">0" => Some(Query::SecondaryDeviceAttributes),
            _ => None,
        },
        // Window ops — only the text-area-size request (18) is a query we answer.
        b't' => match params {
            b"18" => Some(Query::TextAreaSize),
            _ => None,
        },
        // kitty keyboard flags query: `CSI ? u`. A bare `CSI u` (empty params) is
        // SCO restore-cursor, not a query, so only the `?`-marked form matches.
        b'u' => match params {
            b"?" => Some(Query::KittyKeyboardFlags),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_all(bytes: &[u8]) -> Vec<Query> {
        QueryScanner::new().scan(bytes)
    }

    #[test]
    fn recognizes_each_query() {
        assert_eq!(scan_all(b"\x1b[6n"), [Query::CursorPosition]);
        assert_eq!(scan_all(b"\x1b[5n"), [Query::DeviceStatus]);
        assert_eq!(scan_all(b"\x1b[c"), [Query::PrimaryDeviceAttributes]);
        assert_eq!(scan_all(b"\x1b[0c"), [Query::PrimaryDeviceAttributes]);
        assert_eq!(scan_all(b"\x1b[>c"), [Query::SecondaryDeviceAttributes]);
        assert_eq!(scan_all(b"\x1b[>0c"), [Query::SecondaryDeviceAttributes]);
        assert_eq!(scan_all(b"\x1b[18t"), [Query::TextAreaSize]);
        assert_eq!(scan_all(b"\x1b[?u"), [Query::KittyKeyboardFlags]);
    }

    #[test]
    fn ignores_non_queries() {
        assert!(scan_all(b"hello world").is_empty());
        assert!(scan_all(b"\x1b[31m\x1b[2J\x1b[H").is_empty()); // SGR, clear, home
        assert!(scan_all(b"\x1b[8;24;80t").is_empty()); // a resize op, not a query
        assert!(scan_all(b"\x1b[?6n").is_empty()); // DEC-private DSR, left alone
        assert!(scan_all(b"\x1b[>q").is_empty()); // XTVERSION (not answered yet)
        assert!(scan_all(b"\x1b[u").is_empty()); // bare CSI u is SCO restore-cursor
    }

    #[test]
    fn handles_a_query_split_across_feeds() {
        let mut s = QueryScanner::new();
        assert!(s.scan(b"output\x1b[").is_empty());
        assert!(s.scan(b"6").is_empty());
        assert_eq!(s.scan(b"n more"), [Query::CursorPosition]);
    }

    #[test]
    fn finds_queries_amid_other_output() {
        // SGR, then a CPR, then text, then a primary DA.
        assert_eq!(
            scan_all(b"\x1b[1;32mprompt\x1b[6n$ \x1b[c"),
            [Query::CursorPosition, Query::PrimaryDeviceAttributes]
        );
    }

    #[test]
    fn skips_string_payloads() {
        // A title (OSC, BEL-terminated) and a DCS (ST-terminated) whose contents
        // look query-ish must not be parsed as CSI queries; the trailing real CPR
        // still is.
        assert_eq!(scan_all(b"\x1b]0;\x1b[6n title\x07done"), []);
        assert_eq!(scan_all(b"\x1bP1$r\x1b[6n\x1b\\after"), []);
        assert_eq!(
            scan_all(b"\x1b]0;title\x07\x1b[6n"),
            [Query::CursorPosition]
        );
    }

    #[test]
    fn reply_strings_match_vte() {
        assert_eq!(
            Query::CursorPosition.reply((5, 3), (80, 24), 0),
            b"\x1b[3;5R" // row;col, 1-based
        );
        assert_eq!(Query::DeviceStatus.reply((1, 1), (80, 24), 0), b"\x1b[0n");
        assert_eq!(
            Query::PrimaryDeviceAttributes.reply((1, 1), (80, 24), 0),
            b"\x1b[?61;1;21;22;28c"
        );
        assert_eq!(
            Query::SecondaryDeviceAttributes.reply((1, 1), (80, 24), 0),
            b"\x1b[>61;8400;1c"
        );
        assert_eq!(
            Query::TextAreaSize.reply((1, 1), (80, 24), 0),
            b"\x1b[8;24;80t" // rows;cols
        );
        // The kitty query reports the current flags.
        assert_eq!(
            Query::KittyKeyboardFlags.reply((1, 1), (80, 24), 0),
            b"\x1b[?0u"
        );
        assert_eq!(
            Query::KittyKeyboardFlags.reply((1, 1), (80, 24), 5),
            b"\x1b[?5u"
        );
    }
}
