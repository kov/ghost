//! Predictive local echo: mosh's `PredictionEngine` (`src/frontend/terminaloverlay.cc`
//! in mobile-shell/mosh), ported onto ghost's emulator. See
//! `docs/predictive-echo-design.md`.
//!
//! The engine watches the bytes the user sends, guesses what they will do to the
//! screen (a character appears at the cursor, the cursor moves), and checks every
//! guess against the session's real screen once the host acknowledges the input
//! ([`ghost_vt::protocol::ServerMsg::EchoAck`]). It never touches the screen: what
//! it would draw is an [`Overlay`] the view lays over its frame.
//!
//! Pure: the clock comes in as an argument, and the screen is read from the
//! session's [`Vt`].

use std::time::{Duration, Instant};

use ghost_term::{Cell, Line, Pen, Vt};

/// `send_interval` at or below which a link counts as fast again (mosh:
/// `SRTT_TRIGGER_LOW`), in ms. Predictions stop showing only once none are
/// outstanding.
const SRTT_TRIGGER_LOW: u64 = 20;
/// `send_interval` above which predictions are shown (`SRTT_TRIGGER_HIGH`).
const SRTT_TRIGGER_HIGH: u64 = 30;
/// `send_interval` at or below which predictions stop being underlined.
const FLAG_TRIGGER_LOW: u64 = 50;
/// `send_interval` above which predictions are underlined.
const FLAG_TRIGGER_HIGH: u64 = 80;
/// A prediction outstanding this long shows predictions even on a fast link.
const GLITCH_THRESHOLD: Duration = Duration::from_millis(250);
/// Quick confirmations needed to cure the glitch trigger.
const GLITCH_REPAIR_COUNT: u32 = 10;
/// Least time between two confirmations that count toward the cure.
const GLITCH_REPAIR_MININTERVAL: Duration = Duration::from_millis(150);
/// A prediction outstanding this long also underlines.
const GLITCH_FLAG_THRESHOLD: Duration = Duration::from_millis(5000);
/// How often the engine wants re-checking while its timers can still fire.
const TICK: Duration = Duration::from_millis(50);

/// mosh's frame interval bounds: `send_interval` = SRTT/2, clamped.
const SEND_INTERVAL_MIN: u64 = 20;
const SEND_INTERVAL_MAX: u64 = 250;

/// A frame number no acknowledgement reaches: what a reset prediction expires at.
const NEVER: u64 = u64::MAX;

/// When predictions are drawn (mosh's `--predict`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DisplayPreference {
    /// Only on a slow link, or after a prediction took suspiciously long.
    #[default]
    Adaptive,
    /// Whenever there is something to show.
    Always,
    /// Never: the engine stays idle.
    Never,
}

/// What the view should draw over the session's real screen: cells to replace
/// (viewport row, column, cell) and where to put the cursor instead.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Overlay {
    pub cursor: Option<(usize, usize)>,
    pub cells: Vec<(usize, usize, Cell)>,
}

