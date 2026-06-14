//! The framed control protocol exchanged over a [`transport`](crate::transport):
//! attach, input, output, resize, detach, and kill messages.
//!
//! Detach and kill are first-class protocol actions; *how* a user triggers them
//! is left entirely to the client (a key sequence for the CLI, a button for a
//! GUI) — the protocol never assumes a particular keybinding.
