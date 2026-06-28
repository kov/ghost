//! `RootModel` — the top of the model tree: either the single-terminal view or
//! the fleet overview, with one key (F9) toggling between them.
//!
//! Toggling preserves session state: every session this window drives stays
//! live. Going to the fleet hands the foreground *and* the warm background
//! mirrors to the grid as fed tiles; coming back extracts the chosen one as the
//! foreground and keeps the rest as warm mirrors (fed and resized like the
//! foreground), so previews never go cold and Ctrl-Tab switches are instant. The
//! shell drives this model exactly as it drove a bare `TerminalModel` — `update`
//! in, `Cmd`s out, `view` to draw — so the whole tree stays headlessly testable.

use std::collections::{HashMap, HashSet};

use crate::input::{Key, Mods, NamedKey};
use crate::terminal::{Shortcut, classify_shortcut};
use crate::{CellMetrics, Cmd, FleetModel, Scene, SessionId, TerminalModel, UiEvent};

enum Mode {
    Single(Box<TerminalModel>),
    Fleet(Box<FleetModel>),
}

pub struct RootModel {
    mode: Mode,
    metrics: CellMetrics,
    size_px: (u32, u32),
    /// Device scale factor, tracked so a fleet toggle preserves HiDPI sizing.
    scale: f32,
    /// Sessions this window owns, for fleet grouping (this-window vs elsewhere).
    mine: HashSet<SessionId>,
    /// The session the single view shows / a fleet toggle returns to. `None` for
    /// a freshly-opened fleet window that hasn't adopted a session yet.
    primary: Option<SessionId>,
    /// Live mirrors of the window's *background* sessions while in the single
    /// view (the foreground lives in `mode`). They are fed and resized exactly
    /// like the foreground, so their previews stay live and Ctrl-Tab switches are
    /// instant and correctly sized. In the fleet, the models live in its tiles, so
    /// this is empty.
    warm: HashMap<SessionId, TerminalModel>,
}

/// Resize a model to the window (physical px + scale), returning its commands.
fn resize_model(m: &mut TerminalModel, size_px: (u32, u32), scale: f32) -> Vec<Cmd> {
    m.update(UiEvent::Resize {
        w_px: size_px.0.max(1),
        h_px: size_px.1.max(1),
        scale: scale as f64,
    })
}

/// F9 toggles the fleet overview.
fn is_fleet_toggle(key: &Key) -> bool {
    matches!(key, Key::Named(NamedKey::F9))
}

/// Ctrl-Tab / Ctrl-Shift-Tab cycle the window's foreground among its attached
/// sessions: `Some(true)` forward, `Some(false)` backward.
fn cycle_dir(key: &Key, mods: Mods) -> Option<bool> {
    if matches!(key, Key::Named(NamedKey::Tab)) && mods.ctrl {
        Some(!mods.shift)
    } else {
        None
    }
}

impl RootModel {
    /// Start in the single-terminal view around `model`.
    pub fn single(model: TerminalModel, metrics: CellMetrics, size_px: (u32, u32)) -> Self {
        let id = model.session().to_string();
        RootModel {
            mode: Mode::Single(Box::new(model)),
            metrics,
            size_px,
            scale: 1.0,
            mine: HashSet::from([id.clone()]),
            primary: Some(id),
            warm: HashMap::new(),
        }
    }

    /// Start in the fleet overview owning no session — a freshly-opened window.
    /// The returned commands enumerate existing sessions to populate the grid
    /// (the reconcile reply re-arms the periodic refresh).
    pub fn fleet(metrics: CellMetrics, size_px: (u32, u32), scale: f32) -> (Self, Vec<Cmd>) {
        let root = RootModel {
            mode: Mode::Fleet(Box::new(FleetModel::new(metrics, size_px, HashSet::new()))),
            metrics,
            size_px,
            scale,
            mine: HashSet::new(),
            primary: None,
            warm: HashMap::new(),
        };
        (root, vec![Cmd::ListSessions, Cmd::Redraw])
    }

