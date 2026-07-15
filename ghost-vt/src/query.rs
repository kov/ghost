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
//! The attached and detached paths run through this same scanner, so a program
//! sees identical answers either way. Where an answer is a matter of identity
//! rather than fact — the device-attributes replies — ghost presents xterm, which
//! is what the rest of its surface (`TERM`, XTVERSION, the emulator's graded
//! behavior) already claims.

/// A query the host knows how to answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Query {
    /// `CSI 6 n` — cursor position report. Reply: `CSI row ; col R` (1-based).
    CursorPosition,
    /// `CSI 5 n` — device status report. Reply: `CSI 0 n` (OK).
    DeviceStatus,
    /// `CSI c` / `CSI 0 c` — primary device attributes.
    PrimaryDeviceAttributes,
    /// `CSI > c` / `CSI > 0 c` — secondary device attributes. Reply
    /// `CSI > 41 ; 420 ; 0 c` — an xterm-emulating VT420 (`41`).
    ///
    /// We present xterm, not the VTE identity ghost carried as a VTE frontend:
    /// VTE is out of the loop now (both attached and detached answer through this
    /// same scanner), so mirroring it bought nothing, while every other face ghost
    /// shows — `TERM=xterm-256color`, the xterm-shaped primary DA, XTVERSION's
    /// honest `ghost <version>` — is xterm's, and the emulator is graded against
    /// xterm's behavior. DA2 has no "ghost" model number, so *some* mask is
    /// unavoidable; the `420` firmware field is a fixed placeholder in xterm's
    /// DA2-reported range (a real version can't be honest here either).
    SecondaryDeviceAttributes,
    /// `CSI = c` / `CSI = 0 c` — tertiary device attributes (unit id). Reply:
    /// DECRPTUI `DCS ! | 00000000 ST` (xterm's anonymous default).
    TertiaryDeviceAttributes,
    /// `CSI 18 t` — report text-area size in characters. Reply: `CSI 8 ; rows ; cols t`.
    TextAreaSize,
    /// `CSI 19 t` — report the *display's* size in characters: how big the text
    /// area could grow. Reply: `CSI 9 ; rows ; cols t`. A program maximizing the
    /// window reads this to know what it should have got.
    DisplaySize,
    /// `CSI 11 t` — report whether the window is iconified. Reply: `CSI 1 t` when
    /// it is open, `CSI 2 t` when it is iconified.
    WindowState,
    /// `CSI 14 t` — report the text area's size in pixels. Reply: `CSI 4 ; height ;
    /// width t`. `CSI 15 t` reports the display's, as `CSI 5 ; height ; width t`,
    /// and `CSI 16 t` a single cell's, as `CSI 6 ; height ; width t` — between them
    /// a program can do the arithmetic a pixel resize needs. (The request code and
    /// the reply code differ, as xterm has them: ask with 15, hear back a 5.)
    TextAreaSizePixels,
    DisplaySizePixels,
    CellSizePixels,
    /// `CSI 21 t` / `CSI 20 t` — report the window title, and the icon label.
    /// Replies `OSC l <title> ST` and `OSC L <label> ST`. Ghost carries one title
    /// and answers both with it: it has no separate icon label to keep.
    ///
    /// Reported verbatim. xterm can hex-encode a title (its `CSI > Ps t` title
    /// modes), which ghost does not do — no program asks a terminal for a hex
    /// title, and tracking a mode we would not act on is worse than not having it.
    WindowTitle,
    IconLabel,
    /// `CSI ? u` — kitty keyboard protocol flags query. Reply: `CSI ? flags u`
    /// with the terminal's current progressive-enhancement flags. Answering it
    /// (before the DA reply) is how an app detects kitty-keyboard support.
    KittyKeyboardFlags,
    /// `CSI > q` / `CSI > 0 q` — XTVERSION, report terminal name and version.
    /// Reply: `DCS > | ghost <version> ST`. Claude Code sends it at startup.
    TerminalVersion,
    /// `CSI Ps $ p` — ANSI-mode DECRQM (no `?`). A VT300+ feature: answered
    /// `CSI Ps ; Pm $ y` only at conformance level ≥ 3, silent below (which is
    /// how a program probes the level). `Pm` mirrors [`Query::ReportMode`].
    ReportAnsiMode(u16),
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
    /// `OSC 12 ; ? ST` — the cursor color. Reply mirrors
    /// [`Query::ForegroundColor`].
    CursorColor,
    /// `OSC 4 ; c ; ? ST` — an indexed palette color. One OSC may ask for several
    /// (`4;0;?;1;?`), and each is answered by its own `OSC 4 ; c ;
    /// rgb:rrrr/gggg/bbbb ST`, in the order asked. The color reported is what the
    /// screen shows: the app's OSC 4 override if it set one, else the theme's.
    ///
    /// An index at or past [`ghost_term::SPECIAL_COLOR_BASE`] names a special
    /// color (see [`Query::SpecialColors`]) — xterm's other way of addressing
    /// them, answered in the form it was asked in.
    PaletteColors(Vec<u16>),
    /// `OSC 5 ; c ; ? ST` — a special color (bold, underline, blink, reverse,
    /// italic), answered as `OSC 5 ; c ; rgb:rrrr/gggg/bbbb ST`. An app that has
    /// not set one reads back the theme foreground, which is what its text is
    /// painted with.
    SpecialColors(Vec<u16>),
    /// `CSI ? 996 n` — one-shot color-scheme request (contour's dark/light
    /// extension, mode 2031's query form). Reply: `CSI ? 997 ; Ps n` with
    /// Ps 1 = dark, 2 = light.
    ColorScheme,
    /// XTGETTCAP `DCS + q Pt ST` — termcap/terminfo capability query; `Pt` is
    /// a `;`-separated list of hex-encoded capability names, **kept as asked**.
    /// Answered per cap, kitty-style: `DCS 1 + r <hexname>[=<hexvalue>] ST`
    /// for a known cap (no `=value` for a true boolean), `DCS 0 + r <hexname>
    /// ST` for an unknown one — echoing the name hex for hex, since a client
    /// matches the reply against the name it sent (esctest asks for `Co` as
    /// `436F` and drops a reply that comes back as `436f`).
    Termcap(Vec<String>),
    /// DECRQSS `DCS $ q <selector> ST` — request a control-function setting.
    /// Only DECSCUSR (`" q"`, the cursor style — vim's t_RS probe) is
    /// reported; everything else gets the well-formed invalid reply
    /// `DCS 0 $ r ST`.
    Setting(String),
    /// DECRQCRA `CSI Pid ; Pp ; Pt ; Pl ; Pb ; Pr * y` — request the checksum of
    /// a screen rectangle. `Pid` is echoed back; `Pt`/`Pl`/`Pb`/`Pr` are the
    /// 1-based inclusive top/left/bottom/right (the page `Pp` is ignored — ghost
    /// has one page). Reply: DECCKSR `DCS Pid ! ~ HHHH ST`, `HHHH` the negated
    /// 16-bit checksum from [`ghost_term::Vt::rect_checksum`]. Conformance tools
    /// (esctest) read cells this way, since a program can't see a cell directly.
    RectChecksum {
        pid: u16,
        top: u16,
        left: u16,
        bottom: u16,
        right: u16,
    },
    /// DECXCPR `CSI ? 6 n` — the DEC-private extended cursor-position report. Like
    /// CPR but private and with a page field: `CSI ? Pl ; Pc ; Pp R`, `Pp` always
    /// 1 (ghost has one page). Distinct from plain CPR (`CSI 6 n`), which omits it.
    ExtendedCursorPosition,
    /// DECDSR printer status `CSI ? 15 n`. Reply `CSI ? 13 n` — no printer.
    PrinterStatus,
    /// DECDSR user-defined-key lock status `CSI ? 25 n`. Reply `CSI ? 21 n` —
    /// locked, since ghost has no UDKs to set (unlocked would promise settable ones).
    UdkStatus,
    /// DECDSR keyboard status `CSI ? 26 n`. Reply `CSI ? 27 ; Pn ; Pst ; Ptyp n` —
    /// North American (1), ready (0), LK201 (0). Four fields to match the VT level
    /// our secondary DA advertises.
    KeyboardStatus,
    /// DECDSR locator status `CSI ? 55 n` (the DEC and xterm forms share the code).
    /// Reply `CSI ? 50 n` — no locator. NB xterm/esctest read 50 as "no locator,"
    /// 53/55 as "available"; DEC's VT420 manual reads them the other way. We follow
    /// xterm (what every modern terminal and the conformance suite expect), so don't
    /// "fix" this against the DEC docs.
    LocatorStatus,
    /// DECDSR locator type `CSI ? 56 n`. Reply `CSI ? 57 ; 0 n` — unknown/no device.
    LocatorType,
    /// DECMSR macro space `CSI ? 62 n`. Reply `CSI 0 * {` — zero bytes available
    /// for macros (ghost supports no DECDMAC).
    MacroSpace,
    /// DECCKSR macro-memory checksum `CSI ? 63 [; Pid] n`. Reply the DCS
    /// `DCS Pid ! ~ 0000 ST` — `Pid` echoed (0 when omitted), checksum 0 (no macros).
    MacroChecksum {
        pid: u16,
    },
    /// DECDSR data-integrity report `CSI ? 75 n`. Reply `CSI ? 70 n` — no errors.
    DataIntegrity,
    /// DECDSR multiple-session status `CSI ? 85 n`. Reply `CSI ? 83 n` — not
    /// configured for multiple sessions (the SSU/TDSMP tech it refers to is dead).
    MultipleSessionStatus,
}

