//! Full-text search over recorded session output.
//!
//! Recordings are framed-brotli binaries (see [`crate::record`]), so a plain
//! `grep` over the files finds nothing useful. This module replays each
//! recording through the terminal emulator ([`Screen`]) and searches the
//! resulting text — the same lines you would have seen scroll by — so a match is
//! against what the session actually rendered, not its raw escape stream.

use crate::record::Recording;
use crate::screen::Screen;
use crate::{paths, record};
use std::io;

/// Scrollback retained while replaying a recording for search. Generous so a
/// long session's earlier output is still searchable; recordings are size-capped
/// (see [`crate::record::DEFAULT_MAX_RECORDING_BYTES`]) and keep recent history,
/// so in practice this covers the whole file.
const SEARCH_SCROLLBACK: usize = 100_000;

/// A single matching line within a session's rendered output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// The session id (the recording's file stem).
    pub session: String,
    /// 1-based line number within the replayed text (scrollback + screen).
    pub line: usize,
    /// The matching line, trailing whitespace trimmed.
    pub text: String,
}

/// Search one recording's replayed text for `needle`, returning a [`Hit`] per
/// matching line. An empty `needle` matches nothing (rather than every line).
pub fn search_recording(
    session: &str,
    rec: &Recording,
    needle: &str,
    ignore_case: bool,
) -> Vec<Hit> {
    if needle.is_empty() {
        return Vec::new();
    }
    let screen = Screen::from_recording(rec, SEARCH_SCROLLBACK);
    let fold = |s: &str| {
        if ignore_case {
            s.to_lowercase()
        } else {
            s.to_string()
        }
    };
    let target = fold(needle);
    screen
        .text()
        .into_iter()
        .enumerate()
        .filter(|(_, line)| fold(line).contains(&target))
        .map(|(i, line)| Hit {
            session: session.to_string(),
            line: i + 1,
            text: line.trim_end().to_string(),
        })
        .collect()
}

/// Search recorded output for `needle`. With `only` set, searches just that
/// session's recording; otherwise every recording in [`paths::recordings_dir`],
/// in stable session-name order. A recording that fails to decode is skipped.
pub fn search(needle: &str, ignore_case: bool, only: Option<&str>) -> io::Result<Vec<Hit>> {
    let mut names = match only {
        Some(name) => vec![name.to_string()],
        None => recording_names()?,
    };
    names.sort();
    let mut hits = Vec::new();
    for name in names {
        let path = paths::recording_path(&name);
        let Ok(rec) = record::read(&path) else {
            continue;
        };
        hits.extend(search_recording(&name, &rec, needle, ignore_case));
    }
    Ok(hits)
}

/// The session ids of every recording on disk (recording file stems). A missing
/// recordings directory is not an error — it just means nothing to search.
fn recording_names() -> io::Result<Vec<String>> {
    let dir = paths::recordings_dir();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut names = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("ghostrec")
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        {
            names.push(stem.to_string());
        }
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a recording from a raw output stream (escape sequences included).
    fn recording(output: &[u8]) -> Recording {
        let mut buf = Vec::new();
        {
            let mut rec = record::Recorder::new(&mut buf, 80, 24, &[]).unwrap();
            rec.output(output).unwrap();
            rec.flush().unwrap();
        }
        record::read_bytes(&buf).unwrap()
    }

    #[test]
    fn matches_rendered_text_ignoring_escape_sequences() {
        // The needle is split across SGR escapes in the raw stream; only the
        // rendered line ("ERROR: disk boom") should be searched.
        let rec = recording(b"\x1b[31mERROR\x1b[0m: disk boom\r\nsecond line\r\n");
        let hits = search_recording("sess", &rec, "ERROR: disk boom", false);
        assert_eq!(hits.len(), 1, "one line matches: {hits:?}");
        assert_eq!(hits[0].session, "sess");
        assert_eq!(hits[0].line, 1);
        assert_eq!(hits[0].text, "ERROR: disk boom");
    }

    #[test]
    fn case_insensitive_search_folds_both_sides() {
        let rec = recording(b"Building Widget\r\n");
        assert!(
            search_recording("s", &rec, "widget", false).is_empty(),
            "case-sensitive miss"
        );
        let hits = search_recording("s", &rec, "widget", true);
        assert_eq!(hits.len(), 1, "case-insensitive hit: {hits:?}");
        assert_eq!(hits[0].text, "Building Widget");
    }

    #[test]
    fn no_match_and_empty_needle_yield_nothing() {
        let rec = recording(b"hello world\r\n");
        assert!(search_recording("s", &rec, "absent", false).is_empty());
        assert!(
            search_recording("s", &rec, "", false).is_empty(),
            "an empty needle matches nothing, not everything"
        );
    }
}
