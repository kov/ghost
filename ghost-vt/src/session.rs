//! The session host's core state: the authoritative terminal emulator (the
//! vendored `vt` engine, with bounded scrollback) and the recording writer.
//!
//! Each chunk of PTY output fans out to (1) the emulator state and (2) the
//! recorder. On attach, the session produces a single resync *checkpoint* — an
//! extended `dump()` covering scrollback + viewport + display modes + the
//! non-display modes (mouse, bracketed paste, focus, title) — that reconstructs
//! the terminal in a freshly attached client.