impl Overlay {
    pub fn is_empty(&self) -> bool {
        self.cursor.is_none() && self.cells.is_empty()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Validity {
    Pending,
    Correct,
    CorrectNoCredit,
    IncorrectOrExpired,
    Inactive,
}

/// The part every prediction shares (mosh's `ConditionalOverlay`).
#[derive(Clone, Debug)]
struct Conditional {
    /// The input number whose acknowledgement makes this checkable.
    expiration: u64,
    col: usize,
    active: bool,
    /// The epoch this belongs to: hidden until that epoch is confirmed.
    tentative_until: u64,
    prediction_time: Option<Instant>,
}

impl Conditional {
    fn new(expiration: u64, col: usize, tentative_until: u64) -> Self {
        Conditional {
            expiration,
            col,
            active: false,
            tentative_until,
            prediction_time: None,
        }
    }

    fn tentative(&self, confirmed_epoch: u64) -> bool {
        self.tentative_until > confirmed_epoch
    }

    fn reset(&mut self) {
        self.expiration = NEVER;
        self.tentative_until = NEVER;
        self.active = false;
    }

    fn expire(&mut self, expiration: u64, now: Instant) {
        self.expiration = expiration;
        self.prediction_time = Some(now);
    }
}

#[derive(Clone, Debug)]
struct CursorMove {
    c: Conditional,
    row: usize,
}

impl CursorMove {
    fn new(expiration: u64, row: usize, col: usize, tentative_until: u64) -> Self {
        let mut c = Conditional::new(expiration, col, tentative_until);
        c.active = true;
        CursorMove { c, row }
    }

    fn validity(&self, screen: &ScreenView, late_ack: u64) -> Validity {
        if !self.c.active {
            return Validity::Inactive;
        }
        if self.row >= screen.rows || self.c.col >= screen.cols {
            return Validity::IncorrectOrExpired;
        }
        if late_ack >= self.c.expiration {
            return if screen.cursor == (self.row, self.c.col) {
                Validity::Correct
            } else {
                Validity::IncorrectOrExpired
            };
        }
        Validity::Pending
    }
}

#[derive(Clone, Debug)]
struct OverlayCell {
    c: Conditional,
    replacement: Cell,
    /// The prediction knows a change happens here but not what it is (a cell
    /// shifted in from past the right edge): drawn only as an underline.
    unknown: bool,
    /// What was there before: a "correct" match against one of these proves
    /// nothing, so it earns no credit.
    original: Vec<Cell>,
}

impl OverlayCell {
    fn new(col: usize, tentative_until: u64) -> Self {
        OverlayCell {
            c: Conditional::new(0, col, tentative_until),
            replacement: blank(Pen::default()),
            unknown: false,
            original: Vec::new(),
        }
    }

    fn reset(&mut self) {
        self.unknown = false;
        self.original.clear();
        self.c.reset();
    }

    fn reset_with_orig(&mut self) {
        if !self.c.active || self.unknown {
            self.reset();
            return;
        }
        self.original.push(self.replacement);
        self.c.reset();
    }

    fn validity(&self, screen: &ScreenView, row: usize, late_ack: u64) -> Validity {
        if !self.c.active {
            return Validity::Inactive;
        }
        if row >= screen.rows || self.c.col >= screen.cols {
            return Validity::IncorrectOrExpired;
        }
        if late_ack < self.c.expiration {
            return Validity::Pending;
        }
        if self.unknown || is_blank(&self.replacement) {
            // A blank is too easy to match by accident.
            return Validity::CorrectNoCredit;
        }
        let current = screen.cell(row, self.c.col);
        if current.char() != self.replacement.char() {
            return Validity::IncorrectOrExpired;
        }
        if self
            .original
            .iter()
            .any(|o| o.char() == self.replacement.char())
        {
            Validity::CorrectNoCredit
        } else {
            Validity::Correct
        }
    }

    /// What this cell draws over `screen`, if anything.
    fn apply(
        &self,
        screen: &ScreenView,
        confirmed_epoch: u64,
        row: usize,
        flag: bool,
    ) -> Option<Cell> {
        let col = self.c.col;
        if !self.c.active || row >= screen.rows || col >= screen.cols {
            return None;
        }
        if self.c.tentative(confirmed_epoch) {
            return None;
        }
        let actual = screen.cell(row, col);
        let flag = flag && !(is_blank(&self.replacement) && is_blank(&actual));
        if self.unknown {
            return (flag && col != screen.cols - 1).then(|| underlined(actual));
        }
        (actual != self.replacement).then(|| {
            if flag {
                underlined(self.replacement)
            } else {
                self.replacement
            }
        })
    }
}

#[derive(Clone, Debug)]
struct OverlayRow {
    row: usize,
    cells: Vec<OverlayCell>,
}

/// The live viewport of a [`Vt`], read once per call.
struct ScreenView<'a> {
    lines: Vec<&'a Line>,
    cols: usize,
    rows: usize,
    cursor: (usize, usize),
}

impl<'a> ScreenView<'a> {
    fn new(vt: &'a Vt) -> Self {
        let (cols, rows) = vt.size();
        let cursor = vt.cursor();
        ScreenView {
            lines: vt.view().collect(),
            cols,
            rows,
            // A cursor parked past the last column (a pending wrap) sits on it.
            cursor: (cursor.row, cursor.col.min(cols.saturating_sub(1))),
        }
    }

