//! The state of the window, which is shared with the event-loop.

use std::num::NonZeroU32;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use ahash::HashSet;
use tracing::{info, warn};

use sctk::reexports::client::backend::ObjectId;
use sctk::reexports::client::protocol::wl_seat::WlSeat;
use sctk::reexports::client::protocol::wl_shm::WlShm;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::{Connection, Proxy, QueueHandle};
use sctk::reexports::csd_frame::{
    DecorationsFrame, FrameAction, FrameClick, ResizeEdge, WindowState as XdgWindowState,
};
use sctk::reexports::protocols::wp::fractional_scale::v1::client::wp_fractional_scale_v1::WpFractionalScaleV1;
use sctk::reexports::protocols::wp::text_input::zv3::client::zwp_text_input_v3::ZwpTextInputV3;
use sctk::reexports::protocols::wp::viewporter::client::wp_viewport::WpViewport;
use sctk::reexports::protocols::xdg::shell::client::xdg_toplevel::ResizeEdge as XdgResizeEdge;

use sctk::compositor::{CompositorState, Region, SurfaceData, SurfaceDataExt};
use sctk::seat::pointer::{PointerDataExt, ThemedPointer};
use sctk::shell::xdg::window::{DecorationMode, Window, WindowConfigure};
use sctk::shell::xdg::XdgSurface;
use sctk::shell::WaylandSurface;
use sctk::shm::slot::SlotPool;
use sctk::shm::Shm;
use sctk::subcompositor::SubcompositorState;
use wayland_protocols_plasma::blur::client::org_kde_kwin_blur::OrgKdeKwinBlur;

use crate::cursor::CustomCursor as RootCustomCursor;
use crate::dpi::{LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize, Size};
use crate::error::{ExternalError, NotSupportedError};
use crate::platform_impl::wayland::logical_to_physical_rounded;
use crate::platform_impl::wayland::types::cursor::{CustomCursor, SelectedCursor};
use crate::platform_impl::wayland::types::background_effect::{
    BackgroundEffectManager, ExtBackgroundEffectSurfaceV1, WHOLE_SURFACE,
};
use crate::platform_impl::wayland::types::kwin_blur::KWinBlurManager;
use crate::platform_impl::{PlatformCustomCursor, WindowId};
use crate::window::{CursorGrabMode, CursorIcon, ImePurpose, ResizeDirection, Theme};

use crate::platform_impl::wayland::seat::{
    PointerConstraintsState, WinitPointerData, WinitPointerDataExt, ZwpTextInputV3Ext,
};
use crate::platform_impl::wayland::state::{WindowCompositorUpdate, WinitState};

#[cfg(feature = "sctk-adwaita")]
pub type WinitFrame = sctk_adwaita::AdwaitaFrame<WinitState>;
#[cfg(not(feature = "sctk-adwaita"))]
pub type WinitFrame = sctk::shell::xdg::fallback_frame::FallbackFrame<WinitState>;

// Minimum window inner size.
const MIN_WINDOW_SIZE: LogicalSize<u32> = LogicalSize::new(2, 1);

/// Logical-pixel margins a client keeps *outside* the window proper, drawn by
/// the client itself — a drop shadow, most of the time. [vendored addition]
///
/// With `decorations(false)` the surface is the window: there is nowhere to cast
/// a shadow, because every pixel the client owns is a pixel the compositor
/// treats as window. Margins buy that space back the way GTK does it: the
/// surface grows, and `xdg_surface.set_window_geometry` tells the compositor
/// that only the inner rect is the window — so maximizing, snapping, tiling and
/// the visual bounds all follow the inner rect while the client still gets to
/// paint the ring around it.
///
/// The surface keeps taking input across the whole of itself, which is how a
/// GTK window lets you grab its resize handles out in the shadow.
///
/// Only meaningful while the client draws its own decorations: with a frame,
/// sctk's own subsurfaces already own the space around the window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DecorationMargins {
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
    pub left: u32,
}

impl DecorationMargins {
    pub const NONE: Self = Self { top: 0, right: 0, bottom: 0, left: 0 };

    /// Nothing outside the window — the surface *is* the window.
    pub fn is_none(&self) -> bool {
        *self == Self::NONE
    }

    /// The surface that holds a window of `geometry` size plus these margins.
    fn inflate(&self, geometry: LogicalSize<u32>) -> LogicalSize<u32> {
        LogicalSize::new(
            geometry.width.saturating_add(self.left + self.right),
            geometry.height.saturating_add(self.top + self.bottom),
        )
    }

    /// The window inside a surface of `surface` size. Never empty: a surface
    /// smaller than its own margins is a window mid-resize, and a zero-sized
    /// geometry is a protocol error.
    fn deflate(&self, surface: LogicalSize<u32>) -> LogicalSize<u32> {
        LogicalSize::new(
            surface.width.saturating_sub(self.left + self.right).max(1),
            surface.height.saturating_sub(self.top + self.bottom).max(1),
        )
    }
}

/// The state of the window which is being updated from the [`WinitState`].
pub struct WindowState {
    /// The connection to Wayland server.
    pub connection: Connection,

    /// The `Shm` to set cursor.
    pub shm: WlShm,

    // A shared pool where to allocate custom cursors.
    custom_cursor_pool: Arc<Mutex<SlotPool>>,

    /// The last received configure.
    pub last_configure: Option<WindowConfigure>,

    /// The pointers observed on the window.
    pub pointers: Vec<Weak<ThemedPointer<WinitPointerData>>>,

    selected_cursor: SelectedCursor,

    /// Whether the cursor is visible.
    pub cursor_visible: bool,

    /// Pointer constraints to lock/confine pointer.
    pub pointer_constraints: Option<Arc<PointerConstraintsState>>,

    /// Queue handle.
    pub queue_handle: QueueHandle<WinitState>,

    /// Theme variant.
    theme: Option<Theme>,

    /// The current window title.
    title: String,

    /// Whether the frame is resizable.
    resizable: bool,

    // NOTE: we can't use simple counter, since it's racy when seat getting destroyed and new
    // is created, since add/removed stuff could be delivered a bit out of order.
    /// Seats that has keyboard focus on that window.
    seat_focus: HashSet<ObjectId>,

    /// The scale factor of the window.
    scale_factor: f64,

    /// Whether the window is transparent.
    transparent: bool,

    /// The state of the compositor to create WlRegions.
    compositor: Arc<CompositorState>,

    /// The current cursor grabbing mode.
    cursor_grab_mode: GrabState,

    /// Whether the IME input is allowed for that window.
    ime_allowed: bool,

    /// The current IME purpose.
    ime_purpose: ImePurpose,

    /// The text inputs observed on the window.
    text_inputs: Vec<ZwpTextInputV3>,

    /// The inner size of the window, as in without client side decorations.
    size: LogicalSize<u32>,

    /// Whether the CSD fail to create, so we don't try to create them on each iteration.
    csd_fails: bool,

    /// Whether we should decorate the frame.
    decorate: bool,

    /// Min size.
    min_inner_size: LogicalSize<u32>,
    max_inner_size: Option<LogicalSize<u32>>,
    resize_increments: Option<LogicalSize<u32>>,

    /// The size of the window when no states were applied to it. The primary use for it
    /// is to fallback to original window size, before it was maximized, if the compositor
    /// sends `None` for the new size in the configure.
    stateless_size: LogicalSize<u32>,

    /// Initial window size provided by the user. Removed on the first
    /// configure.
    initial_size: Option<Size>,

    /// The state of the frame callback.
    frame_callback_state: FrameCallbackState,

    viewport: Option<WpViewport>,
    fractional_scale: Option<WpFractionalScaleV1>,
    blur: Option<OrgKdeKwinBlur>,
    blur_manager: Option<KWinBlurManager>,

    /// The cross-desktop background-effect object and its manager, used in
    /// preference to the KDE pair above — see [`set_blur`](Self::set_blur).
    /// [vendored addition]
    background_effect: Option<ExtBackgroundEffectSurfaceV1>,
    background_effect_manager: Option<BackgroundEffectManager>,
    /// The shape of that effect — the radii the client rounds its own corners by,
    /// so the blur can be cut to the same curve rather than a square box behind
    /// it — together with what the compositor has already been told. See
    /// [`BlurRegion`], which is also where the *timing* of that telling lives.
    /// [vendored addition]
    blur_shape: BlurRegion,
    /// The size a configure asked for, waiting for a buffer that size to be
    /// stated with. See [`PendingSize`]. [vendored addition]
    pending_size: PendingSize,

    /// Space the client keeps outside the window proper, to draw a shadow into.
    /// See [`DecorationMargins`]. [vendored addition]
    decoration_margins: DecorationMargins,

    /// Whether the client side decorations have pending move operations.
    ///
    /// The value is the serial of the event triggered moved.
    has_pending_move: Option<u32>,

    /// The underlying SCTK window.
    pub window: Window,

    // NOTE: The spec says that destroying parent(`window` in our case), will unmap the
    // subsurfaces. Thus to achieve atomic unmap of the client, drop the decorations
    // frame after the `window` is dropped. To achieve that we rely on rust's struct
    // field drop order guarantees.
    /// The window frame, which is created from the configure request.
    frame: Option<WinitFrame>,
}

