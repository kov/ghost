//! Core input types — ghost-ui-core's own keyboard alphabet, deliberately
//! independent of winit so the core compiles and is tested without a windowing
//! backend. The shell's `from_winit` adapter is the sole place real winit
//! events are mapped onto these.

/// The kind of a key event. Legacy / modifyOtherKeys encoding only acts on a
/// key going down (`Press`/`Repeat`); the kitty keyboard protocol's
/// report-event-types flag additionally reports `Repeat` and `Release`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyEventKind {
    Press,
    Repeat,
    Release,
}

impl KeyEventKind {
    /// Whether the key is going (or staying) down — a press or an auto-repeat.
    /// Shortcuts, local scrolling and IME suppression act on these, never on a
    /// release.
    pub fn is_down(self) -> bool {
        !matches!(self, KeyEventKind::Release)
    }
}

/// The codepoints the kitty report-alternate-keys flag (4) wants for a text key,
/// beyond the key itself. `base` is the key with no modifiers applied — the
/// canonical unicode-key-code; `shifted` is what Shift produces (when it differs
/// from `base`); `base_layout` is the key at this physical position on the
/// standard US (PC-101) layout (when it differs from `base`). The shell fills
/// this from the platform; the pure core only reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyAlternates {
    pub base: char,
    pub shifted: Option<char>,
    pub base_layout: Option<char>,
}

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
