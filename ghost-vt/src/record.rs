//! The on-disk recording: a framed, per-frame brotli-compressed asciicast with
//! periodic state checkpoints, supporting append, seek, and tail-on-attach.
//!
//! Compression is brotli (pure Rust, no C toolchain): on terminal output it
//! matches zstd-3's ratio while staying far above any real output rate, so the
//! whole crate — and the headless binary staged to remotes — builds with just a
//! Rust target. The codec is confined to [`compress`]/[`decompress`].
//!
//! The recording (archival, raw bytes) and the resync (emulator state) are
//! distinct roles that share this format: a checkpoint is the emulator's
//! serialized state, and the frames between checkpoints are the raw output.
//!
//! ## Layout
//!
//! ```text
//! magic  "GHOSTREC"            8 bytes
//! ver    u8                    format version (3)
//! header u32 len + postcard(Header)
//! frame* repeated until EOF:
//!          kind  u8            (0 = events, 1 = checkpoint, 2 = images)
//!          clen  u32 LE        compressed payload length
//!          data  [clen]        brotli( postcard(Vec<Event>) )    for kind 0
//!                              brotli( postcard(Checkpoint) )     for kind 1
//!                              brotli( postcard(Vec<ImageBlob>) ) for kind 2
//! ```
//!
//! A checkpoint frame carries an emulator dump (minus kitty-graphics image
//! transmits): a safe point to start replay from, and a safe point to cut the
//! file at when bounding its size (everything before a checkpoint can be dropped
//! losslessly). Image bytes are content-addressed and stored once in an images
//! frame, then referenced by hash from the checkpoints that need them — so a
//! recurring image is not re-inlined in every checkpoint. The reader resolves
//! those references and reconstructs the full dump; bounding the file re-emits
//! the images its retained checkpoints reference (the cut would otherwise drop
//! the frame that stored them).
//!
//! Frames are independently compressed and length-prefixed, so the writer can
//! append incrementally with bounded memory and a reader can stop cleanly at a
//! torn final frame (e.g. after a crash) without failing — it returns every
//! complete frame it found.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::{self, BufWriter, Read, Write};
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const MAGIC: &[u8; 8] = b"GHOSTREC";
const FORMAT_VERSION: u8 = 3;
const FRAME_EVENTS: u8 = 0;
const FRAME_CHECKPOINT: u8 = 1;
/// A content-addressed image store: kitty-graphics image bytes written once and
/// referenced by hash from later checkpoints, so a recording bakes images in
/// without re-inlining them in every checkpoint.
const FRAME_IMAGES: u8 = 2;
/// Brotli quality. q2 matches the zstd-3 ratio this replaced (~4.4× on terminal
/// output) at ~180 MB/s encode — far above any real output rate, so recording
/// stays real-time — while costing ~⅓ less host CPU than q4 for the same ratio
/// (a flood-CPU benchmark showed frame compression, not checkpoints, dominates,
/// and higher quality bought no ratio worth its CPU). Decode speed is
/// quality-independent. The full-state checkpoint uses the same quality; at the
/// adaptive cadence its cost is negligible.
const BROTLI_QUALITY: u32 = 2;
const BROTLI_LGWIN: u32 = 22;
const BROTLI_BUF: usize = 8 * 1024;
/// Flush a frame once this many bytes of output have accumulated.
const FLUSH_THRESHOLD: usize = 64 * 1024;

/// Compress a frame payload. The recording's one codec seam: brotli (pure Rust)
/// in, [`decompress`] out. In-memory, so it only errors on OOM.
fn compress(raw: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    brotli::CompressorReader::new(raw, BROTLI_BUF, BROTLI_QUALITY, BROTLI_LGWIN)
        .read_to_end(&mut out)?;
    Ok(out)
}

/// Decompress a frame payload written by [`compress`].
fn decompress(payload: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    brotli::Decompressor::new(payload, BROTLI_BUF).read_to_end(&mut out)?;
    Ok(out)
}

/// Default cap on a recording's on-disk size. When exceeded, the oldest
/// pre-checkpoint history is dropped (see [`FileRecorder`]).
pub const DEFAULT_MAX_RECORDING_BYTES: usize = 64 * 1024 * 1024;

/// Recording metadata, written once at the start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    pub cols: u16,
    pub rows: u16,
    /// Wall-clock start time, milliseconds since the Unix epoch.
    pub started_unix_ms: u64,
    /// The command the session runs (empty means the user's shell).
    pub command: Vec<String>,
}

/// What a recorded event carries (the on-wire payload of an events frame).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
enum EventBody {
    /// Bytes the terminal emitted.
    Output(Vec<u8>),
    /// A window-size change.
    Resize { cols: u16, rows: u16 },
}

/// A single timed event (on-wire).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Event {
    /// Milliseconds since the session started.
    t_ms: u64,
    body: EventBody,
}

/// One kitty-graphics image stored in a [`FRAME_IMAGES`] frame, content-addressed
/// by `hash` (over its dimensions and pixels). Stored once per recording; later
/// checkpoints reference it by hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ImageBlob {
    hash: u64,
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

/// A checkpoint's reference to a stored image: the content `hash` to resolve and
/// the `id` to re-transmit it under when reconstructing the dump on replay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ImageRef {
    id: u32,
    hash: u64,
}

/// A live image handed to [`Recorder::checkpoint_with_images`] (borrowed from the
/// emulator's graphics store, not yet hashed or copied).
pub struct CheckpointImage<'a> {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub pixels: &'a [u8],
}