impl WindowState {
    /// Create new window state.
    pub fn new(
        connection: Connection,
        queue_handle: &QueueHandle<WinitState>,
        winit_state: &WinitState,
        initial_size: Size,
        window: Window,
        theme: Option<Theme>,
    ) -> Self {
        let compositor = winit_state.compositor_state.clone();
        let pointer_constraints = winit_state.pointer_constraints.clone();
        let viewport = winit_state
            .viewporter_state
            .as_ref()
            .map(|state| state.get_viewport(window.wl_surface(), queue_handle));
        let fractional_scale = winit_state
            .fractional_scaling_manager
            .as_ref()
            .map(|fsm| fsm.fractional_scaling(window.wl_surface(), queue_handle));

        Self {
            blur: None,
            blur_manager: winit_state.kwin_blur_manager.clone(),
            background_effect: None,
            background_effect_manager: winit_state.background_effect_manager.clone(),
            blur_shape: BlurRegion::default(),
            pending_size: PendingSize::default(),
            decoration_margins: DecorationMargins::NONE,
            compositor,
            connection,
            csd_fails: false,
            cursor_grab_mode: GrabState::new(),
            selected_cursor: Default::default(),
            cursor_visible: true,
            decorate: true,
            fractional_scale,
            frame: None,
            frame_callback_state: FrameCallbackState::None,
            seat_focus: Default::default(),
            has_pending_move: None,
            ime_allowed: false,
            ime_purpose: ImePurpose::Normal,
            last_configure: None,
            max_inner_size: None,
            min_inner_size: MIN_WINDOW_SIZE,
            resize_increments: None,
            pointer_constraints,
            pointers: Default::default(),
            queue_handle: queue_handle.clone(),
            resizable: true,
            scale_factor: 1.,
            shm: winit_state.shm.wl_shm().clone(),
            custom_cursor_pool: winit_state.custom_cursor_pool.clone(),
            size: initial_size.to_logical(1.),
            stateless_size: initial_size.to_logical(1.),
            initial_size: Some(initial_size),
            text_inputs: Vec::new(),
            theme,
            title: String::default(),
            transparent: false,
            viewport,
            window,
        }
    }

    /// Apply closure on the given pointer.
    fn apply_on_pointer<F: FnMut(&ThemedPointer<WinitPointerData>, &WinitPointerData)>(
        &self,
        mut callback: F,
    ) {
        self.pointers.iter().filter_map(Weak::upgrade).for_each(|pointer| {
            let data = pointer.pointer().winit_data();
            callback(pointer.as_ref(), data);
        })
    }

    /// Get the current state of the frame callback.
    pub fn frame_callback_state(&self) -> FrameCallbackState {
        self.frame_callback_state
    }

    /// The frame callback was received, but not yet sent to the user.
    pub fn frame_callback_received(&mut self) {
        self.frame_callback_state = FrameCallbackState::Received;
    }

    /// Reset the frame callbacks state.
    pub fn frame_callback_reset(&mut self) {
        self.frame_callback_state = FrameCallbackState::None;
    }

    /// Request a frame callback if we don't have one for this window in flight.
    pub fn request_frame_callback(&mut self) {
        let surface = self.window.wl_surface();
        match self.frame_callback_state {
            FrameCallbackState::None | FrameCallbackState::Received => {
                self.frame_callback_state = FrameCallbackState::Requested;
                surface.frame(&self.queue_handle, surface.clone());
            },
            FrameCallbackState::Requested => (),
        }
    }

    pub fn configure(
        &mut self,
        configure: WindowConfigure,
        shm: &Shm,
        subcompositor: &Option<Arc<SubcompositorState>>,
    ) -> bool {
        // NOTE: when using fractional scaling or wl_compositor@v6 the scaling
        // should be delivered before the first configure, thus apply it to
        // properly scale the physical sizes provided by the users.
        if let Some(initial_size) = self.initial_size.take() {
            self.size = initial_size.to_logical(self.scale_factor());
            self.stateless_size = self.size;
        }

        if let Some(subcompositor) = subcompositor.as_ref().filter(|_| {
            configure.decoration_mode == DecorationMode::Client
                && self.frame.is_none()
                && !self.csd_fails
        }) {
            match WinitFrame::new(
                &self.window,
                shm,
                #[cfg(feature = "sctk-adwaita")]
                self.compositor.clone(),
                subcompositor.clone(),
                self.queue_handle.clone(),
                #[cfg(feature = "sctk-adwaita")]
                into_sctk_adwaita_config(self.theme),
            ) {
                Ok(mut frame) => {
                    frame.set_title(&self.title);
                    frame.set_scaling_factor(self.scale_factor);
                    // Hide the frame if we were asked to not decorate.
                    frame.set_hidden(!self.decorate);
                    self.frame = Some(frame);
                },
                Err(err) => {
                    warn!("Failed to create client side decorations frame: {err}");
                    self.csd_fails = true;
                },
            }
        } else if configure.decoration_mode == DecorationMode::Server {
            // Drop the frame for server side decorations to save resources.
            self.frame = None;
        }

        let stateless = Self::is_stateless(&configure);

        // Whether the size below came from the compositor. It sizes the window
        // *geometry*, which our margins sit outside of, so that one has to be
        // inflated into the surface we actually paint; the fallbacks are surface
        // sizes of ours already. [vendored addition]
        let mut from_compositor = false;
        let (mut new_size, constrain) = if let Some(frame) = self.frame.as_mut() {
            // Configure the window states.
            frame.update_state(configure.state);

            match configure.new_size {
                (Some(width), Some(height)) => {
                    let (width, height) = frame.subtract_borders(width, height);
                    let width = width.map(|w| w.get()).unwrap_or(1);
                    let height = height.map(|h| h.get()).unwrap_or(1);
                    from_compositor = true;
                    ((width, height).into(), false)
                },
                (..) if stateless => (self.stateless_size, true),
                _ => (self.size, true),
            }
        } else {
            match configure.new_size {
                (Some(width), Some(height)) => {
                    from_compositor = true;
                    ((width.get(), height.get()).into(), false)
                },
                _ if stateless => (self.stateless_size, true),
                _ => (self.size, true),
            }
        };
        // Both branches, not just the undecorated one: an undecorated window
        // still HAS a frame object here — hidden, subtracting no borders — so
        // the branch above is the one it takes, and putting the inflate only in
        // the other left the surface exactly the size the compositor asked the
        // *window* to be. The window then came out a margin short of its own
        // maximized or snapped area, with a band of dead surface down two edges.
        if from_compositor {
            let margins = if Self::is_boxed_in(&configure) {
                DecorationMargins::NONE
            } else {
                self.decoration_margins
            };
            new_size = margins.inflate(new_size);
        }

        // Apply configure bounds only when compositor let the user decide what size to pick.
        if constrain {
            let bounds = self.inner_size_bounds(&configure);
            new_size.width =
                bounds.0.map(|bound_w| new_size.width.min(bound_w.get())).unwrap_or(new_size.width);
            new_size.height = bounds
                .1
                .map(|bound_h| new_size.height.min(bound_h.get()))
                .unwrap_or(new_size.height);
        }

        // Apply size increments.
        //
        // We conditionally apply increments to avoid conflicts with the compositor's layout rules:
        // 1. If the window is floating (constrain == true), we snap to increments to ensure the
        //    app's grid alignment.
        // 2. If the user is interactively resizing (is_resizing), we snap the size to provide
        //    feedback.
        //
        // However, we MUST NOT snap if the compositor enforces a specific size (constrain == false,
        // or states like Maximized/Tiled). Snapping in these cases (e.g. corner tiling) would
        // shrink the window below the allocated area, creating visible gaps between valid
        // windows or screen edges.
        if (constrain || configure.is_resizing())
            && !configure.is_maximized()
            && !configure.is_fullscreen()
            && !configure.is_tiled()
        {
            if let Some(increments) = self.resize_increments {
                // We use min size as a base size for the increments, similar to how X11 does it.
                //
                // This ensures that we can always reach the min size and the increments are
                // calculated from it.
                let (delta_width, delta_height) = (
                    new_size.width.saturating_sub(self.min_inner_size.width),
                    new_size.height.saturating_sub(self.min_inner_size.height),
                );

                let width =
                    self.min_inner_size.width + (delta_width / increments.width) * increments.width;
                let height = self.min_inner_size.height
                    + (delta_height / increments.height) * increments.height;

                new_size = (width, height).into();
            }
        }

        let new_state = configure.state;
        let old_state = self.last_configure.as_ref().map(|configure| configure.state);

        let state_change_requires_resize = old_state
            .map(|old_state| {
                !old_state
                    .symmetric_difference(new_state)
                    .difference(XdgWindowState::ACTIVATED | XdgWindowState::SUSPENDED)
                    .is_empty()
            })
            // NOTE: `None` is present for the initial configure, thus we must always resize.
            .unwrap_or(true);

        // NOTE: Set the configure before doing a resize, since we query it during it.
        self.last_configure = Some(configure);

        if state_change_requires_resize || new_size != self.inner_size() {
            self.resize(new_size);
            true
        } else {
            false
        }
    }

    /// Compute the bounds for the inner size of the surface.
    fn inner_size_bounds(
        &self,
        configure: &WindowConfigure,
    ) -> (Option<NonZeroU32>, Option<NonZeroU32>) {
        let configure_bounds = match configure.suggested_bounds {
            Some((width, height)) => (NonZeroU32::new(width), NonZeroU32::new(height)),
            None => (None, None),
        };

        if let Some(frame) = self.frame.as_ref() {
            let (width, height) = frame.subtract_borders(
                configure_bounds.0.unwrap_or(NonZeroU32::new(1).unwrap()),
                configure_bounds.1.unwrap_or(NonZeroU32::new(1).unwrap()),
            );
            (configure_bounds.0.and(width), configure_bounds.1.and(height))
        } else {
            configure_bounds
        }
    }

