//! ghost-local regression test for the patch in `src/present.rs`:
//! a failed surface acquire must not burn a texture id.
//!
//! `surface_get_current_texture` used to allocate the texture id *before*
//! attempting the acquire, and every early exit dropped it without releasing
//! the index back to the identity manager — both the `Err` returns (a lost
//! device, an unconfigured surface) and the `Ok` outputs that carry no texture
//! (Timeout/Occluded/Outdated). Every leaked index later becomes a permanent
//! 24-byte `Element::Vacant` slot in the textures registry map — so a render
//! loop retrying an unacquirable surface (an occluded window) leaks without
//! bound. Observed in ghost before the patch: ~300M burned ids and a ~7.4GB
//! registry map in one overnight session.
//!
//! The noop backend advertises no surface capabilities, so its surface can
//! never be configured and every acquire takes the `Err(NotConfigured)` exit —
//! one of the leaking paths. The patch removes the eager allocation outright
//! (the id is allocated only once there is a texture to assign), which covers
//! the textureless-`Ok` exits by the same construction.
//!
//! Run with: `cargo test --features noop` (in this crate).

#![cfg(feature = "noop")]

use wgpu_core::global::Global;
use wgpu_types as wgt;

#[test]
fn a_failed_surface_acquire_burns_no_texture_id() {
    let mut instance_desc = wgt::InstanceDescriptor::new_without_display_handle();
    instance_desc.backends = wgt::Backends::NOOP;
    instance_desc.backend_options.noop = wgt::NoopBackendOptions { enable: true };
    let global = Global::new("surface-id-leak-test", instance_desc, None);
    // The noop backend ignores the handles entirely; the Web variants are the
    // ones constructible without unsafe on every platform.
    let display = raw_window_handle::RawDisplayHandle::Web(raw_window_handle::WebDisplayHandle::new());
    let window = raw_window_handle::RawWindowHandle::Web(raw_window_handle::WebWindowHandle::new(1));
    let surface_id = unsafe { global.instance_create_surface(Some(display), window, None) }
        .expect("noop surface creation cannot fail");

    // The shape of a render loop retrying an unacquirable surface: every
    // attempt fails before a texture exists.
    for attempt in 0..100 {
        let result = global.surface_get_current_texture(surface_id, None);
        assert!(result.is_err(), "attempt {attempt}: no texture is acquirable");
    }

    // None of those attempts may keep a texture id allocated. Before the patch
    // each one leaked its eagerly-allocated id: this read 100.
    let report = global.generate_report();
    assert_eq!(
        report.hub.textures.num_allocated, 0,
        "failed surface acquires leaked texture ids"
    );
}