    fn cell(&self, row: usize, col: usize) -> Cell {
        self.lines
            .get(row)
            .and_then(|l| l.cells().get(col))
            .copied()
            .unwrap_or_else(|| blank(Pen::default()))
    }
}

fn blank(pen: Pen) -> Cell {
    Cell::single(' ', pen).expect("a space is one column")
}

fn is_blank(cell: &Cell) -> bool {
    cell.char() == ' '
}

fn with_pen(cell: Cell, pen: Pen) -> Cell {
    Cell::single(cell.char(), pen).unwrap_or(cell)
}

fn underlined(cell: Cell) -> Cell {
    let mut pen = *cell.pen();
    pen.set_underline();
    with_pen(cell, pen)
}

/// What a byte of user input amounts to, as far as prediction cares (the
/// actions of mosh's parser it acts on).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Act {
    Print(char),
    Execute(u8),
    /// A control sequence's final byte; `plain` when it carried no parameters
    /// or intermediates.
    Csi {
        last: u8,
        plain: bool,
    },
    /// Anything else complete: an escape, a string, malformed UTF-8.
    Other,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ParseState {
    #[default]
    Ground,
    Escape,
    Csi,
    Str,
    StrEscape,
}

/// A minimal VT input parser: enough to tell printable text from control
/// sequences in what a terminal sends.
#[derive(Clone, Debug, Default)]
struct InputParser {
    state: ParseState,
    utf8: Vec<u8>,
    utf8_need: usize,
    csi_extra: bool,
}

impl InputParser {
    fn byte(&mut self, b: u8) -> Option<Act> {
        match self.state {
            ParseState::Ground => self.ground(b),
            ParseState::Escape => match b {
                // mosh reads SS3 (`ESC O`, application cursor keys) as CSI.
                b'[' | b'O' => {
                    self.state = ParseState::Csi;
                    self.csi_extra = false;
                    None
                }
                b']' | b'P' | b'_' | b'^' | b'X' => {
                    self.state = ParseState::Str;
                    None
                }
                0x1b => None,
                0x20..=0x2f => None,
                _ => {
                    self.state = ParseState::Ground;
                    Some(Act::Other)
                }
            },
            ParseState::Csi => match b {
                0x20..=0x3f => {
                    self.csi_extra = true;
                    None
                }
                0x40..=0x7e => {
                    self.state = ParseState::Ground;
                    Some(Act::Csi {
                        last: b,
                        plain: !self.csi_extra,
                    })
                }
                0x1b => {
                    self.state = ParseState::Escape;
                    None
                }
                _ => None,
            },
            ParseState::Str => {
                match b {
                    0x07 => {
                        self.state = ParseState::Ground;
                        return Some(Act::Other);
                    }
                    0x1b => self.state = ParseState::StrEscape,
                    _ => {}
                }
                None
            }
            ParseState::StrEscape => {
                self.state = ParseState::Ground;
                Some(Act::Other)
            }
        }
    }

    fn ground(&mut self, b: u8) -> Option<Act> {
        if self.utf8_need > 0 {
            if b & 0xc0 != 0x80 {
                self.utf8.clear();
                self.utf8_need = 0;
                return Some(Act::Other);
            }
            self.utf8.push(b);
            self.utf8_need -= 1;
            if self.utf8_need > 0 {
                return None;
            }
            let ch = std::str::from_utf8(&self.utf8)
                .ok()
                .and_then(|s| s.chars().next());
            self.utf8.clear();
            return Some(ch.map_or(Act::Other, Act::Print));
        }
        match b {
            0x1b => {
                self.state = ParseState::Escape;
                None
            }
            0x7f => Some(Act::Print('\x7f')),
            0x00..=0x1f => Some(Act::Execute(b)),
            0x20..=0x7e => Some(Act::Print(b as char)),
            0xc2..=0xf4 => {
                self.utf8.push(b);
                self.utf8_need = match b {
                    0xc2..=0xdf => 1,
                    0xe0..=0xef => 2,
                    _ => 3,
                };
                None
            }
            _ => Some(Act::Other),
        }
    }
}

/// One session's predictions (see the module docs). Feed it what the user
/// sends ([`input`](Self::input), then [`sent`](Self::sent) with the number the
/// input went out under), what the host acknowledges
/// ([`echo_ack`](Self::echo_ack)), and every change of the screen
/// ([`screen_changed`](Self::screen_changed)); draw [`shown`](Self::shown).
#[derive(Clone, Debug)]
pub struct PredictionEngine {
    preference: DisplayPreference,
    parser: InputParser,
    overlays: Vec<OverlayRow>,
    cursors: Vec<CursorMove>,
    local_frame_sent: u64,
    local_frame_late_acked: u64,
    prediction_epoch: u64,
    confirmed_epoch: u64,
    flagging: bool,
    srtt_trigger: bool,
    glitch_trigger: u32,
    last_quick_confirmation: Option<Instant>,
    send_interval: u64,
    last_size: (usize, usize),
    shown: Overlay,
    generation: u64,
}

impl Default for PredictionEngine {
    fn default() -> Self {
        PredictionEngine::new(DisplayPreference::default())
    }
}

impl PredictionEngine {
    pub fn new(preference: DisplayPreference) -> Self {
        PredictionEngine {
            preference,
            parser: InputParser::default(),
            overlays: Vec::new(),
            cursors: Vec::new(),
            local_frame_sent: 0,
            local_frame_late_acked: 0,
            prediction_epoch: 1,
            confirmed_epoch: 0,
            flagging: false,
            srtt_trigger: false,
            glitch_trigger: 0,
            last_quick_confirmation: None,
            send_interval: SEND_INTERVAL_MAX,
            last_size: (0, 0),
            shown: Overlay::default(),
            generation: 0,
        }
    }

    pub fn set_preference(&mut self, preference: DisplayPreference) {
        if preference != self.preference {
            self.preference = preference;
            self.reset();
        }
    }