    #[inline]
    fn is_stateless(configure: &WindowConfigure) -> bool {
        // NOTE: sctk's `is_tiled()` is `state.contains(TILED)`, where `TILED` is the
        // union of ALL FOUR edges — so a half/quarter snap (e.g. GNOME's Super+Right
        // sends TILED_RIGHT|TOP|BOTTOM, with no LEFT) reports `is_tiled() == false`.
        // Check the individual edges too, so a partially-tiled window counts as
        // non-stateless and its compositor-forced size is not recorded as the
        // floating restore size (which left the window stuck snapped on un-snap).
        // [vendored fix]
        !(configure.is_maximized()
            || configure.is_fullscreen()
            || configure.is_tiled()
            || configure.is_tiled_left()
            || configure.is_tiled_right()
            || configure.is_tiled_top()
            || configure.is_tiled_bottom())
    }

    /// Start interacting drag resize.
    pub fn drag_resize_window(&self, direction: ResizeDirection) -> Result<(), ExternalError> {
        let xdg_toplevel = self.window.xdg_toplevel();

        // TODO(kchibisov) handle touch serials.
        self.apply_on_pointer(|_, data| {
            let serial = data.latest_button_serial();
            let seat = data.seat();
            xdg_toplevel.resize(seat, serial, direction.into());
        });

        Ok(())
    }

    /// Start the window drag.
    pub fn drag_window(&self) -> Result<(), ExternalError> {
        let xdg_toplevel = self.window.xdg_toplevel();
        // TODO(kchibisov) handle touch serials.
        self.apply_on_pointer(|_, data| {
            let serial = data.latest_button_serial();
            let seat = data.seat();
            xdg_toplevel._move(seat, serial);
        });

        Ok(())
    }

    /// Tells whether the window should be closed.
    #[allow(clippy::too_many_arguments)]
    pub fn frame_click(
        &mut self,
        click: FrameClick,
        pressed: bool,
        seat: &WlSeat,
        serial: u32,
        timestamp: Duration,
        window_id: WindowId,
        updates: &mut Vec<WindowCompositorUpdate>,
    ) -> Option<bool> {
        match self.frame.as_mut()?.on_click(timestamp, click, pressed)? {
            FrameAction::Minimize => self.window.set_minimized(),
            FrameAction::Maximize => self.window.set_maximized(),
            FrameAction::UnMaximize => self.window.unset_maximized(),
            FrameAction::Close => WinitState::queue_close(updates, window_id),
            FrameAction::Move => self.has_pending_move = Some(serial),
            FrameAction::Resize(edge) => {
                let edge = match edge {
                    ResizeEdge::None => XdgResizeEdge::None,
                    ResizeEdge::Top => XdgResizeEdge::Top,
                    ResizeEdge::Bottom => XdgResizeEdge::Bottom,
                    ResizeEdge::Left => XdgResizeEdge::Left,
                    ResizeEdge::TopLeft => XdgResizeEdge::TopLeft,
                    ResizeEdge::BottomLeft => XdgResizeEdge::BottomLeft,
                    ResizeEdge::Right => XdgResizeEdge::Right,
                    ResizeEdge::TopRight => XdgResizeEdge::TopRight,
                    ResizeEdge::BottomRight => XdgResizeEdge::BottomRight,
                    _ => return None,
                };
                self.window.resize(seat, serial, edge);
            },
            FrameAction::ShowMenu(x, y) => self.window.show_window_menu(seat, serial, (x, y)),
            _ => (),
        };

        Some(false)
    }

    pub fn frame_point_left(&mut self) {
        if let Some(frame) = self.frame.as_mut() {
            frame.click_point_left();
        }
    }

    // Move the point over decorations.
    pub fn frame_point_moved(
        &mut self,
        seat: &WlSeat,
        surface: &WlSurface,
        timestamp: Duration,
        x: f64,
        y: f64,
    ) -> Option<CursorIcon> {
        // Take the serial if we had any, so it doesn't stick around.
        let serial = self.has_pending_move.take();

        if let Some(frame) = self.frame.as_mut() {
            let cursor = frame.click_point_moved(timestamp, &surface.id(), x, y);
            // If we have a cursor change, that means that cursor is over the decorations,
            // so try to apply move.
            if let Some(serial) = cursor.is_some().then_some(serial).flatten() {
                self.window.move_(seat, serial);
                None
            } else {
                cursor
            }
        } else {
            None
        }
    }

    /// Get the stored resizable state.
    #[inline]
    pub fn resizable(&self) -> bool {
        self.resizable
    }

    /// Set the resizable state on the window.
    ///
    /// Returns `true` when the state was applied.
    #[inline]
    pub fn set_resizable(&mut self, resizable: bool) -> bool {
        if self.resizable == resizable {
            return false;
        }

        self.resizable = resizable;
        if resizable {
            // Restore min/max sizes of the window.
            self.reload_min_max_hints();
        } else {
            self.set_min_inner_size(Some(self.size));
            self.set_max_inner_size(Some(self.size));
        }

        // Reload the state on the frame as well.
        if let Some(frame) = self.frame.as_mut() {
            frame.set_resizable(resizable);
        }

        true
    }

    /// Whether the window is focused by any seat.
    #[inline]
    pub fn has_focus(&self) -> bool {
        !self.seat_focus.is_empty()
    }

    /// Whether the IME is allowed.
    #[inline]
    pub fn ime_allowed(&self) -> bool {
        self.ime_allowed
    }

    /// Get the size of the window.
    #[inline]
    pub fn inner_size(&self) -> LogicalSize<u32> {
        self.size
    }

    /// Whether the window received initial configure event from the compositor.
    #[inline]
    pub fn is_configured(&self) -> bool {
        self.last_configure.is_some()
    }

    #[inline]
    pub fn is_decorated(&mut self) -> bool {
        let csd = self
            .last_configure
            .as_ref()
            .map(|configure| configure.decoration_mode == DecorationMode::Client)
            .unwrap_or(false);
        if let Some(frame) = csd.then_some(self.frame.as_ref()).flatten() {
            !frame.is_hidden()
        } else {
            // Server side decorations.
            true
        }
    }

    /// Get the outer size of the window.
    #[inline]
    pub fn outer_size(&self) -> LogicalSize<u32> {
        self.frame
            .as_ref()
            .map(|frame| frame.add_borders(self.size.width, self.size.height).into())
            .unwrap_or(self.size)
    }

    /// Register pointer on the top-level.
    pub fn pointer_entered(&mut self, added: Weak<ThemedPointer<WinitPointerData>>) {
        self.pointers.push(added);
        self.reload_cursor_style();

        let mode = self.cursor_grab_mode.user_grab_mode;
        let _ = self.set_cursor_grab_inner(mode);
    }

    /// Pointer has left the top-level.
    pub fn pointer_left(&mut self, removed: Weak<ThemedPointer<WinitPointerData>>) {
        let mut new_pointers = Vec::new();
        for pointer in self.pointers.drain(..) {
            if let Some(pointer) = pointer.upgrade() {
                if pointer.pointer() != removed.upgrade().unwrap().pointer() {
                    new_pointers.push(Arc::downgrade(&pointer));
                }
            }
        }

        self.pointers = new_pointers;
    }

    /// Refresh the decorations frame if it's present returning whether the client should redraw.
    pub fn refresh_frame(&mut self) -> bool {
        if let Some(frame) = self.frame.as_mut() {
            if !frame.is_hidden() && frame.is_dirty() {
                return frame.draw();
            }
        }

        false
    }

    /// Reload the cursor style on the given window.
    pub fn reload_cursor_style(&mut self) {
        if self.cursor_visible {
            match &self.selected_cursor {
                SelectedCursor::Named(icon) => self.set_cursor(*icon),
                SelectedCursor::Custom(cursor) => self.apply_custom_cursor(cursor),
            }
        } else {
            self.set_cursor_visible(self.cursor_visible);
        }
    }

    /// Reissue the transparency hint to the compositor.
    pub fn reload_transparency_hint(&self) {
        let surface = self.window.wl_surface();

        if self.transparent {
            surface.set_opaque_region(None);
        } else if let Ok(region) = Region::new(&*self.compositor) {
            region.add(0, 0, i32::MAX, i32::MAX);
            surface.set_opaque_region(Some(region.wl_region()));
        } else {
            warn!("Failed to mark window opaque.");
        }
    }

    /// Try to resize the window when the user can do so.
    pub fn request_inner_size(&mut self, inner_size: Size) -> PhysicalSize<u32> {
        if self.last_configure.as_ref().map(Self::is_stateless).unwrap_or(true) {
            self.resize(inner_size.to_logical(self.scale_factor()))
        }

        logical_to_physical_rounded(self.inner_size(), self.scale_factor())
    }

    /// Resize the window to the new inner size.
    fn resize(&mut self, inner_size: LogicalSize<u32>) {
        self.size = inner_size;

        // Deliberately NOT where the backdrop shape is respecified, though it is
        // spelled out in surface coordinates and this is where those change: the
        // region is double-buffered surface state and the buffer for this size
        // does not exist yet, so a region stated here would ride out on whatever
        // frame the client commits next — one still at the old size. It is stated
        // at present time instead, against the buffer being committed. See
        // [`BlurRegion`]. [vendored addition]

        // Update the stateless (restore) size. `is_stateless` now recognises
        // partially-tiled (snapped) windows, so a snap no longer corrupts it.
        if Some(true) == self.last_configure.as_ref().map(Self::is_stateless) {
            self.stateless_size = inner_size;
        }

        // Nothing is told to the compositor from here: everything below is
        // double-buffered state that would take effect around a buffer of the
        // size before. It waits for the frame drawn at this size. [vendored
        // addition] See [`PendingSize`].
        self.pending_size.wants(inner_size);
    }