    pub fn is_fleet(&self) -> bool {
        matches!(self.mode, Mode::Fleet(_))
    }

    pub fn update(&mut self, ev: UiEvent) -> Vec<Cmd> {
        if let UiEvent::Key {
            key, mods, kind, ..
        } = &ev
            && kind.is_down()
        {
            if is_fleet_toggle(key) {
                return self.toggle();
            }
            if let Some(forward) = cycle_dir(key, *mods) {
                return self.cycle(forward);
            }
            // Window/app-level shortcuts are handled above the active view so
            // they work in either mode, even when the fleet has no focused tile.
            match classify_shortcut(key, *mods) {
                Some(Shortcut::Quit) => return vec![Cmd::Quit],
                Some(Shortcut::NewWindow) => return vec![Cmd::NewWindow],
                Some(Shortcut::CloseWindow) => return vec![Cmd::CloseWindow],
                Some(Shortcut::NewSession) => return vec![Cmd::SpawnSession],
                _ => {} // Copy/Paste/Zoom are per-terminal: delegate below.
            }
        }
        if let UiEvent::AdoptSession(id) = &ev {
            let id = id.clone();
            return self.adopt(id);
        }
        // Output for a background session keeps its warm mirror live (the fleet
        // owns every model, so this only matters in the single view).
        if let UiEvent::SessionData { name, .. } = &ev
            && let Mode::Single(m) = &self.mode
            && m.session() != name
        {
            return self.feed_warm(ev);
        }
        if let UiEvent::Resize { w_px, h_px, scale } = ev {
            self.size_px = (w_px, h_px);
            if scale > 0.0 {
                self.scale = scale as f32;
            }
            // Resize the foreground and every warm background mirror, so a
            // backgrounded session is never left at a stale size (its prompt or a
            // full-screen program like `top` would come back mis-laid-out).
            let mut cmds = match &mut self.mode {
                Mode::Single(m) => resize_model(m, self.size_px, self.scale),
                Mode::Fleet(f) => return f.update(UiEvent::Resize { w_px, h_px, scale }),
            };
            for m in self.warm.values_mut() {
                cmds.extend(resize_model(m, self.size_px, self.scale));
            }
            return cmds;
        }
        match &mut self.mode {
            Mode::Single(m) => m.update(ev),
            Mode::Fleet(f) => f.update(ev),
        }
    }

    /// Feed output to a background session's warm mirror, dropping the mirror if
    /// the session ended. Returns any replies the mirror produced (e.g. a program
    /// querying the terminal still gets answered while backgrounded).
    fn feed_warm(&mut self, ev: UiEvent) -> Vec<Cmd> {
        let UiEvent::SessionData { name, ended, .. } = &ev else {
            return Vec::new();
        };
        let (name, ended) = (name.clone(), *ended);
        let cmds = match self.warm.get_mut(&name) {
            Some(m) => m.update(ev),
            None => Vec::new(), // not a session this window mirrors
        };
        if ended {
            self.warm.remove(&name);
        }
        cmds
    }

    /// Switch to the single view of `id` (the shell has just attached it) and
    /// take ownership. From the fleet, the existing tile's screen is preserved;
    /// otherwise (or from another single session) a fresh terminal is created.
    /// The previously-shown session is NOT detached — the window keeps it warm so
    /// Ctrl-Tab and the fleet can switch back to it.
    fn adopt(&mut self, id: SessionId) -> Vec<Cmd> {
        let placeholder = Mode::Single(Box::new(TerminalModel::new(
            String::new(),
            1,
            1,
            self.metrics,
        )));
        let current = std::mem::replace(&mut self.mode, placeholder);
        let (mut model, mut cmds) = match current {
            Mode::Fleet(f) => {
                let (kept, warm, cmds) =
                    f.into_single_adopting(id.clone(), self.size_px, self.scale);
                // The window's other driven sessions stay warm in the background.
                for m in warm {
                    self.warm.insert(m.session().to_string(), m);
                }
                (kept, cmds)
            }
            Mode::Single(m) => {
                let old = m.session().to_string();
                if old == id {
                    (*m, Vec::new())
                } else {
                    // Stow the outgoing foreground as a warm mirror; restore the
                    // target's mirror if we have one (instant, no re-attach), else
                    // build a fresh model.
                    self.warm.insert(old, *m);
                    let model = self
                        .warm
                        .remove(&id)
                        .unwrap_or_else(|| TerminalModel::new(id.clone(), 1, 1, self.metrics));
                    (model, Vec::new())
                }
            }
        };
        // Size the (possibly restored or fresh) foreground to the window.
        cmds.extend(resize_model(&mut model, self.size_px, self.scale));
        self.mode = Mode::Single(Box::new(model));
        self.mine.insert(id.clone());
        self.primary = Some(id);
        cmds.push(Cmd::Redraw);
        cmds
    }