    pub fn preference(&self) -> DisplayPreference {
        self.preference
    }

    /// Carry a replaced engine's generation count on, so a fresh engine never
    /// repeats a generation the old one already reported.
    pub fn continue_generation(&mut self, from: u64) {
        self.generation = from.wrapping_add(1);
    }

    /// What to draw over the screen right now.
    pub fn shown(&self) -> &Overlay {
        &self.shown
    }

    /// Bumped whenever [`shown`](Self::shown) changes, for a view's frame memo.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The user is sending `bytes`; `vt` is the screen as of now. Call before
    /// the bytes go out, then [`sent`](Self::sent).
    pub fn input(&mut self, bytes: &[u8], vt: &Vt, now: Instant) {
        if self.preference == DisplayPreference::Never {
            return;
        }
        let screen = ScreenView::new(vt);
        for &b in bytes {
            self.new_user_byte(b, &screen, now);
        }
        self.refresh(&screen);
    }

    /// The last input went out numbered `seq`.
    pub fn sent(&mut self, seq: u64) {
        self.local_frame_sent = seq;
    }

    /// The latest round-trip estimate.
    pub fn set_srtt(&mut self, srtt: Duration) {
        let half = (srtt.as_secs_f64() * 1000.0 / 2.0).ceil() as u64;
        self.send_interval = half.clamp(SEND_INTERVAL_MIN, SEND_INTERVAL_MAX);
    }

    /// The host acknowledged input up to `ack`. Call after feeding every byte
    /// of output that arrived with it.
    pub fn echo_ack(&mut self, ack: u64, vt: &Vt, now: Instant) {
        self.local_frame_late_acked = self.local_frame_late_acked.max(ack);
        self.screen_changed(vt, now);
    }

    /// The screen changed (output was fed), or time passed: re-judge.
    pub fn screen_changed(&mut self, vt: &Vt, now: Instant) {
        if self.preference == DisplayPreference::Never {
            return;
        }
        let screen = ScreenView::new(vt);
        self.cull(&screen, now);
        self.refresh(&screen);
    }

    /// Forget every prediction: the screen they were made against is gone (a
    /// resync, a reconnect).
    pub fn reset(&mut self) {
        self.cursors.clear();
        self.overlays.clear();
        self.become_tentative();
        if !self.shown.is_empty() {
            self.shown = Overlay::default();
            self.generation = self.generation.wrapping_add(1);
        }
    }

    /// How soon [`screen_changed`](Self::screen_changed) should run again even
    /// without output, while a timing trigger can still fire (mosh's
    /// `wait_time`).
    pub fn wants_tick(&self) -> Option<Duration> {
        let timing_tests_necessary = !(self.glitch_trigger > 0 && self.flagging);
        (timing_tests_necessary && self.active()).then_some(TICK)
    }

    fn refresh(&mut self, screen: &ScreenView) {
        let shown = self.apply(screen);
        if shown != self.shown {
            self.shown = shown;
            self.generation = self.generation.wrapping_add(1);
        }
    }

    fn apply(&self, screen: &ScreenView) -> Overlay {
        let showing = match self.preference {
            DisplayPreference::Never => false,
            DisplayPreference::Always => true,
            DisplayPreference::Adaptive => self.srtt_trigger || self.glitch_trigger > 0,
        };
        let mut out = Overlay::default();
        if !showing {
            return out;
        }
        for c in &self.cursors {
            if c.c.active && !c.c.tentative(self.confirmed_epoch) {
                out.cursor = Some((c.row, c.c.col));
            }
        }
        for row in &self.overlays {
            for cell in &row.cells {
                if let Some(drawn) =
                    cell.apply(screen, self.confirmed_epoch, row.row, self.flagging)
                {
                    out.cells.push((row.row, cell.c.col, drawn));
                }
            }
        }
        out
    }

    fn active(&self) -> bool {
        !self.cursors.is_empty()
            || self
                .overlays
                .iter()
                .any(|r| r.cells.iter().any(|c| c.c.active))
    }

    fn become_tentative(&mut self) {
        self.prediction_epoch += 1;
    }

    fn cursor(&mut self) -> &mut CursorMove {
        self.cursors.last_mut().expect("init_cursor ran")
    }

    fn init_cursor(&mut self, screen: &ScreenView) {
        let exp = self.local_frame_sent + 1;
        match self.cursors.last() {
            None => {
                let (row, col) = screen.cursor;
                self.cursors
                    .push(CursorMove::new(exp, row, col, self.prediction_epoch));
            }
            Some(last) if last.c.tentative_until != self.prediction_epoch => {
                let (row, col) = (last.row, last.c.col);
                self.cursors
                    .push(CursorMove::new(exp, row, col, self.prediction_epoch));
            }
            Some(_) => {}
        }
    }

