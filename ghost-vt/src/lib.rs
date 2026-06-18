//! `ghost-vt` — run a terminal in the background and reattach to it without
//! losing scrollback, native mouse handling, or terminal keybindings.
//!
//! A "ghost session" is a child process (a shell, or any command) running under
//! a PTY owned by a long-lived background host process. An interactive terminal
//! attaches to that host, which replays enough state to reconstruct the screen
//! plus bounded scrollback and then streams live — the attaching terminal keeps
//! its own native scrollback and mouse behaviour because the client is a
//! transparent pipe and only the terminal *modes* are restored.
//!
//! This crate is the reusable engine behind the `ghost` CLI and any future GUI
//! front-end.

pub mod client;
pub mod keys;
pub mod meta;
pub mod paths;
pub mod protocol;
pub mod pty;
pub mod record;
pub mod screen;
pub mod server;
pub mod session;
mod signals;
pub mod transport;

/// The vendored virtual-terminal engine — a fork of asciinema's `avt` — used as
/// the authoritative server-side screen and scrollback state.
pub use vt::Vt;
