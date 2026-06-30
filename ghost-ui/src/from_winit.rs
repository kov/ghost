//! The single winit -> ghost-ui-core boundary. The shell translates real OS
//! input here; everything downstream speaks only core types. Keeping all winit
//! type knowledge in one module is what lets the core stay pure and testable.

use ghost_ui_core::input::{Key, KeyAlternates, Mods, NamedKey};
use winit::event::KeyEvent;
use winit::keyboard::{Key as WKey, KeyCode, ModifiersState, NamedKey as WNamed, PhysicalKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;

pub fn mods(m: ModifiersState) -> Mods {
    Mods {
        shift: m.shift_key(),
        ctrl: m.control_key(),
        alt: m.alt_key(),
        sup: m.super_key(),
    }
}

pub fn key(k: &WKey, physical: PhysicalKey) -> Key {
    match k {
        // Modifier keys are reported without a side in the logical key; the side
        // lives in the physical key (the kitty protocol distinguishes them).
        WKey::Named(WNamed::Shift) => Key::Named(modifier_side(physical, WNamed::Shift)),
        WKey::Named(WNamed::Control) => Key::Named(modifier_side(physical, WNamed::Control)),
        WKey::Named(WNamed::Alt) => Key::Named(modifier_side(physical, WNamed::Alt)),
        WKey::Named(WNamed::Super) => Key::Named(modifier_side(physical, WNamed::Super)),
        WKey::Named(n) => Key::Named(named(*n)),
        WKey::Character(s) => Key::Char(s.to_string()),
        WKey::Dead(_) => Key::Dead,
        _ => Key::Unidentified,
    }
}

/// Pick the left/right `NamedKey` for a modifier from its physical key; defaults
/// to the left side when the physical position is unknown.
fn modifier_side(physical: PhysicalKey, modifier: WNamed) -> NamedKey {
    let right = matches!(
        physical,
        PhysicalKey::Code(
            KeyCode::ShiftRight | KeyCode::ControlRight | KeyCode::AltRight | KeyCode::SuperRight
        )
    );
    match (modifier, right) {
        (WNamed::Shift, false) => NamedKey::ShiftLeft,
        (WNamed::Shift, true) => NamedKey::ShiftRight,
        (WNamed::Control, false) => NamedKey::ControlLeft,
        (WNamed::Control, true) => NamedKey::ControlRight,
        (WNamed::Alt, false) => NamedKey::AltLeft,
        (WNamed::Alt, true) => NamedKey::AltRight,
        (WNamed::Super, false) => NamedKey::SuperLeft,
        (WNamed::Super, true) => NamedKey::SuperRight,
        _ => NamedKey::Other,
    }
}

/// The kitty report-alternate-keys (flag 4) codepoints for a text key: the
/// unshifted `base` (the canonical unicode-key-code), the `shifted` glyph when
/// Shift is held and differs, and the `base_layout` key at this physical position
/// on the standard US layout when it differs. `None` for non-text keys (their
/// codepoint is fixed and carries no alternates).
pub fn alternates(event: &KeyEvent, m: ModifiersState) -> Option<KeyAlternates> {
    // `key_without_modifiers` strips Shift/AltGr/etc., giving the canonical key.
    let base = match event.key_without_modifiers() {
        WKey::Character(s) => s.chars().next()?,
        _ => return None,
    };
    let shifted = if m.shift_key() {
        match &event.logical_key {
            WKey::Character(s) => s.chars().next().filter(|c| *c != base),
            _ => None,
        }
    } else {
        None
    };
    let base_layout = us_layout_char(event.physical_key).filter(|c| *c != base);
    Some(KeyAlternates {
        base,
        shifted,
        base_layout,
    })
}

/// The unshifted character a physical key produces on the standard US (PC-101)
/// layout — kitty's base-layout key. Only the printable keys have one.
fn us_layout_char(phys: PhysicalKey) -> Option<char> {
    let PhysicalKey::Code(code) = phys else {
        return None;
    };
    use KeyCode::*;
    Some(match code {
        KeyA => 'a',
        KeyB => 'b',
        KeyC => 'c',
        KeyD => 'd',
        KeyE => 'e',
        KeyF => 'f',
        KeyG => 'g',
        KeyH => 'h',
        KeyI => 'i',
        KeyJ => 'j',
        KeyK => 'k',
        KeyL => 'l',
        KeyM => 'm',
        KeyN => 'n',
        KeyO => 'o',
        KeyP => 'p',
        KeyQ => 'q',
        KeyR => 'r',
        KeyS => 's',
        KeyT => 't',
        KeyU => 'u',
        KeyV => 'v',
        KeyW => 'w',
        KeyX => 'x',
        KeyY => 'y',
        KeyZ => 'z',
        Digit0 => '0',
        Digit1 => '1',
        Digit2 => '2',
        Digit3 => '3',
        Digit4 => '4',
        Digit5 => '5',
        Digit6 => '6',
        Digit7 => '7',
        Digit8 => '8',
        Digit9 => '9',
        Backquote => '`',
        Minus => '-',
        Equal => '=',
        BracketLeft => '[',
        BracketRight => ']',
        Backslash => '\\',
        Semicolon => ';',
        Quote => '\'',
        Comma => ',',
        Period => '.',
        Slash => '/',
        _ => return None,
    })
}

fn named(n: WNamed) -> NamedKey {
    match n {
        WNamed::Enter => NamedKey::Enter,
        WNamed::Tab => NamedKey::Tab,
        WNamed::Space => NamedKey::Space,
        WNamed::Backspace => NamedKey::Backspace,
        WNamed::Escape => NamedKey::Escape,
        WNamed::ArrowUp => NamedKey::ArrowUp,
        WNamed::ArrowDown => NamedKey::ArrowDown,
        WNamed::ArrowLeft => NamedKey::ArrowLeft,
        WNamed::ArrowRight => NamedKey::ArrowRight,
        WNamed::Home => NamedKey::Home,
        WNamed::End => NamedKey::End,
        WNamed::Insert => NamedKey::Insert,
        WNamed::Delete => NamedKey::Delete,
        WNamed::PageUp => NamedKey::PageUp,
        WNamed::PageDown => NamedKey::PageDown,
        WNamed::F1 => NamedKey::F1,
        WNamed::F2 => NamedKey::F2,
        WNamed::F3 => NamedKey::F3,
        WNamed::F4 => NamedKey::F4,
        WNamed::F5 => NamedKey::F5,
        WNamed::F6 => NamedKey::F6,
        WNamed::F7 => NamedKey::F7,
        WNamed::F8 => NamedKey::F8,
        WNamed::F9 => NamedKey::F9,
        WNamed::F10 => NamedKey::F10,
        WNamed::F11 => NamedKey::F11,
        WNamed::F12 => NamedKey::F12,
        _ => NamedKey::Other,
    }
}

#[cfg(test)]
mod tests {
    //! Covers the pure winit → core mappings. `alternates` is the one function not
    //! tested here: it needs a real `winit::event::KeyEvent` (private, platform-
    //! populated fields — `key_without_modifiers` is filled by the OS), which can't
    //! be constructed in a unit test. Its downstream effect — the kitty
    //! report-alternate-keys encoding — is covered by `ghost-ui-core`'s `encode`
    //! tests; only the winit extraction is uncovered.
    use super::{key, modifier_side, mods, named, us_layout_char};
    use ghost_ui_core::input::{Key, Mods, NamedKey};
    use winit::keyboard::{
        Key as WKey, KeyCode, ModifiersState, NamedKey as WNamed, NativeKey, NativeKeyCode,
        PhysicalKey,
    };

    fn code(c: KeyCode) -> PhysicalKey {
        PhysicalKey::Code(c)
    }

    #[test]
    fn mods_maps_each_modifier_independently() {
        assert_eq!(mods(ModifiersState::empty()), Mods::default());
        assert_eq!(
            mods(ModifiersState::SHIFT),
            Mods {
                shift: true,
                ..Default::default()
            }
        );
        assert_eq!(
            mods(ModifiersState::CONTROL),
            Mods {
                ctrl: true,
                ..Default::default()
            }
        );
        assert_eq!(
            mods(ModifiersState::ALT),
            Mods {
                alt: true,
                ..Default::default()
            }
        );
        assert_eq!(
            mods(ModifiersState::SUPER),
            Mods {
                sup: true,
                ..Default::default()
            }
        );
    }

    #[test]
    fn mods_combines_all() {
        let all = ModifiersState::SHIFT
            | ModifiersState::CONTROL
            | ModifiersState::ALT
            | ModifiersState::SUPER;
        assert_eq!(
            mods(all),
            Mods {
                shift: true,
                ctrl: true,
                alt: true,
                sup: true,
            }
        );
    }

    #[test]
    fn key_maps_named_character_dead_and_unidentified() {
        let p = code(KeyCode::KeyA); // physical is irrelevant for non-modifier keys
        assert_eq!(
            key(&WKey::Named(WNamed::Enter), p),
            Key::Named(NamedKey::Enter)
        );
        assert_eq!(key(&WKey::Named(WNamed::F9), p), Key::Named(NamedKey::F9));
        // A logical character arrives shift/layout-resolved (the glyph, not the base).
        assert_eq!(key(&WKey::Character("a".into()), p), Key::Char("a".into()));
        assert_eq!(key(&WKey::Character("(".into()), p), Key::Char("(".into()));
        assert_eq!(key(&WKey::Dead(Some('\u{0301}')), p), Key::Dead);
        assert_eq!(key(&WKey::Dead(None), p), Key::Dead);
        assert_eq!(
            key(&WKey::Unidentified(NativeKey::Unidentified), p),
            Key::Unidentified
        );
    }

    #[test]
    fn key_resolves_modifier_side_from_the_physical_key() {
        // The logical key carries no side; `key` reads it from the physical position.
        assert_eq!(
            key(&WKey::Named(WNamed::Shift), code(KeyCode::ShiftLeft)),
            Key::Named(NamedKey::ShiftLeft)
        );
        assert_eq!(
            key(&WKey::Named(WNamed::Shift), code(KeyCode::ShiftRight)),
            Key::Named(NamedKey::ShiftRight)
        );
        assert_eq!(
            key(&WKey::Named(WNamed::Control), code(KeyCode::ControlRight)),
            Key::Named(NamedKey::ControlRight)
        );
    }

    #[test]
    fn modifier_side_picks_left_or_right_per_physical_key() {
        let cases = [
            (WNamed::Shift, KeyCode::ShiftLeft, NamedKey::ShiftLeft),
            (WNamed::Shift, KeyCode::ShiftRight, NamedKey::ShiftRight),
            (WNamed::Control, KeyCode::ControlLeft, NamedKey::ControlLeft),
            (
                WNamed::Control,
                KeyCode::ControlRight,
                NamedKey::ControlRight,
            ),
            (WNamed::Alt, KeyCode::AltLeft, NamedKey::AltLeft),
            (WNamed::Alt, KeyCode::AltRight, NamedKey::AltRight),
            (WNamed::Super, KeyCode::SuperLeft, NamedKey::SuperLeft),
            (WNamed::Super, KeyCode::SuperRight, NamedKey::SuperRight),
        ];
        for (m, phys, want) in cases {
            assert_eq!(modifier_side(code(phys), m), want, "{m:?} at {phys:?}");
        }
    }

    #[test]
    fn modifier_side_defaults_to_left_when_position_unknown() {
        // A modifier whose physical key isn't a known L/R code falls back to left.
        assert_eq!(
            modifier_side(code(KeyCode::KeyA), WNamed::Shift),
            NamedKey::ShiftLeft
        );
        assert_eq!(
            modifier_side(
                PhysicalKey::Unidentified(NativeKeyCode::Unidentified),
                WNamed::Alt
            ),
            NamedKey::AltLeft
        );
    }

    #[test]
    fn named_maps_known_keys_and_falls_back_to_other() {
        assert_eq!(named(WNamed::Enter), NamedKey::Enter);
        assert_eq!(named(WNamed::Tab), NamedKey::Tab);
        assert_eq!(named(WNamed::Escape), NamedKey::Escape);
        assert_eq!(named(WNamed::ArrowLeft), NamedKey::ArrowLeft);
        assert_eq!(named(WNamed::Delete), NamedKey::Delete);
        assert_eq!(named(WNamed::PageDown), NamedKey::PageDown);
        assert_eq!(named(WNamed::F1), NamedKey::F1);
        assert_eq!(named(WNamed::F12), NamedKey::F12);
        // Anything ghost doesn't special-case collapses to `Other`.
        assert_eq!(named(WNamed::CapsLock), NamedKey::Other);
    }

    #[test]
    fn us_layout_char_covers_printable_keys_only() {
        assert_eq!(us_layout_char(code(KeyCode::KeyA)), Some('a'));
        assert_eq!(us_layout_char(code(KeyCode::KeyZ)), Some('z'));
        assert_eq!(us_layout_char(code(KeyCode::Digit1)), Some('1'));
        assert_eq!(us_layout_char(code(KeyCode::Backquote)), Some('`'));
        assert_eq!(us_layout_char(code(KeyCode::Slash)), Some('/'));
        assert_eq!(us_layout_char(code(KeyCode::Backslash)), Some('\\'));
        // Non-printable physical keys, and non-`Code` positions, have no layout char.
        assert_eq!(us_layout_char(code(KeyCode::Enter)), None);
        assert_eq!(
            us_layout_char(PhysicalKey::Unidentified(NativeKeyCode::Unidentified)),
            None
        );
    }
}
