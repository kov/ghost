//! ghost-term — ghost's terminal-emulator core.
//!
//! Forked from asciinema's `avt` (<https://github.com/asciinema/avt>),
//! © Marcin Kulik, licensed Apache-2.0 (see `LICENSE`). Modified for ghost and
//! diverging from upstream; attribution and license are preserved (Apache-2.0
//! §4). The modules below are largely upstream avt, evolving as ghost needs.

mod buffer;
mod cell;
mod charset;
mod color;
mod graphics;
mod line;
mod links;
pub mod parser;
mod pen;
mod tabs;
pub mod terminal;
pub mod util;
mod vt;
pub use cell::Cell;
pub use charset::Charset;
pub use color::{index_rgb, Color, ANSI_16};
pub use graphics::{encode_transmit, Image, Placement};
pub use line::Line;
pub use parser::{Progress, SpecialColor, SPECIAL_COLOR_BASE};
pub use pen::Pen;
pub use terminal::{ClipboardSelection, CursorShape, ModeReport};
pub use vt::{MouseProtocol, Vt};
