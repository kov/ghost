//! The single winit -> ghost-ui-core boundary. The shell translates real OS
//! input here; everything downstream speaks only core types. Keeping all winit
//! type knowledge in one module is what lets the core stay pure and testable.

use ghost_ui_core::input::{Key, Mods, NamedKey};
use winit::keyboard::{Key as WKey, ModifiersState, NamedKey as WNamed};

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