    /// Tell the compositor how big the window is, now that there is a buffer that
    /// size to say it with. Does nothing until the presented buffer answers the
    /// configure being held. [vendored addition] See [`PendingSize`].
    fn apply_pending_size(&mut self, surface: PhysicalSize<u32>, scale: f64) {
        if !self.pending_size.take_for(surface, scale) {
            return;
        }

        // Update the inner frame.
        let ((x, y), outer_size) = if let Some(frame) = self.frame.as_mut() {
            // Resize only visible frame.
            if !frame.is_hidden() {
                frame.resize(
                    NonZeroU32::new(self.size.width).unwrap(),
                    NonZeroU32::new(self.size.height).unwrap(),
                );
            }

            (frame.location(), frame.add_borders(self.size.width, self.size.height).into())
        } else {
            ((0, 0), self.size)
        };
        // Our own margins run the other way to a frame's borders — the surface
        // is bigger than the window, so the geometry is the inner rect — and
        // they are applied on top of whatever the frame said. Not in the `else`
        // alone: an UNDECORATED window still has a frame object, hidden, adding
        // no borders, so that is the branch it takes. Left there, the geometry
        // stayed the whole surface, the compositor never learned the window was
        // smaller than it, and a maximize both fell a margin short of its own
        // area and came back a margin bigger every time. Margins are only ever
        // set while undecorated, so with a real frame this is a no-op.
        // [vendored addition]
        let m = self.margins_now();
        let ((x, y), outer_size) = (
            (x + m.left as i32, y + m.top as i32),
            m.deflate(outer_size),
        );

        // Reload the hint.
        self.reload_transparency_hint();

        // Set the window geometry.
        self.window.xdg_surface().set_window_geometry(
            x,
            y,
            outer_size.width as i32,
            outer_size.height as i32,
        );

        // Update the target viewport, this is used if and only if fractional scaling is in use.
        if let Some(viewport) = self.viewport.as_ref() {
            // Set inner size without the borders.
            viewport.set_destination(self.size.width as _, self.size.height as _);
        }

    }

    /// Keep `margins` logical pixels of surface outside the window proper — see
    /// [`DecorationMargins`]. Re-configures immediately so the geometry and the
    /// surface agree from here on. [vendored addition]
    pub fn set_decoration_margins(&mut self, margins: DecorationMargins) -> LogicalSize<u32> {
        if self.decoration_margins == margins {
            return self.size;
        }
        // The window the compositor knows about must not move or resize under
        // the user just because we changed how much shadow we paint: hold the
        // geometry and let the surface take the difference.
        let geometry = self.margins_now().deflate(self.size);
        self.decoration_margins = margins;
        let now = self.margins_now();
        self.resize(now.inflate(geometry));
        self.size
    }

    /// The margins currently kept outside the window. [vendored addition]
    pub fn decoration_margins(&self) -> DecorationMargins {
        self.decoration_margins
    }

    /// The margins in force *right now*. A window with no free outside edge —
    /// maximized, fullscreen, tiled — has none: its geometry must fill what the
    /// compositor gave it exactly, and there is nowhere outside it to draw.
    ///
    /// State-derived rather than set by the client, so a maximize needs no
    /// second resize to drop them and no third to take them back. Sized twice
    /// for one state change, the compositor gets two differently-sized buffers
    /// in the middle of its own animation. [vendored addition]
    fn margins_now(&self) -> DecorationMargins {
        match self.last_configure.as_ref() {
            Some(c) if Self::is_boxed_in(c) => DecorationMargins::NONE,
            _ => self.decoration_margins,
        }
    }

    /// No free outside edge: the window's geometry has to fill what the
    /// compositor gave it exactly. A half-snapped window is tiled against ONE
    /// edge and reports it there — `is_tiled` alone is only the all-four case,
    /// and taking it for the whole answer left a snapped window still keeping
    /// room for a shadow it does not cast, overflowing its own tile.
    /// [vendored addition]
    fn is_boxed_in(configure: &WindowConfigure) -> bool {
        configure.is_maximized()
            || configure.is_fullscreen()
            || configure.is_tiled()
            || configure.is_tiled_left()
            || configure.is_tiled_right()
            || configure.is_tiled_top()
            || configure.is_tiled_bottom()
    }

    /// Get the scale factor of the window.
    #[inline]
    pub fn scale_factor(&self) -> f64 {
        self.scale_factor
    }

    /// Set the cursor icon.
    pub fn set_cursor(&mut self, cursor_icon: CursorIcon) {
        self.selected_cursor = SelectedCursor::Named(cursor_icon);

        if !self.cursor_visible {
            return;
        }

        self.apply_on_pointer(|pointer, _| {
            if pointer.set_cursor(&self.connection, cursor_icon).is_err() {
                warn!("Failed to set cursor to {:?}", cursor_icon);
            }
        })
    }

    /// Set the custom cursor icon.
    pub(crate) fn set_custom_cursor(&mut self, cursor: RootCustomCursor) {
        let cursor = match cursor {
            RootCustomCursor { inner: PlatformCustomCursor::Wayland(cursor) } => cursor.0,
            #[cfg(x11_platform)]
            RootCustomCursor { inner: PlatformCustomCursor::X(_) } => {
                tracing::error!("passed a X11 cursor to Wayland backend");
                return;
            },
        };

        let cursor = {
            let mut pool = self.custom_cursor_pool.lock().unwrap();
            CustomCursor::new(&mut pool, &cursor)
        };

        if self.cursor_visible {
            self.apply_custom_cursor(&cursor);
        }

        self.selected_cursor = SelectedCursor::Custom(cursor);
    }

    /// Set the resize increments of the window.
    pub fn set_resize_increments(&mut self, increments: Option<LogicalSize<u32>>) {
        self.resize_increments = increments;
        // NOTE: We don't update the window size here, because it will be done on the next resize
        // or configure event.
    }

    /// Get the resize increments of the window.
    pub fn resize_increments(&self) -> Option<LogicalSize<u32>> {
        self.resize_increments
    }

    fn apply_custom_cursor(&self, cursor: &CustomCursor) {
        self.apply_on_pointer(|pointer, data| {
            let surface = pointer.surface();

            let scale = if let Some(viewport) = data.viewport() {
                let scale = self.scale_factor();
                let size = PhysicalSize::new(cursor.w, cursor.h).to_logical(scale);
                viewport.set_destination(size.width, size.height);
                scale
            } else {
                let scale = surface.data::<SurfaceData>().unwrap().surface_data().scale_factor();
                surface.set_buffer_scale(scale);
                scale as f64
            };

            surface.attach(Some(cursor.buffer.wl_buffer()), 0, 0);
            if surface.version() >= 4 {
                surface.damage_buffer(0, 0, cursor.w, cursor.h);
            } else {
                let size = PhysicalSize::new(cursor.w, cursor.h).to_logical(scale);
                surface.damage(0, 0, size.width, size.height);
            }
            surface.commit();

            let serial = pointer
                .pointer()
                .data::<WinitPointerData>()
                .and_then(|data| data.pointer_data().latest_enter_serial())
                .unwrap();

            let hotspot =
                PhysicalPosition::new(cursor.hotspot_x, cursor.hotspot_y).to_logical(scale);
            pointer.pointer().set_cursor(serial, Some(surface), hotspot.x, hotspot.y);
        });
    }

    /// Set maximum inner window size.
    pub fn set_min_inner_size(&mut self, size: Option<LogicalSize<u32>>) {
        // Ensure that the window has the right minimum size.
        let mut size = size.unwrap_or(MIN_WINDOW_SIZE);
        size.width = size.width.max(MIN_WINDOW_SIZE.width);
        size.height = size.height.max(MIN_WINDOW_SIZE.height);

        // Add the borders.
        let size = self
            .frame
            .as_ref()
            .map(|frame| frame.add_borders(size.width, size.height).into())
            .unwrap_or(size);

        self.min_inner_size = size;
        self.window.set_min_size(Some(size.into()));
    }

    /// Set maximum inner window size.
    pub fn set_max_inner_size(&mut self, size: Option<LogicalSize<u32>>) {
        let size = size.map(|size| {
            self.frame
                .as_ref()
                .map(|frame| frame.add_borders(size.width, size.height).into())
                .unwrap_or(size)
        });

        self.max_inner_size = size;
        self.window.set_max_size(size.map(Into::into));
    }

    /// Set the CSD theme.
    pub fn set_theme(&mut self, theme: Option<Theme>) {
        self.theme = theme;
        #[cfg(feature = "sctk-adwaita")]
        if let Some(frame) = self.frame.as_mut() {
            frame.set_config(into_sctk_adwaita_config(theme))
        }
    }

    /// The current theme for CSD decorations.
    #[inline]
    pub fn theme(&self) -> Option<Theme> {
        self.theme
    }

    /// Set the cursor grabbing state on the top-level.
    pub fn set_cursor_grab(&mut self, mode: CursorGrabMode) -> Result<(), ExternalError> {
        if self.cursor_grab_mode.user_grab_mode == mode {
            return Ok(());
        }

        self.set_cursor_grab_inner(mode)?;
        // Update user grab on success.
        self.cursor_grab_mode.user_grab_mode = mode;
        Ok(())
    }

