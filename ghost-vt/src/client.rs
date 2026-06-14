//! The attach client: a transparent pipe.
//!
//! Puts the terminal in raw mode and forwards stdin<->host byte-for-byte,
//! intercepting only the configurable detach/kill trigger (CLI default: `C-\`
//! prefix, then `d` to detach or `k` to kill; the prefix doubled sends a
//! literal). Everything else — including mouse reports and bracketed paste —
//! passes straight through, so the host terminal's native scrollback and mouse
//! keep working.
