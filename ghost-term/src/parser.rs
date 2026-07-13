// Based on Paul Williams' parser for ANSI-compatible video terminals:
// https://www.vt100.net/emu/dec_ansi_parser

use crate::charset::Charset;
use crate::color::Color;
use std::fmt::Display;

const PARAMS_LEN: usize = 32;

#[derive(Debug, Default)]
pub struct Parser {
    pub state: State,
    params: [Param; PARAMS_LEN],
    cur_param: usize,
    intermediate: Option<char>,
    /// Accumulates the body of the current OSC string (between OSC start and
    /// the terminator). Only OSC 0/2 (window title) is acted upon; other OSCs
    /// are collected then discarded.
    osc: String,
    /// Accumulates the body of the current APC string (kitty graphics carrier).
    /// Bounded by [`MAX_APC_LEN`]; once exceeded, `apc_overflow` latches and the
    /// whole sequence is discarded on dispatch.
    apc: String,
    apc_overflow: bool,
}

/// Upper bound on a single APC string's accumulated length (a DoS guard). The
/// kitty protocol chunks large transfers (~4 KiB of base64 per chunk), so a
/// well-behaved client never approaches this; a non-chunked transfer above it is
/// dropped rather than buffered unboundedly.
pub(crate) const MAX_APC_LEN: usize = 4 * 1024 * 1024;

#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub enum State {
    #[default]
    Ground,
    Escape,
    EscapeIntermediate,
    CsiEntry,
    CsiParam,
    CsiIntermediate,
    CsiIgnore,
    DcsEntry,
    DcsParam,
    DcsIntermediate,
    DcsPassthrough,
    DcsIgnore,
    OscString,
    SosPmApcString,
    /// Inside an APC string (`ESC _ …`), accumulating the kitty graphics payload.
    ApcString,
}

#[derive(Debug, PartialEq)]
pub enum Function {
    /// BEL (0x07) seen in the ground state: ring the terminal bell. Ephemeral —
    /// not part of the screen state, so it never appears in a state dump.
    Bell,
    Bs,
    Cbt(u16),
    Cha(u16),
    Cht(u16),
    Cnl(u16),
    Cpl(u16),
    Cr,
    Ctc(CtcOp),
    Cub(u16),
    Cud(u16),
    Cuf(u16),
    Cup(u16, u16),
    Cuu(u16),
    Dch(u16),
    Decaln,
    /// DECBI `ESC 6` — back index. Steps the cursor left; at the left margin it
    /// scrolls the margin box right instead, opening a blank column.
    Decbi,
    /// DECFI `ESC 9` — forward index. The mirror of [`Decbi`]: at the right margin
    /// it scrolls the box left.
    Decfi,
    Decrc,
    Decrst(DecModes),
    Decsc,
    Decset(DecModes),
    Decstbm(u16, u16),
    /// DECSLRM `CSI Pl ; Pr s` — set left/right margins. Shares its final byte
    /// with SCOSC (`CSI s`): the terminal treats it as DECSLRM only while
    /// DECLRMM (`?69`) is on, otherwise as SCOSC (save cursor).
    Decslrm(u16, u16),
    /// DECIC `CSI Pn ' }` — insert `Pn` blank columns at the cursor column.
    Decic(u16),
    /// DECDC `CSI Pn ' ~` — delete `Pn` columns at the cursor column.
    Decdc(u16),
    /// DECSCL `CSI Pl ; Ps " p` — select conformance level. `Pl` is 61–65
    /// (VT100–VT500 → level 1–5); `Ps` picks 7- vs 8-bit controls. Performs a
    /// hard reset, then applies the level (which gates VT400+ features like
    /// DECLRMM).
    Decscl(u16, u16),
    /// DECSCA `CSI Ps " q` — select character protection attribute. `Ps` 1 marks
    /// subsequent writes as DEC-protected (spared by DECSED/DECSEL/DECSERA);
    /// `Ps` 0 or 2 clears it.
    Decsca(u16),
    /// DECSERA `CSI Pt ; Pl ; Pb ; Pr $ {` — selectively erase a rectangle,
    /// sparing DEC-protected cells. Coordinates are 1-based inclusive
    /// (origin-mode relative), and the erase ignores the scroll margins.
    Decsera(u16, u16, u16, u16),
    /// DECERA `CSI Pt ; Pl ; Pb ; Pr $ z` — erase a rectangle. The non-selective
    /// twin of [`Decsera`]: it clears DEC-protected cells too. Same coordinates.
    Decera(u16, u16, u16, u16),
    /// DECFRA `CSI Pch ; Pt ; Pl ; Pb ; Pr $ x` — fill a rectangle with the
    /// character whose code is `Pch` (32..126 or 160..255; anything else is
    /// ignored), under the current pen. Same coordinates as [`Decera`].
    Decfra(u16, u16, u16, u16, u16),
    /// DECCRA `CSI Pts;Pls;Pbs;Prs;Pps;Ptd;Pld;Ppd $ v` — copy the source
    /// rectangle to the destination's top-left corner, source and destination
    /// free to overlap. Coordinates as [`Decera`]; the two page params are
    /// ignored (ghost has a single page).
    Deccra([u16; 8]),
    Decstr,
    Dl(u16),
    Ech(u16),
    Ed(EdScope),
    /// DECSED `CSI ? Ps J` — selective erase in display: like ED, but spares
    /// protected cells.
    Decsed(EdScope),
    El(ElScope),
    /// DECSEL `CSI ? Ps K` — selective erase in line: like EL, but spares
    /// protected cells.
    Decsel(ElScope),
    /// SPA (`ESC V`) / EPA (`ESC W`) — start/end an ISO 6429 guarded area. Cells
    /// written between them are ISO-protected (spared by plain ED/EL/ECH too).
    Spa,
    Epa,
    G1d4(Charset),
    Gzd4(Charset),
    Ht,
    Hts,
    /// OSC 8 hyperlink: `Some(uri)` opens a link (subsequent prints carry it),
    /// `None` (the empty-URI form) closes it. The optional `params` field
    /// (`id=…`) is accepted on input and dropped — ghost groups by URI.
    Hyperlink(Option<String>),
    Ich(u16),
    Il(u16),
    /// kitty keyboard protocol — push the current flags and make `flags` current
    /// (`CSI > flags u`, flags default 0).
    KittyKeyboardPush(u8),
    /// kitty keyboard protocol — pop `n` saved flag-sets, restoring the exposed
    /// one (`CSI < n u`, n default 1; popping past empty resets to 0).
    KittyKeyboardPop(u16),
    /// kitty keyboard protocol — set the current flags (`CSI = flags ; mode u`):
    /// mode 1 = set exactly (default), 2 = set named bits, 3 = clear named bits.
    KittyKeyboardSet(u8, u8),
    /// kitty graphics protocol — a graphics command carried in an APC string
    /// (`ESC _ G <payload> ST`). The string is everything after the leading `G`
    /// (control data, optionally `;` then a base64 payload). Parsed and applied
    /// by the graphics engine; never part of a screen dump.
    KittyGraphics(String),
    Lf,
    /// xterm modifyOtherKeys level (XTMODKEYS resource 4): `CSI > 4 ; Pv m`.
    /// 0 = off, 1 = report keys lacking an unambiguous legacy byte, 2 = also
    /// report the well-known C0 keys (Ctrl+letter, Tab, Enter, Esc, …).
    ModifyOtherKeys(u8),
    Nel,
    /// OSC 133 shell integration (FinalTerm semantic prompts).
    PromptMark(PromptMark),
    Print(char),
    Rep(u16),
    Ri,
    Ris,
    Rm(AnsiModes),
    Scorc,
    Scosc,
    Sd(u16),
    /// OSC 52 clipboard write: the raw `Pc` selection list and the base64
    /// `Pd` payload, decoded and dispatched by the terminal.
    SetClipboard(String, String),
    /// OSC 10/11/12 dynamic-color set (specs already parsed to 8-bit RGB) or the
    /// matching OSC 110/111/112 reset (`None`) back to the theme default. A list
    /// because xterm's consecutive-code form lets one OSC set several: `OSC 10 ;
    /// fg ; bg` sets the foreground *and* the background.
    SetDynamicColor(Vec<(DynamicColor, Option<[u8; 3]>)>),
    /// OSC 4 indexed-palette set: `(index, rgb)` for every pair the sequence
    /// carried (one OSC may set several). Query (`?`) pairs are not here — the
    /// host answers those from the live palette.
    ///
    /// The index is 16-bit because xterm addresses the five special colors
    /// (bold, underline, …) past the end of the 256 indexed ones — `256 + c`,
    /// the same colors OSC 5 names directly. [`SPECIAL_COLOR_BASE`] splits them.
    SetPalette(Vec<(u16, [u8; 3])>),
    /// OSC 104 palette reset: the indices to take back to the theme's colors, or
    /// an empty list for "all of them" (which, as in xterm, leaves the special
    /// colors alone — OSC 105 resets those).
    ResetPalette(Vec<u16>),
    /// OSC 9;4 taskbar progress; `None` (state 0) removes it.
    SetProgress(Option<Progress>),
    /// DECSCUSR (`CSI Ps SP q`): set the cursor style. The raw Ps (0..=6) is
    /// carried verbatim; the terminal decodes it to a shape (blink is dropped).
    SetCursorStyle(u8),
    /// OSC 0/1/2 — set the window title, the icon label, or both. Ghost shows the
    /// window title and only *keeps* the icon label (nothing here has an icon), so
    /// a program that sets it reads back what it set (`CSI 20 t`).
    SetTitle(TitleTarget, String),
    Sgr(SgrOps),
    Si,
    Sm(AnsiModes),
    So,
    Su(u16),
    Tbc(TbcScope),
    Vpa(u16),
    Vpr(u16),
    Xtwinops(XtwinopsOp),
}

#[derive(Debug, Copy, Clone, PartialEq)]
#[repr(u16)]
pub enum AnsiMode {
    // Recognized-but-inert legacy modes: tracked only so DECRQM round-trips the
    // set/reset bit; ghost does not implement their effect.
    KeyboardAction = 2, // KAM
    Insert = 4,         // IRM
    SendReceive = 12,   // SRM
    NewLine = 20,       // LNM
}

#[derive(Debug, PartialEq)]
pub enum CtcOp {
    Set,
    ClearCurrentColumn,
    ClearAll,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u16)]
pub enum DecMode {
    CursorKeys = 1,             // DECCKM
    Columns132 = 3,             // DECCOLM — 80↔132 column mode (needs ?40)
    Origin = 6,                 // DECOM
    AutoWrap = 7,               // DECAWM
    TextCursorEnable = 25,      // DECTCEM
    Allow80To132 = 40,          // enable the DECCOLM 80↔132 switch
    ReverseWrap = 45,           // reverse-wraparound (xterm ?45; needs DECAWM)
    LeftRightMargin = 69,       // DECLRMM — enables DECSLRM left/right margins
    NoClearOnColumnChange = 95, // DECNCSM — keep screen on DECCOLM (VT500)
    // Recognized-but-inert legacy modes: ghost does not implement their effect,
    // but tracks the set/reset bit so DECRQM round-trips it (`Pm` 1/2 rather than
    // 0/unrecognized). Treated as non-display, so a dump re-emits them.
    SmoothScroll = 4,                // DECSCLM
    ReverseVideo = 5,                // DECSCNM
    PrintFormFeed = 18,              // DECPFF
    PrintExtent = 19,                // DECPEX
    NationalReplacementCharset = 42, // DECNRCM
    NumericKeypad = 66,              // DECNKM
    BackarrowKey = 67,               // DECBKM
    // Non-display modes: tracked but not rendered, so a state dump can restore
    // them on reattach. They affect what the terminal *sends*, not the grid.
    MouseReportX11 = 1000,            // button press/release
    MouseReportButton = 1002,         // button-event tracking (drag)
    MouseReportAny = 1003,            // any-event tracking (all motion)
    FocusReport = 1004,               // focus in/out events
    MouseSgr = 1006,                  // SGR extended coordinate encoding
    AltScreenBuffer = 1047,           // xterm
    SaveCursor = 1048,                // xterm
    SaveCursorAltScreenBuffer = 1049, // xterm
    BracketedPaste = 2004,            // wrap pastes in ESC[200~ / ESC[201~
    SynchronizedOutput = 2026,        // atomic frames: hold presentation between h..l
    ColorSchemeReport = 2031,         // unsolicited CSI ?997;Ps n on theme change
}

impl DecMode {
    /// Whether this is a non-display mode — tracked and re-emitted on dump, but
    /// with no effect on the rendered grid.
    pub(crate) fn is_non_display(self) -> bool {
        use DecMode::*;
        matches!(
            self,
            SmoothScroll
                | ReverseVideo
                | PrintFormFeed
                | PrintExtent
                | NationalReplacementCharset
                | NumericKeypad
                | BackarrowKey
                | MouseReportX11
                | MouseReportButton
                | MouseReportAny
                | FocusReport
                | MouseSgr
                | BracketedPaste
                | SynchronizedOutput
                | ColorSchemeReport
        )
    }
}

/// ConEmu/Windows Terminal taskbar progress (OSC 9;4): what a long-running
/// task reports about itself. `None` (state 0) removes the indication.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Progress {
    /// State 1: a determinate task at `percent` (0..=100).
    Normal(u8),
    /// State 2: the task hit an error at `percent`.
    Error(u8),
    /// State 3: busy, no measurable progress.
    Indeterminate,
    /// State 4: paused/warning at `percent`.
    Paused(u8),
}

/// The three xterm "dynamic colors" an application can override at runtime:
/// default text foreground (OSC 10), default background (OSC 11), and the
/// cursor color (OSC 12).
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum DynamicColor {
    Foreground,
    Background,
    Cursor,
}

/// The five xterm "special colors" an application can override with OSC 5 (and
/// reset with OSC 105): the color to paint text carrying an attribute with,
/// instead of the pen's own. Ghost tracks them so a program's set/query
/// round-trips, but — like xterm with its `colorBDMode` and friends off — does
/// not paint with them.
///
/// xterm also addresses them through OSC 4, at [`SPECIAL_COLOR_BASE`] `+ c`,
/// past the end of the 256 indexed colors.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum SpecialColor {
    Bold = 0,
    Underline = 1,
    Blink = 2,
    Reverse = 3,
    Italic = 4,
}

/// The OSC 4 index the special colors start at: `OSC 4 ; 256` is OSC 5's color
/// 0 (bold). The 256 indexed colors come below it.
pub const SPECIAL_COLOR_BASE: u16 = 256;

impl SpecialColor {
    /// The special color OSC 5 names `c` (and OSC 4 names `SPECIAL_COLOR_BASE +
    /// c`), or `None` for one xterm has and ghost does not.
    pub fn from_code(c: u16) -> Option<Self> {
        match c {
            0 => Some(SpecialColor::Bold),
            1 => Some(SpecialColor::Underline),
            2 => Some(SpecialColor::Blink),
            3 => Some(SpecialColor::Reverse),
            4 => Some(SpecialColor::Italic),
            _ => None,
        }
    }
}

/// The OSC 133 semantic marks (FinalTerm shell integration): where a prompt
/// begins, where the user's command line begins, where its output begins, and
/// that it finished (with an exit code when reported).
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum PromptMark {
    PromptStart,
    CommandStart,
    OutputStart,
    CommandDone(Option<i32>),
}

#[derive(Debug, PartialEq)]
pub enum EdScope {
    Below,
    Above,
    All,
    SavedLines,
}

