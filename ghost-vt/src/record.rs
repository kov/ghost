//! The on-disk recording: a framed, per-frame zstd-compressed asciicast with
//! periodic state checkpoints, supporting append, seek, and tail-on-attach.
//!
//! The recording (archival, raw bytes) and the resync (emulator state) are
//! distinct roles that share this format: a checkpoint is the emulator's
//! serialized state, and the frames between checkpoints are the raw output.