    fn row_index(&mut self, row: usize, cols: usize) -> usize {
        if let Some(i) = self.overlays.iter().position(|r| r.row == row) {
            return i;
        }
        let epoch = self.prediction_epoch;
        self.overlays.push(OverlayRow {
            row,
            cells: (0..cols).map(|c| OverlayCell::new(c, epoch)).collect(),
        });
        self.overlays.len() - 1
    }

    fn kill_epoch(&mut self, epoch: u64, screen: &ScreenView) {
        self.cursors.retain(|c| !c.c.tentative(epoch - 1));
        let (row, col) = screen.cursor;
        self.cursors.push(CursorMove::new(
            self.local_frame_sent + 1,
            row,
            col,
            self.prediction_epoch,
        ));
        for row in &mut self.overlays {
            for cell in &mut row.cells {
                if cell.c.tentative(epoch - 1) {
                    cell.reset();
                }
            }
        }
        self.become_tentative();
    }

    fn cull(&mut self, screen: &ScreenView, now: Instant) {
        if self.preference == DisplayPreference::Never {
            return;
        }
        let size = (screen.cols, screen.rows);
        if size != self.last_size {
            self.last_size = size;
            self.reset();
        }

        // Show predictions on a slow link, with hysteresis.
        if self.send_interval > SRTT_TRIGGER_HIGH {
            self.srtt_trigger = true;
        } else if self.srtt_trigger && self.send_interval <= SRTT_TRIGGER_LOW && !self.active() {
            self.srtt_trigger = false;
        }
        // Underline them on a slower one.
        if self.send_interval > FLAG_TRIGGER_HIGH {
            self.flagging = true;
        } else if self.send_interval <= FLAG_TRIGGER_LOW {
            self.flagging = false;
        }
        if self.glitch_trigger > GLITCH_REPAIR_COUNT {
            self.flagging = true;
        }

        self.overlays.retain(|r| r.row < screen.rows);
        let late_ack = self.local_frame_late_acked;
        for ri in 0..self.overlays.len() {
            let row = self.overlays[ri].row;
            for ci in 0..self.overlays[ri].cells.len() {
                let cell = &self.overlays[ri].cells[ci];
                match cell.validity(screen, row, late_ack) {
                    Validity::IncorrectOrExpired => {
                        if cell.c.tentative(self.confirmed_epoch) {
                            let epoch = cell.c.tentative_until;
                            self.kill_epoch(epoch, screen);
                        } else {
                            self.reset();
                            return;
                        }
                    }
                    v @ (Validity::Correct | Validity::CorrectNoCredit) => {
                        if v == Validity::Correct {
                            let tentative_until = cell.c.tentative_until;
                            let prediction_time = cell.c.prediction_time;
                            let col = cell.c.col;
                            if tentative_until > self.confirmed_epoch {
                                self.confirmed_epoch = tentative_until;
                            }
                            // Quick confirmations slowly cure the glitch trigger.
                            let quick = prediction_time
                                .is_some_and(|t| now.duration_since(t) < GLITCH_THRESHOLD);
                            let spaced = self
                                .last_quick_confirmation
                                .is_none_or(|t| now.duration_since(t) >= GLITCH_REPAIR_MININTERVAL);
                            if quick && self.glitch_trigger > 0 && spaced {
                                self.glitch_trigger -= 1;
                                self.last_quick_confirmation = Some(now);
                            }
                            // The rest of the row takes on the real renditions.
                            let pen = *screen.cell(row, col).pen();
                            for k in &mut self.overlays[ri].cells[ci..] {
                                k.replacement = with_pen(k.replacement, pen);
                            }
                        }
                        self.overlays[ri].cells[ci].reset();
                    }
                    Validity::Pending => {
                        // A prediction that takes long to confirm shows
                        // predictions even on a fast link.
                        let age = cell.c.prediction_time.map(|t| now.duration_since(t));
                        if age.is_some_and(|a| a >= GLITCH_FLAG_THRESHOLD) {
                            self.glitch_trigger = GLITCH_REPAIR_COUNT * 2;
                        } else if age.is_some_and(|a| a >= GLITCH_THRESHOLD)
                            && self.glitch_trigger < GLITCH_REPAIR_COUNT
                        {
                            self.glitch_trigger = GLITCH_REPAIR_COUNT;
                        }
                    }
                    Validity::Inactive => {}
                }
            }
        }

        if let Some(c) = self.cursors.last()
            && c.validity(screen, late_ack) == Validity::IncorrectOrExpired
        {
            self.reset();
            return;
        }
        self.cursors
            .retain(|c| c.validity(screen, late_ack) == Validity::Pending);
    }

