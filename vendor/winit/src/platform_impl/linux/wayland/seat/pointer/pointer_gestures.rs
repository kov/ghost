//! Pointer gestures: the touchpad hold gesture (ghost patch).
//!
//! Only `zwp_pointer_gesture_hold_v1` is bound — fingers resting on the pad
//! without moving, which is what stops kinetic scrolling. It needs version 3 of
//! `zwp_pointer_gestures_v1`; an older or missing global binds nothing.

use std::ops::Deref;
use std::sync::Mutex;

use sctk::reexports::client::globals::{BindError, GlobalList};
use sctk::reexports::client::{delegate_dispatch, Dispatch};
use sctk::reexports::client::{Connection, QueueHandle};
use sctk::reexports::protocols::wp::pointer_gestures::zv1::client::{
    zwp_pointer_gesture_hold_v1::{self, ZwpPointerGestureHoldV1},
    zwp_pointer_gestures_v1::ZwpPointerGesturesV1,
};

use sctk::globals::GlobalData;

use crate::event::{TouchPhase, WindowEvent};
use crate::platform_impl::wayland::state::WinitState;
use crate::platform_impl::wayland::{self, DeviceId, WindowId};

/// Wrapper around the pointer gestures manager.
pub struct PointerGesturesState {
    manager: ZwpPointerGesturesV1,
}

impl PointerGesturesState {
    /// Bind the pointer gestures manager, at the version with the hold gesture.
    pub fn new(
        globals: &GlobalList,
        queue_handle: &QueueHandle<WinitState>,
    ) -> Result<Self, BindError> {
        let manager = globals.bind(queue_handle, 3..=3, GlobalData)?;
        Ok(Self { manager })
    }
}

impl Deref for PointerGesturesState {
    type Target = ZwpPointerGesturesV1;

    fn deref(&self) -> &Self::Target {
        &self.manager
    }
}

/// The window a hold began over: `end` carries no surface, so it is kept from
/// `begin`.
#[derive(Debug, Default)]
pub struct HoldGestureData {
    window: Mutex<Option<WindowId>>,
}

impl Dispatch<ZwpPointerGesturesV1, GlobalData, WinitState> for PointerGesturesState {
    fn event(
        _state: &mut WinitState,
        _proxy: &ZwpPointerGesturesV1,
        _event: <ZwpPointerGesturesV1 as wayland_client::Proxy>::Event,
        _data: &GlobalData,
        _conn: &Connection,
        _qhandle: &QueueHandle<WinitState>,
    ) {
    }
}

impl Dispatch<ZwpPointerGestureHoldV1, HoldGestureData, WinitState> for PointerGesturesState {
    fn event(
        state: &mut WinitState,
        _proxy: &ZwpPointerGestureHoldV1,
        event: <ZwpPointerGestureHoldV1 as wayland_client::Proxy>::Event,
        data: &HoldGestureData,
        _conn: &Connection,
        _qhandle: &QueueHandle<WinitState>,
    ) {
        let mut window = data.window.lock().unwrap();
        let (window_id, fingers, phase) = match event {
            zwp_pointer_gesture_hold_v1::Event::Begin { surface, fingers, .. } => {
                let id = wayland::make_wid(&surface);
                *window = Some(id);
                (id, fingers, TouchPhase::Started)
            },
            zwp_pointer_gesture_hold_v1::Event::End { cancelled, .. } => {
                let Some(id) = window.take() else { return };
                let phase = if cancelled != 0 { TouchPhase::Cancelled } else { TouchPhase::Ended };
                (id, 0, phase)
            },
            _ => return,
        };
        let device_id = crate::event::DeviceId(crate::platform_impl::DeviceId::Wayland(DeviceId));
        state
            .events_sink
            .push_window_event(WindowEvent::HoldGesture { device_id, fingers, phase }, window_id);
    }
}

delegate_dispatch!(WinitState: [ZwpPointerGesturesV1: GlobalData] => PointerGesturesState);
delegate_dispatch!(WinitState: [ZwpPointerGestureHoldV1: HoldGestureData] => PointerGesturesState);