    /// Reload the hints for minimum and maximum sizes.
    pub fn reload_min_max_hints(&mut self) {
        self.set_min_inner_size(Some(self.min_inner_size));
        self.set_max_inner_size(self.max_inner_size);
    }

    /// Set the grabbing state on the surface.
    fn set_cursor_grab_inner(&mut self, mode: CursorGrabMode) -> Result<(), ExternalError> {
        let pointer_constraints = match self.pointer_constraints.as_ref() {
            Some(pointer_constraints) => pointer_constraints,
            None if mode == CursorGrabMode::None => return Ok(()),
            None => return Err(ExternalError::NotSupported(NotSupportedError::new())),
        };

        let mut unset_old = false;
        match self.cursor_grab_mode.current_grab_mode {
            CursorGrabMode::None => unset_old = true,
            CursorGrabMode::Confined => self.apply_on_pointer(|_, data| {
                data.unconfine_pointer();
                unset_old = true;
            }),
            CursorGrabMode::Locked => {
                self.apply_on_pointer(|_, data| {
                    data.unlock_pointer();
                    unset_old = true;
                });
            },
        }

        // In case we haven't unset the old mode, it means that we don't have a cursor above
        // the window, thus just wait for it to re-appear.
        if !unset_old {
            return Ok(());
        }

        let mut set_mode = false;
        let surface = self.window.wl_surface();
        match mode {
            CursorGrabMode::Locked => self.apply_on_pointer(|pointer, data| {
                let pointer = pointer.pointer();
                data.lock_pointer(pointer_constraints, surface, pointer, &self.queue_handle);
                set_mode = true;
            }),
            CursorGrabMode::Confined => self.apply_on_pointer(|pointer, data| {
                let pointer = pointer.pointer();
                data.confine_pointer(pointer_constraints, surface, pointer, &self.queue_handle);
                set_mode = true;
            }),
            CursorGrabMode::None => {
                // Current lock/confine was already removed.
                set_mode = true;
            },
        }

        // Replace the current grab mode after we've ensure that it got updated.
        if set_mode {
            self.cursor_grab_mode.current_grab_mode = mode;
        }

        Ok(())
    }

    pub fn show_window_menu(&self, position: LogicalPosition<u32>) {
        // TODO(kchibisov) handle touch serials.
        self.apply_on_pointer(|_, data| {
            let serial = data.latest_button_serial();
            let seat = data.seat();
            self.window.show_window_menu(seat, serial, position.into());
        });
    }

    /// Set the position of the cursor.
    pub fn set_cursor_position(&self, position: LogicalPosition<f64>) -> Result<(), ExternalError> {
        if self.pointer_constraints.is_none() {
            return Err(ExternalError::NotSupported(NotSupportedError::new()));
        }

        // Position can be set only for locked cursor.
        if self.cursor_grab_mode.current_grab_mode != CursorGrabMode::Locked {
            return Err(ExternalError::Os(os_error!(crate::platform_impl::OsError::Misc(
                "cursor position can be set only for locked cursor."
            ))));
        }

        self.apply_on_pointer(|_, data| {
            data.set_locked_cursor_position(position.x, position.y);
        });

        Ok(())
    }

    /// Set the visibility state of the cursor.
    pub fn set_cursor_visible(&mut self, cursor_visible: bool) {
        self.cursor_visible = cursor_visible;

        if self.cursor_visible {
            match &self.selected_cursor {
                SelectedCursor::Named(icon) => self.set_cursor(*icon),
                SelectedCursor::Custom(cursor) => self.apply_custom_cursor(cursor),
            }
        } else {
            for pointer in self.pointers.iter().filter_map(|pointer| pointer.upgrade()) {
                let latest_enter_serial = pointer.pointer().winit_data().latest_enter_serial();

                pointer.pointer().set_cursor(latest_enter_serial, None, 0, 0);
            }
        }
    }

    /// Whether show or hide client side decorations.
    #[inline]
    pub fn set_decorate(&mut self, decorate: bool) {
        if decorate == self.decorate {
            return;
        }

        self.decorate = decorate;

        match self.last_configure.as_ref().map(|configure| configure.decoration_mode) {
            Some(DecorationMode::Server) if !self.decorate => {
                // To disable decorations we should request client and hide the frame.
                self.window.request_decoration_mode(Some(DecorationMode::Client))
            },
            _ if self.decorate => self.window.request_decoration_mode(Some(DecorationMode::Server)),
            _ => (),
        }

        if let Some(frame) = self.frame.as_mut() {
            frame.set_hidden(!decorate);
            // Force the resize.
            self.resize(self.size);
        }
    }

    /// Add seat focus for the window.
    #[inline]
    pub fn add_seat_focus(&mut self, seat: ObjectId) {
        self.seat_focus.insert(seat);
    }

    /// Remove seat focus from the window.
    #[inline]
    pub fn remove_seat_focus(&mut self, seat: &ObjectId) {
        self.seat_focus.remove(seat);
    }

    /// Returns `true` if the requested state was applied.
    pub fn set_ime_allowed(&mut self, allowed: bool) -> bool {
        self.ime_allowed = allowed;

        let mut applied = false;
        for text_input in &self.text_inputs {
            applied = true;
            if allowed {
                text_input.enable();
                text_input.set_content_type_by_purpose(self.ime_purpose);
            } else {
                text_input.disable();
            }
            text_input.commit();
        }

        applied
    }

    /// Set the IME position.
    pub fn set_ime_cursor_area(&self, position: LogicalPosition<u32>, size: LogicalSize<u32>) {
        // FIXME: This won't fly unless user will have a way to request IME window per seat, since
        // the ime windows will be overlapping, but winit doesn't expose API to specify for
        // which seat we're setting IME position.
        let (x, y) = (position.x as i32, position.y as i32);
        let (width, height) = (size.width as i32, size.height as i32);
        for text_input in self.text_inputs.iter() {
            text_input.set_cursor_rectangle(x, y, width, height);
            text_input.commit();
        }
    }

    /// Set the IME purpose.
    pub fn set_ime_purpose(&mut self, purpose: ImePurpose) {
        self.ime_purpose = purpose;

        for text_input in &self.text_inputs {
            text_input.set_content_type_by_purpose(purpose);
            text_input.commit();
        }
    }

    /// Get the IME purpose.
    pub fn ime_purpose(&self) -> ImePurpose {
        self.ime_purpose
    }

    /// Set the scale factor for the given window.
    #[inline]
    pub fn set_scale_factor(&mut self, scale_factor: f64) {
        self.scale_factor = scale_factor;

        // NOTE: When fractional scaling is not used update the buffer scale.
        if self.fractional_scale.is_none() {
            let _ = self.window.set_buffer_scale(self.scale_factor as _);
        }

        if let Some(frame) = self.frame.as_mut() {
            frame.set_scaling_factor(scale_factor);
        }
    }

    /// Make window background blurred
    #[inline]
    /// Ask the compositor to blur what shows through the window.
    ///
    /// Two protocols can do this and the window uses exactly one of them: the
    /// cross-desktop `ext_background_effect_v1` where the compositor offers it,
    /// KDE's older `org_kde_kwin_blur` otherwise. Preferring on the *manager*
    /// rather than on the live capability keeps the choice stable for the life of
    /// the window — a compositor that offers both (KWin does) must not end up
    /// with two blurs stacked on one surface because a capability blinked.
    /// [`Window::blur_supported`] resolves support the same way, so what it
    /// reports is what this method will actually use. [vendored addition]
    ///
    /// [`Window::blur_supported`]: super::Window::blur_supported
    pub fn set_blur(&mut self, blurred: bool) {
        if let Some(manager) = self.background_effect_manager.clone() {
            self.set_background_effect_blur(&manager, blurred);
        } else {
            self.set_kwin_blur(blurred);
        }
    }

    /// Blur through `ext_background_effect_v1`, where the region *is* the effect:
    /// it starts empty and a NULL region removes it, so turning blur on means
    /// handing over a region and turning it off means dropping the object.
    /// [vendored addition]
    ///
    /// Switching it on states the region for the size the window is now, and
    /// commits so it lands even if nothing draws for a while. That commit is safe
    /// only *here*: it carries no new buffer, so during a reconfigure it would
    /// apply the new region to the old frame — blur jumping to the window's new
    /// shape a beat before the window does — and tell the compositor the state
    /// change is already dealt with, ending the transition it was animating.
    fn set_background_effect_blur(&mut self, manager: &BackgroundEffectManager, blurred: bool) {
        if !blurred {
            if let Some(effect) = self.background_effect.take() {
                effect.destroy();
            }
            // The region went with the effect object; a later one starts blank.
            self.blur_shape.forget();
            return;
        }
        if self.background_effect.is_none() {
            self.background_effect =
                Some(manager.effect(self.window.wl_surface(), &self.queue_handle));
            self.blur_shape.forget();
        }
        if self.write_blur_region(self.size) && self.is_configured() {
            self.window.wl_surface().commit();
        }
    }

