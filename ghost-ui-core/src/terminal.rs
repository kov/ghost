//! `TerminalModel` — one terminal view as a pure reducer.
//!
//! Owns the local emulator [`Screen`] mirroring a session, plus the interactive
//! state machine (mouse gesture, text selection, cursor cell) that used to be
//! welded into the winit handlers. `update` turns a [`UiEvent`] into a list of
//! [`Cmd`] effects without touching the world; `view` renders the current state
//! to a [`Scene`]. Both are pure, so the whole interactive behavior is asserted
//! by feeding events and inspecting the returned commands, the model state, and
//! the scene — no window, no socket, no clock.
//!
//! Effects are data: keystrokes/mouse/paste leave as `Cmd::SendInput`, clipboard
//! reads as `Cmd::ReadClipboard` (answered later by `UiEvent::ClipboardText`),
//! and child output arrives as `UiEvent::SessionData`. The pure protocol helpers
//! (`query_replies`, `bracket_paste`, `selection_text`) live here too.

use ghost_render::{
    Layer, RectPx, Scene, SceneId, SceneItem, Selection, TermDamage, layout_frame_at,
};
use ghost_term::{ClipboardSelection, FullscreenOp, Line, MaximizeOp, MouseProtocol, XtwinopsOp};
use ghost_vt::query::{QueryScanner, ReplyCtx, ThemeColors};
use ghost_vt::screen::{self, Screen};

use std::collections::HashMap;

use crate::input::{Key, KeyAlternates, KeyEventKind, Mods, NamedKey};
use crate::{
    CellMetrics, Cmd, PointPx, PointerButton, PointerIcon, PointerPhase, SessionId, UiEvent,
    encode, mouse,
};

/// Lines moved per mouse-wheel notch when scrolling local scrollback.
const SCROLL_LINES: i64 = 3;

/// Cadence of selection-autoscroll steps while a drag hovers past a grid edge.
const AUTOSCROLL_MS: u64 = 30;
/// Fastest selection autoscroll, in lines per step (reached a few line-heights
/// past the edge; one line per step right at it).
const AUTOSCROLL_MAX: i64 = 5;

/// User zoom (font-scale) bounds and step, inherited from the retired ghost-gtk frontend.
const ZOOM_MIN: f32 = 0.5;
const ZOOM_MAX: f32 = 3.0;
const ZOOM_STEP: f32 = 0.1;

/// One zoom step from `scale` by `delta`, rounded to a clean tenth (so repeated
/// steps don't drift) and clamped to [`ZOOM_MIN`]..=[`ZOOM_MAX`].
fn step_zoom(scale: f32, delta: f32) -> f32 {
    (((scale + delta) * 10.0).round() / 10.0).clamp(ZOOM_MIN, ZOOM_MAX)
}

/// A local-viewport scroll requested by a Shift+navigation key.
enum Scroll {
    /// Move by N lines (positive = up, into history).
    By(i64),
    /// Jump to the oldest retained line.
    Top,
    /// Jump back to the live bottom.
    Bottom,
}

/// Granularity of an in-progress selection drag, latched at press: a plain drag
/// selects by cell; a double-click drag extends by whole words; a triple-click
/// drag by whole lines.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SelectMode {
    Char,
    Word,
    Line,
}

/// A frontend-handled key combo intercepted before encoding so it drives the
/// app, not the child.
pub enum Shortcut {
    Paste,
    Copy,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    Quit,
    /// Open a new window (Cmd+N / Ctrl+Shift+N).
    NewWindow,
    /// Open a new window connected to a host over SSH (Cmd+S / Ctrl+Shift+S /
    /// Alt+S). Not bare Ctrl+S — that stays terminal flow control (XOFF).
    NewSshWindow,
    /// Open a new SSH session in *this* window (Cmd+G / Ctrl+Shift+G / Alt+G —
    /// "go"). Not bare Ctrl+G — that stays BEL (^G). Like Alt+S, claiming Alt+G on
    /// Linux and the Ctrl+Shift+G disambiguated key is a minor, deliberate loss.
    NewSshSession,
    /// Close this window (Cmd+W / Ctrl+Shift+W).
    CloseWindow,
    /// Spawn a fresh session in this window and switch to it (Cmd+T / Alt+T).
    NewSession,
}

/// Classify a pressed key as a frontend shortcut, if it is one. The primary
/// modifier is Cmd on macOS and Ctrl elsewhere; copy/paste keep the stricter
/// Cmd / Ctrl+Shift combo so a bare Ctrl+C still sends SIGINT, while zoom uses
/// plain Cmd/Ctrl + `+`/`=`/`-`/`0` (carried over from ghost-gtk's `<Primary>` accels).
pub fn classify_shortcut(key: &Key, mods: Mods) -> Option<Shortcut> {
    // New session: Cmd+T on macOS, Alt+T elsewhere. Checked first because Alt+T is
    // not a "primary" (Cmd/Ctrl) chord, yet must still resolve here rather than be
    // encoded and sent to the child as Meta+T.
    let new_session = if cfg!(target_os = "macos") {
        mods.sup && !mods.ctrl && !mods.alt
    } else {
        mods.alt && !mods.sup && !mods.ctrl
    };
    if new_session && matches!(key, Key::Char(s) if s.eq_ignore_ascii_case("t")) {
        return Some(Shortcut::NewSession);
    }

    // Copy/paste/new-window/new-ssh-window are also on Alt on Linux (in addition to
    // the Ctrl+Shift chord below) — a terminal-app convention that keeps Ctrl free for
    // the shell. Like Alt+T above, these must resolve here rather than be encoded and
    // sent to the child as Meta+<key>; only C/V/N/S are taken, so other Alt+key motions
    // (Alt+B/F, …) still reach the child. macOS keeps Alt = Option/Meta and uses Cmd.
    if !cfg!(target_os = "macos") && mods.alt && !mods.sup && !mods.ctrl {
        match key {
            Key::Char(s) if s.eq_ignore_ascii_case("c") => return Some(Shortcut::Copy),
            Key::Char(s) if s.eq_ignore_ascii_case("v") => return Some(Shortcut::Paste),
            Key::Char(s) if s.eq_ignore_ascii_case("n") => return Some(Shortcut::NewWindow),
            Key::Char(s) if s.eq_ignore_ascii_case("s") => return Some(Shortcut::NewSshWindow),
            Key::Char(s) if s.eq_ignore_ascii_case("g") => return Some(Shortcut::NewSshSession),
            _ => {}
        }
    }

    let primary = mods.sup || mods.ctrl;
    if !primary {
        return None;
    }
    // Quit is Cmd+Q (macOS) or bare Ctrl+Q on every platform, mirroring Cmd+Q.
    // Ctrl+Shift+Q is deliberately NOT quit — it's the escape hatch that falls
    // through to the encoder and still sends XON (0x11) to the child.
    if matches!(key, Key::Char(s) if s.eq_ignore_ascii_case("q"))
        && (mods.sup || (mods.ctrl && !mods.shift))
    {
        return Some(Shortcut::Quit);
    }
    if mods.sup || mods.shift {
        match key {
            Key::Char(s) if s.eq_ignore_ascii_case("v") => return Some(Shortcut::Paste),
            Key::Char(s) if s.eq_ignore_ascii_case("c") => return Some(Shortcut::Copy),
            // Window management, same Cmd / Ctrl+Shift gating. Bare Ctrl+N/W stay
            // terminal input.
            Key::Char(s) if s.eq_ignore_ascii_case("n") => return Some(Shortcut::NewWindow),
            // New SSH window: Cmd+S / Ctrl+Shift+S. The Shift gate keeps bare
            // Ctrl+S as terminal flow control (XOFF).
            Key::Char(s) if s.eq_ignore_ascii_case("s") => return Some(Shortcut::NewSshWindow),
            // New SSH session in this window: Cmd+G / Ctrl+Shift+G. The Shift gate
            // keeps bare Ctrl+G as BEL (^G).
            Key::Char(s) if s.eq_ignore_ascii_case("g") => return Some(Shortcut::NewSshSession),
            Key::Char(s) if s.eq_ignore_ascii_case("w") => return Some(Shortcut::CloseWindow),
            _ => {}
        }
    }
    match key {
        // '+' is usually Shift+'='; accept both so the combo works either way.
        Key::Char(s) if s == "+" || s == "=" => Some(Shortcut::ZoomIn),
        Key::Char(s) if s == "-" => Some(Shortcut::ZoomOut),
        Key::Char(s) if s == "0" => Some(Shortcut::ZoomReset),
        _ => None,
    }
}

/// One terminal view's reducer state.
pub struct TerminalModel {
    session: SessionId,
    /// Base (logical, 1x) cell metrics; physical metrics are these scaled by
    /// [`scale`](Self::scale).
    metrics: CellMetrics,
    /// Device scale factor (physical px per logical px) from the last resize, so
    /// glyphs and the grid track HiDPI displays. 1.0 until the shell reports one.
    scale: f32,
    /// User zoom (font-scale), driven by Cmd/Ctrl +/-/0. Multiplies the device
    /// scale, so a HiDPI display and a zoom level compose.
    zoom: f32,
    size_px: (u32, u32),
    /// Size (physical px) of the display the window is on, from
    /// [`UiEvent::DisplaySize`] — how far a maximizing program can grow the grid,
    /// and what `CSI 19 t` reports. `None` until the shell says (a headless model
    /// never hears): a nominal display then stands in, so a program's arithmetic
    /// still adds up.
    display_px: Option<(u32, u32)>,
    /// Whether a program has iconified the window (XTWINOPS `CSI 2 t`), for
    /// `CSI 11 t`. What the window manager did with the request is its own
    /// business — this is what the program asked for and what it reads back.
    iconified: bool,
    /// Whether the window is maximized or full-screen at a program's asking, and
    /// the grid it had before each — restoring (`CSI 9 ; 0 t`, `CSI 10 ; 0 t`) puts
    /// that grid back rather than guessing a size. The two keep *separate* slots:
    /// they nest (a program may full-screen a maximized window and expect leaving
    /// full-screen to land back on the maximize), and one shared slot let a restore
    /// of a state we were never in steal the other's.
    maximized: bool,
    fullscreen: bool,
    maximize_restore: Option<(u16, u16)>,
    fullscreen_restore: Option<(u16, u16)>,
    /// Inner padding in *logical* px per side between the grid and the window edges.
    /// Scaled by the device factor (not zoom — it's a fixed window-space border) into
    /// [`Self::pad_px`], which insets the grid, the scene item rect, pointer
    /// hit-testing and the IME caret. The scene canvas stays the full window, so the
    /// border is filled by the terminal background. 0 = flush to the edges.
    pad: f32,
    screen: Screen,
    scanner: QueryScanner,
    cols: u16,
    rows: u16,
    /// Last 1-based `(col, row)` cell the pointer was over (`None` until moved).
    cursor_cell: Option<(u16, u16)>,
    /// Button currently held (drag vs hover).
    held: Option<mouse::Button>,
    /// Whether the in-progress gesture is forwarded to the child (latched at press).
    gesture_report: bool,
    /// The drag anchor's extent (a single cell, or the whole word/line under
    /// the press), latched at press. Rows are ABSOLUTE line indices (the
    /// monotonic lines-ever space, see [`Self::abs_top`]) so the anchor stays
    /// pinned to its content while the viewport scrolls mid-drag.
    sel_anchor: Option<Selection>,
    /// Granularity of the active drag (cell / word / line), latched at press.
    sel_mode: SelectMode,
    /// The selection, in ABSOLUTE line indices like `sel_anchor`; the public
    /// [`Self::selection`] getter projects it into the current viewport.
    selection: Option<Selection>,
    /// Armed selection autoscroll: lines per step (positive = into history),
    /// 0 = off. Set from the pointer's overshoot past the grid edge while
    /// dragging; each `Tick` steps and re-arms until the drag ends or the
    /// viewport hits its limit.
    autoscroll: i64,
    /// Lines scrolled up into history; 0 = pinned to the live bottom.
    scroll_offset: usize,
    /// In-progress IME composition string; non-empty means composing, during
    /// which raw key input is suppressed.
    preedit: String,
    /// Last window title pushed to the shell, to emit `SetTitle` only on change.
    last_title: String,
    /// kitty-graphics image ids whose pixels have been uploaded to the renderer,
    /// mapped to the [`generation`](ghost_term::graphics::Image::generation) sent, so
    /// the (potentially large) blob is sent once per transmit — but a re-transmit
    /// under an existing id (a higher generation) re-uploads the replaced pixels
    /// rather than leaving the stale image on screen.
    uploaded_images: HashMap<u32, u64>,
    /// Count of stored graphics images at the last feed. When it grows, a newly
    /// stored image may be referenced by a placeholder that has already scrolled
    /// out of the live viewport, so we rescan all retained lines (not just the
    /// viewport) for placeholder ids to upload.
    last_image_count: usize,
    ended: bool,
    /// The transport dropped but the session may still be alive on the far side
    /// (a remote session over ssh): the screen is frozen and dimmed while the shell
    /// retries the attach. Cleared when it re-attaches ([`UiEvent::SessionReattached`]),
    /// whose resync repaints the recovered screen. Distinct from `ended` — this
    /// session is NOT gone, so it must never tear the tile down.
    reconnecting: bool,
    /// Viewport rows dirtied by feeds since the last present, from the core's per-feed
    /// hint (`Screen::feed`) — the localizable part of the [`TermDamage`] `view` reports.
    /// `None` = no feed changed the viewport; a range accumulates across coalesced feeds.
    feed_dirty: Option<(usize, usize)>,
    /// The view-shaping state at the last present. `view` reports `TermDamage::All` when
    /// any of it moved (scroll, selection, resize, zoom, HiDPI scale) — changes a per-row
    /// feed hint can't localize — and otherwise reports just `feed_dirty`. `None` until
    /// the first present, so the first frame is always `All`.
    presented: Option<Presented>,
    /// The visible window slid under a scroll offset pinned at the scrollback cap
    /// (eviction moved every row but the offset couldn't grow to follow it) since the
    /// last present. Like a scroll, this is whole-view damage the per-row feed hint
    /// can't localize, so [`Self::damage`] reports `All` until the next present clears it.
    view_slid: bool,
    /// A repaint is being held back because the app is mid synchronized-output
    /// frame (DEC mode 2026). Released by the mode resetting, or by the tick
    /// scheduled when the hold began (so a stuck app can't freeze the window).
    sync_held: bool,
    /// Whether this session's window currently holds keyboard focus. Tracked so
    /// that when an app first enables focus reporting (DEC ?1004) we can report
    /// the *current* focus state immediately, not only on the next change (see
    /// [`Self::session_data`]). Warm background mirrors never receive a focus
    /// event, so they correctly stay `false`.
    focused: bool,
    /// The scheme's default fg/bg, for answering OSC 10/11 color queries (vim
    /// and fzf theme detection). Defaults to ghost's default scheme; the shell
    /// overrides it when a scheme is configured (see `set_theme`).
    theme: ThemeColors,
    /// The interned link id under a Ctrl/Cmd-hover, if any: `view` underlines
    /// every visible run of it and the pointer shows a hand (see
    /// [`Cmd::PointerIcon`]). Updated on pointer motion.
    hovered_link: Option<u16>,
    /// The session's user-chosen display name (`ghost rename`), empty if
    /// unlabeled. A human-facing label only: `session` stays the immutable id
    /// every effect routes by. Feeds the [`Self::title`] fallback.
    display_name: String,
    /// Render-gate counters (see [`TermTrace`]). Only `feeds_seen`, `visible_feeds`,
    /// `redraws_emitted`, the sync-hold tallies, and `presents_marked` are stored
    /// here; the live `sync_held`/`feed_dirty` are folded in by [`Self::trace`].
    trace: TermTrace,
}

/// How long a synchronized-output hold may last before the scheduled tick
/// releases it anyway. Generous for an atomic repaint burst, short enough that
/// an app dying between BSU and ESU reads as a hiccup, not a hang.
const SYNC_RELEASE_MS: u64 = 150;

/// Always-maintained counters over the foreground render gates, snapshotted by
/// the shell's render trace to diagnose a stalled single view — feeds arriving
/// while the foreground stops presenting (the recurring "Claude Code freezes,
/// preview stays live, a scroll fixes it" bug). Pure integer counters, no clock,
/// so the model stays deterministic; the shell timestamps them (`ghost::render`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TermTrace {
    /// `session_data` feeds carrying bytes for this session.
    pub feeds_seen: u64,
    /// Of those, feeds that changed something visible (`want_redraw`).
    pub visible_feeds: u64,
    /// Repaints this model emitted from the feed/tick path (`Cmd::Redraw`).
    pub redraws_emitted: u64,
    /// The live synchronized-output (DEC 2026) hold flag.
    pub sync_held: bool,
    /// Synchronized-output holds entered.
    pub sync_holds: u64,
    /// Holds released by the mode resetting (2026l).
    pub sync_released_by_reset: u64,
    /// Holds released by the backstop or an animation tick.
    pub sync_released_by_tick: u64,
    /// Visible feeds swallowed by an open hold (their repaint deferred).
    pub feeds_while_held: u64,
    /// Accumulated feed damage awaiting the next present.
    pub feed_dirty: Option<(usize, usize)>,
    /// `mark_presented` calls (a present the shell confirmed).
    pub presents_marked: u64,
}

/// Snapshot of the view-shaping state at a present (see [`TerminalModel::presented`]).
#[derive(Clone, PartialEq)]
struct Presented {
    scroll: usize,
    selection: Option<Selection>,
    size: (u32, u32),
    zoom: f32,
    scale: f32,
    /// App-set dynamic colors (OSC 10/11/12) at the present. A change dirties
    /// no rows — the default bg is every otherwise-untouched pixel — so it
    /// must force whole-view damage the way a resize does.
    colors: [Option<[u8; 3]>; 3],
}

/// A cheap identity of one direct graphics placement — image id, placement id, cell
/// position, footprint, and z — for detecting a feed that changed the placed images
/// without writing a cell (see [`TerminalModel::placement_signature`]).
type PlacementSig = (u32, u32, usize, usize, u32, u32, i32);

impl TerminalModel {
    pub fn new(session: SessionId, cols: u16, rows: u16, metrics: CellMetrics) -> Self {
        let size_px = (
            (f32::from(cols) * metrics.advance) as u32,
            (f32::from(rows) * metrics.line_height) as u32,
        );
        let screen = Screen::new(cols, rows, screen::DEFAULT_SCROLLBACK);
        TerminalModel {
            session,
            metrics,
            scale: 1.0,
            zoom: 1.0,
            size_px,
            display_px: None,
            iconified: false,
            maximized: false,
            fullscreen: false,
            maximize_restore: None,
            fullscreen_restore: None,
            pad: 0.0,
            screen,
            scanner: QueryScanner::new(),
            cols,
            rows,
            cursor_cell: None,
            held: None,
            gesture_report: false,
            sel_anchor: None,
            sel_mode: SelectMode::Char,
            selection: None,
            autoscroll: 0,
            scroll_offset: 0,
            preedit: String::new(),
            last_title: String::new(),
            uploaded_images: HashMap::new(),
            last_image_count: 0,
            ended: false,
            reconnecting: false,
            feed_dirty: None,
            presented: None,
            view_slid: false,
            sync_held: false,
            focused: false,
            theme: ThemeColors::default(),
            hovered_link: None,
            display_name: String::new(),
            trace: TermTrace::default(),
        }
    }

    /// Set the scheme's default fg/bg reported to apps that query them
    /// (OSC 10/11). Called once per model right after construction; on a real
    /// theme *change*, sessions subscribed to mode 2031 get the unsolicited
    /// `CSI ? 997 ; Ps n` dark/light notification.
    pub fn set_theme(&mut self, theme: ThemeColors) -> Vec<Cmd> {
        let changed = theme != self.theme;
        self.theme = theme;
        if changed && self.screen.vt().dec_mode_state(2031) == ghost_term::ModeReport::Set {
            let colors = self.screen.effective_colors(self.theme);
            return self.send(ghost_vt::query::color_scheme_report(&colors));
        }
        Vec::new()
    }

    /// The [`TermDamage`] to stamp on this session's scene item: `All` on the first
    /// frame or when any view-shaping state moved since the last present (scroll,
    /// selection, resize, zoom, scale), otherwise the feed-dirtied rows (`None` if a
    /// present was requested but nothing on screen changed). See [`Self::presented`].
    fn damage(&self) -> TermDamage {
        let moved = self.view_slid
            || match &self.presented {
                None => true,
                Some(p) => {
                    p.scroll != self.scroll_offset
                        || p.selection != self.selection
                        || p.size != self.size_px
                        || p.zoom != self.zoom
                        || p.scale != self.scale
                        || p.colors != self.render_colors()
                }
            };
        if moved {
            TermDamage::All
        } else {
            match self.feed_dirty {
                // `feed_dirty` rows are live-viewport rows, but the renderer bands
                // `TermDamage::Rows` in frame space. While scrolled back a stable offset
                // (a scroll *change* is already `All` via `moved`), live row L is drawn at
                // frame row L + offset, so shift the claim into frame space and clip to the
                // visible window; a range entirely below the fold changed nothing on
                // screen. Omitting this left the banded foreground texture stale on an
                // in-place rewrite while scrolled — the recurring foreground-stall bug.
                Some((lo, hi)) => {
                    let rows = self.rows as usize;
                    let lo = lo + self.scroll_offset;
                    if lo >= rows {
                        TermDamage::None
                    } else {
                        TermDamage::Rows {
                            lo,
                            hi: (hi + self.scroll_offset).min(rows - 1),
                        }
                    }
                }
                None => TermDamage::None,
            }
        }
    }

    /// Record that the current view was just composited: snapshot the view-shaping state
    /// and drop the accumulated feed damage, so the next [`Self::damage`] measures from
    /// here. Driven by the shell after a successful present (never from `view`, which is
    /// called more than once per frame), so damage is never cleared before it is applied.
    pub fn mark_presented(&mut self) {
        self.presented = Some(Presented {
            scroll: self.scroll_offset,
            selection: self.selection,
            size: self.size_px,
            zoom: self.zoom,
            scale: self.scale,
            colors: self.render_colors(),
        });
        self.feed_dirty = None;
        self.view_slid = false;
        self.trace.presents_marked += 1;
    }

    /// A snapshot of this session's render-gate counters, with the live
    /// `sync_held`/`feed_dirty` folded in (see [`TermTrace`]).
    pub fn trace(&self) -> TermTrace {
        TermTrace {
            sync_held: self.sync_held,
            feed_dirty: self.feed_dirty,
            ..self.trace
        }
    }

    /// The app-set dynamic colors (OSC 10/11/12) the renderer paints with —
    /// the view-shaping color state [`Presented`] snapshots.
    fn render_colors(&self) -> [Option<[u8; 3]>; 3] {
        let vt = self.screen.vt();
        [
            vt.dynamic_foreground(),
            vt.dynamic_background(),
            vt.dynamic_cursor_color(),
        ]
    }

    pub fn screen(&self) -> &Screen {
        &self.screen
    }

    /// The session id these effects target.
    pub fn session(&self) -> &str {
        &self.session
    }

    /// Set the session's user-chosen display name (empty = unlabeled). A label
    /// for humans only — routing stays on the immutable [`Self::session`] id.
    pub fn set_display_name(&mut self, name: String) {
        self.display_name = name;
    }

