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

/// The process's handle on its toplevel session — see
/// [`ActiveEventLoopExtWayland::open_session`]. [vendored addition]
#[cfg(wayland_platform)]
pub use crate::platform_impl::wayland::types::session_management::{SessionReason, SessionShared};

/// Additional methods on [`ActiveEventLoop`] that are specific to Wayland.
pub trait ActiveEventLoopExtWayland {
    /// True if the [`ActiveEventLoop`] uses Wayland.
    fn is_wayland(&self) -> bool;

    /// Open this process's toplevel session, so the compositor remembers where
    /// the windows named through
    /// [`WindowAttributesExtWayland::with_session_toplevel`] were, and restores
    /// them on a later run. [vendored addition]
    ///
    /// Pass the identifier a previous run persisted, or [`None`] for a first
    /// run. An identifier the compositor has since forgotten is *not* an error:
    /// it is treated as [`None`], and a fresh one arrives on the returned
    /// handle. So persist [`SessionShared::id`] rather than the id you passed,
    /// or a forgotten session is asked for forever.
    ///
    /// `reason` says how the instance came to be started. Compositors are free
    /// to restore more state for a recovered or session-restored instance than
    /// for a freshly launched one, and to decide differently whether a restored
    /// window may take focus.
    ///
    /// Returns [`None`] where the compositor does not offer the protocol, on
    /// X11, or if a session is already open — a second request for a live
    /// identifier is a protocol error that would disconnect the application, so
    /// it is refused here rather than sent.
    #[cfg(wayland_platform)]
    fn open_session(
        &self,
        reason: SessionReason,
        id: Option<&str>,
    ) -> Option<std::sync::Arc<SessionShared>>;

    /// Forget the session opened by [`open_session`](Self::open_session) and
    /// every window state the compositor stored against it, then allow a new one
    /// to be opened. For a launch that means to start over from nothing.
    /// [vendored addition]
    #[cfg(wayland_platform)]
    fn remove_session(&self);

    /// Forget one named toplevel and the state stored against it, leaving the
    /// rest of the session alone. [vendored addition]
    ///
    /// Named rather than taken as a [`Window`] because a client that forgets a
    /// window on close generally does so once the window is already gone.
    #[cfg(wayland_platform)]
    fn remove_session_toplevel(&self, name: String);
}

impl ActiveEventLoopExtWayland for ActiveEventLoop {
    #[inline]
    fn is_wayland(&self) -> bool {
        self.p.is_wayland()
    }

    #[cfg(wayland_platform)]
    #[inline]
    fn open_session(
        &self,
        reason: SessionReason,
        id: Option<&str>,
    ) -> Option<std::sync::Arc<SessionShared>> {
        match &self.p {
            crate::platform_impl::ActiveEventLoop::Wayland(window_target) => {
                window_target.open_session(reason, id)
            },
            #[cfg(x11_platform)]
            _ => None,
        }
    }

    #[cfg(wayland_platform)]
    #[inline]
    fn remove_session(&self) {
        #[allow(irrefutable_let_patterns)]
        if let crate::platform_impl::ActiveEventLoop::Wayland(window_target) = &self.p {
            window_target.remove_session();
        }
    }

