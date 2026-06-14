//! The on-disk recording: a framed, per-frame zstd-compressed asciicast with
//! periodic state checkpoints, supporting append, seek, and tail-on-attach.
//!
//! The recording (archival, raw bytes) and the resync (emulator state) are
//! distinct roles that share this format: a checkpoint is the emulator's
//! serialized state, and the frames between checkpoints are the raw output.
//!
//! ## Layout
//!
//! ```text
//! magic  "GHOSTREC"            8 bytes
//! ver    u8                    format version
//! header u32 len + postcard(Header)
//! frame* repeated until EOF:
//!          kind  u8            (0 = events, 1 = checkpoint; others reserved)
//!          clen  u32 LE        compressed payload length
//!          data  [clen]        zstd( postcard(Vec<Event>) )  for kind 0
//!                              zstd( postcard(Checkpoint) )   for kind 1
//! ```
//!
//! A checkpoint frame carries a full emulator dump: a safe point to start
//! replay from, and a safe point to cut the file at when bounding its size
//! (everything before a checkpoint can be dropped losslessly).
//!
//! Frames are independently compressed and length-prefixed, so the writer can
//! append incrementally with bounded memory and a reader can stop cleanly at a
//! torn final frame (e.g. after a crash) without failing — it returns every
//! complete frame it found.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const MAGIC: &[u8; 8] = b"GHOSTREC";
const FORMAT_VERSION: u8 = 1;
const FRAME_EVENTS: u8 = 0;
const FRAME_CHECKPOINT: u8 = 1;
const ZSTD_LEVEL: i32 = 3;
/// Flush a frame once this many bytes of output have accumulated.
const FLUSH_THRESHOLD: usize = 64 * 1024;

/// A `Recorder` writing to a buffered file — the host's concrete recorder type.
pub type FileRecorder = Recorder<BufWriter<File>>;

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

/// The on-wire payload of a checkpoint frame: the emulator's serialized state
/// (an extended `dump`) plus the geometry it was taken at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Checkpoint {
    t_ms: u64,
    cols: u16,
    rows: u16,
    /// The dump bytes that reconstruct the emulator state when fed to a fresh vt.
    dump: Vec<u8>,
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
}

impl FileRecorder {
    /// Create a recording at `path`, creating parent directories as needed.
    pub fn create(path: &Path, cols: u16, rows: u16, command: &[String]) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let writer = BufWriter::new(File::create(path)?);
        Recorder::new(writer, cols, rows, command)
    }
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

    /// Write a full-state checkpoint: a safe point to start replay from. Any
    /// buffered output is flushed first so the checkpoint reflects everything
    /// recorded before it. `dump` is an extended emulator dump (state as bytes).
    pub fn checkpoint(&mut self, cols: u16, rows: u16, dump: &[u8]) -> io::Result<()> {
        self.flush_frame()?;
        let ckpt = Checkpoint {
            t_ms: self.elapsed_ms(),
            cols,
            rows,
            dump: dump.to_vec(),
        };
        let compressed = zstd::encode_all(&to_postcard(&ckpt)?[..], ZSTD_LEVEL)?;
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
        let compressed = zstd::encode_all(&raw[..], ZSTD_LEVEL)?;
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
                let raw = zstd::decode_all(payload)?;
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
            FRAME_CHECKPOINT => {
                let raw = zstd::decode_all(payload)?;
                let c: Checkpoint = postcard::from_bytes(&raw).map_err(io::Error::other)?;
                items.push(Item::Checkpoint {
                    t_ms: c.t_ms,
                    cols: c.cols,
                    rows: c.rows,
                    dump: c.dump,
                });
            }
            // Unknown frame kinds (forward compatibility) are skipped.
            _ => {}
        }
    }

    Ok(Recording { header, items })
}

/// Produce a smaller recording that starts at the most recent checkpoint:
/// the header followed by the latest checkpoint frame and everything after it.
/// This is the safe way to bound a recording's size — a checkpoint is a
/// complete state, so dropping the frames before it loses no reconstructable
/// information. Returns `None` if there is no checkpoint to cut at.
pub fn truncate_before_latest_checkpoint(bytes: &[u8]) -> Option<Vec<u8>> {
    if bytes.len() < MAGIC.len() + 1 || &bytes[..MAGIC.len()] != MAGIC {
        return None;
    }
    let mut pos = MAGIC.len() + 1;
    let hlen = read_u32(bytes, &mut pos)?;
    take(bytes, &mut pos, hlen)?;
    let frames_start = pos;

    let mut last_checkpoint: Option<usize> = None;
    while pos < bytes.len() {
        let frame_start = pos;
        let kind = bytes[pos];
        let mut next = pos + 1;
        let clen = read_u32(bytes, &mut next)?;
        take(bytes, &mut next, clen)?;
        if kind == FRAME_CHECKPOINT {
            last_checkpoint = Some(frame_start);
        }
        pos = next;
    }

    let cut = last_checkpoint?;
    let mut out = Vec::with_capacity(frames_start + (bytes.len() - cut));
    out.extend_from_slice(&bytes[..frames_start]);
    out.extend_from_slice(&bytes[cut..]);
    Some(out)
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
}
