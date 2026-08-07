//! # Wayland
//!
//! **Note:** Windows don't appear on Wayland until you draw/present to them.
//!
//! By default, Winit loads system libraries using `dlopen`. This can be
//! disabled by disabling the `"wayland-dlopen"` cargo feature.
//!
//! ## Client-side decorations
//!
//! Winit provides client-side decorations by default, but the behaviour can
//! be controlled with the following feature flags:
//!
//! * `wayland-csd-adwaita` (default).
//! * `wayland-csd-adwaita-crossfont`.
//! * `wayland-csd-adwaita-notitle`.

use std::ffi::c_void;
use std::ptr::NonNull;

use crate::dpi::PhysicalSize;
use crate::event_loop::{ActiveEventLoop, EventLoop, EventLoopBuilder};
use crate::monitor::MonitorHandle;
use crate::window::{Window, WindowAttributes};

pub use crate::window::Theme;

/// Space kept outside the window proper for the client to draw into — see
/// [`WindowExtWayland::set_decoration_margins`]. [vendored addition]
#[cfg(wayland_platform)]
pub use crate::platform_impl::wayland::DecorationMargins;

/// Additional methods on [`ActiveEventLoop`] that are specific to Wayland.
pub trait ActiveEventLoopExtWayland {
    /// True if the [`ActiveEventLoop`] uses Wayland.
    fn is_wayland(&self) -> bool;
}

impl ActiveEventLoopExtWayland for ActiveEventLoop {
    #[inline]
    fn is_wayland(&self) -> bool {
        self.p.is_wayland()
    }
}

/// Additional methods on [`EventLoop`] that are specific to Wayland.
pub trait EventLoopExtWayland {
    /// True if the [`EventLoop`] uses Wayland.
    fn is_wayland(&self) -> bool;
}

impl<T: 'static> EventLoopExtWayland for EventLoop<T> {
    #[inline]
    fn is_wayland(&self) -> bool {
        self.event_loop.is_wayland()
    }
}

/// Additional methods on [`EventLoopBuilder`] that are specific to Wayland.
pub trait EventLoopBuilderExtWayland {
    /// Force using Wayland.
    fn with_wayland(&mut self) -> &mut Self;

    /// Whether to allow the event loop to be created off of the main thread.
    ///
    /// By default, the window is only allowed to be created on the main
    /// thread, to make platform compatibility easier.
    fn with_any_thread(&mut self, any_thread: bool) -> &mut Self;
}

impl<T> EventLoopBuilderExtWayland for EventLoopBuilder<T> {
    #[inline]
    fn with_wayland(&mut self) -> &mut Self {
        self.platform_specific.forced_backend = Some(crate::platform_impl::Backend::Wayland);
        self
    }

    #[inline]
    fn with_any_thread(&mut self, any_thread: bool) -> &mut Self {
        self.platform_specific.any_thread = any_thread;
        self
    }
}

/// Additional methods on [`Window`] that are specific to Wayland.
///
/// [`Window`]: crate::window::Window
pub trait WindowExtWayland {
    /// Returns `xdg_toplevel` of the window or [`None`] if the window is X11 window.
    fn xdg_toplevel(&self) -> Option<NonNull<c_void>>;

    /// Whether [`Window::set_blur`] can actually blur this window's backdrop
    /// right now; always `false` for an X11 window.
    ///
    /// Backdrop blur is the compositor's to give and most don't offer it, so
    /// asking for it is a request, not a guarantee. This reports whether the
    /// request will be honoured, which is what a client needs in order to fall
    /// back to drawing its own translucency treatment rather than silently
    /// getting flat alpha where it expected glass.
    ///
    /// Not a constant: the compositor re-advertises its capabilities whenever
    /// they change, so a blur effect switched off mid-session turns this to
    /// `false` while the window is up. Poll it rather than caching it.
    ///
    /// [`Window::set_blur`]: crate::window::Window::set_blur
    fn blur_supported(&self) -> bool;

    /// Whether the compositor has tiled this window against something — a screen
    /// edge, another window, a tiling layout — as opposed to leaving it floating.
    /// Always `false` for an X11 window.
    ///
    /// A tiled window has no free outside corner: its edges meet the screen or a
    /// neighbour, so a client that rounds its corners, draws a drop shadow, or
    /// offers resize handles has to drop all three where it is tiled, exactly as
    /// it does when maximized. `Window::is_maximized` does not cover this — a
    /// half-snapped window is tiled but not maximized.
    ///
    /// True if *any* edge is tiled, which is what the state means in practice: a
    /// half or quarter snap tiles some edges and not others.
    fn is_tiled(&self) -> bool;

    /// Round the bottom corners of the backdrop effect ([`Window::set_blur`]) by
    /// `radius` logical pixels; 0 (the default) leaves it square.
    ///
    /// The effect fills the surface's rectangle, so a client that rounds its own
    /// corners — drawing them transparent — gets the blur at *full* strength in
    /// exactly the pixels it cut away, undimmed by any content of its own: the
    /// window ends in a bright square wedge poking out of its own curve. Setting
    /// the same radius here cuts the effect to the same shape. Applies to
    /// `ext_background_effect_v1`; KDE's older blur protocol takes no region
    /// from us and is left square.
    ///
    /// [`Window::set_blur`]: crate::window::Window::set_blur
    fn set_blur_corner_radii(&self, top: u32, bottom: u32);