    /// State the blur region for a surface of `surface` logical pixels, unless the
    /// compositor already has exactly that one. Returns whether anything was sent.
    /// Never commits: the caller decides which buffer it rides with.
    /// [vendored addition]
    ///
    /// The shape belongs to the **window**, which our margins sit outside of:
    /// blurring the shadow's own ring would put a blurred halo where the window
    /// is see-through.
    fn write_blur_region(&mut self, surface: LogicalSize<u32>) -> bool {
        let Some(effect) = self.background_effect.clone() else {
            return false;
        };
        let m = self.margins_now();
        let Some(rects) = self
            .blur_shape
            .for_present(m.deflate(surface), (m.left as i32, m.top as i32))
        else {
            return false;
        };
        let region = match Region::new(&*self.compositor) {
            Ok(region) => region,
            Err(err) => {
                warn!("Failed to create the blur region: {err}");
                // Nothing reached the compositor, so it must not be remembered as
                // having: let the next frame try again.
                self.blur_shape.forget();
                return false;
            },
        };
        for (x, y, w, h) in rects {
            region.add(x, y, w, h);
        }
        // `set_blur_region` copies, hence dropping the region here.
        effect.set_blur_region(Some(region.wl_region()));
        true
    }

    /// State everything the compositor is told about size for the buffer the
    /// client is about to commit, whose surface is `surface` logical pixels. Call
    /// it immediately before presenting.
    ///
    /// All of it — the window geometry and the decoration frame ([`PendingSize`]),
    /// the blur region ([`BlurRegion`]) — is double-buffered state that takes
    /// effect at the next commit, so it belongs here, paired with the buffer it
    /// describes, rather than at the configure that asked for it. [vendored
    /// addition]
    pub fn present_size(&mut self, surface: PhysicalSize<u32>) {
        let scale = self.scale_factor();
        self.apply_pending_size(surface, scale);
        self.write_blur_region(surface.to_logical(scale));
    }

    /// Round the backdrop effect's corners by `top` and `bottom` logical pixels,
    /// matching a client that rounds those corners in its own drawing. 0 (the
    /// default) leaves that end square. Carried by the next frame, like every
    /// other change to the shape. [vendored addition]
    pub fn set_blur_corner_radii(&mut self, top: u32, bottom: u32) {
        self.blur_shape.set_radii(top, bottom);
    }

    /// Blur through KDE's `org_kde_kwin_blur`, for compositors that offer only
    /// that. [vendored addition: extracted from `set_blur`]
    fn set_kwin_blur(&mut self, blurred: bool) {
        if blurred && self.blur.is_none() {
            if let Some(blur_manager) = self.blur_manager.as_ref() {
                let blur = blur_manager.blur(self.window.wl_surface(), &self.queue_handle);
                blur.commit();
                self.blur = Some(blur);
            } else {
                info!("Blur manager unavailable, unable to change blur")
            }
        } else if !blurred && self.blur.is_some() {
            self.blur_manager.as_ref().unwrap().unset(self.window.wl_surface());
            self.blur.take().unwrap().release();
        }
    }

    /// Set the window title to a new value.
    ///
    /// This will automatically truncate the title to something meaningful.
    pub fn set_title(&mut self, mut title: String) {
        // Truncate the title to at most 1024 bytes, so that it does not blow up the protocol
        // messages
        if title.len() > 1024 {
            let mut new_len = 1024;
            while !title.is_char_boundary(new_len) {
                new_len -= 1;
            }
            title.truncate(new_len);
        }

        // Update the CSD title.
        if let Some(frame) = self.frame.as_mut() {
            frame.set_title(&title);
        }

        self.window.set_title(&title);
        self.title = title;
    }

    /// Mark the window as transparent.
    #[inline]
    pub fn set_transparent(&mut self, transparent: bool) {
        self.transparent = transparent;
        self.reload_transparency_hint();
    }

    /// Register text input on the top-level.
    #[inline]
    pub fn text_input_entered(&mut self, text_input: &ZwpTextInputV3) {
        if !self.text_inputs.iter().any(|t| t == text_input) {
            self.text_inputs.push(text_input.clone());
        }
    }

    /// The text input left the top-level.
    #[inline]
    pub fn text_input_left(&mut self, text_input: &ZwpTextInputV3) {
        if let Some(position) = self.text_inputs.iter().position(|t| t == text_input) {
            self.text_inputs.remove(position);
        }
    }

    /// Get the cached title.
    #[inline]
    pub fn title(&self) -> &str {
        &self.title
    }
}

impl Drop for WindowState {
    fn drop(&mut self) {
        if let Some(blur) = self.blur.take() {
            blur.release();
        }

        if let Some(fs) = self.fractional_scale.take() {
            fs.destroy();
        }

        if let Some(viewport) = self.viewport.take() {
            viewport.destroy();
        }

        // NOTE: the wl_surface used by the window is being cleaned up when
        // dropping SCTK `Window`.
    }
}

/// The state of the cursor grabs.
#[derive(Clone, Copy)]
struct GrabState {
    /// The grab mode requested by the user.
    user_grab_mode: CursorGrabMode,

    /// The current grab mode.
    current_grab_mode: CursorGrabMode,
}

impl GrabState {
    fn new() -> Self {
        Self { user_grab_mode: CursorGrabMode::None, current_grab_mode: CursorGrabMode::None }
    }
}

/// The state of the frame callback.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameCallbackState {
    /// No frame callback was requested.
    #[default]
    None,
    /// The frame callback was requested, but not yet arrived, the redraw events are throttled.
    Requested,
    /// The callback was marked as done, and user could receive redraw requested
    Received,
}

impl From<ResizeDirection> for XdgResizeEdge {
    fn from(value: ResizeDirection) -> Self {
        match value {
            ResizeDirection::North => XdgResizeEdge::Top,
            ResizeDirection::West => XdgResizeEdge::Left,
            ResizeDirection::NorthWest => XdgResizeEdge::TopLeft,
            ResizeDirection::NorthEast => XdgResizeEdge::TopRight,
            ResizeDirection::East => XdgResizeEdge::Right,
            ResizeDirection::SouthWest => XdgResizeEdge::BottomLeft,
            ResizeDirection::SouthEast => XdgResizeEdge::BottomRight,
            ResizeDirection::South => XdgResizeEdge::Bottom,
        }
    }
}

// NOTE: Rust doesn't allow `From<Option<Theme>>`.
#[cfg(feature = "sctk-adwaita")]
fn into_sctk_adwaita_config(theme: Option<Theme>) -> sctk_adwaita::FrameConfig {
    match theme {
        Some(Theme::Light) => sctk_adwaita::FrameConfig::light(),
        Some(Theme::Dark) => sctk_adwaita::FrameConfig::dark(),
        None => sctk_adwaita::FrameConfig::auto(),
    }
}

/// The rectangles making up the shape whose backdrop should be blurred, for a
/// surface of `size` whose bottom corners the client rounds by `bottom_radius`
/// logical pixels.
///
/// Square by default, and then one rect says it: the compositor clips the region
/// to the surface, so an oversized rect means "all of it" at every size and never
/// needs respecifying. A rounded shape has to be spelled out instead —
/// everything above the corners, then one rect per row across the arcs.
/// [vendored addition]
fn blur_shape_rects(
    size: LogicalSize<u32>,
    top_radius: u32,
    bottom_radius: u32,
) -> Vec<(i32, i32, i32, i32)> {
    let (w, h) = (size.width, size.height);
    let rt = top_radius.min(w / 2).min(h);
    let rb = bottom_radius.min(w / 2).min(h - rt);
    if rt == 0 && rb == 0 {
        return vec![(0, 0, WHOLE_SURFACE, WHOLE_SURFACE)];
    }
    // Everything between the corners, overwide so the sides are never clipped
    // short of the surface by a stale size.
    let mut rects = vec![(0, rt as i32, WHOLE_SURFACE, (h - rt - rb) as i32)];
    // Half-chord of the corner circle at a row `dy` from its centre. Rounded
    // *inward* (and then one more pixel), so the region always stops short of the
    // curve the client drew: a region edge that overshoots it by even half a pixel
    // leaves a thread of blur along the whole arc, where falling short hides under
    // the client's own antialiasing.
    let inset = |r: u32, dy: f32| {
        let rf = r as f32;
        let dx = (rf * rf - dy * dy).max(0.0).sqrt();
        (rf - dx).ceil() as i32 + 1
    };
    let row_rect = |x: i32, y: u32| {
        let width = w as i32 - 2 * x;
        if width > 0 {
            Some((x, y as i32, width, 1))
        } else {
            None
        }
    };
    // The top arc, counting *up* to its centre at `rt`, then the bottom one
    // counting down from its centre at `h - rb`.
    for row in 0..rt {
        if let Some(rect) = row_rect(inset(rt, rt as f32 - row as f32 - 0.5), row) {
            rects.push(rect);
        }
    }
    for row in 0..rb {
        if let Some(rect) = row_rect(inset(rb, row as f32 + 0.5), h - rb + row) {
            rects.push(rect);
        }
    }
    rects
}

