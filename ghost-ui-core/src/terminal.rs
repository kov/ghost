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
use ghost_term::terminal::Cursor;
use ghost_term::{ClipboardSelection, Line, MouseProtocol};
use ghost_vt::query::{QueryScanner, ReplyCtx, ThemeColors};
use ghost_vt::screen::{self, Screen};

use std::collections::HashSet;

use crate::input::{Key, KeyAlternates, KeyEventKind, Mods, NamedKey};
use crate::{
    CellMetrics, Cmd, PointPx, PointerButton, PointerIcon, PointerPhase, SessionId, UiEvent,
    encode, mouse,
};

/// Lines moved per mouse-wheel notch when scrolling local scrollback.
const SCROLL_LINES: i64 = 3;

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

    // Copy/paste/new-window are also on Alt on Linux (in addition to the Ctrl+Shift
    // chord below) — a terminal-app convention that keeps Ctrl free for the shell.
    // Like Alt+T above, these must resolve here rather than be encoded and sent to the
    // child as Meta+<key>; only C/V/N are taken, so other Alt+key motions (Alt+B/F, …)
    // still reach the child. macOS keeps Alt = Option/Meta and uses Cmd for these.
    if !cfg!(target_os = "macos") && mods.alt && !mods.sup && !mods.ctrl {
        match key {
            Key::Char(s) if s.eq_ignore_ascii_case("c") => return Some(Shortcut::Copy),
            Key::Char(s) if s.eq_ignore_ascii_case("v") => return Some(Shortcut::Paste),
            Key::Char(s) if s.eq_ignore_ascii_case("n") => return Some(Shortcut::NewWindow),
            _ => {}
        }
    }

    let primary = mods.sup || mods.ctrl;
    if !primary {
        return None;
    }
    if mods.sup || mods.shift {
        match key {
            Key::Char(s) if s.eq_ignore_ascii_case("v") => return Some(Shortcut::Paste),
            Key::Char(s) if s.eq_ignore_ascii_case("c") => return Some(Shortcut::Copy),
            // Cmd+Q (macOS) / Ctrl+Shift+Q (elsewhere) quits — never bare Ctrl+Q,
            // which must stay XOFF flow control.
            Key::Char(s) if s.eq_ignore_ascii_case("q") => return Some(Shortcut::Quit),
            // Window management, same Cmd / Ctrl+Shift gating. Bare Ctrl+N/W stay
            // terminal input.
            Key::Char(s) if s.eq_ignore_ascii_case("n") => return Some(Shortcut::NewWindow),
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
    /// Selection anchor while dragging, 0-based `(row, col)`.
    sel_anchor: Option<(usize, usize)>,
    /// Granularity of the active drag (cell / word / line), latched at press.
    sel_mode: SelectMode,
    selection: Option<Selection>,
    /// Lines scrolled up into history; 0 = pinned to the live bottom.
    scroll_offset: usize,
    /// In-progress IME composition string; non-empty means composing, during
    /// which raw key input is suppressed.
    preedit: String,
    /// Last window title pushed to the shell, to emit `SetTitle` only on change.
    last_title: String,
    /// kitty-graphics image ids whose pixels have been uploaded to the renderer,
    /// so the (potentially large) blob is sent once rather than every feed.
    uploaded_images: HashSet<u32>,
    /// Count of stored graphics images at the last feed. When it grows, a newly
    /// stored image may be referenced by a placeholder that has already scrolled
    /// out of the live viewport, so we rescan all retained lines (not just the
    /// viewport) for placeholder ids to upload.
    last_image_count: usize,
    ended: bool,
    /// Viewport rows dirtied by feeds since the last present, from the core's per-feed
    /// hint (`Screen::feed`) — the localizable part of the [`TermDamage`] `view` reports.
    /// `None` = no feed changed the viewport; a range accumulates across coalesced feeds.
    feed_dirty: Option<(usize, usize)>,
    /// The view-shaping state at the last present. `view` reports `TermDamage::All` when
    /// any of it moved (scroll, selection, resize, zoom, HiDPI scale) — changes a per-row
    /// feed hint can't localize — and otherwise reports just `feed_dirty`. `None` until
    /// the first present, so the first frame is always `All`.
    presented: Option<Presented>,
    /// A repaint is being held back because the app is mid synchronized-output
    /// frame (DEC mode 2026). Released by the mode resetting, or by the tick
    /// scheduled when the hold began (so a stuck app can't freeze the window).
    sync_held: bool,
    /// The scheme's default fg/bg, for answering OSC 10/11 color queries (vim
    /// and fzf theme detection). Defaults to ghost's default scheme; the shell
    /// overrides it when a scheme is configured (see `set_theme`).
    theme: ThemeColors,
    /// The interned link id under a Ctrl/Cmd-hover, if any: `view` underlines
    /// every visible run of it and the pointer shows a hand (see
    /// [`Cmd::PointerIcon`]). Updated on pointer motion.
    hovered_link: Option<u16>,
    /// The text cursor (0-based, with visibility and shape) as of the previous feed.
    /// Moving the cursor writes no cell, so a bare CUP/CUF — common in full-screen
    /// apps whose differential renderers reposition without rewriting — leaves
    /// `Screen::feed` reporting no dirty row. Diffing against this makes a cursor
    /// change its own damage (the row it left + the row it entered) and repaint
    /// trigger, so the block never lags the child (the "space doesn't advance the
    /// cursor" jank). Visibility and shape count too: hiding or reshaping the cursor
    /// changes the drawn frame without touching a cell.
    prev_cursor: Cursor,
}

/// How long a synchronized-output hold may last before the scheduled tick
/// releases it anyway. Generous for an atomic repaint burst, short enough that
/// an app dying between BSU and ESU reads as a hiccup, not a hang.
const SYNC_RELEASE_MS: u64 = 150;

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

impl TerminalModel {
    pub fn new(session: SessionId, cols: u16, rows: u16, metrics: CellMetrics) -> Self {
        let size_px = (
            (f32::from(cols) * metrics.advance) as u32,
            (f32::from(rows) * metrics.line_height) as u32,
        );
        let screen = Screen::new(cols, rows, screen::DEFAULT_SCROLLBACK);
        let prev_cursor = screen.vt().cursor();
        TerminalModel {
            session,
            metrics,
            scale: 1.0,
            zoom: 1.0,
            size_px,
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
            scroll_offset: 0,
            preedit: String::new(),
            last_title: String::new(),
            uploaded_images: HashSet::new(),
            last_image_count: 0,
            ended: false,
            feed_dirty: None,
            presented: None,
            sync_held: false,
            theme: ThemeColors::default(),
            hovered_link: None,
            prev_cursor,
        }
    }

    /// Set the scheme's default fg/bg reported to apps that query them
    /// (OSC 10/11). Called once per model right after construction; on a real
    /// theme *change*, sessions subscribed to mode 2031 get the unsolicited
    /// `CSI ? 997 ; Ps n` dark/light notification.
    pub fn set_theme(&mut self, theme: ThemeColors) -> Vec<Cmd> {
        let changed = theme != self.theme;
        self.theme = theme;
        if changed && self.screen.vt().dec_mode_state(2031) == Some(true) {
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
        let moved = match &self.presented {
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
                Some((lo, hi)) => TermDamage::Rows { lo, hi },
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

    /// The terminal's grid size in cells (cols, rows).
    pub fn dims(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    pub fn selection(&self) -> Option<Selection> {
        self.selection
    }

    /// Whether the child exited / the session closed.
    pub fn ended(&self) -> bool {
        self.ended
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
            UiEvent::ClipboardText(text) => self.paste(text),
            UiEvent::SessionData { name, bytes, ended } => self.session_data(&name, &bytes, ended),
            // The clock only matters as the synchronized-output release
            // backstop: present what accumulated if a hold is still open.
            UiEvent::Tick { .. } => {
                if std::mem::take(&mut self.sync_held) {
                    vec![Cmd::Redraw]
                } else {
                    Vec::new()
                }
            }
            // A lone terminal ignores enumeration, and never sees
            // `AdoptSession` — `RootModel` handles it.
            UiEvent::SessionList(_) | UiEvent::AdoptSession(_) => Vec::new(),
        }
    }

    /// Combined render scale: device scale × user zoom. The shell multiplies the
    /// base font size by this to rasterize glyphs at the same size the grid is
    /// laid out for, keeping the two in lockstep.
    pub fn render_scale(&self) -> f32 {
        self.scale * self.zoom
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

    /// Physical-pixel rect of the text cursor, for positioning the IME candidate
    /// window. `None` while scrolled into history (no live cursor is shown).
    pub fn ime_cursor_area(&self) -> Option<RectPx> {
        if self.scroll_offset != 0 {
            return None;
        }
        let (col1, row1) = self.screen.cursor();
        let m = self.effective_metrics();
        Some(RectPx {
            x: f32::from(col1.saturating_sub(1)) * m.advance,
            y: f32::from(row1.saturating_sub(1)) * m.line_height,
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

    /// Render the current state to a single full-window terminal scene.
    pub fn view(&self) -> Scene {
        let frame = std::rc::Rc::new(layout_frame_at(
            self.screen.vt(),
            self.effective_metrics(),
            self.scroll_offset,
        ));
        let rect = RectPx {
            x: 0.0,
            y: 0.0,
            w: self.size_px.0 as f32,
            h: self.size_px.1 as f32,
        };
        let mut items = vec![SceneItem::Terminal {
            id: SceneId::Root,
            session: ghost_render::session_key(self.session()),
            rect,
            frame,
            selection: self.selection,
            dim: false,
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
            // Don't move the viewport out from under an in-progress drag: the
            // selection is window-relative, so scrolling would retarget it.
            if self.held.is_some() {
                return Vec::new();
            }
            return match scroll {
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
        }
        if let Some(back) = self.prompt_jump_key(key, mods) {
            // Same drag guard as scroll_key: a moving viewport would retarget
            // an in-progress window-relative selection.
            if self.held.is_some() {
                return Vec::new();
            }
            return self.jump_to_prompt(back);
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
                let text = selection_text(&self.screen, sel, self.scroll_offset);
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
        let cols = (w_px as f32 / m.advance).floor().max(1.0) as u16;
        let rows = (h_px as f32 / m.line_height).floor().max(1.0) as u16;
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
            let before = self.screen.vt().lines_scrolled_off();
            let colors_before = self.render_colors();
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
            // Keep a scrolled-up view pinned to its content: advance the offset by
            // the GROSS lines that scrolled off the top this feed. That count
            // survives scrollback trimming (unlike the net scrollback_len delta,
            // which reads zero once the cap is hit), clamped to retained history.
            // At the bottom (offset 0) we just follow the live output.
            if self.scroll_offset > 0 {
                let pushed = self.screen.vt().lines_scrolled_off().saturating_sub(before);
                self.scroll_offset = (self.scroll_offset + pushed).min(self.max_scroll());
            }
            let screen = &self.screen;
            let mode_state = |m: u16| screen.vt().dec_mode_state(m);
            let ctx = ReplyCtx {
                cursor: screen.cursor(),
                size: screen.dimensions(),
                kitty_flags: screen.kitty_keyboard_flags(),
                cursor_style: ghost_vt::query::decscusr_digit(screen.vt().cursor().shape),
                colors: screen.effective_colors(self.theme),
                mode_state: &mode_state,
            };
            let replies = query_replies(&mut self.scanner, bytes, &ctx);
            if !replies.is_empty() {
                cmds.push(Cmd::SendInput {
                    session: self.session.clone(),
                    bytes: replies,
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
            if self.screen.title() != self.last_title.as_str() {
                self.last_title = self.screen.title().to_string();
                cmds.push(Cmd::SetTitle(self.last_title.clone()));
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
            // App-set dynamic colors (OSC 10/11/12) dirty no rows, but they
            // recolor everything; `damage` reports All via the `Presented`
            // snapshot — this only makes sure a repaint is actually requested.
            let colors_changed = colors_before != self.render_colors();
            // The cursor is part of the drawn frame, but moving it writes no cell, so a
            // bare CUP/CUF (how full-screen apps like an editor or Claude Code reposition
            // between keystrokes) leaves `Screen::feed` reporting no dirty row. Treat a
            // change in the *drawn* cursor as its own damage so the block never lags the
            // child: dirty the row it left and the row it entered, and force a repaint.
            // Only matters at the live bottom — scrolled into history the cursor isn't
            // drawn, and a scroll is already a full repaint. Visibility and shape count
            // too: hiding or reshaping the cursor changes the frame without touching a
            // cell. Always advance the baseline so the next feed measures from here.
            let cursor_redrawn = if self.scroll_offset == 0 {
                let now = self.screen.vt().cursor();
                let prev = std::mem::replace(&mut self.prev_cursor, now);
                if now != prev {
                    // Clamp to the live viewport: `prev` can name a row past the bottom
                    // after a shrink that reflowed the cursor up, and the band feeds the
                    // renderer's row range directly.
                    let max_row = self.rows.saturating_sub(1) as usize;
                    if prev.visible {
                        let r = prev.row.min(max_row);
                        self.accumulate_dirty(r, r);
                    }
                    if now.visible {
                        let r = now.row.min(max_row);
                        self.accumulate_dirty(r, r);
                    }
                    // A repaint is only needed when the block was or is drawn; a move that
                    // stays hidden the whole time paints nothing.
                    prev.visible || now.visible
                } else {
                    false
                }
            } else {
                self.prev_cursor = self.screen.vt().cursor();
                false
            };
            let want_redraw = viewport_changed
                || selection_dropped
                || images_added
                || colors_changed
                || cursor_redrawn;
            // Synchronized output (DEC 2026): between set and reset the app is
            // composing one atomic frame, so hold the repaint (damage keeps
            // accumulating above) and schedule a release tick as the backstop.
            // Any tick releases the hold — an early animation tick just means
            // one mid-frame paint, the status quo without the mode.
            let sync = self.screen.vt().synchronized_output();
            if sync && want_redraw && !self.sync_held {
                self.sync_held = true;
                cmds.push(Cmd::ScheduleTick {
                    after_ms: SYNC_RELEASE_MS,
                });
            }
            if !sync && (want_redraw || std::mem::take(&mut self.sync_held)) {
                cmds.push(Cmd::Redraw);
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
    fn upload_new_images(&mut self, cmds: &mut Vec<Cmd>) {
        let mut fresh: Vec<u32> = Vec::new();
        for p in self.screen.vt().graphics_placements() {
            let id = p.image_id;
            if !self.uploaded_images.contains(&id) && !fresh.contains(&id) {
                fresh.push(id);
            }
        }
        // Placeholder cells reference an image by id without a direct placement.
        // Normally scan just the live viewport; but when a new image was just
        // stored, also scan the retained scrollback, since the image may belong to
        // a placeholder that already scrolled out of view (otherwise it would never
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
            if !self.uploaded_images.contains(&id) && !fresh.contains(&id) {
                fresh.push(id);
            }
        }
        for id in fresh {
            if let Some(image) = self.screen.vt().graphics_image(id) {
                cmds.push(Cmd::UploadImage {
                    id,
                    width: image.width,
                    height: image.height,
                    rgba: image.pixels.clone(),
                });
                self.uploaded_images.insert(id);
            }
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

    /// 1-based `(col, row)` cell under a pointer position. Pointer coordinates
    /// are physical pixels, so they divide by the physical (scaled) metrics.
    fn point_to_cell(&self, pos: PointPx) -> (u16, u16) {
        let m = self.effective_metrics();
        let col = (pos.x / f64::from(m.advance)).floor().max(0.0) as u16 + 1;
        let row = (pos.y / f64::from(m.line_height)).floor().max(0.0) as u16 + 1;
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

    /// Extend a drag selection from `anchor` to `active` (both 0-based viewport
    /// `(row, col)`) at the latched granularity: by cell, or growing to cover the
    /// whole words / lines that contain both endpoints. On a blank cell (no word
    /// or line) the endpoint degrades to that single cell.
    fn extend_selection(
        &self,
        anchor: (usize, usize),
        active: (usize, usize),
    ) -> Option<Selection> {
        match self.sel_mode {
            SelectMode::Char => Some(Selection::new(anchor, active)),
            SelectMode::Word => {
                let a = self
                    .word_at(anchor.0, anchor.1)
                    .unwrap_or_else(|| Selection::new(anchor, anchor));
                let b = self
                    .word_at(active.0, active.1)
                    .unwrap_or_else(|| Selection::new(active, active));
                Some(Selection {
                    start: a.start.min(b.start),
                    end: a.end.max(b.end),
                })
            }
            SelectMode::Line => {
                let a = self
                    .line_at(anchor.0)
                    .unwrap_or_else(|| Selection::new(anchor, anchor));
                let b = self
                    .line_at(active.0)
                    .unwrap_or_else(|| Selection::new(active, active));
                Some(Selection {
                    start: a.start.min(b.start),
                    end: a.end.max(b.end),
                })
            }
        }
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
                        self.selection = self.extend_selection(anchor, self.pointer_cell0());
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
                        // latches that granularity so a drag extends by it.
                        let (row, col) = self.pointer_cell0();
                        self.sel_anchor = Some((row, col));
                        self.sel_mode = if clicks == 2 {
                            SelectMode::Word
                        } else {
                            SelectMode::Line
                        };
                        self.selection = if clicks == 2 {
                            self.word_at(row, col)
                        } else {
                            self.line_at(row)
                        };
                    } else {
                        // Begin a by-cell drag selection (anchor once the pointer
                        // is known).
                        self.sel_mode = SelectMode::Char;
                        self.sel_anchor = self.cursor_cell.map(|_| self.pointer_cell0());
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
                // A finalized local selection becomes the primary selection, so a
                // middle-click elsewhere pastes it (X11/Wayland convention).
                if let Some(sel) = self.selection {
                    let text = selection_text(&self.screen, sel, self.scroll_offset);
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
                } else if self.held.is_some() {
                    // A drag is in progress: don't scroll the viewport out from
                    // under the (window-relative) selection.
                    Vec::new()
                } else {
                    // Otherwise scroll local scrollback (up = into history).
                    let delta = if wheel_dy > 0.0 {
                        SCROLL_LINES
                    } else {
                        -SCROLL_LINES
                    };
                    self.scroll_by(delta)
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
/// `scroll_offset` lines into history (0 = live), one line per row joined by
/// newlines. Selection rows are relative to the *visible* window, so copying
/// while scrolled back yields the history the user sees, not the live viewport.
/// Wide-cell tail placeholders are dropped; the terminating row keeps its
/// trailing spaces (selected content) while earlier rows are trimmed.
pub fn selection_text(screen: &Screen, sel: Selection, scroll_offset: usize) -> String {
    let (cols, _rows) = screen.dimensions();
    let cols = cols as usize;
    let window: Vec<&Line> = screen.vt().view_at(scroll_offset).collect();
    let mut lines: Vec<String> = Vec::new();
    for row in sel.start.0..=sel.end.0 {
        let Some(line) = window.get(row) else {
            break;
        };
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
    fn quit_shortcut_is_cmd_q_or_ctrl_shift_q_never_bare_ctrl_q() {
        let mut m = model();
        assert_eq!(
            key(&mut m, Key::Char("q".into()), Mods::SUPER),
            vec![Cmd::Quit],
            "Cmd+Q quits"
        );
        assert_eq!(
            key(&mut m, Key::Char("q".into()), Mods::CTRL | Mods::SHIFT),
            vec![Cmd::Quit],
            "Ctrl+Shift+Q quits"
        );
        // Bare Ctrl+Q must stay XOFF flow control (0x11), not quit.
        assert_eq!(
            key(&mut m, Key::Char("q".into()), Mods::CTRL),
            vec![sent("alpha", b"\x11")],
            "bare Ctrl+Q is XOFF, not quit"
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

        // More held content: still no redraw, and the timeout is not re-armed.
        let cmds = m.update(UiEvent::SessionData {
            name: "alpha".to_string(),
            bytes: b" world".to_vec(),
            ended: false,
        });
        assert!(!cmds.contains(&Cmd::Redraw), "redraw leaked: {cmds:?}");
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::ScheduleTick { .. })),
            "timeout re-armed mid-hold: {cmds:?}"
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
    fn scrolling_is_suppressed_during_an_active_drag() {
        let mut m = model();
        feed_lines(&mut m, 100);
        m.update(wheel(1.0)); // top row "L73"
        // Begin a left-drag selecting "L73".
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
        // A wheel mid-drag must not move the viewport out from under the selection.
        assert!(
            !m.update(wheel(1.0)).contains(&Cmd::Redraw),
            "wheel is ignored while a drag is held"
        );
        // Shift+PageUp mid-drag is likewise ignored.
        m.update(UiEvent::Key {
            key: Key::Named(NamedKey::PageUp),
            mods: Mods::SHIFT,
            kind: KeyEventKind::Press,
            alts: None,
        });
        m.update(ptr(
            PointerPhase::Release,
            Some(PointerButton::Left),
            18.0,
            1.0,
        ));
        assert_eq!(
            key(&mut m, Key::Char("c".into()), Mods::CTRL | Mods::SHIFT),
            vec![Cmd::WriteClipboard("L73".to_string())],
            "copy reads exactly the dragged line, not a scrolled-away one"
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
    fn no_modes(_: u16) -> Option<bool> {
        None
    }

    /// A baseline reply context; tests override the fields they exercise.
    fn reply_ctx() -> ReplyCtx<'static> {
        ReplyCtx {
            cursor: (1, 1),
            size: (80, 24),
            kitty_flags: 0,
            cursor_style: 2,
            colors: ThemeColors::default(),
            mode_state: &no_modes,
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

    #[test]
    fn selection_text_extracts_and_trims() {
        let mut screen = Screen::new(20, 3, screen::DEFAULT_SCROLLBACK);
        screen.feed(b"hello world");
        assert_eq!(
            selection_text(&screen, Selection::new((0, 0), (0, 4)), 0),
            "hello"
        );

        let mut screen = Screen::new(20, 3, screen::DEFAULT_SCROLLBACK);
        screen.feed(b"a  b");
        // Trailing spaces on the terminating row are kept (WYSIWYG copy).
        assert_eq!(
            selection_text(&screen, Selection::new((0, 0), (0, 2)), 0),
            "a  "
        );
    }
}