    /// The session's user-chosen display name, empty if unlabeled.
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    /// The name a human should see for this session: its display name when
    /// labeled, else its immutable id.
    pub fn display(&self) -> &str {
        if self.display_name.is_empty() {
            &self.session
        } else {
            &self.display_name
        }
    }

    /// The window title for this session. A user-chosen display name (`ghost
    /// rename`) prefixes the app-set title (OSC 0/2): "label — title" — the
    /// label only earns the titlebar when it differs from the auto-generated
    /// session id, since the id carries no meaning the app title doesn't.
    /// With one of the two missing the other stands alone, and with neither
    /// the id does — so the titlebar always shows something meaningful. Lives
    /// on the screen state and the label, so it is remembered across
    /// background/foreground switches.
    pub fn title(&self) -> String {
        let title = self.screen.title();
        let label = (!self.display_name.is_empty() && self.display_name != self.session)
            .then_some(self.display_name.as_str());
        match (label, title.is_empty()) {
            (Some(label), false) => format!("{label} — {title}"),
            (Some(label), true) => label.to_string(),
            (None, false) => title.to_string(),
            (None, true) => self.session.clone(),
        }
    }

    /// The terminal's grid size in cells (cols, rows).
    pub fn dims(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// The selection projected into the current viewport for painting, clamped
    /// at the window edges. The model keeps the full range in absolute line
    /// space (it survives scrolling and can span beyond the window); `None`
    /// when there is no selection or it lies entirely off-screen.
    pub fn selection(&self) -> Option<Selection> {
        let s = self.selection?;
        let top = self.abs_top();
        let rows = self.rows as usize;
        if s.end.0 < top || s.start.0 >= top + rows {
            return None;
        }
        let start = if s.start.0 < top {
            (0, 0)
        } else {
            (s.start.0 - top, s.start.1)
        };
        let end = if s.end.0 >= top + rows {
            (rows - 1, (self.cols as usize).saturating_sub(1))
        } else {
            (s.end.0 - top, s.end.1)
        };
        Some(Selection { start, end })
    }

    /// The first viewport row's absolute line index — the monotonic
    /// lines-ever-scrolled-off space selections are anchored in.
    fn abs_top(&self) -> usize {
        self.screen.vt().lines_scrolled_off() - self.scroll_offset
    }

    /// Lift a viewport cell into absolute line space.
    fn abs_cell(&self, (row, col): (usize, usize)) -> (usize, usize) {
        (self.abs_top() + row, col)
    }

    /// Lift a viewport-relative selection into absolute line space.
    fn abs_sel(&self, sel: Selection) -> Selection {
        Selection {
            start: (sel.start.0 + self.abs_top(), sel.start.1),
            end: (sel.end.0 + self.abs_top(), sel.end.1),
        }
    }

    /// Whether the child exited / the session closed.
    pub fn ended(&self) -> bool {
        self.ended
    }

    /// Whether the transport dropped and the shell is retrying (frozen + dimmed).
    pub fn reconnecting(&self) -> bool {
        self.reconnecting
    }

    /// Enter or leave the reconnecting hold for `name` (a no-op for another
    /// session). A redraw repaints the dim; the reconnected resync repaints the
    /// recovered screen. Never sets `ended` — the session isn't gone.
    fn set_reconnecting(&mut self, name: &str, on: bool) -> Vec<Cmd> {
        if name != self.session || self.reconnecting == on {
            return Vec::new();
        }
        self.reconnecting = on;
        vec![Cmd::Redraw]
    }

    /// Apply an event, returning the effects to perform.
    pub fn update(&mut self, ev: UiEvent) -> Vec<Cmd> {
        match ev {
            UiEvent::Key {
                key,
                mods,
                kind,
                alts,
            } => self.key(&key, mods, kind, alts),
            UiEvent::Text(s) => self.text(&s),
            UiEvent::Preedit(s) => self.set_preedit(s),
            UiEvent::SetZoom(z) => self.apply_zoom(z.clamp(ZOOM_MIN, ZOOM_MAX)),
            UiEvent::Pointer {
                phase,
                button,
                pos,
                mods,
                wheel_dy,
                clicks,
            } => self.pointer(phase, button, pos, mods, wheel_dy, clicks),
            UiEvent::Focus(focused) => self.focus(focused),
            UiEvent::Resize { w_px, h_px, scale } => self.resize(w_px, h_px, scale as f32),
            UiEvent::DisplaySize { w_px, h_px } => {
                self.display_px = Some((w_px, h_px));
                Vec::new()
            }
            UiEvent::ClipboardText(text) => self.paste(text),
            UiEvent::SessionData { name, bytes, ended } => self.session_data(&name, &bytes, ended),
            UiEvent::SessionDisconnected { name } => self.set_reconnecting(&name, true),
            UiEvent::SessionReattached { name } => self.set_reconnecting(&name, false),
            // The clock releases a synchronized-output hold and steps an armed
            // selection autoscroll.
            UiEvent::Tick { .. } => {
                let mut cmds = if std::mem::take(&mut self.sync_held) {
                    self.trace.sync_released_by_tick += 1;
                    self.trace.redraws_emitted += 1;
                    vec![Cmd::Redraw]
                } else {
                    Vec::new()
                };
                cmds.extend(self.autoscroll_tick());
                cmds
            }
            // A lone terminal ignores enumeration, subscription, and group
            // state, and never sees `AdoptSession` — `RootModel` handles those.
            UiEvent::SessionList(_)
            | UiEvent::AdoptSession(_)
            | UiEvent::SessionPush { .. }
            | UiEvent::SessionsChanged
            | UiEvent::GroupsLoaded(_)
            | UiEvent::DeadSessions(_) => Vec::new(),
        }
    }

    /// Combined render scale: device scale × user zoom. The shell multiplies the
    /// base font size by this to rasterize glyphs at the same size the grid is
    /// laid out for, keeping the two in lockstep.
    pub fn render_scale(&self) -> f32 {
        self.scale * self.zoom
    }

    /// Set the inner padding (logical px per side). The caller re-grids by resizing
    /// afterwards; storing it here is enough for [`Self::view`] and hit-testing.
    pub fn set_padding(&mut self, pad_logical: f32) {
        self.pad = pad_logical.max(0.0);
    }

    /// Padding in physical px per side: the logical value scaled by the device factor
    /// (device scale, not the zoom-inclusive render scale — the border is a fixed
    /// window-space inset that must not grow when the font is zoomed).
    fn pad_px(&self) -> f32 {
        self.pad * self.scale
    }

    /// Physical cell metrics: the logical metrics scaled by the combined render
    /// scale, so layout and hit-testing match what the renderer rasterizes.
    fn effective_metrics(&self) -> CellMetrics {
        let s = self.render_scale();
        CellMetrics {
            advance: self.metrics.advance * s,
            line_height: self.metrics.line_height * s,
        }
    }

    /// Physical inner-window size that fits a `cols` × `rows` grid at the current
    /// metrics — the inverse of the grid math in [`Self::resize`], padding included.
    /// Rounded *up* so the floor there gives back exactly this grid.
    fn window_px_for_grid(&self, cols: u16, rows: u16) -> (u32, u32) {
        let m = self.effective_metrics();
        let pad = self.pad_px();
        let w = (f32::from(cols) * m.advance + 2.0 * pad).ceil().max(1.0);
        let h = (f32::from(rows) * m.line_height + 2.0 * pad)
            .ceil()
            .max(1.0);
        (w as u32, h as u32)
    }

    /// Re-grid the screen at a program's asking. Child output is untrusted, so the
    /// ask is bounded by what a terminal could be (`ghost_term::MAX_PROGRAM_*`) —
    /// a `CSI 4 ; 65535 ; 65535 t` names a grid no display has and no host should
    /// try to allocate. The emulator bounds the resizes *it* performs the same way.
    fn resize_grid(&mut self, cols: u16, rows: u16) {
        let cols = cols.clamp(1, ghost_term::MAX_PROGRAM_COLS as u16);
        let rows = rows.clamp(1, ghost_term::MAX_PROGRAM_ROWS as u16);
        self.screen.resize(cols, rows);
    }

    /// One dimension of an XTWINOPS resize: omitted keeps what it has, zero is
    /// xterm's "as much as the display fits", anything else is itself.
    fn fit_dimension(asked: Option<u16>, current: u16, display: u16) -> u16 {
        match asked {
            None => current,
            Some(0) => display,
            Some(n) => n,
        }
    }

    /// The grid a window filling the whole display would hold — what a maximized
    /// or full-screen program gets, and what `CSI 19 t` reports. Until the shell
    /// says how big the display is (a headless model is never told), the nominal
    /// one stands in — but measured in *our* cell, so the grid we report and the
    /// pixels we report are the same display. A program that maximizes and then
    /// checks its arithmetic finds it adds up.
    fn display_grid(&self) -> (u16, u16) {
        let (w_px, h_px) = self
            .display_px
            .unwrap_or(ghost_vt::query::NOMINAL_DISPLAY_PX);
        let m = self.effective_metrics();
        let pad = self.pad_px();
        let cols = ((w_px as f32 - 2.0 * pad).max(0.0) / m.advance)
            .floor()
            .max(1.0);
        let rows = ((h_px as f32 - 2.0 * pad).max(0.0) / m.line_height)
            .floor()
            .max(1.0);
        (cols as u16, rows as u16)
    }

    /// Carry out the window ops the emulator queued for us (see
    /// [`ghost_term::XtwinopsOp`]). The ones that grow the window re-grid the
    /// screen here — the emulator can't, since the size depends on the display —
    /// and the caller's grid reconciliation turns that into the window resize and
    /// the child's SIGWINCH, exactly as for a DECCOLM.
    ///
    /// The window may of course refuse any of it; a program reads back what it
    /// asked for, which is all xterm promises it.
    fn apply_window_ops(&mut self) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        for op in self.screen.take_window_ops() {
            // The screen's own size, not `self.cols`/`self.rows` — those are
            // reconciled once, after this loop, so within a burst they are the grid
            // we *started* with. Two ops in one write (`\e[9;2t\e[9;3t`: grow one
            // axis, then the other) have to each see what the one before it left.
            let (cols, rows) = self.screen.dimensions();
            let (display_cols, display_rows) = self.display_grid();
            match op {
                XtwinopsOp::Iconify => {
                    self.iconified = true;
                    cmds.push(Cmd::SetIconified(true));
                }
                XtwinopsOp::Deiconify => {
                    self.iconified = false;
                    cmds.push(Cmd::SetIconified(false));
                }
                XtwinopsOp::Maximize(op) => {
                    let leaving = op == MaximizeOp::Restore;
                    let grid = match op {
                        MaximizeOp::Both => Some((display_cols, display_rows)),
                        MaximizeOp::Horizontally => Some((display_cols, rows)),
                        MaximizeOp::Vertically => Some((cols, display_rows)),
                        // Only a maximize we made has anything to come back to.
                        MaximizeOp::Restore => self.maximize_restore.take(),
                    };
                    if !leaving && !self.maximized {
                        // Save on the way in only: growing the second axis (or
                        // maximizing twice) must not overwrite the grid from before
                        // the first with an already-grown one.
                        self.maximize_restore = Some((cols, rows));
                    }
                    self.maximized = !leaving;
                    // Only a both-axes maximize is a state a window manager has;
                    // one axis is just a size, which the re-grid below asks for.
                    if op == MaximizeOp::Both || leaving {
                        cmds.push(Cmd::SetMaximized(!leaving));
                    }
                    if let Some((cols, rows)) = grid {
                        self.resize_grid(cols, rows);
                    }
                }
                XtwinopsOp::Fullscreen(op) => {
                    let entering = match op {
                        FullscreenOp::Enter => true,
                        FullscreenOp::Leave => false,
                        FullscreenOp::Toggle => !self.fullscreen,
                    };
                    // Full-screen keeps its *own* saved grid, and only touches it on
                    // a real transition. Sharing one slot with the maximize let a
                    // leave-full-screen we were never in (a no-op in xterm, and one
                    // programs send defensively) walk off with the grid the maximize
                    // saved. Keeping them apart also nests the two the way xterm
                    // does: full-screen over a maximize comes back to the maximized
                    // grid, and the maximize still restores what preceded it.
                    if entering && !self.fullscreen {
                        self.fullscreen_restore = Some((cols, rows));
                        self.resize_grid(display_cols, display_rows);
                    } else if !entering
                        && self.fullscreen
                        && let Some((cols, rows)) = self.fullscreen_restore.take()
                    {
                        self.resize_grid(cols, rows);
                    }
                    self.fullscreen = entering;
                    cmds.push(Cmd::SetFullscreen(entering));
                }
                // A resize the emulator left to us: one with a zero dimension, which
                // xterm reads as "as big as the display fits" — it has no display.
                // An omitted dimension keeps the one it has.
                XtwinopsOp::Resize(w, h) => {
                    let cols = Self::fit_dimension(w, cols, display_cols);
                    let rows = Self::fit_dimension(h, rows, display_rows);
                    self.resize_grid(cols, rows);
                }
                // The same, in pixels: only we know how many a cell is.
                XtwinopsOp::ResizePixels(w_px, h_px) => {
                    let m = self.effective_metrics();
                    let pad = self.pad_px();
                    let cells = |px: u16, per_cell: f32| {
                        (((f32::from(px) - 2.0 * pad).max(0.0) / per_cell).floor() as u16).max(1)
                    };
                    let cols = match w_px {
                        Some(0) => display_cols,
                        Some(px) => cells(px, m.advance),
                        None => cols,
                    };
                    let rows = match h_px {
                        Some(0) => display_rows,
                        Some(px) => cells(px, m.line_height),
                        None => rows,
                    };
                    self.resize_grid(cols, rows);
                }
                // The emulator does these itself: a fully-given grid, the page
                // height, and the title stack (it holds the titles).
                XtwinopsOp::SetLines(..) | XtwinopsOp::PushTitle(..) | XtwinopsOp::PopTitle(..) => {
                }
            }
        }
        cmds
    }

    /// Physical-pixel rect of the text cursor, for positioning the IME candidate
    /// window. `None` while scrolled into history (no live cursor is shown).
    pub fn ime_cursor_area(&self) -> Option<RectPx> {
        if self.scroll_offset != 0 {
            return None;
        }
        let (col1, row1) = self.screen.cursor();
        let m = self.effective_metrics();
        let pad = self.pad_px();
        Some(RectPx {
            x: pad + f32::from(col1.saturating_sub(1)) * m.advance,
            y: pad + f32::from(row1.saturating_sub(1)) * m.line_height,
            w: m.advance,
            h: m.line_height,
        })
    }

    /// Set the user zoom and re-grid the child for it. A no-op (no commands)
    /// when the level is unchanged, e.g. a step that clamps at a bound.
    fn apply_zoom(&mut self, zoom: f32) -> Vec<Cmd> {
        if (zoom - self.zoom).abs() < f32::EPSILON {
            return Vec::new();
        }
        self.zoom = zoom;
        let (w, h) = self.size_px;
        self.resize(w, h, self.scale)
    }

    /// Render the current state to a single terminal scene. The canvas is the whole
    /// window; the terminal item is inset by the padding, leaving a background-filled
    /// border (see [`Self::pad`]).
    pub fn view(&self) -> Scene {
        let frame = std::rc::Rc::new(layout_frame_at(
            self.screen.vt(),
            self.effective_metrics(),
            self.scroll_offset,
        ));
        let pad = self.pad_px();
        let rect = RectPx {
            x: pad,
            y: pad,
            w: (self.size_px.0 as f32 - 2.0 * pad).max(0.0),
            h: (self.size_px.1 as f32 - 2.0 * pad).max(0.0),
        };
        let mut items = vec![SceneItem::Terminal {
            id: SceneId::Root,
            session: ghost_render::session_key(self.session()),
            rect,
            frame,
            selection: self.selection(),
            // Dim the frozen screen while the connection is being re-established.
            dim: self.reconnecting,
            damage: self.damage(),
        }];
        items.extend(self.hover_underlines());
        let mut scene = Scene::new(self.size_px);
        scene.layers.push(Layer::new(0, items));
        scene
    }

    /// Thin underline rects over every visible run of the Ctrl/Cmd-hovered
    /// hyperlink (all runs of the same URI, VTE-style), in window pixels.
    fn hover_underlines(&self) -> Vec<SceneItem> {
        let Some(id) = self.hovered_link else {
            return Vec::new();
        };
        let m = self.effective_metrics();
        let [r, g, b] = self.theme.fg;
        let color = [
            f32::from(r) / 255.0,
            f32::from(g) / 255.0,
            f32::from(b) / 255.0,
            0.9,
        ];
        let mut items = Vec::new();
        for (row, line) in self
            .screen
            .vt()
            .view_at(self.scroll_offset)
            .enumerate()
            .take(self.rows as usize)
        {
            let cells = line.cells();
            let mut col = 0;
            while col < cells.len() {
                if cells[col].pen().link_id() != Some(id) {
                    col += 1;
                    continue;
                }
                let start = col;
                while col < cells.len() && cells[col].pen().link_id() == Some(id) {
                    col += 1;
                }
                items.push(SceneItem::Rect {
                    id: SceneId::Root,
                    rect: RectPx {
                        x: start as f32 * m.advance,
                        y: (row + 1) as f32 * m.line_height - 2.0,
                        w: (col - start) as f32 * m.advance,
                        h: 1.5,
                    },
                    color,
                    radius: 0.0,
                });
            }
        }
        items
    }

    /// Widen the pending feed-dirty row range to cover `lo..=hi`.
    fn accumulate_dirty(&mut self, lo: usize, hi: usize) {
        self.feed_dirty = Some(match self.feed_dirty {
            Some((a, b)) => (a.min(lo), b.max(hi)),
            None => (lo, hi),
        });
    }

    fn send(&self, bytes: Vec<u8>) -> Vec<Cmd> {
        vec![Cmd::SendInput {
            session: self.session.clone(),
            bytes,
        }]
    }

    /// The maximum scroll-up offset (retained scrollback lines).
    fn max_scroll(&self) -> usize {
        self.screen.vt().scrollback_len()
    }

    /// Clamp `offset` to the retained history and apply it; returns whether the
    /// view actually moved.
    fn set_scroll(&mut self, offset: usize) -> bool {
        let offset = offset.min(self.max_scroll());
        let changed = offset != self.scroll_offset;
        self.scroll_offset = offset;
        changed
    }

    /// Scroll by `delta` lines (positive = up, into history). `Redraw` if moved.
    fn scroll_by(&mut self, delta: i64) -> Vec<Cmd> {
        let target = (self.scroll_offset as i64 + delta).max(0) as usize;
        if self.set_scroll(target) {
            vec![Cmd::Redraw]
        } else {
            Vec::new()
        }
    }

    /// Jump back to the live bottom; `Redraw` if it moved.
    fn snap_to_bottom(&mut self) -> Vec<Cmd> {
        if self.set_scroll(0) {
            vec![Cmd::Redraw]
        } else {
            Vec::new()
        }
    }

    /// A Shift+navigation key that scrolls the local viewport, if it is one.
    /// Plain (unshifted) keys are left for the child, matching xterm.
    fn scroll_key(&self, key: &Key, mods: Mods) -> Option<Scroll> {
        if !mods.shift || mods.ctrl || mods.alt || mods.sup {
            return None;
        }
        let page = i64::from(self.rows.saturating_sub(1)).max(1);
        match key {
            Key::Named(NamedKey::PageUp) => Some(Scroll::By(page)),
            Key::Named(NamedKey::PageDown) => Some(Scroll::By(-page)),
            Key::Named(NamedKey::Home) => Some(Scroll::Top),
            Key::Named(NamedKey::End) => Some(Scroll::Bottom),
            _ => None,
        }
    }

    /// A Ctrl/Cmd+Shift+arrow that jumps between OSC 133 prompt marks, if it
    /// is one. `true` = back into history (Up), `false` = forward (Down).
    fn prompt_jump_key(&self, key: &Key, mods: Mods) -> Option<bool> {
        if !mods.shift || !(mods.ctrl || mods.sup) || mods.alt {
            return None;
        }
        match key {
            Key::Named(NamedKey::ArrowUp) => Some(true),
            Key::Named(NamedKey::ArrowDown) => Some(false),
            _ => None,
        }
    }

    /// Scroll so the nearest prompt mark above (`back`) or below the current
    /// viewport top lands at the top. The chord is always consumed; with no
    /// prompt to jump to the view just stays put.
    fn jump_to_prompt(&mut self, back: bool) -> Vec<Cmd> {
        let vt = self.screen.vt();
        let scrolled_off = vt.lines_scrolled_off();
        let top = scrolled_off - self.scroll_offset;
        let target = if back {
            vt.prompt_rows().filter(|&r| r < top).last()
        } else {
            vt.prompt_rows().find(|&r| r > top)
        };
        let Some(row) = target else {
            return Vec::new();
        };
        if self.set_scroll(scrolled_off.saturating_sub(row)) {
            vec![Cmd::Redraw]
        } else {
            Vec::new()
        }
    }

    fn key(
        &mut self,
        key: &Key,
        mods: Mods,
        kind: KeyEventKind,
        alts: Option<KeyAlternates>,
    ) -> Vec<Cmd> {
        // While an IME composition is active the keystrokes belong to the IME
        // (which delivers its result via `Preedit`/`Text`); sending them to the
        // child as well would double-type. Releases that land while composing are
        // swallowed here too. (A release arriving just after the commit clears the
        // preedit can still slip through on backends that re-surface keyUp
        // post-commit — a benign orphan `:3` under the kitty event-types flag, for
        // the rare commit key that carries a modifier; a press-tracking fix waits
        // for the broader IME work.)
        if !self.preedit.is_empty() {
            return Vec::new();
        }
        // A release never triggers shortcuts / scrolling / IME — it only matters
        // to the kitty report-event-types flag, which the encoder handles (and
        // returns nothing for when that flag is off or the key has no escape-code
        // form). Auto-repeat (`Repeat`) otherwise behaves exactly like a press.
        if matches!(kind, KeyEventKind::Release) {
            let app_cursor = self.screen.vt().cursor_key_app_mode();
            let modify_other_keys = self.screen.vt().modify_other_keys();
            let kitty_flags = self.screen.vt().kitty_keyboard_flags();
            return match encode::encode(
                key,
                mods,
                app_cursor,
                modify_other_keys,
                kitty_flags,
                kind,
                alts,
            ) {
                Some(bytes) => self.send(bytes),
                None => Vec::new(),
            };
        }
        if let Some(scroll) = self.scroll_key(key, mods) {
            let cmds = match scroll {
                Scroll::By(d) => self.scroll_by(d),
                Scroll::Top => {
                    let top = self.max_scroll();
                    if self.set_scroll(top) {
                        vec![Cmd::Redraw]
                    } else {
                        Vec::new()
                    }
                }
                Scroll::Bottom => self.snap_to_bottom(),
            };
            // A drag in progress follows the viewport: the selection is
            // anchored in absolute line space, so re-extend it to whatever
            // content now sits under the pointer.
            self.re_extend();
            return cmds;
        }
        if let Some(back) = self.prompt_jump_key(key, mods) {
            let cmds = self.jump_to_prompt(back);
            self.re_extend();
            return cmds;
        }
        match classify_shortcut(key, mods) {
            Some(Shortcut::Paste) => vec![Cmd::ReadClipboard],
            Some(Shortcut::Copy) => self.copy(),
            Some(Shortcut::ZoomIn) => self.apply_zoom(step_zoom(self.zoom, ZOOM_STEP)),
            Some(Shortcut::ZoomOut) => self.apply_zoom(step_zoom(self.zoom, -ZOOM_STEP)),
            Some(Shortcut::ZoomReset) => self.apply_zoom(1.0),
            Some(Shortcut::Quit) => vec![Cmd::Quit],
            // Window/session management is window-level; `RootModel` intercepts
            // these before delegation, so these arms are the safety net that
            // keeps the chords from ever leaking to the child as input.
            Some(Shortcut::NewWindow) => vec![Cmd::NewWindow],
            Some(Shortcut::NewSshWindow) => vec![Cmd::NewSshWindow],
            Some(Shortcut::NewSshSession) => vec![Cmd::NewSshSession],
            Some(Shortcut::CloseWindow) => vec![Cmd::CloseWindow],
            Some(Shortcut::NewSession) => vec![Cmd::SpawnSession],
            None => {
                let app_cursor = self.screen.vt().cursor_key_app_mode();
                let modify_other_keys = self.screen.vt().modify_other_keys();
                let kitty_flags = self.screen.vt().kitty_keyboard_flags();
                match encode::encode(
                    key,
                    mods,
                    app_cursor,
                    modify_other_keys,
                    kitty_flags,
                    kind,
                    alts,
                ) {
                    // Typing returns to the live bottom, then sends the keystroke.
                    Some(bytes) => {
                        let mut cmds = self.snap_to_bottom();
                        cmds.extend(self.send(bytes));
                        cmds
                    }
                    None => Vec::new(),
                }
            }
        }
    }

