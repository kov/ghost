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

use ghost_render::{Layer, RectPx, Scene, SceneId, SceneItem, Selection, layout_frame};
use ghost_term::MouseProtocol;
use ghost_vt::query::QueryScanner;
use ghost_vt::screen::{self, Screen};

use crate::input::{Key, Mods};
use crate::{
    CellMetrics, Cmd, PointPx, PointerButton, PointerPhase, SessionId, UiEvent, encode, mouse,
};

/// A frontend-handled key combo (Super+key, or Ctrl+Shift+key) intercepted
/// before encoding so it drives the app, not the child.
pub enum Shortcut {
    Paste,
    Copy,
}

/// Classify a pressed key as a paste/copy shortcut, if it is one.
pub fn classify_shortcut(key: &Key, mods: Mods) -> Option<Shortcut> {
    let combo = mods.sup || (mods.ctrl && mods.shift);
    if !combo {
        return None;
    }
    match key {
        Key::Char(s) if s.eq_ignore_ascii_case("v") => Some(Shortcut::Paste),
        Key::Char(s) if s.eq_ignore_ascii_case("c") => Some(Shortcut::Copy),
        _ => None,
    }
}

/// One terminal view's reducer state.
pub struct TerminalModel {
    session: SessionId,
    metrics: CellMetrics,
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
    selection: Option<Selection>,
    ended: bool,
}

impl TerminalModel {
    pub fn new(session: SessionId, cols: u16, rows: u16, metrics: CellMetrics) -> Self {
        let size_px = (
            (f32::from(cols) * metrics.advance) as u32,
            (f32::from(rows) * metrics.line_height) as u32,
        );
        TerminalModel {
            session,
            metrics,
            size_px,
            screen: Screen::new(cols, rows, screen::DEFAULT_SCROLLBACK),
            scanner: QueryScanner::new(),
            cols,
            rows,
            cursor_cell: None,
            held: None,
            gesture_report: false,
            sel_anchor: None,
            selection: None,
            ended: false,
        }
    }

