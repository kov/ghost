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

    /// Write a checkpoint, then compact the file if it has grown past the cap.
    pub fn checkpoint(&mut self, cols: u16, rows: u16, dump: &[u8]) -> io::Result<()> {
        self.inner.checkpoint(cols, rows, dump)?;
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
        // Continue appending to the freshly rewritten file.
        let file = std::fs::OpenOptions::new().append(true).open(&self.path)?;
        self.inner.writer = BufWriter::new(file);
        Ok(())
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

    // Offsets of every checkpoint frame, earliest first.
    let mut checkpoints = Vec::new();
    while pos < bytes.len() {
        let frame_start = pos;
        let kind = bytes[pos];
        let mut next = pos + 1;
        let clen = read_u32(bytes, &mut next)?;
        take(bytes, &mut next, clen)?;
        if kind == FRAME_CHECKPOINT {
            checkpoints.push(frame_start);
        }
        pos = next;
    }

    let len = bytes.len();
    // Suffix length shrinks as the cut moves later, so the earliest checkpoint
    // whose suffix fits retains the most history; fall back to the latest.
    let cut = checkpoints
        .iter()
        .copied()
        .find(|&o| len - o <= target_bytes)
        .or_else(|| checkpoints.last().copied())?;

    let mut out = Vec::with_capacity(frames_start + (len - cut));
    out.extend_from_slice(&bytes[..frames_start]);
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