/// What the compositor has been told to blur — and, more to the point, *when* it
/// is told. [vendored addition]
///
/// The region is double-buffered **surface** state: whatever was set last is what
/// the next `wl_surface.commit` applies, including a commit carrying a buffer of
/// some other size. A configure is therefore exactly the wrong moment to state
/// one. The client's buffer for that configure does not exist yet — a client
/// drawing through a GPU swapchain presents on its own schedule — so every frame
/// committed in between pairs the *new* region with an *old* buffer. On a shrink
/// the region falls short of the surface and the window wears an unblurred band
/// down the edge that is still catching up; on a grow it is merely clipped back
/// to the surface, which is why only one direction of the drag showed it.
///
/// Measured on a real drag before this existed: 873 of 1910 commits paired a
/// buffer with a region of a different height, out to 162 logical pixels.
///
/// So the region goes where the buffer goes: [`Self::for_present`] is handed the
/// size of the buffer about to be committed and answers with the region that
/// describes *that*. Nothing else states one.
/// The size a configure asked for, held until the client has a buffer that size.
///
/// Everything the compositor is told about how big the window is —
/// `xdg_surface.set_window_geometry`, the decoration frame's own size and the
/// position of its subsurfaces, the viewport destination — is double-buffered
/// state that takes effect at the surface's next commit. Sent when the configure
/// arrives, it takes effect at whatever the client commits next, and through a
/// GPU swapchain that is a frame the client queued *before* the configure: the
/// window's edges move to the new size around content that is still the old one.
/// On a left-edge drag the compositor anchors placement from the geometry, so the
/// whole window shifts left while the buffer stays short, and the shortfall opens
/// as a strip of bare desktop down the right.
///
/// Nor can the client simply refuse to commit early — the commit is not its to
/// time. Mesa's display queue puts the attach and commit on the wire on its own
/// FIFO schedule, and a configure handled meanwhile lands between the two.
///
/// So the size is held here and applied from the present hook, against the buffer
/// being committed, exactly as [`BlurRegion`] is. A drag that outruns the client
/// simply overwrites it: only the newest configure is worth answering.
#[derive(Debug, Default)]
struct PendingSize {
    wanted: Option<LogicalSize<u32>>,
}

impl PendingSize {
    /// Note the size a configure (or the client itself) just asked for.
    fn wants(&mut self, size: LogicalSize<u32>) {
        self.wanted = Some(size);
    }

    /// Whether the buffer now being committed is the one that answers the held
    /// configure — and if so, consume it, since it is about to be applied.
    ///
    /// Compared in physical pixels, the units the buffer is actually in: a
    /// fractional scale does not round-trip through logical exactly, and a
    /// half-pixel disagreement here would strand the configure forever — the
    /// compositor would never learn the window's size again.
    fn take_for(&mut self, surface: PhysicalSize<u32>, scale: f64) -> bool {
        if self.wanted.map(|w| logical_to_physical_rounded(w, scale)) == Some(surface) {
            self.wanted = None;
            true
        } else {
            false
        }
    }
}

#[derive(Debug, Default)]
struct BlurRegion {
    top_radius: u32,
    bottom_radius: u32,
    /// The rects the compositor already has, so a steady stream of frames at one
    /// shape respecifies nothing. Kept as the rects themselves rather than the
    /// size they came from: a square shape is one oversized rect at *every* size
    /// (see [`blur_shape_rects`]), and comparing what is actually sent is what
    /// makes a resize free for it without a special case saying so.
    written: Option<Vec<(i32, i32, i32, i32)>>,
}

impl BlurRegion {
    /// Round the corners by `top`/`bottom` logical pixels; 0 leaves that end
    /// square. Stated by the next frame, like every other shape change.
    fn set_radii(&mut self, top: u32, bottom: u32) {
        self.top_radius = top;
        self.bottom_radius = bottom;
    }

    /// The region for a buffer covering `window` at `origin` within the surface,
    /// or `None` when the compositor already has exactly that one.
    fn for_present(
        &mut self,
        window: LogicalSize<u32>,
        origin: (i32, i32),
    ) -> Option<Vec<(i32, i32, i32, i32)>> {
        let rects: Vec<_> = blur_shape_rects(window, self.top_radius, self.bottom_radius)
            .into_iter()
            .map(|rect| place_blur_rect(rect, window, origin))
            .collect();
        if self.written.as_deref() == Some(rects.as_slice()) {
            return None;
        }
        self.written = Some(rects.clone());
        Some(rects)
    }

    /// Forget what the compositor knows. The effect object is per-surface state,
    /// so destroying it takes the region with it and a fresh one must be told.
    fn forget(&mut self) {
        self.written = None;
    }
}

/// Put one shape rect where it belongs on the surface: offset into the window,
/// and clipped to it. [vendored addition]
///
/// The body rect is deliberately overwide — the compositor clips the region to
/// the surface, so one oversized rect means "all of it" at every size and never
/// needs respecifying. Offset into a margin it no longer does: it overshoots the
/// window on the far side and blurs the shadow's own ring, which shows as a
/// strip of blurred backdrop down that edge. With nothing outside the window
/// there is nothing to overshoot into, and the oversized rect is left alone.
fn place_blur_rect(
    rect: (i32, i32, i32, i32),
    window: LogicalSize<u32>,
    origin: (i32, i32),
) -> (i32, i32, i32, i32) {
    let (x, y, w, h) = rect;
    if origin == (0, 0) {
        return rect;
    }
    // Intersect with the window, held one pixel in on every side. Exactly on the
    // boundary is not good enough: the region is logical and the edge we draw is
    // device, so at a fractional scale they round apart and the region ends up a
    // device pixel past the border — a bright thread of blurred backdrop running
    // the whole edge, which is what the arc rows already step inward to avoid.
    let (l, t) = (origin.0 + 1, origin.1 + 1);
    let r = origin.0 + window.width as i32 - 1;
    let b = origin.1 + window.height as i32 - 1;
    let (x0, y0) = ((x + origin.0).max(l), (y + origin.1).max(t));
    let (x1, y1) = ((x + origin.0 + w).min(r), (y + origin.1 + h).min(b));
    (x0, y0, (x1 - x0).max(0), (y1 - y0).max(0))
}

#[cfg(test)]
mod decoration_margin_tests {
    use super::*;

    const M: DecorationMargins = DecorationMargins { top: 12, right: 20, bottom: 28, left: 20 };

    #[test]
    fn the_surface_is_the_window_plus_its_margins() {
        // What the compositor configures is the window; what we paint is the
        // surface around it.
        assert_eq!(M.inflate(LogicalSize::new(800, 600)), LogicalSize::new(840, 640));
        assert_eq!(M.deflate(LogicalSize::new(840, 640)), LogicalSize::new(800, 600));
    }

    #[test]
    fn no_margins_leave_the_surface_exactly_the_window() {
        let size = LogicalSize::new(800, 600);
        assert!(DecorationMargins::NONE.is_none());
        assert_eq!(DecorationMargins::NONE.inflate(size), size);
        assert_eq!(DecorationMargins::NONE.deflate(size), size);
    }

    /// The blur must stop at the window, not run on into the shadow: the margin
    /// is see-through, so blur out there lands at full strength on nothing and
    /// reads as a strip of blurred backdrop stuck to that edge.
    #[test]
    fn the_effect_shape_stops_at_the_window_not_the_surface() {
        const WINDOW: LogicalSize<u32> = LogicalSize::new(800, 600);
        // The overwide body rect, as `blur_shape_rects` states it.
        let body = (0, 0, WHOLE_SURFACE, WHOLE_SURFACE);

        // No margins: the oversized rect is the point, and it is kept.
        assert_eq!(place_blur_rect(body, WINDOW, (0, 0)), body);

        // Offset into a margin, it is clipped to the window it belongs to —
        // and held a pixel inside it, so a fractional scale cannot round the
        // region past the border and leave a thread of blur down the edge.
        assert_eq!(place_blur_rect(body, WINDOW, (20, 12)), (21, 13, 798, 598));
        // A rect already inside the window keeps its own size.
        assert_eq!(
            place_blur_rect((5, 7, 100, 1), WINDOW, (20, 12)),
            (25, 19, 100, 1)
        );
        // One that would reach the far edge is pulled back off it.
        let (x, _, w, _) = place_blur_rect((0, 7, 800, 1), WINDOW, (20, 12));
        assert_eq!((x, x + w), (21, 819), "must stop short of the window's edge");
    }

    #[test]
    fn a_surface_smaller_than_its_margins_still_names_a_window() {
        // Mid-resize the compositor can hand us less than the margins alone
        // take up. A zero-sized `set_window_geometry` is a protocol error, so
        // the window bottoms out at one pixel rather than vanishing.
        let tiny = M.deflate(LogicalSize::new(1, 1));
        assert!(tiny.width >= 1 && tiny.height >= 1, "got {tiny:?}");
    }
}

#[cfg(test)]
mod blur_shape_tests {
    use super::*;

    const SIZE: LogicalSize<u32> = LogicalSize::new(800, 600);

    #[test]
    fn a_square_window_asks_for_one_oversized_rect() {
        // Size-independent, so it survives every resize without being respecified.
        assert_eq!(blur_shape_rects(SIZE, 0, 0), vec![(0, 0, WHOLE_SURFACE, WHOLE_SURFACE)]);
    }

    #[test]
    fn a_rounded_window_stops_short_of_its_own_curve() {
        let r = 10u32;
        let rects = blur_shape_rects(SIZE, 0, r);
        let (_, _, _, top_h) = rects[0];
        assert_eq!(top_h, (SIZE.height - r) as i32, "the body must reach the corners");

        for &(x, y, w, h) in &rects[1..] {
            assert_eq!(h, 1);
            let row = y - (SIZE.height - r) as i32;
            // Inside the circle: the region's edge must not sit outside the arc,
            // or a thread of blur shows along the curve.
            let dy = row as f32 + 0.5;
            let dx = ((r * r) as f32 - dy * dy).max(0.0).sqrt();
            assert!(
                x as f32 >= r as f32 - dx,
                "row {row} starts at {x}, outside the arc at {}",
                r as f32 - dx
            );
            assert_eq!(w, SIZE.width as i32 - 2 * x, "the two corners must match");
            assert!(x >= 0 && x + w <= SIZE.width as i32, "row {row} leaves the surface");
        }
    }

