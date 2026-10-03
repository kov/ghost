//! ghost's own titlebar on macOS, under AppKit's own traffic lights.
//!
//! The bar is ours on every platform where we can have one, because it is where
//! a window says what the desktop's frame has nowhere to put (the freeze
//! notice). On macOS the window keeps its native frame and lets our surface run
//! up under a transparent titlebar, so we draw the bar and AppKit goes on
//! drawing — and owning — the lights: they are what VoiceOver, AX automation and
//! the tiling tools find a window by, and they bring zoom, the tiling popover,
//! fullscreen and edge resizing with them (winit cannot resize a borderless
//! macOS window at all). See `docs/window-decorations.md`, option A.
//!
//! All the shell needs from here is the window attributes and two numbers: how
//! tall the titlebar AppKit lays the lights out in is, so our bar is that tall
//! and the lights sit centred on it, and how far along it they reach, so
//! nothing of ours is drawn under them.

use objc2::rc::Retained;
use objc2_app_kit::{NSView, NSWindow, NSWindowButton, NSWindowStyleMask};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize};
use winit::window::{Window, WindowAttributes};

/// The gap the bar keeps past the last traffic light, in points — about the
/// space AppKit leaves between the lights themselves.
const CONTROLS_GAP: f64 = 8.0;

/// A window that is ours to the top edge, with AppKit's titlebar left there
/// only for its lights: it draws no background and no title, both being the
/// bar's to draw.
pub fn attributes(attrs: WindowAttributes) -> WindowAttributes {
    use winit::platform::macos::WindowAttributesExtMacOS;
    attrs
        .with_fullsize_content_view(true)
        .with_titlebar_transparent(true)
        .with_title_hidden(true)
}

/// The native titlebar our bar stands in for, in points.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NativeTitlebar {
    /// Its height: the band AppKit centres the lights in.
    pub height: f64,
    /// How far from the leading edge the lights (and a gap past them) reach.
    pub controls: f64,
}

/// The `NSWindow` behind a winit window.
pub fn ns_window(window: &Window) -> Option<Retained<NSWindow>> {
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = window.window_handle().ok()?;
    let RawWindowHandle::AppKit(h) = handle.as_raw() else {
        return None;
    };
    // SAFETY: on macOS the AppKit handle's `ns_view` is a live NSView owned by
    // `window`, which outlives this borrow.
    let view: &NSView = unsafe { &*h.ns_view.as_ptr().cast::<NSView>() };
    view.window()
}

/// The titlebar height a titled window gets, asked before there is one — for
/// opening a window tall enough to fit both the bar and the grid.
pub fn titlebar_height(mtm: MainThreadMarker) -> f64 {
    let content = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(400.0, 300.0));
    // SAFETY: a pure geometry query on the class.
    let frame = unsafe {
        NSWindow::frameRectForContentRect_styleMask(content, NSWindowStyleMask::Titled, mtm)
    };
    frame.size.height - content.size.height
}

/// The titlebar `window` actually has, measured off the window: the part of
/// its frame its content layout rect leaves out, and the zoom button's far
/// edge. `None` without a window or lights to measure.
pub fn measure(window: &Window) -> Option<NativeTitlebar> {
    let ns = ns_window(window)?;
    // SAFETY: plain geometry reads on the main thread the window lives on.
    let layout = unsafe { ns.contentLayoutRect() };
    let height = ns.frame().size.height - layout.size.height;
    let zoom = button_frame(&ns, NSWindowButton::NSWindowZoomButton)?;
    Some(NativeTitlebar {
        height,
        controls: zoom.origin.x + zoom.size.width + CONTROLS_GAP,
    })
}

/// A standard window button's frame in the window's own coordinates (origin
/// bottom-left, in points).
pub fn button_frame(ns: &NSWindow, which: NSWindowButton) -> Option<NSRect> {
    let button = ns.standardWindowButton(which)?;
    // Converting to no view at all is converting to the window's own base
    // coordinate system.
    Some(button.convertRect_toView(button.bounds(), None))
}