#[derive(Debug, PartialEq)]
pub enum ElScope {
    ToRight,
    ToLeft,
    All,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum SgrOp {
    Reset,                     // 0
    SetBoldIntensity,          // 1
    SetFaintIntensity,         // 2
    SetItalic,                 // 3
    SetUnderline,              // 4
    SetBlink,                  // 5
    SetInverse,                // 7
    SetStrikethrough,          // 9
    ResetIntensity,            // 21, 22
    ResetItalic,               // 23
    ResetUnderline,            // 24
    ResetBlink,                // 25
    ResetInverse,              // 27
    ResetStrikethrough,        // 29
    SetForegroundColor(Color), // 30-38
    ResetForegroundColor,      // 39
    SetBackgroundColor(Color), // 40-48
    ResetBackgroundColor,      // 49
}

#[derive(Debug, PartialEq)]
pub enum TbcScope {
    CurrentColumn,
    All,
}

#[derive(Debug, PartialEq)]
pub enum XtwinopsOp {
    /// `CSI 8 ; rows ; cols t` — resize the text area to a character grid.
    /// `None` is an omitted dimension (keep the one it has); `Some(0)` is xterm's
    /// "as big as the display fits", which only the frontend can resolve.
    Resize(Option<u16>, Option<u16>),
    /// `CSI 4 ; height ; width t` — the same, in *pixels*. Only the frontend knows
    /// how many pixels a cell is, so it does the arithmetic.
    ResizePixels(Option<u16>, Option<u16>),
    /// `CSI Ps t` with `Ps` ≥ 24 — DECSLPP, set the page height in lines. Only
    /// the height moves; the width stays.
    SetLines(u16),
    /// `CSI 2 t` / `CSI 1 t` — iconify (minimize) the window, and restore it.
    Iconify,
    Deiconify,
    /// `CSI 9 ; Ps t` — maximize the window, in one axis or both, or restore it.
    Maximize(MaximizeOp),
    /// `CSI 10 ; Ps t` — take the window full-screen, leave, or toggle.
    Fullscreen(FullscreenOp),
    /// `CSI 22 ; Ps t` / `CSI 23 ; Ps t` — push a title onto the terminal's title
    /// stack, and pop it back. The emulator holds the titles, so it does these
    /// itself; a program that changes a title around a full-screen editor gets its
    /// old one back this way.
    PushTitle(TitleTarget),
    PopTitle(TitleTarget),
}

/// The axes `CSI 9 ; Ps t` maximizes over (xterm's `Ps`).
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum MaximizeOp {
    /// `9 ; 0` — restore the window to the size it had before it was maximized.
    Restore,
    /// `9 ; 1` — maximize over both axes.
    Both,
    /// `9 ; 2` — maximize the height only.
    Vertically,
    /// `9 ; 3` — maximize the width only.
    Horizontally,
}

/// Which of the two titles a sequence addresses — OSC 0/1/2, and the XTWINOPS
/// title stack (`CSI 22 ; Ps t` / `CSI 23 ; Ps t`, where `Ps` is 0 for both,
/// 1 for the icon label and 2 for the window title).
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum TitleTarget {
    Both,
    Icon,
    Window,
}

impl TitleTarget {
    /// The `Ps` of an XTWINOPS title op (0 both, 1 icon, 2 window); anything else
    /// names no title.
    fn from_ps(ps: u16) -> Option<Self> {
        match ps {
            0 => Some(TitleTarget::Both),
            1 => Some(TitleTarget::Icon),
            2 => Some(TitleTarget::Window),
            _ => None,
        }
    }

    pub fn window(self) -> bool {
        matches!(self, TitleTarget::Window | TitleTarget::Both)
    }

    pub fn icon(self) -> bool {
        matches!(self, TitleTarget::Icon | TitleTarget::Both)
    }
}

/// What `CSI 10 ; Ps t` asks of full-screen mode (xterm's `Ps`).
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum FullscreenOp {
    Leave,
    Enter,
    Toggle,
}

/// Number of SgrOp values that fit inline without heap allocation.
pub const SGR_OPS_INLINE_CAP: usize = 4;

/// Small-buffer-optimized collection of SgrOp values.
#[derive(Debug, Clone, PartialEq)]
pub struct SgrOps(SgrOpsStorage);

#[derive(Debug, Clone, PartialEq)]
enum SgrOpsStorage {
    Inline {
        ops: [SgrOp; SGR_OPS_INLINE_CAP],
        len: u8,
    },
    Heap(Vec<SgrOp>),
}

impl SgrOps {
    pub(crate) fn new() -> Self {
        Self(SgrOpsStorage::Inline {
            ops: [SgrOp::Reset; SGR_OPS_INLINE_CAP],
            len: 0,
        })
    }

    pub(crate) fn collect<I: IntoIterator<Item = SgrOp>>(iter: I) -> Self {
        let mut ops = Self::new();

        for op in iter {
            ops.push(op);
        }

        ops
    }

