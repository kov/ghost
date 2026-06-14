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
//!          kind  u8            (0 = events; others reserved, e.g. checkpoint)
//!          clen  u32 LE        compressed payload length
//!          data  [clen]        zstd( postcard(Vec<Event>) )
//! ```
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

/// What a recorded event carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventBody {
    /// Bytes the terminal emitted.
    Output(Vec<u8>),
    /// A window-size change.
    Resize { cols: u16, rows: u16 },
}

/// A single timed event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// Milliseconds since the session started.
    pub t_ms: u64,
    pub body: EventBody,
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

/// A decoded recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recording {
    pub header: Header,
    pub events: Vec<Event>,
}

impl Recording {
    /// All recorded output bytes concatenated, in order.
    pub fn output_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for e in &self.events {
            if let EventBody::Output(b) = &e.body {
                out.extend_from_slice(b);
            }
        }
        out
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

    let mut events = Vec::new();
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
        if kind == FRAME_EVENTS {
            let raw = zstd::decode_all(payload)?;
            let mut frame: Vec<Event> = postcard::from_bytes(&raw).map_err(io::Error::other)?;
            events.append(&mut frame);
        }
        // Unknown frame kinds (e.g. future checkpoints) are skipped.
    }

    Ok(Recording { header, events })
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
        let bodies: Vec<_> = rec.events.iter().map(|e| e.body.clone()).collect();
        assert_eq!(
            bodies,
            vec![
                EventBody::Output(b"hello ".to_vec()),
                EventBody::Resize {
                    cols: 100,
                    rows: 30
                },
                EventBody::Output(b"world".to_vec()),
            ]
        );
        assert_eq!(rec.output_bytes(), b"hello world");
    }

    #[test]
    fn timestamps_are_monotonic() {
        let buf = record_to_buf(|rec| {
            rec.output(b"a").unwrap();
            rec.output(b"b").unwrap();
        });
        let rec = read_bytes(&buf).unwrap();
        assert!(rec.events.windows(2).all(|w| w[0].t_ms <= w[1].t_ms));
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