    /// A client that rounds all four corners — one drawing its own decorations —
    /// needs the effect to stop at the top curve as well. Left square, the
    /// corner it cut to transparent is where the blur shows at full strength.
    #[test]
    fn a_window_that_rounds_its_top_too_gets_the_effect_cut_there() {
        let r = 10u32;
        let rects = blur_shape_rects(SIZE, r, r);
        let (_, body_y, _, body_h) = rects[0];
        assert_eq!(body_y, r as i32, "the body must start below the top corners");
        assert_eq!(body_h, (SIZE.height - 2 * r) as i32, "and end above the bottom ones");

        let rows: Vec<_> = rects[1..].iter().filter(|r| r.1 < body_y).collect();
        assert_eq!(rows.len() as u32, r, "one row per pixel of the top arc");
        for &&(x, y, w, h) in &rows {
            assert_eq!(h, 1);
            // Inside the circle whose centre is `r` down from the top.
            let dy = r as f32 - y as f32 - 0.5;
            let dx = ((r * r) as f32 - dy * dy).max(0.0).sqrt();
            assert!(
                x as f32 >= r as f32 - dx,
                "row {y} starts at {x}, outside the arc at {}",
                r as f32 - dx
            );
            assert_eq!(w, SIZE.width as i32 - 2 * x, "the two corners must match");
        }
        // And it opens out downward, the mirror of the bottom arc closing in.
        let widths: Vec<i32> = rows.iter().map(|r| r.2).collect();
        assert!(
            widths.windows(2).all(|p| p[1] >= p[0]),
            "the top arc should open out, got {widths:?}"
        );
    }

    #[test]
    fn the_rows_narrow_toward_the_bottom() {
        let rects = blur_shape_rects(SIZE, 0, 12);
        let widths: Vec<i32> = rects[1..].iter().map(|r| r.2).collect();
        assert!(widths.len() > 1, "a rounded corner needs more than one row");
        assert!(
            widths.windows(2).all(|p| p[1] <= p[0]),
            "the arc should close in, got {widths:?}"
        );
    }

    #[test]
    fn a_radius_bigger_than_the_window_cannot_escape_it() {
        // A tiny window mid-resize must not produce rects outside the surface.
        for (w, h) in [(20u32, 8u32), (1, 1), (40, 40)] {
            let size = LogicalSize::new(w, h);
            for &(x, y, rw, rh) in &blur_shape_rects(size, 100, 100)[1..] {
                assert!(x >= 0 && rw > 0 && x + rw <= w as i32, "{x}+{rw} outside {w}");
                assert!(y >= 0 && y + rh <= h as i32, "{y}+{rh} outside {h}");
            }
        }
    }
}

/// A configure is applied with the buffer that answers it — see [`PendingSize`].
#[cfg(test)]
mod pending_size_tests {
    use super::*;

    fn size(w: u32, h: u32) -> LogicalSize<u32> {
        LogicalSize::new(w, h)
    }

    /// The buffer a client presents for a window of `size` logical pixels.
    fn drawn(size: LogicalSize<u32>, scale: f64) -> PhysicalSize<u32> {
        logical_to_physical_rounded(size, scale)
    }

    #[test]
    fn a_configure_the_client_has_not_drawn_yet_is_held() {
        let mut pending = PendingSize::default();
        pending.wants(size(1223, 1123));
        // The frame in flight is still the size before. Applying the configure
        // around it would move the window's edges past its own content.
        assert!(!pending.take_for(drawn(size(1255, 1123), 1.0), 1.0));
    }

    #[test]
    fn the_frame_that_answers_a_configure_applies_it() {
        let mut pending = PendingSize::default();
        pending.wants(size(1223, 1123));
        assert!(pending.take_for(drawn(size(1223, 1123), 1.0), 1.0));
    }

    #[test]
    fn a_configure_is_applied_once_and_not_on_every_later_frame() {
        let mut pending = PendingSize::default();
        pending.wants(size(1223, 1123));
        assert!(pending.take_for(drawn(size(1223, 1123), 1.0), 1.0));
        // The window is still this size a hundred frames later; restating it
        // would be pure traffic.
        assert!(!pending.take_for(drawn(size(1223, 1123), 1.0), 1.0));
    }

    #[test]
    fn only_the_newest_configure_is_waited_for() {
        let mut pending = PendingSize::default();
        pending.wants(size(1223, 1123));
        pending.wants(size(1186, 1123));
        // A drag outruns the client: the size it drew for two configures ago is
        // not the one the compositor is waiting to hear about.
        assert!(!pending.take_for(drawn(size(1223, 1123), 1.0), 1.0));
        assert!(pending.take_for(drawn(size(1186, 1123), 1.0), 1.0));
    }

    #[test]
    fn a_fractional_scale_does_not_strand_a_configure() {
        // The window is sized in logical pixels and drawn in physical ones, and
        // at 1.5x the two do not round-trip: comparing them anywhere but in the
        // buffer's own units can leave a configure that no frame ever answers,
        // and a window whose size the compositor never hears again.
        for scale in [1.25, 1.5, 1.75, 2.0] {
            for w in 1000..1100 {
                let mut pending = PendingSize::default();
                pending.wants(size(w, 723));
                assert!(
                    pending.take_for(drawn(size(w, 723), scale), scale),
                    "{w} logical at {scale}x"
                );
            }
        }
    }
}

/// The region has to reach the compositor paired with the buffer it describes —
/// see [`BlurRegion`] for what goes wrong when it doesn't.
#[cfg(test)]
mod blur_region_tests {
    use super::*;

    /// The height of the shape, which is what a vertical resize moves.
    fn height(rects: &[(i32, i32, i32, i32)]) -> i32 {
        rects.iter().map(|&(_, y, _, h)| y + h).max().unwrap_or(0)
    }

    fn rounded() -> BlurRegion {
        let mut blur = BlurRegion::default();
        blur.set_radii(0, 14);
        blur
    }

    /// The bug, stated: a drag hands the client a stream of configures, and the
    /// client's buffer for each one lands a frame or more later (ghost draws
    /// through wgpu, which presents on its own schedule). Every frame committed
    /// in between used to carry the region for the newest configure over a buffer
    /// that was still the old size — on a shrink, an unblurred band down the edge.
    #[test]
    fn a_configure_the_client_has_not_drawn_yet_reaches_the_compositor_with_nothing() {
        let mut blur = rounded();
        let first = blur
            .for_present(LogicalSize::new(800, 600), (0, 0))
            .expect("the first frame states the region");
        assert_eq!(height(&first), 600);

        // A drag shrinks the window three times. Nothing here writes a region:
        // there is no method to call, because a configure has no buffer to pair
        // one with. The client keeps presenting the size it last drew...
        for _ in 0..3 {
            assert_eq!(
                blur.for_present(LogicalSize::new(800, 600), (0, 0)),
                None,
                "a frame at an unchanged size must respecify nothing"
            );
        }

        // ...and only the buffer that really is 800x480 carries the region for it.
        let landed = blur
            .for_present(LogicalSize::new(800, 480), (0, 0))
            .expect("a new size states a new region");
        assert_eq!(height(&landed), 480, "the region must describe the buffer it lands with");
    }

    /// The measurement that found this: on the traced drag, 873 of 1910 commits
    /// paired a buffer with a region of another height, out to 162 logical pixels.
    /// Whatever size a frame is, its region is that size.
    #[test]
    fn every_frame_of_a_drag_states_its_own_size() {
        let mut blur = rounded();
        for h in (400..=900).step_by(7) {
            let size = LogicalSize::new(1000, h);
            let rects = blur.for_present(size, (0, 0)).expect("a new size states a region");
            assert_eq!(height(&rects), h as i32, "region must match the buffer at {h}");
        }
    }

    /// A square shape is one oversized rect at every size (the compositor clips it
    /// to the surface), so a resize gives the compositor nothing new to hear —
    /// which is exactly why this class of bug never showed on a square window.
    #[test]
    fn a_square_window_states_its_region_once_and_never_again() {
        let mut blur = BlurRegion::default();
        assert!(blur.for_present(LogicalSize::new(800, 600), (0, 0)).is_some());
        for h in [560, 520, 480] {
            assert_eq!(blur.for_present(LogicalSize::new(800, h), (0, 0)), None);
        }
    }

    #[test]
    fn rounding_the_corners_is_carried_by_the_next_frame() {
        let mut blur = BlurRegion::default();
        let square = blur.for_present(LogicalSize::new(800, 600), (0, 0)).unwrap();
        blur.set_radii(0, 14);
        let curved = blur
            .for_present(LogicalSize::new(800, 600), (0, 0))
            .expect("a new shape at the same size still has to be stated");
        assert_ne!(square, curved);
    }

    /// The effect object is per-surface state: destroying it (blur switched off)
    /// throws the region away with it, so switching back on must state it afresh
    /// rather than trust what the old effect was told.
    #[test]
    fn a_destroyed_effect_forgets_what_the_compositor_knew() {
        let mut blur = rounded();
        let size = LogicalSize::new(800, 600);
        assert!(blur.for_present(size, (0, 0)).is_some());
        assert_eq!(blur.for_present(size, (0, 0)), None);
        blur.forget();
        assert!(
            blur.for_present(size, (0, 0)).is_some(),
            "a fresh effect knows nothing and must be told"
        );
    }
}
