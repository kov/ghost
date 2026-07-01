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

pub mod cmd;
pub mod encode;
pub mod event;
pub mod fleet;
pub mod input;
pub mod mouse;
pub mod root;
pub mod terminal;

pub use cmd::Cmd;
pub use event::{PointPx, PointerButton, PointerPhase, UiEvent};
pub use fleet::{FleetModel, Locality};
/// The scheme's default fg/bg the models report to OSC 10/11 color queries.
pub use ghost_vt::query::ThemeColors;
pub use input::{Key, KeyAlternates, KeyEventKind, Mods, NamedKey};
pub use root::RootModel;
pub use terminal::{
    Shortcut, TerminalModel, bracket_paste, classify_shortcut, query_replies, selection_text,
};

/// A session's stable identity (its name). Focus and input routing key on this,
/// never a list index — so reordering tiles can't silently retarget input.
pub type SessionId = String;

// The shared layout/scene vocabulary, re-exported so the app and shell import
// it from one place (the core) rather than reaching into ghost-render directly.
pub use ghost_render::{
    BadgeKind, CellMetrics, Frame, Layer, RectPx, Rgba, Run, Scene, SceneId, SceneItem, Selection,
    Style, TermDamage, Transform,
};