    fn new_user_byte(&mut self, b: u8, screen: &ScreenView, now: Instant) {
        self.cull(screen, now);
        let Some(act) = self.parser.byte(b) else {
            return;
        };
        let exp = self.local_frame_sent + 1;
        let (cols, rows) = (screen.cols, screen.rows);
        if cols == 0 || rows == 0 {
            return;
        }
        match act {
            Act::Print('\x7f') => {
                self.init_cursor(screen);
                let (crow, ccol) = (self.cursor().row, self.cursor().c.col);
                let ri = self.row_index(crow, cols);
                if ccol > 0 {
                    let ccol = ccol - 1;
                    let cur = self.cursor();
                    cur.c.col = ccol;
                    cur.c.expire(exp, now);
                    let epoch = self.prediction_epoch;
                    for i in ccol..cols {
                        let next = (i + 2 < cols).then(|| {
                            let n = &self.overlays[ri].cells[i + 1];
                            if n.c.active {
                                (!n.unknown).then_some(n.replacement)
                            } else {
                                Some(screen.cell(crow, i + 1))
                            }
                        });
                        let cell = &mut self.overlays[ri].cells[i];
                        cell.reset_with_orig();
                        cell.c.active = true;
                        cell.c.tentative_until = epoch;
                        cell.c.expire(exp, now);
                        cell.original.push(screen.cell(crow, i));
                        match next.flatten() {
                            Some(r) => {
                                cell.unknown = false;
                                cell.replacement = r;
                            }
                            None => cell.unknown = true,
                        }
                    }
                }
            }
            Act::Print(ch) => {
                self.init_cursor(screen);
                let (crow, ccol) = (self.cursor().row, self.cursor().c.col);
                // Wide, zero-width, or otherwise unknowable: don't guess.
                let Some(typed) = Cell::single(ch, Pen::default()) else {
                    self.become_tentative();
                    return;
                };
                if crow >= rows || ccol >= cols {
                    return;
                }
                let ri = self.row_index(crow, cols);
                if ccol + 1 >= cols {
                    // The last column is tricky: an editor may show a wrap mark,
                    // a shell just puts the character there.
                    self.become_tentative();
                }
                let epoch = self.prediction_epoch;
                // Insert: everything right of the cursor shifts one column.
                for i in (ccol + 1..cols).rev() {
                    let prev = {
                        let p = &self.overlays[ri].cells[i - 1];
                        if i == cols - 1 {
                            None
                        } else if p.c.active {
                            (!p.unknown).then_some(p.replacement)
                        } else {
                            Some(screen.cell(crow, i - 1))
                        }
                    };
                    let cell = &mut self.overlays[ri].cells[i];
                    cell.reset_with_orig();
                    cell.c.active = true;
                    cell.c.tentative_until = epoch;
                    cell.c.expire(exp, now);
                    cell.original.push(screen.cell(crow, i));
                    match prev {
                        Some(r) => {
                            cell.unknown = false;
                            cell.replacement = r;
                        }
                        None => cell.unknown = true,
                    }
                }
                // The typed character takes the renditions of its left neighbour.
                let pen = if ccol > 0 {
                    let p = &self.overlays[ri].cells[ccol - 1];
                    if p.c.active && !p.unknown {
                        *p.replacement.pen()
                    } else {
                        *screen.cell(crow, ccol - 1).pen()
                    }
                } else {
                    Pen::default()
                };
                let cell = &mut self.overlays[ri].cells[ccol];
                cell.reset_with_orig();
                cell.c.active = true;
                cell.c.tentative_until = epoch;
                cell.c.expire(exp, now);
                cell.replacement = with_pen(typed, pen);
                cell.original.push(screen.cell(crow, ccol));

                self.cursor().c.expire(exp, now);
                if ccol < cols - 1 {
                    self.cursor().c.col += 1;
                } else {
                    self.become_tentative();
                    self.newline_carriage_return(screen, now);
                }
            }
            Act::Execute(b'\r') => {
                self.become_tentative();
                self.newline_carriage_return(screen, now);
            }
            Act::Csi {
                last: b'C',
                plain: true,
            } => {
                self.init_cursor(screen);
                let cur = self.cursor();
                if cur.c.col < cols - 1 {
                    cur.c.col += 1;
                    cur.c.expire(exp, now);
                }
            }
            Act::Csi {
                last: b'D',
                plain: true,
            } => {
                self.init_cursor(screen);
                let cur = self.cursor();
                if cur.c.col > 0 {
                    cur.c.col -= 1;
                    cur.c.expire(exp, now);
                }
            }
            Act::Execute(_) | Act::Csi { .. } | Act::Other => self.become_tentative(),
        }
    }

