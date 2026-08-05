//! What the desktop tells us about how a window should look: the titlebar font,
//! which window buttons go on which side, and what a double-click on the bar
//! does.
//!
//! All of it comes from GNOME's `gsettings`, which is a subprocess — so each is
//! asked once and remembered. None of them change mid-session in practice, and
//! the CSD frame we are replacing reads its own copies once for the same reason.

use ghost_ui_core::frame::ButtonLayout;

/// Read one `org.gnome.desktop.wm.preferences` key, or `None` if gsettings
/// cannot answer (not GNOME, not installed, no session bus).
fn wm_preference(key: &str) -> Option<String> {
    let out = std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.wm.preferences", key])
        .output()
        .ok()?;
    String::from_utf8(out.stdout).ok()
}

/// The desktop's configured titlebar font: family, style and point size, as
/// GNOME states it (`Adwaita Sans Bold 11`).
#[derive(Debug, Clone, PartialEq)]
pub struct DesktopFont {
    pub family: String,
    pub style: Option<String>,
    pub pt_size: f32,
}

impl Default for DesktopFont {
    /// What to draw with when the desktop has no opinion (or no gsettings).
    fn default() -> Self {
        DesktopFont {
            family: "sans-serif".into(),
            style: None,
            pt_size: 11.0,
        }
    }
}

impl DesktopFont {
    /// Parse GNOME's `titlebar-font` form: a family, then an optional style,
    /// then an optional size — `Cantarell`, `Cantarell 12`, `Cantarell Bold 12`,
    /// `Noto Serif CJK HK Bold 12`. Only the last word can be the size and only
    /// the one before it can be the style, so a multi-word family survives.
    fn parse(spec: &str) -> Option<Self> {
        let spec = spec.trim().trim_matches('\'').trim();
        if spec.is_empty() {
            return None;
        }
        let mut words: Vec<&str> = spec.split_whitespace().collect();
        let pt_size = match words.last().and_then(|w| w.parse::<f32>().ok()) {
            Some(size) if words.len() > 1 => {
                words.pop();
                size
            }
            _ => Self::default().pt_size,
        };
        // A trailing style word, but never the only word — that is the family.
        let style = match words.last() {
            Some(w) if words.len() > 1 && crate::font::style_weight(Some(w)).is_some() => {
                Some(words.pop()?.to_string())
            }
            _ => None,
        };
        Some(DesktopFont {
            family: words.join(" "),
            style,
            pt_size,
        })
    }

    /// The em size in physical pixels at `scale`, from points at the usual 96 dpi.
    pub fn px_size(&self, scale: f32) -> f32 {
        self.pt_size * (96.0 / 72.0) * scale
    }
}

/// The desktop's titlebar font, asked once. `gsettings` is a subprocess — see the module docs.
pub fn desktop_font() -> DesktopFont {
    static FONT: std::sync::OnceLock<DesktopFont> = std::sync::OnceLock::new();
    FONT.get_or_init(|| {
        wm_preference("titlebar-font")
            .and_then(|s| DesktopFont::parse(&s))
            .unwrap_or_default()
    })
    .clone()
}

/// Which window buttons the desktop wants, and on which side. Defaults to the
/// GNOME arrangement — close alone on the right — when it has no opinion.
pub fn button_layout() -> ButtonLayout {
    static LAYOUT: std::sync::OnceLock<ButtonLayout> = std::sync::OnceLock::new();
    LAYOUT
        .get_or_init(|| {
            wm_preference("button-layout")
                .map(|s| ButtonLayout::parse(&s))
                .filter(|l| !l.is_empty())
                .unwrap_or_else(|| ButtonLayout::parse(":close"))
        })
        .clone()
}

/// What a double-click on the titlebar does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DoubleClick {
    #[default]
    ToggleMaximize,
    Minimize,
    Menu,
    None,
}

