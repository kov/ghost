//! ghost-ui-core: the pure heart of ghost's custom frontend.
//!
//! No winit, no wgpu, no sockets, no clock — just plain data and pure functions,
//! so the UI's behavior is exercised headlessly by feeding events and asserting
//! on the results. The winit/wgpu shell (the `ghost-ui` app crate) translates
//! real OS input into these types at a single boundary and executes the effects
//! this core returns as data.
//!
//! This is step 1 of the [testability-spine retrofit]: the pure key/mouse
//! encoders and their input alphabet live here. Later steps add the
//! `update`/`view` reducer, the `Cmd` effect type, and the `Scene` model.

pub mod encode;
pub mod input;
pub mod mouse;

pub use input::{Key, Mods, NamedKey};
