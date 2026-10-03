//! What the desktop tells us about how a window should look: the titlebar font,
//! which window buttons go on which side, what a double-click on the bar does,
//! and the colours to draw the bar in.
//!
//! On Linux all of it comes from GNOME's `gsettings`, which is a subprocess — so
//! each is asked once and remembered. None of them change mid-session in
//! practice, and the CSD frame we replaced read its own copies once for the same
//! reason. On macOS it comes from AppKit and CoreText, asked once for the same
//! reason, and the window buttons are not ours at all: the traffic lights stay
//! AppKit's, sitting on our bar.

use ghost_render::scene::Rgba;
#[cfg(not(target_os = "macos"))]
use ghost_ui_core::frame::ButtonLayout;

/// Read one key of one schema, or `None` if gsettings cannot answer (not GNOME,
/// not installed, no session bus).
#[cfg(not(target_os = "macos"))]
fn setting(schema: &str, key: &str) -> Option<String> {
    let out = std::process::Command::new("gsettings")
        .args(["get", schema, key])
        .output()
        .ok()?;
    String::from_utf8(out.stdout).ok()
}

#[cfg(not(target_os = "macos"))]
fn wm_preference(key: &str) -> Option<String> {
    setting("org.gnome.desktop.wm.preferences", key)
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
    #[cfg(not(target_os = "macos"))]
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

    /// The em size in physical pixels at `scale`. GNOME states points at the
    /// usual 96 dpi; a macOS point is a logical pixel.
    pub fn px_size(&self, scale: f32) -> f32 {
        let px_per_pt = if cfg!(target_os = "macos") {
            1.0
        } else {
            96.0 / 72.0
        };
        self.pt_size * px_per_pt * scale
    }
}

/// The desktop's titlebar font, asked once. `gsettings` is a subprocess — see the module docs.
#[cfg(not(target_os = "macos"))]
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
#[cfg(not(target_os = "macos"))]
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

/// The colours a window frame is drawn in, for one focus state.
///
/// Adwaita's, transcribed from the GTK and libadwaita stylesheets — the same
/// values the CSD frame we replaced used, so a ghost window sits alongside the
/// rest of the desktop rather than beside it.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct FrameColors {
    /// The headerbar fill.
    pub bg: Rgba,
    /// The title text on it.
    pub fg: Rgba,
    /// The hairline traced around the outside of the whole window: the last
    /// layer of Adwaita's `decoration` box-shadow, `0 0 0 1px rgba(0,0,0,0.75)`
    /// dark and `rgba(0,0,0,0.23)` light, both read out of the stylesheets
    /// compiled into `libgtk-3.so.0`. The dark value matches a measured
    /// gnome-terminal edge (~0.73 over the wallpaper).
    ///
    /// libadwaita's own is a far fainter `rgb(0 0 0/5%)`, but it can afford
    /// that: a GTK4 window is opaque, so its *fill* draws the edge and the ring
    /// only darkens the transition. ghost is translucent — at 5% there is
    /// nothing at the boundary at all, and the outline stops dead where the
    /// headerbar ends.
    pub outline: f32,
}

#[cfg(not(target_os = "macos"))]
const fn rgb(r: u8, g: u8, b: u8) -> Rgba {
    [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0]
}

/// Whether the desktop asks for a dark window frame. `color-scheme` is GNOME's
/// own key; the cross-desktop route is the `org.freedesktop.appearance` portal,
/// which is worth reaching for only once a non-GNOME desktop is in the picture.
#[cfg(not(target_os = "macos"))]
fn prefer_dark() -> bool {
    setting("org.gnome.desktop.interface", "color-scheme")
        .is_some_and(|s| s.contains("prefer-dark"))
}