/// Content hash of an image, over its dimensions and pixels. Only ever compared
/// against other hashes read back from the same recording, so the algorithm need
/// only be deterministic, not stable across versions.
///
/// The 64-bit hash is the dedup key without a byte re-check, so a collision
/// between two genuinely distinct images would alias them (the second is never
/// stored and resolves to the first's pixels). With SipHash over the full pixels
/// that is astronomically unlikely for any realistic image set; if it ever
/// mattered, the fix is to byte-verify on a hash hit before treating it as a dup.
fn image_hash(width: u32, height: u32, pixels: &[u8]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    width.hash(&mut h);
    height.hash(&mut h);
    pixels.hash(&mut h);
    h.finish()
}

/// The on-wire payload of a checkpoint frame: the emulator's serialized state
/// (an extended `dump`) plus the geometry it was taken at. As of format v2 the
/// dump omits image transmit escapes; the images it needs are listed in `images`
/// and stored in [`FRAME_IMAGES`] frames, and the reader reconstructs the full
/// dump by re-transmitting them before the dump.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Checkpoint {
    t_ms: u64,
    cols: u16,
    rows: u16,
    /// The dump bytes that reconstruct the emulator state when fed to a fresh vt,
    /// minus the image transmits (those are reconstructed from `images`).
    dump: Vec<u8>,
    /// References to the images this checkpoint's placeholders/placements need.
    images: Vec<ImageRef>,
}

/// A decoded timeline entry. Checkpoints and output/resize events are flattened
/// into one ordered sequence so a reader can replay or seek over them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    /// Bytes the terminal emitted, at `t_ms` since session start.
    Output { t_ms: u64, data: Vec<u8> },
    /// A window-size change.
    Resize { t_ms: u64, cols: u16, rows: u16 },
    /// A full-state checkpoint: a safe point to start replay from.
    Checkpoint {
        t_ms: u64,
        cols: u16,
        rows: u16,
        dump: Vec<u8>,
    },
}

/// Appends events to a recording, buffering them into compressed frames.
pub struct Recorder<W: Write> {
    writer: W,
    start: Instant,
    pending: Vec<Event>,
    pending_bytes: usize,
    /// Content hashes of images already written to a [`FRAME_IMAGES`] frame, so a
    /// checkpoint references an unchanged image rather than re-storing its bytes.
    written_hashes: HashSet<u64>,
}

/// A recording on disk whose size is kept bounded. It appends like any
/// [`Recorder`], but after each checkpoint, if the file has grown past its cap,
/// it compacts: the oldest history (everything before a checkpoint) is dropped,
/// keeping the most recent state that fits. Because a checkpoint is a complete
/// state, this loses nothing reconstructable about the retained window.
pub struct FileRecorder {
    inner: Recorder<BufWriter<File>>,
    path: std::path::PathBuf,
    /// Cap on the file's size, or `None` for an unbounded recording.
    max_bytes: Option<usize>,
}

impl FileRecorder {
    /// Create a recording at `path`, creating parent directories as needed.
    /// `max_bytes` caps the file's size (`None` = unbounded).
    pub fn create(
        path: &Path,
        cols: u16,
        rows: u16,
        command: &[String],
        max_bytes: Option<usize>,
    ) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let writer = BufWriter::new(File::create(path)?);
        Ok(FileRecorder {
            inner: Recorder::new(writer, cols, rows, command)?,
            path: path.to_path_buf(),
            max_bytes,
        })
    }

    /// Record a chunk of terminal output.
    pub fn output(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.inner.output(bytes)
    }

    /// Record a window-size change.
    pub fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        self.inner.resize(cols, rows)
    }

    /// Move the recording to `new_path`, keeping the open writer valid. The
    /// underlying file descriptor is unaffected by the rename, so buffered and
    /// future writes continue to land in the same (now-renamed) file; only the
    /// path used for later compaction is updated.
    pub fn rename(&mut self, new_path: &Path) -> io::Result<()> {
        if let Some(parent) = new_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(&self.path, new_path)?;
        self.path = new_path.to_path_buf();
        Ok(())
    }

    /// Write a checkpoint, then compact the file if it has grown past the cap.
    pub fn checkpoint(&mut self, cols: u16, rows: u16, dump: &[u8]) -> io::Result<()> {
        self.inner.checkpoint(cols, rows, dump)?;
        self.compact_if_needed()
    }

    /// Write a checkpoint with its referenced images (content-addressed dedup),
    /// then compact the file if it has grown past the cap.
    pub fn checkpoint_with_images(
        &mut self,
        cols: u16,
        rows: u16,
        dump: &[u8],
        images: &[CheckpointImage],
    ) -> io::Result<()> {
        self.inner
            .checkpoint_with_images(cols, rows, dump, images)?;
        self.compact_if_needed()
    }

    fn compact_if_needed(&mut self) -> io::Result<()> {
        let Some(max) = self.max_bytes else {
            return Ok(());
        };
        // The checkpoint just written is on disk only after a flush.
        self.inner.flush()?;
        if std::fs::metadata(&self.path)?.len() as usize <= max {
            return Ok(());
        }
        let bytes = std::fs::read(&self.path)?;
        // Keep the most recent history that fits in half the cap, so the file
        // grows back toward the cap before the next (infrequent) rewrite.
        let Some(bounded) = truncate_to_fit(&bytes, max / 2) else {
            return Ok(());
        };
        let mut tmp = self.path.clone().into_os_string();
        tmp.push(".tmp");
        let tmp = std::path::PathBuf::from(tmp);
        std::fs::write(&tmp, &bounded)?;
        std::fs::rename(&tmp, &self.path)?;
        // Compaction may have dropped image blobs the writer believed were on
        // disk; resync `written_hashes` to exactly what survived, so a later
        // re-transmit of a dropped image re-stores its blob rather than emitting
        // a reference whose bytes are gone.
        self.inner.written_hashes = stored_image_hashes(&bounded);
        // Continue appending to the freshly rewritten file.
        let file = std::fs::OpenOptions::new().append(true).open(&self.path)?;
        self.inner.writer = BufWriter::new(file);
        Ok(())
    }
}