    fn text(&mut self, s: &str) -> Vec<Cmd> {
        // Committed text ends any composition.
        self.preedit.clear();
        if s.is_empty() {
            Vec::new()
        } else {
            let mut cmds = self.snap_to_bottom();
            cmds.extend(self.send(s.as_bytes().to_vec()));
            cmds
        }
    }

    /// Store the in-progress IME composition. Non-empty suppresses raw key input
    /// (see [`key`](Self::key)); empty ends composition.
    fn set_preedit(&mut self, text: String) -> Vec<Cmd> {
        let changed = self.preedit != text;
        self.preedit = text;
        if changed {
            vec![Cmd::Redraw]
        } else {
            Vec::new()
        }
    }

    /// Paste reply from the shell: wrap with bracketed-paste markers if enabled.
    fn paste(&mut self, text: Option<String>) -> Vec<Cmd> {
        match text {
            Some(s) => {
                let bytes = bracket_paste(s.as_bytes(), self.screen.vt().bracketed_paste());
                let mut cmds = self.snap_to_bottom();
                cmds.extend(self.send(bytes));
                cmds
            }
            None => Vec::new(),
        }
    }

    fn copy(&self) -> Vec<Cmd> {
        match self.selection {
            Some(sel) => {
                let text = selection_text(&self.screen, sel);
                if text.is_empty() {
                    Vec::new()
                } else {
                    vec![Cmd::WriteClipboard(text)]
                }
            }
            None => Vec::new(),
        }
    }

    fn focus(&mut self, focused: bool) -> Vec<Cmd> {
        self.focused = focused;
        if !focused {
            // Losing focus aborts any IME composition; clear it so we don't get
            // stuck swallowing input should the platform omit `Ime::Disabled`.
            self.preedit.clear();
        }
        if self.screen.vt().focus_report() {
            self.send(if focused {
                b"\x1b[I".to_vec()
            } else {
                b"\x1b[O".to_vec()
            })
        } else {
            Vec::new()
        }
    }

    fn resize(&mut self, w_px: u32, h_px: u32, scale: f32) -> Vec<Cmd> {
        self.size_px = (w_px, h_px);
        // A non-positive scale (never sent by winit) would break the grid math;
        // ignore it and keep the last good value, as the Fleet/Root models do.
        if scale > 0.0 {
            self.scale = scale;
        }
        let m = self.effective_metrics();
        // The grid fills the window *inset by the padding* on each side; the border is
        // left for the terminal background (see [`Self::pad`]).
        let pad = self.pad_px();
        let content_w = (w_px as f32 - 2.0 * pad).max(0.0);
        let content_h = (h_px as f32 - 2.0 * pad).max(0.0);
        let cols = (content_w / m.advance).floor().max(1.0) as u16;
        let rows = (content_h / m.line_height).floor().max(1.0) as u16;
        if (cols, rows) == (self.cols, self.rows) {
            // Grid unchanged, but a scale change still needs a repaint at the new
            // (physical) glyph size.
            return vec![Cmd::Redraw];
        }
        self.cols = cols;
        self.rows = rows;
        self.screen.resize(cols, rows);
        // Reflow invalidates cell coordinates and the history view; drop any
        // stale selection and return to the live bottom.
        self.selection = None;
        self.sel_anchor = None;
        self.scroll_offset = 0;
        vec![
            Cmd::Resize {
                session: self.session.clone(),
                cols,
                rows,
            },
            Cmd::Redraw,
        ]
    }

    fn session_data(&mut self, name: &str, bytes: &[u8], ended: bool) -> Vec<Cmd> {
        if name != self.session {
            return Vec::new();
        }
        let mut cmds = Vec::new();
        if !bytes.is_empty() {
            self.trace.feeds_seen += 1;
            let before = self.screen.vt().lines_scrolled_off();
            let colors_before = self.render_colors();
            let focus_report_before = self.screen.vt().focus_report();
            let placements_before = self.placement_signature();
            // `Screen::feed` reports the viewport rows this feed changed; an empty
            // slice means nothing on screen moved (a mode set, a query that only
            // produced a reply, an incomplete UTF-8 tail). That is the "backing
            // buffer modified since last composition" signal — gate the repaint on
            // it so a no-op feed doesn't drive a present, and carry the dirtied row
            // range into `view`'s per-session `TermDamage` (see [`Self::damage`]).
            let (viewport_changed, dirty) = {
                let d = self.screen.feed(bytes);
                (
                    !d.is_empty(),
                    d.first().zip(d.last()).map(|(&lo, &hi)| (lo, hi)),
                )
            };
            // The window ops a program asked for that the emulator couldn't do
            // itself (iconify, maximize, full-screen). Carried out *before* the grid
            // is reconciled below, so a maximize's new grid rides the same path a
            // DECCOLM does — and so a `CSI 18 t` in this very burst already answers
            // with it.
            cmds.extend(self.apply_window_ops());
            // A program can resize the emulator from within the feed (DECCOLM
            // 80↔132) — the one change that comes bottom-up, from the child rather
            // than from the window. Follow it: adopt the new grid, ask the window to
            // resize to fit, and tell the child its new size (xterm SIGWINCHes after
            // DECCOLM too). The reply context below is built from the adopted grid,
            // so a `CSI 18 t` in the same burst answers with the size the program
            // just asked for. The window may refuse or clamp the request; its next
            // `UiEvent::Resize` re-grids us to what it actually is, which is the
            // fallback. Force a full repaint — the reflow invalidates every cell
            // coordinate — and drop the (now meaningless) selection and scroll.
            if self.screen.dimensions() != (self.cols, self.rows) {
                (self.cols, self.rows) = self.screen.dimensions();
                self.selection = None;
                self.sel_anchor = None;
                self.scroll_offset = 0;
                self.view_slid = true;
                let (w_px, h_px) = self.window_px_for_grid(self.cols, self.rows);
                // Hold the size we asked the window for, so a `CSI 14 t` in this
                // burst reports the pixels the program just asked for rather than
                // the ones it had. The window's next `UiEvent::Resize` — what it
                // actually granted — overwrites this, as it does the grid.
                self.size_px = (w_px, h_px);
                cmds.push(Cmd::ResizeWindow { w_px, h_px });
                cmds.push(Cmd::Resize {
                    session: self.session.clone(),
                    cols: self.cols,
                    rows: self.rows,
                });
            }
            // Keep a scrolled-up view pinned to its content: advance the offset by
            // the GROSS lines that scrolled off the top this feed. That count
            // survives scrollback trimming (unlike the net scrollback_len delta,
            // which reads zero once the cap is hit), clamped to retained history.
            // At the bottom (offset 0) we just follow the live output.
            if self.scroll_offset > 0 {
                let pushed = self.screen.vt().lines_scrolled_off().saturating_sub(before);
                let desired = self.scroll_offset + pushed;
                let capped = desired.min(self.max_scroll());
                // At the scrollback cap the offset can't grow to keep the view pinned to
                // its content, so the evicted lines slide the whole visible window while
                // the offset stays put. The per-row feed hint names only the live rows
                // that changed, not the slid history above them — force a full repaint the
                // way a scroll does (see [`Self::damage`]).
                if capped < desired {
                    self.view_slid = true;
                }
                self.scroll_offset = capped;
            }
            let display_size = self.display_grid();
            let screen = &self.screen;
            let mode_state = |m: u16| screen.vt().dec_mode_state(m);
            let ansi_mode_state = |m: u16| screen.vt().ansi_mode_state(m);
            let checksum = |t, l, b, r| screen.vt().rect_checksum(t, l, b, r);
            let palette = |i: u8| screen.vt().palette_color(i);
            let special = |t| screen.vt().special_color(t);
            let (lm, rm) = screen.vt().left_right_margins();
            let (tm, bm) = screen.vt().top_bottom_margins();
            let ctx = ReplyCtx {
                cursor: screen.cursor_report(),
                size: screen.dimensions(),
                display_size,
                iconified: self.iconified,
                size_px: self.size_px,
                display_px: self
                    .display_px
                    .unwrap_or(ghost_vt::query::NOMINAL_DISPLAY_PX),
                cell_px: {
                    let m = self.effective_metrics();
                    (m.advance.ceil() as u32, m.line_height.ceil() as u32)
                },
                title: screen.title(),
                icon_title: screen.icon_title(),
                kitty_flags: screen.kitty_keyboard_flags(),
                cursor_style: ghost_vt::query::decscusr_digit(screen.vt().cursor().shape),
                left_right_margins: (lm as u16, rm as u16),
                top_bottom_margins: (tm as u16, bm as u16),
                sgr_report: screen.vt().sgr_report(),
                decsca: screen.vt().decsca_report(),
                conformance_level: screen.vt().conformance_level(),
                ansi_mode_state: &ansi_mode_state,
                colors: screen.effective_colors(self.theme),
                palette: &palette,
                special: &special,
                mode_state: &mode_state,
                checksum: &checksum,
            };
            let replies = query_replies(&mut self.scanner, bytes, &ctx);
            if !replies.is_empty() {
                cmds.push(Cmd::SendInput {
                    session: self.session.clone(),
                    bytes: replies,
                });
            }
            // Report the current focus state when an app first subscribes to
            // focus events (DEC ?1004 rising edge). xterm reports only on the
            // next *change*, so an app that enables focus reporting while the
            // window already holds focus never learns it does — Claude Code's
            // prompt does exactly this and then swallows input until a focus
            // change happens to arrive. Emit the current state on enable so a
            // newly-subscribed app is never left guessing (a deliberate,
            // documented divergence from xterm).
            if self.screen.vt().focus_report() && !focus_report_before {
                cmds.push(Cmd::SendInput {
                    session: self.session.clone(),
                    bytes: if self.focused {
                        b"\x1b[I".to_vec()
                    } else {
                        b"\x1b[O".to_vec()
                    },
                });
            }
            // kitty-graphics acknowledgements are stateful, so (unlike the scanner
            // queries) they come from the emulator. The detached host stays out of
            // the way while a client is attached, so we — the attached frontend —
            // send them back to the child ourselves.
            let graphics_replies = self.screen.take_graphics_responses();
            if !graphics_replies.is_empty() {
                cmds.push(Cmd::SendInput {
                    session: self.session.clone(),
                    bytes: graphics_replies,
                });
            }
            // OSC 52: apply the app's clipboard writes (copy-over-ssh, tmux
            // set-clipboard). The emulator already decoded, size-capped, and
            // refused the read form; route each write to its selection.
            for (target, text) in self.screen.take_clipboard_writes() {
                cmds.push(match target {
                    ClipboardSelection::Clipboard => Cmd::WriteClipboard(text),
                    ClipboardSelection::Primary => Cmd::WritePrimary(text),
                });
            }
            // At the live bottom, new output replaces the viewport, so a
            // viewport-relative selection no longer maps — drop it (unless a drag
            // is live). While scrolled back, stay-put keeps the same content on
            // screen, so the selection stays valid and is preserved. Dropping a
            // visible highlight is itself a repaint even if no row's text changed.
            let had_selection = self.selection.is_some();
            if self.held.is_none() && self.scroll_offset == 0 {
                self.selection = None;
                self.sel_anchor = None;
            }
            let selection_dropped = had_selection && self.selection.is_none();
            // Reflect an OSC 0/2 window-title change to the shell, once per change.
            // Emit the fallback (session name when the app cleared its title) so an
            // empty OSC 2 never blanks the titlebar — consistent with switch paths.
            if self.screen.title() != self.last_title.as_str() {
                self.last_title = self.screen.title().to_string();
                cmds.push(Cmd::SetTitle(self.title()));
            }
            // A new image may be a direct placement the row-damage hint doesn't
            // cover, so upload count is its own repaint trigger.
            let images_before = cmds.len();
            self.upload_new_images(&mut cmds);
            let images_added = cmds.len() > images_before;
            // Fold this feed's dirty rows into the pending damage. A new image gets the
            // whole viewport (its footprint may sit outside the row hint); a dropped
            // selection needs no range — `view`'s structural check reports it as `All`.
            if let Some((lo, hi)) = dirty {
                self.accumulate_dirty(lo, hi);
            }
            if images_added {
                self.accumulate_dirty(0, self.rows.saturating_sub(1) as usize);
            }
            // A direct placement changed WITHOUT a new upload — a delete (`a=d`), a move,
            // or a re-place of an already-uploaded image — alters the drawn frame but
            // writes no cell and sends no blob, so nothing above dirtied its rows. Damage
            // the whole viewport (a placement's footprint sits outside any row hint), the
            // same as a fresh upload does.
            let placements_changed = placements_before != self.placement_signature();
            if placements_changed && !images_added {
                self.accumulate_dirty(0, self.rows.saturating_sub(1) as usize);
            }
            // App-set dynamic colors (OSC 10/11/12) dirty no rows, but they
            // recolor everything; `damage` reports All via the `Presented`
            // snapshot — this only makes sure a repaint is actually requested.
            let colors_changed = colors_before != self.render_colors();
            // The cursor is part of the drawn frame, but moving it writes no cell, so a
            // bare CUP/CUF (how full-screen apps like an editor or Claude Code reposition
            // between keystrokes) dirties no content row. `Screen::feed` tracks the drawn
            // cursor and reports the move as its own damage (the row it left + entered);
            // fold that in — but only at the live bottom, since scrolled into history the
            // cursor isn't drawn (and a scroll is already a full repaint). `Screen`
            // advances its own baseline every feed, so there's nothing to do when scrolled.
            let cursor_redrawn = if self.scroll_offset == 0 {
                let cursor = self.screen.cursor_damage();
                if let Some(r) = cursor.left {
                    self.accumulate_dirty(r, r);
                }
                if let Some(r) = cursor.entered {
                    self.accumulate_dirty(r, r);
                }
                cursor.repaint
            } else {
                false
            };
            let want_redraw = viewport_changed
                || selection_dropped
                || images_added
                || placements_changed
                || colors_changed
                || cursor_redrawn
                || self.view_slid;
            if want_redraw {
                self.trace.visible_feeds += 1;
            }
            // Synchronized output (DEC 2026): between set and reset the app is
            // composing one atomic frame, so hold the repaint (damage keeps
            // accumulating above) and schedule a release tick as the backstop.
            // Any tick releases the hold — an early animation tick just means
            // one mid-frame paint, the status quo without the mode.
            let sync = self.screen.vt().synchronized_output();
            if sync && want_redraw {
                // Re-arm the release backstop on EVERY swallowed feed, not just
                // the rising edge. A hold can be latched into a warm background
                // mirror whose one rising-edge tick is then spent on the wrong
                // (foreground) model; a later non-animated promotion carries the
                // still-held mirror to the foreground with nothing pending, and
                // it freezes until output happens to pause outside a frame.
                // Re-scheduling every feed closes that race for free: the shell
                // coalesces deadlines and a tick to an unlatched model is a
                // no-op. `sync_holds` still counts the rising edge only.
                if !self.sync_held {
                    self.sync_held = true;
                    self.trace.sync_holds += 1;
                }
                // The repaint is deferred by the open hold — counted so the render
                // trace can see feeds piling up behind a latched 2026 frame.
                self.trace.feeds_while_held += 1;
                cmds.push(Cmd::ScheduleTick {
                    after_ms: SYNC_RELEASE_MS,
                });
            }
            if !sync {
                let released = std::mem::take(&mut self.sync_held);
                if released {
                    self.trace.sync_released_by_reset += 1;
                }
                if want_redraw || released {
                    cmds.push(Cmd::Redraw);
                    self.trace.redraws_emitted += 1;
                }
            }
        }
        if ended {
            self.ended = true;
            cmds.push(Cmd::Redraw);
        }
        cmds
    }

    /// Emit a [`Cmd::UploadImage`] for every image newly displayed — whether by a
    /// direct placement or by a Unicode-placeholder cell in the viewport — whose
    /// pixels we have not yet sent the renderer. The blob travels out of band (not
    /// through the `Scene`) and once per image.
    /// The on-screen direct graphics placements as cheap identity tuples, so a feed that
    /// deletes (`a=d`), moves, or re-places an already-uploaded image — none of which
    /// writes a cell or uploads a blob — can be detected as a frame change. Placeholder
    /// cells aren't placements; they change a cell and are covered by the row-damage hint.
    fn placement_signature(&self) -> Vec<PlacementSig> {
        self.screen
            .vt()
            .graphics_placements()
            .map(|p| {
                (
                    p.image_id,
                    p.placement_id,
                    p.row,
                    p.col,
                    p.cols,
                    p.rows,
                    p.z,
                )
            })
            .collect()
    }

    fn upload_new_images(&mut self, cmds: &mut Vec<Cmd>) {
        // Every image id referenced on screen: direct placements first...
        let mut referenced: Vec<u32> = Vec::new();
        for p in self.screen.vt().graphics_placements() {
            if !referenced.contains(&p.image_id) {
                referenced.push(p.image_id);
            }
        }
        // ...then Unicode-placeholder cells, which reference an image by id without a
        // direct placement. Normally scan just the live viewport; but when a new image
        // was just stored, also scan the retained scrollback, since the image may belong
        // to a placeholder that already scrolled out of view (otherwise it would never
        // upload and would render blank when scrolled back to).
        let count = self.screen.vt().graphics_image_count();
        let scan_all = count != self.last_image_count;
        self.last_image_count = count;
        let placeholder_ids: Vec<u32> = if scan_all {
            self.screen
                .vt()
                .lines()
                .flat_map(|line| line.cells())
                .filter_map(|cell| cell.placeholder_image_id())
                .collect()
        } else {
            self.screen
                .vt()
                .view()
                .flat_map(|line| line.cells())
                .filter_map(|cell| cell.placeholder_image_id())
                .collect()
        };
        for id in placeholder_ids {
            if !referenced.contains(&id) {
                referenced.push(id);
            }
        }
        // Upload any whose current store generation differs from what we last sent — a
        // first transmit (no entry yet) or a re-transmit that replaced the pixels under
        // an existing id. Keying on the id alone would leave a re-transmit stale.
        for id in referenced {
            let Some(image) = self.screen.vt().graphics_image(id) else {
                continue;
            };
            if self.uploaded_images.get(&id) == Some(&image.generation) {
                continue;
            }
            let generation = image.generation;
            cmds.push(Cmd::UploadImage {
                id,
                width: image.width,
                height: image.height,
                rgba: image.pixels.clone(),
            });
            self.uploaded_images.insert(id, generation);
        }
    }

    // ---- pointer / selection state machine ----

    fn mouse_active(&self) -> bool {
        self.screen.vt().mouse_protocol() != MouseProtocol::Off
    }

    /// Whether a gesture should be forwarded to the child rather than driving
    /// local selection. Shift forces local selection even when the child grabs
    /// the mouse, as xterm does.
    fn report_to_app(&self, mods: Mods) -> bool {
        self.mouse_active() && !mods.shift
    }

    /// 1-based `(col, row)` cell under a pointer position. Pointer coordinates are
    /// physical pixels in window space; subtract the padding so the grid origin sits
    /// at the inset content corner, then divide by the physical (scaled) metrics.
    fn point_to_cell(&self, pos: PointPx) -> (u16, u16) {
        let m = self.effective_metrics();
        let pad = f64::from(self.pad_px());
        let col = ((pos.x - pad) / f64::from(m.advance)).floor().max(0.0) as u16 + 1;
        let row = ((pos.y - pad) / f64::from(m.line_height)).floor().max(0.0) as u16 + 1;
        (col, row)
    }

    /// The safe-to-open hyperlink under a pointer position — its interned id
    /// and URI — honoring the scrollback offset. `None` on unlinked cells and
    /// disallowed schemes.
    fn link_at(&self, pos: PointPx) -> Option<(u16, String)> {
        let (col1, row1) = self.point_to_cell(pos);
        let row = usize::from(row1.saturating_sub(1)).min((self.rows as usize).saturating_sub(1));
        let col = usize::from(col1.saturating_sub(1)).min((self.cols as usize).saturating_sub(1));
        let vt = self.screen.vt();
        let line = vt.view_at(self.scroll_offset).nth(row)?;
        let id = line.cells().get(col)?.pen().link_id()?;
        let uri = vt.hyperlink(id)?;
        // Only schemes whose handlers are safe to invoke on a click; anything
        // else (javascript:, custom app schemes, …) stays inert.
        let scheme = uri.split_once(':')?.0.to_ascii_lowercase();
        matches!(
            scheme.as_str(),
            "http" | "https" | "file" | "mailto" | "ftp"
        )
        .then(|| (id, uri.to_string()))
    }

    /// The safe-to-open hyperlink URI under a pointer position (see
    /// [`link_at`](Self::link_at)).
    fn link_under(&self, pos: PointPx) -> Option<String> {
        self.link_at(pos).map(|(_, uri)| uri)
    }

    /// Track the Ctrl/Cmd-hover over hyperlinks: on a change, repaint (the
    /// underline overlay) and switch the pointer between hand and default.
    fn update_hover(&mut self, pos: PointPx, mods: Mods) -> Vec<Cmd> {
        let hovered = ((mods.ctrl || mods.sup) && self.held.is_none())
            .then(|| self.link_at(pos).map(|(id, _)| id))
            .flatten();
        if hovered == self.hovered_link {
            return Vec::new();
        }
        self.hovered_link = hovered;
        let icon = if hovered.is_some() {
            PointerIcon::Pointer
        } else {
            PointerIcon::Default
        };
        vec![Cmd::PointerIcon(icon), Cmd::Redraw]
    }

    /// 0-based `(row, col)` cell under the pointer, clamped to the grid.
    fn pointer_cell0(&self) -> (usize, usize) {
        let (col1, row1) = self.cursor_cell.unwrap_or((1, 1));
        let row0 = usize::from(row1.saturating_sub(1));
        let col0 = usize::from(col1.saturating_sub(1));
        (
            row0.min((self.rows as usize).saturating_sub(1)),
            col0.min((self.cols as usize).saturating_sub(1)),
        )
    }