    /// Cycle the window's foreground among its attached sessions (Ctrl-Tab),
    /// resolving the target from the owned set in a stable order. The switch is a
    /// warm-mirror swap — no re-attach — so it's instant and correctly sized. A
    /// window with fewer than two sessions has nothing to cycle.
    fn cycle(&mut self, forward: bool) -> Vec<Cmd> {
        let mut names: Vec<&SessionId> = self.mine.iter().collect();
        names.sort();
        if names.len() < 2 {
            return Vec::new();
        }
        let cur = self
            .primary
            .as_ref()
            .and_then(|p| names.iter().position(|n| *n == p))
            .unwrap_or(0);
        let n = names.len();
        let next = if forward {
            (cur + 1) % n
        } else {
            (cur + n - 1) % n
        };
        let to = names[next].clone();
        if Some(&to) == self.primary.as_ref() {
            return Vec::new();
        }
        self.adopt(to)
    }

    pub fn view(&self) -> Scene {
        match &self.mode {
            Mode::Single(m) => m.view(),
            Mode::Fleet(f) => f.view(),
        }
    }

    /// Combined render scale (device × zoom) of the active view, so the shell
    /// rasterizes glyphs at the size the current scene was laid out for.
    pub fn render_scale(&self) -> f32 {
        match &self.mode {
            Mode::Single(m) => m.render_scale(),
            Mode::Fleet(f) => f.render_scale(),
        }
    }

    /// Physical-pixel rect of the text cursor for the IME candidate window. Only
    /// the single view has a well-defined caret; the fleet overview returns None.
    pub fn ime_cursor_area(&self) -> Option<ghost_render::RectPx> {
        match &self.mode {
            Mode::Single(m) => m.ime_cursor_area(),
            Mode::Fleet(_) => None,
        }
    }

    /// Whether the app should exit: the single view's child ended. A fleet tile
    /// ending never quits the app.
    pub fn ended(&self) -> bool {
        match &self.mode {
            Mode::Single(m) => m.ended(),
            Mode::Fleet(_) => false,
        }
    }