/// The content hashes of every image physically stored (in a [`FRAME_IMAGES`]
/// frame) in an encoded recording. Used to resync a recorder's dedup set with
/// the file after compaction. Frames that fail to decode are skipped.
fn stored_image_hashes(bytes: &[u8]) -> HashSet<u64> {
    let mut hashes = HashSet::new();
    if bytes.len() < MAGIC.len() + 1 {
        return hashes;
    }
    let mut pos = MAGIC.len() + 1;
    let Some(hlen) = read_u32(bytes, &mut pos) else {
        return hashes;
    };
    if take(bytes, &mut pos, hlen).is_none() {
        return hashes;
    }
    while pos < bytes.len() {
        let kind = bytes[pos];
        let mut next = pos + 1;
        let Some(clen) = read_u32(bytes, &mut next) else {
            break;
        };
        let Some(payload) = take(bytes, &mut next, clen) else {
            break;
        };
        pos = next;
        if kind == FRAME_IMAGES
            && let Ok(raw) = decompress(payload)
            && let Ok(list) = postcard::from_bytes::<Vec<ImageBlob>>(&raw)
        {
            for b in list {
                hashes.insert(b.hash);
            }
        }
    }
    hashes
}

impl<W: Write> Recorder<W> {
    /// Start a recording on an arbitrary writer (the header is written now).
    pub fn new(mut writer: W, cols: u16, rows: u16, command: &[String]) -> io::Result<Self> {
        writer.write_all(MAGIC)?;
        writer.write_all(&[FORMAT_VERSION])?;
        let header = Header {
            cols,
            rows,
            started_unix_ms: now_unix_ms(),
            command: command.to_vec(),
        };
        write_len_prefixed(&mut writer, &to_postcard(&header)?)?;
        Ok(Recorder {
            writer,
            start: Instant::now(),
            pending: Vec::new(),
            pending_bytes: 0,
            written_hashes: HashSet::new(),
        })
    }

    /// Record a chunk of terminal output.
    pub fn output(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.pending.push(Event {
            t_ms: self.elapsed_ms(),
            body: EventBody::Output(bytes.to_vec()),
        });
        self.pending_bytes += bytes.len();
        if self.pending_bytes >= FLUSH_THRESHOLD {
            self.flush_frame()?;
        }
        Ok(())
    }

    /// Record a window-size change.
    pub fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        self.pending.push(Event {
            t_ms: self.elapsed_ms(),
            body: EventBody::Resize { cols, rows },
        });
        Ok(())
    }

    /// Write a full-state checkpoint with no images (see
    /// [`checkpoint_with_images`](Self::checkpoint_with_images)).
    pub fn checkpoint(&mut self, cols: u16, rows: u16, dump: &[u8]) -> io::Result<()> {
        self.checkpoint_with_images(cols, rows, dump, &[])
    }

    /// Write a full-state checkpoint: a safe point to start replay from. Any
    /// buffered output is flushed first so the checkpoint reflects everything
    /// recorded before it. `dump` is the emulator dump *without* image transmits;
    /// `images` are the graphics images it references. Each image not already
    /// stored is written to a [`FRAME_IMAGES`] frame first; the checkpoint then
    /// references all of them by content hash, so an unchanged image is stored
    /// once across many checkpoints.
    pub fn checkpoint_with_images(
        &mut self,
        cols: u16,
        rows: u16,
        dump: &[u8],
        images: &[CheckpointImage],
    ) -> io::Result<()> {
        self.flush_frame()?;

        let mut refs = Vec::with_capacity(images.len());
        let mut new_blobs = Vec::new();
        for img in images {
            let hash = image_hash(img.width, img.height, img.pixels);
            refs.push(ImageRef { id: img.id, hash });
            if self.written_hashes.insert(hash) {
                new_blobs.push(ImageBlob {
                    hash,
                    width: img.width,
                    height: img.height,
                    pixels: img.pixels.to_vec(),
                });
            }
        }
        if !new_blobs.is_empty() {
            let compressed = compress(&to_postcard(&new_blobs)?[..])?;
            self.writer.write_all(&[FRAME_IMAGES])?;
            write_len_prefixed(&mut self.writer, &compressed)?;
        }

        let ckpt = Checkpoint {
            t_ms: self.elapsed_ms(),
            cols,
            rows,
            dump: dump.to_vec(),
            images: refs,
        };
        let compressed = compress(&to_postcard(&ckpt)?[..])?;
        self.writer.write_all(&[FRAME_CHECKPOINT])?;
        write_len_prefixed(&mut self.writer, &compressed)?;
        Ok(())
    }

    /// Write any buffered events as a frame and flush the underlying writer.
    pub fn flush(&mut self) -> io::Result<()> {
        self.flush_frame()?;
        self.writer.flush()
    }

    fn elapsed_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    fn flush_frame(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let raw = to_postcard(&self.pending)?;
        let compressed = compress(&raw[..])?;
        self.writer.write_all(&[FRAME_EVENTS])?;
        write_len_prefixed(&mut self.writer, &compressed)?;
        self.pending.clear();
        self.pending_bytes = 0;
        Ok(())
    }
}

impl<W: Write> Drop for Recorder<W> {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

/// A decoded recording: the header plus the flattened, ordered timeline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recording {
    pub header: Header,
    pub items: Vec<Item>,
}

impl Recording {
    /// All recorded output bytes concatenated, in order (checkpoints ignored).
    pub fn output_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for item in &self.items {
            if let Item::Output { data, .. } = item {
                out.extend_from_slice(data);
            }
        }
        out
    }

    /// Index in [`items`](Self::items) of the most recent checkpoint, if any.
    pub fn latest_checkpoint(&self) -> Option<usize> {
        self.items
            .iter()
            .rposition(|i| matches!(i, Item::Checkpoint { .. }))
    }

    /// How many checkpoints the recording contains.
    pub fn checkpoint_count(&self) -> usize {
        self.items
            .iter()
            .filter(|i| matches!(i, Item::Checkpoint { .. }))
            .count()
    }
}

