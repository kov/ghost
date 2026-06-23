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

pub fn key(k: &WKey) -> Key {
    match k {
        WKey::Named(n) => Key::Named(named(*n)),
        WKey::Character(s) => Key::Char(s.to_string()),
        WKey::Dead(_) => Key::Dead,
        _ => Key::Unidentified,
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