    /// Extend the drag selection from the latched `anchor` extent (absolute
    /// line space) to the viewport cell under the pointer, at the latched
    /// granularity: by cell, or growing to cover the whole word / line that
    /// contains the active cell (degrading to the cell itself when blank).
    /// The result is absolute, so it survives the viewport scrolling mid-drag.
    fn extend_selection(&self, anchor: Selection, active: (usize, usize)) -> Selection {
        let ext = match self.sel_mode {
            SelectMode::Char => None,
            SelectMode::Word => self.word_at(active.0, active.1),
            SelectMode::Line => self.line_at(active.0),
        }
        .map(|s| self.abs_sel(s));
        let cell = self.abs_cell(active);
        let b = ext.unwrap_or_else(|| Selection::new(cell, cell));
        Selection {
            start: anchor.start.min(b.start),
            end: anchor.end.max(b.end),
        }
    }

    /// After the viewport moved mid-drag, re-extend the selection to the cell
    /// still under the pointer — which now covers different content.
    fn re_extend(&mut self) {
        if self.held == Some(mouse::Button::Left)
            && !self.gesture_report
            && let Some(anchor) = self.sel_anchor
        {
            self.selection = Some(self.extend_selection(anchor, self.pointer_cell0()));
        }
    }

    /// Track selection autoscroll from the pointer's vertical overshoot past
    /// the grid: hovering above the top edge scrolls into history, below the
    /// bottom back toward live, faster the further past the edge. Arms the
    /// tick loop on the off-to-on transition; [`Self::autoscroll_tick`] keeps
    /// it alive while armed.
    fn update_autoscroll(&mut self, pos: PointPx) -> Vec<Cmd> {
        let m = self.effective_metrics();
        let pad = f64::from(self.pad_px());
        let lh = f64::from(m.line_height);
        let bottom = pad + lh * f64::from(self.rows);
        let overshoot = if pos.y < pad {
            pad - pos.y
        } else if pos.y > bottom {
            bottom - pos.y
        } else {
            0.0
        };
        let speed = if overshoot == 0.0 {
            0
        } else {
            (1 + (overshoot.abs() / lh) as i64).min(AUTOSCROLL_MAX) * overshoot.signum() as i64
        };
        let was = std::mem::replace(&mut self.autoscroll, speed);
        if was == 0 && speed != 0 {
            vec![Cmd::ScheduleTick {
                after_ms: AUTOSCROLL_MS,
            }]
        } else {
            Vec::new()
        }
    }

    /// One armed autoscroll step: scroll the viewport, re-extend the selection
    /// to the pointer (whose cell clamps to the hovered edge row), and keep
    /// the tick loop alive. Disarms when the drag has ended or the viewport
    /// hit its limit; a later edge-hover motion re-arms it.
    fn autoscroll_tick(&mut self) -> Vec<Cmd> {
        if self.autoscroll == 0 {
            return Vec::new();
        }
        let dragging = self.held == Some(mouse::Button::Left) && self.sel_anchor.is_some();
        let target = (self.scroll_offset as i64 + self.autoscroll).max(0) as usize;
        if !dragging || !self.set_scroll(target) {
            self.autoscroll = 0;
            return Vec::new();
        }
        self.re_extend();
        vec![
            Cmd::Redraw,
            Cmd::ScheduleTick {
                after_ms: AUTOSCROLL_MS,
            },
        ]
    }

    /// The word under viewport cell `(row, col)` — a maximal run of word cells —
    /// as an inclusive selection, or `None` on a blank/non-word cell. Reads the
    /// scrolled-back view by cell (not `screen.text()`, whose char indices don't
    /// line up with cell columns once a wide character is present).
    fn word_at(&self, row: usize, col: usize) -> Option<Selection> {
        let window: Vec<&Line> = self.screen.vt().view_at(self.scroll_offset).collect();
        let cells = window.get(row)?.cells();
        // A word cell is one holding a word character, or the (zero-width) tail
        // of a wide character, which continues whatever head precedes it.
        let word = |i: usize| {
            cells
                .get(i)
                .is_some_and(|c| is_word_char(c.char()) || c.width() == 0)
        };
        if !word(col) {
            return None;
        }
        let mut start = col;
        while start > 0 && word(start - 1) {
            start -= 1;
        }
        let mut end = col;
        while end + 1 < cells.len() && word(end + 1) {
            end += 1;
        }
        Some(Selection::new((row, start), (row, end)))
    }

    /// The line at viewport `row`: column 0 through its last non-blank cell (the
    /// whole row when blank), as an inclusive selection.
    fn line_at(&self, row: usize) -> Option<Selection> {
        let window: Vec<&Line> = self.screen.vt().view_at(self.scroll_offset).collect();
        let cells = window.get(row)?.cells();
        let last = cells.iter().rposition(|c| !c.is_default()).unwrap_or(0);
        Some(Selection::new((row, 0), (row, last)))
    }

    fn mouse_report(
        &self,
        kind: mouse::Kind,
        button: Option<mouse::Button>,
        held: bool,
        cell: (u16, u16),
        mods: Mods,
    ) -> Vec<Cmd> {
        let proto = self.screen.vt().mouse_protocol();
        let sgr = self.screen.vt().mouse_sgr();
        match mouse::encode(proto, sgr, kind, button, held, cell.0, cell.1, mods) {
            Some(bytes) => self.send(bytes),
            None => Vec::new(),
        }
    }

    fn pointer(
        &mut self,
        phase: PointerPhase,
        button: Option<PointerButton>,
        pos: PointPx,
        mods: Mods,
        wheel_dy: f64,
        clicks: u8,
    ) -> Vec<Cmd> {
        match phase {
            PointerPhase::Motion => {
                let mut cmds = self.update_hover(pos, mods);
                // Edge-hover autoscroll is tracked BEFORE the same-cell
                // early-return: past the edge the clamped cell stops changing,
                // but the overshoot (and so the scroll speed) still does.
                if self.held == Some(mouse::Button::Left)
                    && !self.gesture_report
                    && self.sel_anchor.is_some()
                {
                    cmds.extend(self.update_autoscroll(pos));
                }
                let cell = self.point_to_cell(pos);
                if self.cursor_cell == Some(cell) {
                    return cmds;
                }
                self.cursor_cell = Some(cell);
                cmds.extend(if let Some(b) = self.held {
                    if self.gesture_report {
                        self.mouse_report(mouse::Kind::Motion, Some(b), true, cell, mods)
                    } else if b == mouse::Button::Left
                        && let Some(anchor) = self.sel_anchor
                    {
                        self.selection = Some(self.extend_selection(anchor, self.pointer_cell0()));
                        vec![Cmd::Redraw]
                    } else {
                        Vec::new()
                    }
                } else if self.report_to_app(mods) {
                    self.mouse_report(mouse::Kind::Motion, None, false, cell, mods)
                } else {
                    Vec::new()
                });
                cmds
            }
            PointerPhase::Press => {
                let Some(b) = button.map(map_button) else {
                    return Vec::new();
                };
                // Ctrl+click (or Cmd+click) on an OSC 8 hyperlink opens it,
                // consuming the press. Checked before mouse forwarding: apps
                // like Claude Code hold any-motion tracking, so a forwarded
                // Ctrl+click would otherwise make their links unreachable.
                if b == mouse::Button::Left
                    && (mods.ctrl || mods.sup)
                    && let Some(url) = self.link_under(pos)
                {
                    self.gesture_report = false;
                    return vec![Cmd::OpenUrl(url)];
                }
                self.held = Some(b);
                self.gesture_report = self.report_to_app(mods);
                if self.gesture_report {
                    let cell = self.cursor_cell.unwrap_or((1, 1));
                    let mut cmds = self.mouse_report(mouse::Kind::Press, Some(b), true, cell, mods);
                    // A forwarded left-click still dismisses a stale local highlight.
                    if b == mouse::Button::Left && self.selection.take().is_some() {
                        cmds.push(Cmd::Redraw);
                    }
                    cmds
                } else if b == mouse::Button::Left {
                    if clicks >= 2 && self.cursor_cell.is_some() {
                        // Double-click selects the word, triple-click the line, and
                        // latches that granularity so a drag extends by it. The
                        // anchor extent is lifted to absolute line space so it
                        // stays pinned to its content if the drag scrolls.
                        let (row, col) = self.pointer_cell0();
                        self.sel_mode = if clicks == 2 {
                            SelectMode::Word
                        } else {
                            SelectMode::Line
                        };
                        let ext = if clicks == 2 {
                            self.word_at(row, col)
                        } else {
                            self.line_at(row)
                        }
                        .map(|sel| self.abs_sel(sel));
                        let cell = self.abs_cell((row, col));
                        self.sel_anchor = Some(ext.unwrap_or_else(|| Selection::new(cell, cell)));
                        self.selection = ext;
                    } else {
                        // Begin a by-cell drag selection (anchor once the pointer
                        // is known).
                        self.sel_mode = SelectMode::Char;
                        self.sel_anchor = self.cursor_cell.map(|_| {
                            let cell = self.abs_cell(self.pointer_cell0());
                            Selection::new(cell, cell)
                        });
                        self.selection = None;
                    }
                    vec![Cmd::Redraw]
                } else if b == mouse::Button::Middle {
                    // Middle-click pastes the primary selection (the reply comes
                    // back as `ClipboardText`, like a normal paste).
                    vec![Cmd::ReadPrimary]
                } else {
                    Vec::new()
                }
            }
            PointerPhase::Release => {
                let mut cmds = match button.map(map_button) {
                    Some(b) if self.gesture_report => {
                        let cell = self.cursor_cell.unwrap_or((1, 1));
                        self.mouse_report(mouse::Kind::Release, Some(b), false, cell, mods)
                    }
                    _ => Vec::new(),
                };
                self.held = None;
                self.autoscroll = 0;
                // A finalized local selection becomes the primary selection, so a
                // middle-click elsewhere pastes it (X11/Wayland convention).
                if let Some(sel) = self.selection {
                    let text = selection_text(&self.screen, sel);
                    if !text.is_empty() {
                        cmds.push(Cmd::WritePrimary(text));
                    }
                }
                cmds
            }
            PointerPhase::Wheel => {
                if wheel_dy == 0.0 {
                    return Vec::new();
                }
                if self.report_to_app(mods) {
                    // The child grabbed the mouse: report the wheel as a button.
                    let b = if wheel_dy > 0.0 {
                        mouse::Button::WheelUp
                    } else {
                        mouse::Button::WheelDown
                    };
                    let cell = self.cursor_cell.unwrap_or((1, 1));
                    self.mouse_report(mouse::Kind::Press, Some(b), self.held.is_some(), cell, mods)
                } else {
                    // Scroll local scrollback (up = into history). Mid-drag
                    // this is fine — the selection lives in absolute line
                    // space — it just re-extends to the content now under the
                    // pointer.
                    let delta = if wheel_dy > 0.0 {
                        SCROLL_LINES
                    } else {
                        -SCROLL_LINES
                    };
                    let cmds = self.scroll_by(delta);
                    self.re_extend();
                    cmds
                }
            }
        }
    }
}

fn map_button(b: PointerButton) -> mouse::Button {
    match b {
        PointerButton::Left => mouse::Button::Left,
        PointerButton::Middle => mouse::Button::Middle,
        PointerButton::Right => mouse::Button::Right,
    }
}

/// Whether `c` is part of a word for double-click selection: alphanumerics and
/// underscore (so identifiers select whole, stopping at spaces and punctuation).
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

// ---- pure protocol helpers (shared with the shell) ----

/// Scan child output for terminal queries and build the reply bytes from the
/// given context (cursor, size, kitty flags, theme colors, mode state). Pure.
pub fn query_replies(scanner: &mut QueryScanner, output: &[u8], ctx: &ReplyCtx) -> Vec<u8> {
    let mut out = Vec::new();
    for query in scanner.scan(output) {
        out.extend_from_slice(&query.reply(ctx));
    }
    out
}

/// Wrap pasted bytes in bracketed-paste markers when DEC mode 2004 is on.
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