    pub(crate) fn len(&self) -> usize {
        match &self.0 {
            SgrOpsStorage::Inline { len, .. } => *len as usize,
            SgrOpsStorage::Heap(v) => v.len(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn as_slice(&self) -> &[SgrOp] {
        match &self.0 {
            SgrOpsStorage::Inline { ops, len } => &ops[..*len as usize],
            SgrOpsStorage::Heap(v) => v.as_slice(),
        }
    }

    pub(crate) fn push(&mut self, op: SgrOp) {
        match &mut self.0 {
            SgrOpsStorage::Inline { ops, len } => {
                if (*len as usize) < SGR_OPS_INLINE_CAP {
                    ops[*len as usize] = op;
                    *len += 1;
                } else {
                    let mut v = Vec::with_capacity(SGR_OPS_INLINE_CAP * 2);
                    v.extend_from_slice(ops);
                    v.push(op);
                    self.0 = SgrOpsStorage::Heap(v);
                }
            }
            SgrOpsStorage::Heap(v) => v.push(op),
        }
    }
}

impl From<Vec<SgrOp>> for SgrOps {
    fn from(v: Vec<SgrOp>) -> Self {
        if v.len() <= SGR_OPS_INLINE_CAP {
            Self::collect(v)
        } else {
            Self(SgrOpsStorage::Heap(v))
        }
    }
}

impl From<&[SgrOp]> for SgrOps {
    fn from(v: &[SgrOp]) -> Self {
        Self::collect(v.iter().copied())
    }
}

/// Number of mode values that fit inline without heap allocation.
pub const MODES_INLINE_CAP: usize = 4;

/// Small-buffer-optimized collection of mode values.
#[derive(Debug, Clone, PartialEq)]
pub struct AnsiModes(AnsiModesStorage);

#[derive(Debug, Clone, PartialEq)]
enum AnsiModesStorage {
    Inline {
        modes: [AnsiMode; MODES_INLINE_CAP],
        len: u8,
    },
    Heap(Vec<AnsiMode>),
}

impl AnsiModes {
    pub(crate) fn new() -> Self {
        Self(AnsiModesStorage::Inline {
            modes: [AnsiMode::Insert; MODES_INLINE_CAP],
            len: 0,
        })
    }

    pub(crate) fn one(mode: AnsiMode) -> Self {
        let mut modes = Self::new();
        modes.push(mode);
        modes
    }

    pub(crate) fn collect<I: IntoIterator<Item = AnsiMode>>(iter: I) -> Self {
        let mut v = Self::new();

        for m in iter {
            v.push(m);
        }

        v
    }

    pub(crate) fn as_slice(&self) -> &[AnsiMode] {
        match &self.0 {
            AnsiModesStorage::Inline { modes, len } => &modes[..*len as usize],
            AnsiModesStorage::Heap(v) => v.as_slice(),
        }
    }

    pub(crate) fn push(&mut self, m: AnsiMode) {
        match &mut self.0 {
            AnsiModesStorage::Inline { modes, len } => {
                if (*len as usize) < MODES_INLINE_CAP {
                    modes[*len as usize] = m;
                    *len += 1;
                } else {
                    let mut v = Vec::with_capacity(MODES_INLINE_CAP * 2);
                    v.extend_from_slice(modes);
                    v.push(m);
                    self.0 = AnsiModesStorage::Heap(v);
                }
            }
            AnsiModesStorage::Heap(v) => v.push(m),
        }
    }
}

impl From<Vec<AnsiMode>> for AnsiModes {
    fn from(v: Vec<AnsiMode>) -> Self {
        if v.len() <= MODES_INLINE_CAP {
            Self::collect(v)
        } else {
            Self(AnsiModesStorage::Heap(v))
        }
    }
}

impl From<&[AnsiMode]> for AnsiModes {
    fn from(v: &[AnsiMode]) -> Self {
        Self::collect(v.iter().copied())
    }
}

/// Small-buffer-optimized collection of DEC mode values.
#[derive(Debug, Clone, PartialEq)]
pub struct DecModes(DecModesStorage);

#[derive(Debug, Clone, PartialEq)]
enum DecModesStorage {
    Inline {
        modes: [DecMode; MODES_INLINE_CAP],
        len: u8,
    },
    Heap(Vec<DecMode>),
}

impl DecModes {
    pub(crate) fn new() -> Self {
        Self(DecModesStorage::Inline {
            modes: [DecMode::CursorKeys; MODES_INLINE_CAP],
            len: 0,
        })
    }

    pub(crate) fn one(mode: DecMode) -> Self {
        let mut modes = Self::new();
        modes.push(mode);
        modes
    }

    pub(crate) fn collect<I: IntoIterator<Item = DecMode>>(iter: I) -> Self {
        let mut v = Self::new();

        for m in iter {
            v.push(m);
        }

        v
    }

    pub(crate) fn as_slice(&self) -> &[DecMode] {
        match &self.0 {
            DecModesStorage::Inline { modes, len } => &modes[..*len as usize],
            DecModesStorage::Heap(v) => v.as_slice(),
        }
    }

    pub(crate) fn push(&mut self, m: DecMode) {
        match &mut self.0 {
            DecModesStorage::Inline { modes, len } => {
                if (*len as usize) < MODES_INLINE_CAP {
                    modes[*len as usize] = m;
                    *len += 1;
                } else {
                    let mut v = Vec::with_capacity(MODES_INLINE_CAP * 2);
                    v.extend_from_slice(modes);
                    v.push(m);
                    self.0 = DecModesStorage::Heap(v);
                }
            }
            DecModesStorage::Heap(v) => v.push(m),
        }
    }
}

impl From<Vec<DecMode>> for DecModes {
    fn from(v: Vec<DecMode>) -> Self {
        if v.len() <= MODES_INLINE_CAP {
            Self::collect(v)
        } else {
            Self(DecModesStorage::Heap(v))
        }
    }
}

impl From<&[DecMode]> for DecModes {
    fn from(v: &[DecMode]) -> Self {
        Self::collect(v.iter().copied())
    }
}

impl Parser {
    pub fn new() -> Self {
        Default::default()
    }

    pub fn feed(&mut self, input: char) -> Option<Function> {
        use State::*;

        let input2 = if input >= '\u{a0}' { '\u{41}' } else { input };

        match (&self.state, input2) {
            (Ground, '\u{20}'..='\u{7f}') => {
                return Some(Function::Print(input));
            }

            (CsiParam, '\u{30}'..='\u{3b}') => {
                self.param(input);
            }

            (OscString, '\u{1b}') => {
                // ESC terminates the OSC string (ST is ESC \); dispatch before
                // re-entering Escape so the following byte is parsed normally.
                self.state = Escape;
                self.clear();
                return self.osc_dispatch();
            }

            (ApcString, '\u{1b}') => {
                // ESC begins the ST (ESC \) that terminates the APC; dispatch
                // before re-entering Escape so the trailing `\` parses normally.
                self.state = Escape;
                self.clear();
                return self.apc_dispatch();
            }

            (_, '\u{1b}') => {
                self.state = Escape;
                self.clear();
            }

            (Escape, '\u{5b}') => {
                self.state = CsiEntry;
                self.clear();
            }

            (CsiParam | CsiEntry | CsiIntermediate, '\u{40}'..='\u{7e}') => {
                self.state = Ground;
                return self.csi_dispatch(input);
            }

            (CsiEntry, '\u{30}'..='\u{39}') | (CsiEntry, '\u{3b}') => {
                self.state = CsiParam;
                self.param(input);
            }

            (Ground, '\u{00}'..='\u{17}') | (Ground, '\u{19}') | (Ground, '\u{1c}'..='\u{1f}') => {
                return self.execute(input);
            }

            (OscString, '\u{20}'..='\u{7f}') => {
                self.osc_put(input);
            }

            // APC payload bytes (printable ASCII: base64 + key=value control data).
            (ApcString, '\u{20}'..='\u{7e}') => {
                self.apc_put(input);
            }

            // C0 controls and DEL inside an APC are ignored (CAN/SUB/ESC handled
            // elsewhere); the payload is printable ASCII only.
            (ApcString, '\u{00}'..='\u{17}')
            | (ApcString, '\u{19}')
            | (ApcString, '\u{1c}'..='\u{1f}')
            | (ApcString, '\u{7f}') => {}

            (Escape, '\u{20}'..='\u{2f}') => {
                self.state = EscapeIntermediate;
                self.collect(input);
            }

            (EscapeIntermediate, '\u{30}'..='\u{7e}')
            | (Escape, '\u{30}'..='\u{4f}')
            | (Escape, '\u{51}'..='\u{57}')
            | (Escape, '\u{59}')
            | (Escape, '\u{5a}')
            | (Escape, '\u{5c}')
            | (Escape, '\u{60}'..='\u{7e}') => {
                self.state = Ground;
                return self.esc_dispatch(input);
            }

            (CsiEntry, '\u{3c}'..='\u{3f}') => {
                self.state = CsiParam;
                self.collect(input);
            }

            (DcsPassthrough, '\u{00}'..='\u{17}')
            | (DcsPassthrough, '\u{19}')
            | (DcsPassthrough, '\u{1c}'..='\u{7e}') => {
                self.put(input);
            }

            (CsiIgnore, '\u{40}'..='\u{7e}') => {
                self.state = Ground;
            }

            (CsiParam, '\u{3c}'..='\u{3f}')
            | (CsiIntermediate, '\u{30}'..='\u{3f}')
            | (CsiEntry, '\u{3a}') => {
                self.state = CsiIgnore;
            }

            (Escape, '\u{5d}') => {
                self.state = OscString;
                self.osc.clear();
            }

            (OscString, '\u{07}') => {
                // 0x07 is xterm non-ANSI variant of transition to ground
                self.state = Ground;
                return self.osc_dispatch();
            }

            (_, '\u{18}')
            | (_, '\u{1a}')
            | (_, '\u{80}'..='\u{8f}')
            | (_, '\u{91}'..='\u{97}')
            | (_, '\u{99}')
            | (_, '\u{9a}') => {
                self.state = Ground;
                return self.execute(input);
            }

            (Escape, '\u{50}') => {
                self.state = DcsEntry;
                self.clear();
            }

            (CsiParam | CsiEntry, '\u{20}'..='\u{2f}') => {
                self.state = CsiIntermediate;
                self.collect(input);
            }

            (DcsParam, '\u{30}'..='\u{39}') | (DcsParam, '\u{3b}') => {
                self.param(input);
            }

            (DcsEntry, '\u{3c}'..='\u{3f}') => {
                self.state = DcsParam;
                self.collect(input);
            }

            (DcsEntry | DcsParam | DcsIntermediate, '\u{40}'..='\u{7e}') => {
                self.state = DcsPassthrough;
            }

            (DcsEntry | DcsParam, '\u{20}'..='\u{2f}') => {
                self.state = DcsIntermediate;
                self.collect(input);
            }

            (CsiIntermediate | EscapeIntermediate | DcsIntermediate, '\u{20}'..='\u{2f}') => {
                self.collect(input);
            }

            (DcsEntry, '\u{3a}')
            | (DcsIntermediate, '\u{30}'..='\u{3f}')
            | (DcsParam, '\u{3a}')
            | (DcsParam, '\u{3c}'..='\u{3f}') => {
                self.state = DcsIgnore;
            }

            (DcsEntry, '\u{30}'..='\u{39}') | (DcsEntry, '\u{3b}') => {
                self.state = DcsParam;
                self.param(input);
            }

            (Escape | EscapeIntermediate
            | CsiEntry | CsiParam | CsiIntermediate | CsiIgnore,
            '\u{00}'..='\u{17}')
            | (Escape | EscapeIntermediate
            | CsiEntry | CsiParam | CsiIntermediate | CsiIgnore,
            '\u{19}')
            | (Escape | EscapeIntermediate
            | CsiEntry | CsiParam | CsiIntermediate | CsiIgnore,
            '\u{1c}'..='\u{1f}') => {
                return self.execute(input);
            }

            // SOS (ESC X) and PM (ESC ^) strings are collected then discarded.
            (Escape, '\u{58}') | (Escape, '\u{5e}') => {
                self.state = SosPmApcString;
            }

            (_, '\u{98}') | (_, '\u{9e}') => {
                self.state = SosPmApcString;
            }

            // APC (ESC _ / C1 0x9f) carries the kitty graphics protocol; unlike
            // SOS/PM it is accumulated so its payload can be acted upon.
            (Escape, '\u{5f}') | (_, '\u{9f}') => {
                self.state = ApcString;
                self.apc.clear();
                self.apc_overflow = false;
            }

            (OscString, '\u{9c}') => {
                // C1 ST terminates the OSC string.
                self.state = Ground;
                return self.osc_dispatch();
            }

            (ApcString, '\u{9c}') => {
                // C1 ST terminates the APC string.
                self.state = Ground;
                return self.apc_dispatch();
            }

            (_, '\u{9c}') => {
                self.state = Ground;
            }

            (_, '\u{9d}') => {
                self.state = OscString;
                self.osc.clear();
            }

            (_, '\u{90}') => {
                self.state = DcsEntry;
                self.clear();
            }

            (_, '\u{9b}') => {
                self.state = CsiEntry;
                self.clear();
            }

            // DEL (0x7F) is ignored in all states except Ground and OscString
            (Escape | EscapeIntermediate
            | CsiEntry | CsiParam | CsiIntermediate | CsiIgnore
            | DcsEntry | DcsParam | DcsIntermediate | DcsPassthrough | DcsIgnore
            | SosPmApcString, '\u{7f}')

            // CsiIgnore: params and intermediates range ignored
            | (CsiIgnore, '\u{20}'..='\u{3f}')

            // C0 controls ignored in DCS entry/param/intermediate
            | (DcsEntry | DcsParam | DcsIntermediate, '\u{00}'..='\u{17}')
            | (DcsEntry | DcsParam | DcsIntermediate, '\u{19}')
            | (DcsEntry | DcsParam | DcsIntermediate, '\u{1c}'..='\u{1f}')

            // C0 controls and printable range ignored in DcsIgnore and SosPmApcString
            | (DcsIgnore | SosPmApcString, '\u{00}'..='\u{17}')
            | (DcsIgnore | SosPmApcString, '\u{19}')
            | (DcsIgnore | SosPmApcString, '\u{1c}'..='\u{7e}')

            // Some C0 controls ignored in OscString (0x07 handled above as xterm ST)
            | (OscString, '\u{00}'..='\u{06}')
            | (OscString, '\u{08}'..='\u{17}')
            | (OscString, '\u{19}')
            | (OscString, '\u{1c}'..='\u{1f}') => {}

            // input2 is always < 0xA0 due to the mapping above
            (Ground | Escape | EscapeIntermediate
            | CsiEntry | CsiParam | CsiIntermediate | CsiIgnore
            | DcsEntry | DcsParam | DcsIntermediate | DcsPassthrough | DcsIgnore
            | OscString | SosPmApcString | ApcString, '\u{a0}'..='\u{10ffff}') => {
                unreachable!()
            }
        }

        None
    }

    fn execute(&mut self, input: char) -> Option<Function> {
        use Function::*;

        match input {
            '\u{07}' => Some(Bell),
            '\u{08}' => Some(Bs),
            '\u{09}' => Some(Ht),
            '\u{0a}' => Some(Lf),
            '\u{0b}' => Some(Lf),
            '\u{0c}' => Some(Lf),
            '\u{0d}' => Some(Cr),
            '\u{0e}' => Some(So),
            '\u{0f}' => Some(Si),
            '\u{84}' => Some(Lf),
            '\u{85}' => Some(Nel),
            '\u{88}' => Some(Hts),
            '\u{8d}' => Some(Ri),
            '\u{96}' => Some(Spa),
            '\u{97}' => Some(Epa),
            _ => None,
        }
    }

    fn clear(&mut self) {
        for p in &mut self.params[..=self.cur_param] {
            p.clear();
        }

        self.cur_param = 0;
        self.intermediate = None;
    }

    fn collect(&mut self, input: char) {
        self.intermediate = Some(input);
    }

    fn param(&mut self, input: char) {
        if input == ';' {
            self.cur_param += 1;

            if self.cur_param == PARAMS_LEN {
                self.cur_param = PARAMS_LEN - 1;
            }
        } else if input == ':' {
            self.params[self.cur_param].add_part();
        } else {
            self.params[self.cur_param].add_digit((input as u8) - 0x30);
        }
    }

    fn esc_dispatch(&mut self, input: char) -> Option<Function> {
        use Function::*;

        match (self.intermediate, input) {
            (None, c) if ('@'..='_').contains(&c) => self.execute(((input as u8) + 0x40) as char),

            (None, '6') => Some(Decbi),

            (None, '7') => Some(Decsc),

            (None, '8') => Some(Decrc),

            (None, '9') => Some(Decfi),

            (None, 'c') => {
                self.state = State::Ground;
                Some(Ris)
            }

            (Some('#'), '8') => Some(Decaln),

            (Some('('), '0') => Some(Gzd4(Charset::Drawing)),

            (Some('('), _) => Some(Gzd4(Charset::Ascii)),

            (Some(')'), '0') => Some(G1d4(Charset::Drawing)),

            (Some(')'), _) => Some(G1d4(Charset::Ascii)),

            _ => None,
        }
    }

    fn csi_dispatch(&mut self, input: char) -> Option<Function> {
        use Function::*;

        let ps = &self.params;

        match (self.intermediate, input) {
            (None, '@') => Some(Ich(ps[0].as_u16())),

            (None, 'A') => Some(Cuu(ps[0].as_u16())),

            (None, 'B') => Some(Cud(ps[0].as_u16())),

            (None, 'C') => Some(Cuf(ps[0].as_u16())),

            (None, 'D') => Some(Cub(ps[0].as_u16())),

            (None, 'E') => Some(Cnl(ps[0].as_u16())),

            (None, 'F') => Some(Cpl(ps[0].as_u16())),

            (None, 'G') => Some(Cha(ps[0].as_u16())),

            (None, 'H') => Some(Cup(ps[0].as_u16(), ps[1].as_u16())),

            (None, 'I') => Some(Cht(ps[0].as_u16())),

            (None, 'J') => match ps[0].as_u16() {
                0 => Some(Ed(EdScope::Below)),
                1 => Some(Ed(EdScope::Above)),
                2 => Some(Ed(EdScope::All)),
                3 => Some(Ed(EdScope::SavedLines)),
                _ => None,
            },

            (Some('?'), 'J') => match ps[0].as_u16() {
                0 => Some(Decsed(EdScope::Below)),
                1 => Some(Decsed(EdScope::Above)),
                2 => Some(Decsed(EdScope::All)),
                3 => Some(Decsed(EdScope::SavedLines)),
                _ => None,
            },

            (None, 'K') => match ps[0].as_u16() {
                0 => Some(El(ElScope::ToRight)),
                1 => Some(El(ElScope::ToLeft)),
                2 => Some(El(ElScope::All)),
                _ => None,
            },

            (Some('?'), 'K') => match ps[0].as_u16() {
                0 => Some(Decsel(ElScope::ToRight)),
                1 => Some(Decsel(ElScope::ToLeft)),
                2 => Some(Decsel(ElScope::All)),
                _ => None,
            },

            (None, 'L') => Some(Il(ps[0].as_u16())),

            (None, 'M') => Some(Dl(ps[0].as_u16())),

            (None, 'P') => Some(Dch(ps[0].as_u16())),

            (None, 'S') => Some(Su(ps[0].as_u16())),

            (None, 'T') => Some(Sd(ps[0].as_u16())),

            (None, 'W') => match ps[0].as_u16() {
                0 => Some(Ctc(CtcOp::Set)),
                2 => Some(Ctc(CtcOp::ClearCurrentColumn)),
                5 => Some(Ctc(CtcOp::ClearAll)),
                _ => None,
            },

            (None, 'X') => Some(Ech(ps[0].as_u16())),

            (None, 'Z') => Some(Cbt(ps[0].as_u16())),

            (None, '`') => Some(Cha(ps[0].as_u16())),

            (None, 'a') => Some(Cuf(ps[0].as_u16())),

            (None, 'b') => Some(Rep(ps[0].as_u16())),

            (None, 'd') => Some(Vpa(ps[0].as_u16())),

            (None, 'e') => Some(Vpr(ps[0].as_u16())),

            (None, 'f') => Some(Cup(ps[0].as_u16(), ps[1].as_u16())),

            (None, 'g') => match ps[0].as_u16() {
                0 => Some(Tbc(TbcScope::CurrentColumn)),
                3 => Some(Tbc(TbcScope::All)),
                _ => None,
            },

            (None, 'h') => Some(Sm(AnsiModes::collect(
                ps[..=self.cur_param].iter().filter_map(ansi_mode),
            ))),

            (None, 'l') => Some(Rm(AnsiModes::collect(
                ps[..=self.cur_param].iter().filter_map(ansi_mode),
            ))),

            (None, 'm') => Some(Sgr(SgrOps::collect(SgrOpsDecoder {
                ps: &ps[..=self.cur_param],
            }))),

            (None, 'r') => Some(Decstbm(ps[0].as_u16(), ps[1].as_u16())),

            (None, 's') => Some(Decslrm(ps[0].as_u16(), ps[1].as_u16())),

            // XTWINOPS. The reporting ops (`CSI 11 t`, `18 t`, `19 t`, …) are
            // questions, not changes, and are answered by the host's query layer
            // (`ghost_vt::query`) — the emulator lets them fall through. So does
            // `CSI 3 t` (move): a Wayland client cannot position itself.
            (None, 't') => match ps[0].as_u16() {
                1 => Some(Xtwinops(XtwinopsOp::Deiconify)),
                2 => Some(Xtwinops(XtwinopsOp::Iconify)),
                4 => Some(Xtwinops(XtwinopsOp::ResizePixels(
                    ps[2].given(),
                    ps[1].given(),
                ))),
                8 => Some(Xtwinops(XtwinopsOp::Resize(ps[2].given(), ps[1].given()))),
                9 => Some(Xtwinops(XtwinopsOp::Maximize(match ps[1].as_u16() {
                    0 => MaximizeOp::Restore,
                    1 => MaximizeOp::Both,
                    2 => MaximizeOp::Vertically,
                    3 => MaximizeOp::Horizontally,
                    _ => return None,
                }))),
                10 => Some(Xtwinops(XtwinopsOp::Fullscreen(match ps[1].as_u16() {
                    0 => FullscreenOp::Leave,
                    1 => FullscreenOp::Enter,
                    2 => FullscreenOp::Toggle,
                    _ => return None,
                }))),
                22 => Some(Xtwinops(XtwinopsOp::PushTitle(TitleTarget::from_ps(
                    ps[1].as_u16(),
                )?))),
                23 => Some(Xtwinops(XtwinopsOp::PopTitle(TitleTarget::from_ps(
                    ps[1].as_u16(),
                )?))),
                // DECSLPP — xterm reads a lone `Ps` of 24 or more as a page height,
                // which is why the window ops stop below it.
                lines if lines >= 24 => Some(Xtwinops(XtwinopsOp::SetLines(lines))),
                _ => None,
            },

            (None, 'u') => Some(Scorc),

            (Some('!'), 'p') => Some(Decstr),

            // DECSCL: `CSI Pl ; Ps " p` (intermediate `"`) — select conformance level.
            (Some('"'), 'p') => Some(Decscl(ps[0].as_u16(), ps[1].as_u16())),

            // DECSCA: `CSI Ps " q` (intermediate `"`) — select character protection.
            (Some('"'), 'q') => Some(Decsca(ps[0].as_u16())),

            // DECSERA: `CSI Pt;Pl;Pb;Pr $ {` (intermediate `$`) — selective erase rect.
            (Some('$'), '{') => Some(Decsera(
                ps[0].as_u16(),
                ps[1].as_u16(),
                ps[2].as_u16(),
                ps[3].as_u16(),
            )),

            // DECERA: `CSI Pt;Pl;Pb;Pr $ z` — erase rectangle.
            (Some('$'), 'z') => Some(Decera(
                ps[0].as_u16(),
                ps[1].as_u16(),
                ps[2].as_u16(),
                ps[3].as_u16(),
            )),

            // DECFRA: `CSI Pch;Pt;Pl;Pb;Pr $ x` — fill rectangle with `Pch`.
            (Some('$'), 'x') => Some(Decfra(
                ps[0].as_u16(),
                ps[1].as_u16(),
                ps[2].as_u16(),
                ps[3].as_u16(),
                ps[4].as_u16(),
            )),

            // DECCRA: `CSI Pts;Pls;Pbs;Prs;Pps;Ptd;Pld;Ppd $ v` — copy rectangle.
            // The page params are parsed and ignored: ghost has a single page.
            (Some('$'), 'v') => Some(Deccra([
                ps[0].as_u16(),
                ps[1].as_u16(),
                ps[2].as_u16(),
                ps[3].as_u16(),
                ps[4].as_u16(),
                ps[5].as_u16(),
                ps[6].as_u16(),
                ps[7].as_u16(),
            ])),

            // DECIC / DECDC: `CSI Pn ' }` / `CSI Pn ' ~` (intermediate `'`).
            (Some('\''), '}') => Some(Decic(ps[0].as_u16())),

            (Some('\''), '~') => Some(Decdc(ps[0].as_u16())),

            (Some('?'), 'h') => Some(Decset(DecModes::collect(
                ps[..=self.cur_param].iter().filter_map(dec_mode),
            ))),

            (Some('?'), 'l') => Some(Decrst(DecModes::collect(
                ps[..=self.cur_param].iter().filter_map(dec_mode),
            ))),

            // XTMODKEYS: `CSI > Pp ; Pv m`. We track only modifyOtherKeys
            // (Pp == 4); the other resources (modifyCursorKeys, …) are ignored.
            // The reset form `CSI > 4 m` omits Pv, which defaults to 0 (off).
            // Pv is 0/1/2; clamp anything higher to the most aggressive level
            // rather than truncating to u8 (256 would wrap to 0 = off).
            (Some('>'), 'm') if ps[0].as_u16() == 4 => {
                let level = match ps[1].as_u16() {
                    0 => 0,
                    1 => 1,
                    _ => 2,
                };
                Some(ModifyOtherKeys(level))
            }

            // DECSCUSR: `CSI Ps SP q` sets the cursor style (Ps 0..=6). Clamp to
            // 255 rather than truncating, so an over-large Ps stays out of range
            // (the terminal ignores it) instead of wrapping into a valid code.
            (Some(' '), 'q') => Some(SetCursorStyle(ps[0].as_u16().min(255) as u8)),

            // kitty keyboard protocol negotiation. The marker disambiguates the
            // three forms; `CSI u` with no marker stays SCO restore-cursor above,
            // and the `CSI ? u` query (no state change) falls through — it is
            // answered by the query scanner, not tracked here.
            (Some('>'), 'u') => Some(KittyKeyboardPush(ps[0].as_u16().min(255) as u8)),
            (Some('<'), 'u') => Some(KittyKeyboardPop(ps[0].as_u16().max(1))),
            (Some('='), 'u') => {
                let flags = ps[0].as_u16().min(255) as u8;
                let mode = match ps[1].as_u16() {
                    2 => 2,
                    3 => 3,
                    _ => 1,
                };
                Some(KittyKeyboardSet(flags, mode))
            }

            _ => None,
        }
    }

    fn put(&mut self, _input: char) {}

    fn osc_put(&mut self, input: char) {
        self.osc.push(input);
    }

    fn apc_put(&mut self, input: char) {
        if self.apc_overflow {
            return;
        }
        if self.apc.len() >= MAX_APC_LEN {
            // Stop accumulating and latch; the sequence is dropped on dispatch.
            self.apc_overflow = true;
            return;
        }
        self.apc.push(input);
    }

    /// Interpret a completed APC string. Only the kitty graphics carrier (a
    /// leading `G`) is recognised; the emitted [`Function::KittyGraphics`] holds
    /// everything after that `G`. Oversized or non-graphics APCs yield nothing.
    fn apc_dispatch(&self) -> Option<Function> {
        if self.apc_overflow {
            return None;
        }
        self.apc
            .strip_prefix('G')
            .map(|rest| Function::KittyGraphics(rest.to_string()))
    }

    /// Interpret a completed OSC string. Only OSC 0 (icon name + title) and
    /// OSC 2 (title) are recognised — both set the window title; everything
    /// else is ignored.
    fn osc_dispatch(&self) -> Option<Function> {
        let (ps, rest) = match self.osc.split_once(';') {
            Some(parts) => parts,
            None => (self.osc.as_str(), ""),
        };

        match ps {
            "0" => Some(Function::SetTitle(TitleTarget::Both, rest.to_string())),
            "1" => Some(Function::SetTitle(TitleTarget::Icon, rest.to_string())),
            "2" => Some(Function::SetTitle(TitleTarget::Window, rest.to_string())),
            // OSC 8 ; params ; URI — a URI may itself contain `;`, so only the
            // params/URI split is taken here; an absent or empty URI closes
            // the link. Absurdly long URIs are dropped (the sequence is still
            // consumed) so hostile output can't bloat the intern table.
            "8" => {
                let uri = rest.split_once(';').map(|(_params, uri)| uri)?;
                Some(Function::Hyperlink(
                    (!uri.is_empty() && uri.len() <= MAX_HYPERLINK_LEN).then(|| uri.to_string()),
                ))
            }
            // OSC 52 ; Pc ; Pd — clipboard write (Pd = base64). Kept syntactic
            // here: the terminal decodes, picks targets from Pc, and ignores
            // the "?" query form. Oversized payloads are dropped whole.
            "52" => {
                let (selection, payload) = rest.split_once(';')?;
                (payload.len() <= MAX_CLIPBOARD_B64)
                    .then(|| Function::SetClipboard(selection.to_string(), payload.to_string()))
            }
            // OSC 9;4 — ConEmu/Windows Terminal taskbar progress. The other
            // OSC 9 sub-commands (desktop notifications, ConEmu extras) are
            // not implemented; dropped whole. A missing percentage reads as 0,
            // an out-of-range one clamps to 100, unknown states are ignored.
            "9" => {
                let mut parts = rest.split(';');
                if parts.next() != Some("4") {
                    return None;
                }
                let st = parts.next().unwrap_or("");
                let pct = parts
                    .next()
                    .and_then(|p| p.parse::<u32>().ok())
                    .map(|p| p.min(100) as u8)
                    .unwrap_or(0);
                let progress = match st {
                    "0" => None,
                    "1" => Some(Progress::Normal(pct)),
                    "2" => Some(Progress::Error(pct)),
                    "3" => Some(Progress::Indeterminate),
                    "4" => Some(Progress::Paused(pct)),
                    _ => return None,
                };
                Some(Function::SetProgress(progress))
            }
            // OSC 4 ; index ; spec [; index ; spec]… — set indexed palette
            // colors. A `?` spec is a query, which the host answers from the
            // live palette (see `ghost_vt::query`), so it sets nothing here;
            // unparseable specs (named X11 colors) and out-of-range indices are
            // skipped, leaving their neighbours in the same OSC untouched.
            // Indices past the palette (`SPECIAL_COLOR_BASE + c`) name the special
            // colors OSC 5 names directly; the terminal routes them.
            "4" => {
                let mut fields = rest.split(';');
                let mut set = Vec::new();
                while let (Some(index), Some(spec)) = (fields.next(), fields.next()) {
                    if let (Ok(i), Some(rgb)) = (index.parse::<u16>(), parse_color_spec(spec)) {
                        if palette_index_exists(i) {
                            set.push((i, rgb));
                        }
                    }
                }
                (!set.is_empty()).then_some(Function::SetPalette(set))
            }
            // OSC 5 ; c ; spec [; c ; spec]… — the same as OSC 4, addressing the
            // special colors from 0 rather than past the palette. It folds onto
            // the OSC 4 form, so both reach the terminal as one function.
            "5" => {
                let mut fields = rest.split(';');
                let mut set = Vec::new();
                while let (Some(code), Some(spec)) = (fields.next(), fields.next()) {
                    if let (Ok(c), Some(rgb)) = (code.parse::<u16>(), parse_color_spec(spec)) {
                        if SpecialColor::from_code(c).is_some() {
                            set.push((SPECIAL_COLOR_BASE + c, rgb));
                        }
                    }
                }
                (!set.is_empty()).then_some(Function::SetPalette(set))
            }
            // OSC 104 [; index]… — reset palette colors to the theme's; with no
            // index, the whole palette (`OSC 104` and `OSC 104 ;`, xterm's
            // reset-all, both arrive with an empty `rest`).
            "104" => Some(Function::ResetPalette(reset_indices(
                rest,
                |i| i,
                palette_index_exists,
            )?)),
            // OSC 105 [; c]… — reset special colors; with no code, all five. The
            // "all" case is spelled out rather than left empty, which OSC 104
            // already means (the whole indexed palette, special colors aside).
            "105" => Some(Function::ResetPalette(if rest.is_empty() {
                (0..5).map(|c| SPECIAL_COLOR_BASE + c).collect()
            } else {
                reset_indices(
                    rest,
                    |c| SPECIAL_COLOR_BASE + c,
                    |c| SpecialColor::from_code(c).is_some(),
                )?
            })),
            // OSC 10/11/12 — set a dynamic color. xterm's consecutive-code form
            // gives one OSC several specs, each setting the *next* color (`OSC 10 ;
            // fg ; bg`); codes past the cursor (12) name colors ghost doesn't have
            // (mouse fg/bg, highlight, …) and are dropped. The "?" query form is the
            // host's to answer, and specs we can't parse (named colors) are skipped
            // — without disturbing the ones beside them.
            "10" | "11" | "12" => {
                let first = match ps {
                    "10" => 0,
                    "11" => 1,
                    _ => 2,
                };
                let mut set = Vec::new();
                for (i, spec) in rest.split(';').enumerate() {
                    let target = match first + i {
                        0 => DynamicColor::Foreground,
                        1 => DynamicColor::Background,
                        2 => DynamicColor::Cursor,
                        _ => break,
                    };
                    if let Some(rgb) = parse_color_spec(spec) {
                        set.push((target, Some(rgb)));
                    }
                }
                (!set.is_empty()).then_some(Function::SetDynamicColor(set))
            }
            // OSC 110/111/112 — reset a dynamic color to the theme default.
            "110" => Some(Function::SetDynamicColor(vec![(
                DynamicColor::Foreground,
                None,
            )])),
            "111" => Some(Function::SetDynamicColor(vec![(
                DynamicColor::Background,
                None,
            )])),
            "112" => Some(Function::SetDynamicColor(vec![(
                DynamicColor::Cursor,
                None,
            )])),
            // OSC 133 — FinalTerm shell integration. The letter picks the
            // mark; anything after a `;` is extension parameters (kitty's
            // `k=s` and friends), accepted and dropped — except D's first
            // field, the command's exit code.
            "133" => {
                let (mark, params) = match rest.split_once(';') {
                    Some((m, p)) => (m, Some(p)),
                    None => (rest, None),
                };
                let mark = match mark {
                    "A" => PromptMark::PromptStart,
                    "B" => PromptMark::CommandStart,
                    "C" => PromptMark::OutputStart,
                    "D" => PromptMark::CommandDone(
                        params
                            .and_then(|p| p.split(';').next())
                            .and_then(|c| c.parse().ok()),
                    ),
                    _ => return None,
                };
                Some(Function::PromptMark(mark))
            }
            _ => None,
        }
    }

    pub(crate) fn dump(&self) -> String {
        use State::*;

        let mut seq = String::new();

        match self.state {
            Ground => {}

            Escape => {
                seq.push('\u{1b}');
            }

            EscapeIntermediate => {
                let intermediates = self.intermediate.iter().collect::<String>();
                let s = format!("\u{1b}{intermediates}");
                seq.push_str(&s);
            }

            CsiEntry => {
                seq.push('\u{9b}');
            }

            CsiParam => {
                let intermediates = self.intermediate.iter().collect::<String>();

                let params = &self.params[..=self.cur_param]
                    .iter()
                    .map(|param| param.to_string())
                    .collect::<Vec<_>>()
                    .join(";");

                let s = &format!("\u{9b}{intermediates}{params}");
                seq.push_str(s);
            }

            CsiIntermediate => {
                let intermediates = self.intermediate.iter().collect::<String>();
                let s = &format!("\u{9b}{intermediates}");
                seq.push_str(s);
            }

            CsiIgnore => {
                seq.push_str("\u{9b}\u{3a}");
            }

            DcsEntry => {
                seq.push('\u{90}');
            }

            DcsIntermediate => {
                let intermediates = self.intermediate.iter().collect::<String>();
                let s = &format!("\u{90}{intermediates}");
                seq.push_str(s);
            }

            DcsParam => {
                let intermediates = self.intermediate.iter().collect::<String>();

                let params = &self.params[..=self.cur_param]
                    .iter()
                    .map(|param| param.to_string())
                    .collect::<Vec<_>>()
                    .join(";");

                let s = &format!("\u{90}{intermediates}{params}");
                seq.push_str(s);
            }

            DcsPassthrough => {
                let intermediates = self.intermediate.iter().collect::<String>();
                let s = &format!("\u{90}{intermediates}\u{40}");
                seq.push_str(s);
            }

            DcsIgnore => {
                seq.push_str("\u{90}\u{3a}");
            }

            OscString => {
                // Re-emit the introducer AND the accumulated body so a resumed
                // parser reconstructs the same OSC (dump/resume must round-trip —
                // see prop_dump_resume_equivalence).
                seq.push('\u{9d}');
                seq.push_str(&self.osc);
            }

            SosPmApcString => {
                seq.push('\u{98}');
            }

            ApcString => {
                // Likewise re-emit the introducer and the accumulated graphics
                // payload so the resumed parser produces the identical
                // KittyGraphics command.
                seq.push('\u{9f}');
                if self.apc_overflow {
                    // The payload was already dropped (over the cap); emit a
                    // non-`G` sentinel so the resumed parser's dispatch is also a
                    // no-op, without re-emitting the megabytes we discarded.
                    seq.push(' ');
                } else {
                    seq.push_str(&self.apc);
                }
            }
        }

        seq
    }

    #[cfg(test)]
    pub fn assert_eq(&self, other: &Parser) {
        use State::*;

        assert_eq!(self.state, other.state);

        if self.state == CsiParam || self.state == DcsParam {
            assert_eq!(self.params, other.params);
        }

        if self.state == EscapeIntermediate
            || self.state == CsiIntermediate
            || self.state == CsiParam
            || self.state == DcsIntermediate
            || self.state == DcsParam
        {
            assert_eq!(self.intermediate, other.intermediate);
        }
    }
}

pub(crate) fn dump(funs: &[Function]) -> String {
    let mut seq = String::new();

    for fun in funs {
        dump_function(&mut seq, fun);
    }

    seq
}

pub(crate) fn dump_sgr_color(color: Color, base: u8) -> String {
    match color {
        Color::Indexed(c) if c < 8 => (base + c).to_string(),
        Color::Indexed(c) if c < 16 => (base + 52 + c).to_string(),
        Color::Indexed(c) => format!("{}:5:{}", base + 8, c),
        Color::RGB(c) => format!("{}:2:{}:{}:{}", base + 8, c.r, c.g, c.b),
    }
}

/// The SGR parameter (as a string) that reproduces one [`SgrOp`]. Shared by the
/// `Sgr` dump and the DECRQSS SGR report so both stay in lockstep.
pub(crate) fn sgr_op_param(op: &SgrOp) -> String {
    use SgrOp::*;
    match op {
        Reset => "0".to_owned(),
        SetBoldIntensity => "1".to_owned(),
        SetFaintIntensity => "2".to_owned(),
        SetItalic => "3".to_owned(),
        SetUnderline => "4".to_owned(),
        SetBlink => "5".to_owned(),
        SetInverse => "7".to_owned(),
        SetStrikethrough => "9".to_owned(),
        ResetIntensity => "22".to_owned(),
        ResetItalic => "23".to_owned(),
        ResetUnderline => "24".to_owned(),
        ResetBlink => "25".to_owned(),
        ResetInverse => "27".to_owned(),
        ResetStrikethrough => "29".to_owned(),
        SetForegroundColor(color) => dump_sgr_color(*color, 30),
        ResetForegroundColor => "39".to_owned(),
        SetBackgroundColor(color) => dump_sgr_color(*color, 40),
        ResetBackgroundColor => "49".to_owned(),
    }
}

fn dump_function(seq: &mut String, fun: &Function) {
    use CtcOp::*;
    use EdScope::*;
    use ElScope::*;
    use Function::*;
    use TbcScope::*;
    use XtwinopsOp::*;

    match fun {
        Bell => seq.push('\u{07}'),
        Bs => seq.push('\u{08}'),
        Cbt(n) => push_csi(seq, None, &[n.to_string()], 'Z'),
        Cha(n) => push_csi(seq, None, &[n.to_string()], 'G'),
        Cht(n) => push_csi(seq, None, &[n.to_string()], 'I'),
        Cnl(n) => push_csi(seq, None, &[n.to_string()], 'E'),
        Cpl(n) => push_csi(seq, None, &[n.to_string()], 'F'),
        Cr => seq.push('\r'),

        Ctc(op) => {
            let param = match op {
                Set => 0,
                ClearCurrentColumn => 2,
                ClearAll => 5,
            };

            push_csi(seq, None, &[param.to_string()], 'W');
        }

        Cub(n) => push_csi(seq, None, &[n.to_string()], 'D'),
        Cud(n) => push_csi(seq, None, &[n.to_string()], 'B'),
        Cuf(n) => push_csi(seq, None, &[n.to_string()], 'C'),
        Cup(row, col) => push_csi(seq, None, &[row.to_string(), col.to_string()], 'H'),
        Cuu(n) => push_csi(seq, None, &[n.to_string()], 'A'),
        Dch(n) => push_csi(seq, None, &[n.to_string()], 'P'),
        Decaln => push_esc(seq, Some('#'), '8'),
        Decbi => push_esc(seq, None, '6'),

        Decfi => push_esc(seq, None, '9'),

        Decrc => push_esc(seq, None, '8'),

        Decrst(modes) => {
            // Every `DecMode` discriminant is its DEC mode number (`#[repr(u16)]`).
            let params = modes
                .as_slice()
                .iter()
                .map(|mode| (*mode as u16).to_string())
                .collect::<Vec<_>>();

            push_csi(seq, Some('?'), &params, 'l');
        }

        Decsc => push_esc(seq, None, '7'),

        Decset(modes) => {
            let params = modes
                .as_slice()
                .iter()
                .map(|mode| (*mode as u16).to_string())
                .collect::<Vec<_>>();

            push_csi(seq, Some('?'), &params, 'h');
        }

        Decstbm(top, bottom) => {
            push_csi(seq, None, &[top.to_string(), bottom.to_string()], 'r');
        }

        Decslrm(left, right) => {
            push_csi(seq, None, &[left.to_string(), right.to_string()], 's');
        }

        Decic(n) => push_csi(seq, Some('\''), &[n.to_string()], '}'),
        Decdc(n) => push_csi(seq, Some('\''), &[n.to_string()], '~'),

        Decscl(level, controls) => {
            push_csi(
                seq,
                Some('"'),
                &[level.to_string(), controls.to_string()],
                'p',
            );
        }

        Decsca(ps) => push_csi(seq, Some('"'), &[ps.to_string()], 'q'),

        Decsera(pt, pl, pb, pr) => push_csi(
            seq,
            Some('$'),
            &[
                pt.to_string(),
                pl.to_string(),
                pb.to_string(),
                pr.to_string(),
            ],
            '{',
        ),

        Decera(pt, pl, pb, pr) => push_csi(
            seq,
            Some('$'),
            &[
                pt.to_string(),
                pl.to_string(),
                pb.to_string(),
                pr.to_string(),
            ],
            'z',
        ),

        Decfra(pch, pt, pl, pb, pr) => push_csi(
            seq,
            Some('$'),
            &[
                pch.to_string(),
                pt.to_string(),
                pl.to_string(),
                pb.to_string(),
                pr.to_string(),
            ],
            'x',
        ),

        Deccra(ps) => push_csi(seq, Some('$'), &ps.map(|p| p.to_string()), 'v'),

        Decstr => push_csi(seq, Some('!'), &[], 'p'),
        Dl(n) => push_csi(seq, None, &[n.to_string()], 'M'),
        Ech(n) => push_csi(seq, None, &[n.to_string()], 'X'),

        Ed(scope) => {
            let param = match scope {
                Below => 0,
                Above => 1,
                EdScope::All => 2,
                SavedLines => 3,
            };

            push_csi(seq, None, &[param.to_string()], 'J');
        }

        Decsed(scope) => {
            let param = match scope {
                Below => 0,
                Above => 1,
                EdScope::All => 2,
                SavedLines => 3,
            };

            push_csi(seq, Some('?'), &[param.to_string()], 'J');
        }

        El(scope) => {
            let param = match scope {
                ToRight => 0,
                ToLeft => 1,
                ElScope::All => 2,
            };

            push_csi(seq, None, &[param.to_string()], 'K');
        }

        Decsel(scope) => {
            let param = match scope {
                ToRight => 0,
                ToLeft => 1,
                ElScope::All => 2,
            };

            push_csi(seq, Some('?'), &[param.to_string()], 'K');
        }

        G1d4(charset) => push_esc(
            seq,
            Some(')'),
            match charset {
                Charset::Drawing => '0',
                Charset::Ascii => 'B',
            },
        ),

        Gzd4(charset) => push_esc(
            seq,
            Some('('),
            match charset {
                Charset::Drawing => '0',
                Charset::Ascii => 'B',
            },
        ),

        Ht => seq.push('\t'),
        Hts => push_esc(seq, None, 'H'),
        Ich(n) => push_csi(seq, None, &[n.to_string()], '@'),
        Il(n) => push_csi(seq, None, &[n.to_string()], 'L'),
        KittyKeyboardPush(flags) => push_csi(seq, Some('>'), &[flags.to_string()], 'u'),
        KittyKeyboardPop(n) => push_csi(seq, Some('<'), &[n.to_string()], 'u'),
        // Graphics commands are transient (applied to the image store, not part
        // of the cell grid); image state is re-emitted separately, not here.
        KittyGraphics(_) => {}
        KittyKeyboardSet(flags, mode) => {
            push_csi(seq, Some('='), &[flags.to_string(), mode.to_string()], 'u')
        }
        Lf => seq.push('\n'),
        ModifyOtherKeys(level) => {
            push_csi(seq, Some('>'), &["4".to_owned(), level.to_string()], 'm')
        }
        Nel => push_esc(seq, None, 'E'),
        Print(ch) => seq.push(*ch),
        Rep(n) => push_csi(seq, None, &[n.to_string()], 'b'),
        Ri => push_esc(seq, None, 'M'),
        Ris => push_esc(seq, None, 'c'),

        // SPA/EPA as their 7-bit `ESC V` / `ESC W` forms (reparsed as C1 via the
        // generic `ESC @..._` rule).
        Spa => push_esc(seq, None, 'V'),
        Epa => push_esc(seq, None, 'W'),

        Rm(modes) => {
            // Every `AnsiMode` discriminant is its ANSI mode number (`#[repr(u16)]`).
            let params = modes
                .as_slice()
                .iter()
                .map(|mode| (*mode as u16).to_string())
                .collect::<Vec<_>>();

            push_csi(seq, None, &params, 'l');
        }

        Scorc => push_csi(seq, None, &[], 'u'),
        Scosc => push_csi(seq, None, &[], 's'),
        Sd(n) => push_csi(seq, None, &[n.to_string()], 'T'),
        SetCursorStyle(ps) => {
            // DECSCUSR's space is an intermediate that follows the parameter
            // (`CSI Ps SP q`), unlike the `?`/`>`/`!` private-marker prefixes
            // push_csi emits, so build it by hand.
            seq.push('\u{1b}');
            seq.push('[');
            seq.push_str(&ps.to_string());
            seq.push(' ');
            seq.push('q');
        }
        SetTitle(target, title) => {
            let ps = match target {
                TitleTarget::Both => '0',
                TitleTarget::Icon => '1',
                TitleTarget::Window => '2',
            };
            seq.push_str("\u{1b}]");
            seq.push(ps);
            seq.push(';');
            seq.push_str(title);
            seq.push('\u{07}');
        }

        Hyperlink(uri) => {
            seq.push_str("\u{1b}]8;;");
            if let Some(uri) = uri {
                seq.push_str(uri);
            }
            seq.push_str("\u{1b}\\");
        }

        SetClipboard(selection, payload) => {
            seq.push_str("\u{1b}]52;");
            seq.push_str(selection);
            seq.push(';');
            seq.push_str(payload);
            seq.push('\u{07}');
        }

        SetProgress(progress) => {
            let (st, pct) = match progress {
                None => (0, 0),
                Some(Progress::Normal(p)) => (1, *p),
                Some(Progress::Error(p)) => (2, *p),
                Some(Progress::Indeterminate) => (3, 0),
                Some(Progress::Paused(p)) => (4, *p),
            };
            seq.push_str(&format!("\u{1b}]9;4;{st};{pct}\u{7}"));
        }

        // One OSC per color rather than the consecutive-code form: a dump is read
        // by our own parser, and per-color is what a partial (fg-only) state needs
        // anyway.
        SetDynamicColor(entries) => {
            for (target, rgb) in entries {
                let code = match target {
                    DynamicColor::Foreground => 10,
                    DynamicColor::Background => 11,
                    DynamicColor::Cursor => 12,
                };
                match rgb {
                    Some([r, g, b]) => {
                        seq.push_str(&format!("\u{1b}]{code};rgb:{r:02x}/{g:02x}/{b:02x}\u{7}"));
                    }
                    // Resets are OSC 110/111/112.
                    None => seq.push_str(&format!("\u{1b}]1{code}\u{7}")),
                }
            }
        }

        SetPalette(entries) => {
            let pairs: Vec<String> = entries
                .iter()
                .map(|(i, [r, g, b])| format!("{i};rgb:{r:02x}/{g:02x}/{b:02x}"))
                .collect();
            seq.push_str(&format!("\u{1b}]4;{}\u{7}", pairs.join(";")));
        }

        ResetPalette(indices) => {
            let list: Vec<String> = indices.iter().map(|i| i.to_string()).collect();
            seq.push_str(&format!("\u{1b}]104;{}\u{7}", list.join(";")));
        }

        PromptMark(mark) => {
            seq.push_str("\u{1b}]133;");
            match mark {
                crate::parser::PromptMark::PromptStart => seq.push('A'),
                crate::parser::PromptMark::CommandStart => seq.push('B'),
                crate::parser::PromptMark::OutputStart => seq.push('C'),
                crate::parser::PromptMark::CommandDone(code) => {
                    seq.push('D');
                    if let Some(code) = code {
                        seq.push(';');
                        seq.push_str(&code.to_string());
                    }
                }
            }
            seq.push('\u{07}');
        }

        Sgr(ops) => {
            if ops.is_empty() {
                // `CSI m` roundtrips to `Sgr([Reset])`, so we need a syntactically
                // valid but semantically incomplete SGR sequence for `Sgr([])`.
                seq.push_str("\x1b[38;2m");
            } else {
                let params = ops.as_slice().iter().map(sgr_op_param).collect::<Vec<_>>();

                push_csi(seq, None, &params, 'm');
            }
        }

        Si => seq.push('\u{0f}'),

        Sm(modes) => {
            let params = modes
                .as_slice()
                .iter()
                .map(|mode| (*mode as u16).to_string())
                .collect::<Vec<_>>();

            push_csi(seq, None, &params, 'h');
        }

        So => seq.push('\u{0e}'),
        Su(n) => push_csi(seq, None, &[n.to_string()], 'S'),

        Tbc(scope) => {
            let param = match scope {
                CurrentColumn => 0,
                TbcScope::All => 3,
            };

            push_csi(seq, None, &[param.to_string()], 'g');
        }

        Vpa(n) => push_csi(seq, None, &[n.to_string()], 'd'),
        Vpr(n) => push_csi(seq, None, &[n.to_string()], 'e'),

        Xtwinops(op) => {
            let params: Vec<String> = match op {
                Resize(cols, rows) => vec!["8".to_owned(), dim_param(*rows), dim_param(*cols)],
                ResizePixels(w, h) => vec!["4".to_owned(), dim_param(*h), dim_param(*w)],
                SetLines(rows) => vec![rows.to_string()],
                Deiconify => vec!["1".to_owned()],
                Iconify => vec!["2".to_owned()],
                Maximize(op) => vec![
                    "9".to_owned(),
                    match op {
                        MaximizeOp::Restore => "0",
                        MaximizeOp::Both => "1",
                        MaximizeOp::Vertically => "2",
                        MaximizeOp::Horizontally => "3",
                    }
                    .to_owned(),
                ],
                Fullscreen(op) => vec![
                    "10".to_owned(),
                    match op {
                        FullscreenOp::Leave => "0",
                        FullscreenOp::Enter => "1",
                        FullscreenOp::Toggle => "2",
                    }
                    .to_owned(),
                ],
                PushTitle(target) => vec!["22".to_owned(), title_ps(*target)],
                PopTitle(target) => vec!["23".to_owned(), title_ps(*target)],
            };
            push_csi(seq, None, &params, 't');
        }
    }
}

fn push_esc(seq: &mut String, intermediate: Option<char>, final_char: char) {
    seq.push('\u{1b}');

    if let Some(intermediate) = intermediate {
        seq.push(intermediate);
    }

    seq.push(final_char);
}

fn push_csi(seq: &mut String, intermediate: Option<char>, params: &[String], final_char: char) {
    seq.push('\u{1b}');
    seq.push('[');

    // ECMA-48 byte order is: private-marker prefix (`<=>?`, 0x3C–0x3F), then
    // parameter bytes, then intermediate bytes (0x20–0x2F), then the final byte.
    // Callers pass both kinds through `intermediate`; a marker sits before the
    // params, a true intermediate after (a param byte following an intermediate
    // is malformed and the parser would drop the whole sequence).
    let marker = intermediate.filter(|c| ('<'..='?').contains(c));
    let trailing = intermediate.filter(|c| !('<'..='?').contains(c));

    if let Some(marker) = marker {
        seq.push(marker);
    }

    if let Some((first, rest)) = params.split_first() {
        seq.push_str(first);

        for param in rest {
            seq.push(';');
            seq.push_str(param);
        }
    }

    if let Some(trailing) = trailing {
        seq.push(trailing);
    }

    seq.push(final_char);
}

fn ansi_mode(param: &Param) -> Option<AnsiMode> {
    use AnsiMode::*;

    match param.as_u16() {
        2 => Some(KeyboardAction),
        4 => Some(Insert),
        12 => Some(SendReceive),
        20 => Some(NewLine),
        _ => None,
    }
}

struct SgrOpsDecoder<'a> {
    ps: &'a [Param],
}

impl<'a> Iterator for SgrOpsDecoder<'a> {
    type Item = SgrOp;

    fn next(&mut self) -> Option<Self::Item> {
        use SgrOp::*;

        while let Some(param) = self.ps.first() {
            match param.parts() {
                [0] => {
                    self.ps = &self.ps[1..];

                    return Some(Reset);
                }

                [1] => {
                    self.ps = &self.ps[1..];

                    return Some(SetBoldIntensity);
                }

                [2] => {
                    self.ps = &self.ps[1..];

                    return Some(SetFaintIntensity);
                }

                [3] => {
                    self.ps = &self.ps[1..];

                    return Some(SetItalic);
                }

                [4] => {
                    self.ps = &self.ps[1..];

                    return Some(SetUnderline);
                }

                [5] => {
                    self.ps = &self.ps[1..];

                    return Some(SetBlink);
                }

                [7] => {
                    self.ps = &self.ps[1..];

                    return Some(SetInverse);
                }

                [9] => {
                    self.ps = &self.ps[1..];

                    return Some(SetStrikethrough);
                }

                [21] | [22] => {
                    self.ps = &self.ps[1..];

                    return Some(ResetIntensity);
                }

                [23] => {
                    self.ps = &self.ps[1..];

                    return Some(ResetItalic);
                }

                [24] => {
                    self.ps = &self.ps[1..];

                    return Some(ResetUnderline);
                }

                [25] => {
                    self.ps = &self.ps[1..];

                    return Some(ResetBlink);
                }

                [27] => {
                    self.ps = &self.ps[1..];

                    return Some(ResetInverse);
                }

                [29] => {
                    self.ps = &self.ps[1..];

                    return Some(ResetStrikethrough);
                }

                [param] if *param >= 30 && *param <= 37 => {
                    let color = Color::Indexed((param - 30) as u8);
                    self.ps = &self.ps[1..];

                    return Some(SetForegroundColor(color));
                }

                [38, 2, r, g, b] | [38, 2, _, r, g, b] => {
                    self.ps = &self.ps[1..];

                    return Some(SetForegroundColor(Color::rgb(*r as u8, *g as u8, *b as u8)));
                }

                [38, 5, idx] => {
                    let color = Color::Indexed(*idx as u8);
                    self.ps = &self.ps[1..];

                    return Some(SetForegroundColor(color));
                }

                [38] => match self.ps.get(1).map(|p| p.parts()) {
                    None => {
                        self.ps = &self.ps[1..];
                    }

                    Some([2]) => {
                        if let Some(b) = self.ps.get(4) {
                            let r = self.ps.get(2).unwrap().as_u16();
                            let g = self.ps.get(3).unwrap().as_u16();
                            let b = b.as_u16();
                            let color = Color::rgb(r as u8, g as u8, b as u8);
                            self.ps = &self.ps[5..];

                            return Some(SetForegroundColor(color));
                        } else {
                            self.ps = &self.ps[2..];
                        }
                    }

                    Some([5]) => {
                        if let Some(idx) = self.ps.get(2) {
                            let idx = idx.as_u16();
                            let color = Color::Indexed(idx as u8);
                            self.ps = &self.ps[3..];

                            return Some(SetForegroundColor(color));
                        } else {
                            self.ps = &self.ps[2..];
                        }
                    }

                    Some(_) => {
                        self.ps = &self.ps[1..];
                    }
                },

                [39] => {
                    self.ps = &self.ps[1..];

                    return Some(ResetForegroundColor);
                }

                [param] if *param >= 40 && *param <= 47 => {
                    let color = Color::Indexed((param - 40) as u8);
                    self.ps = &self.ps[1..];

                    return Some(SetBackgroundColor(color));
                }

                [48, 2, r, g, b] | [48, 2, _, r, g, b] => {
                    let color = Color::rgb(*r as u8, *g as u8, *b as u8);
                    self.ps = &self.ps[1..];

                    return Some(SetBackgroundColor(color));
                }

                [48, 5, idx] => {
                    let color = Color::Indexed(*idx as u8);
                    self.ps = &self.ps[1..];

                    return Some(SetBackgroundColor(color));
                }

                [48] => match self.ps.get(1).map(|p| p.parts()) {
                    None => {
                        self.ps = &self.ps[1..];
                    }

                    Some([2]) => {
                        if let Some(b) = self.ps.get(4) {
                            let r = self.ps.get(2).unwrap().as_u16();
                            let g = self.ps.get(3).unwrap().as_u16();
                            let b = b.as_u16();
                            let color = Color::rgb(r as u8, g as u8, b as u8);
                            self.ps = &self.ps[5..];

                            return Some(SetBackgroundColor(color));
                        } else {
                            self.ps = &self.ps[2..];
                        }
                    }

                    Some([5]) => {
                        if let Some(idx) = self.ps.get(2) {
                            let idx = idx.as_u16();
                            let color = Color::Indexed(idx as u8);
                            self.ps = &self.ps[3..];

                            return Some(SetBackgroundColor(color));
                        } else {
                            self.ps = &self.ps[2..];
                        }
                    }

                    Some(_) => {
                        self.ps = &self.ps[1..];
                    }
                },

                [49] => {
                    self.ps = &self.ps[1..];

                    return Some(ResetBackgroundColor);
                }

                [param] if *param >= 90 && *param <= 97 => {
                    let color = Color::Indexed((param - 90 + 8) as u8);
                    self.ps = &self.ps[1..];

                    return Some(SetForegroundColor(color));
                }

                [param] if *param >= 100 && *param <= 107 => {
                    let color = Color::Indexed((param - 100 + 8) as u8);
                    self.ps = &self.ps[1..];

                    return Some(SetBackgroundColor(color));
                }

                _ => {
                    self.ps = &self.ps[1..];
                }
            }
        }

        None
    }
}

fn dec_mode(param: &Param) -> Option<DecMode> {
    dec_mode_from(param.as_u16())
}

/// The tracked [`DecMode`] for a raw DEC private-mode number, if any.
pub(crate) fn dec_mode_from(param: u16) -> Option<DecMode> {
    use DecMode::*;

    match param {
        1 => Some(CursorKeys),
        3 => Some(Columns132),
        4 => Some(SmoothScroll),
        5 => Some(ReverseVideo),
        6 => Some(Origin),
        7 => Some(AutoWrap),
        18 => Some(PrintFormFeed),
        19 => Some(PrintExtent),
        25 => Some(TextCursorEnable),
        40 => Some(Allow80To132),
        42 => Some(NationalReplacementCharset),
        45 => Some(ReverseWrap),
        66 => Some(NumericKeypad),
        67 => Some(BackarrowKey),
        69 => Some(LeftRightMargin),
        95 => Some(NoClearOnColumnChange),
        47 => Some(AltScreenBuffer), // legacy variant of 1047
        1000 => Some(MouseReportX11),
        1002 => Some(MouseReportButton),
        1003 => Some(MouseReportAny),
        1004 => Some(FocusReport),
        1006 => Some(MouseSgr),
        1047 => Some(AltScreenBuffer),
        1048 => Some(SaveCursor),
        1049 => Some(SaveCursorAltScreenBuffer),
        2004 => Some(BracketedPaste),
        2026 => Some(SynchronizedOutput),
        2031 => Some(ColorSchemeReport),
        _ => None,
    }
}

/// A window-op dimension as a parameter: an omitted one is emitted omitted, so
/// it re-parses to the same `None` (keep this dimension).
fn dim_param(dim: Option<u16>) -> String {
    dim.map(|d| d.to_string()).unwrap_or_default()
}

/// The XTWINOPS `Ps` naming a title (see [`TitleTarget::from_ps`]).
fn title_ps(target: TitleTarget) -> String {
    match target {
        TitleTarget::Both => "0",
        TitleTarget::Icon => "1",
        TitleTarget::Window => "2",
    }
    .to_owned()
}

/// Whether an OSC 4 index names a color ghost has: one of the 256 indexed
/// colors, or one of the five special colors past them. Anything else (xterm's
/// mouse/Tek/highlight colors, or nonsense) is skipped, leaving its neighbours
/// in the same OSC untouched.
fn palette_index_exists(i: u16) -> bool {
    i < SPECIAL_COLOR_BASE || SpecialColor::from_code(i - SPECIAL_COLOR_BASE).is_some()
}

/// The colors an OSC 104/105 reset names, mapped into palette indices by `slot`
/// — dropping the ones that name a color ghost doesn't have, the way a set does.
///
/// `None` if the sequence named colors but none of them survived. An *empty*
/// list is how "reset all of them" reaches the terminal, so a reset that named
/// only colors we don't have must not collapse into it and wipe the palette.
fn reset_indices(
    rest: &str,
    slot: impl Fn(u16) -> u16,
    exists: impl Fn(u16) -> bool,
) -> Option<Vec<u16>> {
    if rest.is_empty() {
        return Some(Vec::new());
    }
    let indices: Vec<u16> = rest
        .split(';')
        .filter_map(|i| i.parse::<u16>().ok())
        .filter(|i| exists(*i))
        .map(slot)
        .collect();
    (!indices.is_empty()).then_some(indices)
}

/// Parse an XParseColor-style color spec to 8-bit RGB. `rgb:R/G/B` takes 1–4
/// hex digits per component, scaled by digit count; `#…` takes 3/6/9/12
/// digits, left-justified with the high byte kept (X11 semantics). Named
/// colors are not supported.
fn parse_color_spec(spec: &str) -> Option<[u8; 3]> {
    fn scaled(part: &str) -> Option<u8> {
        if part.is_empty() || part.len() > 4 {
            return None;
        }
        let v = u32::from_str_radix(part, 16).ok()?;
        let max = (1u32 << (4 * part.len() as u32)) - 1;
        Some(((v * 255 + max / 2) / max) as u8)
    }

    if let Some(rest) = spec.strip_prefix("rgb:") {
        let mut parts = rest.split('/');
        let r = scaled(parts.next()?)?;
        let g = scaled(parts.next()?)?;
        let b = scaled(parts.next()?)?;
        if parts.next().is_some() {
            return None;
        }
        return Some([r, g, b]);
    }

    let hex = spec.strip_prefix('#')?;
    let n = hex.len() / 3;
    if hex.len() != n * 3 || !(1..=4).contains(&n) {
        return None;
    }
    let mut out = [0u8; 3];
    for (i, byte) in out.iter_mut().enumerate() {
        let v = u32::from_str_radix(&hex[i * n..(i + 1) * n], 16).ok()?;
        *byte = ((v << (16 - 4 * n as u32)) >> 8) as u8;
    }
    Some(out)
}

/// Upper bound on an accepted OSC 8 URI. Generous (browsers cap around 2k;
/// kitty at 2083) while keeping a hostile stream from interning megabytes.
const MAX_HYPERLINK_LEN: usize = 4096;

/// Upper bound on an accepted OSC 52 base64 payload (kitty caps at 8 MiB).
const MAX_CLIPBOARD_B64: usize = 8 * 1024 * 1024;

const MAX_PARAM_LEN: usize = 6;

#[derive(Debug, PartialEq, Clone)]
struct Param {
    cur_part: usize,
    pub parts: [u16; MAX_PARAM_LEN],
    /// Whether the sequence actually carried a digit here. An *omitted* parameter
    /// and an explicit `0` both read as zero, and for most controls that is the
    /// same thing — but not for the window ops, where an omitted dimension keeps
    /// the one it has and a zero means "as big as the display" (`CSI 8 t`).
    given: bool,
}

impl Param {
    pub fn new(number: u16) -> Self {
        Self {
            cur_part: 0,
            parts: [number, 0, 0, 0, 0, 0],
            given: false,
        }
    }

    pub fn clear(&mut self) {
        self.parts[..=self.cur_part].fill(0);
        self.cur_part = 0;
        self.given = false;
    }

    pub fn add_part(&mut self) {
        self.cur_part = (self.cur_part + 1).min(5);
    }

    pub fn add_digit(&mut self, input: u8) {
        let number = &mut self.parts[self.cur_part];
        *number = (10 * (*number as u32) + (input as u32)) as u16;
        self.given = true;
    }

    /// The value, or `None` if the sequence left this parameter out (see `given`).
    pub fn given(&self) -> Option<u16> {
        self.given.then_some(self.parts[0])
    }

    pub fn as_u16(&self) -> u16 {
        self.parts[0]
    }

    pub fn parts(&self) -> &[u16] {
        &self.parts[..=self.cur_part]
    }
}

impl Display for Param {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // A parameter the sequence left out is written out left out — it reads as
        // zero either way for most controls, but the window ops tell them apart
        // (see `Param::given`), and a dump of a half-parsed CSI must resume into
        // the parser state it left.
        if !self.given {
            return Ok(());
        }
        match self.parts() {
            [] => unreachable!(),

            [part] => write!(f, "{}", part),

            [first, rest @ ..] => {
                write!(f, "{first}")?;

                for part in rest {
                    write!(f, ":{part}")?;
                }

                Ok(())
            }
        }
    }
}

impl Default for Param {
    fn default() -> Self {
        Self::new(0)
    }
}

impl From<u16> for Param {
    fn from(value: u16) -> Self {
        Self::new(value)
    }
}

impl From<Vec<u16>> for Param {
    fn from(values: Vec<u16>) -> Self {
        let mut parts = [0u16; MAX_PARAM_LEN];
        let mut cur_part = 0;

        for (i, v) in values.iter().take(MAX_PARAM_LEN).enumerate() {
            cur_part = i;
            parts[i] = *v;
        }

        Self {
            cur_part,
            parts,
            given: true,
        }
    }
}

impl PartialEq<u16> for Param {
    fn eq(&self, other: &u16) -> bool {
        self.parts[0] == *other
    }
}

impl PartialEq<Vec<u16>> for Param {
    fn eq(&self, other: &Vec<u16>) -> bool {
        self.parts[..=self.cur_part] == other[..]
    }
}

#[cfg(test)]
mod tests {
    use super::AnsiMode;
    use super::AnsiModes;
    use super::CtcOp;
    use super::DecMode;
    use super::DecModes;
    use super::EdScope;
    use super::ElScope;
    use super::Function;
    use super::Function::*;
    use super::Parser;
    use super::SgrOp::*;
    use super::SgrOps;
    use super::State;
    use super::TbcScope;
    use super::{FullscreenOp, MaximizeOp, TitleTarget, XtwinopsOp};
    use crate::charset::Charset;
    use crate::color::Color;
    use proptest::prelude::*;