    fn newline_carriage_return(&mut self, screen: &ScreenView, now: Instant) {
        let exp = self.local_frame_sent + 1;
        self.init_cursor(screen);
        self.cursor().c.col = 0;
        let row = self.cursor().row;
        if row == screen.rows - 1 {
            // No scroll prediction: the last row is predicted blank instead.
            let ri = self.row_index(row, screen.cols);
            let epoch = self.prediction_epoch;
            for cell in &mut self.overlays[ri].cells {
                cell.c.active = true;
                cell.c.tentative_until = epoch;
                cell.c.expire(exp, now);
                cell.replacement = blank(*cell.replacement.pen());
            }
        } else {
            self.cursor().row += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// A shell-like screen: a prompt, the cursor after it.
    fn prompt() -> Vt {
        let mut vt = Vt::new(20, 4);
        vt.feed_str("$ ");
        vt
    }

    /// An engine on a slow link (500ms round trip): predictions show.
    fn slow() -> PredictionEngine {
        let mut e = PredictionEngine::new(DisplayPreference::Adaptive);
        e.set_srtt(ms(500));
        e
    }

    /// Type `bytes` the way the shell does: predict, then send under the next number.
    fn type_bytes(e: &mut PredictionEngine, seq: &mut u64, bytes: &str, vt: &Vt, now: Instant) {
        e.input(bytes.as_bytes(), vt, now);
        *seq += 1;
        e.sent(*seq);
    }

    fn drawn(e: &PredictionEngine) -> Vec<(usize, usize, char)> {
        e.shown()
            .cells
            .iter()
            .map(|(r, c, cell)| (*r, *c, cell.char()))
            .collect()
    }

    /// Type `q`, echo and acknowledge it: the epoch is confirmed.
    fn confirmed(vt: &mut Vt, seq: &mut u64, now: Instant) -> PredictionEngine {
        let mut e = slow();
        type_bytes(&mut e, seq, "q", vt, now);
        vt.feed_str("q");
        e.echo_ack(*seq, vt, now + ms(300));
        e
    }

    #[test]
    fn the_first_key_of_an_epoch_is_held_back_until_its_echo_confirms_it() {
        let t0 = Instant::now();
        let mut vt = prompt();
        let mut seq = 0;
        let mut e = slow();
        type_bytes(&mut e, &mut seq, "q", &vt, t0);
        assert!(e.shown().is_empty(), "unconfirmed: nothing drawn yet");

        vt.feed_str("q");
        e.echo_ack(seq, &vt, t0 + ms(300));
        type_bytes(&mut e, &mut seq, "w", &vt, t0 + ms(310));
        assert_eq!(
            drawn(&e),
            vec![(0, 3, 'w')],
            "confirmed: the next key shows at once"
        );
        assert_eq!(e.shown().cursor, Some((0, 4)));
    }

    #[test]
    fn a_correct_echo_retires_the_prediction() {
        let t0 = Instant::now();
        let mut vt = prompt();
        let mut seq = 0;
        let mut e = confirmed(&mut vt, &mut seq, t0);
        type_bytes(&mut e, &mut seq, "w", &vt, t0 + ms(400));
        assert!(!e.shown().is_empty());
        vt.feed_str("w");
        e.echo_ack(seq, &vt, t0 + ms(700));
        assert!(
            e.shown().is_empty(),
            "the real screen says it all: {:?}",
            e.shown()
        );
    }

    #[test]
    fn keys_that_never_echo_are_never_drawn() {
        let t0 = Instant::now();
        let mut vt = prompt();
        let mut seq = 0;
        let mut e = confirmed(&mut vt, &mut seq, t0);
        // Enter: a new epoch, then a password prompt that echoes nothing.
        type_bytes(&mut e, &mut seq, "\r", &vt, t0 + ms(400));
        vt.feed_str("\r\nPassword: ");
        type_bytes(&mut e, &mut seq, "s", &vt, t0 + ms(500));
        assert!(drawn(&e).is_empty(), "a new epoch starts hidden");
        e.echo_ack(seq, &vt, t0 + ms(800));
        type_bytes(&mut e, &mut seq, "e", &vt, t0 + ms(810));
        assert!(
            drawn(&e).is_empty(),
            "the unechoed key killed its epoch; the next one waits again: {:?}",
            e.shown()
        );
    }

    #[test]
    fn a_wrong_guess_in_a_confirmed_epoch_wipes_every_prediction() {
        let t0 = Instant::now();
        let mut vt = prompt();
        let mut seq = 0;
        let mut e = confirmed(&mut vt, &mut seq, t0);
        type_bytes(&mut e, &mut seq, "w", &vt, t0 + ms(400));
        vt.feed_str("Z");
        e.echo_ack(seq, &vt, t0 + ms(700));
        assert!(e.shown().is_empty(), "reset: {:?}", e.shown());
    }

    #[test]
    fn backspace_and_arrows_move_the_predicted_cursor() {
        let t0 = Instant::now();
        let mut vt = prompt();
        let mut seq = 0;
        let mut e = confirmed(&mut vt, &mut seq, t0);
        type_bytes(&mut e, &mut seq, "\x7f", &vt, t0 + ms(400));
        assert_eq!(e.shown().cursor, Some((0, 2)));
        assert_eq!(drawn(&e), vec![(0, 2, ' ')], "the `q` is predicted erased");
        type_bytes(&mut e, &mut seq, "\x1b[D", &vt, t0 + ms(410));
        assert_eq!(e.shown().cursor, Some((0, 1)));
        type_bytes(&mut e, &mut seq, "\x1bOC", &vt, t0 + ms(420));
        assert_eq!(e.shown().cursor, Some((0, 2)), "SS3 arrows count too");
    }

    #[test]
    fn escape_coded_keys_are_not_guessed_at() {
        let t0 = Instant::now();
        let mut vt = prompt();
        let mut seq = 0;
        let mut e = confirmed(&mut vt, &mut seq, t0);
        // A kitty-protocol `a`, and a modified arrow (word motion).
        type_bytes(&mut e, &mut seq, "\x1b[97u", &vt, t0 + ms(400));
        type_bytes(&mut e, &mut seq, "\x1b[1;5C", &vt, t0 + ms(410));
        type_bytes(&mut e, &mut seq, "w", &vt, t0 + ms(420));
        assert!(drawn(&e).is_empty(), "a fresh epoch: {:?}", e.shown());
    }

    #[test]
    fn a_fast_link_shows_nothing() {
        let t0 = Instant::now();
        let mut vt = prompt();
        let mut seq = 0;
        let mut e = PredictionEngine::new(DisplayPreference::Adaptive);
        e.set_srtt(ms(10));
        type_bytes(&mut e, &mut seq, "q", &vt, t0);
        vt.feed_str("q");
        e.echo_ack(seq, &vt, t0 + ms(20));
        type_bytes(&mut e, &mut seq, "w", &vt, t0 + ms(30));
        assert!(e.shown().is_empty());
    }

    #[test]
    fn a_slower_link_underlines_its_predictions() {
        let t0 = Instant::now();
        let mut vt = prompt();
        let mut seq = 0;
        let mut e = confirmed(&mut vt, &mut seq, t0);
        type_bytes(&mut e, &mut seq, "w", &vt, t0 + ms(400));
        let (_, _, cell) = e.shown().cells[0];
        assert!(cell.pen().is_underline(), "500ms round trip: flagged");

        let mut vt = prompt();
        let mut seq = 0;
        let mut e = PredictionEngine::new(DisplayPreference::Adaptive);
        e.set_srtt(ms(80));
        type_bytes(&mut e, &mut seq, "q", &vt, t0);
        vt.feed_str("q");
        e.echo_ack(seq, &vt, t0 + ms(100));
        type_bytes(&mut e, &mut seq, "w", &vt, t0 + ms(110));
        let (_, _, cell) = e.shown().cells[0];
        assert!(
            !cell.pen().is_underline(),
            "80ms round trip: shown, not flagged"
        );
    }

    #[test]
    fn a_resize_drops_every_prediction() {
        let t0 = Instant::now();
        let mut vt = prompt();
        let mut seq = 0;
        let mut e = confirmed(&mut vt, &mut seq, t0);
        type_bytes(&mut e, &mut seq, "w", &vt, t0 + ms(400));
        vt.resize(30, 4);
        e.screen_changed(&vt, t0 + ms(410));
        assert!(e.shown().is_empty());
    }

    #[test]
    fn the_generation_moves_only_when_what_is_drawn_does() {
        let t0 = Instant::now();
        let mut vt = prompt();
        let mut seq = 0;
        let mut e = confirmed(&mut vt, &mut seq, t0);
        let g = e.generation();
        e.screen_changed(&vt, t0 + ms(350));
        assert_eq!(e.generation(), g, "nothing shown, nothing changed");
        type_bytes(&mut e, &mut seq, "w", &vt, t0 + ms(400));
        assert_ne!(e.generation(), g);
    }

    #[test]
    fn never_predicts_nothing() {
        let t0 = Instant::now();
        let mut vt = prompt();
        let mut seq = 0;
        let mut e = PredictionEngine::new(DisplayPreference::Never);
        type_bytes(&mut e, &mut seq, "q", &vt, t0);
        vt.feed_str("q");
        e.echo_ack(seq, &vt, t0 + ms(300));
        type_bytes(&mut e, &mut seq, "w", &vt, t0 + ms(310));
        assert!(e.shown().is_empty());
        assert_eq!(e.wants_tick(), None);
    }
}