    fn toggle(&mut self) -> Vec<Cmd> {
        // Swap the mode out behind a cheap placeholder so we can move the owned
        // model/fleet into the conversion.
        let placeholder = Mode::Single(Box::new(TerminalModel::new(
            String::new(),
            1,
            1,
            self.metrics,
        )));
        let current = std::mem::replace(&mut self.mode, placeholder);
        let (next, cmds) = match current {
            Mode::Single(m) => {
                // Hand the foreground and every warm background mirror to the
                // fleet, so all of this window's previews are live, not cold.
                let warm: Vec<TerminalModel> = self.warm.drain().map(|(_, m)| m).collect();
                let (fleet, mut cmds) = FleetModel::adopting(
                    *m,
                    warm,
                    self.metrics,
                    self.size_px,
                    self.scale,
                    self.mine.clone(),
                );
                cmds.insert(0, Cmd::ListSessions); // populate the grid
                (Mode::Fleet(Box::new(fleet)), cmds)
            }
            Mode::Fleet(f) => {
                let (model, warm, mut cmds) =
                    f.into_single_keeping(self.primary.clone(), self.size_px, self.scale);
                // The extracted session becomes the foreground; the rest of the
                // window's driven sessions stay warm in the background.
                for m in warm {
                    self.warm.insert(m.session().to_string(), m);
                }
                cmds.push(Cmd::Redraw);
                let id = model.session().to_string();
                self.mine.insert(id.clone());
                self.primary = Some(id);
                (Mode::Single(Box::new(model)), cmds)
            }
        };
        self.mode = next;
        cmds
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{KeyEventKind, Mods};

    const METRICS: CellMetrics = CellMetrics {
        advance: 9.0,
        line_height: 18.0,
    };
    const SIZE: (u32, u32) = (720, 432);

    fn root() -> RootModel {
        let m = TerminalModel::new("alpha".to_string(), 80, 24, METRICS);
        RootModel::single(m, METRICS, SIZE)
    }

    fn key(r: &mut RootModel, k: Key, mods: Mods) -> Vec<Cmd> {
        r.update(UiEvent::Key {
            key: k,
            mods,
            kind: KeyEventKind::Press,
            alts: None,
        })
    }

    #[test]
    fn single_delegates_text_to_the_terminal() {
        let mut r = root();
        assert_eq!(
            r.update(UiEvent::Text("a".into())),
            vec![Cmd::SendInput {
                session: "alpha".into(),
                bytes: b"a".to_vec()
            }]
        );
    }

    #[test]
    fn toggle_enters_fleet_and_lists_sessions() {
        let mut r = root();
        assert!(!r.is_fleet());
        let cmds = key(&mut r, Key::Named(NamedKey::F9), Mods::NONE);
        assert!(r.is_fleet());
        assert!(
            cmds.contains(&Cmd::ListSessions),
            "entering fleet enumerates sessions"
        );
    }

    #[test]
    fn toggle_round_trips_back_to_single() {
        let mut r = root();
        key(&mut r, Key::Named(NamedKey::F9), Mods::NONE);
        assert!(r.is_fleet());
        key(&mut r, Key::Named(NamedKey::F9), Mods::NONE);
        assert!(!r.is_fleet());
        // Back in single view, input is delegated to the (preserved) terminal.
        assert_eq!(
            r.update(UiEvent::Text("z".into())),
            vec![Cmd::SendInput {
                session: "alpha".into(),
                bytes: b"z".to_vec()
            }]
        );
    }

    #[test]
    fn toggle_back_targets_owned_session_after_focus_moved() {
        use crate::input::NamedKey;
        use ghost_vt::session::SessionInfo;
        fn info(name: &str, attached: bool) -> SessionInfo {
            SessionInfo {
                name: name.to_string(),
                pid: 1,
                created_at: None,
                title: name.to_string(),
                command: vec![],
                attached,
                bell: false,
            }
        }
        let mut r = root(); // owns "alpha"
        key(&mut r, Key::Named(NamedKey::F9), Mods::NONE); // -> fleet
        // The shell's ListSessions reply: our alpha plus a foreign detached beta.
        r.update(UiEvent::SessionList(vec![
            info("alpha", true),
            info("beta", false),
        ]));
        // Move focus onto the foreign tile (in the section below), then toggle back.
        r.update(UiEvent::Key {
            key: Key::Named(NamedKey::ArrowDown),
            mods: Mods::NONE,
            kind: KeyEventKind::Press,
            alts: None,
        });
        key(&mut r, Key::Named(NamedKey::F9), Mods::NONE); // -> single
        assert!(!r.is_fleet());
        // The single view returns to the OWNED session, not the focused foreign one.
        assert_eq!(
            r.update(UiEvent::Text("z".into())),
            vec![Cmd::SendInput {
                session: "alpha".into(),
                bytes: b"z".to_vec()
            }]
        );
    }

    #[test]
    fn fleet_toggle_preserves_device_scale() {
        use crate::SceneItem;
        let mut r = root();
        // HiDPI: a 2x surface. Cells double, so the grid halves and the rendered
        // frame carries the physical metrics.
        r.update(UiEvent::Resize {
            w_px: 720,
            h_px: 432,
            scale: 2.0,
        });
        // Round-trip through the fleet overview.
        key(&mut r, Key::Named(NamedKey::F9), Mods::NONE);
        key(&mut r, Key::Named(NamedKey::F9), Mods::NONE);
        assert!(!r.is_fleet());
        match r.view().terminals().next().unwrap() {
            SceneItem::Terminal { frame, .. } => {
                assert_eq!(
                    frame.metrics.advance, 18.0,
                    "single view still renders at 2x"
                );
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn fleet_toggle_key_is_not_forwarded_as_input() {
        let mut r = root();
        // The toggle key must drive the app, never reach the child as bytes.
        let cmds = key(&mut r, Key::Named(NamedKey::F9), Mods::NONE);
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::SendInput { .. })));
        // A plain 'e' still types into the terminal.
        let mut r = root();
        assert!(matches!(
            key(&mut r, Key::Char("e".into()), Mods::NONE).as_slice(),
            [Cmd::SendInput { .. }]
        ));
    }

    #[test]
    fn f9_toggles_the_fleet_overview() {
        let mut r = root();
        assert!(!r.is_fleet());
        let cmds = key(&mut r, Key::Named(NamedKey::F9), Mods::NONE);
        assert!(r.is_fleet(), "F9 enters the fleet overview");
        assert!(
            cmds.contains(&Cmd::ListSessions),
            "entering the fleet enumerates sessions"
        );
        // F9 again returns to the single view.
        key(&mut r, Key::Named(NamedKey::F9), Mods::NONE);
        assert!(!r.is_fleet(), "F9 toggles back to the single view");
    }

    #[test]
    fn ctrl_shift_e_no_longer_toggles_the_fleet() {
        let mut r = root();
        let _ = key(&mut r, Key::Char("e".into()), Mods::CTRL | Mods::SHIFT);
        assert!(
            !r.is_fleet(),
            "Ctrl+Shift+E is no longer the fleet toggle (F9 is)"
        );
    }

    use ghost_vt::session::SessionInfo;
    fn info(name: &str, attached: bool) -> SessionInfo {
        SessionInfo {
            name: name.to_string(),
            pid: 1,
            created_at: None,
            title: name.to_string(),
            command: vec![],
            attached,
            bell: false,
        }
    }

    #[test]
    fn window_and_session_shortcuts_are_intercepted_above_the_view() {
        // Window management: Cmd+X on macOS, Ctrl+Shift+X elsewhere.
        for chord in [Mods::SUPER, Mods::CTRL | Mods::SHIFT] {
            let mut r = root();
            assert_eq!(
                key(&mut r, Key::Char("n".into()), chord),
                vec![Cmd::NewWindow]
            );
            assert_eq!(
                key(&mut r, Key::Char("w".into()), chord),
                vec![Cmd::CloseWindow]
            );
        }
        // New session is Cmd+T on macOS, Alt+T elsewhere.
        let new_session = if cfg!(target_os = "macos") {
            Mods::SUPER
        } else {
            Mods::ALT
        };
        let mut r = root();
        assert_eq!(
            key(&mut r, Key::Char("t".into()), new_session),
            vec![Cmd::SpawnSession]
        );
        // They also fire in the fleet overview, which may have no focused tile to
        // forward keys to — so they can't be left to the per-terminal path.
        let (mut f, _) = RootModel::fleet(METRICS, SIZE, 1.0);
        assert!(f.is_fleet());
        assert_eq!(
            key(&mut f, Key::Char("n".into()), Mods::SUPER),
            vec![Cmd::NewWindow]
        );
        assert_eq!(
            key(&mut f, Key::Char("t".into()), new_session),
            vec![Cmd::SpawnSession]
        );
        // Bare Ctrl+N (no Shift) is NOT a shortcut: it must reach the child.
        let mut r = root();
        assert!(matches!(
            key(&mut r, Key::Char("n".into()), Mods::CTRL).as_slice(),
            [Cmd::SendInput { .. }]
        ));
    }

    #[test]
    fn fleet_constructor_starts_in_the_overview_and_enumerates() {
        let (r, cmds) = RootModel::fleet(METRICS, SIZE, 1.0);
        assert!(r.is_fleet());
        assert!(
            cmds.contains(&Cmd::ListSessions),
            "a fresh fleet window enumerates sessions: {cmds:?}"
        );
    }

    fn feed(r: &mut RootModel, name: &str, bytes: &[u8]) -> Vec<Cmd> {
        r.update(UiEvent::SessionData {
            name: name.into(),
            bytes: bytes.to_vec(),
            ended: false,
        })
    }

    #[test]
    fn background_sessions_stay_live_and_keep_their_screens() {
        // The window drives two sessions; alpha starts foreground.
        let mut r = root(); // single view of alpha, mine = {alpha}
        // A tall window so both readable tiles are on screen at once (otherwise the
        // grid scrolls and culls the off-screen one — that is exercised in fleet's
        // own tests; here we only care that both are live, not "starting…").
        r.update(UiEvent::Resize {
            w_px: 720,
            h_px: 1200,
            scale: 1.0,
        });
        feed(&mut r, "alpha", b"alpha-screen");
        // Switch to beta (alpha goes to the background, kept warm).
        r.update(UiEvent::AdoptSession("beta".into()));
        feed(&mut r, "beta", b"beta-screen");
        // Background alpha keeps receiving output while beta is shown.
        feed(&mut r, "alpha", b" still-running");

        // Opening the fleet must show BOTH as live previews, not "starting…".
        key(&mut r, Key::Named(NamedKey::F9), Mods::NONE);
        assert!(r.is_fleet());
        assert_eq!(
            r.view().terminals().count(),
            2,
            "every session the window drives previews live"
        );

        // Switching back to alpha restores its (warm) screen and routes input to
        // it — a warm-mirror swap, no re-attach (no Attach/Spawn/TakeOver).
        key(&mut r, Key::Named(NamedKey::F9), Mods::NONE); // -> single (beta)
        let cmds = r.update(UiEvent::AdoptSession("alpha".into()));
        assert!(
            !cmds
                .iter()
                .any(|c| matches!(c, Cmd::Attach(_) | Cmd::Spawn { .. } | Cmd::TakeOver(_))),
            "switching to a warm session needs no re-attach: {cmds:?}"
        );
        assert_eq!(
            r.update(UiEvent::Text("x".into())),
            vec![Cmd::SendInput {
                session: "alpha".into(),
                bytes: b"x".to_vec()
            }]
        );
    }

    #[test]
    fn refleeting_an_adopted_session_shows_a_live_preview_not_starting() {
        // A fleet-started window (owns nothing); the user attaches a detached
        // session, the shell feeds it, then the user reopens the fleet.
        let (mut r, _) = RootModel::fleet(METRICS, SIZE, 1.0);
        r.update(UiEvent::SessionList(vec![info("d", false)])); // detached
        r.update(UiEvent::AdoptSession("d".into())); // attach + show single
        r.update(UiEvent::SessionData {
            name: "d".into(),
            bytes: b"hello$ ".to_vec(),
            ended: false,
        });
        key(&mut r, Key::Named(NamedKey::F9), Mods::NONE); // back to the fleet
        assert!(r.is_fleet());
        // d is now a session this window drives, so its tile must be a live
        // preview (a Terminal in the scene) — never the "starting…" placeholder.
        assert_eq!(
            r.view().terminals().count(),
            1,
            "the adopted session previews live, not as a placeholder"
        );
    }

    #[test]
    fn adopt_from_fleet_drops_into_that_sessions_single_view() {
        let mut r = root(); // owns alpha
        key(&mut r, Key::Named(NamedKey::F9), Mods::NONE); // -> fleet
        r.update(UiEvent::SessionList(vec![
            info("alpha", true),
            info("beta", false),
        ]));
        // What the shell sends after attaching a double-clicked / spawned session.
        let cmds = r.update(UiEvent::AdoptSession("beta".into()));
        assert!(!r.is_fleet(), "adopting leaves the overview");
        assert!(cmds.contains(&Cmd::Redraw));
        // Input now routes to the adopted session.
        assert_eq!(
            r.update(UiEvent::Text("z".into())),
            vec![Cmd::SendInput {
                session: "beta".into(),
                bytes: b"z".to_vec()
            }]
        );
    }

    #[test]
    fn adopt_of_a_freshly_spawned_session_makes_a_new_terminal_and_keeps_previews() {
        let mut r = root(); // owns alpha
        key(&mut r, Key::Named(NamedKey::F9), Mods::NONE); // -> fleet
        r.update(UiEvent::SessionList(vec![
            info("alpha", true),
            info("beta", false),
        ]));
        // Adopt a session that is NOT a tile yet (just spawned by the shell).
        let cmds = r.update(UiEvent::AdoptSession("gamma".into()));
        assert!(!r.is_fleet());
        // Nothing detaches — the window keeps its sessions warm.
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Detach(_))),
            "previews stay attached: {cmds:?}"
        );
        assert_eq!(
            r.update(UiEvent::Text("z".into())),
            vec![Cmd::SendInput {
                session: "gamma".into(),
                bytes: b"z".to_vec()
            }]
        );
    }

    #[test]
    fn adopt_from_single_view_keeps_the_previous_session_attached() {
        let mut r = root(); // single view of alpha
        let cmds = r.update(UiEvent::AdoptSession("beta".into()));
        assert!(!r.is_fleet());
        assert!(
            !cmds.iter().any(|c| matches!(c, Cmd::Detach(_))),
            "the previous session stays attached (warm): {cmds:?}"
        );
        assert_eq!(
            r.update(UiEvent::Text("z".into())),
            vec![Cmd::SendInput {
                session: "beta".into(),
                bytes: b"z".to_vec()
            }]
        );
    }

    /// Adopt sessions into the window so it owns several, then return the sorted
    /// owned set the cycle walks.
    fn with_three(r: &mut RootModel) {
        r.update(UiEvent::AdoptSession("beta".into()));
        r.update(UiEvent::AdoptSession("gamma".into()));
        // Owns alpha, beta, gamma; foreground is gamma (last adopted).
    }

    fn ctrl_tab(r: &mut RootModel, shift: bool) -> Vec<Cmd> {
        let mods = if shift {
            Mods::CTRL | Mods::SHIFT
        } else {
            Mods::CTRL
        };
        key(r, Key::Named(NamedKey::Tab), mods)
    }

    /// The session the single view currently routes input to.
    fn foreground(r: &mut RootModel) -> String {
        match r.update(UiEvent::Text("x".into())).into_iter().next() {
            Some(Cmd::SendInput { session, .. }) => session,
            other => panic!("expected SendInput, got {other:?}"),
        }
    }

    #[test]
    fn ctrl_tab_cycles_to_the_next_attached_session() {
        let mut r = root(); // owns alpha
        with_three(&mut r); // -> alpha, beta, gamma (foreground gamma)
        // Forward from gamma wraps to alpha (sorted: alpha, beta, gamma); the
        // switch is a warm swap, not a re-attach.
        let cmds = ctrl_tab(&mut r, false);
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::SendInput { .. })));
        assert_eq!(
            foreground(&mut r),
            "alpha",
            "Ctrl-Tab advances the foreground"
        );
    }

    #[test]
    fn ctrl_shift_tab_cycles_to_the_previous_attached_session() {
        let mut r = root();
        with_three(&mut r); // foreground gamma
        ctrl_tab(&mut r, true);
        assert_eq!(
            foreground(&mut r),
            "beta",
            "Ctrl-Shift-Tab steps the foreground backward"
        );
    }

    #[test]
    fn ctrl_tab_with_a_single_session_is_a_noop() {
        let mut r = root(); // owns only alpha
        assert!(
            ctrl_tab(&mut r, false).is_empty(),
            "nothing to cycle with one session"
        );
        // And the Tab is not forwarded to the child as input.
        assert!(
            !ctrl_tab(&mut r, false)
                .iter()
                .any(|c| matches!(c, Cmd::SendInput { .. })),
        );
    }
}