    fn parse(s: &str) -> Vec<Function> {
        let mut parser = Parser::new();

        s.chars().filter_map(|ch| parser.feed(ch)).collect()
    }

    fn emit(parser: &mut Parser, input: &[char]) -> Vec<Function> {
        input.iter().filter_map(|ch| parser.feed(*ch)).collect()
    }

    fn feed(parser: &mut Parser, s: &str) {
        for ch in s.chars() {
            assert_eq!(parser.feed(ch), None);
        }
    }

    fn sgr_ops<const N: usize>(ops: [super::SgrOp; N]) -> SgrOps {
        SgrOps::from(&ops[..])
    }

    fn ansi_modes<const N: usize>(modes: [AnsiMode; N]) -> AnsiModes {
        AnsiModes::from(&modes[..])
    }

    fn dec_modes<const N: usize>(modes: [DecMode; N]) -> DecModes {
        DecModes::from(&modes[..])
    }

    fn assert_dump(input: &str, state: State, dump: &str) {
        let mut parser = Parser::new();

        feed(&mut parser, input);

        assert_eq!(parser.state, state);
        assert_eq!(parser.dump(), dump);
    }

    fn gen_parser_char() -> impl Strategy<Value = char> {
        prop_oneof![
            prop::sample::select(vec![
                '\x1b', '\x18', '\x1a', '\u{9b}', '\u{9c}', '\u{9d}', '\u{90}', '\u{98}', '\u{9e}',
                '\u{9f}', '[', ']', 'P', 'X', '^', '_', '?', '!', ';', ':', ' ', '#', '(', ')',
                '@', 'A', 'B', 'C', 'D', 'H', 'J', 'K', 'L', 'M', 'P', 'S', 'T', 'W', 'X', 'Z',
                '`', 'a', 'b', 'd', 'e', 'f', 'g', 'h', 'l', 'm', 'p', 'r', 's', 't', 'u', '0',
                '1', '2', '3', '4', '5', '6', '7', '8', '9', '\x08', '\x09', '\x0a', '\x0d',
                '\x0e', '\x0f',
            ]),
            (0x20u8..=0x7eu8).prop_map(|b| b as char),
            prop::sample::select(vec!['日', '▒', 'ハ']),
        ]
    }

