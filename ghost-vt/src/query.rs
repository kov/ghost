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
    /// `CSI > q` / `CSI > 0 q` — XTVERSION, report terminal name and version.
    /// Reply: `DCS > | ghost <version> ST`. Claude Code sends it at startup.
    TerminalVersion,
    /// `CSI ? Ps $ p` — DECRQM, request a DEC private mode's state. Reply is
    /// DECRPM `CSI ? Ps ; Pm $ y` with Pm 0 = unrecognized, 1 = set, 2 = reset.
    /// Apps probe synchronized output (2026) this way before using it.
    ReportMode(u16),
    /// `OSC 10 ; ? ST` — the default foreground color. Reply: `OSC 10 ;
    /// rgb:rrrr/gggg/bbbb ST` (16-bit-per-channel xterm form).
    ForegroundColor,
    /// `OSC 11 ; ? ST` — the default background color; what vim/fzf/neovim
    /// theme detection rides on. Reply mirrors [`Query::ForegroundColor`].
    BackgroundColor,
}

/// The default fg/bg an OSC 10/11 color query is answered with. The attached
/// frontend passes its live scheme; the detached host answers with this
/// `Default` — ghost's default scheme (`ghost-renderer`'s `Theme::default`,
/// duplicated here because the layering points the other way) — until
/// last-attached colors are persisted per session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThemeColors {
    pub fg: [u8; 3],
    pub bg: [u8; 3],
}

impl Default for ThemeColors {
    fn default() -> Self {
        ThemeColors {
            fg: [0xd8, 0xdb, 0xe0],
            bg: [0x10, 0x10, 0x12],
        }
    }
}

/// Everything [`Query::reply`] can draw on, threaded identically by the
/// attached frontend and the detached host.
pub struct ReplyCtx<'a> {
    /// 1-based `(col, row)` cursor position (CPR).
    pub cursor: (u16, u16),
    /// `(cols, rows)` text-area size.
    pub size: (u16, u16),
    /// Current kitty-keyboard progressive-enhancement flags.
    pub kitty_flags: u8,
    /// Default fg/bg for the OSC color queries.
    pub colors: ThemeColors,
    /// A DEC private mode's state for DECRQM: `Some(true)` set, `Some(false)`
    /// reset, `None` unrecognized.
    pub mode_state: &'a dyn Fn(u16) -> Option<bool>,
}

/// The 16-bit-per-channel `rgb:rrrr/gggg/bbbb` form xterm reports colors in
/// (each 8-bit channel doubled into 16 bits).
fn xterm_rgb([r, g, b]: [u8; 3]) -> String {
    format!("rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}")
}

impl Query {
    /// The reply bytes to write back to the child's PTY.
    ///
    /// The device-attribute strings mirror VTE's non-test replies: DA1 reports a
    /// VT100-level (61) terminal with 132-column mode (1), horizontal scrolling
    /// (21), colour (22) and rectangular editing (28); DA2 reports the same level
    /// with VTE's version encoding as the firmware field.
    pub fn reply(&self, ctx: &ReplyCtx) -> Vec<u8> {
        let (col, row) = ctx.cursor;
        let (cols, rows) = ctx.size;
        match self {
            Query::CursorPosition => format!("\x1b[{row};{col}R").into_bytes(),
            Query::DeviceStatus => b"\x1b[0n".to_vec(),
            Query::PrimaryDeviceAttributes => b"\x1b[?61;1;21;22;28c".to_vec(),
            Query::SecondaryDeviceAttributes => b"\x1b[>61;8400;1c".to_vec(),
            Query::TextAreaSize => format!("\x1b[8;{rows};{cols}t").into_bytes(),
            Query::KittyKeyboardFlags => format!("\x1b[?{}u", ctx.kitty_flags).into_bytes(),
            Query::TerminalVersion => {
                format!("\x1bP>|ghost {}\x1b\\", env!("CARGO_PKG_VERSION")).into_bytes()
            }
            Query::ReportMode(mode) => {
                let pm = match (ctx.mode_state)(*mode) {
                    Some(true) => 1,
                    Some(false) => 2,
                    None => 0,
                };
                format!("\x1b[?{mode};{pm}$y").into_bytes()
            }
            Query::ForegroundColor => {
                format!("\x1b]10;{}\x1b\\", xterm_rgb(ctx.colors.fg)).into_bytes()
            }
            Query::BackgroundColor => {
                format!("\x1b]11;{}\x1b\\", xterm_rgb(ctx.colors.bg)).into_bytes()
            }
        }
    }
}

