//! The client<->server transport seam.
//!
//! A local Unix-socket transport comes first; SSH-tunneled stdio and (later) a
//! mosh-style UDP transport plug in behind the same trait. The session host and
//! attach client are written against this abstraction, not a concrete socket.