    fn gen_parser_input(max_len: usize) -> impl Strategy<Value = Vec<char>> {
        prop::collection::vec(gen_parser_char(), 0..=max_len)
    }

    fn gen_printable_text(max_len: usize) -> impl Strategy<Value = Vec<char>> {
        prop::collection::vec(
            prop_oneof![
                (0x20u8..=0x7eu8).prop_map(|b| b as char),
                prop::sample::select(vec!['日', '▒', 'ハ']),
            ],
            0..=max_len,
        )
    }

    fn gen_non_ground_prefix() -> impl Strategy<Value = Vec<char>> {
        prop_oneof![
            Just("\x1b".chars().collect()),
            Just("\x1b[".chars().collect()),
            Just("\x1b[12".chars().collect()),
            Just("\x1b[1;".chars().collect()),
            Just("\x1b[?".chars().collect()),
            Just("\x1b[ ".chars().collect()),
            Just("\x1b[:".chars().collect()),
            Just("\x1b(".chars().collect()),
            Just("\x1b]".chars().collect()),
            Just("\x1b]title".chars().collect()),
            Just("\x1bP".chars().collect()),
            Just("\x1bP1;2".chars().collect()),
            Just("\x1bP:".chars().collect()),
            Just("\x1bX".chars().collect()),
            Just("\x1bXabc".chars().collect()),
            Just("\u{9b}".chars().collect()),
            Just("\u{9b}12".chars().collect()),
            Just("\u{9d}title".chars().collect()),
            Just("\u{90}".chars().collect()),
            Just("\u{90}1;2".chars().collect()),
            Just("\u{98}".chars().collect()),
            Just("\u{98}abc".chars().collect()),
        ]
    }