/// The default fg/bg an OSC 10/11 color query is answered with. The attached
/// frontend passes its live scheme; the detached host answers with this
/// `Default` — ghost's default scheme (`ghost-renderer`'s `Theme::default`,
/// duplicated here because the layering points the other way) — until
/// last-attached colors are persisted per session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ThemeColors {
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    /// Cursor color (OSC 12). Ghost paints the cursor with the theme
    /// foreground, so that is the default.
    pub cursor: [u8; 3],
    /// The scheme's 16 base ANSI colors — what an OSC 4 query reports for an
    /// index the app hasn't overridden (above 15, xterm's cube and grey ramp
    /// apply; see [`ghost_term::index_rgb`]).
    ///
    /// Deliberately **not** on the wire: `ThemeColors` is postcard-encoded in the
    /// client's theme report, and postcard is not self-describing, so a new field
    /// would make a new client's report undecodable to an older host — the exact
    /// break `ghost-ui/tests/proto_compat.rs` guards. An attached frontend fills
    /// this in-process (the palette is exact); a *detached* host, which only ever
    /// receives fg/bg/cursor, answers OSC 4 from the standard xterm palette.
    #[serde(skip, default = "default_ansi")]
    pub ansi: [[u8; 3]; 16],
}

/// The serde default for [`ThemeColors::ansi`] (which never crosses the wire).
fn default_ansi() -> [[u8; 3]; 16] {
    ghost_term::ANSI_16
}

impl Default for ThemeColors {
    fn default() -> Self {
        ThemeColors {
            fg: [0xd8, 0xdb, 0xe0],
            bg: [0x10, 0x10, 0x12],
            cursor: [0xd8, 0xdb, 0xe0],
            ansi: ghost_term::ANSI_16,
        }
    }
}

/// The `CSI ? 997 ; Ps n` color-scheme report for `colors` (Ps 1 = dark,
/// 2 = light, by the background's relative luminance) — both the `?996`
/// query's reply and the unsolicited mode-2031 notification.
pub fn color_scheme_report(colors: &ThemeColors) -> Vec<u8> {
    let [r, g, b] = colors.bg;
    let luma = 2126 * u32::from(r) + 7152 * u32::from(g) + 722 * u32::from(b);
    let ps = if luma < 128 * 10_000 { 1 } else { 2 };
    format!("\x1b[?997;{ps}n").into_bytes()
}

/// Everything [`Query::reply`] can draw on, threaded identically by the
/// attached frontend and the detached host.
pub struct ReplyCtx<'a> {
    /// 1-based `(col, row)` cursor position (CPR).
    pub cursor: (u16, u16),
    /// `(cols, rows)` text-area size.
    pub size: (u16, u16),
    /// `(cols, rows)` the display could hold — the size a maximized window's grid
    /// takes, for `CSI 19 t`. A host with no window answers from a nominal display.
    pub display_size: (u16, u16),
    /// Whether the window is iconified (minimized), for `CSI 11 t`.
    pub iconified: bool,
    /// The text area's size in pixels (`CSI 14 t`), the display's (`CSI 5 t`), and
    /// one cell's (`CSI 6 t`) — the three a program needs to resize in pixels.
    pub size_px: (u32, u32),
    pub display_px: (u32, u32),
    pub cell_px: (u32, u32),
    /// The window title (OSC 0/2), for `CSI 21 t`.
    pub title: &'a str,
    /// The icon label (OSC 0/1), for `CSI 20 t`.
    pub icon_title: &'a str,
    /// What the program on this tty is allowed to be told (see
    /// [`ghost_term::policy`]). It lives on the context rather than at the two call
    /// sites — the GUI's and the session host's — so that the answer a program gets
    /// cannot depend on which of them happened to be listening.
    ///
    /// A denied query is never *silenced*, only shaped: an app blocked on a reply
    /// that never comes hangs, which is exactly the stall this whole reply path
    /// exists to prevent.
    pub policy: ghost_term::TerminalPolicy,
    /// Current kitty-keyboard progressive-enhancement flags.
    pub kitty_flags: u8,
    /// The cursor style as a steady DECSCUSR digit (2 block, 4 underline,
    /// 6 bar — see [`decscusr_digit`]), for DECRQSS `" q"`.
    pub cursor_style: u8,
    /// The current left/right scroll margins, 1-based inclusive, for DECRQSS
    /// DECSLRM (selector `s`). Full width `(1, cols)` when DECLRMM is off.
    pub left_right_margins: (u16, u16),
    /// The current top/bottom scroll margins, 1-based inclusive, for DECRQSS
    /// DECSTBM (selector `r`). Full height `(1, rows)` when unset.
    pub top_bottom_margins: (u16, u16),
    /// The current pen as a DECRQSS SGR report body (e.g. `"0;1"`, always led by
    /// a `0` reset), for the selector `m`. See [`ghost_term::Vt::sgr_report`].
    pub sgr_report: String,
    /// The current DECSCA state (0/1) for the DECRQSS `" q` selector. See
    /// [`ghost_term::Vt::decsca_report`].
    pub decsca: u16,
    /// The DECSCL conformance level (1–5); ANSI-mode DECRQM is silent below 3.
    pub conformance_level: u8,
    /// An ANSI (non-private) mode's DECRQM report for `CSI Ps $ p`.
    pub ansi_mode_state: &'a dyn Fn(u16) -> ghost_term::ModeReport,
    /// Default fg/bg for the OSC color queries.
    pub colors: ThemeColors,
    /// The app's OSC 4 override for a palette index, if it set one — what an OSC 4
    /// query must report, since it is what the screen shows. `None` falls back to
    /// the theme (see [`ThemeColors::ansi`]). Fed by [`ghost_term::Vt::palette_color`].
    pub palette: &'a dyn Fn(u8) -> Option<[u8; 3]>,
    /// The app's OSC 5 override for a special color, if it set one. `None` falls
    /// back to the theme foreground. Fed by [`ghost_term::Vt::special_color`].
    pub special: &'a dyn Fn(ghost_term::SpecialColor) -> Option<[u8; 3]>,
    /// A DEC private mode's DECRQM report for `CSI ? Ps $ p`.
    pub mode_state: &'a dyn Fn(u16) -> ghost_term::ModeReport,
    /// The DECRQCRA rectangle checksum over 0-based inclusive `(top, left,
    /// bottom, right)` screen coordinates — [`ghost_term::Vt::rect_checksum`],
    /// threaded in so the pure query layer can read cells without owning a grid.
    pub checksum: &'a dyn Fn(usize, usize, usize, usize) -> u16,
}

/// The 16-bit-per-channel `rgb:rrrr/gggg/bbbb` form xterm reports colors in
/// (each 8-bit channel doubled into 16 bits).
fn xterm_rgb([r, g, b]: [u8; 3]) -> String {
    format!("rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}")
}

