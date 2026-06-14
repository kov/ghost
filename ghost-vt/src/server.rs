//! The session host: a synchronous `poll()` loop over the PTY master, the
//! listening socket, attached client connections, and signals (via `signalfd`).
//!
//! Single-threaded and lock-free by construction — one owner of the terminal
//! state and the recorder. No async runtime: the fd count is small and fixed,
//! and a plain poll loop keeps backtraces and profiling honest.