    #[test]
    fn parse_c0() {
        assert_eq!(parse("\x07"), [Bell]);
        assert_eq!(parse("\x08"), [Bs]);
        assert_eq!(parse("\x0a"), [Lf]);
        assert_eq!(parse("\x0d"), [Cr]);
        assert_eq!(parse("\x0e"), [So]);
        assert_eq!(parse("\x0f"), [Si]);
    }

    #[test]
    fn bel_rings_only_in_ground_state_not_as_osc_terminator() {
        // A lone BEL is a bell.
        assert_eq!(parse("\x07"), [Bell]);
        // A BEL terminating an OSC string is a string terminator, not a bell: it
        // must yield the OSC's function (here a title set) and no Bell.
        assert_eq!(
            parse("\x1b]2;hi\x07"),
            [SetTitle(TitleTarget::Window, "hi".to_string())]
        );
    }

    #[test]
    fn parse_c1() {
        assert_eq!(parse("\u{84}"), [Lf]);
        assert_eq!(parse("\u{85}"), [Nel]);
        assert_eq!(parse("\u{88}"), [Hts]);
        assert_eq!(parse("\u{8d}"), [Ri]);
    }

    #[test]
    fn parse_esc_seq() {
        assert_eq!(parse("\x1b7"), [Decsc]);
        assert_eq!(parse("\x1b8"), [Decrc]);
        assert_eq!(parse("\x1bc"), [Ris]);
        assert_eq!(parse("\x1bM"), [Ri]);
        assert_eq!(parse("\x1b#8"), [Decaln]);
        assert_eq!(parse("\x1b6"), [Decbi]);
        assert_eq!(parse("\x1b9"), [Decfi]);
        assert_eq!(parse("\x1b(0"), [Gzd4(Charset::Drawing)]);
        assert_eq!(parse("\x1b(B"), [Gzd4(Charset::Ascii)]);
        assert_eq!(parse("\x1b)0"), [G1d4(Charset::Drawing)]);
        assert_eq!(parse("\x1b)B"), [G1d4(Charset::Ascii)]);
    }

