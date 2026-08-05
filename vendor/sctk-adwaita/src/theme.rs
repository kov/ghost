pub use tiny_skia::Color;
use tiny_skia::{Paint, Shader};

// https://gitlab.gnome.org/GNOME/gtk/-/blob/1bf88f1d81043fd99740e2f91e56ade7ede7303b/gtk/gtkwindow.c#L165
pub(crate) const RESIZE_HANDLE_SIZE: u32 = 12;
// https://gitlab.gnome.org/GNOME/gtk/-/blob/1bf88f1d81043fd99740e2f91e56ade7ede7303b/gtk/gtkwindow.c#L166
pub(crate) const RESIZE_HANDLE_CORNER_SIZE: u32 = 24;
pub(crate) const BORDER_SIZE: u32 = crate::shadow::SHADOW_SIZE + VISIBLE_BORDER_SIZE;
pub(crate) const HEADER_SIZE: u32 = 35;
pub(crate) const CORNER_RADIUS: u32 = 10;
pub(crate) const VISIBLE_BORDER_SIZE: u32 = 1;

/// The color theme to use with the decorations frame.
#[derive(Debug, Clone)]
pub struct ColorTheme {
    pub active: ColorMap,
    pub inactive: ColorMap,
}

impl ColorTheme {
    /// Automatically choose between light & dark themes based on:
    /// * dbus org.freedesktop.portal.Settings
    ///   <https://flatpak.github.io/xdg-desktop-portal/#gdbus-interface-org-freedesktop-portal-Settings>
    pub fn auto() -> Self {
        match crate::config::prefer_dark() {
            true => Self::dark(),
            false => Self::light(),
        }
    }

    /// Predefined light variant, which aims to replecate Adwaita theme.
    pub fn light() -> Self {
        Self {
            active: ColorMap {
                headerbar: Color::from_rgba8(235, 235, 235, 255),
                button_idle: Color::from_rgba8(216, 216, 216, 255),
                button_hover: Color::from_rgba8(207, 207, 207, 255),
                button_icon: Color::from_rgba8(42, 42, 42, 255),
                border_color: Color::from_rgba8(220, 220, 220, 255),
                outer_border: light_outer_border(),
                font_color: Color::from_rgba8(47, 47, 47, 255),
            },
            inactive: ColorMap {
                headerbar: Color::from_rgba8(250, 250, 250, 255),
                button_idle: Color::from_rgba8(240, 240, 240, 255),
                button_hover: Color::from_rgba8(216, 216, 216, 255),
                button_icon: Color::from_rgba8(148, 148, 148, 255),
                border_color: Color::from_rgba8(220, 220, 220, 255),
                outer_border: light_outer_border(),
                font_color: Color::from_rgba8(150, 150, 150, 255),
            },
        }
    }

    /// Predefined dark variant, which aims to replecate Adwaita-dark theme.
    pub fn dark() -> Self {
        Self {
            active: ColorMap {
                headerbar: Color::from_rgba8(48, 48, 48, 255),
                button_idle: Color::from_rgba8(69, 69, 69, 255),
                button_hover: Color::from_rgba8(79, 79, 79, 255),
                button_icon: Color::from_rgba8(255, 255, 255, 255),
                border_color: Color::from_rgba8(58, 58, 58, 255),
                outer_border: dark_outer_border(),
                font_color: Color::from_rgba8(255, 255, 255, 255),
            },
            inactive: ColorMap {
                headerbar: Color::from_rgba8(36, 36, 36, 255),
                button_idle: Color::from_rgba8(47, 47, 47, 255),
                button_hover: Color::from_rgba8(57, 57, 57, 255),
                button_icon: Color::from_rgba8(144, 144, 144, 255),
                border_color: Color::from_rgba8(58, 58, 58, 255),
                outer_border: dark_outer_border(),
                font_color: Color::from_rgba8(144, 144, 144, 255),
            },
        }
    }

    pub(crate) fn for_state(&self, active: bool) -> &ColorMap {
        if active {
            &self.active
        } else {
            &self.inactive
        }
    }
}

impl Default for ColorTheme {
    fn default() -> Self {
        Self::auto()
    }
}

/// The color map for various decorcation parts.
#[derive(Debug, Clone)]
pub struct ColorMap {
    pub headerbar: Color,
    pub button_idle: Color,
    pub button_hover: Color,
    pub button_icon: Color,
    pub border_color: Color,
    /// The hairline that traces the outside of the whole window.
    pub outer_border: Color,
    pub font_color: Color,
}

impl ColorMap {
    pub(crate) fn headerbar_paint(&self) -> Paint {
        Paint {
            shader: Shader::SolidColor(self.headerbar),
            anti_alias: true,
            ..Default::default()
        }
    }

    pub(crate) fn button_idle_paint(&self) -> Paint {
        Paint {
            shader: Shader::SolidColor(self.button_idle),
            anti_alias: true,
            ..Default::default()
        }
    }

    pub(crate) fn button_hover_paint(&self) -> Paint {
        Paint {
            shader: Shader::SolidColor(self.button_hover),
            anti_alias: true,
            ..Default::default()
        }
    }

    pub(crate) fn button_icon_paint(&self) -> Paint {
        Paint {
            shader: Shader::SolidColor(self.button_icon),
            ..Default::default()
        }
    }

    pub(crate) fn border_paint(&self) -> Paint {
        Paint {
            shader: Shader::SolidColor(self.border_color),
            ..Default::default()
        }
    }

    /// Kept apart from [`ColorMap::border_paint`], which is the headerbar's
    /// bottom separator — a different line with a different job.
    pub(crate) fn outer_border_paint(&self) -> Paint {
        Paint {
            shader: Shader::SolidColor(self.outer_border),
            anti_alias: true,
            ..Default::default()
        }
    }
}

/// The ring GTK's Adwaita draws around the outside of every window — the last
/// layer of the `decoration` box-shadow, `0 0 0 1px rgba(0,0,0,0.75)` in the
/// dark stylesheet and `rgba(0,0,0,0.23)` in the light one. Both were read out
/// of the stylesheets compiled into `libgtk-3.so.0`, and the dark value matches
/// a measured gnome-terminal edge (~0.73 over the wallpaper).
///
/// libadwaita's own value is a far fainter `rgb(0 0 0/5%)`, but it can afford
/// that: a GTK4 window is opaque, so its *fill* draws the edge and the ring only
/// darkens the transition. ghost is translucent — with 5% there is nothing at
/// the boundary at all, and the window's outline stops dead where the headerbar
/// ends.
fn dark_outer_border() -> Color {
    Color::from_rgba8(0, 0, 0, 191)
}

fn light_outer_border() -> Color {
    Color::from_rgba8(0, 0, 0, 59)
}
