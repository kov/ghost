//! Handling of the cross-desktop background-effect (blur) protocol.
//!
//! `ext_background_effect_v1` is the vendor-neutral successor to
//! `org_kde_kwin_blur` (see [`kwin_blur`]): the compositor advertises which
//! effects it can apply, and the client marks the part of its surface whose
//! backdrop should get them.
//!
//! Two things differ from the KDE protocol and shape this module:
//!
//! * The effect is driven by a *region*. It starts empty and a NULL region
//!   removes it, so "blur the whole window" has to be spelled out as a region
//!   rather than implied by the object's existence.
//! * The capability is *live*. It arrives on bind and again whenever it changes,
//!   so a compositor whose blur effect is switched off mid-session takes the
//!   capability away — which is why it is kept in shared, lock-free state that
//!   [`Window::blur_supported`] can poll.
//!
//! [`kwin_blur`]: super::kwin_blur
//! [`Window::blur_supported`]: crate::platform_impl::wayland::window::Window::blur_supported

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use sctk::reexports::client::globals::{BindError, GlobalList};
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::{delegate_dispatch, Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::ext::background_effect::v1::client::ext_background_effect_manager_v1::{
    Capability, Event as ManagerEvent, ExtBackgroundEffectManagerV1,
};
pub use wayland_protocols::ext::background_effect::v1::client::ext_background_effect_surface_v1::ExtBackgroundEffectSurfaceV1;

use crate::platform_impl::wayland::state::WinitState;

/// A blur region big enough to cover any surface.
///
/// The protocol clips the region to the surface, so one fixed rect means "all of
/// it" and stays correct across every resize without being respecified. Kept far
/// below `i32::MAX` so a compositor that computes `x + width` in `i32` cannot
/// overflow on it.
pub const WHOLE_SURFACE: i32 = 1 << 24;

/// The manager's object data: the sink the `capabilities` event writes into.
///
/// The capability lives here rather than in [`WinitState`] because every window
/// reads it independently, on its own thread-free path, and long after the event
/// that set it.
#[derive(Debug, Clone)]
pub struct CapabilityData(Arc<AtomicU32>);

/// Background-effect manager, and what the compositor last said it can do.
#[derive(Debug, Clone)]
pub struct BackgroundEffectManager {
    manager: ExtBackgroundEffectManagerV1,
    capabilities: Arc<AtomicU32>,
}

impl BackgroundEffectManager {
    pub fn new(
        globals: &GlobalList,
        queue_handle: &QueueHandle<WinitState>,
    ) -> Result<Self, BindError> {
        let capabilities = Arc::new(AtomicU32::new(0));
        let manager = globals.bind(queue_handle, 1..=1, CapabilityData(capabilities.clone()))?;
        Ok(Self { manager, capabilities })
    }

    /// A handle on the live capability bits, for the window to poll.
    pub fn capabilities(&self) -> Arc<AtomicU32> {
        self.capabilities.clone()
    }

    /// Attach a background-effect object to `surface`. The caller owns it, and
    /// must not create a second one for the same surface — that is a protocol
    /// error (`background_effect_exists`).
    pub fn effect(
        &self,
        surface: &WlSurface,
        queue_handle: &QueueHandle<WinitState>,
    ) -> ExtBackgroundEffectSurfaceV1 {
        self.manager.get_background_effect(surface, queue_handle, ())
    }
}

/// Whether `capabilities` currently includes blur. Unknown bits (a newer
/// compositor advertising an effect we don't implement) are truncated away rather
/// than being allowed to poison the ones we do understand.
pub fn blur_supported(capabilities: &AtomicU32) -> bool {
    Capability::from_bits_truncate(capabilities.load(Ordering::Relaxed))
        .contains(Capability::Blur)
}

impl Dispatch<ExtBackgroundEffectManagerV1, CapabilityData, WinitState>
    for BackgroundEffectManager
{
    fn event(
        _: &mut WinitState,
        _: &ExtBackgroundEffectManagerV1,
        event: <ExtBackgroundEffectManagerV1 as Proxy>::Event,
        data: &CapabilityData,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
        match event {
            // Store the raw bits and mask them on read, so a bit we don't know
            // about today survives a round-trip instead of being dropped here.
            ManagerEvent::Capabilities { flags } => {
                data.0.store(flags.into(), Ordering::Relaxed)
            },
            _ => unreachable!("unknown ext_background_effect_manager_v1 event"),
        }
    }
}

impl Dispatch<ExtBackgroundEffectSurfaceV1, (), WinitState> for BackgroundEffectManager {
    fn event(
        _: &mut WinitState,
        _: &ExtBackgroundEffectSurfaceV1,
        _: <ExtBackgroundEffectSurfaceV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
        unreachable!("no events defined for ext_background_effect_surface_v1");
    }
}

delegate_dispatch!(WinitState: [ExtBackgroundEffectManagerV1: CapabilityData] => BackgroundEffectManager);
delegate_dispatch!(WinitState: [ExtBackgroundEffectSurfaceV1: ()] => BackgroundEffectManager);