    #[test]
    fn parse_csi_seq() {
        // Cursor movement and positioning.
        assert_eq!(parse("\x1b[A"), [Cuu(0)]);
        assert_eq!(parse("\x1b[B"), [Cud(0)]);
        assert_eq!(parse("\x1b[2B"), [Cud(2)]);
        assert_eq!(parse("\x1b[C"), [Cuf(0)]);
        assert_eq!(parse("\x1b[3C"), [Cuf(3)]);
        assert_eq!(parse("\x1b[D"), [Cub(0)]);
        assert_eq!(parse("\x1b[4D"), [Cub(4)]);
        assert_eq!(parse("\x1b[5E"), [Cnl(5)]);
        assert_eq!(parse("\x1b[6F"), [Cpl(6)]);
        assert_eq!(parse("\x1b[G"), [Cha(0)]);
        assert_eq!(parse("\x1b[7G"), [Cha(7)]);
        assert_eq!(parse("\x1b[H"), [Cup(0, 0)]);
        assert_eq!(parse("\x1b[3;4H"), [Cup(3, 4)]);
        assert_eq!(parse("\u{9b}3;4H"), [Cup(3, 4)]);
        assert_eq!(parse("\x1b[8I"), [Cht(8)]);
        assert_eq!(parse("\x1b[2Z"), [Cbt(2)]);
        assert_eq!(parse("\x1b[`"), [Cha(0)]);
        assert_eq!(parse("\x1b[9`"), [Cha(9)]);
        assert_eq!(parse("\x1b[10a"), [Cuf(10)]);
        assert_eq!(parse("\x1b[11b"), [Rep(11)]);
        assert_eq!(parse("\x1b[12d"), [Vpa(12)]);
        assert_eq!(parse("\x1b[13e"), [Vpr(13)]);
        assert_eq!(parse("\x1b[f"), [Cup(0, 0)]);
        assert_eq!(parse("\x1b[14;15f"), [Cup(14, 15)]);

        // Erase, insert/delete, scrolling, and tab control.
        assert_eq!(parse("\x1b[@"), [Ich(0)]);
        assert_eq!(parse("\x1b[J"), [Ed(EdScope::Below)]);
        assert_eq!(parse("\x1b[0J"), [Ed(EdScope::Below)]);
        assert_eq!(parse("\x1b[1J"), [Ed(EdScope::Above)]);
        assert_eq!(parse("\x1b[2J"), [Ed(EdScope::All)]);
        assert_eq!(parse("\u{9b}2J"), [Ed(EdScope::All)]);
        assert_eq!(parse("\x1b[3J"), [Ed(EdScope::SavedLines)]);
        assert_eq!(parse("\x1b[?J"), [Decsed(EdScope::Below)]);
        assert_eq!(parse("\x1b[?1J"), [Decsed(EdScope::Above)]);
        assert_eq!(parse("\x1b[?2J"), [Decsed(EdScope::All)]);
        assert_eq!(parse("\x1b[K"), [El(ElScope::ToRight)]);
        assert_eq!(parse("\x1b[0K"), [El(ElScope::ToRight)]);
        assert_eq!(parse("\x1b[1K"), [El(ElScope::ToLeft)]);
        assert_eq!(parse("\x1b[2K"), [El(ElScope::All)]);
        assert_eq!(parse("\x1b[?K"), [Decsel(ElScope::ToRight)]);
        assert_eq!(parse("\x1b[?1K"), [Decsel(ElScope::ToLeft)]);
        assert_eq!(parse("\x1b[?2K"), [Decsel(ElScope::All)]);
        assert_eq!(parse("\x1b[16L"), [Il(16)]);
        assert_eq!(parse("\x1b[17M"), [Dl(17)]);
        assert_eq!(parse("\x1b[18P"), [Dch(18)]);
        assert_eq!(parse("\x1b[19S"), [Su(19)]);
        assert_eq!(parse("\x1b[20T"), [Sd(20)]);
        assert_eq!(parse("\x1b[W"), [Ctc(CtcOp::Set)]);
        assert_eq!(parse("\x1b[2W"), [Ctc(CtcOp::ClearCurrentColumn)]);
        assert_eq!(parse("\x1b[5W"), [Ctc(CtcOp::ClearAll)]);
        assert_eq!(parse("\x1b[21X"), [Ech(21)]);
        assert_eq!(parse("\x1b[g"), [Tbc(TbcScope::CurrentColumn)]);
        assert_eq!(parse("\x1b[3g"), [Tbc(TbcScope::All)]);

        // ANSI mode setting and generic CSI operations.
        assert_eq!(
            parse("\x1b[4;20h"),
            [Sm(ansi_modes([AnsiMode::Insert, AnsiMode::NewLine]))]
        );
        assert_eq!(
            parse("\x1b[4;20l"),
            [Rm(ansi_modes([AnsiMode::Insert, AnsiMode::NewLine]))]
        );
        assert_eq!(parse("\x1b[m"), [Sgr(sgr_ops([Reset]))]);
        assert_eq!(parse("\x1b[2;5r"), [Decstbm(2, 5)]);
        assert_eq!(
            parse("\x1b[8;24;80t"),
            [Xtwinops(XtwinopsOp::Resize(Some(80), Some(24)))]
        );
        assert_eq!(parse("\x1b[2t"), [Xtwinops(XtwinopsOp::Iconify)]);
        assert_eq!(parse("\x1b[1t"), [Xtwinops(XtwinopsOp::Deiconify)]);
        assert_eq!(
            parse("\x1b[9;1t"),
            [Xtwinops(XtwinopsOp::Maximize(MaximizeOp::Both))]
        );
        assert_eq!(
            parse("\x1b[10;2t"),
            [Xtwinops(XtwinopsOp::Fullscreen(FullscreenOp::Toggle))]
        );
        // A lone Ps of 24 or more is DECSLPP, a page height — not a window op.
        assert_eq!(parse("\x1b[30t"), [Xtwinops(XtwinopsOp::SetLines(30))]);
        // The reports are the host's to answer, and a move is not ours to make.
        assert_eq!(parse("\x1b[11t"), []);
        assert_eq!(parse("\x1b[19t"), []);
        assert_eq!(parse("\x1b[3;10;20t"), []);
        // `CSI s` parses to DECSLRM(0,0); the terminal treats it as SCOSC when
        // DECLRMM is off, or as a reset-to-full-margins when it is on.
        assert_eq!(parse("\x1b[s"), [Decslrm(0, 0)]);
        assert_eq!(parse("\x1b[5;10s"), [Decslrm(5, 10)]);
        assert_eq!(parse("\x1b[u"), [Scorc]);
        assert_eq!(parse("\x1b['}"), [Decic(0)]);
        assert_eq!(parse("\x1b[3'}"), [Decic(3)]);
        assert_eq!(parse("\x1b['~"), [Decdc(0)]);
        assert_eq!(parse("\x1b[3'~"), [Decdc(3)]);
        assert_eq!(parse("\x1b[64;1\"p"), [Decscl(64, 1)]);
        assert_eq!(parse("\x1b[61\"p"), [Decscl(61, 0)]);
        assert_eq!(parse("\x1b[!p"), [Decstr]);

        // DECSCA and the SPA/EPA guarded-area controls (7-bit ESC V / ESC W).
        assert_eq!(parse("\x1b[\"q"), [Decsca(0)]);
        assert_eq!(parse("\x1b[1\"q"), [Decsca(1)]);
        assert_eq!(parse("\x1b[2;3;5;7${"), [Decsera(2, 3, 5, 7)]);
        assert_eq!(parse("\x1b[2;3;5;7$z"), [Decera(2, 3, 5, 7)]);
        assert_eq!(parse("\x1b[37;2;3;5;7$x"), [Decfra(37, 2, 3, 5, 7)]);
        assert_eq!(
            parse("\x1b[2;3;5;7;1;9;9;1$v"),
            [Deccra([2, 3, 5, 7, 1, 9, 9, 1])]
        );
        assert_eq!(parse("\x1bV"), [Spa]);
        assert_eq!(parse("\x1bW"), [Epa]);
        assert_eq!(parse("\u{96}"), [Spa]);
        assert_eq!(parse("\u{97}"), [Epa]);

        // DEC private modes.
        assert_eq!(parse("\x1b[?7h"), [Decset(dec_modes([DecMode::AutoWrap]))]);
        assert_eq!(parse("\u{9b}?7h"), [Decset(dec_modes([DecMode::AutoWrap]))]);
        assert_eq!(
            parse("\x1b[?45h"),
            [Decset(dec_modes([DecMode::ReverseWrap]))]
        );
        assert_eq!(
            parse("\x1b[?45l"),
            [Decrst(dec_modes([DecMode::ReverseWrap]))]
        );
        assert_eq!(
            parse("\x1b[?6;1047h"),
            [Decset(dec_modes([
                DecMode::Origin,
                DecMode::AltScreenBuffer
            ]))]
        );
        assert_eq!(
            parse("\x1b[?47h"),
            [Decset(dec_modes([DecMode::AltScreenBuffer]))]
        );
        assert_eq!(
            parse("\x1b[?1049h"),
            [Decset(dec_modes([DecMode::SaveCursorAltScreenBuffer]))]
        );
        assert_eq!(
            parse("\u{9b}?1049h"),
            [Decset(dec_modes([DecMode::SaveCursorAltScreenBuffer]))]
        );
        assert_eq!(parse("\x1b[?7l"), [Decrst(dec_modes([DecMode::AutoWrap]))]);
        assert_eq!(parse("\u{9b}?7l"), [Decrst(dec_modes([DecMode::AutoWrap]))]);
        assert_eq!(
            parse("\x1b[?47l"),
            [Decrst(dec_modes([DecMode::AltScreenBuffer]))]
        );
        assert_eq!(
            parse("\x1b[?6;1049l"),
            [Decrst(dec_modes([
                DecMode::Origin,
                DecMode::SaveCursorAltScreenBuffer,
            ]))]
        );
        assert_eq!(
            parse("\u{9b}?6;1049l"),
            [Decrst(dec_modes([
                DecMode::Origin,
                DecMode::SaveCursorAltScreenBuffer,
            ]))]
        );
    }

    #[test]
    fn parse_partial_and_interrupted_seq() {
        let mut parser = Parser::new();

        assert_eq!(parser.feed('\x1b'), None);
        assert_eq!(parser.feed('['), None);
        assert_eq!(parser.feed('3'), None);
        assert_eq!(parser.feed(';'), None);
        assert_eq!(parser.feed('4'), None);
        assert_eq!(parser.feed('H'), Some(Cup(3, 4)));

        assert_eq!(parser.feed('\x1b'), None);
        assert_eq!(parser.feed('['), None);
        assert_eq!(parser.feed('3'), None);
        assert_eq!(parser.feed('\x1b'), None);
        assert_eq!(parser.feed('M'), Some(Ri));

        feed(&mut parser, "\x1b");
        assert_eq!(parser.state, State::Escape);
        assert_eq!(parser.feed('\u{18}'), None);
        assert_eq!(parser.state, State::Ground);
        assert_eq!(parser.feed('A'), Some(Print('A')));

        feed(&mut parser, "\x1b[12");
        assert_eq!(parser.state, State::CsiParam);
        assert_eq!(parser.feed('\u{1a}'), None);
        assert_eq!(parser.state, State::Ground);
        assert_eq!(parser.feed('B'), Some(Print('B')));

        feed(&mut parser, "\x1b]title");
        assert_eq!(parser.state, State::OscString);
        assert_eq!(parser.feed('\u{18}'), None);
        assert_eq!(parser.state, State::Ground);
        assert_eq!(parser.feed('C'), Some(Print('C')));

        feed(&mut parser, "\x1bP1;2");
        assert_eq!(parser.state, State::DcsParam);
        assert_eq!(parser.feed('\u{1a}'), None);
        assert_eq!(parser.state, State::Ground);
        assert_eq!(parser.feed('D'), Some(Print('D')));
    }

    #[test]
    fn parse_non_display_modes() {
        use DecMode::*;
        assert_eq!(parse("\x1b[?1000h"), [Decset(dec_modes([MouseReportX11]))]);
        assert_eq!(
            parse("\x1b[?1002h"),
            [Decset(dec_modes([MouseReportButton]))]
        );
        assert_eq!(parse("\x1b[?1003h"), [Decset(dec_modes([MouseReportAny]))]);
        assert_eq!(parse("\x1b[?1004h"), [Decset(dec_modes([FocusReport]))]);
        assert_eq!(parse("\x1b[?1006h"), [Decset(dec_modes([MouseSgr]))]);
        assert_eq!(parse("\x1b[?2004h"), [Decset(dec_modes([BracketedPaste]))]);
        assert_eq!(parse("\x1b[?1000l"), [Decrst(dec_modes([MouseReportX11]))]);
    }

    #[test]
    fn parse_modify_other_keys() {
        // XTMODKEYS resource 4: set levels 1 and 2.
        assert_eq!(parse("\x1b[>4;1m"), [ModifyOtherKeys(1)]);
        assert_eq!(parse("\x1b[>4;2m"), [ModifyOtherKeys(2)]);
        // Explicit off, and the no-Pv reset form (Pv defaults to 0).
        assert_eq!(parse("\x1b[>4;0m"), [ModifyOtherKeys(0)]);
        assert_eq!(parse("\x1b[>4m"), [ModifyOtherKeys(0)]);
        // Other XTMODKEYS resources (Pp != 4) are not tracked.
        assert_eq!(parse("\x1b[>1;2m"), []);
        assert_eq!(parse("\x1b[>2;2m"), []);
    }

    #[test]
    fn parse_kitty_keyboard() {
        // Push flags (flags default 0 when omitted).
        assert_eq!(parse("\x1b[>1u"), [KittyKeyboardPush(1)]);
        assert_eq!(parse("\x1b[>u"), [KittyKeyboardPush(0)]);
        // Pop n entries (n defaults to 1; an explicit 0 is also treated as 1).
        assert_eq!(parse("\x1b[<3u"), [KittyKeyboardPop(3)]);
        assert_eq!(parse("\x1b[<u"), [KittyKeyboardPop(1)]);
        // Set flags with a mode (mode defaults to 1 = set-exact).
        assert_eq!(parse("\x1b[=5;3u"), [KittyKeyboardSet(5, 3)]);
        assert_eq!(parse("\x1b[=5u"), [KittyKeyboardSet(5, 1)]);
        // The query (CSI ? u) changes no state — it is answered by the query
        // scanner, not turned into a Function here.
        assert_eq!(parse("\x1b[?u"), []);
        // A bare CSI u is still SCO restore-cursor, not a kitty sequence.
        assert_eq!(parse("\x1b[u"), [Scorc]);
    }

    #[test]
    fn parse_kitty_graphics_apc() {
        // `ESC _ G <control>;<payload> ST` carries a kitty graphics command; the
        // emitted Function holds everything after the leading `G`. ST = ESC \.
        assert_eq!(
            parse("\x1b_Gi=31,a=q,s=1,v=1,f=24;AAAA\x1b\\"),
            [KittyGraphics("i=31,a=q,s=1,v=1,f=24;AAAA".to_string())]
        );
        // The C1 APC introducer (0x9f) and C1 ST (0x9c) work too.
        assert_eq!(
            parse("\u{9f}Ga=q;AA\u{9c}"),
            [KittyGraphics("a=q;AA".to_string())]
        );
        // A control-data-only command (no payload) is fine.
        assert_eq!(
            parse("\x1b_Ga=d,d=A\x1b\\"),
            [KittyGraphics("a=d,d=A".to_string())]
        );
        // An APC that is not a graphics command (no leading `G`) is discarded.
        assert_eq!(parse("\x1b_Zhello\x1b\\"), []);
        // SOS and PM strings remain discarded — only APC carries graphics.
        assert_eq!(parse("\x1bXsos data\x1b\\"), []);
        assert_eq!(parse("\x1b^pm data\x1b\\"), []);
        // An oversized payload is dropped (bounded accumulator), and the parser
        // still returns to the ground state cleanly.
        let huge = format!("\x1b_G{}\x1b\\", "A".repeat(super::MAX_APC_LEN + 16));
        assert_eq!(parse(&huge), []);
        let mut p = Parser::new();
        feed(&mut p, &huge);
        assert_eq!(p.state, State::Ground);
    }

    #[test]
    fn parse_osc_title() {
        // OSC 0 (icon + title) and OSC 2 (title) both set the window title,
        // terminated by BEL, C1 ST, or ESC \.
        assert_eq!(
            parse("\x1b]0;my title\x07"),
            [SetTitle(TitleTarget::Both, "my title".to_string())]
        );
        assert_eq!(
            parse("\x1b]2;my title\x07"),
            [SetTitle(TitleTarget::Window, "my title".to_string())]
        );
        assert_eq!(
            parse("\x1b]2;via st\x1b\\"),
            [SetTitle(TitleTarget::Window, "via st".to_string())]
        );
        assert_eq!(
            parse("\u{9d}2;via c1\u{9c}"),
            [SetTitle(TitleTarget::Window, "via c1".to_string())]
        );
        // A title may contain ';' — only the first separator is the code.
        assert_eq!(
            parse("\x1b]2;a;b;c\x07"),
            [SetTitle(TitleTarget::Window, "a;b;c".to_string())]
        );
        // An empty title clears it.
        assert_eq!(
            parse("\x1b]2;\x07"),
            [SetTitle(TitleTarget::Window, String::new())]
        );
        // OSC 1 sets the icon label alone — the title ghost keeps but never shows,
        // and reports back through `CSI 20 t`.
        assert_eq!(
            parse("\x1b]1;icon\x07"),
            [SetTitle(TitleTarget::Icon, "icon".to_string())]
        );
    }

    #[test]
    fn parse_osc_palette() {
        assert_eq!(
            parse("\x1b]4;1;rgb:ffff/0000/0000\x07"),
            [SetPalette(vec![(1, [0xff, 0x00, 0x00])])]
        );
        // Several pairs in one OSC; a query pair sets nothing, and a bad spec or
        // an out-of-range index drops that pair alone.
        assert_eq!(
            parse("\x1b]4;1;#ff0000;2;?;300;#fff;3;#00ff00\x1b\\"),
            [SetPalette(vec![(1, [0xff, 0, 0]), (3, [0, 0xff, 0])])]
        );
        // An all-query OSC 4 is not a set at all (the host answers it).
        assert_eq!(parse("\x1b]4;1;?\x07"), []);
        // OSC 104: named indices, or all of them.
        assert_eq!(parse("\x1b]104;1;2\x07"), [ResetPalette(vec![1, 2])]);
        assert_eq!(parse("\x1b]104\x07"), [ResetPalette(vec![])]);
        assert_eq!(parse("\x1b]104;\x07"), [ResetPalette(vec![])]);
    }

    #[test]
    fn parse_osc_clipboard() {
        let set = |sel: &str, b64: &str| SetClipboard(sel.to_string(), b64.to_string());
        assert_eq!(parse("\x1b]52;c;Zm9v\x07"), [set("c", "Zm9v")]);
        assert_eq!(parse("\x1b]52;;Zm9v\x1b\\"), [set("", "Zm9v")]);
        // The query form is carried through; the terminal ignores it.
        assert_eq!(parse("\x1b]52;c;?\x07"), [set("c", "?")]);
        // No Pc/Pd separator is malformed.
        assert_eq!(parse("\x1b]52;Zm9v\x07"), []);
    }

    #[test]
    fn parse_osc_hyperlink() {
        let link = |uri: &str| Hyperlink(Some(uri.to_string()));
        // ST- and BEL-terminated; params (id=…) accepted and dropped.
        assert_eq!(parse("\x1b]8;;https://a\x1b\\"), [link("https://a")]);
        assert_eq!(parse("\x1b]8;;https://a\x07"), [link("https://a")]);
        assert_eq!(parse("\x1b]8;id=x;https://a\x07"), [link("https://a")]);
        // A URI may contain ';' — only the params/URI split is taken.
        assert_eq!(
            parse("\x1b]8;;https://a?x=1;y=2\x07"),
            [link("https://a?x=1;y=2")]
        );
        // The empty URI closes the link; a missing separator is malformed.
        assert_eq!(parse("\x1b]8;;\x07"), [Hyperlink(None)]);
        assert_eq!(parse("\x1b]8;id=x;\x07"), [Hyperlink(None)]);
        assert_eq!(parse("\x1b]8;\x07"), []);
        // An absurdly long URI is dropped, closing the link instead of
        // interning a hostile payload.
        let huge = format!("\x1b]8;;https://a/{}\x07", "x".repeat(5000));
        assert_eq!(parse(&huge), [Hyperlink(None)]);
    }