/// The display size, in characters, a host with no window answers `CSI 19 t`
/// with — a 1920×1080 display at ghost's default cell. Nothing can be measured
/// while detached, and a program that maximizes needs *some* answer to check its
/// arithmetic against; an attached frontend reports the real monitor.
pub const NOMINAL_DISPLAY_CHARS: (u16, u16) = (213, 51);

/// The same nominal display, in pixels (`CSI 5 t`).
pub const NOMINAL_DISPLAY_PX: (u32, u32) = (1920, 1080);

/// The cell a window with no renderer answers `CSI 6 t` with — ghost's default
/// font at its default size. `NOMINAL_DISPLAY_CHARS` is this cell into that
/// display, so the two reports agree with each other.
pub const NOMINAL_CELL_PX: (u32, u32) = (9, 21);

/// The color numbers an OSC 4/OSC 5 body asks about: the `c` of every `c ; ?`
/// pair, in the order asked, keeping only those `exists` names (the sets and
/// the colors ghost doesn't have are not replied to). One reply per ask keeps
/// an app's reads in step with its questions.
fn asked_colors(body: &str, exists: impl Fn(u16) -> bool) -> Vec<u16> {
    let mut fields = body.split(';');
    let mut asked = Vec::new();
    while let (Some(number), Some(spec)) = (fields.next(), fields.next()) {
        // A color we don't have is dropped on its own — the asks beside it are
        // still answered, as the set path leaves its neighbours alone. Dropping
        // the whole OSC instead would leave an app blocking on a reply it asked
        // for and could have had.
        if spec == "?"
            && let Ok(c) = number.parse::<u16>()
            && exists(c)
        {
            asked.push(c);
        }
    }
    asked
}

/// Whether an OSC 4 index names a color ghost has: one of the 256 indexed
/// colors, or one of the five special colors past them.
fn palette_index_exists(i: u16) -> bool {
    i < ghost_term::SPECIAL_COLOR_BASE
        || ghost_term::SpecialColor::from_code(i - ghost_term::SPECIAL_COLOR_BASE).is_some()
}

/// What a special color (OSC 5's `c`) reports: the app's override, else the
/// theme foreground — ghost paints bold/underline/… text in the pen's own
/// color, so that is the color the screen shows. An unknown code (xterm has
/// five) reads as the foreground too; the scanner does not produce one.
fn special_rgb(ctx: &ReplyCtx, code: u16) -> [u8; 3] {
    ghost_term::SpecialColor::from_code(code)
        .and_then(ctx.special)
        .unwrap_or(ctx.colors.fg)
}

/// The DECRPM `Pm` value for a mode report: 0 unrecognized, 1 set, 2 reset,
/// 3 permanently set, 4 permanently reset.
fn decrpm_pm(report: ghost_term::ModeReport) -> u8 {
    use ghost_term::ModeReport::*;
    match report {
        Set => 1,
        Reset => 2,
        PermanentlySet => 3,
        PermanentlyReset => 4,
        Unrecognized => 0,
    }
}

/// The steady DECSCUSR digit for a cursor shape (blink is not tracked):
/// block 2, underline 4, bar 6.
pub fn decscusr_digit(shape: ghost_term::CursorShape) -> u8 {
    match shape {
        ghost_term::CursorShape::Block => 2,
        ghost_term::CursorShape::Underline => 4,
        ghost_term::CursorShape::Bar => 6,
    }
}