/// Extract the text covered by `sel` from `screen`'s viewport scrolled
/// one line per row joined by newlines. Selection rows are ABSOLUTE line
/// indices (the monotonic lines-ever space drags are anchored in), so the text
/// spans retained scrollback and viewport alike — including ranges wider than
/// one window — regardless of where the view sits now. Rows already evicted
/// from the bounded scrollback are skipped. Wide-cell tail placeholders are
/// dropped; the terminating row keeps its trailing spaces (selected content)
/// while earlier rows are trimmed.
pub fn selection_text(screen: &Screen, sel: Selection) -> String {
    let (cols, _rows) = screen.dimensions();
    let cols = cols as usize;
    let vt = screen.vt();
    // The oldest retained line's absolute index; anything older is gone.
    let first_abs = vt.lines_scrolled_off() - vt.scrollback_len();
    let start_row = sel.start.0.max(first_abs);
    if sel.end.0 < start_row {
        return String::new();
    }
    let window: Vec<&Line> = vt
        .lines()
        .skip(start_row - first_abs)
        .take(sel.end.0 - start_row + 1)
        .collect();
    let mut lines: Vec<String> = Vec::new();
    for (i, line) in window.iter().enumerate() {
        let row = start_row + i;
        let text = match sel.row_span(row, cols) {
            Some((c0, c1)) => {
                let len = line.len();
                line.cells()[c0.min(len)..c1.min(len)]
                    .iter()
                    .filter(|cell| cell.width() != 0)
                    .map(|cell| cell.char())
                    .collect::<String>()
            }
            None => String::new(),
        };
        let text = if row == sel.end.0 {
            text
        } else {
            text.trim_end().to_string()
        };
        lines.push(text);
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::NamedKey;

    const METRICS: CellMetrics = CellMetrics {
        advance: 9.0,
        line_height: 18.0,
    };

    fn model() -> TerminalModel {
        TerminalModel::new("alpha".to_string(), 80, 24, METRICS)
    }

    /// The reply a program reading `bytes` gets back, as text (the `Cmd::SendInput`
    /// the model answers a query with).
    fn reply_to(m: &mut TerminalModel, bytes: &[u8]) -> String {
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: bytes.to_vec(),
            ended: false,
        });
        cmds.iter()
            .filter_map(|c| match c {
                Cmd::SendInput { bytes, .. } => Some(String::from_utf8_lossy(bytes).into_owned()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_program_maximizing_the_window_gets_the_display_sized_grid_it_reads_back() {
        let mut m = model();
        // A 1800x900 display at the test's 9x18 cell: 200 x 50 characters.
        m.update(UiEvent::DisplaySize {
            w_px: 1800,
            h_px: 900,
        });
        assert_eq!(reply_to(&mut m, b"\x1b[19t"), "\x1b[9;50;200t");
        assert_eq!(reply_to(&mut m, b"\x1b[18t"), "\x1b[8;24;80t");

        // Maximizing takes the grid to the display, asks the window to follow, and
        // tells the child its new size — and `CSI 18 t` now answers with it.
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"\x1b[9;1t".to_vec(),
            ended: false,
        });
        assert!(cmds.contains(&Cmd::SetMaximized(true)));
        assert!(cmds.iter().any(|c| matches!(c, Cmd::ResizeWindow { .. })));
        assert!(cmds.contains(&Cmd::Resize {
            session: "alpha".to_string(),
            cols: 200,
            rows: 50,
        }));
        assert_eq!(reply_to(&mut m, b"\x1b[18t"), "\x1b[8;50;200t");

        // Restoring puts back the grid it had, not a guess.
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"\x1b[9;0t".to_vec(),
            ended: false,
        });
        assert!(cmds.contains(&Cmd::SetMaximized(false)));
        assert_eq!(reply_to(&mut m, b"\x1b[18t"), "\x1b[8;24;80t");
    }

    #[test]
    fn a_single_axis_maximize_grows_only_that_axis() {
        let mut m = model();
        m.update(UiEvent::DisplaySize {
            w_px: 1800,
            h_px: 900,
        });
        // Horizontally: the display's width, the rows it had.
        feed(&mut m, b"\x1b[9;3t");
        assert_eq!(reply_to(&mut m, b"\x1b[18t"), "\x1b[8;24;200t");
        feed(&mut m, b"\x1b[9;0t");
        // Vertically: the columns it had, the display's height.
        feed(&mut m, b"\x1b[9;2t");
        assert_eq!(reply_to(&mut m, b"\x1b[18t"), "\x1b[8;50;80t");
    }

    #[test]
    fn window_ops_in_one_burst_compose_instead_of_clobbering_each_other() {
        let mut m = model();
        m.update(UiEvent::DisplaySize {
            w_px: 1800,
            h_px: 900,
        });
        // Grow one axis and then the other in a *single* write. Each op has to see
        // the grid the one before it left, or the second silently undoes the first.
        feed(&mut m, b"\x1b[9;2t\x1b[9;3t");
        assert_eq!(reply_to(&mut m, b"\x1b[18t"), "\x1b[8;50;200t");
    }

    #[test]
    fn maximize_and_fullscreen_each_restore_their_own_grid() {
        let mut m = model();
        m.update(UiEvent::DisplaySize {
            w_px: 1800,
            h_px: 900,
        });
        // A leave-fullscreen while not full-screen is a no-op in xterm — and a
        // program that sends one defensively at startup is common. It must not
        // walk off with the grid a maximize saved to come back to.
        feed(&mut m, b"\x1b[9;1t"); // maximize: 200x50, remembering 80x24
        feed(&mut m, b"\x1b[10;0t"); // leave a full-screen we were never in
        assert_eq!(
            reply_to(&mut m, b"\x1b[18t"),
            "\x1b[8;50;200t",
            "the no-op left the maximized grid alone"
        );
        feed(&mut m, b"\x1b[9;0t"); // and the maximize still has 80x24 to restore
        assert_eq!(reply_to(&mut m, b"\x1b[18t"), "\x1b[8;24;80t");

        // Full-screen *over* a maximize comes back to the maximized grid, not to
        // the grid from before the maximize: the two states nest.
        feed(&mut m, b"\x1b[8;30;90t"); // a plain 90x30
        feed(&mut m, b"\x1b[9;1t"); // maximize: 200x50, remembering 90x30
        feed(&mut m, b"\x1b[10;1t"); // full-screen
        feed(&mut m, b"\x1b[10;0t"); // leave it
        assert_eq!(
            reply_to(&mut m, b"\x1b[18t"),
            "\x1b[8;50;200t",
            "leaving full-screen lands back on the maximize"
        );
        feed(&mut m, b"\x1b[9;0t");
        assert_eq!(
            reply_to(&mut m, b"\x1b[18t"),
            "\x1b[8;30;90t",
            "and the maximize still restores what it saved"
        );
    }

    #[test]
    fn a_program_reads_back_the_iconified_and_fullscreen_state_it_asked_for() {
        let mut m = model();
        m.update(UiEvent::DisplaySize {
            w_px: 1800,
            h_px: 900,
        });
        assert_eq!(
            reply_to(&mut m, b"\x1b[11t"),
            "\x1b[1t",
            "open to begin with"
        );

        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"\x1b[2t".to_vec(),
            ended: false,
        });
        assert!(cmds.contains(&Cmd::SetIconified(true)));
        assert_eq!(reply_to(&mut m, b"\x1b[11t"), "\x1b[2t", "iconified");
        feed(&mut m, b"\x1b[1t");
        assert_eq!(reply_to(&mut m, b"\x1b[11t"), "\x1b[1t", "and open again");

        // Full-screen fills the display and toggles back to the grid it had.
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"\x1b[10;1t".to_vec(),
            ended: false,
        });
        assert!(cmds.contains(&Cmd::SetFullscreen(true)));
        assert_eq!(reply_to(&mut m, b"\x1b[18t"), "\x1b[8;50;200t");
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"\x1b[10;2t".to_vec(),
            ended: false,
        });
        assert!(cmds.contains(&Cmd::SetFullscreen(false)), "2 toggles");
        assert_eq!(reply_to(&mut m, b"\x1b[18t"), "\x1b[8;24;80t");
    }

    #[test]
    fn a_pixel_resize_from_hostile_output_cannot_blow_the_grid_up() {
        let mut m = model();
        m.update(UiEvent::DisplaySize {
            w_px: 1800,
            h_px: 900,
        });
        // 65535 x 65535 px at a 9x18 cell is a 7281 x 3640 grid — 26 million cells
        // the session host would try to allocate. It is bounded to a grid a
        // terminal could actually have.
        feed(&mut m, b"\x1b[4;65535;65535t");
        let (cols, rows) = (m.cols, m.rows);
        assert!(
            cols as usize <= ghost_term::MAX_PROGRAM_COLS
                && rows as usize <= ghost_term::MAX_PROGRAM_ROWS,
            "hostile output re-gridded us to {cols}x{rows}"
        );
    }

    #[test]
    fn decslpp_sets_the_page_height_and_the_window_follows() {
        let mut m = model();
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"\x1b[30t".to_vec(),
            ended: false,
        });
        assert!(cmds.contains(&Cmd::Resize {
            session: "alpha".to_string(),
            cols: 80,
            rows: 30,
        }));
        assert_eq!(reply_to(&mut m, b"\x1b[18t"), "\x1b[8;30;80t");
    }

    fn key(m: &mut TerminalModel, k: Key, mods: Mods) -> Vec<Cmd> {
        m.update(UiEvent::Key {
            key: k,
            mods,
            kind: KeyEventKind::Press,
            alts: None,
        })
    }

    fn key_kind(m: &mut TerminalModel, k: Key, mods: Mods, kind: KeyEventKind) -> Vec<Cmd> {
        m.update(UiEvent::Key {
            key: k,
            mods,
            kind,
            alts: None,
        })
    }

    fn feed(m: &mut TerminalModel, bytes: &[u8]) {
        m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: bytes.to_vec(),
            ended: false,
        });
    }

    fn ptr(phase: PointerPhase, button: Option<PointerButton>, x: f64, y: f64) -> UiEvent {
        UiEvent::Pointer {
            phase,
            button,
            pos: PointPx { x, y },
            mods: Mods::NONE,
            wheel_dy: 0.0,
            clicks: 1,
        }
    }

    /// A left-button press carrying a click count (for word/line selection).
    fn press_n(x: f64, y: f64, clicks: u8) -> UiEvent {
        UiEvent::Pointer {
            phase: PointerPhase::Press,
            button: Some(PointerButton::Left),
            pos: PointPx { x, y },
            mods: Mods::NONE,
            wheel_dy: 0.0,
            clicks,
        }
    }

    fn sent(session: &str, bytes: &[u8]) -> Cmd {
        Cmd::SendInput {
            session: session.to_string(),
            bytes: bytes.to_vec(),
        }
    }

    /// A wheel event with vertical delta `dy` (positive = scroll up / into
    /// history), mouse reporting off.
    fn wheel(dy: f64) -> UiEvent {
        UiEvent::Pointer {
            phase: PointerPhase::Wheel,
            button: None,
            pos: PointPx { x: 1.0, y: 1.0 },
            mods: Mods::NONE,
            wheel_dy: dy,
            clicks: 1,
        }
    }

    /// Feed `n` lines "L0".."L{n-1}" (no trailing newline).
    fn feed_lines(m: &mut TerminalModel, n: usize) {
        let mut s = String::new();
        for i in 0..n {
            if i > 0 {
                s.push_str("\r\n");
            }
            s.push_str(&format!("L{i}"));
        }
        feed(m, s.as_bytes());
    }

    /// The text of the first run of the top rendered row (what the user sees at
    /// the top of the terminal, honoring any scrollback offset).
    fn top_row_text(m: &TerminalModel) -> String {
        let scene = m.view();
        match scene.terminals().next().unwrap() {
            SceneItem::Terminal { frame, .. } => frame
                .rows_layout
                .first()
                .and_then(|r| r.runs.first())
                .map(|run| run.text.clone())
                .unwrap_or_default(),
            _ => unreachable!(),
        }
    }

    #[test]
    fn key_routes_to_send_input_for_focused_session() {
        let mut m = model();
        assert_eq!(
            key(&mut m, Key::Char("a".into()), Mods::NONE),
            vec![sent("alpha", b"a")]
        );
        assert_eq!(
            m.update(UiEvent::Key {
                key: Key::Char("x".into()),
                mods: Mods::NONE,
                kind: KeyEventKind::Release,
                alts: None
            }),
            vec![]
        );
    }

    #[test]
    fn paste_shortcut_requests_clipboard_then_pastes_reply() {
        let mut m = model();
        assert_eq!(
            key(&mut m, Key::Char("v".into()), Mods::CTRL | Mods::SHIFT),
            vec![Cmd::ReadClipboard]
        );
        // Reply with no bracketed-paste mode: raw bytes.
        assert_eq!(
            m.update(UiEvent::ClipboardText(Some("hi".into()))),
            vec![sent("alpha", b"hi")]
        );
        // Enable DEC 2004; the next paste is wrapped.
        feed(&mut m, b"\x1b[?2004h");
        assert_eq!(
            m.update(UiEvent::ClipboardText(Some("hi".into()))),
            vec![sent("alpha", b"\x1b[200~hi\x1b[201~")]
        );
    }

    #[test]
    fn copy_writes_the_selection_text() {
        let mut m = model();
        feed(&mut m, b"hello world");
        // Move, press, drag to select "hello" (cells 0..=4).
        m.update(ptr(PointerPhase::Motion, None, 1.0, 1.0));
        m.update(ptr(
            PointerPhase::Press,
            Some(PointerButton::Left),
            1.0,
            1.0,
        ));
        m.update(ptr(
            PointerPhase::Motion,
            Some(PointerButton::Left),
            40.0,
            1.0,
        ));
        assert_eq!(
            key(&mut m, Key::Char("c".into()), Mods::CTRL | Mods::SHIFT),
            vec![Cmd::WriteClipboard("hello".to_string())]
        );
    }

    #[test]
    fn releasing_a_drag_selection_sets_the_primary_selection() {
        let mut m = model();
        feed(&mut m, b"hello world");
        m.update(ptr(PointerPhase::Motion, None, 1.0, 1.0));
        m.update(ptr(
            PointerPhase::Press,
            Some(PointerButton::Left),
            1.0,
            1.0,
        ));
        m.update(ptr(
            PointerPhase::Motion,
            Some(PointerButton::Left),
            40.0,
            1.0,
        ));
        let cmds = m.update(ptr(
            PointerPhase::Release,
            Some(PointerButton::Left),
            40.0,
            1.0,
        ));
        assert!(
            cmds.contains(&Cmd::WritePrimary("hello".to_string())),
            "release should publish the selection to primary: {cmds:?}"
        );
    }

    #[test]
    fn a_plain_click_release_publishes_no_primary() {
        let mut m = model();
        feed(&mut m, b"hi");
        m.update(ptr(PointerPhase::Motion, None, 1.0, 1.0));
        m.update(ptr(
            PointerPhase::Press,
            Some(PointerButton::Left),
            1.0,
            1.0,
        ));
        let cmds = m.update(ptr(
            PointerPhase::Release,
            Some(PointerButton::Left),
            1.0,
            1.0,
        ));
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::WritePrimary(_))),
            "a click with no selection must not touch primary: {cmds:?}"
        );
    }

    #[test]
    fn middle_click_pastes_the_primary_selection() {
        let mut m = model();
        feed(&mut m, b"text");
        m.update(ptr(PointerPhase::Motion, None, 1.0, 1.0));
        assert_eq!(
            m.update(ptr(
                PointerPhase::Press,
                Some(PointerButton::Middle),
                1.0,
                1.0
            )),
            vec![Cmd::ReadPrimary],
            "middle-click requests a primary-selection paste"
        );
    }

    #[test]
    fn key_input_is_suppressed_while_composing() {
        let mut m = model();
        // Composition starts: a non-empty preedit arrives (dead key / CJK).
        m.update(UiEvent::Preedit("´".into()));
        // The physical keystroke driving composition must NOT reach the child.
        assert_eq!(key(&mut m, Key::Char("e".into()), Mods::NONE), vec![]);
        // Committing sends the composed text and ends composition.
        assert_eq!(
            m.update(UiEvent::Text("é".into())),
            vec![sent("alpha", "é".as_bytes())]
        );
        // After commit, normal keys flow again.
        assert_eq!(
            key(&mut m, Key::Char("x".into()), Mods::NONE),
            vec![sent("alpha", b"x")]
        );
    }

    #[test]
    fn focus_loss_clears_stuck_composition() {
        let mut m = model();
        m.update(UiEvent::Preedit("ねこ".into()));
        // Composing: the keystroke is swallowed.
        assert_eq!(key(&mut m, Key::Char("a".into()), Mods::NONE), vec![]);
        // The window loses focus mid-composition without an Ime::Disabled — the
        // composition must still be aborted so input isn't swallowed forever.
        m.update(UiEvent::Focus(false));
        assert_eq!(
            key(&mut m, Key::Char("a".into()), Mods::NONE),
            vec![sent("alpha", b"a")]
        );
    }

    #[test]
    fn cancelled_preedit_restores_key_input() {
        let mut m = model();
        m.update(UiEvent::Preedit("か".into()));
        assert_eq!(key(&mut m, Key::Char("a".into()), Mods::NONE), vec![]);
        // Empty preedit = composition cancelled; raw keys flow again.
        m.update(UiEvent::Preedit(String::new()));
        assert_eq!(
            key(&mut m, Key::Char("a".into()), Mods::NONE),
            vec![sent("alpha", b"a")]
        );
    }

    #[test]
    fn ime_cursor_area_tracks_cursor_and_scale() {
        let mut m = model();
        m.update(UiEvent::Resize {
            w_px: 720,
            h_px: 432,
            scale: 1.0,
        }); // 80x24, base 9x18
        // Fresh cursor at the top-left cell maps to the origin.
        let a = m.ime_cursor_area().unwrap();
        assert_eq!((a.x, a.y, a.w, a.h), (0.0, 0.0, 9.0, 18.0));
        // Output advances the cursor: "abc" -> 0-based col 3 -> x = 3 * 9.
        feed(&mut m, b"abc");
        let a = m.ime_cursor_area().unwrap();
        assert_eq!((a.x, a.y), (27.0, 0.0));
        // At 2x device scale the area scales with the cell.
        m.update(UiEvent::Resize {
            w_px: 720,
            h_px: 432,
            scale: 2.0,
        });
        let a = m.ime_cursor_area().unwrap();
        assert_eq!((a.w, a.h), (18.0, 36.0));
    }

    #[test]
    fn deccolm_asks_the_shell_to_resize_the_window_to_the_new_grid() {
        let mut m = model();
        m.update(UiEvent::Resize {
            w_px: 720,
            h_px: 432,
            scale: 1.0,
        });
        // An app enables 80↔132 switching and requests 132-column mode: the grid
        // follows the program, and the window is asked to grow to fit it (80 * 9 =
        // 720 px wide becomes 132 * 9 = 1188; the height is unchanged).
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"\x1b[?40h\x1b[?3h".to_vec(),
            ended: false,
        });
        assert_eq!(m.screen.dimensions(), (132, 24));
        assert_eq!((m.cols, m.rows), (132, 24));
        assert!(
            cmds.contains(&Cmd::ResizeWindow {
                w_px: 1188,
                h_px: 432,
            }),
            "DECCOLM asks the window to fit the new grid: {cmds:?}"
        );
        // The child learns its new width too (xterm SIGWINCHes after DECCOLM).
        assert!(
            cmds.contains(&Cmd::Resize {
                session: "alpha".to_string(),
                cols: 132,
                rows: 24,
            }),
            "DECCOLM re-sizes the pty: {cmds:?}"
        );

        // Back to 80 columns: the window is asked to shrink again.
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"\x1b[?3l".to_vec(),
            ended: false,
        });
        assert_eq!(m.screen.dimensions(), (80, 24));
        assert!(cmds.contains(&Cmd::ResizeWindow {
            w_px: 720,
            h_px: 432,
        }));
    }

    #[test]
    fn a_denied_deccolm_window_resize_is_reconciled_by_the_next_window_size() {
        let mut m = model();
        m.update(UiEvent::Resize {
            w_px: 720,
            h_px: 432,
            scale: 1.0,
        });
        feed(&mut m, b"\x1b[?40h\x1b[?3h");
        assert_eq!(m.cols, 132);
        // The window manager granted nothing (tiled, fullscreen, clamped): whatever
        // size the window does report next wins, and the grid snaps to it.
        let cmds = m.update(UiEvent::Resize {
            w_px: 720,
            h_px: 432,
            scale: 1.0,
        });
        assert_eq!(m.screen.dimensions(), (80, 24));
        assert_eq!((m.cols, m.rows), (80, 24));
        assert!(cmds.contains(&Cmd::Resize {
            session: "alpha".to_string(),
            cols: 80,
            rows: 24,
        }));
    }

    #[test]
    fn deccolm_without_allow_80_to_132_neither_regrids_nor_resizes_the_window() {
        let mut m = model();
        m.update(UiEvent::Resize {
            w_px: 720,
            h_px: 432,
            scale: 1.0,
        });
        // DECCOLM is gated on ?40 (off by default): the sequence is inert, so the
        // window must not be jostled.
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"\x1b[?3h".to_vec(),
            ended: false,
        });
        assert_eq!(m.screen.dimensions(), (80, 24));
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::ResizeWindow { .. })));
    }

    #[test]
    fn ime_cursor_area_none_while_scrolled_back() {
        let mut m = model();
        m.update(UiEvent::Resize {
            w_px: 720,
            h_px: 432,
            scale: 1.0,
        });
        for _ in 0..40 {
            feed(&mut m, b"line\r\n");
        }
        key(&mut m, Key::Named(NamedKey::Home), Mods::SHIFT); // into history
        assert!(m.ime_cursor_area().is_none());
    }

    #[test]
    fn double_click_selects_the_word() {
        let mut m = model();
        feed(&mut m, b"foo bar_baz qux");
        // Hover over 'r' in "bar_baz" (0-based col 6), then double-click.
        m.update(ptr(PointerPhase::Motion, None, 55.0, 1.0));
        m.update(press_n(55.0, 1.0, 2));
        // "bar_baz" spans cols 4..=10 (underscore is a word char).
        assert_eq!(m.selection(), Some(Selection::new((0, 4), (0, 10))));
    }

    #[test]
    fn triple_click_selects_the_line() {
        let mut m = model();
        feed(&mut m, b"hello world");
        m.update(ptr(PointerPhase::Motion, None, 9.0, 1.0)); // col 1, row 0
        m.update(press_n(9.0, 1.0, 3));
        // Whole line: col 0 through the last non-blank ('d' at col 10).
        assert_eq!(m.selection(), Some(Selection::new((0, 0), (0, 10))));
    }

    #[test]
    fn double_click_word_after_a_wide_char_uses_cell_columns() {
        let mut m = model();
        // 世 occupies cells 0-1, 'a'=2, space=3, 'b'=4, 'c'=5. A char-index view
        // would mis-map; cell-indexed selection must land on "bc" at cols 4..=5.
        feed(&mut m, "世a bc".as_bytes());
        m.update(ptr(PointerPhase::Motion, None, 9.0 * 4.0 + 1.0, 1.0)); // col 4 = 'b'
        m.update(press_n(9.0 * 4.0 + 1.0, 1.0, 2));
        assert_eq!(m.selection(), Some(Selection::new((0, 4), (0, 5))));
    }

    #[test]
    fn double_click_on_blank_selects_nothing() {
        let mut m = model();
        feed(&mut m, b"hi");
        m.update(ptr(PointerPhase::Motion, None, 9.0 * 40.0, 1.0)); // col 40, blank
        m.update(press_n(9.0 * 40.0, 1.0, 2));
        assert_eq!(m.selection(), None);
    }

    #[test]
    fn single_click_still_starts_a_drag_not_a_word() {
        let mut m = model();
        feed(&mut m, b"foo bar");
        m.update(ptr(PointerPhase::Motion, None, 9.0, 1.0));
        m.update(press_n(9.0, 1.0, 1));
        // A plain click anchors a drag and shows no selection yet.
        assert_eq!(m.selection(), None);
    }

    #[test]
    fn double_click_drag_extends_by_whole_words() {
        let mut m = model();
        feed(&mut m, b"foo bar baz"); // foo=0..=2, bar=4..=6, baz=8..=10
        m.update(ptr(PointerPhase::Motion, None, 1.0, 1.0)); // col 0, in "foo"
        m.update(press_n(1.0, 1.0, 2));
        assert_eq!(
            m.selection(),
            Some(Selection::new((0, 0), (0, 2))),
            "the double-click selects just the word"
        );
        // Drag into the MIDDLE of "baz" (col 9): the selection grows to the whole
        // word, not merely to the cell under the pointer.
        m.update(ptr(
            PointerPhase::Motion,
            Some(PointerButton::Left),
            9.0 * 9.0 + 1.0,
            1.0,
        ));
        assert_eq!(
            m.selection(),
            Some(Selection {
                start: (0, 0),
                end: (0, 10)
            }),
            "the drag extends by whole words, through the end of 'baz'"
        );
    }

    #[test]
    fn triple_click_drag_extends_by_whole_lines() {
        let mut m = model();
        feed(&mut m, b"line one\r\nline two"); // row 0 + row 1, each 0..=7
        m.update(ptr(PointerPhase::Motion, None, 1.0, 1.0)); // row 0
        m.update(press_n(1.0, 1.0, 3));
        assert_eq!(
            m.selection(),
            Some(Selection::new((0, 0), (0, 7))),
            "the triple-click selects just the first line"
        );
        // Drag down onto row 1 (line height 18): the selection covers both whole
        // lines regardless of the column under the pointer.
        m.update(ptr(
            PointerPhase::Motion,
            Some(PointerButton::Left),
            30.0,
            20.0,
        ));
        assert_eq!(
            m.selection(),
            Some(Selection {
                start: (0, 0),
                end: (1, 7)
            }),
            "the drag extends by whole lines, through the end of row 1"
        );
    }

    #[test]
    fn bare_ctrl_c_sends_sigint_not_copy() {
        let mut m = model();
        // Ctrl+C (no Shift) is NOT the copy shortcut — it must reach the child
        // as 0x03 so programs still interrupt. (Copy is Ctrl+Shift+C / Cmd+C.)
        assert_eq!(
            key(&mut m, Key::Char("c".into()), Mods::CTRL),
            vec![sent("alpha", b"\x03")]
        );
    }

    #[test]
    fn modify_other_keys_is_negotiated_through_the_terminal() {
        let mut m = model();
        // Without negotiation, Ctrl+I is the legacy Tab byte.
        assert_eq!(
            key(&mut m, Key::Char("i".into()), Mods::CTRL),
            vec![sent("alpha", b"\x09")]
        );
        // The app enables modifyOtherKeys level 2 (XTMODKEYS) on its PTY...
        feed(&mut m, b"\x1b[>4;2m");
        // ...so Ctrl+I now reports as CSI 27;5;105~, distinct from a real Tab.
        assert_eq!(
            key(&mut m, Key::Char("i".into()), Mods::CTRL),
            vec![sent("alpha", b"\x1b[27;5;105~")]
        );
        // Reset (CSI > 4 m) returns to the legacy encoding.
        feed(&mut m, b"\x1b[>4m");
        assert_eq!(
            key(&mut m, Key::Char("i".into()), Mods::CTRL),
            vec![sent("alpha", b"\x09")]
        );
    }

    #[test]
    fn kitty_keyboard_disambiguates_keys_after_negotiation() {
        let mut m = model();
        // Legacy: Ctrl+I collapses to the Tab byte, Esc is a bare ESC.
        assert_eq!(
            key(&mut m, Key::Char("i".into()), Mods::CTRL),
            vec![sent("alpha", b"\x09")]
        );
        // The app pushes kitty disambiguate (flag 1) on its PTY...
        feed(&mut m, b"\x1b[>1u");
        // ...so Ctrl+I is now a distinct CSI u report, and Esc disambiguates.
        assert_eq!(
            key(&mut m, Key::Char("i".into()), Mods::CTRL),
            vec![sent("alpha", b"\x1b[105;5u")]
        );
        assert_eq!(
            key(&mut m, Key::Named(NamedKey::Escape), Mods::NONE),
            vec![sent("alpha", b"\x1b[27u")]
        );
        // Popping the stack restores the legacy encoding.
        feed(&mut m, b"\x1b[<u");
        assert_eq!(
            key(&mut m, Key::Char("i".into()), Mods::CTRL),
            vec![sent("alpha", b"\x09")]
        );
    }

    #[test]
    fn kitty_keyboard_query_is_answered_with_the_negotiated_flags() {
        let mut m = model();
        // The app enables kitty disambiguate (flag 1) on its PTY, then queries.
        feed(&mut m, b"\x1b[>1u");
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"\x1b[?u".to_vec(),
            ended: false,
        });
        assert!(
            cmds.contains(&sent("alpha", b"\x1b[?1u")),
            "the kitty query must report the negotiated flags, got {cmds:?}"
        );
    }

    #[test]
    fn kitty_keyboard_reports_repeat_and_release_only_under_the_event_types_flag() {
        let mut m = model();
        // Under flag 1 alone, a repeat re-presses and a release sends nothing.
        feed(&mut m, b"\x1b[>1u");
        assert_eq!(
            key_kind(
                &mut m,
                Key::Char("i".into()),
                Mods::CTRL,
                KeyEventKind::Repeat
            ),
            vec![sent("alpha", b"\x1b[105;5u")]
        );
        assert_eq!(
            key_kind(
                &mut m,
                Key::Char("i".into()),
                Mods::CTRL,
                KeyEventKind::Release
            ),
            vec![]
        );
        // The app upgrades to disambiguate + report-event-types (flags 1|2)...
        feed(&mut m, b"\x1b[>3u");
        // ...so repeats carry :2 and releases carry :3.
        assert_eq!(
            key_kind(
                &mut m,
                Key::Char("i".into()),
                Mods::CTRL,
                KeyEventKind::Repeat
            ),
            vec![sent("alpha", b"\x1b[105;5:2u")]
        );
        assert_eq!(
            key_kind(
                &mut m,
                Key::Char("i".into()),
                Mods::CTRL,
                KeyEventKind::Release
            ),
            vec![sent("alpha", b"\x1b[105;5:3u")]
        );
    }

    #[test]
    fn quit_shortcut_is_cmd_q_or_ctrl_q_while_ctrl_shift_q_sends_xon() {
        let mut m = model();
        assert_eq!(
            key(&mut m, Key::Char("q".into()), Mods::SUPER),
            vec![Cmd::Quit],
            "Cmd+Q quits"
        );
        // Bare Ctrl+Q quits, mirroring Cmd+Q on every platform.
        assert_eq!(
            key(&mut m, Key::Char("q".into()), Mods::CTRL),
            vec![Cmd::Quit],
            "bare Ctrl+Q quits"
        );
        // Ctrl+Shift+Q is the escape hatch that still sends XON (0x11) to the child.
        assert_eq!(
            key(&mut m, Key::Char("q".into()), Mods::CTRL | Mods::SHIFT),
            vec![sent("alpha", b"\x11")],
            "Ctrl+Shift+Q sends XON, not quit"
        );
    }

    #[test]
    fn new_session_shortcut_is_cmd_t_on_macos_and_alt_t_elsewhere() {
        let mut m = model();
        // The platform's new-session chord spawns a fresh session.
        let chord = if cfg!(target_os = "macos") {
            Mods::SUPER
        } else {
            Mods::ALT
        };
        assert_eq!(
            key(&mut m, Key::Char("t".into()), chord),
            vec![Cmd::SpawnSession],
            "the platform new-session chord spawns a session"
        );
        // Bare 't' is ordinary terminal input.
        assert_eq!(
            key(&mut m, Key::Char("t".into()), Mods::NONE),
            vec![sent("alpha", b"t")],
            "bare t is terminal input"
        );
        // The other platform's modifier must NOT spawn — on Linux the former
        // Ctrl+Shift+T no longer has a binding; on macOS Option+T types a glyph.
        let other = if cfg!(target_os = "macos") {
            Mods::ALT
        } else {
            Mods::CTRL | Mods::SHIFT
        };
        assert_ne!(
            key(&mut m, Key::Char("t".into()), other),
            vec![Cmd::SpawnSession],
            "the non-platform chord does not spawn a session"
        );
    }

    #[test]
    fn cmd_copy_paste_use_super_while_bare_ctrl_stays_control() {
        // Copy/paste are the stricter Cmd (macOS) / Ctrl+Shift combo so a bare
        // Ctrl+C/Ctrl+V still reaches the child as a control byte. The native menu
        // re-injects exactly these chords, so this pins the mapping it relies on.
        let mut m = model();
        assert_eq!(
            key(&mut m, Key::Char("v".into()), Mods::SUPER),
            vec![Cmd::ReadClipboard],
            "Cmd+V pastes"
        );
        assert_eq!(
            key(&mut m, Key::Char("v".into()), Mods::CTRL),
            vec![sent("alpha", b"\x16")],
            "bare Ctrl+V is a literal control byte, not paste"
        );

        // Cmd+C copies the current selection; bare Ctrl+C stays 0x03 (SIGINT).
        let mut m = model();
        feed(&mut m, b"hello world");
        m.update(ptr(PointerPhase::Motion, None, 1.0, 1.0));
        m.update(ptr(
            PointerPhase::Press,
            Some(PointerButton::Left),
            1.0,
            1.0,
        ));
        m.update(ptr(
            PointerPhase::Motion,
            Some(PointerButton::Left),
            40.0,
            1.0,
        ));
        assert_eq!(
            key(&mut m, Key::Char("c".into()), Mods::SUPER),
            vec![Cmd::WriteClipboard("hello".to_string())],
            "Cmd+C copies the selection"
        );
        let mut m = model();
        assert_eq!(
            key(&mut m, Key::Char("c".into()), Mods::CTRL),
            vec![sent("alpha", b"\x03")],
            "bare Ctrl+C is SIGINT, not copy"
        );
    }

    #[test]
    fn cmd_n_and_w_are_window_management_while_bare_ctrl_stays_control() {
        let mut m = model();
        assert_eq!(
            key(&mut m, Key::Char("n".into()), Mods::SUPER),
            vec![Cmd::NewWindow],
            "Cmd+N opens a new window"
        );
        assert_eq!(
            key(&mut m, Key::Char("w".into()), Mods::SUPER),
            vec![Cmd::CloseWindow],
            "Cmd+W closes the window"
        );
        // Bare Ctrl+N / Ctrl+W stay ordinary terminal input (0x0e / 0x17), never
        // window management — only the Cmd (or Ctrl+Shift) chord manages windows.
        assert_eq!(
            key(&mut m, Key::Char("n".into()), Mods::CTRL),
            vec![sent("alpha", b"\x0e")],
            "bare Ctrl+N is terminal input"
        );
        assert_eq!(
            key(&mut m, Key::Char("w".into()), Mods::CTRL),
            vec![sent("alpha", b"\x17")],
            "bare Ctrl+W is terminal input"
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn alt_c_v_n_are_copy_paste_new_window_on_linux() {
        // On Linux, Alt+C/V/N are frontend shortcuts (copy / paste / new-window) — a
        // terminal convention that keeps Ctrl free for the shell — resolved here rather
        // than encoded as Meta+<key> to the child. macOS keeps Alt = Option/Meta (it
        // uses Cmd for these), so the behaviour and this test are gated off there.
        let mut m = model();
        assert_eq!(
            key(&mut m, Key::Char("v".into()), Mods::ALT),
            vec![Cmd::ReadClipboard],
            "Alt+V pastes"
        );
        assert_eq!(
            key(&mut m, Key::Char("n".into()), Mods::ALT),
            vec![Cmd::NewWindow],
            "Alt+N opens a new window"
        );

        // Alt+C copies the current selection.
        let mut m = model();
        feed(&mut m, b"hello world");
        m.update(ptr(PointerPhase::Motion, None, 1.0, 1.0));
        m.update(ptr(
            PointerPhase::Press,
            Some(PointerButton::Left),
            1.0,
            1.0,
        ));
        m.update(ptr(
            PointerPhase::Motion,
            Some(PointerButton::Left),
            40.0,
            1.0,
        ));
        assert_eq!(
            key(&mut m, Key::Char("c".into()), Mods::ALT),
            vec![Cmd::WriteClipboard("hello".to_string())],
            "Alt+C copies the selection"
        );

        // Only c/v/n are grabbed: another Alt+letter (e.g. Alt+B word motion) still
        // reaches the child as Meta input, never a frontend shortcut.
        let mut m = model();
        let out = key(&mut m, Key::Char("b".into()), Mods::ALT);
        assert!(
            !out.iter().any(|c| matches!(
                c,
                Cmd::NewWindow | Cmd::ReadClipboard | Cmd::WriteClipboard(_)
            )),
            "Alt+B must stay Meta input, not a shortcut: {out:?}"
        );
        assert!(
            !out.is_empty(),
            "Alt+B should still send Meta bytes to the child"
        );
    }

    #[test]
    fn output_clears_selection_when_not_dragging() {
        let mut m = model();
        feed(&mut m, b"hello world");
        m.update(ptr(PointerPhase::Motion, None, 1.0, 1.0));
        m.update(ptr(
            PointerPhase::Press,
            Some(PointerButton::Left),
            1.0,
            1.0,
        ));
        m.update(ptr(
            PointerPhase::Motion,
            Some(PointerButton::Left),
            40.0,
            1.0,
        ));
        // Releasing ends the drag; subsequent output invalidates the selection.
        m.update(ptr(
            PointerPhase::Release,
            Some(PointerButton::Left),
            40.0,
            1.0,
        ));
        assert!(m.selection().is_some());
        feed(&mut m, b"\r\nmore output");
        assert!(m.selection().is_none());
    }

    #[test]
    fn resize_clears_selection_and_emits_resize() {
        let mut m = model();
        feed(&mut m, b"hello");
        m.update(ptr(PointerPhase::Motion, None, 1.0, 1.0));
        m.update(ptr(
            PointerPhase::Press,
            Some(PointerButton::Left),
            1.0,
            1.0,
        ));
        m.update(ptr(
            PointerPhase::Motion,
            Some(PointerButton::Left),
            40.0,
            1.0,
        ));
        assert!(m.selection().is_some());
        let cmds = m.update(UiEvent::Resize {
            w_px: 40 * 9,
            h_px: 10 * 18,
            scale: 1.0,
        });
        assert_eq!(
            cmds,
            vec![
                Cmd::Resize {
                    session: "alpha".to_string(),
                    cols: 40,
                    rows: 10
                },
                Cmd::Redraw
            ]
        );
        assert!(m.selection().is_none());
    }

    #[test]
    fn resize_applies_device_scale_to_metrics_and_grid() {
        let mut m = model(); // base metrics advance 9, line_height 18
        // A 2x HiDPI surface 720x432 physical px: cells are 18x36 px, so the grid
        // is half of the 1x 80x24 — 40 cols x 12 rows — and the rendered frame
        // carries the scaled (physical) metrics so glyphs rasterize crisp.
        let cmds = m.update(UiEvent::Resize {
            w_px: 720,
            h_px: 432,
            scale: 2.0,
        });
        assert!(cmds.contains(&Cmd::Resize {
            session: "alpha".to_string(),
            cols: 40,
            rows: 12
        }));
        match m.view().terminals().next().unwrap() {
            SceneItem::Terminal { frame, .. } => {
                assert_eq!(frame.metrics.advance, 18.0);
                assert_eq!(frame.metrics.line_height, 36.0);
            }
            _ => unreachable!(),
        }
    }

    fn frame_advance(m: &TerminalModel) -> f32 {
        match m.view().terminals().next().unwrap() {
            SceneItem::Terminal { frame, .. } => frame.metrics.advance,
            _ => unreachable!(),
        }
    }

    #[test]
    fn zoom_in_grows_cells_and_reset_restores() {
        let mut m = model();
        m.update(UiEvent::Resize {
            w_px: 720,
            h_px: 432,
            scale: 1.0,
        }); // 80x24 at base 9x18
        assert_eq!(frame_advance(&m), 9.0);
        // Ctrl + '+' : one 0.1 zoom step -> advance 9 * 1.1 = 9.9, so the grid shrinks.
        let cmds = key(&mut m, Key::Char("+".into()), Mods::CTRL);
        assert!(
            (frame_advance(&m) - 9.9).abs() < 1e-4,
            "one zoom-in step is 1.1x"
        );
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::Resize { cols, rows, .. } if *cols < 80 && *rows < 24)),
            "zoom re-grids the child"
        );
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::SendInput { .. })),
            "the zoom key is not forwarded to the child"
        );
        // Ctrl + '0' : reset to 1.0 -> back to 9.0 and the full 80x24 grid.
        let cmds = key(&mut m, Key::Char("0".into()), Mods::CTRL);
        assert_eq!(frame_advance(&m), 9.0);
        assert!(
            cmds.iter()
                .any(|c| matches!(c, Cmd::Resize { cols, rows, .. } if *cols == 80 && *rows == 24))
        );
    }

    #[test]
    fn set_zoom_applies_and_clamps_an_absolute_zoom() {
        let mut m = model();
        m.update(UiEvent::Resize {
            w_px: 720,
            h_px: 432,
            scale: 1.0,
        }); // 80x24 at base 9x18
        m.update(UiEvent::SetZoom(2.0));
        assert_eq!(frame_advance(&m), 18.0, "absolute 2x zoom doubles the cell");
        // Out-of-range zoom is clamped to the model's bounds (max 3.0 -> 27px).
        m.update(UiEvent::SetZoom(9.0));
        assert_eq!(frame_advance(&m), 27.0, "clamped at ZOOM_MAX");
    }

    #[test]
    fn zoom_clamps_and_steps_on_clean_tenths() {
        let mut m = model();
        m.update(UiEvent::Resize {
            w_px: 720,
            h_px: 432,
            scale: 1.0,
        });
        // Zoom out past the floor: clamps at 0.5x (advance 4.5).
        for _ in 0..20 {
            key(&mut m, Key::Char("-".into()), Mods::CTRL);
        }
        assert!(
            (frame_advance(&m) - 4.5).abs() < 1e-4,
            "clamped at ZOOM_MIN 0.5"
        );
        // Zoom in past the ceiling: clamps at 3.0x (advance 27.0).
        for _ in 0..40 {
            key(&mut m, Key::Char("=".into()), Mods::CTRL); // '=' is also zoom-in
        }
        assert!(
            (frame_advance(&m) - 27.0).abs() < 1e-4,
            "clamped at ZOOM_MAX 3.0"
        );
    }

    #[test]
    fn resize_ignores_non_positive_scale_and_keeps_last_good() {
        let mut m = model();
        m.update(UiEvent::Resize {
            w_px: 720,
            h_px: 432,
            scale: 2.0,
        });
        // A bogus scale (winit never sends one) must not corrupt the grid: keep 2x.
        let cmds = m.update(UiEvent::Resize {
            w_px: 720,
            h_px: 432,
            scale: 0.0,
        });
        // Grid unchanged at 2x (40x12), so no Resize — only a Redraw.
        assert_eq!(cmds, vec![Cmd::Redraw]);
        match m.view().terminals().next().unwrap() {
            SceneItem::Terminal { frame, .. } => {
                assert_eq!(frame.metrics.advance, 18.0, "scale held at 2x, not reset");
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn focus_reports_only_when_enabled() {
        let mut m = model();
        assert_eq!(m.update(UiEvent::Focus(true)), vec![]);
        feed(&mut m, b"\x1b[?1004h");
        assert_eq!(
            m.update(UiEvent::Focus(true)),
            vec![sent("alpha", b"\x1b[I")]
        );
        assert_eq!(
            m.update(UiEvent::Focus(false)),
            vec![sent("alpha", b"\x1b[O")]
        );
    }

    #[test]
    fn focus_report_enable_reports_current_state_when_focused() {
        let mut m = model();
        // The window already holds focus when the app subscribes (the common
        // case: an app enables ?1004h after it is already in the foreground).
        m.update(UiEvent::Focus(true));
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"\x1b[?1004h".to_vec(),
            ended: false,
        });
        // Enabling focus reporting reports the current state (focused) at once,
        // so the app doesn't block waiting for a change that never comes.
        assert!(
            cmds.contains(&sent("alpha", b"\x1b[I")),
            "enable-while-focused should report ESC[I, got {cmds:?}"
        );
    }

    #[test]
    fn focus_report_enable_reports_current_state_when_unfocused() {
        let mut m = model();
        m.update(UiEvent::Focus(false));
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"\x1b[?1004h".to_vec(),
            ended: false,
        });
        assert!(
            cmds.contains(&sent("alpha", b"\x1b[O")),
            "enable-while-unfocused should report ESC[O, got {cmds:?}"
        );
    }

    #[test]
    fn focus_report_reports_only_on_the_enable_edge() {
        let mut m = model();
        m.update(UiEvent::Focus(true));
        feed(&mut m, b"\x1b[?1004h");
        // A second feed that does not touch ?1004 must not re-report focus —
        // only the rising edge of the mode does.
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"x".to_vec(),
            ended: false,
        });
        assert!(
            !cmds.contains(&sent("alpha", b"\x1b[I")),
            "no ?1004 edge, so no focus report, got {cmds:?}"
        );
    }

    #[test]
    fn mouse_reported_when_child_grabs_the_mouse() {
        let mut m = model();
        // Enable X11 mouse + SGR coordinates.
        feed(&mut m, b"\x1b[?1000h\x1b[?1006h");
        m.update(ptr(PointerPhase::Motion, None, 1.0, 1.0)); // cell (1,1)
        let cmds = m.update(ptr(
            PointerPhase::Press,
            Some(PointerButton::Left),
            1.0,
            1.0,
        ));
        assert_eq!(cmds, vec![sent("alpha", b"\x1b[<0;1;1M")]);
    }

    #[test]
    fn session_data_feeds_screen_and_answers_queries() {
        let mut m = model();
        // Device status report query -> the model answers it.
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"hi\x1b[6n".to_vec(),
            ended: false,
        });
        // cursor after "hi" is col 3, row 1 -> CSI 1;3 R, plus a redraw.
        assert_eq!(cmds, vec![sent("alpha", b"\x1b[1;3R"), Cmd::Redraw]);
        assert!(m.screen().text()[0].starts_with("hi"));
    }

    fn is_dimmed(m: &TerminalModel) -> bool {
        match &m.view().layers[0].items[0] {
            SceneItem::Terminal { dim, .. } => *dim,
            other => panic!("expected a terminal item, got {other:?}"),
        }
    }

    #[test]
    fn a_dropped_connection_dims_into_the_reconnecting_hold_until_reattach() {
        let mut m = model();
        feed(&mut m, b"work in progress");
        assert!(!m.reconnecting());
        assert!(!is_dimmed(&m));

        // The transport drops: enter the hold (frozen + dimmed), never `ended`.
        let cmds = m.update(UiEvent::SessionDisconnected {
            name: "alpha".to_string(),
        });
        assert_eq!(cmds, vec![Cmd::Redraw]);
        assert!(m.reconnecting());
        assert!(
            !m.ended(),
            "a dropped connection must never end the session"
        );
        assert!(is_dimmed(&m), "the frozen screen dims while reconnecting");
        assert!(
            m.screen().text()[0].starts_with("work in progress"),
            "the screen is frozen, preserved for the resync"
        );

        // Reattaching clears the hold; the host's resync then repaints normally.
        let cmds = m.update(UiEvent::SessionReattached {
            name: "alpha".to_string(),
        });
        assert_eq!(cmds, vec![Cmd::Redraw]);
        assert!(!m.reconnecting());
        assert!(!is_dimmed(&m));
    }

    #[test]
    fn a_disconnect_for_another_session_is_ignored() {
        let mut m = model();
        let cmds = m.update(UiEvent::SessionDisconnected {
            name: "other".to_string(),
        });
        assert!(cmds.is_empty());
        assert!(!m.reconnecting());
    }

    #[test]
    fn content_feed_redraws() {
        let mut m = model();
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"hello".to_vec(),
            ended: false,
        });
        assert!(cmds.contains(&Cmd::Redraw), "visible output must repaint");
    }

    #[test]
    fn no_op_feed_does_not_redraw() {
        let mut m = model();
        // A lone incomplete UTF-8 lead byte is held back whole for the next feed:
        // nothing is decoded, no cell is written, so `Screen::feed` reports zero
        // dirty rows and there is nothing to repaint.
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: vec![0xf0],
            ended: false,
        });
        assert!(
            !cmds.contains(&Cmd::Redraw),
            "a feed that changes no viewport row must not repaint: {cmds:?}"
        );
    }

    #[test]
    fn trace_counts_visible_versus_invisible_feeds() {
        let mut m = model();
        feed(&mut m, b"hello"); // a visible feed
        feed(&mut m, &[0xf0]); // an incomplete UTF-8 lead: bytes, but nothing visible
        let t = m.trace();
        assert_eq!(t.feeds_seen, 2, "both feeds carried bytes");
        assert_eq!(
            t.visible_feeds, 1,
            "only the printing feed changed the viewport"
        );
        assert_eq!(
            t.redraws_emitted, 1,
            "only the visible feed drove a repaint"
        );
    }

    #[test]
    fn trace_counts_a_latched_synchronized_hold() {
        let mut m = model();
        // A synchronized-output frame opens and content lands but never resets:
        // the hold latches — this is the shape of the freeze bug.
        feed(&mut m, b"\x1b[?2026hhello");
        let t = m.trace();
        assert_eq!(t.feeds_seen, 1);
        assert_eq!(t.visible_feeds, 1);
        assert!(t.sync_held, "the hold is latched");
        assert_eq!(t.sync_holds, 1);
        assert_eq!(
            t.feeds_while_held, 1,
            "the visible feed was swallowed by the hold"
        );
        assert_eq!(t.redraws_emitted, 0, "no repaint is emitted while held");
        // A tick (the backstop, or an animation tick) releases it.
        m.update(UiEvent::Tick { now_ms: 1_000 });
        let t = m.trace();
        assert!(!t.sync_held, "the tick released the hold");
        assert_eq!(t.sync_released_by_tick, 1);
        assert_eq!(
            t.redraws_emitted, 1,
            "the release drove the deferred repaint"
        );
    }

    /// The foreground render-stall latch (the recurring "Claude Code freezes, its
    /// fleet preview stays live" bug): a synchronized-output hold schedules its
    /// release backstop ONLY on the rising edge (`!sync_held` in `session_data`),
    /// but the shell delivers ticks to the window's *foreground* model only — so a
    /// hold latched while this model was a warm background mirror (or a fleet tile)
    /// has its one backstop spent on the wrong recipient. Promoted to the
    /// foreground by a non-animated adopt, the model then swallows every feed whose
    /// pump batch ends inside the still-open 2026 frame — no `Cmd::Redraw`, and
    /// (the bug) no new `Cmd::ScheduleTick` — until the app happens to end a batch
    /// outside the frame: a stale window over a live screen, self-healing only when
    /// output pauses. Every swallowed feed must therefore re-arm the backstop; the
    /// shell coalesces duplicate deadlines and a tick reaching an unlatched model
    /// is a no-op, so re-scheduling is free.
    #[test]
    fn a_feed_swallowed_by_an_open_hold_re_arms_the_release_backstop() {
        let mut m = model();
        // The hold opens mid-frame: the backstop tick is scheduled once.
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"\x1b[?2026hhello".to_vec(),
            ended: false,
        });
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::ScheduleTick { .. })),
            "opening a hold schedules the release backstop"
        );
        // The backstop was spent elsewhere (it fired into the then-foreground
        // model): no tick ever reaches this one. The next batch also ends inside
        // the open frame — swallowed, and it must leave a release pending.
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"world".to_vec(),
            ended: false,
        });
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::ScheduleTick { .. })),
            "a feed swallowed by an already-open hold must re-arm the backstop, \
             or a hold latched while this model was not the tick recipient never \
             releases: {cmds:?}"
        );
    }

    /// A left press at 0-based cell `(row, col)` carrying `mods`.
    fn press_at_cell(row: usize, col: usize, mods: Mods) -> UiEvent {
        UiEvent::Pointer {
            phase: PointerPhase::Press,
            button: Some(PointerButton::Left),
            pos: PointPx {
                x: (col as f64 + 0.5) * f64::from(METRICS.advance),
                y: (row as f64 + 0.5) * f64::from(METRICS.line_height),
            },
            mods,
            wheel_dy: 0.0,
            clicks: 1,
        }
    }

    const CTRL: Mods = Mods {
        shift: false,
        ctrl: true,
        alt: false,
        sup: false,
    };

    #[test]
    fn ctrl_hover_underlines_the_link_and_requests_a_pointer_cursor() {
        let hover = |m: &mut TerminalModel, x: f64, y: f64, mods: Mods| {
            m.update(UiEvent::Pointer {
                phase: PointerPhase::Motion,
                button: None,
                pos: PointPx { x, y },
                mods,
                wheel_dy: 0.0,
                clicks: 1,
            })
        };
        let underlines = |m: &TerminalModel| {
            m.view().layers[0]
                .items
                .iter()
                .filter(|it| matches!(it, SceneItem::Rect { .. }))
                .count()
        };
        let mut m = model();
        feed(
            &mut m,
            b"\x1b]8;;https://example.com\x1b\\LINK\x1b]8;;\x1b\\ plain",
        );
        assert_eq!(underlines(&m), 0);

        // Ctrl-hovering the link underlines its span and asks for a hand cursor.
        let cmds = hover(&mut m, 13.0, 4.0, CTRL); // over the "I" of LINK
        assert!(
            cmds.contains(&Cmd::PointerIcon(PointerIcon::Pointer)),
            "no pointer-cursor request: {cmds:?}"
        );
        assert!(cmds.contains(&Cmd::Redraw));
        assert_eq!(underlines(&m), 1, "one contiguous underline span");

        // Moving off the link restores the cursor and drops the underline.
        let cmds = hover(&mut m, 9.0 * 8.0 + 4.0, 4.0, CTRL); // over "plain"
        assert!(
            cmds.contains(&Cmd::PointerIcon(PointerIcon::Default)),
            "cursor not restored: {cmds:?}"
        );
        assert_eq!(underlines(&m), 0);

        // A plain hover (no modifier) over the link changes nothing.
        let cmds = hover(&mut m, 13.0, 4.0, Mods::NONE);
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::PointerIcon(_))));
        assert_eq!(underlines(&m), 0);
    }

    #[test]
    fn ctrl_click_opens_a_hyperlink() {
        let mut m = model();
        feed(
            &mut m,
            b"\x1b]8;;https://example.com/doc\x1b\\LINK\x1b]8;;\x1b\\ plain",
        );

        // Ctrl+click on the linked run opens it, and starts no selection.
        let cmds = m.update(press_at_cell(0, 1, CTRL));
        assert!(
            cmds.contains(&Cmd::OpenUrl("https://example.com/doc".to_string())),
            "no OpenUrl: {cmds:?}"
        );
        assert!(m.selection().is_none(), "link click must not select");

        // A plain click on the link selects as usual, opens nothing.
        let cmds = m.update(press_at_cell(0, 1, Mods::NONE));
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::OpenUrl(_))),
            "plain click opened a link: {cmds:?}"
        );

        // Ctrl+click on unlinked text opens nothing.
        let cmds = m.update(press_at_cell(0, 6, CTRL));
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::OpenUrl(_))),
            "unlinked cell opened a link: {cmds:?}"
        );
    }

    #[test]
    fn ctrl_click_opens_links_even_when_the_app_grabs_the_mouse() {
        let mut m = model();
        // The app tracks all mouse motion (as Claude Code does) — Ctrl+click
        // on a link must still open locally, not be forwarded.
        feed(
            &mut m,
            b"\x1b[?1003h\x1b[?1006h\x1b]8;;https://example.com\x1b\\LINK",
        );
        let cmds = m.update(press_at_cell(0, 0, CTRL));
        assert!(
            cmds.contains(&Cmd::OpenUrl("https://example.com".to_string())),
            "no OpenUrl under mouse grab: {cmds:?}"
        );
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::SendInput { .. })),
            "the consumed click leaked to the app: {cmds:?}"
        );
    }

    #[test]
    fn unsafe_hyperlink_schemes_are_not_opened() {
        let mut m = model();
        feed(&mut m, b"\x1b]8;;javascript:alert(1)\x1b\\EVIL");
        let cmds = m.update(press_at_cell(0, 0, CTRL));
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::OpenUrl(_))),
            "unsafe scheme opened: {cmds:?}"
        );
    }

    #[test]
    fn osc52_writes_reach_the_system_clipboard_cmds() {
        let mut m = model();
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"\x1b]52;c;aGVsbG8=\x07".to_vec(), // "hello"
            ended: false,
        });
        assert!(
            cmds.contains(&Cmd::WriteClipboard("hello".to_string())),
            "no clipboard write: {cmds:?}"
        );
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"\x1b]52;p;cHJpbWFyeQ==\x07".to_vec(), // "primary"
            ended: false,
        });
        assert!(
            cmds.contains(&Cmd::WritePrimary("primary".to_string())),
            "no primary write: {cmds:?}"
        );
        // The read form gets no reply — nothing goes back to the app.
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"\x1b]52;c;?\x07".to_vec(),
            ended: false,
        });
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::SendInput { .. })),
            "clipboard query must stay unanswered: {cmds:?}"
        );
    }

    #[test]
    fn cursor_only_move_repaints_and_damages_both_rows() {
        let mut m = model();
        // Establish content and a known cursor (col 5, row 0 after "hello"), then
        // composite so the damage baseline is clear.
        feed(&mut m, b"hello");
        m.mark_presented();
        assert!(matches!(view_damage(&m), TermDamage::None));

        // A bare cursor move — CUP to row 3, col 1 — writes no cell, so `Screen::feed`
        // reports no dirty row. But the drawn block still jumps from row 0 to row 2, so
        // the frame changed: it must repaint and damage the row the cursor left (0) and
        // the row it entered (2). This is the "cursor doesn't advance on space" jank in
        // full-screen apps, whose differential renderers move the cursor without
        // rewriting the cell.
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"\x1b[3;1H".to_vec(),
            ended: false,
        });
        assert!(
            cmds.contains(&Cmd::Redraw),
            "a cursor-only move must repaint so the block isn't left stale: {cmds:?}"
        );
        assert!(
            matches!(view_damage(&m), TermDamage::Rows { lo: 0, hi: 2 }),
            "damage must cover the row the cursor left and the one it entered, got {:?}",
            view_damage(&m)
        );
    }

    #[test]
    fn synchronized_output_holds_redraw_until_reset() {
        let mut m = model();
        // An atomic frame opens (DEC 2026) and content lands: presentation is
        // held — no redraw — and a release timeout is scheduled in case the
        // app never closes the frame.
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"\x1b[?2026hhello".to_vec(),
            ended: false,
        });
        assert!(
            !cmds.contains(&Cmd::Redraw),
            "redraw leaked mid-frame: {cmds:?}"
        );
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::ScheduleTick { .. })),
            "no release timeout scheduled: {cmds:?}"
        );

        // More held content: still no redraw, but the release backstop IS
        // re-armed. A hold can be latched into a warm mirror whose single
        // rising-edge tick is spent on the wrong model, then promoted to the
        // foreground with nothing pending; re-arming every held feed keeps a
        // release always in flight (see
        // `a_feed_swallowed_by_an_open_hold_re_arms_the_release_backstop`).
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b" world".to_vec(),
            ended: false,
        });
        assert!(!cmds.contains(&Cmd::Redraw), "redraw leaked: {cmds:?}");
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::ScheduleTick { .. })),
            "a held feed must re-arm the release backstop: {cmds:?}"
        );

        // The frame closes: one redraw presents the accumulated content, even
        // though the closing feed itself changed no viewport row.
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"\x1b[?2026l".to_vec(),
            ended: false,
        });
        assert!(
            cmds.contains(&Cmd::Redraw),
            "no redraw on frame close: {cmds:?}"
        );
    }

    #[test]
    fn synchronized_output_hold_times_out() {
        let mut m = model();
        m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"\x1b[?2026hhello".to_vec(),
            ended: false,
        });
        // The app never closes the frame: the scheduled tick releases the hold
        // so a stuck client cannot freeze the window.
        let cmds = m.update(UiEvent::Tick { now_ms: 1_000 });
        assert!(
            cmds.contains(&Cmd::Redraw),
            "timeout did not release the hold: {cmds:?}"
        );
        // With nothing held, a tick is a no-op.
        let cmds = m.update(UiEvent::Tick { now_ms: 2_000 });
        assert!(!cmds.contains(&Cmd::Redraw), "spurious redraw: {cmds:?}");
    }

    #[test]
    fn hiding_the_cursor_repaints_its_row() {
        let mut m = model();
        feed(&mut m, b"hello"); // visible cursor on row 0
        m.mark_presented();
        assert!(matches!(view_damage(&m), TermDamage::None));

        // Hiding the cursor (DECTCEM reset) erases its block — a visible change on its
        // row even though no cell content moved.
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"\x1b[?25l".to_vec(),
            ended: false,
        });
        assert!(
            cmds.contains(&Cmd::Redraw),
            "hiding the cursor must repaint to erase the block: {cmds:?}"
        );
        assert!(
            matches!(view_damage(&m), TermDamage::Rows { lo: 0, hi: 0 }),
            "hiding damages the cursor's row, got {:?}",
            view_damage(&m)
        );
    }

    /// The single terminal item's rect from a model's `view` (there is exactly one).
    fn view_rect(m: &TerminalModel) -> RectPx {
        match m.view().terminals().next().expect("a single terminal item") {
            SceneItem::Terminal { rect, .. } => *rect,
            other => panic!("expected one terminal item, got {other:?}"),
        }
    }

    #[test]
    fn padding_insets_the_grid_and_scene_rect() {
        let mut m = model();
        // 720x432 at scale 1 with 9x18 cells is exactly 80x24, filling the window.
        m.update(UiEvent::Resize {
            w_px: 720,
            h_px: 432,
            scale: 1.0,
        });
        let base = view_rect(&m);
        assert_eq!((base.x, base.y, base.w, base.h), (0.0, 0.0, 720.0, 432.0));
        assert_eq!(m.screen().dimensions(), (80, 24));

        // 18 logical px of padding (== two columns / one row here) insets the grid by
        // a cell on each side and the item rect by the padding, while the scene canvas
        // stays the full window — so the border is bg, not clipped content.
        m.set_padding(18.0);
        m.update(UiEvent::Resize {
            w_px: 720,
            h_px: 432,
            scale: 1.0,
        });
        let scene = m.view();
        assert_eq!(scene.size_px, (720, 432), "canvas stays the whole window");
        let r = match scene.terminals().next().unwrap() {
            SceneItem::Terminal { rect, .. } => *rect,
            _ => unreachable!(),
        };
        assert_eq!(
            (r.x, r.y, r.w, r.h),
            (18.0, 18.0, 720.0 - 36.0, 432.0 - 36.0)
        );
        // (720-36)/9 = 76 cols, (432-36)/18 = 22 rows.
        assert_eq!(m.screen().dimensions(), (76, 22));
    }

    #[test]
    fn padding_scales_with_the_device_factor() {
        // Padding is logical px, so a 2x display doubles it in physical px: the inset
        // rect and grid must reflect the physical border, matching the renderer.
        let mut m = model();
        m.set_padding(10.0);
        m.update(UiEvent::Resize {
            w_px: 1440,
            h_px: 864,
            scale: 2.0,
        });
        let r = view_rect(&m);
        assert_eq!(
            (r.x, r.y),
            (20.0, 20.0),
            "10 logical px -> 20 physical at 2x"
        );
        assert_eq!((r.w, r.h), (1440.0 - 40.0, 864.0 - 40.0));
    }

    #[test]
    fn padding_offsets_the_ime_cursor_area() {
        let mut m = model();
        m.set_padding(18.0);
        m.update(UiEvent::Resize {
            w_px: 720,
            h_px: 432,
            scale: 1.0,
        });
        // A fresh cursor at cell (0,0) sits at the padding origin, not the corner.
        let a = m.ime_cursor_area().unwrap();
        assert_eq!((a.x, a.y), (18.0, 18.0));
        // "abc" advances the cursor three cells: x = pad + 3*advance.
        feed(&mut m, b"abc");
        let a = m.ime_cursor_area().unwrap();
        assert_eq!((a.x, a.y), (18.0 + 27.0, 18.0));
    }

    #[test]
    fn padding_offsets_pointer_hit_testing() {
        let mut m = model();
        m.set_padding(18.0);
        m.update(UiEvent::Resize {
            w_px: 720,
            h_px: 432,
            scale: 1.0,
        });
        feed(&mut m, b"hello world");
        // A press at the padding origin lands on cell (0,0) — the same pixel maps to a
        // different cell without the inset, so this pins the offset.
        m.update(ptr(PointerPhase::Motion, None, 18.0, 18.0));
        m.update(ptr(
            PointerPhase::Press,
            Some(PointerButton::Left),
            18.0,
            18.0,
        ));
        m.update(ptr(
            PointerPhase::Motion,
            Some(PointerButton::Left),
            18.0 + 9.0 * 3.0,
            18.0,
        ));
        match m.view().terminals().next().unwrap() {
            SceneItem::Terminal { selection, .. } => {
                let sel = selection.expect("a drag selects");
                assert_eq!(
                    sel.start,
                    (0, 0),
                    "press at the padding origin is cell (0,0)"
                );
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn hidden_cursor_move_does_not_repaint() {
        let mut m = model();
        feed(&mut m, b"hello\x1b[?25l"); // draw, then hide the cursor
        m.mark_presented();
        assert!(matches!(view_damage(&m), TermDamage::None));

        // With the cursor hidden nothing is drawn at it, so a bare move paints no
        // pixels and must not force a repaint.
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"\x1b[3;1H".to_vec(),
            ended: false,
        });
        assert!(
            !cmds.contains(&Cmd::Redraw),
            "moving a hidden cursor changes no pixels: {cmds:?}"
        );
    }

    /// The per-session [`TermDamage`] the model stamps on its scene item — the cue the
    /// renderer uses to decide how much of the Surface to re-raster.
    fn view_damage(m: &TerminalModel) -> TermDamage {
        match m.view().terminals().next().expect("a single terminal item") {
            SceneItem::Terminal { damage, .. } => *damage,
            other => panic!("the single view is one terminal item, got {other:?}"),
        }
    }

    /// The number of kitty-graphics images the view's frame carries.
    fn frame_image_count(m: &TerminalModel) -> usize {
        match m.view().terminals().next().expect("a single terminal item") {
            SceneItem::Terminal { frame, .. } => frame.images.len(),
            other => panic!("the single view is one terminal item, got {other:?}"),
        }
    }

    #[test]
    fn feed_damage_localizes_and_accumulates_until_presented() {
        let mut m = model();
        // No present has happened yet, so the first frame is a full repaint, and the
        // first feed carries the terminal's initial all-rows paint (a full band).
        assert!(
            matches!(view_damage(&m), TermDamage::All),
            "the first frame is always full"
        );
        feed(&mut m, b"ready> ");

        // Compositing establishes the baseline: with nothing fed since, there is no
        // damage — the renderer keeps the Surface it already holds.
        m.mark_presented();
        assert!(matches!(view_damage(&m), TermDamage::None));

        // Steady output now: one more line dirties exactly its row; the scene carries
        // that band, so the renderer updates only those rows in place, not the whole
        // Surface. (The cursor sits on row 0 after "ready> ".)
        feed(&mut m, b"hello");
        assert!(
            matches!(view_damage(&m), TermDamage::Rows { lo: 0, hi: 0 }),
            "a one-row feed localizes to its row, got {:?}",
            view_damage(&m)
        );

        // Damage accumulates across coalesced feeds until the next present: a later
        // feed on a different row widens the band to cover both.
        feed(&mut m, b"\r\nworld");
        assert!(
            matches!(view_damage(&m), TermDamage::Rows { lo: 0, hi: 1 }),
            "coalesced feeds widen the dirty band, got {:?}",
            view_damage(&m)
        );

        // Presenting clears the accumulated damage; the same view is unchanged again.
        m.mark_presented();
        assert!(matches!(view_damage(&m), TermDamage::None));
    }

    #[test]
    fn a_scroll_is_a_full_repaint_no_feed_hint_can_localize() {
        let mut m = model();
        feed_lines(&mut m, 100); // build scrollback to scroll into
        m.mark_presented();
        assert!(matches!(view_damage(&m), TermDamage::None));

        // Scrolling up moves the viewport — a change the per-row feed hint can't
        // express as a band — so the whole view repaints.
        let cmds = m.update(wheel(1.0));
        assert!(cmds.contains(&Cmd::Redraw), "a scroll repaints");
        assert!(
            matches!(view_damage(&m), TermDamage::All),
            "a scroll is a full repaint, got {:?}",
            view_damage(&m)
        );
    }

    #[test]
    fn deleting_a_graphics_placement_repaints_and_damages_the_view() {
        let mut m = model();
        // a=T transmits AND places a 2x1-cell image. The upload is its own
        // repaint trigger, damaging the whole view (its footprint sits outside
        // the row hint), so the baseline present below starts clean.
        feed(&mut m, b"\x1b_Ga=T,i=7,f=24,s=2,v=1,c=2,r=1;/wAAAP8A\x1b\\");
        assert_eq!(frame_image_count(&m), 1, "the placement is in the frame");
        m.mark_presented();
        assert!(matches!(view_damage(&m), TermDamage::None));

        // Deleting every placement (a=d,d=a) removes the image from the frame:
        // the rows it covered now render without it. No cell was written and no
        // image was *uploaded*, so nothing else will trigger the repaint.
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"\x1b_Ga=d,d=a\x1b\\".to_vec(),
            ended: false,
        });
        assert_eq!(frame_image_count(&m), 0, "the placement left the frame");
        assert!(
            cmds.contains(&Cmd::Redraw),
            "removing an on-screen image must repaint, got {cmds:?}"
        );
        assert!(
            !matches!(view_damage(&m), TermDamage::None),
            "the rows the image uncovered must be damaged"
        );
    }

    /// The rows of the *visible* (scroll-adjusted) frame, as text — the ground
    /// truth a damage claim is checked against, in the same row space
    /// [`TermDamage::Rows`] is consumed in (see `rows_differ_outside`).
    fn visible_rows(m: &TerminalModel) -> Vec<String> {
        m.screen
            .vt()
            .view_at(m.scroll_offset)
            .map(|l| l.text())
            .collect()
    }

    /// Assert the view's damage claim covers every frame row whose text differs
    /// between `before` and `after` — the `rows_differ_outside` contract the
    /// renderer's band re-raster trusts.
    #[track_caller]
    fn assert_damage_covers(m: &TerminalModel, before: &[String], after: &[String]) {
        let damage = view_damage(m);
        let covered = |row: usize| match damage {
            TermDamage::All => true,
            TermDamage::Rows { lo, hi } => lo <= row && row <= hi,
            TermDamage::None => false,
        };
        let missed: Vec<usize> = before
            .iter()
            .zip(after)
            .enumerate()
            .filter(|(row, (b, a))| b != a && !covered(*row))
            .map(|(row, _)| row)
            .collect();
        assert!(
            missed.is_empty(),
            "TermDamage under-report: frame rows {missed:?} changed outside the claim {damage:?}"
        );
    }

    #[test]
    fn in_place_update_while_scrolled_back_damages_the_visible_frame_row() {
        let mut m = model();
        feed_lines(&mut m, 100); // history to scroll into
        m.set_scroll(3); // live row L is visible at frame row L+3
        m.mark_presented();
        assert!(matches!(view_damage(&m), TermDamage::None));

        // The app rewrites live row 0 in place (CUP home + text) — the way a
        // spinner or status line redraws between pushes. Nothing scrolled, so
        // stay-put pinning leaves the offset alone and `moved` stays false.
        let before = visible_rows(&m);
        feed(&mut m, b"\x1b[1;1HREWRITTEN");
        assert_eq!(m.scroll_offset, 3, "no lines pushed; the view stays put");
        let after = visible_rows(&m);
        let changed: Vec<usize> = before
            .iter()
            .zip(&after)
            .enumerate()
            .filter(|(_, (b, a))| b != a)
            .map(|(row, _)| row)
            .collect();
        assert_eq!(changed, vec![3], "live row 0 is visible at frame row 3");

        // The claim must cover frame row 3 — the row that actually changed on
        // screen — not (only) live row 0.
        assert_damage_covers(&m, &before, &after);
    }

    #[test]
    fn history_sliding_under_a_view_pinned_at_the_scrollback_cap_is_covered() {
        let mut m = model();
        // Fill scrollback past its cap (DEFAULT_SCROLLBACK) so trimming is
        // live, then pin the view at the very top of retained history.
        feed_lines(&mut m, screen::DEFAULT_SCROLLBACK + 100);
        m.set_scroll(m.max_scroll());
        m.mark_presented();
        assert!(matches!(view_damage(&m), TermDamage::None));

        // A scroll region pinned to the top (DECSTBM 1;5) scrolling pushes its
        // top line into scrollback while dirtying only live rows 0..5. At the
        // cap the eviction slides the whole pinned window, but the offset is
        // clamped (max_scroll is unchanged), so stay-put pinning cannot absorb
        // it and `moved` stays false.
        let before = visible_rows(&m);
        feed(&mut m, b"\x1b[1;5r\x1b[5;1H\ntail-a\ntail-b\x1b[r");
        assert_eq!(m.scroll_offset, m.max_scroll(), "still pinned at the cap");
        let after = visible_rows(&m);
        assert_ne!(before, after, "the capped history window slides");

        assert_damage_covers(&m, &before, &after);
    }

    #[test]
    fn osc_title_change_emits_set_title() {
        let mut m = model();
        let feed_cmds = |m: &mut TerminalModel, b: &[u8]| {
            m.update(UiEvent::SessionData {
                name: "alpha".to_string(),
                bytes: b.to_vec(),
                ended: false,
            })
        };
        // OSC 2 sets the window title -> the model asks the shell to apply it.
        let cmds = feed_cmds(&mut m, b"\x1b]2;my-prog\x07");
        assert!(cmds.contains(&Cmd::SetTitle("my-prog".to_string())));
        // Plain output with the same title doesn't re-emit.
        let cmds = feed_cmds(&mut m, b"x");
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::SetTitle(_))));
        // A different title emits again.
        let cmds = feed_cmds(&mut m, b"\x1b]2;other\x07");
        assert!(cmds.contains(&Cmd::SetTitle("other".to_string())));
    }

    #[test]
    fn title_prefixes_a_custom_display_name_onto_the_app_title() {
        let mut m = model(); // session id "alpha"
        assert_eq!(m.title(), "alpha", "no title, no label: the id");
        m.set_display_name("build box".to_string());
        assert_eq!(m.title(), "build box", "the display name beats the id");
        m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"\x1b]2;vim\x07".to_vec(),
            ended: false,
        });
        assert_eq!(
            m.title(),
            "build box — vim",
            "a user-chosen label prefixes the app's OSC title"
        );
        // A label merely equal to the auto-generated id is not a rename the
        // user cares to see twice: no prefix.
        m.set_display_name("alpha".to_string());
        assert_eq!(m.title(), "vim", "a label equal to the id does not prefix");
        m.set_display_name(String::new());
        assert_eq!(m.title(), "vim", "no label: the OSC title alone");
    }

    #[test]
    fn clearing_the_title_falls_back_to_the_session_name() {
        let mut m = model(); // session "alpha"
        let feed_cmds = |m: &mut TerminalModel, b: &[u8]| {
            m.update(UiEvent::SessionData {
                name: "alpha".to_string(),
                bytes: b.to_vec(),
                ended: false,
            })
        };
        feed_cmds(&mut m, b"\x1b]2;my-prog\x07");
        // Clearing the title (OSC 2 with an empty payload — some TUIs send this on
        // exit) must not blank the titlebar: fall back to the session name, matching
        // what a foreground switch would show for a titleless session.
        let cmds = feed_cmds(&mut m, b"\x1b]2;\x07");
        assert!(
            cmds.contains(&Cmd::SetTitle("alpha".to_string())),
            "a cleared title falls back to the session name, not empty: {cmds:?}"
        );
    }

    #[test]
    fn session_data_for_another_session_is_ignored() {
        let mut m = model();
        let cmds = m.update(UiEvent::SessionData {
            name: "beta".to_string(),
            bytes: b"nope".to_vec(),
            ended: false,
        });
        assert_eq!(cmds, vec![]);
    }

    #[test]
    fn session_data_uploads_displayed_images_once_and_answers_the_transfer() {
        let mut m = model();
        // a=T: transmit a 2x1 RGB image (id 5) and display it.
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"\x1b_Gi=5,a=T,f=24,s=2,v=1;/wAAAP8A\x1b\\".to_vec(),
            ended: false,
        });
        // The pixels are uploaded out of band, RGBA (red, green, opaque).
        assert!(cmds.contains(&Cmd::UploadImage {
            id: 5,
            width: 2,
            height: 1,
            rgba: vec![255, 0, 0, 255, 0, 255, 0, 255],
        }));
        // The attached frontend answers the transfer with the OK ack itself.
        assert!(cmds.contains(&sent("alpha", b"\x1b_Gi=5;OK\x1b\\")));

        // Later plain output does not re-upload the same image.
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"x".to_vec(),
            ended: false,
        });
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::UploadImage { .. })));
    }

    #[test]
    fn re_transmitting_an_image_under_the_same_id_re_uploads_the_new_pixels() {
        let mut m = model();
        // a=T: transmit a 2x1 RGB image (id 5, red|green) and display it.
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"\x1b_Gi=5,a=T,f=24,s=2,v=1;/wAAAP8A\x1b\\".to_vec(),
            ended: false,
        });
        assert!(cmds.contains(&Cmd::UploadImage {
            id: 5,
            width: 2,
            height: 1,
            rgba: vec![255, 0, 0, 255, 0, 255, 0, 255],
        }));
        // Re-transmit id 5 with DIFFERENT pixels (blue|green). kitty lets a client
        // replace an image under an existing id (an animation frame, a reused id); the
        // renderer still holds the OLD pixels, so the model must send the new ones.
        // Keying uploads on the id alone leaves the image stale on screen forever.
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"\x1b_Gi=5,a=T,f=24,s=2,v=1;AAD/AP8A\x1b\\".to_vec(),
            ended: false,
        });
        let uploaded = cmds.iter().find_map(|c| match c {
            Cmd::UploadImage { id: 5, rgba, .. } => Some(rgba.clone()),
            _ => None,
        });
        assert_eq!(
            uploaded,
            Some(vec![0, 0, 255, 255, 0, 255, 0, 255]),
            "a re-transmit under an existing id must re-upload the replaced pixels: {cmds:?}"
        );
    }

    #[test]
    fn session_data_uploads_images_referenced_only_by_placeholders() {
        let mut m = model();
        // Transmit (store, don't display) a 2x1 image as id 7, then print two
        // Unicode-placeholder cells referencing it via the foreground colour.
        let mut bytes = b"\x1b_Gi=7,a=t,f=24,s=2,v=1;/wAAAP8A\x1b\\\x1b[38;2;0;0;7m".to_vec();
        bytes.extend("\u{10eeee}\u{10eeee}".as_bytes());
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes,
            ended: false,
        });
        // Even with no direct placement, the placeholder reference triggers upload.
        assert!(cmds.contains(&Cmd::UploadImage {
            id: 7,
            width: 2,
            height: 1,
            rgba: vec![255, 0, 0, 255, 0, 255, 0, 255],
        }));
        // And it uploads once: a later redraw-causing feed does not re-upload.
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"x".to_vec(),
            ended: false,
        });
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::UploadImage { .. })));
    }

    #[test]
    fn placeholder_image_transmitted_after_scrolling_off_still_uploads() {
        let mut m = model();
        // A placeholder referencing id 7 is printed before the image exists, then
        // scrolled out of the viewport. No upload yet (nothing stored).
        let mut bytes = b"\x1b[38;2;0;0;7m".to_vec();
        bytes.extend("\u{10eeee}\r\n".as_bytes());
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes,
            ended: false,
        });
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::UploadImage { .. })));
        m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: vec![b'\n'; 30], // push the placeholder line into scrollback
            ended: false,
        });

        // Now the image arrives. The placeholder is in scrollback, not the live
        // viewport, but the freshly-stored image must still upload.
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"\x1b_Gi=7,a=t,f=24,s=2,v=1;/wAAAP8A\x1b\\".to_vec(),
            ended: false,
        });
        assert!(cmds.contains(&Cmd::UploadImage {
            id: 7,
            width: 2,
            height: 1,
            rgba: vec![255, 0, 0, 255, 0, 255, 0, 255],
        }));
    }

    #[test]
    fn session_data_answers_a_graphics_query_without_uploading() {
        let mut m = model();
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"\x1b_Gi=31,a=q,f=24,s=1,v=1;AAAA\x1b\\".to_vec(),
            ended: false,
        });
        // A query is answered (support probe) but stores/displays nothing.
        assert!(cmds.contains(&sent("alpha", b"\x1b_Gi=31;OK\x1b\\")));
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::UploadImage { .. })));
    }

    #[test]
    fn ended_session_sets_the_flag() {
        let mut m = model();
        assert!(!m.ended());
        m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: vec![],
            ended: true,
        });
        assert!(m.ended());
    }

    #[test]
    fn view_renders_one_terminal_carrying_the_selection() {
        let mut m = model();
        feed(&mut m, b"hello world");
        m.update(ptr(PointerPhase::Motion, None, 1.0, 1.0));
        m.update(ptr(
            PointerPhase::Press,
            Some(PointerButton::Left),
            1.0,
            1.0,
        ));
        m.update(ptr(
            PointerPhase::Motion,
            Some(PointerButton::Left),
            40.0,
            1.0,
        ));
        let scene = m.view();
        assert_eq!(scene.terminals().count(), 1);
        match scene.terminals().next().unwrap() {
            SceneItem::Terminal {
                id,
                selection,
                frame,
                ..
            } => {
                assert_eq!(*id, SceneId::Root);
                assert!(selection.is_some());
                assert!(frame.rows_layout[0].runs[0].text.starts_with("hello"));
            }
            _ => unreachable!(),
        }
    }

    // ---- scrollback ----

    #[test]
    fn wheel_scrolls_back_into_history() {
        let mut m = model(); // 80x24
        feed_lines(&mut m, 100); // viewport L76..L99
        assert_eq!(top_row_text(&m), "L76", "starts at the live bottom");
        let cmds = m.update(wheel(1.0)); // one notch up
        assert!(cmds.contains(&Cmd::Redraw));
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::SendInput { .. })),
            "local scroll never sends to the child"
        );
        assert_eq!(top_row_text(&m), "L73", "scrolled up one notch (3 lines)");
    }

    #[test]
    fn wheel_down_returns_to_live_and_clamps_at_bottom() {
        let mut m = model();
        feed_lines(&mut m, 100);
        m.update(wheel(1.0)); // up -> L73
        m.update(wheel(-1.0)); // down -> L76 (live)
        assert_eq!(top_row_text(&m), "L76");
        // Already at the bottom: scrolling further down does nothing.
        let cmds = m.update(wheel(-1.0));
        assert_eq!(top_row_text(&m), "L76");
        assert!(!cmds.contains(&Cmd::Redraw), "no-op scroll emits no redraw");
    }

    #[test]
    fn scroll_clamps_at_the_oldest_line() {
        let mut m = model();
        feed_lines(&mut m, 100); // scrollback = 76 lines
        for _ in 0..100 {
            m.update(wheel(1.0)); // scroll far past the top
        }
        assert_eq!(top_row_text(&m), "L0", "clamps at the oldest retained line");
    }

    #[test]
    fn ctrl_shift_arrows_jump_between_prompts() {
        const CTRL_SHIFT: Mods = Mods {
            shift: true,
            ctrl: true,
            alt: false,
            sup: false,
        };
        let mut m = model();
        // Three OSC 133;A-marked prompts with enough output between them to
        // push the early ones into scrollback (24-row viewport).
        let mut s = String::from("\x1b]133;A\x07P0");
        for i in 1..40 {
            s.push_str(&format!("\r\nA{i}"));
        }
        s.push_str("\r\n\x1b]133;A\x07P1");
        for i in 1..40 {
            s.push_str(&format!("\r\nB{i}"));
        }
        s.push_str("\r\n\x1b]133;A\x07P2");
        feed(&mut m, s.as_bytes());
        assert_eq!(top_row_text(&m), "B17", "starts at the live bottom");

        // Up walks back through prompt history, landing each prompt at the top.
        let cmds = key(&mut m, Key::Named(NamedKey::ArrowUp), CTRL_SHIFT);
        assert_eq!(top_row_text(&m), "P1");
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::SendInput { .. })),
            "the jump chord never reaches the child"
        );
        key(&mut m, Key::Named(NamedKey::ArrowUp), CTRL_SHIFT);
        assert_eq!(top_row_text(&m), "P0");
        key(&mut m, Key::Named(NamedKey::ArrowUp), CTRL_SHIFT);
        assert_eq!(top_row_text(&m), "P0", "no prompt above: stays put");

        // Down walks forward again, then back to the live view.
        key(&mut m, Key::Named(NamedKey::ArrowDown), CTRL_SHIFT);
        assert_eq!(top_row_text(&m), "P1");
        key(&mut m, Key::Named(NamedKey::ArrowDown), CTRL_SHIFT);
        assert_eq!(
            top_row_text(&m),
            "B17",
            "last prompt is on-screen: live view"
        );
        let cmds = key(&mut m, Key::Named(NamedKey::ArrowDown), CTRL_SHIFT);
        assert_eq!(top_row_text(&m), "B17", "no prompt below: stays put");
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::SendInput { .. })),
            "the chord is consumed even when there is nowhere to jump"
        );
    }

    #[test]
    fn typing_snaps_to_the_bottom() {
        let mut m = model();
        feed_lines(&mut m, 100);
        m.update(wheel(1.0));
        assert_eq!(top_row_text(&m), "L73");
        let cmds = key(&mut m, Key::Char("x".into()), Mods::NONE);
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::SendInput { .. })),
            "the keystroke still reaches the child"
        );
        assert_eq!(top_row_text(&m), "L76", "typing jumps back to live output");
    }

    #[test]
    fn output_keeps_the_scroll_position_stable() {
        let mut m = model();
        feed_lines(&mut m, 100); // top live L76, scrollback 76
        m.update(wheel(1.0)); // offset 3 -> top L73
        assert_eq!(top_row_text(&m), "L73");
        // New output arrives while scrolled; the viewed line stays put.
        feed(&mut m, b"\r\nL100\r\nL101");
        assert_eq!(top_row_text(&m), "L73");
    }

    #[test]
    fn output_keeps_scroll_position_stable_at_the_scrollback_cap() {
        // Saturate scrollback past DEFAULT_SCROLLBACK so every new line trims an
        // old one — the case where a naive scrollback_len delta reads zero growth.
        let mut m = model();
        feed_lines(&mut m, 1100);
        for _ in 0..10 {
            m.update(wheel(1.0)); // scroll ~30 lines up, away from both ends
        }
        let pinned = top_row_text(&m);
        // More output arrives; the viewed line must stay put even at the cap.
        feed(&mut m, b"\r\nX0\r\nX1\r\nX2\r\nX3\r\nX4");
        assert_eq!(
            top_row_text(&m),
            pinned,
            "view stays pinned to its history line even while scrollback trims"
        );
    }

    #[test]
    fn scrolling_mid_drag_retargets_the_selection_not_the_content() {
        let mut m = model();
        feed_lines(&mut m, 100);
        m.update(wheel(1.0)); // top row "L73"
        // Begin a left-drag anchored on "L73" (anchor at col 2, active at col 0).
        begin_drag(&mut m, 20.0, 1.0);
        m.update(ptr(
            PointerPhase::Motion,
            Some(PointerButton::Left),
            1.0,
            1.0,
        ));
        // A wheel mid-drag scrolls the viewport; the selection stays pinned to
        // its content (absolute line space) and extends to whatever now sits
        // under the pointer.
        assert!(
            m.update(wheel(1.0)).contains(&Cmd::Redraw),
            "wheel scrolls mid-drag"
        );
        assert_eq!(top_row_text(&m), "L70");
        // Shift+PageUp/PageDown mid-drag likewise scroll (and cancel out).
        let cmds = key(&mut m, Key::Named(NamedKey::PageUp), Mods::SHIFT);
        assert!(cmds.contains(&Cmd::Redraw), "Shift+PageUp scrolls mid-drag");
        key(&mut m, Key::Named(NamedKey::PageDown), Mods::SHIFT);
        assert_eq!(top_row_text(&m), "L70");
        m.update(ptr(
            PointerPhase::Release,
            Some(PointerButton::Left),
            1.0,
            1.0,
        ));
        assert_eq!(
            key(&mut m, Key::Char("c".into()), Mods::CTRL | Mods::SHIFT),
            vec![Cmd::WriteClipboard("L70\nL71\nL72\nL73".to_string())],
            "the copy runs from the anchored content to the pointer's row"
        );
    }

    #[test]
    fn scrolled_selection_survives_background_output() {
        let mut m = model();
        feed_lines(&mut m, 100);
        m.update(wheel(1.0)); // top row "L73"
        m.update(ptr(PointerPhase::Motion, None, 1.0, 1.0));
        m.update(ptr(
            PointerPhase::Press,
            Some(PointerButton::Left),
            1.0,
            1.0,
        ));
        m.update(ptr(
            PointerPhase::Motion,
            Some(PointerButton::Left),
            18.0,
            1.0,
        ));
        m.update(ptr(
            PointerPhase::Release,
            Some(PointerButton::Left),
            18.0,
            1.0,
        ));
        assert!(m.selection().is_some());
        // Background output keeps the line pinned (stay-put) AND keeps the
        // selection, since its content is still on screen.
        feed(&mut m, b"\r\nL100\r\nL101");
        assert_eq!(top_row_text(&m), "L73");
        assert_eq!(
            key(&mut m, Key::Char("c".into()), Mods::CTRL | Mods::SHIFT),
            vec![Cmd::WriteClipboard("L73".to_string())]
        );
    }

    #[test]
    fn shift_pageup_scrolls_a_page_without_sending_input() {
        let mut m = model(); // 24 rows -> page = 23 lines
        feed_lines(&mut m, 100);
        let cmds = m.update(UiEvent::Key {
            key: Key::Named(NamedKey::PageUp),
            mods: Mods::SHIFT,
            kind: KeyEventKind::Press,
            alts: None,
        });
        assert!(cmds.contains(&Cmd::Redraw));
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::SendInput { .. })),
            "Shift+PageUp scrolls locally, not into the child"
        );
        assert_eq!(top_row_text(&m), "L53", "scrolled up one page (23 lines)");
    }

    #[test]
    fn copy_reads_text_from_scrolled_history() {
        let mut m = model();
        feed_lines(&mut m, 100);
        m.update(wheel(1.0)); // top row is now "L73"
        // Select columns 0..=2 of the top (historical) row.
        m.update(ptr(PointerPhase::Motion, None, 1.0, 1.0));
        m.update(ptr(
            PointerPhase::Press,
            Some(PointerButton::Left),
            1.0,
            1.0,
        ));
        m.update(ptr(
            PointerPhase::Motion,
            Some(PointerButton::Left),
            18.0,
            1.0,
        ));
        assert_eq!(
            key(&mut m, Key::Char("c".into()), Mods::CTRL | Mods::SHIFT),
            vec![Cmd::WriteClipboard("L73".to_string())],
            "copy reads the visible history, not the live viewport"
        );
    }

    // ---- moved pure-helper tests ----

    /// A `mode_state` for tests: nothing is recognized.
    fn no_modes(_: u16) -> ghost_term::ModeReport {
        ghost_term::ModeReport::Unrecognized
    }

    /// A `checksum` for tests: the query tests here don't exercise DECRQCRA.
    fn no_checksum(_: usize, _: usize, _: usize, _: usize) -> u16 {
        0
    }

    /// A `palette` for tests: the app has overridden no indexed color.
    fn no_palette(_: u8) -> Option<[u8; 3]> {
        None
    }

    /// A `special` for tests: the app has overridden no special color.
    fn no_special(_: ghost_term::SpecialColor) -> Option<[u8; 3]> {
        None
    }

    /// A baseline reply context; tests override the fields they exercise.
    fn reply_ctx() -> ReplyCtx<'static> {
        ReplyCtx {
            cursor: (1, 1),
            size: (80, 24),
            display_size: ghost_vt::query::NOMINAL_DISPLAY_CHARS,
            iconified: false,
            size_px: (720, 432),
            display_px: ghost_vt::query::NOMINAL_DISPLAY_PX,
            cell_px: ghost_vt::query::NOMINAL_CELL_PX,
            title: "",
            icon_title: "",
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
            checksum: &no_checksum,
        }
    }

    #[test]
    fn query_replies_answers_cursor_position() {
        let mut s = QueryScanner::new();
        let ctx = ReplyCtx {
            cursor: (3, 5),
            ..reply_ctx()
        };
        assert_eq!(query_replies(&mut s, b"\x1b[6n", &ctx), b"\x1b[5;3R");
    }

    #[test]
    fn query_replies_answers_the_kitty_keyboard_query_with_current_flags() {
        let mut s = QueryScanner::new();
        // `CSI ? u` is answered with the flags passed in (the model supplies the
        // live `kitty_keyboard_flags()`); a bare `CSI u` is not a query.
        let ctx = ReplyCtx {
            kitty_flags: 5,
            ..reply_ctx()
        };
        assert_eq!(query_replies(&mut s, b"\x1b[?u", &ctx), b"\x1b[?5u");
        assert!(query_replies(&mut s, b"\x1b[u", &ctx).is_empty());
    }

    #[test]
    fn osc_color_queries_answer_from_the_set_theme() {
        let mut m = model();
        m.set_theme(ThemeColors {
            fg: [0x01, 0x02, 0x03],
            bg: [0x0a, 0x0b, 0x0c],
            ..ThemeColors::default()
        });
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"\x1b]11;?\x1b\\\x1b]10;?\x07".to_vec(),
            ended: false,
        });
        assert!(
            cmds.contains(&sent(
                "alpha",
                b"\x1b]11;rgb:0a0a/0b0b/0c0c\x1b\\\x1b]10;rgb:0101/0202/0303\x1b\\"
            )),
            "no themed color reply: {cmds:?}"
        );
    }

    #[test]
    fn color_replies_prefer_app_set_dynamic_colors() {
        let mut m = model();
        // The app overrides the background (OSC 11 set), then queries it and
        // the cursor color back in the same feed — replies must reflect
        // post-feed state: the override for bg, the theme default for cursor.
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"\x1b]11;#204060\x07\x1b]11;?\x07\x1b]12;?\x07".to_vec(),
            ended: false,
        });
        assert!(
            cmds.contains(&sent(
                "alpha",
                b"\x1b]11;rgb:2020/4040/6060\x1b\\\x1b]12;rgb:d8d8/dbdb/e0e0\x1b\\"
            )),
            "dynamic color not preferred: {cmds:?}"
        );
    }

    #[test]
    fn dynamic_color_changes_repaint_and_damage_the_whole_view() {
        let term_damage = |m: &TerminalModel| {
            let scene = m.view();
            match scene.terminals().next().unwrap() {
                SceneItem::Terminal { damage, .. } => *damage,
                _ => unreachable!(),
            }
        };
        let mut m = model();
        feed(&mut m, b"hello");
        m.mark_presented();

        // A color-only feed dirties no rows, but the default bg is every
        // pixel: it must repaint and report whole-view damage.
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"\x1b]11;#204060\x07".to_vec(),
            ended: false,
        });
        assert!(cmds.contains(&Cmd::Redraw), "no repaint: {cmds:?}");
        assert_eq!(term_damage(&m), ghost_render::TermDamage::All);

        // Once presented, the view settles again.
        m.mark_presented();
        assert_eq!(term_damage(&m), ghost_render::TermDamage::None);
    }

    #[test]
    fn color_scheme_query_answers_from_the_live_theme() {
        let mut m = model();
        // Ghost's default theme is dark.
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"\x1b[?996n".to_vec(),
            ended: false,
        });
        assert!(
            cmds.contains(&sent("alpha", b"\x1b[?997;1n")),
            "no color-scheme reply: {cmds:?}"
        );
        // An app-set light background flips the answer.
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"\x1b]11;#ffffff\x07\x1b[?996n".to_vec(),
            ended: false,
        });
        assert!(
            cmds.contains(&sent("alpha", b"\x1b[?997;2n")),
            "no light color-scheme reply: {cmds:?}"
        );
    }

    #[test]
    fn theme_changes_notify_mode_2031_subscribers() {
        const LIGHT: ThemeColors = ThemeColors {
            fg: [0x10, 0x10, 0x12],
            bg: [0xff, 0xff, 0xff],
            cursor: [0x10, 0x10, 0x12],
            ansi: ghost_term::ANSI_16,
        };
        let mut m = model();
        // Nobody subscribed: a theme change stays silent.
        assert!(m.set_theme(LIGHT).is_empty());
        feed(&mut m, b"\x1b[?2031h");
        // Subscribed: flipping back to the dark default reports dark (1).
        let cmds = m.set_theme(ThemeColors::default());
        assert_eq!(cmds, [sent("alpha", b"\x1b[?997;1n")]);
        // Re-setting the same theme is not a change.
        assert!(m.set_theme(ThemeColors::default()).is_empty());
    }

    #[test]
    fn query_replies_answers_decrqm_from_the_live_screen() {
        // Drive the whole model path: the app sets 2026 and queries it in the
        // same feed — the reply must come from post-feed state (mode reported
        // set), as `Cmd::SendInput` on the session.
        let mut m = model();
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b"\x1b[?2026h\x1b[?2026$p".to_vec(),
            ended: false,
        });
        assert!(
            cmds.contains(&sent("alpha", b"\x1b[?2026;1$y")),
            "no DECRPM reply: {cmds:?}"
        );
    }

    #[test]
    fn bracketed_paste_wraps_only_when_enabled() {
        assert_eq!(bracket_paste(b"hi", false), b"hi");
        assert_eq!(bracket_paste(b"hi", true), b"\x1b[200~hi\x1b[201~".to_vec());
    }

    /// Press at `(x, y)` after a motion there, beginning a by-cell drag.
    fn begin_drag(m: &mut TerminalModel, x: f64, y: f64) {
        m.update(ptr(PointerPhase::Motion, None, x, y));
        m.update(ptr(PointerPhase::Press, Some(PointerButton::Left), x, y));
    }

    fn tick(m: &mut TerminalModel) -> Vec<Cmd> {
        m.update(UiEvent::Tick { now_ms: 0 })
    }

    #[test]
    fn dragging_above_the_top_edge_autoscrolls_the_selection_into_history() {
        let mut m = model();
        feed_lines(&mut m, 30); // L0..L29: 6 lines in scrollback, top row shows L6
        assert_eq!(top_row_text(&m), "L6");
        begin_drag(&mut m, 10.0, 1.0); // anchor on the top row, col 1
        // Hovering just above the grid arms the autoscroll.
        let cmds = m.update(ptr(
            PointerPhase::Motion,
            Some(PointerButton::Left),
            1.0,
            -1.0,
        ));
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::ScheduleTick { .. })),
            "hovering the top edge schedules autoscroll ticks: {cmds:?}"
        );
        // Each tick scrolls one line deeper and extends the selection to the
        // revealed row; the tick keeps itself alive.
        let cmds = tick(&mut m);
        assert_eq!(top_row_text(&m), "L5");
        assert!(cmds.contains(&Cmd::Redraw));
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::ScheduleTick { .. })),
            "autoscroll reschedules while armed: {cmds:?}"
        );
        for _ in 0..5 {
            tick(&mut m);
        }
        // Pinned at the top of scrollback: the autoscroll stops rescheduling.
        assert_eq!(top_row_text(&m), "L0");
        let cmds = tick(&mut m);
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::ScheduleTick { .. })),
            "pinned at the scrollback top the autoscroll disarms: {cmds:?}"
        );
        // The selection covers everything from L0 up to the anchor row.
        let cmds = m.update(ptr(
            PointerPhase::Release,
            Some(PointerButton::Left),
            1.0,
            -1.0,
        ));
        let text = cmds.iter().find_map(|c| match c {
            Cmd::WritePrimary(t) => Some(t.clone()),
            _ => None,
        });
        assert_eq!(
            text.as_deref(),
            Some("L0\nL1\nL2\nL3\nL4\nL5\nL6"),
            "the drag selected history that was never in the original viewport"
        );
    }

    #[test]
    fn dragging_below_the_bottom_edge_autoscrolls_back_toward_live() {
        let mut m = model();
        feed_lines(&mut m, 30);
        // Scroll all the way into history, anchor on L0, then hover below the grid.
        for _ in 0..2 {
            m.update(wheel(3.0));
        }
        assert_eq!(top_row_text(&m), "L0");
        begin_drag(&mut m, 1.0, 1.0);
        let cmds = m.update(ptr(
            PointerPhase::Motion,
            Some(PointerButton::Left),
            20.0,
            24.0 * 18.0 + 1.0,
        ));
        assert!(
            cmds.iter().any(|c| matches!(c, Cmd::ScheduleTick { .. })),
            "hovering the bottom edge schedules autoscroll ticks: {cmds:?}"
        );
        tick(&mut m);
        assert_eq!(top_row_text(&m), "L1", "the tick scrolled back toward live");
        // The anchor row (L0) is now above the window: the painted selection
        // clamps to the window top while the real range is preserved.
        let sel = m.selection().expect("the drag still shows a selection");
        assert_eq!(
            sel.start,
            (0, 0),
            "the painted selection clamps to the window"
        );
        // Drain the remaining distance and copy: everything from L0 down.
        for _ in 0..8 {
            tick(&mut m);
        }
        assert_eq!(top_row_text(&m), "L6");
        let cmds = m.update(ptr(
            PointerPhase::Release,
            Some(PointerButton::Left),
            20.0,
            24.0 * 18.0 + 1.0,
        ));
        let text = cmds
            .iter()
            .find_map(|c| match c {
                Cmd::WritePrimary(t) => Some(t.clone()),
                _ => None,
            })
            .expect("the finished drag publishes the primary selection");
        assert!(
            text.starts_with("L0\nL1\n") && text.contains("L28"),
            "the copy spans from the anchor in history down past the old window: {text:?}"
        );
    }

    #[test]
    fn autoscroll_stops_on_release() {
        let mut m = model();
        feed_lines(&mut m, 30);
        begin_drag(&mut m, 20.0, 1.0);
        m.update(ptr(
            PointerPhase::Motion,
            Some(PointerButton::Left),
            20.0,
            -1.0,
        ));
        m.update(ptr(
            PointerPhase::Release,
            Some(PointerButton::Left),
            20.0,
            -1.0,
        ));
        let before = top_row_text(&m);
        let cmds = tick(&mut m);
        assert_eq!(top_row_text(&m), before, "no scrolling after release");
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::ScheduleTick { .. })),
            "released autoscroll does not reschedule: {cmds:?}"
        );
    }

    #[test]
    fn wheel_scrolling_mid_drag_extends_the_selection() {
        let mut m = model();
        feed_lines(&mut m, 30);
        begin_drag(&mut m, 20.0, 1.0); // anchor on L6 (top row), col 2
        m.update(ptr(
            PointerPhase::Motion,
            Some(PointerButton::Left),
            1.0,
            1.0,
        ));
        // Wheel up during the drag: the viewport scrolls and the selection
        // follows the pointer over the revealed content instead of being stuck.
        let cmds = m.update(wheel(3.0));
        assert!(
            cmds.contains(&Cmd::Redraw),
            "the wheel scrolls mid-drag: {cmds:?}"
        );
        assert_eq!(top_row_text(&m), "L3");
        let sel = m.selection().expect("the selection survived the scroll");
        assert_eq!(
            (sel.start.0, sel.end.0),
            (0, 3),
            "the selection runs from the pointer (L3, window row 0) to the anchor (L6)"
        );
    }

    #[test]
    fn selection_text_extracts_and_trims() {
        let mut screen = Screen::new(20, 3, screen::DEFAULT_SCROLLBACK);
        screen.feed(b"hello world");
        assert_eq!(
            selection_text(&screen, Selection::new((0, 0), (0, 4))),
            "hello"
        );

        let mut screen = Screen::new(20, 3, screen::DEFAULT_SCROLLBACK);
        screen.feed(b"a  b");
        // Trailing spaces on the terminating row are kept (WYSIWYG copy).
        assert_eq!(
            selection_text(&screen, Selection::new((0, 0), (0, 2))),
            "a  "
        );
    }
}