    #[test]
    fn ignore_unsupported_seq() {
        assert_eq!(parse("\x1b[4q"), []);
        assert_eq!(parse("\x1b[9W"), []);
        assert_eq!(parse("\x1b[?9999h"), [Decset(dec_modes([]))]);
        assert_eq!(parse("\x1b[:m"), []);
        assert_eq!(parse("\x1b[1?m"), []);
        assert_eq!(parse("\x1b[ 1H"), []);
        assert_eq!(parse("\x1b[ 1m"), []);
        assert_eq!(parse("\x1b[38;2m"), [Sgr(sgr_ops([]))]);
        assert_eq!(parse("\x1b[48;5m"), [Sgr(sgr_ops([]))]);
    }

    #[test]
    fn parse_cursor_style() {
        // DECSCUSR `CSI Ps SP q`, Ps 0..=6.
        assert_eq!(parse("\x1b[1 q"), [SetCursorStyle(1)]);
        assert_eq!(parse("\x1b[4 q"), [SetCursorStyle(4)]);
        assert_eq!(parse("\x1b[6 q"), [SetCursorStyle(6)]);
        // Omitted Ps defaults to 0 (`CSI SP q`).
        assert_eq!(parse("\x1b[ q"), [SetCursorStyle(0)]);
        // Out-of-range Ps still parses; the terminal decides to ignore it.
        assert_eq!(parse("\x1b[7 q"), [SetCursorStyle(7)]);
        // An over-large Ps clamps to 255 rather than wrapping to 0, so it can't
        // masquerade as a valid "block" request once it reaches the terminal.
        assert_eq!(parse("\x1b[256 q"), [SetCursorStyle(255)]);
    }

    #[test]
    fn parse_sgr_seq() {
        assert_eq!(
            parse("\x1b[;1;m"),
            [Sgr(sgr_ops([Reset, SetBoldIntensity, Reset]))]
        );

        assert_eq!(parse("\x1b[1m"), [Sgr(sgr_ops([SetBoldIntensity]))]);
        assert_eq!(parse("\x1b[2m"), [Sgr(sgr_ops([SetFaintIntensity]))]);
        assert_eq!(parse("\x1b[3m"), [Sgr(sgr_ops([SetItalic]))]);
        assert_eq!(parse("\x1b[4m"), [Sgr(sgr_ops([SetUnderline]))]);
        assert_eq!(parse("\x1b[5m"), [Sgr(sgr_ops([SetBlink]))]);
        assert_eq!(parse("\x1b[7m"), [Sgr(sgr_ops([SetInverse]))]);
        assert_eq!(parse("\x1b[9m"), [Sgr(sgr_ops([SetStrikethrough]))]);
        assert_eq!(parse("\x1b[21m"), [Sgr(sgr_ops([ResetIntensity]))]);
        assert_eq!(parse("\x1b[22m"), [Sgr(sgr_ops([ResetIntensity]))]);
        assert_eq!(parse("\x1b[23m"), [Sgr(sgr_ops([ResetItalic]))]);
        assert_eq!(parse("\x1b[24m"), [Sgr(sgr_ops([ResetUnderline]))]);
        assert_eq!(parse("\x1b[25m"), [Sgr(sgr_ops([ResetBlink]))]);
        assert_eq!(parse("\x1b[27m"), [Sgr(sgr_ops([ResetInverse]))]);
        assert_eq!(parse("\x1b[29m"), [Sgr(sgr_ops([ResetStrikethrough]))]);

        assert_eq!(
            parse("\x1b[31m"),
            [Sgr(sgr_ops([SetForegroundColor(Color::Indexed(1))]))]
        );

        assert_eq!(
            parse("\x1b[38:2:1:2:3m"),
            [Sgr(sgr_ops([SetForegroundColor(Color::rgb(1, 2, 3))]))]
        );

        assert_eq!(
            parse("\x1b[38:2::1:2:3m"),
            [Sgr(sgr_ops([SetForegroundColor(Color::rgb(1, 2, 3))]))]
        );

        assert_eq!(
            parse("\x1b[38:5:88m"),
            [Sgr(sgr_ops([SetForegroundColor(Color::Indexed(88))]))]
        );

        assert_eq!(parse("\x1b[39m"), [Sgr(sgr_ops([ResetForegroundColor]))]);

        assert_eq!(
            parse("\x1b[41m"),
            [Sgr(sgr_ops([SetBackgroundColor(Color::Indexed(1))]))]
        );

        assert_eq!(
            parse("\x1b[91m"),
            [Sgr(sgr_ops([SetForegroundColor(Color::Indexed(9))]))]
        );

        assert_eq!(
            parse("\x1b[48:2:1:2:3m"),
            [Sgr(sgr_ops([SetBackgroundColor(Color::rgb(1, 2, 3))]))]
        );

        assert_eq!(
            parse("\x1b[48:2::1:2:3m"),
            [Sgr(sgr_ops([SetBackgroundColor(Color::rgb(1, 2, 3))]))]
        );

        assert_eq!(
            parse("\x1b[48:5:99m"),
            [Sgr(sgr_ops([SetBackgroundColor(Color::Indexed(99))]))]
        );

        assert_eq!(parse("\x1b[49m"), [Sgr(sgr_ops([ResetBackgroundColor]))]);

        assert_eq!(
            parse("\x1b[104m"),
            [Sgr(sgr_ops([SetBackgroundColor(Color::Indexed(12))]))]
        );

        // legacy syntax for 24-bit color, within a larger sequence
        assert_eq!(
            parse("\x1b[1;38;2;1;2;3;48;2;1;2;3;0m"),
            [Sgr(sgr_ops([
                SetBoldIntensity,
                SetForegroundColor(Color::rgb(1, 2, 3)),
                SetBackgroundColor(Color::rgb(1, 2, 3)),
                Reset,
            ]))]
        );

        // legacy syntax for 8-bit color, within a larger sequence
        assert_eq!(
            parse("\x1b[1;38;5;88;48;5;99;0m"),
            [Sgr(sgr_ops([
                SetBoldIntensity,
                SetForegroundColor(Color::Indexed(88)),
                SetBackgroundColor(Color::Indexed(99)),
                Reset,
            ]))]
        );
    }

    #[test]
    fn parse_string_seq() {
        assert_eq!(parse("\x1b]title\x07A"), [Print('A')]);
        assert_eq!(parse("\x1b]title\x1b\\A"), [Print('A')]);
        assert_eq!(parse("\u{9d}title\u{9c}A"), [Print('A')]);
        assert_eq!(parse("\x1bPabc\u{9c}A"), [Print('A')]);
        assert_eq!(parse("\x1bPabc\x1b\\A"), [Print('A')]);
        assert_eq!(parse("\u{90}abc\u{9c}A"), [Print('A')]);
        assert_eq!(parse("\x1bXabc\u{9c}A"), [Print('A')]);
        assert_eq!(parse("\x1bXabc\x1b\\A"), [Print('A')]);
        assert_eq!(parse("\x1b^abc\u{9c}A"), [Print('A')]);
        assert_eq!(parse("\x1b^abc\x1b\\A"), [Print('A')]);
        assert_eq!(parse("\x1b_abc\u{9c}A"), [Print('A')]);
        assert_eq!(parse("\x1b_abc\x1b\\A"), [Print('A')]);
        assert_eq!(parse("\u{98}abc\u{9c}A"), [Print('A')]);
        assert_eq!(parse("\u{9e}abc\u{9c}A"), [Print('A')]);
        assert_eq!(parse("\u{9f}abc\u{9c}A"), [Print('A')]);
    }

    #[test]
    fn parse_unicode() {
        assert_eq!(parse("日"), [Print('日')]);
        assert_eq!(parse("\x1b[日A"), [Print('A')]);
    }

    #[test]
    fn dump_non_ground_states() {
        assert_dump("\x1b", State::Escape, "\x1b");
        assert_dump("\x1b(", State::EscapeIntermediate, "\x1b(");
        assert_dump("\x1b[", State::CsiEntry, "\u{9b}");
        assert_dump("\x1b[ ", State::CsiIntermediate, "\u{9b} ");
        assert_dump("\x1b[:", State::CsiIgnore, "\u{9b}\u{3a}");
        assert_dump("\x1bP", State::DcsEntry, "\u{90}");
        assert_dump("\x1bP ", State::DcsIntermediate, "\u{90} ");
        assert_dump("\x1bP1;2", State::DcsParam, "\u{90}1;2");
        assert_dump("\x1bPz", State::DcsPassthrough, "\u{90}\u{40}");
        assert_dump("\x1bP:", State::DcsIgnore, "\u{90}\u{3a}");
        assert_dump("\x1b]", State::OscString, "\u{9d}");
        // OSC and APC re-emit their accumulated body so a mid-sequence checkpoint
        // round-trips the eventual dispatch.
        assert_dump("\x1b]0;hi", State::OscString, "\u{9d}0;hi");
        assert_dump("\x1bX", State::SosPmApcString, "\u{98}");
        assert_dump("\x1b_", State::ApcString, "\u{9f}");
        assert_dump("\x1b_Gf=24,a=q", State::ApcString, "\u{9f}Gf=24,a=q");
    }

    #[test]
    fn apc_dump_resume_round_trips_the_graphics_command() {
        // A checkpoint landing mid-APC must not lose the in-flight command: the
        // resumed parser, fed the dump then the rest of the sequence, emits the
        // same KittyGraphics as a parser that saw the whole stream uninterrupted.
        let mut live = Parser::new();
        feed(&mut live, "\x1b_Gi=1,a=q;AAAA"); // mid-APC, before the ST

        let mut resumed = Parser::new();
        feed(&mut resumed, &live.dump());
        live.assert_eq(&resumed);

        let suffix: Vec<char> = "\x1b\\".chars().collect();
        assert_eq!(
            emit(&mut live, &suffix),
            [KittyGraphics("i=1,a=q;AAAA".to_string())]
        );
        assert_eq!(
            emit(&mut resumed, &suffix),
            [KittyGraphics("i=1,a=q;AAAA".to_string())]
        );
    }

    #[test]
    fn dump() {
        let mut parser = Parser::new();

        for ch in "\x1b[;1;;38:2:1:2:3;".chars() {
            parser.feed(ch);
        }

        // An omitted parameter dumps omitted, so resuming lands in the state the
        // dump left — the sequence comes back as it went in.
        assert_eq!(parser.dump(), "\u{9b};1;;38:2:1:2:3;");
    }

    #[test]
    fn dump_functions_roundtrip() {
        let functions = vec![
            Bs,
            Cbt(2),
            Cha(7),
            Cht(8),
            Cnl(5),
            Cpl(6),
            Cr,
            Ctc(CtcOp::Set),
            Ctc(CtcOp::ClearCurrentColumn),
            Ctc(CtcOp::ClearAll),
            Cub(4),
            Cud(2),
            Cuf(3),
            Cup(3, 4),
            Cuu(1),
            Dch(18),
            Decaln,
            Decbi,
            Decfi,
            Decrc,
            Decrst(dec_modes([])),
            Decrst(dec_modes([
                DecMode::CursorKeys,
                DecMode::Origin,
                DecMode::AutoWrap,
                DecMode::TextCursorEnable,
                DecMode::AltScreenBuffer,
                DecMode::SaveCursor,
                DecMode::SaveCursorAltScreenBuffer,
            ])),
            Decsc,
            Decset(dec_modes([])),
            Decset(dec_modes([
                DecMode::CursorKeys,
                DecMode::Origin,
                DecMode::AutoWrap,
                DecMode::TextCursorEnable,
                DecMode::AltScreenBuffer,
                DecMode::SaveCursor,
                DecMode::SaveCursorAltScreenBuffer,
            ])),
            Decslrm(5, 10),
            Decic(3),
            Decdc(7),
            Decscl(64, 1),
            Decsca(0),
            Decsca(1),
            Decsera(2, 3, 5, 7),
            Decera(2, 3, 5, 7),
            Decfra(37, 2, 3, 5, 7),
            Deccra([2, 3, 5, 7, 1, 9, 9, 1]),
            Decstbm(2, 5),
            Decstr,
            Dl(17),
            Ech(21),
            Ed(EdScope::Below),
            Ed(EdScope::Above),
            Ed(EdScope::All),
            Ed(EdScope::SavedLines),
            Decsed(EdScope::Below),
            Decsed(EdScope::Above),
            Decsed(EdScope::All),
            Decsed(EdScope::SavedLines),
            El(ElScope::ToRight),
            El(ElScope::ToLeft),
            El(ElScope::All),
            Decsel(ElScope::ToRight),
            Decsel(ElScope::ToLeft),
            Decsel(ElScope::All),
            Spa,
            Epa,
            G1d4(Charset::Drawing),
            G1d4(Charset::Ascii),
            Gzd4(Charset::Drawing),
            Gzd4(Charset::Ascii),
            Ht,
            Hts,
            Ich(16),
            Il(16),
            KittyKeyboardPush(0),
            KittyKeyboardPush(15),
            KittyKeyboardPop(1),
            KittyKeyboardPop(3),
            KittyKeyboardSet(5, 1),
            KittyKeyboardSet(9, 3),
            Lf,
            ModifyOtherKeys(0),
            ModifyOtherKeys(1),
            ModifyOtherKeys(2),
            Nel,
            Print('A'),
            Print('日'),
            Rep(11),
            Ri,
            Ris,
            Rm(ansi_modes([])),
            Rm(ansi_modes([AnsiMode::Insert, AnsiMode::NewLine])),
            Scorc,
            Sd(20),
            SetPalette(vec![(1, [0xff, 0x00, 0x00]), (200, [0x10, 0x20, 0x30])]),
            ResetPalette(vec![]),
            ResetPalette(vec![3, 9]),
            SetCursorStyle(0),
            SetCursorStyle(4),
            SetCursorStyle(6),
            Sgr(sgr_ops([])),
            Sgr(sgr_ops([
                Reset,
                SetBoldIntensity,
                SetFaintIntensity,
                SetItalic,
                SetUnderline,
                SetBlink,
                SetInverse,
                SetStrikethrough,
                ResetIntensity,
                ResetItalic,
                ResetUnderline,
                ResetBlink,
                ResetInverse,
                ResetStrikethrough,
                SetForegroundColor(Color::Indexed(1)),
                ResetForegroundColor,
                SetBackgroundColor(Color::rgb(1, 2, 3)),
                ResetBackgroundColor,
            ])),
            Si,
            Sm(ansi_modes([])),
            Sm(ansi_modes([AnsiMode::Insert, AnsiMode::NewLine])),
            So,
            Su(19),
            Tbc(TbcScope::CurrentColumn),
            Tbc(TbcScope::All),
            Vpa(12),
            Vpr(13),
            Xtwinops(XtwinopsOp::Resize(Some(80), Some(24))),
        ];

        assert_eq!(parse(&super::dump(&functions)), functions);
    }

    proptest! {
        #[test]
        fn prop_dump_resume_equivalence(
            input in gen_parser_input(64),
            split in 0usize..65,
        ) {
            let split = split.min(input.len());
            let (prefix, suffix) = input.split_at(split);

            let mut parser1 = Parser::new();
            let _ = emit(&mut parser1, prefix);

            let mut parser2 = Parser::new();
            let dumped = parser1.dump();
            let dump_output = dumped
                .chars()
                .filter_map(|ch| parser2.feed(ch))
                .collect::<Vec<_>>();

            prop_assert!(dump_output.is_empty());
            parser1.assert_eq(&parser2);

            let suffix_output1 = emit(&mut parser1, suffix);
            let suffix_output2 = emit(&mut parser2, suffix);

            prop_assert_eq!(suffix_output1, suffix_output2);
            parser1.assert_eq(&parser2);
        }

        #[test]
        fn prop_cancel_then_continue(
            prefix in gen_non_ground_prefix(),
            cancel in prop::sample::select(vec!['\x18', '\x1a']),
            suffix in gen_printable_text(16),
        ) {
            let mut parser = Parser::new();

            let prefix_output = emit(&mut parser, &prefix);

            prop_assert!(prefix_output.is_empty());
            prop_assert_ne!(parser.state, State::Ground);

            let cancel_output = parser.feed(cancel);

            prop_assert_eq!(cancel_output, None);
            prop_assert_eq!(parser.state, State::Ground);

            let suffix_output = emit(&mut parser, &suffix);
            let expected = suffix.iter().copied().map(Function::Print).collect::<Vec<_>>();

            prop_assert_eq!(suffix_output, expected);
            prop_assert_eq!(parser.state, State::Ground);
        }
    }
}