/// Lowercase-hex encode (XTGETTCAP carries names and values hex-encoded).
fn hex(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

/// Decode a hex-encoded XTGETTCAP capability name; `None` on bad hex/UTF-8.
fn unhex(s: &str) -> Option<String> {
    if s.is_empty() || !s.len().is_multiple_of(2) {
        return None;
    }
    let bytes: Option<Vec<u8>> = (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect();
    String::from_utf8(bytes?).ok()
}

/// The value ghost reports for a termcap/terminfo capability name:
/// `Some(Some(v))` = string/number cap, `Some(None)` = true boolean cap,
/// `None` = not a capability we advertise. Kept consistent with the shipped
/// xterm-kitty terminfo entry.
fn termcap_value(name: &str) -> Option<Option<String>> {
    match name {
        "TN" | "name" => Some(Some("xterm-kitty".to_string())),
        "Co" | "colors" => Some(Some("256".to_string())),
        // 24-bit color, both spellings (tmux checks either).
        "RGB" | "Tc" => Some(None),
        _ => None,
    }
}

impl Query {
    /// The reply bytes to write back to the child's PTY.
    ///
    /// DA1 answers with the level DECSCL is *actually* running at (`60 + level`:
    /// 61 VT100 … 65 VT510), then the options ghost really has — 132-column mode
    /// (1), selective erase (6), horizontal scrolling, i.e. left/right margins
    /// (21), colour (22) and rectangular editing (28). Nothing else is claimed:
    /// the printer port (2), national replacement charsets (9), the DEC technical
    /// set (15), the locators (16, 29), terminal state reports (17) and user
    /// windows (18) are all things ghost does not do, and a DA1 reply is a
    /// promise a program is entitled to act on. (This is why esctest's DA tests
    /// still fail: they expect xterm's full option list.)
    ///
    /// DA2 still mirrors VTE's version encoding.
    pub fn reply(&self, ctx: &ReplyCtx) -> Vec<u8> {
        let (col, row) = ctx.cursor;
        let (cols, rows) = ctx.size;
        match self {
            Query::CursorPosition => format!("\x1b[{row};{col}R").into_bytes(),
            Query::DeviceStatus => b"\x1b[0n".to_vec(),
            Query::PrimaryDeviceAttributes => {
                // 61 VT100, 62 VT220, … 65 VT510 — the level DECSCL has us at.
                let level = 60 + u16::from(ctx.conformance_level.clamp(1, 5));
                format!("\x1b[?{level};1;6;21;22;28c").into_bytes()
            }
            Query::SecondaryDeviceAttributes => b"\x1b[>41;420;0c".to_vec(),
            Query::TertiaryDeviceAttributes => b"\x1bP!|00000000\x1b\\".to_vec(),
            Query::TextAreaSize => format!("\x1b[8;{rows};{cols}t").into_bytes(),
            Query::DisplaySize => {
                let (cols, rows) = ctx.display_size;
                format!("\x1b[9;{rows};{cols}t").into_bytes()
            }
            Query::WindowState => {
                let ps = if ctx.iconified { 2 } else { 1 };
                format!("\x1b[{ps}t").into_bytes()
            }
            Query::TextAreaSizePixels => {
                let (w, h) = ctx.size_px;
                format!("\x1b[4;{h};{w}t").into_bytes()
            }
            Query::DisplaySizePixels => {
                let (w, h) = ctx.display_px;
                format!("\x1b[5;{h};{w}t").into_bytes()
            }
            Query::CellSizePixels => {
                let (w, h) = ctx.cell_px;
                format!("\x1b[6;{h};{w}t").into_bytes()
            }
            Query::WindowTitle => {
                let title = if ctx.policy.report_title {
                    ctx.title
                } else {
                    ""
                };
                format!("\x1b]l{title}\x1b\\").into_bytes()
            }
            Query::IconLabel => {
                let icon = if ctx.policy.report_title {
                    ctx.icon_title
                } else {
                    ""
                };
                format!("\x1b]L{icon}\x1b\\").into_bytes()
            }
            Query::KittyKeyboardFlags => format!("\x1b[?{}u", ctx.kitty_flags).into_bytes(),
            Query::TerminalVersion => {
                format!("\x1bP>|ghost {}\x1b\\", env!("CARGO_PKG_VERSION")).into_bytes()
            }
            Query::ReportMode(mode) => {
                let pm = decrpm_pm((ctx.mode_state)(*mode));
                format!("\x1b[?{mode};{pm}$y").into_bytes()
            }
            Query::ReportAnsiMode(mode) => {
                // A VT300+ feature: silent below conformance level 3, which is
                // how a host distinguishes a level-2 terminal.
                if ctx.conformance_level < 3 {
                    return Vec::new();
                }
                let pm = decrpm_pm((ctx.ansi_mode_state)(*mode));
                format!("\x1b[{mode};{pm}$y").into_bytes()
            }
            Query::ForegroundColor => {
                format!("\x1b]10;{}\x1b\\", xterm_rgb(ctx.colors.fg)).into_bytes()
            }
            Query::BackgroundColor => {
                format!("\x1b]11;{}\x1b\\", xterm_rgb(ctx.colors.bg)).into_bytes()
            }
            Query::CursorColor => {
                format!("\x1b]12;{}\x1b\\", xterm_rgb(ctx.colors.cursor)).into_bytes()
            }
            Query::PaletteColors(indices) => {
                let mut out = String::new();
                for &i in indices {
                    let rgb = match u8::try_from(i) {
                        // What the screen shows: the app's override, else the theme's.
                        Ok(i) => (ctx.palette)(i)
                            .unwrap_or_else(|| ghost_term::index_rgb(i, &ctx.colors.ansi)),
                        // Past the palette: a special color, asked for the long way.
                        Err(_) => special_rgb(ctx, i - ghost_term::SPECIAL_COLOR_BASE),
                    };
                    out.push_str(&format!("\x1b]4;{i};{}\x1b\\", xterm_rgb(rgb)));
                }
                out.into_bytes()
            }
            Query::SpecialColors(codes) => {
                let mut out = String::new();
                for &c in codes {
                    out.push_str(&format!(
                        "\x1b]5;{c};{}\x1b\\",
                        xterm_rgb(special_rgb(ctx, c))
                    ));
                }
                out.into_bytes()
            }
            Query::ColorScheme => color_scheme_report(&ctx.colors),
            Query::Termcap(names) => {
                let mut out = String::new();
                for asked in names {
                    // The name goes back exactly as it came (see `Query::Termcap`).
                    match unhex(asked).as_deref().and_then(termcap_value) {
                        Some(Some(value)) => {
                            out.push_str(&format!("\x1bP1+r{asked}={}\x1b\\", hex(&value)));
                        }
                        Some(None) => out.push_str(&format!("\x1bP1+r{asked}\x1b\\")),
                        None => out.push_str(&format!("\x1bP0+r{asked}\x1b\\")),
                    }
                }
                out.into_bytes()
            }
            Query::Setting(selector) => match selector.as_str() {
                " q" => format!("\x1bP1$r{} q\x1b\\", ctx.cursor_style).into_bytes(),
                "s" => {
                    let (l, r) = ctx.left_right_margins;
                    format!("\x1bP1$r{l};{r}s\x1b\\").into_bytes()
                }
                "r" => {
                    let (t, b) = ctx.top_bottom_margins;
                    format!("\x1bP1$r{t};{b}r\x1b\\").into_bytes()
                }
                "m" => format!("\x1bP1$r{}m\x1b\\", ctx.sgr_report).into_bytes(),
                "\"q" => format!("\x1bP1$r{}\"q\x1b\\", ctx.decsca).into_bytes(),
                _ => b"\x1bP0$r\x1b\\".to_vec(),
            },
            Query::RectChecksum {
                pid,
                top,
                left,
                bottom,
                right,
            } => {
                // esctest coordinates are 1-based inclusive; `rect_checksum`
                // wants 0-based. Reply DECCKSR even for a degenerate rect —
                // silence would just stall the querying program for a timeout.
                let z = |v: u16| v.saturating_sub(1) as usize;
                let sum = (ctx.checksum)(z(*top), z(*left), z(*bottom), z(*right));
                deccksr_reply(*pid, sum)
            }
            // DECDSR device-status family. Pure status reports — no window/display
            // facts and nothing an app could probe for secrets, so unlike the title
            // and color replies these are policy-exempt (always answered).
            Query::ExtendedCursorPosition => format!("\x1b[?{row};{col};1R").into_bytes(),
            Query::PrinterStatus => b"\x1b[?13n".to_vec(),
            Query::UdkStatus => b"\x1b[?21n".to_vec(),
            Query::KeyboardStatus => b"\x1b[?27;1;0;0n".to_vec(),
            Query::LocatorStatus => b"\x1b[?50n".to_vec(),
            Query::LocatorType => b"\x1b[?57;0n".to_vec(),
            Query::MacroSpace => b"\x1b[0*{".to_vec(),
            Query::MacroChecksum { pid } => deccksr_reply(*pid, 0),
            Query::DataIntegrity => b"\x1b[?70n".to_vec(),
            Query::MultipleSessionStatus => b"\x1b[?83n".to_vec(),
        }
    }
}

/// The DECCKSR reply envelope `DCS Pid ! ~ HHHH ST`, shared by DECRQCRA (a screen
/// rectangle's checksum) and DECCKSR (a macro-memory checksum, always 0 for us).
fn deccksr_reply(pid: u16, sum: u16) -> Vec<u8> {
    format!("\x1bP{pid}!~{sum:04X}\x1b\\").into_bytes()
}

/// Upper bound on the bytes collected for one CSI sequence; well beyond any query
/// we recognize, so a pathological run of parameters can't grow the buffer.
const MAX_CSI_PARAMS: usize = 32;

/// Upper bound on a collected OSC payload. The OSC queries we answer are small —
/// "11;?", or a handful of OSC 4 index/spec pairs ("4;0;?;1;?", and a set whose
/// specs we skip) — so this is generous while still keeping a title / clipboard /
/// other long string from being collected: it is marked overflowed and never
/// classified, so a truncation can't masquerade as a query.
const MAX_OSC_PAYLOAD: usize = 128;

/// Upper bound on a collected DCS payload. XTGETTCAP requests carry a short
/// hex-encoded cap-name list; anything longer (sixel data, big DECRQSS-style
/// blobs) is marked overflowed and never classified.
const MAX_DCS_PAYLOAD: usize = 128;

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
    /// Inside a DCS string (`ESC P`), collecting a small payload (XTGETTCAP
    /// and DECRQSS live here) until its ST terminator.
    Dcs,
    /// Saw `ESC` inside a DCS string — maybe the start of an ST (`ESC \`).
    DcsEsc,
    /// Inside a SOS/PM/APC string, skipping until its terminator.
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
    /// Payload of the DCS currently being collected (bounded; see
    /// [`MAX_DCS_PAYLOAD`]).
    dcs: Vec<u8>,
    /// The current DCS outgrew [`MAX_DCS_PAYLOAD`] (or carried an embedded
    /// escape): whatever terminates it, it is not a query.
    dcs_overflow: bool,
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
            dcs: Vec::new(),
            dcs_overflow: false,
        }
    }

    /// Classify the just-terminated OSC, if it stayed small enough to be one
    /// of the queries we answer.
    fn osc_query(&self) -> Vec<Query> {
        if self.osc_overflow {
            return Vec::new();
        }
        let Ok(osc) = std::str::from_utf8(&self.osc) else {
            return Vec::new();
        };
        let Some((ps, rest)) = osc.split_once(';') else {
            return Vec::new();
        };
        match ps {
            // OSC 10/11/12, including xterm's consecutive-code form: each spec
            // after the first asks about the *next* color, so `10;?;?` reports the
            // foreground and the background. A spec that isn't `?` is a set, which
            // the emulator applies — but it still advances the code.
            "10" | "11" | "12" => {
                let first = match ps {
                    "10" => 0,
                    "11" => 1,
                    _ => 2,
                };
                rest.split(';')
                    .enumerate()
                    .filter(|(_, spec)| *spec == "?")
                    .map_while(|(i, _)| match first + i {
                        0 => Some(Query::ForegroundColor),
                        1 => Some(Query::BackgroundColor),
                        2 => Some(Query::CursorColor),
                        // Codes past the cursor name colors ghost doesn't have.
                        _ => None,
                    })
                    .collect()
            }
            // OSC 4 ; c ; spec [; c ; spec]… — only the `?` pairs are queries; the
            // rest are sets the emulator applies (and are skipped here). One reply
            // per index asked, so they stay in step with the app's reads.
            // An index past the palette names a special color, which is answered in
            // the OSC 4 form it was asked in (`Query::PaletteColors`).
            "4" => match asked_colors(rest, palette_index_exists) {
                asked if asked.is_empty() => Vec::new(),
                asked => vec![Query::PaletteColors(asked)],
            },
            // OSC 5 ; c ; spec … — the same colors named from 0, answered as OSC 5.
            "5" => match asked_colors(rest, |c| ghost_term::SpecialColor::from_code(c).is_some()) {
                asked if asked.is_empty() => Vec::new(),
                asked => vec![Query::SpecialColors(asked)],
            },
            _ => Vec::new(),
        }
    }

    /// Classify the just-terminated DCS, if it stayed small enough to be one
    /// of the queries we answer (XTGETTCAP `+q`, DECRQSS `$q`).
    fn dcs_query(&self) -> Option<Query> {
        if self.dcs_overflow {
            return None;
        }
        let s = std::str::from_utf8(&self.dcs).ok()?;
        if let Some(names) = s.strip_prefix("+q") {
            // Kept hex-encoded, as asked — `unhex` only vets that it *is* a name.
            let names: Vec<String> = names
                .split(';')
                .filter(|name| unhex(name).is_some())
                .map(str::to_string)
                .collect();
            return (!names.is_empty()).then_some(Query::Termcap(names));
        }
        s.strip_prefix("$q")
            .map(|sel| Query::Setting(sel.to_string()))
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
                    // DCS: collected (XTGETTCAP/DECRQSS ride in it).
                    b'P' => {
                        self.dcs.clear();
                        self.dcs_overflow = false;
                        self.state = State::Dcs;
                    }
                    // DECID: the old spelling of DA1, and answered as one — so it
                    // cannot drift from it, nor slip past the policy shaping it.
                    b'Z' => {
                        out.push(Query::PrimaryDeviceAttributes);
                        self.state = State::Ground;
                    }
                    // SOS, PM, APC: string controls skipped until their ST.
                    b'X' | b'^' | b'_' => self.state = State::Str,
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
                        out.extend(self.osc_query());
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
                        out.extend(self.osc_query());
                        self.state = State::Ground;
                    } else {
                        self.osc_overflow = true;
                        self.state = State::Osc;
                    }
                }
                State::Dcs => match b {
                    0x1b => self.state = State::DcsEsc,
                    _ if self.dcs.len() < MAX_DCS_PAYLOAD => self.dcs.push(b),
                    _ => self.dcs_overflow = true,
                },
                State::DcsEsc => {
                    // `ESC \` is ST; any other byte is still DCS payload (and
                    // an embedded escape disqualifies it as a query).
                    if b == b'\\' {
                        if let Some(q) = self.dcs_query() {
                            out.push(q);
                        }
                        self.state = State::Ground;
                    } else {
                        self.dcs_overflow = true;
                        self.state = State::Dcs;
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
        // DSR — device status report. The two ANSI forms, then the `?`-private DEC
        // variants (DECDSR + the color-scheme request), classified by `classify_dsr`.
        b'n' => match params {
            b"6" => Some(Query::CursorPosition),
            b"5" => Some(Query::DeviceStatus),
            _ => params.strip_prefix(b"?").and_then(classify_dsr),
        },
        // DA — device attributes. `>` marks the secondary request, `=` the
        // tertiary (unit id).
        b'c' => match params {
            b"" | b"0" => Some(Query::PrimaryDeviceAttributes),
            b">" | b">0" => Some(Query::SecondaryDeviceAttributes),
            b"=" | b"=0" => Some(Query::TertiaryDeviceAttributes),
            _ => None,
        },
        // Window ops: the ones that *ask* something. The ones that change the
        // window are the emulator's and the frontend's (`ghost_term::XtwinopsOp`).
        b't' => match params {
            b"11" => Some(Query::WindowState),
            // `14` is the text area, `14;2` the whole window. Ghost draws no
            // decorations of its own, so the two are the same pixels.
            b"14" | b"14;2" => Some(Query::TextAreaSizePixels),
            b"15" => Some(Query::DisplaySizePixels),
            b"16" => Some(Query::CellSizePixels),
            b"18" => Some(Query::TextAreaSize),
            b"19" => Some(Query::DisplaySize),
            b"20" => Some(Query::IconLabel),
            b"21" => Some(Query::WindowTitle),
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
            let body = params.strip_suffix(b"$")?;
            match body.strip_prefix(b"?") {
                // `CSI ? Ps $ p` — DEC-private DECRQM.
                Some(mode) => std::str::from_utf8(mode)
                    .ok()?
                    .parse()
                    .ok()
                    .map(Query::ReportMode),
                // `CSI Ps $ p` — ANSI-mode DECRQM (`CSI ! p` DECSTR has no `$`
                // and never reaches here).
                None => std::str::from_utf8(body)
                    .ok()?
                    .parse()
                    .ok()
                    .map(Query::ReportAnsiMode),
            }
        }
        // DECRQCRA: `CSI Pid ; Pp ; Pt ; Pl ; Pb ; Pr * y`. The `*` intermediate
        // is appended to the params (as `$` is for DECRQM). `Pid` leads; the
        // rectangle is the last four fields (so an omitted page still parses).
        b'y' => {
            let body = std::str::from_utf8(params.strip_suffix(b"*")?).ok()?;
            let fields: Vec<&str> = body.split(';').collect();
            if fields.len() < 5 {
                return None;
            }
            let n = fields.len();
            Some(Query::RectChecksum {
                pid: fields[0].parse().ok()?,
                top: fields[n - 4].parse().ok()?,
                left: fields[n - 3].parse().ok()?,
                bottom: fields[n - 2].parse().ok()?,
                right: fields[n - 1].parse().ok()?,
            })
        }
        _ => None,
    }
}