/// Upper bound on the bytes collected for one CSI sequence; well beyond any query
/// we recognize, so a pathological run of parameters can't grow the buffer.
const MAX_CSI_PARAMS: usize = 32;

/// Upper bound on a collected OSC payload. The OSC queries we answer ("10;?",
/// "11;?") are tiny; anything longer is a title/clipboard/other string, marked
/// overflowed and never classified — so a truncation can't masquerade as one.
const MAX_OSC_PAYLOAD: usize = 16;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    /// Normal output.
    Ground,
    /// Saw `ESC`; awaiting the sequence type.
    Esc,
    /// Inside a CSI (`ESC [`), collecting parameter/intermediate bytes.
    Csi,
    /// Inside an OSC string (`ESC ]`), collecting a small payload (the color
    /// queries live here) and skipping the rest until its terminator.
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
    /// Payload of the OSC currently being collected (bounded; see
    /// [`MAX_OSC_PAYLOAD`]).
    osc: Vec<u8>,
    /// The current OSC outgrew [`MAX_OSC_PAYLOAD`] (or carried an embedded
    /// escape): whatever terminates it, it is not a query.
    osc_overflow: bool,
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
            osc: Vec::new(),
            osc_overflow: false,
        }
    }

    /// Classify the just-terminated OSC, if it stayed small enough to be one
    /// of the queries we answer.
    fn osc_query(&self) -> Option<Query> {
        if self.osc_overflow {
            return None;
        }
        match self.osc.as_slice() {
            b"10;?" => Some(Query::ForegroundColor),
            b"11;?" => Some(Query::BackgroundColor),
            _ => None,
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
                    b']' => {
                        self.osc.clear();
                        self.osc_overflow = false;
                        self.state = State::Osc;
                    }
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
                    // Parameter and intermediate bytes (dropped past the cap).
                    0x20..=0x3f if self.params.len() < MAX_CSI_PARAMS => {
                        self.params.push(b);
                    }
                    0x20..=0x3f => {}
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
                    // BEL terminates: classify what was collected.
                    0x07 => {
                        if let Some(q) = self.osc_query() {
                            out.push(q);
                        }
                        self.state = State::Ground;
                    }
                    0x1b => self.state = State::OscEsc,
                    _ if self.osc.len() < MAX_OSC_PAYLOAD => self.osc.push(b),
                    _ => self.osc_overflow = true,
                },
                State::OscEsc => {
                    // `ESC \` is ST; any other byte is still OSC payload (and
                    // an embedded escape disqualifies it as a query).
                    if b == b'\\' {
                        if let Some(q) = self.osc_query() {
                            out.push(q);
                        }
                        self.state = State::Ground;
                    } else {
                        self.osc_overflow = true;
                        self.state = State::Osc;
                    }
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
        // XTVERSION: `CSI > q` / `CSI > 0 q`. DECSCUSR shares the final byte but
        // carries a space intermediate, never the `>` marker.
        b'q' => match params {
            b">" | b">0" => Some(Query::TerminalVersion),
            _ => None,
        },
        // DECRQM: `CSI ? Ps $ p` — the `?` and the `$` intermediate bracket a
        // single mode number. `CSI ! p` (DECSTR) and the ANSI-mode form (no `?`)
        // fall through.
        b'p' => {
            let mode = params.strip_prefix(b"?")?.strip_suffix(b"$")?;
            std::str::from_utf8(mode)
                .ok()?
                .parse()
                .ok()
                .map(Query::ReportMode)
        }
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
        assert_eq!(scan_all(b"\x1b[>q"), [Query::TerminalVersion]);
        assert_eq!(scan_all(b"\x1b[>0q"), [Query::TerminalVersion]);
        assert_eq!(scan_all(b"\x1b[?2026$p"), [Query::ReportMode(2026)]);
        assert_eq!(scan_all(b"\x1b[?25$p"), [Query::ReportMode(25)]);
    }

    #[test]
    fn ignores_non_queries() {
        assert!(scan_all(b"hello world").is_empty());
        assert!(scan_all(b"\x1b[31m\x1b[2J\x1b[H").is_empty()); // SGR, clear, home
        assert!(scan_all(b"\x1b[8;24;80t").is_empty()); // a resize op, not a query
        assert!(scan_all(b"\x1b[?6n").is_empty()); // DEC-private DSR, left alone
        assert!(scan_all(b"\x1b[u").is_empty()); // bare CSI u is SCO restore-cursor
        assert!(scan_all(b"\x1b[0 q").is_empty()); // DECSCUSR shares `q`, not a query
        assert!(scan_all(b"\x1b[!p").is_empty()); // DECSTR shares `p`, not a query
        assert!(scan_all(b"\x1b[2026$p").is_empty()); // ANSI-mode DECRQM (no `?`)
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

    /// A `mode_state` for tests: nothing is recognized.
    fn no_modes(_: u16) -> Option<bool> {
        None
    }

    /// A baseline reply context; tests override the fields they exercise.
    fn ctx() -> ReplyCtx<'static> {
        ReplyCtx {
            cursor: (1, 1),
            size: (80, 24),
            kitty_flags: 0,
            colors: ThemeColors::default(),
            mode_state: &no_modes,
        }
    }

    #[test]
    fn recognizes_osc_color_queries() {
        assert_eq!(scan_all(b"\x1b]10;?\x07"), [Query::ForegroundColor]);
        assert_eq!(scan_all(b"\x1b]11;?\x1b\\"), [Query::BackgroundColor]);
        // Set forms and unrelated OSCs are not queries.
        assert!(scan_all(b"\x1b]11;#101012\x07").is_empty());
        assert!(scan_all(b"\x1b]12;?\x07").is_empty()); // cursor color: not yet
        assert!(scan_all(b"\x1b]0;title\x07").is_empty());
        // A long payload overflows the small buffer and is never classified,
        // even if its prefix looks like a query.
        let mut long = b"\x1b]10;?".to_vec();
        long.extend(std::iter::repeat_n(b'x', 100));
        long.push(0x07);
        assert!(scan_all(&long).is_empty());
        // Split across feeds still recognized.
        let mut s = QueryScanner::new();
        assert!(s.scan(b"\x1b]11").is_empty());
        assert!(s.scan(b";?").is_empty());
        assert_eq!(s.scan(b"\x07"), [Query::BackgroundColor]);
    }

    #[test]
    fn reply_strings_match_vte() {
        assert_eq!(
            Query::CursorPosition.reply(&ReplyCtx {
                cursor: (5, 3),
                ..ctx()
            }),
            b"\x1b[3;5R" // row;col, 1-based
        );
        assert_eq!(Query::DeviceStatus.reply(&ctx()), b"\x1b[0n");
        assert_eq!(
            Query::PrimaryDeviceAttributes.reply(&ctx()),
            b"\x1b[?61;1;21;22;28c"
        );
        assert_eq!(
            Query::SecondaryDeviceAttributes.reply(&ctx()),
            b"\x1b[>61;8400;1c"
        );
        assert_eq!(
            Query::TextAreaSize.reply(&ctx()),
            b"\x1b[8;24;80t" // rows;cols
        );
        // The kitty query reports the current flags.
        assert_eq!(Query::KittyKeyboardFlags.reply(&ctx()), b"\x1b[?0u");
        assert_eq!(
            Query::KittyKeyboardFlags.reply(&ReplyCtx {
                kitty_flags: 5,
                ..ctx()
            }),
            b"\x1b[?5u"
        );
    }

    #[test]
    fn color_replies_use_the_xterm_rgb_form() {
        let themed = ReplyCtx {
            colors: ThemeColors {
                fg: [0xd8, 0xdb, 0xe0],
                bg: [0x10, 0x10, 0x12],
            },
            ..ctx()
        };
        assert_eq!(
            Query::ForegroundColor.reply(&themed),
            b"\x1b]10;rgb:d8d8/dbdb/e0e0\x1b\\"
        );
        assert_eq!(
            Query::BackgroundColor.reply(&themed),
            b"\x1b]11;rgb:1010/1010/1212\x1b\\"
        );
    }

    #[test]
    fn xtversion_reply_names_ghost() {
        let reply = Query::TerminalVersion.reply(&ctx());
        let s = String::from_utf8(reply).unwrap();
        assert!(
            s.starts_with("\x1bP>|ghost ") && s.ends_with("\x1b\\"),
            "malformed XTVERSION reply: {s:?}"
        );
    }

    #[test]
    fn decrqm_reply_reports_mode_state() {
        // DECRPM Pm: 1 = set, 2 = reset, 0 = unrecognized.
        let state = |m: u16| match m {
            2026 => Some(true),
            2004 => Some(false),
            _ => None,
        };
        let modal = ReplyCtx {
            mode_state: &state,
            ..ctx()
        };
        assert_eq!(Query::ReportMode(2026).reply(&modal), b"\x1b[?2026;1$y");
        assert_eq!(Query::ReportMode(2004).reply(&modal), b"\x1b[?2004;2$y");
        assert_eq!(Query::ReportMode(12345).reply(&modal), b"\x1b[?12345;0$y");
    }
}
