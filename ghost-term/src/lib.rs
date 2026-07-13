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
pub mod policy;
mod tabs;
pub mod terminal;
pub mod util;
mod vt;
pub use cell::Cell;
pub use charset::Charset;
pub use color::{index_rgb, Color, ANSI_16};
pub use graphics::{encode_transmit, Image, Placement};
pub use line::Line;
pub use parser::{
    FullscreenOp, MaximizeOp, Progress, SpecialColor, TitleTarget, XtwinopsOp, SPECIAL_COLOR_BASE,
};
pub use pen::Pen;
pub use policy::{ActionPolicy, SessionPolicy, TerminalPolicy};
pub use terminal::{
    ClipboardSelection, CursorShape, ModeReport, MAX_PROGRAM_COLS, MAX_PROGRAM_ROWS,
};
pub use vt::{MouseProtocol, Vt};