/// Classify a DEC-private DSR (`CSI ? Ps n`) from its body (the `?` already
/// stripped): the DECDSR device-status family and the color-scheme request. Exact
/// byte-matches only, so a stray parameter (`?6;1`, `?06`) stays unanswered.
fn classify_dsr(body: &[u8]) -> Option<Query> {
    // DECCKSR carries an optional request id: `CSI ? 63 [; Pid] n` (Pid 0 omitted).
    if let Some(rest) = body.strip_prefix(b"63") {
        let pid = match rest {
            b"" => 0,
            _ => std::str::from_utf8(rest.strip_prefix(b";")?)
                .ok()?
                .parse()
                .ok()?,
        };
        return Some(Query::MacroChecksum { pid });
    }
    match body {
        b"6" => Some(Query::ExtendedCursorPosition),
        b"15" => Some(Query::PrinterStatus),
        b"25" => Some(Query::UdkStatus),
        b"26" => Some(Query::KeyboardStatus),
        b"55" => Some(Query::LocatorStatus),
        b"56" => Some(Query::LocatorType),
        b"62" => Some(Query::MacroSpace),
        b"75" => Some(Query::DataIntegrity),
        b"85" => Some(Query::MultipleSessionStatus),
        b"996" => Some(Query::ColorScheme),
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
        assert_eq!(scan_all(b"\x1b[=c"), [Query::TertiaryDeviceAttributes]);
        assert_eq!(scan_all(b"\x1b[=0c"), [Query::TertiaryDeviceAttributes]);
        assert_eq!(scan_all(b"\x1b[18t"), [Query::TextAreaSize]);
        assert_eq!(scan_all(b"\x1b[?u"), [Query::KittyKeyboardFlags]);
        assert_eq!(scan_all(b"\x1b[>q"), [Query::TerminalVersion]);
        assert_eq!(scan_all(b"\x1b[>0q"), [Query::TerminalVersion]);
        assert_eq!(scan_all(b"\x1b[?2026$p"), [Query::ReportMode(2026)]);
        assert_eq!(scan_all(b"\x1b[?25$p"), [Query::ReportMode(25)]);
        // DECRQCRA: pid ; page ; top ; left ; bottom ; right * y.
        assert_eq!(
            scan_all(b"\x1b[1;0;1;1;25;80*y"),
            [Query::RectChecksum {
                pid: 1,
                top: 1,
                left: 1,
                bottom: 25,
                right: 80,
            }]
        );
        // A bare `CSI ... y` without the `*` intermediate is not DECRQCRA.
        assert!(scan_all(b"\x1b[1;1;1;1y").is_empty());
    }

    #[test]
    fn recognizes_dec_dsr_queries() {
        assert_eq!(scan_all(b"\x1b[?6n"), [Query::ExtendedCursorPosition]);
        assert_eq!(scan_all(b"\x1b[?15n"), [Query::PrinterStatus]);
        assert_eq!(scan_all(b"\x1b[?25n"), [Query::UdkStatus]);
        assert_eq!(scan_all(b"\x1b[?26n"), [Query::KeyboardStatus]);
        assert_eq!(scan_all(b"\x1b[?55n"), [Query::LocatorStatus]);
        assert_eq!(scan_all(b"\x1b[?56n"), [Query::LocatorType]);
        assert_eq!(scan_all(b"\x1b[?62n"), [Query::MacroSpace]);
        assert_eq!(scan_all(b"\x1b[?75n"), [Query::DataIntegrity]);
        assert_eq!(scan_all(b"\x1b[?85n"), [Query::MultipleSessionStatus]);
        assert_eq!(scan_all(b"\x1b[?996n"), [Query::ColorScheme]);
        // DECCKSR carries a request id, and defaults it to 0 when omitted.
        assert_eq!(
            scan_all(b"\x1b[?63;123n"),
            [Query::MacroChecksum { pid: 123 }]
        );
        assert_eq!(scan_all(b"\x1b[?63n"), [Query::MacroChecksum { pid: 0 }]);
    }

    #[test]
    fn dec_dsr_replies_match_esctest() {
        // DECXCPR mirrors CPR's row;col order and adds the page (always 1). Cursor
        // here is (col 5, row 6), as esctest's CUP(Point(5,6)) leaves it.
        let at_5_6 = ReplyCtx {
            cursor: (5, 6),
            ..ctx()
        };
        assert_eq!(
            Query::ExtendedCursorPosition.reply(&at_5_6),
            b"\x1b[?6;5;1R"
        );
        assert_eq!(Query::PrinterStatus.reply(&ctx()), b"\x1b[?13n");
        assert_eq!(Query::UdkStatus.reply(&ctx()), b"\x1b[?21n");
        assert_eq!(Query::KeyboardStatus.reply(&ctx()), b"\x1b[?27;1;0;0n");
        assert_eq!(Query::LocatorStatus.reply(&ctx()), b"\x1b[?50n");
        assert_eq!(Query::LocatorType.reply(&ctx()), b"\x1b[?57;0n");
        assert_eq!(Query::MacroSpace.reply(&ctx()), b"\x1b[0*{");
        assert_eq!(Query::DataIntegrity.reply(&ctx()), b"\x1b[?70n");
        assert_eq!(Query::MultipleSessionStatus.reply(&ctx()), b"\x1b[?83n");
        // DECCKSR: DCS Pid ! ~ 0000 ST, Pid echoed.
        assert_eq!(
            Query::MacroChecksum { pid: 123 }.reply(&ctx()),
            b"\x1bP123!~0000\x1b\\"
        );
    }

    #[test]
    fn rect_checksum_reply_is_deccksr() {
        // esctest coords are 1-based; the closure receives 0-based (here echoed
        // as 1;2;1;2 -> 1212 = 0x04BC). Pid echoes back, 4 uppercase hex in a
        // DCS ! ~ ... ST envelope.
        let reply = Query::RectChecksum {
            pid: 7,
            top: 2,
            left: 3,
            bottom: 2,
            right: 3,
        }
        .reply(&ctx());
        assert_eq!(reply, b"\x1bP7!~04BC\x1b\\");
    }

    #[test]
    fn ignores_non_queries() {
        assert!(scan_all(b"hello world").is_empty());
        assert!(scan_all(b"\x1b[31m\x1b[2J\x1b[H").is_empty()); // SGR, clear, home
        assert!(scan_all(b"\x1b[8;24;80t").is_empty()); // a resize op, not a query
        assert!(scan_all(b"\x1b[?6;1n").is_empty()); // a stray param defeats DECXCPR
        assert!(scan_all(b"\x1b[?99n").is_empty()); // an unknown DEC-private DSR
        assert!(scan_all(b"\x1b[u").is_empty()); // bare CSI u is SCO restore-cursor
        assert!(scan_all(b"\x1b[0 q").is_empty()); // DECSCUSR shares `q`, not a query
        assert!(scan_all(b"\x1b[!p").is_empty()); // DECSTR shares `p`, not a query
        // ANSI-mode DECRQM `CSI Ps $ p` (no `?`) IS a query now (level-gated reply).
        assert_eq!(scan_all(b"\x1b[2026$p"), [Query::ReportAnsiMode(2026)]);
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
    fn no_modes(_: u16) -> ghost_term::ModeReport {
        ghost_term::ModeReport::Unrecognized
    }

    /// A `checksum` for tests: encodes the requested rect so assertions can see
    /// the coordinates the reply was built from.
    fn echo_rect(top: usize, left: usize, bottom: usize, right: usize) -> u16 {
        (top * 1000 + left * 100 + bottom * 10 + right) as u16
    }

    /// A `palette` for tests: the app has overridden nothing.
    fn no_palette(_: u8) -> Option<[u8; 3]> {
        None
    }

    /// A `special` for tests: the app has overridden nothing.
    fn no_special(_: ghost_term::SpecialColor) -> Option<[u8; 3]> {
        None
    }

    /// A baseline reply context; tests override the fields they exercise.
    fn ctx() -> ReplyCtx<'static> {
        ReplyCtx {
            cursor: (1, 1),
            size: (80, 24),
            display_size: NOMINAL_DISPLAY_CHARS,
            iconified: false,
            size_px: (720, 432),
            display_px: NOMINAL_DISPLAY_PX,
            cell_px: NOMINAL_CELL_PX,
            title: "",
            icon_title: "",
            policy: ghost_term::TerminalPolicy::default(),
            kitty_flags: 0,
            cursor_style: 2,
            left_right_margins: (1, 80),
            top_bottom_margins: (1, 24),
            sgr_report: "0".to_owned(),
            decsca: 0,
            conformance_level: 5,
            ansi_mode_state: &no_modes,
            colors: ThemeColors::default(),
            palette: &no_palette,
            special: &no_special,
            mode_state: &no_modes,
            checksum: &echo_rect,
        }
    }

    #[test]
    fn recognizes_osc_color_queries() {
        assert_eq!(scan_all(b"\x1b]10;?\x07"), [Query::ForegroundColor]);
        assert_eq!(scan_all(b"\x1b]11;?\x1b\\"), [Query::BackgroundColor]);
        assert_eq!(scan_all(b"\x1b]12;?\x07"), [Query::CursorColor]);
        // xterm's consecutive-code form: each further spec asks about the next
        // color, so one OSC can ask for two — and gets two replies.
        assert_eq!(
            scan_all(b"\x1b]10;?;?\x07"),
            [Query::ForegroundColor, Query::BackgroundColor]
        );
        assert_eq!(
            scan_all(b"\x1b]11;?;?\x07"),
            [Query::BackgroundColor, Query::CursorColor]
        );
        // Set/reset forms and unrelated OSCs are not queries.
        assert!(scan_all(b"\x1b]11;#101012\x07").is_empty());
        assert!(scan_all(b"\x1b]112\x07").is_empty());
        assert!(scan_all(b"\x1b]0;title\x07").is_empty());
        // A long payload overflows the small buffer and is never classified,
        // even if its prefix looks like a query.
        let mut long = b"\x1b]10;?".to_vec();
        long.extend(std::iter::repeat_n(b'x', 2 * MAX_OSC_PAYLOAD));
        long.push(0x07);
        assert!(scan_all(&long).is_empty());
        // Split across feeds still recognized.
        let mut s = QueryScanner::new();
        assert!(s.scan(b"\x1b]11").is_empty());
        assert!(s.scan(b";?").is_empty());
        assert_eq!(s.scan(b"\x07"), [Query::BackgroundColor]);
    }

    /// DA1's leading number is the terminal's *level*, and DECSCL is what sets it
    /// (`CSI Ps " p`) — so the two have to tell a program the same story. A
    /// terminal that runs at VT420 and answers "VT100" gets treated as a VT100.
    #[test]
    fn primary_device_attributes_follow_the_conformance_level() {
        let at = |level: u8| {
            Query::PrimaryDeviceAttributes.reply(&ReplyCtx {
                conformance_level: level,
                ..ctx()
            })
        };
        assert_eq!(at(1), b"\x1b[?61;1;6;21;22;28c"); // VT100
        assert_eq!(at(4), b"\x1b[?64;1;6;21;22;28c"); // VT420
        assert_eq!(at(5), b"\x1b[?65;1;6;21;22;28c"); // VT510 (ghost's default)
    }

    /// DECID is the VT52-era spelling of DA1 and must answer the same thing —
    /// including being shaped by the same policy.
    #[test]
    fn decid_is_answered_as_primary_device_attributes() {
        let mut s = QueryScanner::default();
        assert_eq!(s.scan(b"\x1bZ"), [Query::PrimaryDeviceAttributes]);
        // Split across two reads, as a PTY is free to deliver it.
        let mut s = QueryScanner::default();
        assert_eq!(s.scan(b"\x1b"), []);
        assert_eq!(s.scan(b"Z"), [Query::PrimaryDeviceAttributes]);
    }

    #[test]
    fn common_reply_strings() {
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
            b"\x1b[?65;1;6;21;22;28c"
        );
        // Secondary DA presents an xterm-emulating VT420 (see the variant docs).
        assert_eq!(
            Query::SecondaryDeviceAttributes.reply(&ctx()),
            b"\x1b[>41;420;0c"
        );
        // DA3: DECRPTUI with xterm's all-zero site/serial unit id.
        assert_eq!(
            Query::TertiaryDeviceAttributes.reply(&ctx()),
            b"\x1bP!|00000000\x1b\\"
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
                ..ThemeColors::default()
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
    fn recognizes_osc_4_palette_queries() {
        assert_eq!(
            scan_all(b"\x1b]4;1;?\x07"),
            [Query::PaletteColors(vec![1])],
            "a single index"
        );
        assert_eq!(
            scan_all(b"\x1b]4;0;?;255;?\x1b\\"),
            [Query::PaletteColors(vec![0, 255])],
            "one OSC may ask for several"
        );
        // A set is not a query, and a mixed OSC asks only for its `?` pairs.
        assert!(scan_all(b"\x1b]4;1;rgb:ffff/0000/0000\x07").is_empty());
        assert_eq!(
            scan_all(b"\x1b]4;1;#ff0000;2;?\x07"),
            [Query::PaletteColors(vec![2])]
        );
        // OSC 104 (reset) is not a query either.
        assert!(scan_all(b"\x1b]104;1\x07").is_empty());
    }

    #[test]
    fn palette_replies_report_the_override_then_the_theme() {
        // Index 1 is the app's (OSC 4), index 2 the scheme's, and index 196 comes
        // from xterm's cube — which no scheme carries.
        let overridden = |i: u8| (i == 1).then_some([0xff, 0x00, 0x00]);
        let mut ansi = ghost_term::ANSI_16;
        ansi[2] = [0x00, 0x11, 0x22];
        let cx = ReplyCtx {
            colors: ThemeColors {
                ansi,
                ..ThemeColors::default()
            },
            palette: &overridden,
            ..ctx()
        };
        assert_eq!(
            Query::PaletteColors(vec![1]).reply(&cx),
            b"\x1b]4;1;rgb:ffff/0000/0000\x1b\\"
        );
        assert_eq!(
            Query::PaletteColors(vec![2]).reply(&cx),
            b"\x1b]4;2;rgb:0000/1111/2222\x1b\\"
        );
        assert_eq!(
            Query::PaletteColors(vec![196]).reply(&cx),
            b"\x1b]4;196;rgb:ffff/0000/0000\x1b\\"
        );
        // Several indices in one query get one reply each, in the order asked.
        assert_eq!(
            Query::PaletteColors(vec![2, 1]).reply(&cx),
            b"\x1b]4;2;rgb:0000/1111/2222\x1b\\\x1b]4;1;rgb:ffff/0000/0000\x1b\\"
        );
    }

    #[test]
    fn recognizes_special_color_queries_in_both_forms() {
        // OSC 5 names the special colors from 0; OSC 4 names the same ones past
        // the 256 indexed colors, and answers in the form it was asked in.
        assert_eq!(
            scan_all(b"\x1b]5;0;?\x07"),
            [Query::SpecialColors(vec![0])],
            "OSC 5, bold"
        );
        assert_eq!(
            scan_all(b"\x1b]5;0;?;1;?\x1b\\"),
            [Query::SpecialColors(vec![0, 1])],
            "one OSC may ask for several"
        );
        assert_eq!(
            scan_all(b"\x1b]4;256;?\x07"),
            [Query::PaletteColors(vec![256])],
            "the OSC 4 form of the same color"
        );
        // Sets and resets are not queries.
        assert!(scan_all(b"\x1b]5;0;#ff0000\x07").is_empty());
        assert!(scan_all(b"\x1b]105;0\x07").is_empty());
    }

    #[test]
    fn window_state_and_display_size_answer_the_window_op_reports() {
        assert_eq!(scan_all(b"\x1b[11t"), [Query::WindowState]);
        assert_eq!(scan_all(b"\x1b[19t"), [Query::DisplaySize]);
        // The ops that *change* the window are not queries — the emulator has them.
        assert!(scan_all(b"\x1b[2t").is_empty());
        assert!(scan_all(b"\x1b[9;1t").is_empty());

        let open = ReplyCtx {
            display_size: (200, 60),
            ..ctx()
        };
        assert_eq!(Query::WindowState.reply(&open), b"\x1b[1t");
        assert_eq!(Query::DisplaySize.reply(&open), b"\x1b[9;60;200t");

        let iconified = ReplyCtx {
            iconified: true,
            ..ctx()
        };
        assert_eq!(Query::WindowState.reply(&iconified), b"\x1b[2t");
    }

    #[test]
    fn an_ask_for_a_color_we_do_not_have_does_not_swallow_the_asks_beside_it() {
        // The app is blocked on one reply per `?` it sent; dropping the whole OSC
        // over an index we can't answer would hang it on the ones we can.
        assert_eq!(
            scan_all(b"\x1b]4;1;?;9999;?;2;?\x07"),
            [Query::PaletteColors(vec![1, 2])]
        );
        assert_eq!(
            scan_all(b"\x1b]5;0;?;7;?\x07"),
            [Query::SpecialColors(vec![0])]
        );
        // An OSC that asks only for colors we don't have is answered by silence.
        assert!(scan_all(b"\x1b]5;7;?\x07").is_empty());
    }

    #[test]
    fn special_color_replies_report_the_override_then_the_theme() {
        let bold_is_red = |t: ghost_term::SpecialColor| {
            (t == ghost_term::SpecialColor::Bold).then_some([0xff, 0x00, 0x00])
        };
        let cx = ReplyCtx {
            special: &bold_is_red,
            ..ctx()
        };
        // Asked as OSC 5, answered as OSC 5 — and an unset one falls back to the
        // theme foreground, which is what ghost paints the text with.
        assert_eq!(
            Query::SpecialColors(vec![0]).reply(&cx),
            b"\x1b]5;0;rgb:ffff/0000/0000\x1b\\"
        );
        assert_eq!(
            Query::SpecialColors(vec![4]).reply(&cx),
            b"\x1b]5;4;rgb:d8d8/dbdb/e0e0\x1b\\"
        );
        // Asked as OSC 4, answered as OSC 4 — same color, the index it was asked by.
        assert_eq!(
            Query::PaletteColors(vec![256]).reply(&cx),
            b"\x1b]4;256;rgb:ffff/0000/0000\x1b\\"
        );
    }

    #[test]
    fn xtgettcap_echoes_the_name_as_it_was_asked() {
        // xterm answers with the name the client sent, hex for hex — esctest asks
        // for `Co` as uppercase "436F" and string-matches the echo, so lowercasing
        // it loses the reply.
        assert_eq!(
            Query::Termcap(vec!["436F".into()]).reply(&ctx()),
            b"\x1bP1+r436F=323536\x1b\\"
        );
        assert_eq!(
            Query::Termcap(vec!["436f".into()]).reply(&ctx()),
            b"\x1bP1+r436f=323536\x1b\\"
        );
    }

    #[test]
    fn cursor_color_reply_uses_the_theme_cursor() {
        let themed = ReplyCtx {
            colors: ThemeColors {
                cursor: [0xff, 0x00, 0x00],
                ..ThemeColors::default()
            },
            ..ctx()
        };
        assert_eq!(
            Query::CursorColor.reply(&themed),
            b"\x1b]12;rgb:ffff/0000/0000\x1b\\"
        );
    }

    #[test]
    fn color_scheme_query_reports_dark_or_light() {
        assert_eq!(scan_all(b"\x1b[?996n"), [Query::ColorScheme]);
        // Ghost's default scheme is dark…
        assert_eq!(Query::ColorScheme.reply(&ctx()), b"\x1b[?997;1n");
        // …a white background reports light.
        let light = ReplyCtx {
            colors: ThemeColors {
                bg: [0xff, 0xff, 0xff],
                ..ThemeColors::default()
            },
            ..ctx()
        };
        assert_eq!(Query::ColorScheme.reply(&light), b"\x1b[?997;2n");
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
    fn xtgettcap_answers_per_cap_with_hex_encoding() {
        // "TN"=544e (name string), "RGB"=524742 (boolean), "Co"=436f
        // (number), "XX"=5858 (unknown). One DCS reply per cap, kitty-style.
        let qs = scan_all(b"\x1bP+q544e;524742;436f;5858\x1b\\");
        assert_eq!(qs.len(), 1);
        let reply = String::from_utf8(qs[0].reply(&ctx())).unwrap();
        assert_eq!(
            reply,
            concat!(
                "\x1bP1+r544e=787465726d2d6b69747479\x1b\\", // TN=xterm-kitty
                "\x1bP1+r524742\x1b\\",                      // RGB (boolean true)
                "\x1bP1+r436f=323536\x1b\\",                 // Co=256
                "\x1bP0+r5858\x1b\\",                        // XX: unknown
            )
        );
        // Split across feeds still recognized; garbage hex is dropped.
        let mut s = QueryScanner::new();
        assert!(s.scan(b"\x1bP+q54").is_empty());
        assert_eq!(s.scan(b"63\x1b\\").len(), 1); // "Tc"
        assert!(scan_all(b"\x1bP+qZZ\x1b\\").is_empty());
    }

    #[test]
    fn decrqss_reports_the_cursor_style_and_rejects_the_rest() {
        // DECRQSS for DECSCUSR (selector " q") answers the current style.
        let qs = scan_all(b"\x1bP$q q\x1b\\");
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].reply(&ctx()), b"\x1bP1$r2 q\x1b\\");
        let bar = ReplyCtx {
            cursor_style: 6,
            ..ctx()
        };
        assert_eq!(qs[0].reply(&bar), b"\x1bP1$r6 q\x1b\\");
        // Settings we do not report get the well-formed invalid reply
        // (validity 0), not silence — the prober can move on immediately.
        // (`t` is DECSLPP, which ghost does not report.)
        let qs = scan_all(b"\x1bP$qt\x1b\\");
        assert_eq!(qs[0].reply(&ctx()), b"\x1bP0$r\x1b\\");
    }

    #[test]
    fn decrqss_reports_decsca() {
        // DECRQSS for DECSCA (selector `"q`) reports the current protection bit.
        let qs = scan_all(b"\x1bP$q\"q\x1b\\");
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].reply(&ctx()), b"\x1bP1$r0\"q\x1b\\");
        let protected = ReplyCtx { decsca: 1, ..ctx() };
        assert_eq!(qs[0].reply(&protected), b"\x1bP1$r1\"q\x1b\\");
    }

    #[test]
    fn decrqss_reports_the_sgr() {
        // DECRQSS for SGR (selector "m") echoes the current pen as a param list,
        // always led by a `0` reset — matching what a `Sgr` dump would emit.
        let qs = scan_all(b"\x1bP$qm\x1b\\");
        assert_eq!(qs.len(), 1);
        let bold = ReplyCtx {
            sgr_report: "0;1".to_owned(),
            ..ctx()
        };
        assert_eq!(qs[0].reply(&bold), b"\x1bP1$r0;1m\x1b\\");
    }

    #[test]
    fn decrqss_reports_the_left_right_margins() {
        // DECRQSS for DECSLRM (selector "s") answers the current margins, 1-based.
        let qs = scan_all(b"\x1bP$qs\x1b\\");
        assert_eq!(qs.len(), 1);
        let margined = ReplyCtx {
            left_right_margins: (3, 4),
            ..ctx()
        };
        assert_eq!(qs[0].reply(&margined), b"\x1bP1$r3;4s\x1b\\");
    }

    #[test]
    fn decrqss_reports_the_top_bottom_margins() {
        // DECRQSS for DECSTBM (selector "r") answers the current margins, 1-based.
        let qs = scan_all(b"\x1bP$qr\x1b\\");
        assert_eq!(qs.len(), 1);
        let margined = ReplyCtx {
            top_bottom_margins: (5, 6),
            ..ctx()
        };
        assert_eq!(qs[0].reply(&margined), b"\x1bP1$r5;6r\x1b\\");
    }

    #[test]
    fn decrqm_reply_reports_mode_state() {
        // DECRPM Pm: 1 = set, 2 = reset, 4 = permanently reset, 0 = unrecognized.
        use ghost_term::ModeReport::*;
        let state = |m: u16| match m {
            2026 => Set,
            2004 => Reset,
            60 => PermanentlyReset,
            _ => Unrecognized,
        };
        let modal = ReplyCtx {
            mode_state: &state,
            ..ctx()
        };
        assert_eq!(Query::ReportMode(2026).reply(&modal), b"\x1b[?2026;1$y");
        assert_eq!(Query::ReportMode(2004).reply(&modal), b"\x1b[?2004;2$y");
        assert_eq!(Query::ReportMode(60).reply(&modal), b"\x1b[?60;4$y");
        assert_eq!(Query::ReportMode(12345).reply(&modal), b"\x1b[?12345;0$y");
    }

    #[test]
    fn a_denied_title_report_is_answered_empty_not_left_unanswered() {
        // Reading the title back is the classic reflection trick: a program sets a
        // title with a command in it, reads it back, and the answer arrives on the
        // shell's stdin looking exactly like the user typed it. xterm turns this
        // off by default. But a *denied* query must still be ANSWERED — an app that
        // blocks on a reply hangs forever — so the shape is right and the title is
        // simply nothing.
        let told = ReplyCtx {
            title: "rm -rf ~",
            icon_title: "rm -rf ~",
            // The default policy now denies the read-back, so the allowed case
            // has to ask for it explicitly.
            policy: ghost_term::TerminalPolicy::allow_all(),
            ..ctx()
        };
        assert_eq!(Query::WindowTitle.reply(&told), b"\x1b]lrm -rf ~\x1b\\");

        let denied = ReplyCtx {
            title: "rm -rf ~",
            icon_title: "rm -rf ~",
            policy: ghost_term::TerminalPolicy {
                report_title: false,
                ..Default::default()
            },
            ..ctx()
        };
        assert_eq!(Query::WindowTitle.reply(&denied), b"\x1b]l\x1b\\");
        assert_eq!(Query::IconLabel.reply(&denied), b"\x1b]L\x1b\\");
    }

    #[test]
    fn ansi_decrqm_is_gated_by_conformance_level() {
        // ANSI-mode DECRQM `CSI Ps $ p` (no `?`) is a VT300+ feature: answered
        // only at conformance level >= 3, silent below.
        assert_eq!(scan_all(b"\x1b[4$p"), [Query::ReportAnsiMode(4)]);
        use ghost_term::ModeReport::*;
        let ansi = |m: u16| match m {
            4 => Set,
            _ => Unrecognized,
        };
        let l4 = ReplyCtx {
            conformance_level: 4,
            ansi_mode_state: &ansi,
            ..ctx()
        };
        assert_eq!(Query::ReportAnsiMode(4).reply(&l4), b"\x1b[4;1$y");
        assert_eq!(Query::ReportAnsiMode(20).reply(&l4), b"\x1b[20;0$y");
        let l2 = ReplyCtx {
            conformance_level: 2,
            ansi_mode_state: &ansi,
            ..ctx()
        };
        assert!(
            Query::ReportAnsiMode(4).reply(&l2).is_empty(),
            "silent below level 3"
        );
    }
}
