//! `RootModel` — the top of the model tree: either the single-terminal view or
//! the fleet overview, with one key (Ctrl+Shift+E) toggling between them.
//!
//! Toggling preserves session state: going to the fleet *adopts* the current
//! terminal as its focused tile (so its screen survives), and coming back
//! *extracts* the focused tile's terminal (detaching the rest). The shell drives
//! this model exactly as it drove a bare `TerminalModel` — `update` in, `Cmd`s
//! out, `view` to draw — so the whole tree stays headlessly testable.

use std::collections::HashSet;

use crate::input::{Key, Mods};
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
}

/// Ctrl+Shift+E toggles the fleet overview.
fn is_fleet_toggle(key: &Key, mods: Mods) -> bool {
    mods.ctrl && mods.shift && matches!(key, Key::Char(s) if s.eq_ignore_ascii_case("e"))
}

impl RootModel {
    /// Start in the single-terminal view around `model`.
    pub fn single(model: TerminalModel, metrics: CellMetrics, size_px: (u32, u32)) -> Self {
        let mine = HashSet::from([model.session().to_string()]);
        RootModel {
            mode: Mode::Single(Box::new(model)),
            metrics,
            size_px,
            scale: 1.0,
            mine,
        }
    }

    pub fn is_fleet(&self) -> bool {
        matches!(self.mode, Mode::Fleet(_))
    }

    pub fn update(&mut self, ev: UiEvent) -> Vec<Cmd> {
        if let UiEvent::Key {
            key,
            mods,
            pressed: true,
        } = &ev
            && is_fleet_toggle(key, *mods)
        {
            return self.toggle();
        }
        if let UiEvent::Resize { w_px, h_px, scale } = &ev {
            self.size_px = (*w_px, *h_px);
            if *scale > 0.0 {
                self.scale = *scale as f32;
            }
        }
        match &mut self.mode {
            Mode::Single(m) => m.update(ev),
            Mode::Fleet(f) => f.update(ev),
        }
    }

    pub fn view(&self) -> Scene {
        match &self.mode {
            Mode::Single(m) => m.view(),
            Mode::Fleet(f) => f.view(),
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
                let (fleet, mut cmds) = FleetModel::adopting(
                    *m,
                    self.metrics,
                    self.size_px,
                    self.scale,
                    self.mine.clone(),
                );
                cmds.insert(0, Cmd::ListSessions); // populate the grid
                (Mode::Fleet(Box::new(fleet)), cmds)
            }
            Mode::Fleet(f) => {
                let (model, mut cmds) = f.into_single(self.size_px, self.scale);
                cmds.push(Cmd::Redraw);
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
            pressed: true,
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
        let cmds = key(&mut r, Key::Char("e".into()), Mods::CTRL | Mods::SHIFT);
        assert!(r.is_fleet());
        assert!(
            cmds.contains(&Cmd::ListSessions),
            "entering fleet enumerates sessions"
        );
    }

    #[test]
    fn toggle_round_trips_back_to_single() {
        let mut r = root();
        key(&mut r, Key::Char("e".into()), Mods::CTRL | Mods::SHIFT);
        assert!(r.is_fleet());
        key(&mut r, Key::Char("e".into()), Mods::CTRL | Mods::SHIFT);
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
        key(&mut r, Key::Char("e".into()), Mods::CTRL | Mods::SHIFT); // -> fleet
        // The shell's ListSessions reply: our alpha plus a foreign detached beta.
        r.update(UiEvent::SessionList(vec![
            info("alpha", true),
            info("beta", false),
        ]));
        // Move focus onto the foreign tile, then toggle back.
        r.update(UiEvent::Key {
            key: Key::Named(NamedKey::ArrowRight),
            mods: Mods::NONE,
            pressed: true,
        });
        key(&mut r, Key::Char("e".into()), Mods::CTRL | Mods::SHIFT); // -> single
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
        key(&mut r, Key::Char("e".into()), Mods::CTRL | Mods::SHIFT);
        key(&mut r, Key::Char("e".into()), Mods::CTRL | Mods::SHIFT);
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
        // The toggle chord must drive the app, never reach the child as bytes.
        let cmds = key(&mut r, Key::Char("e".into()), Mods::CTRL | Mods::SHIFT);
        assert!(!cmds.iter().any(|c| matches!(c, Cmd::SendInput { .. })));
        // A plain 'e' still types into the terminal.
        let mut r = root();
        assert!(matches!(
            key(&mut r, Key::Char("e".into()), Mods::NONE).as_slice(),
            [Cmd::SendInput { .. }]
        ));
    }
}