    /// Shape the backdrop effect for the buffer the client is about to commit,
    /// whose surface is `size` physical pixels. [vendored addition]
    ///
    /// Call this immediately before presenting, on the thread that presents, and
    /// pass the size of the buffer being presented — not the size the last
    /// configure asked for. The blur region is double-buffered *surface* state:
    /// the compositor applies whatever was set last at the next
    /// `wl_surface.commit`, so a region stated when a configure arrives rides out
    /// on whatever frame the client happens to commit next. A client drawing
    /// through a GPU swapchain presents on its own schedule, so that is a frame
    /// still at the old size, and the region and the window disagree for as long
    /// as the drag lasts — an unblurred band down the edge that is catching up.
    ///
    /// Stating it here instead pairs the region with the buffer it describes,
    /// because the request is written to the connection just ahead of the
    /// `attach`/`commit` that carries the buffer.
    ///
    /// Cheap to call every frame: a shape the compositor already has sends
    /// nothing, and a square window's shape is one oversized rect at every size.
    ///
    /// [`Window::set_blur`]: crate::window::Window::set_blur
    fn set_blur_present_size(&self, size: PhysicalSize<u32>);

    /// Keep `margins` logical pixels of surface *outside* the window proper, for
    /// the client to draw a shadow into. [vendored addition]
    ///
    /// With `decorations(false)` the surface is the window and there is nowhere
    /// to cast a shadow. This grows the surface and points
    /// `xdg_surface.set_window_geometry` at the inner rect, so the compositor
    /// still snaps, maximizes and tiles to the window while the client paints
    /// the ring around it — the model GTK uses (`_GTK_FRAME_EXTENTS` on X11).
    ///
    /// [`Window::inner_size`] then reports the whole SURFACE, which is what the
    /// client has to paint; the window inside it is that less the margins.
    /// Wayland-only, and only meaningful while the client is undecorated.
    ///
    /// Returns the surface's new size, which changes the moment the margins do.
    /// No `Resized` event follows — like [`Window::request_inner_size`], the
    /// caller is told by the return value.
    ///
    /// [`Window::inner_size`]: crate::window::Window::inner_size
    /// [`Window::request_inner_size`]: crate::window::Window::request_inner_size
    fn set_decoration_margins(&self, margins: DecorationMargins) -> PhysicalSize<u32>;
}

impl WindowExtWayland for Window {
    #[inline]
    fn xdg_toplevel(&self) -> Option<NonNull<c_void>> {
        #[allow(clippy::single_match)]
        match &self.window {
            #[cfg(x11_platform)]
            crate::platform_impl::Window::X(_) => None,
            #[cfg(wayland_platform)]
            crate::platform_impl::Window::Wayland(window) => window.xdg_toplevel(),
        }
    }

    #[inline]
    fn blur_supported(&self) -> bool {
        #[allow(clippy::single_match)]
        match &self.window {
            #[cfg(x11_platform)]
            crate::platform_impl::Window::X(_) => false,
            #[cfg(wayland_platform)]
            crate::platform_impl::Window::Wayland(window) => window.blur_supported(),
        }
    }

    #[inline]
    fn is_tiled(&self) -> bool {
        #[allow(clippy::single_match)]
        match &self.window {
            #[cfg(x11_platform)]
            crate::platform_impl::Window::X(_) => false,
            #[cfg(wayland_platform)]
            crate::platform_impl::Window::Wayland(window) => window.is_tiled(),
        }
    }

    #[inline]
    fn set_blur_corner_radii(&self, top: u32, bottom: u32) {
        #[allow(clippy::single_match)]
        match &self.window {
            #[cfg(x11_platform)]
            crate::platform_impl::Window::X(_) => (),
            #[cfg(wayland_platform)]
            crate::platform_impl::Window::Wayland(window) => {
                window.set_blur_corner_radii(top, bottom)
            },
        }
    }

    #[inline]
    fn set_blur_present_size(&self, size: PhysicalSize<u32>) {
        #[allow(clippy::single_match)]
        match &self.window {
            #[cfg(x11_platform)]
            crate::platform_impl::Window::X(_) => (),
            #[cfg(wayland_platform)]
            crate::platform_impl::Window::Wayland(window) => window.set_blur_present_size(size),
        }
    }

    #[inline]
    fn set_decoration_margins(&self, margins: DecorationMargins) -> PhysicalSize<u32> {
        #[allow(clippy::single_match)]
        match &self.window {
            #[cfg(x11_platform)]
            crate::platform_impl::Window::X(_) => self.inner_size(),
            #[cfg(wayland_platform)]
            crate::platform_impl::Window::Wayland(window) => {
                window.set_decoration_margins(margins)
            },
        }
    }
}

/// Additional methods on [`WindowAttributes`] that are specific to Wayland.
pub trait WindowAttributesExtWayland {
    /// Build window with the given name.
    ///
    /// The `general` name sets an application ID, which should match the `.desktop`
    /// file distributed with your program. The `instance` is a `no-op`.
    ///
    /// For details about application ID conventions, see the
    /// [Desktop Entry Spec](https://specifications.freedesktop.org/desktop-entry-spec/desktop-entry-spec-latest.html#desktop-file-id)
    fn with_name(self, general: impl Into<String>, instance: impl Into<String>) -> Self;
}

impl WindowAttributesExtWayland for WindowAttributes {
    #[inline]
    fn with_name(mut self, general: impl Into<String>, instance: impl Into<String>) -> Self {
        self.platform_specific.name =
            Some(crate::platform_impl::ApplicationName::new(general.into(), instance.into()));
        self
    }
}

/// Additional methods on `MonitorHandle` that are specific to Wayland.
pub trait MonitorHandleExtWayland {
    /// Returns the inner identifier of the monitor.
    fn native_id(&self) -> u32;
}

impl MonitorHandleExtWayland for MonitorHandle {
    #[inline]
    fn native_id(&self) -> u32 {
        self.inner.native_identifier()
    }
}