/// Read and decode a recording file.
pub fn read(path: &Path) -> io::Result<Recording> {
    read_bytes(&std::fs::read(path)?)
}

/// Decode a recording from an in-memory buffer. A torn trailing frame is
/// tolerated: decoding stops at the last complete frame.
pub fn read_bytes(bytes: &[u8]) -> io::Result<Recording> {
    if bytes.len() < MAGIC.len() + 1 || &bytes[..MAGIC.len()] != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a ghost recording",
        ));
    }
    let version = bytes[MAGIC.len()];
    if version != FORMAT_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported recording version {version}"),
        ));
    }
    let mut pos = MAGIC.len() + 1;

    let hlen = read_u32(bytes, &mut pos).ok_or_else(|| truncated("header length"))?;
    let hbytes = take(bytes, &mut pos, hlen).ok_or_else(|| truncated("header"))?;
    let header: Header = postcard::from_bytes(hbytes).map_err(io::Error::other)?;

    let mut items = Vec::new();
    // The content-addressed image store, accumulated from FRAME_IMAGES frames as
    // they are read, so a later checkpoint can reconstruct its image transmits.
    let mut image_store: HashMap<u64, ImageBlob> = HashMap::new();
    // Each frame: kind(u8) + clen(u32) + payload. Stop cleanly on a short read,
    // which means the writer was interrupted mid-frame.
    while pos < bytes.len() {
        let kind = bytes[pos];
        let mut next = pos + 1;
        let Some(clen) = read_u32(bytes, &mut next) else {
            break;
        };
        let Some(payload) = take(bytes, &mut next, clen) else {
            break;
        };
        pos = next;
        match kind {
            FRAME_EVENTS => {
                let raw = decompress(payload)?;
                let frame: Vec<Event> = postcard::from_bytes(&raw).map_err(io::Error::other)?;
                for e in frame {
                    items.push(match e.body {
                        EventBody::Output(data) => Item::Output { t_ms: e.t_ms, data },
                        EventBody::Resize { cols, rows } => Item::Resize {
                            t_ms: e.t_ms,
                            cols,
                            rows,
                        },
                    });
                }
            }
            FRAME_IMAGES => {
                let raw = decompress(payload)?;
                let blobs: Vec<ImageBlob> = postcard::from_bytes(&raw).map_err(io::Error::other)?;
                for blob in blobs {
                    image_store.insert(blob.hash, blob);
                }
            }
            FRAME_CHECKPOINT => {
                let raw = decompress(payload)?;
                let c: Checkpoint = postcard::from_bytes(&raw).map_err(io::Error::other)?;
                // Reconstruct the full dump: re-transmit the referenced images
                // (resolved from the store) before the transmit-free dump, so the
                // returned dump is self-contained like a v1 dump was.
                let mut dump = Vec::new();
                for r in &c.images {
                    if let Some(blob) = image_store.get(&r.hash) {
                        dump.extend_from_slice(
                            ghost_term::encode_transmit(
                                r.id,
                                blob.width,
                                blob.height,
                                &blob.pixels,
                            )
                            .as_bytes(),
                        );
                    }
                }
                dump.extend_from_slice(&c.dump);
                items.push(Item::Checkpoint {
                    t_ms: c.t_ms,
                    cols: c.cols,
                    rows: c.rows,
                    dump,
                });
            }
            // Unknown frame kinds (forward compatibility) are skipped.
            _ => {}
        }
    }

    Ok(Recording { header, items })
}

