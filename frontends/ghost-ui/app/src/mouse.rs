//! Pure mouse-event -> terminal-bytes encoder (xterm scheme).
//!
//! Given the terminal's active reporting protocol and coordinate mode (from
//! `Vt::mouse_protocol` / `Vt::mouse_sgr`) plus an event, produce the report the
//! child expects — or `None` when the active protocol says this event shouldn't
//! be reported (e.g. motion while only press/release tracking is on). The
//! caller owns pixel->cell conversion and button-held state.

use ghost_term::MouseProtocol;
use winit::keyboard::ModifiersState;

/// A mouse button, or a wheel notch (which xterm models as buttons 4/5).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Button {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
}

/// What happened to the pointer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Press,
    Release,
    Motion,
}

/// The low button bits before modifier/motion flags. Wheel notches set bit 6.
fn base_code(b: Button) -> u32 {
    match b {
        Button::Left => 0,
        Button::Middle => 1,
        Button::Right => 2,
        Button::WheelUp => 64,
        Button::WheelDown => 65,
    }
}

/// xterm modifier bits: Shift=4, Meta/Alt=8, Control=16.
fn mod_flags(m: ModifiersState) -> u32 {
    (if m.shift_key() { 4 } else { 0 })
        + (if m.alt_key() { 8 } else { 0 })
        + (if m.control_key() { 16 } else { 0 })
}

/// Encode a mouse event, or `None` if the active protocol suppresses it.
///
/// `button` is the pressed/released button for `Press`/`Release`, or for
/// `Motion` the button being dragged (`None` for buttonless hover motion).
/// `held` says whether any button is currently down (drag vs hover).
/// `col`/`row` are 1-based cell coordinates.
#[allow(clippy::too_many_arguments)]
pub fn encode(
    proto: MouseProtocol,
    sgr: bool,
    kind: Kind,
    button: Option<Button>,
    held: bool,
    col: u16,
    row: u16,
    mods: ModifiersState,
) -> Option<Vec<u8>> {
    if proto == MouseProtocol::Off {
        return None;
    }
    // Motion is only reported under any-motion tracking, or under button-event
    // tracking while a button is held (a drag).
    if kind == Kind::Motion {
        match proto {
            MouseProtocol::AnyMotion => {}
            MouseProtocol::ButtonDrag if held => {}
            _ => return None,
        }
    }

    // Legacy release collapses to the generic "button up" code 3; SGR keeps the
    // real button and signals release with the `m` terminator instead.
    let raw = match (kind, button) {
        (Kind::Release, _) if !sgr => 3,
        (_, Some(b)) => base_code(b),
        (_, None) => 3, // buttonless motion (hover)
    };
    let mut cb = raw + mod_flags(mods);
    if kind == Kind::Motion {
        cb += 32;
    }

    if sgr {
        let term = if kind == Kind::Release { 'm' } else { 'M' };
        Some(format!("\x1b[<{cb};{col};{row}{term}").into_bytes())
    } else {
        // Legacy: CSI M, then button/x/y each offset by 32 into a single byte.
        // Coordinates beyond 223 can't be represented and are clamped.
        let cx = (col.min(223) as u32 + 32) as u8;
        let cy = (row.min(223) as u32 + 32) as u8;
        let cbb = (cb.min(223) + 32) as u8;
        Some(vec![0x1b, b'[', b'M', cbb, cx, cy])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ghost_term::MouseProtocol::*;

    fn none() -> ModifiersState {
        ModifiersState::empty()
    }

    #[test]
    fn sgr_press_and_release_left() {
        let press = encode(
            Press,
            true,
            Kind::Press,
            Some(Button::Left),
            true,
            1,
            1,
            none(),
        );
        assert_eq!(press.unwrap(), b"\x1b[<0;1;1M");
        let release = encode(
            Press,
            true,
            Kind::Release,
            Some(Button::Left),
            false,
            1,
            1,
            none(),
        );
        assert_eq!(release.unwrap(), b"\x1b[<0;1;1m");
    }

    #[test]
    fn sgr_modifiers_and_wheel() {
        let ctrl = encode(
            Press,
            true,
            Kind::Press,
            Some(Button::Left),
            true,
            1,
            1,
            ModifiersState::CONTROL,
        );
        assert_eq!(ctrl.unwrap(), b"\x1b[<16;1;1M");
        let wheel = encode(
            Press,
            true,
            Kind::Press,
            Some(Button::WheelUp),
            false,
            10,
            2,
            none(),
        );
        assert_eq!(wheel.unwrap(), b"\x1b[<64;10;2M");
    }

    #[test]
    fn motion_gating_by_protocol() {
        // Drag (button held) reports under button-event tracking.
        let drag = encode(
            ButtonDrag,
            true,
            Kind::Motion,
            Some(Button::Left),
            true,
            3,
            4,
            none(),
        );
        assert_eq!(drag.unwrap(), b"\x1b[<32;3;4M");
        // Buttonless hover reports only under any-motion tracking (code 3+32).
        let hover = encode(AnyMotion, true, Kind::Motion, None, false, 3, 4, none());
        assert_eq!(hover.unwrap(), b"\x1b[<35;3;4M");
        // No motion under press-only tracking, nor button-drag while unheld.
        assert!(encode(Press, true, Kind::Motion, None, false, 3, 4, none()).is_none());
        assert!(encode(ButtonDrag, true, Kind::Motion, None, false, 3, 4, none()).is_none());
    }

    #[test]
    fn off_reports_nothing() {
        assert!(
            encode(
                Off,
                true,
                Kind::Press,
                Some(Button::Left),
                true,
                1,
                1,
                none()
            )
            .is_none()
        );
    }

    #[test]
    fn legacy_press_and_release() {
        let press = encode(
            Press,
            false,
            Kind::Press,
            Some(Button::Left),
            true,
            1,
            1,
            none(),
        );
        assert_eq!(press.unwrap(), vec![0x1b, b'[', b'M', 32, 33, 33]);
        // Legacy release uses the generic button-up code 3 (3 + 32 = 35).
        let release = encode(
            Press,
            false,
            Kind::Release,
            Some(Button::Left),
            false,
            1,
            1,
            none(),
        );
        assert_eq!(release.unwrap(), vec![0x1b, b'[', b'M', 35, 33, 33]);
    }
}