/// The frame colours for a focused or backdropped window, asked once.
///
/// Read once for the reason the module docs give, and so a light/dark switch
/// mid-session does not repaint half the windows — the frame does not follow
/// one, exactly as the frame it replaced did not.
#[cfg(not(target_os = "macos"))]
pub fn frame_colors(focused: bool) -> FrameColors {
    static COLORS: std::sync::OnceLock<[FrameColors; 2]> = std::sync::OnceLock::new();
    COLORS.get_or_init(|| {
        if prefer_dark() {
            [
                FrameColors {
                    bg: rgb(34, 34, 38),
                    fg: rgb(144, 144, 144),
                    outline: 191.0 / 255.0,
                },
                FrameColors {
                    bg: rgb(46, 46, 50),
                    fg: rgb(255, 255, 255),
                    outline: 191.0 / 255.0,
                },
            ]
        } else {
            [
                FrameColors {
                    bg: rgb(250, 250, 251),
                    fg: rgb(150, 150, 150),
                    outline: 59.0 / 255.0,
                },
                FrameColors {
                    bg: rgb(255, 255, 255),
                    fg: rgb(47, 47, 47),
                    outline: 59.0 / 255.0,
                },
            ]
        }
    })[usize::from(focused)]
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
    #[cfg(not(target_os = "macos"))]
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
#[cfg(not(target_os = "macos"))]
pub fn double_click_action() -> DoubleClick {
    static ACTION: std::sync::OnceLock<DoubleClick> = std::sync::OnceLock::new();
    *ACTION.get_or_init(|| {
        wm_preference("action-double-click-titlebar")
            .map(|s| DoubleClick::parse(&s))
            .unwrap_or_default()
    })
}

#[cfg(target_os = "macos")]
mod macos {
    use super::{DesktopFont, DoubleClick, FrameColors};
    use ghost_render::scene::Rgba;
    use ghost_ui_core::frame::ButtonLayout;

    /// The style name a CoreText normalized weight (-1 to 1) is nearest to, on
    /// the scale `NSFontWeight` names: regular 0, medium 0.23, semibold 0.3,
    /// bold 0.4, heavy 0.56, black 0.62, and light -0.4 below.
    pub(super) fn weight_style(normalized: f64) -> Option<&'static str> {
        const NAMED: [(f64, &str); 9] = [
            (-0.8, "ExtraLight"),
            (-0.6, "Thin"),
            (-0.4, "Light"),
            (0.0, "Regular"),
            (0.23, "Medium"),
            (0.3, "Semibold"),
            (0.4, "Bold"),
            (0.56, "Heavy"),
            (0.62, "Black"),
        ];
        let (_, name) = NAMED.iter().min_by(|a, b| {
            (a.0 - normalized)
                .abs()
                .total_cmp(&(b.0 - normalized).abs())
        })?;
        (*name != "Regular").then_some(*name)
    }

    /// The system's window-title font — see [`crate::font::window_title_font`].
    pub fn desktop_font() -> DesktopFont {
        use core_text::font_descriptor::TraitAccessors;
        static FONT: std::sync::OnceLock<DesktopFont> = std::sync::OnceLock::new();
        FONT.get_or_init(|| {
            let font = crate::font::window_title_font();
            DesktopFont {
                family: font.family_name(),
                style: weight_style(font.all_traits().normalized_weight()).map(str::to_owned),
                pt_size: font.pt_size() as f32,
            }
        })
        .clone()
    }

    /// None of ours: the traffic lights are AppKit's own, sitting on our bar,
    /// so they stay what VoiceOver, AX automation and the tiling tools find a
    /// window by — and keep zoom, the tiling popover and fullscreen with them.
    pub fn button_layout() -> ButtonLayout {
        ButtonLayout::default()
    }

    /// `color` as sRGB components, resolved in `appearance` — AppKit's semantic
    /// colours are dynamic, and only have values inside an appearance.
    fn resolve(
        appearance: &objc2_app_kit::NSAppearance,
        color: impl Fn() -> objc2::rc::Retained<objc2_app_kit::NSColor>,
    ) -> Rgba {
        use objc2_app_kit::NSColorSpace;
        let out = std::cell::Cell::new([0.0f32, 0.0, 0.0, 1.0]);
        let block = block2::StackBlock::new(|| {
            // SAFETY: plain AppKit colour reads on the main thread, inside the
            // appearance `performAsCurrentDrawingAppearance` makes current.
            unsafe {
                if let Some(c) = color().colorUsingColorSpace(&NSColorSpace::sRGBColorSpace()) {
                    out.set([
                        c.redComponent() as f32,
                        c.greenComponent() as f32,
                        c.blueComponent() as f32,
                        c.alphaComponent() as f32,
                    ]);
                }
            }
        });
        // SAFETY: the block runs synchronously, before this returns.
        unsafe { appearance.performAsCurrentDrawingAppearance(&block) };
        out.get()
    }

    /// `fg` laid over an opaque `bg`, so the title is one solid colour however
    /// translucent AppKit's label colours are.
    fn over(fg: Rgba, bg: Rgba) -> Rgba {
        let a = fg[3];
        let mix = |f: f32, b: f32| f * a + b * (1.0 - a);
        [mix(fg[0], bg[0]), mix(fg[1], bg[1]), mix(fg[2], bg[2]), 1.0]
    }

    /// The bar in the window's own background colour, with its title in the
    /// label colour AppKit dims for a window in the background — the colours a
    /// native titlebar draws in, in whichever appearance the app is in. Asked
    /// once, for the reason the module docs give.
    pub fn frame_colors(focused: bool) -> FrameColors {
        use objc2_app_kit::{NSApplication, NSColor};
        static COLORS: std::sync::OnceLock<[FrameColors; 2]> = std::sync::OnceLock::new();
        COLORS.get_or_init(|| {
            let Some(mtm) = objc2_foundation::MainThreadMarker::new() else {
                return [FrameColors::default(); 2];
            };
            let appearance = NSApplication::sharedApplication(mtm).effectiveAppearance();
            // SAFETY: class-method reads of AppKit's semantic colours.
            let bg = resolve(&appearance, || unsafe { NSColor::windowBackgroundColor() });
            let bg = [bg[0], bg[1], bg[2], 1.0];
            let label = resolve(&appearance, || unsafe { NSColor::labelColor() });
            let dim = resolve(&appearance, || unsafe { NSColor::tertiaryLabelColor() });
            [
                FrameColors {
                    bg,
                    fg: over(dim, bg),
                    outline: 0.0,
                },
                FrameColors {
                    bg,
                    fg: over(label, bg),
                    outline: 0.0,
                },
            ]
        })[usize::from(focused)]
    }

    /// Read the global `AppleActionOnDoubleClick` (System Settings › Desktop &
    /// Dock › "Double-click a window's title bar to"), falling back to the older
    /// `AppleMiniaturizeOnDoubleClick` switch it replaced.
    pub(super) fn parse_double_click(action: Option<&str>, miniaturize: bool) -> DoubleClick {
        match action {
            // "Zoom" in the settings pane; "Fill" is its tiling-era sibling,
            // which for a window that is not tiled is the same toggle.
            Some("Maximize" | "Fill") => DoubleClick::ToggleMaximize,
            Some("Minimize") => DoubleClick::Minimize,
            Some("None") => DoubleClick::None,
            _ if miniaturize => DoubleClick::Minimize,
            _ => DoubleClick::ToggleMaximize,
        }
    }

    pub fn double_click_action() -> DoubleClick {
        use objc2_foundation::{NSString, NSUserDefaults};
        static ACTION: std::sync::OnceLock<DoubleClick> = std::sync::OnceLock::new();
        *ACTION.get_or_init(|| {
            // SAFETY: reads of the user's global defaults domain.
            let defaults = unsafe { NSUserDefaults::standardUserDefaults() };
            let action =
                unsafe { defaults.stringForKey(&NSString::from_str("AppleActionOnDoubleClick")) }
                    .map(|s| s.to_string());
            let miniaturize = unsafe {
                defaults.boolForKey(&NSString::from_str("AppleMiniaturizeOnDoubleClick"))
            };
            parse_double_click(action.as_deref(), miniaturize)
        })
    }
}

