//! Core input types — ghost-ui-core's own keyboard alphabet, deliberately
//! independent of winit so the core compiles and is tested without a windowing
//! backend. The shell's `from_winit` adapter is the sole place real winit
//! events are mapped onto these.

/// Keyboard modifier state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    /// The "super" / Command / Windows key.
    pub sup: bool,
}

impl Mods {
    pub const NONE: Mods = Mods {
        shift: false,
        ctrl: false,
        alt: false,
        sup: false,
    };
    pub const SHIFT: Mods = Mods {
        shift: true,
        ctrl: false,
        alt: false,
        sup: false,
    };
    pub const CTRL: Mods = Mods {
        shift: false,
        ctrl: true,
        alt: false,
        sup: false,
    };
    pub const ALT: Mods = Mods {
        shift: false,
        ctrl: false,
        alt: true,
        sup: false,
    };
    pub const SUPER: Mods = Mods {
        shift: false,
        ctrl: false,
        alt: false,
        sup: true,
    };
}

impl std::ops::BitOr for Mods {
    type Output = Mods;
    fn bitor(self, o: Mods) -> Mods {
        Mods {
            shift: self.shift | o.shift,
            ctrl: self.ctrl | o.ctrl,
            alt: self.alt | o.alt,
            sup: self.sup | o.sup,
        }
    }
}

/// A logical key. `Char` carries the text the key produces (already
/// shift/layout-resolved); `Named` is a non-text key we interpret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Key {
    Char(String),
    Named(NamedKey),
    /// A dead key mid-composition — left to the IME, produces nothing alone.
    Dead,
    /// A key the core doesn't interpret.
    Unidentified,
}

/// The non-text keys ghost encodes; anything else maps to `Other`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamedKey {
    Enter,
    Tab,
    Backspace,
    Escape,
    Space,
    ArrowUp,
    ArrowDown,
    ArrowLeft,
    ArrowRight,
    Home,
    End,
    Insert,
    Delete,
    PageUp,
    PageDown,
    F1,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    F10,
    F11,
    F12,
    /// A named key the core doesn't encode.
    Other,
}