/// Produce a smaller recording retaining the most recent history that fits in
/// `target_bytes` of frames, cut at a checkpoint boundary: the header followed
/// by the earliest checkpoint whose suffix fits, and everything after it. A
/// checkpoint is a complete state, so this loses nothing reconstructable about
/// the retained window. If no checkpoint's suffix fits, the latest checkpoint
/// (smallest suffix) is kept as a best effort. Returns `None` if there is no
/// checkpoint to cut at.
pub fn truncate_to_fit(bytes: &[u8], target_bytes: usize) -> Option<Vec<u8>> {
    if bytes.len() < MAGIC.len() + 1 || &bytes[..MAGIC.len()] != MAGIC {
        return None;
    }
    let mut pos = MAGIC.len() + 1;
    let hlen = read_u32(bytes, &mut pos)?;
    take(bytes, &mut pos, hlen)?;
    let frames_start = pos;

    // Walk frames once, keeping each frame's (start, kind, payload).
    struct Frame<'a> {
        start: usize,
        kind: u8,
        payload: &'a [u8],
    }
    let mut frames = Vec::new();
    while pos < bytes.len() {
        let frame_start = pos;
        let kind = bytes[pos];
        let mut next = pos + 1;
        // Tolerate a torn trailing frame the way `read_bytes` does: stop at the
        // last complete frame rather than failing the whole compaction.
        let Some(clen) = read_u32(bytes, &mut next) else {
            break;
        };
        let Some(payload) = take(bytes, &mut next, clen) else {
            break;
        };
        frames.push(Frame {
            start: frame_start,
            kind,
            payload,
        });
        pos = next;
    }

    let len = bytes.len();
    // Suffix length shrinks as the cut moves later, so the earliest checkpoint
    // whose suffix fits retains the most history; fall back to the latest.
    let cut = frames
        .iter()
        .filter(|f| f.kind == FRAME_CHECKPOINT)
        .map(|f| f.start)
        .find(|&o| len - o <= target_bytes)
        .or_else(|| {
            frames
                .iter()
                .rev()
                .find(|f| f.kind == FRAME_CHECKPOINT)
                .map(|f| f.start)
        })?;

    // The cut drops every frame before it, including the FRAME_IMAGES frames that
    // stored the content-addressed images. So gather the images referenced by the
    // retained checkpoints and re-emit, at the front of the truncated recording,
    // those not already present in the retained suffix — otherwise their hashes
    // would no longer resolve. Image-free recordings collect nothing here, so the
    // output is byte-identical to a plain cut.
    let mut all_blobs: HashMap<u64, ImageBlob> = HashMap::new();
    let mut in_suffix: HashSet<u64> = HashSet::new();
    let mut needed: Vec<u64> = Vec::new();
    let mut needed_seen: HashSet<u64> = HashSet::new();
    for f in &frames {
        match f.kind {
            FRAME_IMAGES => {
                let raw = decompress(f.payload).ok()?;
                let list: Vec<ImageBlob> = postcard::from_bytes(&raw).ok()?;
                for b in list {
                    if f.start >= cut {
                        in_suffix.insert(b.hash);
                    }
                    all_blobs.insert(b.hash, b);
                }
            }
            FRAME_CHECKPOINT if f.start >= cut => {
                let raw = decompress(f.payload).ok()?;
                let c: Checkpoint = postcard::from_bytes(&raw).ok()?;
                for r in c.images {
                    if needed_seen.insert(r.hash) {
                        needed.push(r.hash);
                    }
                }
            }
            _ => {}
        }
    }
    let reemit: Vec<ImageBlob> = needed
        .iter()
        .filter(|h| !in_suffix.contains(h))
        .filter_map(|h| all_blobs.get(h).cloned())
        .collect();

    let mut out = Vec::with_capacity(frames_start + (len - cut) + 64);
    out.extend_from_slice(&bytes[..frames_start]);
    if !reemit.is_empty() {
        let compressed = compress(&to_postcard(&reemit).ok()?[..]).ok()?;
        out.push(FRAME_IMAGES);
        out.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
        out.extend_from_slice(&compressed);
    }
    out.extend_from_slice(&bytes[cut..]);
    Some(out)
}

/// Bound a recording to only its most recent checkpoint onward — the smallest
/// self-contained recording. Equivalent to [`truncate_to_fit`] with a target
/// of zero. Returns `None` if there is no checkpoint to cut at.
pub fn truncate_before_latest_checkpoint(bytes: &[u8]) -> Option<Vec<u8>> {
    truncate_to_fit(bytes, 0)
}

/// Write the recording as an [asciicast v2] stream (the asciinema format), so
/// it plays with `asciinema play`. The header line is the geometry; each event
/// line is `[seconds, "o"|"r", data]` with times normalized so the first event
/// is at 0. A bounded recording begins with a checkpoint, which is emitted as
/// the initial paint; mid-stream checkpoints are redundant with the output that
/// produced them and are skipped.
///
/// [asciicast v2]: https://docs.asciinema.org/manual/asciicast/v2/
pub fn write_asciicast<W: Write>(rec: &Recording, out: &mut W) -> io::Result<()> {
    let (width, height) = match rec.items.first() {
        Some(Item::Checkpoint { cols, rows, .. }) => (*cols, *rows),
        _ => (rec.header.cols, rec.header.rows),
    };
    let header = serde_json::json!({ "version": 2, "width": width, "height": height });
    writeln!(out, "{header}")?;

    let mut offset: Option<u64> = None;
    for (i, item) in rec.items.iter().enumerate() {
        let (t_ms, kind, data) = match item {
            Item::Output { t_ms, data } => (*t_ms, "o", String::from_utf8_lossy(data).into_owned()),
            Item::Resize { t_ms, cols, rows } => (*t_ms, "r", format!("{cols}x{rows}")),
            // A leading checkpoint reconstructs the starting screen; emit its
            // dump as the first output. Mid-stream ones add nothing playable.
            Item::Checkpoint { t_ms, dump, .. } if i == 0 => {
                (*t_ms, "o", String::from_utf8_lossy(dump).into_owned())
            }
            Item::Checkpoint { .. } => continue,
        };
        let off = *offset.get_or_insert(t_ms);
        let secs = t_ms.saturating_sub(off) as f64 / 1000.0;
        let line = serde_json::to_string(&(secs, kind, data)).map_err(io::Error::other)?;
        writeln!(out, "{line}")?;
    }
    Ok(())
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn to_postcard<T: Serialize>(value: &T) -> io::Result<Vec<u8>> {
    postcard::to_allocvec(value).map_err(io::Error::other)
}

fn write_len_prefixed<W: Write>(writer: &mut W, bytes: &[u8]) -> io::Result<()> {
    writer.write_all(&(bytes.len() as u32).to_le_bytes())?;
    writer.write_all(bytes)
}

fn read_u32(bytes: &[u8], pos: &mut usize) -> Option<usize> {
    let end = pos.checked_add(4)?;
    let slice = bytes.get(*pos..end)?;
    *pos = end;
    Some(u32::from_le_bytes(slice.try_into().unwrap()) as usize)
}

fn take<'a>(bytes: &'a [u8], pos: &mut usize, len: usize) -> Option<&'a [u8]> {
    let end = pos.checked_add(len)?;
    let slice = bytes.get(*pos..end)?;
    *pos = end;
    Some(slice)
}