    pub fn screen(&self) -> &Screen {
        &self.screen
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
            UiEvent::Key { key, mods, pressed } => self.key(&key, mods, pressed),
            UiEvent::Text(s) => self.text(&s),
            UiEvent::Pointer {
                phase,
                button,
                pos,
                mods,
                wheel_dy,
            } => self.pointer(phase, button, pos, mods, wheel_dy),
            UiEvent::Focus(focused) => self.focus(focused),
            UiEvent::Resize { w_px, h_px, .. } => self.resize(w_px, h_px),
            UiEvent::ClipboardText(text) => self.paste(text),
            UiEvent::SessionData { name, bytes, ended } => self.session_data(&name, &bytes, ended),
            // A lone terminal ignores enumeration and the clock (no animation yet).
            UiEvent::SessionList(_) | UiEvent::Tick { .. } => Vec::new(),
        }
    }

    /// Render the current state to a single full-window terminal scene.
    pub fn view(&self) -> Scene {
        let frame = layout_frame(self.screen.vt(), self.metrics);
        let rect = RectPx {
            x: 0.0,
            y: 0.0,
            w: self.size_px.0 as f32,
            h: self.size_px.1 as f32,
        };
        let mut scene = Scene::new(self.size_px);
        scene.layers.push(Layer {
            z: 0,
            items: vec![SceneItem::Terminal {
                id: SceneId::Root,
                rect,
                frame,
                selection: self.selection,
                dim: false,
            }],
        });
        scene
    }

    fn send(&self, bytes: Vec<u8>) -> Vec<Cmd> {
        vec![Cmd::SendInput {
            session: self.session.clone(),
            bytes,
        }]
    }

    fn key(&mut self, key: &Key, mods: Mods, pressed: bool) -> Vec<Cmd> {
        if !pressed {
            return Vec::new();
        }
        match classify_shortcut(key, mods) {
            Some(Shortcut::Paste) => vec![Cmd::ReadClipboard],
            Some(Shortcut::Copy) => self.copy(),
            None => {
                let app_cursor = self.screen.vt().cursor_key_app_mode();
                match encode::encode(key, mods, app_cursor) {
                    Some(bytes) => self.send(bytes),
                    None => Vec::new(),
                }
            }
        }
    }

    fn text(&self, s: &str) -> Vec<Cmd> {
        if s.is_empty() {
            Vec::new()
        } else {
            self.send(s.as_bytes().to_vec())
        }
    }

    /// Paste reply from the shell: wrap with bracketed-paste markers if enabled.
    fn paste(&self, text: Option<String>) -> Vec<Cmd> {
        match text {
            Some(s) => {
                let bytes = bracket_paste(s.as_bytes(), self.screen.vt().bracketed_paste());
                self.send(bytes)
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

    fn focus(&self, focused: bool) -> Vec<Cmd> {
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

    fn resize(&mut self, w_px: u32, h_px: u32) -> Vec<Cmd> {
        self.size_px = (w_px, h_px);
        let cols = (w_px as f32 / self.metrics.advance).floor().max(1.0) as u16;
        let rows = (h_px as f32 / self.metrics.line_height).floor().max(1.0) as u16;
        if (cols, rows) == (self.cols, self.rows) {
            return vec![Cmd::Redraw];
        }
        self.cols = cols;
        self.rows = rows;
        self.screen.resize(cols, rows);
        // Reflow invalidates cell coordinates; drop any stale selection.
        self.selection = None;
        self.sel_anchor = None;
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
            self.screen.feed(bytes);
            let cursor = self.screen.cursor();
            let size = self.screen.dimensions();
            let replies = query_replies(&mut self.scanner, bytes, cursor, size);
            if !replies.is_empty() {
                cmds.push(Cmd::SendInput {
                    session: self.session.clone(),
                    bytes: replies,
                });
            }
            // Output moved the viewport; a viewport-relative selection no longer
            // maps to what the user picked, so drop it — unless a drag is live.
            if self.held.is_none() {
                self.selection = None;
                self.sel_anchor = None;
            }
            cmds.push(Cmd::Redraw);
        }
        if ended {
            self.ended = true;
            cmds.push(Cmd::Redraw);
        }
        cmds
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

    /// 1-based `(col, row)` cell under a pointer position.
    fn point_to_cell(&self, pos: PointPx) -> (u16, u16) {
        let col = (pos.x / f64::from(self.metrics.advance)).floor().max(0.0) as u16 + 1;
        let row = (pos.y / f64::from(self.metrics.line_height))
            .floor()
            .max(0.0) as u16
            + 1;
        (col, row)
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
    ) -> Vec<Cmd> {
        match phase {
            PointerPhase::Motion => {
                let cell = self.point_to_cell(pos);
                if self.cursor_cell == Some(cell) {
                    return Vec::new();
                }
                self.cursor_cell = Some(cell);
                if let Some(b) = self.held {
                    if self.gesture_report {
                        self.mouse_report(mouse::Kind::Motion, Some(b), true, cell, mods)
                    } else if b == mouse::Button::Left
                        && let Some(anchor) = self.sel_anchor
                    {
                        self.selection = Some(Selection::new(anchor, self.pointer_cell0()));
                        vec![Cmd::Redraw]
                    } else {
                        Vec::new()
                    }
                } else if self.report_to_app(mods) {
                    self.mouse_report(mouse::Kind::Motion, None, false, cell, mods)
                } else {
                    Vec::new()
                }
            }
            PointerPhase::Press => {
                let Some(b) = button.map(map_button) else {
                    return Vec::new();
                };
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
                    // Begin a local selection (only anchor once the pointer is known).
                    self.sel_anchor = self.cursor_cell.map(|_| self.pointer_cell0());
                    self.selection = None;
                    vec![Cmd::Redraw]
                } else {
                    Vec::new()
                }
            }
            PointerPhase::Release => {
                let cmds = match button.map(map_button) {
                    Some(b) if self.gesture_report => {
                        let cell = self.cursor_cell.unwrap_or((1, 1));
                        self.mouse_report(mouse::Kind::Release, Some(b), false, cell, mods)
                    }
                    _ => Vec::new(),
                };
                self.held = None;
                cmds
            }
            PointerPhase::Wheel => {
                if !self.report_to_app(mods) || wheel_dy == 0.0 {
                    return Vec::new();
                }
                let b = if wheel_dy > 0.0 {
                    mouse::Button::WheelUp
                } else {
                    mouse::Button::WheelDown
                };
                let cell = self.cursor_cell.unwrap_or((1, 1));
                self.mouse_report(mouse::Kind::Press, Some(b), self.held.is_some(), cell, mods)
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

// ---- pure protocol helpers (shared with the shell) ----

/// Scan child output for terminal queries and build the reply bytes from the
/// 1-based `(col, row)` cursor and `(cols, rows)` size. Pure.
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

/// Extract the text covered by `sel` from `screen`, one line per row joined by
/// newlines. Wide-cell tail placeholders are dropped; the terminating row keeps
/// its trailing spaces (selected content) while earlier rows are trimmed.
pub fn selection_text(screen: &Screen, sel: Selection) -> String {
    let (cols, rows) = screen.dimensions();
    let (cols, rows) = (cols as usize, rows as usize);
    let mut lines: Vec<String> = Vec::new();
    for row in sel.start.0..=sel.end.0 {
        if row >= rows {
            break;
        }
        let text = match sel.row_span(row, cols) {
            Some((c0, c1)) => {
                let line = screen.vt().line(row);
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
            pressed: true,
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
        }
    }

    fn sent(session: &str, bytes: &[u8]) -> Cmd {
        Cmd::SendInput {
            session: session.to_string(),
            bytes: bytes.to_vec(),
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
                pressed: false
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

    // ---- moved pure-helper tests ----

    #[test]
    fn query_replies_answers_cursor_position() {
        let mut s = QueryScanner::new();
        assert_eq!(
            query_replies(&mut s, b"\x1b[6n", (3, 5), (80, 24)),
            b"\x1b[5;3R"
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