#[cfg(target_os = "macos")]
pub use macos::{button_layout, desktop_font, double_click_action, frame_colors};

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use super::DoubleClick;
    use super::macos::{parse_double_click, weight_style};

    #[test]
    fn the_title_weight_is_named_from_coretexts_scale() {
        // The window-title font reports bold as 0.4 — it has to come out as a
        // name `style_weight` knows, or the title draws at the regular weight.
        assert_eq!(weight_style(0.4), Some("Bold"));
        assert_eq!(weight_style(0.3), Some("Semibold"));
        assert_eq!(weight_style(0.0), None);
        assert_eq!(weight_style(0.05), None);
        for w in [-0.8, -0.6, -0.4, 0.23, 0.3, 0.4, 0.56, 0.62] {
            let name = weight_style(w).expect("named");
            assert!(
                crate::font::style_weight(Some(name)).is_some(),
                "{name} is no weight the chrome text path knows"
            );
        }
    }

    #[test]
    fn a_title_double_click_does_what_system_settings_says() {
        assert_eq!(
            parse_double_click(Some("Maximize"), false),
            DoubleClick::ToggleMaximize
        );
        assert_eq!(
            parse_double_click(Some("Fill"), false),
            DoubleClick::ToggleMaximize
        );
        assert_eq!(
            parse_double_click(Some("Minimize"), false),
            DoubleClick::Minimize
        );
        assert_eq!(parse_double_click(Some("None"), true), DoubleClick::None);
        // Never set: the old switch decides, and with neither it is zoom,
        // which is what macOS does out of the box.
        assert_eq!(parse_double_click(None, true), DoubleClick::Minimize);
        assert_eq!(parse_double_click(None, false), DoubleClick::ToggleMaximize);
    }
}

#[cfg(all(test, not(target_os = "macos")))]
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
    fn the_frame_wears_the_desktops_own_colours() {
        // Transcribed values, so what they are worth pinning against is
        // Adwaita itself: a headerbar that is #2e2e32 dark and white light,
        // legible title text on each, and the outer ring at the stylesheet's
        // 0.75 / 0.23 rather than libadwaita's 5%, which vanishes against a
        // translucent window.
        let dark = FrameColors {
            bg: rgb(46, 46, 50),
            fg: rgb(255, 255, 255),
            outline: 191.0 / 255.0,
        };
        assert!((dark.outline - 0.75).abs() < 0.01);
        let light = FrameColors {
            bg: rgb(255, 255, 255),
            fg: rgb(47, 47, 47),
            outline: 59.0 / 255.0,
        };
        assert!((light.outline - 0.23).abs() < 0.01);
        // Whichever this desktop asks for, a focused window's bar stands
        // forward of a backdropped one and its title is the stronger of the
        // two against it.
        let (focused, backdrop) = (frame_colors(true), frame_colors(false));
        assert!(focused == dark || focused == light);
        let contrast = |c: &FrameColors| (c.fg[0] - c.bg[0]).abs();
        assert!(contrast(&focused) > contrast(&backdrop));
        assert_ne!(focused.bg, backdrop.bg);
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