    #[cfg(wayland_platform)]
    #[inline]
    fn remove_session_toplevel(&self, name: String) {
        #[allow(irrefutable_let_patterns)]
        if let crate::platform_impl::ActiveEventLoop::Wayland(window_target) = &self.p {
            window_target.remove_session_toplevel(name);
        }
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

    /// State the window's size to the compositor for the buffer the client is
    /// about to commit, whose surface is `size` physical pixels. [vendored
    /// addition]
    ///
    /// Call this immediately before presenting, on the thread that presents, and
    /// pass the size of the buffer being presented — not the size the last
    /// configure asked for. Everything the compositor learns about how big a
    /// window is — `xdg_surface.set_window_geometry`, the decoration frame's
    /// size and the placement of its subsurfaces, the viewport destination, the
    /// backdrop-blur region — is double-buffered state, applied at the surface's
    /// next `wl_surface.commit`. Sent when a configure arrives it rides out on
    /// whatever the client commits next, and a client drawing through a GPU
    /// swapchain has frames queued from before that configure: the window's
    /// edges take the new size around content that is still the old one. On a
    /// left-edge drag, where the compositor anchors placement from the geometry,
    /// the whole window shifts while the buffer stays short and the shortfall
    /// shows as bare desktop down the other edge.
    ///
    /// Nor is refusing to commit early enough on its own, because the commit is
    /// not the client's to time — a Vulkan/EGL driver puts the `attach` and
    /// `commit` on the wire from its own presentation thread, and a configure
    /// handled meanwhile lands between them. Stating it here is what pairs it
    /// with the buffer it describes: the requests are written to the connection
    /// just ahead of the `attach`/`commit` that carries it.
    ///
    /// Cheap to call every frame: state the compositor already has sends
    /// nothing, and a buffer that answers no configure applies nothing.
    ///
    /// [`Window::set_blur`]: crate::window::Window::set_blur
    fn set_present_size(&self, size: PhysicalSize<u32>);

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

    /// The size of the window *proper*: [`Window::inner_size`] less the margins
    /// in force. [vendored addition]
    ///
    /// The two differ only while margins are set, and the difference is not
    /// their pixelated total: the surface grows by the margins' LOGICAL total
    /// and that sum is rounded once, so a client splitting the ring back into
    /// per-side pixels must take its totals from here rather than rounding each
    /// side of its own margins. Falls back to the surface size wherever margins
    /// cannot be set (X11).
    ///
    /// [`Window::inner_size`]: crate::window::Window::inner_size
    fn geometry_size(&self) -> PhysicalSize<u32>;

    /// Whether the compositor restored remembered state into this window's first
    /// configure, for a window named through
    /// [`WindowAttributesExtWayland::with_session_toplevel`]. [vendored addition]
    ///
    /// `false` for an unnamed window, on X11, and for a name the compositor had
    /// nothing stored against — which is the ordinary first-run case, not an
    /// error. Settled by the time the window exists, because the event that sets
    /// it is pinned to arrive before the first configure and window creation
    /// waits for that configure.
    ///
    /// A client that wants to fall back to its own remembered size when the
    /// compositor had nothing needs this; one that is happy to open at its
    /// default size does not.
    fn session_restored(&self) -> bool;
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
    fn set_present_size(&self, size: PhysicalSize<u32>) {
        #[allow(clippy::single_match)]
        match &self.window {
            #[cfg(x11_platform)]
            crate::platform_impl::Window::X(_) => (),
            #[cfg(wayland_platform)]
            crate::platform_impl::Window::Wayland(window) => window.set_present_size(size),
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

    #[inline]
    fn geometry_size(&self) -> PhysicalSize<u32> {
        match &self.window {
            #[cfg(x11_platform)]
            crate::platform_impl::Window::X(_) => self.inner_size(),
            #[cfg(wayland_platform)]
            crate::platform_impl::Window::Wayland(window) => window.geometry_size(),
        }
    }

    #[inline]
    fn session_restored(&self) -> bool {
        match &self.window {
            #[cfg(x11_platform)]
            crate::platform_impl::Window::X(_) => false,
            #[cfg(wayland_platform)]
            crate::platform_impl::Window::Wayland(window) => window.session_restored(),
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

    /// Identify this window within the session opened by
    /// [`ActiveEventLoopExtWayland::open_session`], so the compositor restores
    /// whatever it remembers about a window of that name — geometry, workspace,
    /// maximized or fullscreen state — into the window's first configure.
    /// [vendored addition]
    ///
    /// The name must be stable across runs and unique within the session; it is
    /// the client's job to pick one that survives a restart, since a name the
    /// next run cannot reproduce restores nothing. A name the compositor has
    /// nothing stored against is not an error — the window simply opens as
    /// asked, and its state is remembered under that name from then on.
    ///
    /// Restored state arrives as an ordinary configure, so a client that already
    /// follows configures needs no further work; use
    /// [`WindowExtWayland::session_restored`] only to find out whether anything
    /// was in fact restored.
    ///
    /// Ignored on X11, and where the compositor does not offer the protocol.
    fn with_session_toplevel(self, name: impl Into<String>) -> Self;
}

impl WindowAttributesExtWayland for WindowAttributes {
    #[inline]
    fn with_name(mut self, general: impl Into<String>, instance: impl Into<String>) -> Self {
        self.platform_specific.name =
            Some(crate::platform_impl::ApplicationName::new(general.into(), instance.into()));
        self
    }

    #[inline]
    fn with_session_toplevel(mut self, name: impl Into<String>) -> Self {
        self.platform_specific.session_toplevel = Some(name.into());
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