fn truncated(what: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::UnexpectedEof,
        format!("recording truncated in {what}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record_to_buf(build: impl FnOnce(&mut Recorder<&mut Vec<u8>>)) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut rec = Recorder::new(&mut buf, 80, 24, &[]).unwrap();
            build(&mut rec);
            rec.flush().unwrap();
        }
        buf
    }

    /// Count the frames of a given kind in an encoded recording.
    fn count_frames(bytes: &[u8], target_kind: u8) -> usize {
        let mut pos = MAGIC.len() + 1;
        let hlen = read_u32(bytes, &mut pos).unwrap();
        take(bytes, &mut pos, hlen).unwrap();
        let mut count = 0;
        while pos < bytes.len() {
            let kind = bytes[pos];
            let mut next = pos + 1;
            let Some(clen) = read_u32(bytes, &mut next) else {
                break;
            };
            if take(bytes, &mut next, clen).is_none() {
                break;
            }
            if kind == target_kind {
                count += 1;
            }
            pos = next;
        }
        count
    }

    #[test]
    fn roundtrip_output_and_resize() {
        let buf = record_to_buf(|rec| {
            rec.output(b"hello ").unwrap();
            rec.resize(100, 30).unwrap();
            rec.output(b"world").unwrap();
        });

        let rec = read_bytes(&buf).unwrap();
        assert_eq!(rec.header.cols, 80);
        assert_eq!(rec.header.rows, 24);
        let bodies: Vec<_> = rec
            .items
            .iter()
            .map(|i| match i {
                Item::Output { data, .. } => ("o", data.clone(), 0, 0),
                Item::Resize { cols, rows, .. } => ("r", Vec::new(), *cols, *rows),
                Item::Checkpoint { .. } => ("c", Vec::new(), 0, 0),
            })
            .collect();
        assert_eq!(
            bodies,
            vec![
                ("o", b"hello ".to_vec(), 0, 0),
                ("r", Vec::new(), 100, 30),
                ("o", b"world".to_vec(), 0, 0),
            ]
        );
        assert_eq!(rec.output_bytes(), b"hello world");
    }

    fn item_t_ms(i: &Item) -> u64 {
        match i {
            Item::Output { t_ms, .. }
            | Item::Resize { t_ms, .. }
            | Item::Checkpoint { t_ms, .. } => *t_ms,
        }
    }

    #[test]
    fn timestamps_are_monotonic() {
        let buf = record_to_buf(|rec| {
            rec.output(b"a").unwrap();
            rec.output(b"b").unwrap();
        });
        let rec = read_bytes(&buf).unwrap();
        assert!(
            rec.items
                .windows(2)
                .all(|w| item_t_ms(&w[0]) <= item_t_ms(&w[1]))
        );
    }

    /// Write a content-addressed checkpoint from a vt, as the server does: the
    /// transmit-free dump plus references to the graphics images.
    fn checkpoint_vt(rec: &mut Recorder<&mut Vec<u8>>, vt: &ghost_term::Vt) {
        let dump = vt.dump_with_scrollback_without_images().into_bytes();
        let refs: Vec<CheckpointImage> = vt
            .graphics_images()
            .map(|i| CheckpointImage {
                id: i.id,
                width: i.width,
                height: i.height,
                pixels: &i.pixels,
            })
            .collect();
        rec.checkpoint_with_images(20, 5, &dump, &refs).unwrap();
    }

    /// As [`checkpoint_vt`] but to a [`FileRecorder`] (so compaction can fire).
    fn checkpoint_file(rec: &mut FileRecorder, vt: &ghost_term::Vt) {
        let dump = vt.dump_with_scrollback_without_images().into_bytes();
        let refs: Vec<CheckpointImage> = vt
            .graphics_images()
            .map(|i| CheckpointImage {
                id: i.id,
                width: i.width,
                height: i.height,
                pixels: &i.pixels,
            })
            .collect();
        rec.checkpoint_with_images(20, 5, &dump, &refs).unwrap();
    }

    #[test]
    fn a_recurring_image_survives_compaction_that_dropped_its_blob() {
        use ghost_term::Vt;

        // Regression: the writer must not believe a blob is still on disk after
        // compaction dropped it, or a returning image becomes a dangling
        // reference and silently vanishes on replay.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rec.ghostrec");
        // A 2×1 image displayed at the cursor (its size is irrelevant; what
        // matters is that its blob frame exists, then is dropped, then is needed).
        let img = "\x1b_Gi=7,a=T,f=24,s=2,v=1,c=1,r=1;/wAAAP8A\x1b\\";
        // Incompressible filler so a recorded output frame actually grows the file
        // past the cap (compression would crush repetitive text away).
        let filler = |seed: u32, n: usize| -> Vec<u8> {
            let mut x = seed;
            (0..n)
                .map(|_| {
                    x = x.wrapping_mul(1664525).wrapping_add(1013904223);
                    (x >> 24) as u8
                })
                .collect()
        };

        let cap = 4 * 1024;
        let mut rec = FileRecorder::create(&path, 20, 5, &[], Some(cap)).unwrap();
        let mut vt = Vt::new(20, 5);

        // Phase 1: image displayed; its blob is written to disk once.
        vt.feed_str(img);
        checkpoint_file(&mut rec, &vt);

        // Phase 2: image gone, then a big incompressible output forces a single
        // compaction whose retained (image-free) checkpoint doesn't reference the
        // image — so its blob is dropped from disk.
        vt.feed_str("\x1b_Ga=d,d=A\x1b\\");
        assert_eq!(vt.graphics_image_count(), 0);
        rec.output(&filler(1, 8 * 1024)).unwrap();
        checkpoint_file(&mut rec, &vt);
        assert!(
            std::fs::metadata(&path).unwrap().len() < cap as u64,
            "phase 2 should have compacted below the cap"
        );

        // Phase 3: the same image returns once; the file stays under the cap, so a
        // stale dedup set would leave a dangling reference with no blob on disk.
        vt.feed_str(img);
        rec.output(b"phase3\r\n").unwrap();
        checkpoint_file(&mut rec, &vt);
        drop(rec);

        // The latest checkpoint must still reconstruct the recurring image.
        let recording = read(&path).unwrap();
        let ck = recording.latest_checkpoint().unwrap();
        let Item::Checkpoint { dump, .. } = &recording.items[ck] else {
            panic!("expected a checkpoint");
        };
        let mut fresh = Vt::new(20, 5);
        fresh.feed_str(std::str::from_utf8(dump).unwrap());
        assert_eq!(
            fresh.graphics_image_count(),
            1,
            "the image that left and returned must survive compaction"
        );
    }

    #[test]
    fn checkpoint_bakes_in_images_for_self_contained_replay() {
        use ghost_term::Vt;

        // A session transmits an image; the periodic checkpoint snapshots it via
        // the content-addressed dedup path.
        let mut vt = Vt::new(20, 5);
        vt.feed_str("\x1b_Gi=7,a=t,f=24,s=2,v=1;/wAAAP8A\x1b\\");
        let buf = record_to_buf(|rec| checkpoint_vt(rec, &vt));

        // A player seeking to the checkpoint feeds only the reconstructed dump to
        // a fresh terminal; the image must come back, so replay is self-contained.
        let rec = read_bytes(&buf).unwrap();
        let ck = rec.latest_checkpoint().unwrap();
        let Item::Checkpoint { dump, .. } = &rec.items[ck] else {
            panic!("expected a checkpoint");
        };
        let mut fresh = Vt::new(20, 5);
        fresh.feed_str(std::str::from_utf8(dump).unwrap());
        assert_eq!(
            fresh.graphics_image_count(),
            1,
            "the checkpoint baked the image in"
        );
    }

    #[test]
    fn an_unchanged_image_is_stored_once_across_checkpoints() {
        use ghost_term::Vt;

        // The same image is displayed across many checkpoints. Content-addressed
        // dedup must store its bytes once: only one FRAME_IMAGES frame appears.
        let mut vt = Vt::new(20, 5);
        vt.feed_str("\x1b_Gi=7,a=t,f=24,s=2,v=1;/wAAAP8A\x1b\\");
        let buf = record_to_buf(|rec| {
            for _ in 0..5 {
                rec.output(b"tick").unwrap();
                checkpoint_vt(rec, &vt);
            }
        });

        let image_frames = count_frames(&buf, FRAME_IMAGES);
        assert_eq!(
            image_frames, 1,
            "the image is stored once, not per checkpoint"
        );
        assert_eq!(read_bytes(&buf).unwrap().checkpoint_count(), 5);
    }

    #[test]
    fn truncation_re_emits_images_a_retained_checkpoint_needs() {
        use ghost_term::Vt;

        // The image is stored before the first checkpoint. Bounding to the latest
        // checkpoint drops that early image frame, so truncation must re-emit the
        // image the retained checkpoint references — else its hash won't resolve.
        let mut vt = Vt::new(20, 5);
        vt.feed_str("\x1b_Gi=7,a=t,f=24,s=2,v=1;/wAAAP8A\x1b\\");
        let buf = record_to_buf(|rec| {
            rec.output(b"first").unwrap();
            checkpoint_vt(rec, &vt); // stores the image blob here
            rec.output(b"second").unwrap();
            checkpoint_vt(rec, &vt); // references it again, no new blob
        });

        let bounded = truncate_before_latest_checkpoint(&buf).unwrap();
        let rec = read_bytes(&bounded).unwrap();
        let ck = rec.latest_checkpoint().unwrap();
        let Item::Checkpoint { dump, .. } = &rec.items[ck] else {
            panic!("expected a checkpoint");
        };
        let mut fresh = Vt::new(20, 5);
        fresh.feed_str(std::str::from_utf8(dump).unwrap());
        assert_eq!(
            fresh.graphics_image_count(),
            1,
            "truncation re-emitted the image the retained checkpoint needs"
        );
    }

    #[test]
    fn checkpoints_decode_in_order_and_truncate() {
        let buf = record_to_buf(|rec| {
            rec.output(b"before").unwrap();
            rec.checkpoint(20, 5, b"STATE-DUMP").unwrap();
            rec.output(b"after").unwrap();
        });

        let rec = read_bytes(&buf).unwrap();
        assert_eq!(rec.checkpoint_count(), 1);
        let ck = rec.latest_checkpoint().unwrap();
        assert!(matches!(
            &rec.items[ck],
            Item::Checkpoint { cols: 20, rows: 5, dump, .. } if dump == b"STATE-DUMP"
        ));
        // "before" precedes the checkpoint, "after" follows it.
        assert!(matches!(&rec.items[ck - 1], Item::Output { data, .. } if data == b"before"));
        assert!(matches!(&rec.items[ck + 1], Item::Output { data, .. } if data == b"after"));

        // Bounding at the checkpoint keeps a valid recording that begins there.
        let bounded = read_bytes(&truncate_before_latest_checkpoint(&buf).unwrap()).unwrap();
        assert!(matches!(
            bounded.items.first(),
            Some(Item::Checkpoint { .. })
        ));
        assert_eq!(bounded.output_bytes(), b"after");
    }

    #[test]
    fn truncate_to_fit_keeps_a_recent_checkpoint_window() {
        // Five checkpoints, each preceded by a distinct output segment.
        let buf = record_to_buf(|rec| {
            for seg in 0..5 {
                rec.output(format!("seg{seg}-payload\r\n").as_bytes())
                    .unwrap();
                rec.checkpoint(20, 5, format!("DUMP{seg}").as_bytes())
                    .unwrap();
            }
        });
        let full = read_bytes(&buf).unwrap();
        assert_eq!(full.checkpoint_count(), 5);

        let dump_of = |r: &Recording| match &r.items[r.latest_checkpoint().unwrap()] {
            Item::Checkpoint { dump, .. } => dump.clone(),
            _ => unreachable!(),
        };

        // A target that fits only the most recent part drops early checkpoints
        // but keeps the latest one and stays within the budget.
        let target = buf.len() / 3;
        let bounded_bytes = truncate_to_fit(&buf, target).unwrap();
        assert!(bounded_bytes.len() < buf.len());
        let bounded = read_bytes(&bounded_bytes).unwrap();
        assert!(matches!(
            bounded.items.first(),
            Some(Item::Checkpoint { .. })
        ));
        assert!(bounded.checkpoint_count() >= 1);
        assert!(bounded.checkpoint_count() < full.checkpoint_count());
        // The retained window ends at the same latest checkpoint.
        assert_eq!(dump_of(&bounded), dump_of(&full));
        assert_eq!(dump_of(&full), b"DUMP4");
    }

    #[test]
    fn spans_multiple_frames() {
        // Enough output to cross the flush threshold several times.
        let chunk = vec![b'x'; 10 * 1024];
        let buf = record_to_buf(|rec| {
            for _ in 0..20 {
                rec.output(&chunk).unwrap();
            }
        });
        let rec = read_bytes(&buf).unwrap();
        assert_eq!(rec.output_bytes().len(), 20 * 10 * 1024);
    }

    #[test]
    fn tolerates_a_torn_final_frame() {
        let buf = record_to_buf(|rec| {
            rec.output(b"intact").unwrap();
            // Force the first frame out so it is complete on disk...
            rec.flush().unwrap();
            rec.output(b"lost").unwrap();
        });
        // Lop off part of the (second) trailing frame to simulate a crash.
        let torn = &buf[..buf.len() - 3];
        let rec = read_bytes(torn).unwrap();
        assert_eq!(rec.output_bytes(), b"intact");
    }

    #[test]
    fn rejects_bad_magic() {
        let err = read_bytes(b"not a recording at all").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn file_recorder_rename_moves_file_and_keeps_writing() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.ghostrec");
        let new = dir.path().join("new.ghostrec");

        {
            let mut rec = FileRecorder::create(&old, 80, 24, &[], None).unwrap();
            rec.output(b"before-rename ").unwrap();
            rec.rename(&new).unwrap();
            rec.output(b"after-rename").unwrap();
            // Drop flushes the buffered frame to the (renamed) file.
        }

        assert!(!old.exists(), "old recording path should be gone");
        assert!(new.exists(), "recording should be at the new path");
        let rec = read(&new).unwrap();
        assert_eq!(rec.output_bytes(), b"before-rename after-rename");
    }

    fn asciicast(rec: &Recording) -> (serde_json::Value, Vec<serde_json::Value>) {
        let mut buf = Vec::new();
        write_asciicast(rec, &mut buf).unwrap();
        let text = String::from_utf8(buf).unwrap();
        let mut lines = text.lines();
        let header = serde_json::from_str(lines.next().unwrap()).unwrap();
        let events = lines.map(|l| serde_json::from_str(l).unwrap()).collect();
        (header, events)
    }

    #[test]
    fn exports_asciicast_v2() {
        let rec = Recording {
            header: Header {
                cols: 80,
                rows: 24,
                started_unix_ms: 0,
                command: vec![],
            },
            items: vec![
                Item::Output {
                    t_ms: 0,
                    data: b"hi\r\n".to_vec(),
                },
                Item::Resize {
                    t_ms: 100,
                    cols: 100,
                    rows: 30,
                },
                // Mid-stream checkpoint: must be skipped on export.
                Item::Checkpoint {
                    t_ms: 150,
                    cols: 100,
                    rows: 30,
                    dump: b"X".to_vec(),
                },
                Item::Output {
                    t_ms: 200,
                    data: b"bye".to_vec(),
                },
            ],
        };
        let (header, evs) = asciicast(&rec);
        assert_eq!(header["version"], 2);
        assert_eq!(header["width"], 80);
        assert_eq!(header["height"], 24);
        assert_eq!(evs.len(), 3, "mid-stream checkpoint should be skipped");
        assert_eq!(
            (evs[0][1].as_str(), evs[0][2].as_str()),
            (Some("o"), Some("hi\r\n"))
        );
        assert_eq!(
            (evs[1][1].as_str(), evs[1][2].as_str()),
            (Some("r"), Some("100x30"))
        );
        assert_eq!(
            (evs[2][1].as_str(), evs[2][2].as_str()),
            (Some("o"), Some("bye"))
        );
        // Times are normalized to the first event and monotonic.
        assert_eq!(evs[0][0].as_f64(), Some(0.0));
        assert!((evs[2][0].as_f64().unwrap() - 0.2).abs() < 1e-9);
    }

    #[test]
    fn exports_leading_checkpoint_as_initial_paint() {
        let rec = Recording {
            header: Header {
                cols: 80,
                rows: 24,
                started_unix_ms: 0,
                command: vec![],
            },
            items: vec![
                Item::Checkpoint {
                    t_ms: 5000,
                    cols: 120,
                    rows: 40,
                    dump: b"STATE".to_vec(),
                },
                Item::Output {
                    t_ms: 5200,
                    data: b"more".to_vec(),
                },
            ],
        };
        let (header, evs) = asciicast(&rec);
        // Geometry comes from the leading checkpoint.
        assert_eq!(header["width"], 120);
        assert_eq!(header["height"], 40);
        assert_eq!(evs.len(), 2);
        // The leading checkpoint dump is the initial paint, at normalized t=0.
        assert_eq!(evs[0][0].as_f64(), Some(0.0));
        assert_eq!(
            (evs[0][1].as_str(), evs[0][2].as_str()),
            (Some("o"), Some("STATE"))
        );
        assert!((evs[1][0].as_f64().unwrap() - 0.2).abs() < 1e-9);
        assert_eq!(evs[1][2].as_str(), Some("more"));
    }
}