impl DoubleClick {
    fn parse(spec: &str) -> Self {
        match spec.trim().trim_matches('\'') {
            "toggle-maximize" => DoubleClick::ToggleMaximize,
            "minimize" => DoubleClick::Minimize,
            "menu" => DoubleClick::Menu,
            // `none`, `lower`, `toggle-shade` — nothing a terminal can do, and
            // doing something else instead would be worse than doing nothing.
            "none" | "lower" | "toggle-shade" => DoubleClick::None,
            _ => DoubleClick::default(),
        }
    }
}

/// The desktop's double-click-titlebar action, asked once.
pub fn double_click_action() -> DoubleClick {
    static ACTION: std::sync::OnceLock<DoubleClick> = std::sync::OnceLock::new();
    *ACTION.get_or_init(|| {
        wm_preference("action-double-click-titlebar")
            .map(|s| DoubleClick::parse(&s))
            .unwrap_or_default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ghost_ui_core::frame::WindowButton;

    #[test]
    fn the_desktop_font_spec_keeps_multi_word_families() {
        // Only the last word can be the size and only the one before it can be a
        // style, so everything left is the family — `Noto Serif CJK HK` is one
        // family, not a family called `Noto` wearing three styles.
        let f = DesktopFont::parse("'Noto Serif CJK HK Bold 12'").expect("parses");
        assert_eq!(f.family, "Noto Serif CJK HK");
        assert_eq!(f.style.as_deref(), Some("Bold"));
        assert_eq!(f.pt_size, 12.0);
    }

    #[test]
    fn a_spec_may_leave_out_the_style_or_the_size() {
        let f = DesktopFont::parse("Cantarell 12").expect("parses");
        assert_eq!((f.family.as_str(), f.style.as_deref()), ("Cantarell", None));
        assert_eq!(f.pt_size, 12.0);

        let f = DesktopFont::parse("Cantarell").expect("parses");
        assert_eq!((f.family.as_str(), f.style.as_deref()), ("Cantarell", None));
        assert_eq!(f.pt_size, DesktopFont::default().pt_size);

        let f = DesktopFont::parse("Adwaita Sans Bold").expect("parses");
        assert_eq!(f.family, "Adwaita Sans");
        assert_eq!(f.style.as_deref(), Some("Bold"));
    }

    #[test]
    fn a_family_named_like_a_style_is_still_a_family() {
        // "Black" is a weight name, but a one-word spec is all family — dropping
        // it would leave us asking for a font with no name at all.
        let f = DesktopFont::parse("Black").expect("parses");
        assert_eq!(f.family, "Black");
        assert_eq!(f.style, None);
    }

    #[test]
    fn an_empty_or_unset_spec_has_no_opinion() {
        assert_eq!(DesktopFont::parse(""), None);
        assert_eq!(DesktopFont::parse("''"), None);
    }

    #[test]
    fn points_become_pixels_at_96dpi_and_scale() {
        let f = DesktopFont {
            pt_size: 12.0,
            ..DesktopFont::default()
        };
        assert_eq!(f.px_size(1.0), 16.0);
        assert_eq!(f.px_size(2.0), 32.0);
    }

    #[test]
    fn a_double_click_action_we_cannot_perform_does_nothing() {
        assert_eq!(
            DoubleClick::parse("'toggle-maximize'"),
            DoubleClick::ToggleMaximize
        );
        assert_eq!(DoubleClick::parse("minimize"), DoubleClick::Minimize);
        // Shading and lowering are window-manager tricks a terminal has no
        // version of; pretending they are "maximize" would be worse.
        assert_eq!(DoubleClick::parse("toggle-shade"), DoubleClick::None);
        assert_eq!(DoubleClick::parse("lower"), DoubleClick::None);
        // An unknown value falls back to what GNOME ships as the default.
        assert_eq!(DoubleClick::parse("wat"), DoubleClick::ToggleMaximize);
    }

    #[test]
    fn a_desktop_with_no_button_preference_still_gets_a_close_button() {
        // A window you cannot close is worse than one whose buttons sit on the
        // wrong side.
        assert!(!button_layout().is_empty());
        let l = ButtonLayout::parse(":close");
        assert_eq!(l.right, vec![WindowButton::Close]);
    }
}
