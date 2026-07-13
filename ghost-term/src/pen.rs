use crate::color::Color;
use std::num::NonZeroU16;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct Pen {
    pub(crate) foreground: Option<Color>,
    pub(crate) background: Option<Color>,
    pub(crate) intensity: Intensity,
    pub(crate) attrs: u8,
    /// OSC 8 hyperlink the pen is writing under, as an id interned on the
    /// terminal (resolve with `Vt::hyperlink`). Not an SGR attribute: SGR 0
    /// does not clear it, only OSC 8 with an empty URI (or a full reset) does.
    pub(crate) link: Option<NonZeroU16>,
    /// Character protection guarding cells written under this pen from erasure.
    /// Not an SGR attribute (set by DECSCA / SPA-EPA); no visual effect — it only
    /// governs which erases spare the cell. See [`Protection`].
    pub(crate) protection: Protection,
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Intensity {
    Normal,
    Bold,
    Faint,
}

/// A cell's erase protection. Two independent mechanisms set it, and the two
/// erase families honor them differently (matching xterm):
///
/// - [`Protection::Dec`] — DECSCA (`CSI Ps " q`). Spared only by the *selective*
///   erases (DECSED / DECSEL / DECSERA); a plain ED/EL/ECH erases it.
/// - [`Protection::Iso`] — the ISO 6429 guarded area (SPA / EPA). Spared by the
///   *plain* erases (ED / EL / ECH) as well as the selective ones.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub enum Protection {
    #[default]
    None,
    Dec,
    Iso,
}

const ITALIC_MASK: u8 = 1;
const UNDERLINE_MASK: u8 = 1 << 1;
const STRIKETHROUGH_MASK: u8 = 1 << 2;
const BLINK_MASK: u8 = 1 << 3;
const INVERSE_MASK: u8 = 1 << 4;

impl Pen {
    pub fn foreground(&self) -> Option<Color> {
        self.foreground
    }

    pub fn background(&self) -> Option<Color> {
        self.background
    }

    pub fn is_bold(&self) -> bool {
        self.intensity == Intensity::Bold
    }

    pub fn is_faint(&self) -> bool {
        self.intensity == Intensity::Faint
    }

    pub fn is_italic(&self) -> bool {
        (self.attrs & ITALIC_MASK) != 0
    }

    pub fn is_underline(&self) -> bool {
        (self.attrs & UNDERLINE_MASK) != 0
    }

    pub fn is_strikethrough(&self) -> bool {
        (self.attrs & STRIKETHROUGH_MASK) != 0
    }

    pub fn is_blink(&self) -> bool {
        (self.attrs & BLINK_MASK) != 0
    }

    pub fn is_inverse(&self) -> bool {
        (self.attrs & INVERSE_MASK) != 0
    }

    pub fn set_italic(&mut self) {
        self.attrs |= ITALIC_MASK;
    }

    pub fn set_underline(&mut self) {
        self.attrs |= UNDERLINE_MASK;
    }

    pub fn set_blink(&mut self) {
        self.attrs |= BLINK_MASK;
    }

    pub fn set_strikethrough(&mut self) {
        self.attrs |= STRIKETHROUGH_MASK;
    }

    pub fn set_inverse(&mut self) {
        self.attrs |= INVERSE_MASK;
    }

    pub fn unset_italic(&mut self) {
        self.attrs &= !ITALIC_MASK;
    }

    pub fn unset_underline(&mut self) {
        self.attrs &= !UNDERLINE_MASK;
    }

    pub fn unset_blink(&mut self) {
        self.attrs &= !BLINK_MASK;
    }

    pub fn unset_strikethrough(&mut self) {
        self.attrs &= !STRIKETHROUGH_MASK;
    }

    pub fn unset_inverse(&mut self) {
        self.attrs &= !INVERSE_MASK;
    }

    /// The interned id of the OSC 8 hyperlink this pen writes under, if any —
    /// resolve to the URI with `Vt::hyperlink`.
    pub fn link_id(&self) -> Option<u16> {
        self.link.map(NonZeroU16::get)
    }

    /// The erase protection cells written under this pen carry.
    pub(crate) fn protection(&self) -> Protection {
        self.protection
    }

    /// Set the erase protection for subsequent writes (DECSCA / SPA-EPA).
    pub(crate) fn set_protection(&mut self, protection: Protection) {
        self.protection = protection;
    }

    /// This pen minus its hyperlink and protection — what erase/fill operations
    /// use, so blank cells never read as clickable and are never guarded, and
    /// what style comparison uses, since neither is an SGR-visible attribute.
    pub(crate) fn without_link(&self) -> Pen {
        Pen {
            link: None,
            protection: Protection::None,
            ..*self
        }
    }

    /// Whether the two pens agree on everything SGR can express (the link is
    /// carried by OSC 8, not SGR — see `to_sgr_diff`).
    pub(crate) fn same_style(&self, other: &Pen) -> bool {
        self.without_link() == other.without_link()
    }

    pub fn is_default(&self) -> bool {
        self.foreground.is_none()
            && self.background.is_none()
            && self.intensity == Intensity::Normal
            && !self.is_italic()
            && !self.is_underline()
            && !self.is_strikethrough()
            && !self.is_blink()
            && !self.is_inverse()
            && self.link.is_none()
            && self.protection == Protection::None
    }
}

impl Default for Pen {
    fn default() -> Self {
        Pen {
            foreground: None,
            background: None,
            intensity: Intensity::Normal,
            attrs: 0,
            link: None,
            protection: Protection::None,
        }
    }
}
