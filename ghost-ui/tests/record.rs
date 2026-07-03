//! End-to-end tests for session recording: the host writes a durable,
//! framed-zstd recording of session output that can be decoded back.
//!
//! The recorded children end themselves with a SIGTERM (`kill $$`) rather
//! than exiting cleanly: a clean exit is an explicit end and discards the
//! recording with the session, while a signaled death — the stand-in for a
//! crash or logout here — flushes and keeps it.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use ghost_vt::record;

const GHOST: &str = env!("CARGO_BIN_EXE_ghost");

fn wait_until(timeout: Duration, mut pred: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    loop {
        if pred() {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Where a recording lands given the temp `XDG_DATA_HOME`.
fn recording_path(data_home: &Path, name: &str) -> PathBuf {
    data_home
        .join("ghost")
        .join("recordings")
        .join(format!("{name}.ghostrec"))
}

/// Wait until the recording at `path` is readable and its reconstructed screen
/// shows `marker`. This is the reliable "session is done and flushed" signal:
/// the marker is the child's final output, so seeing it proves the session
/// started, ran to completion, and the daemon finished writing the recording.
///
/// Polling session *listing* instead is racy — `!ls.contains(name)` reads as
/// true both after the session ends and (under load) before the daemon has even
/// registered, so a test can race ahead and read a not-yet-written file.
fn wait_for_recording_marker(path: &Path, marker: &str) -> bool {
    wait_until(Duration::from_secs(15), || {
        let Ok(rec) = record::read(path) else {
            return false;
        };
        let screen = ghost_vt::screen::Screen::from_recording(&rec, 1000);
        screen.text().iter().any(|l| l.contains(marker))
    })
}

#[test]
fn session_output_is_recorded() {
    let run = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let name = "rec-basic";

    // A short-lived session that prints a marker and dies (uncleanly, so the
    // recording survives), making the host flush and close it on its own.
    let out = Command::new(GHOST)
        .env("XDG_RUNTIME_DIR", run.path())
        .env("XDG_DATA_HOME", data.path())
        .args([
            "new",
            name,
            "-d",
            "--",
            "sh",
            "-c",
            "echo RECORDED-MARKER; kill -TERM $$",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let path = recording_path(data.path(), name);
    assert!(
        wait_until(Duration::from_secs(5), || {
            record::read(&path)
                .map(|r| String::from_utf8_lossy(&r.output_bytes()).contains("RECORDED-MARKER"))
                .unwrap_or(false)
        }),
        "recording missing or lacks the marker at {}",
        path.display()
    );
}

#[test]
fn no_record_flag_skips_recording() {
    let run = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let name = "rec-disabled";

    let out = Command::new(GHOST)
        .env("XDG_RUNTIME_DIR", run.path())
        .env("XDG_DATA_HOME", data.path())
        .args([
            "new",
            name,
            "-d",
            "--no-record",
            "--",
            "sh",
            "-c",
            "echo NOPE",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Give the (now-exited) session time to have written anything it might.
    std::thread::sleep(Duration::from_millis(300));
    let path = recording_path(data.path(), name);
    assert!(
        !path.exists(),
        "recording was written despite --no-record: {}",
        path.display()
    );
}

#[test]
fn long_session_writes_checkpoints() {
    let run = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let name = "rec-checkpoints";

    // Emit several MB — comfortably more than the host's (default-cap)
    // checkpoint interval — then a sentinel, then exit. The sentinel lets us
    // know the whole recording has been flushed before we inspect it (the file
    // is written concurrently as the session runs, and a mid-write read would be
    // incomplete).
    let out = Command::new(GHOST)
        .env("XDG_RUNTIME_DIR", run.path())
        .env("XDG_DATA_HOME", data.path())
        .args([
            "new",
            name,
            "-d",
            "--",
            "sh",
            "-c",
            "seq 1 1000000; echo DONE-CHK; kill -TERM $$",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Wait until the recording is complete (sentinel present), then assert it
    // reconstructs to the true final screen with at least one mid-session
    // checkpoint.
    let path = recording_path(data.path(), name);
    assert!(
        wait_for_recording_marker(&path, "DONE-CHK"),
        "recording did not reconstruct to completion at {}",
        path.display()
    );
    let rec = record::read(&path).unwrap();
    assert!(
        rec.checkpoint_count() >= 1,
        "long session lacked a mid-session checkpoint"
    );
}

#[test]
fn a_non_rendering_flood_writes_no_checkpoints() {
    let run = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let name = "rec-idle";

    // A 1 MiB cap pins the checkpoint interval to its 128 KiB floor. The child
    // then emits ~450 KiB of SGR resets — several intervals — without ever
    // touching a cell, then renders a sentinel and exits. Every interval boundary
    // lands on an unchanged screen, so a checkpoint there would be pure waste. The
    // clean output compresses to almost nothing, so the recording stays far under
    // the cap (no truncation skews the checkpoint count).
    let out = Command::new(GHOST)
        .env("XDG_RUNTIME_DIR", run.path())
        .env("XDG_DATA_HOME", data.path())
        .args([
            "new",
            name,
            "-d",
            "--max-recording-size",
            "1048576",
            "--",
            "sh",
            "-c",
            "awk 'BEGIN{for(i=0;i<150000;i++)printf \"\\033[m\"}'; echo done-idle-chk; kill -TERM $$",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Poll the recording itself until it reconstructs to the sentinel — proving
    // it is fully flushed — rather than racing a delisting against the final
    // write (the file is written concurrently as the session runs).
    let path = recording_path(data.path(), name);
    assert!(
        wait_for_recording_marker(&path, "done-idle-chk"),
        "recording did not reconstruct to the sentinel at {}",
        path.display()
    );
    // Without the idle guard this would be several checkpoints, one per interval
    // boundary crossed by the flood.
    let rec = record::read(&path).unwrap();
    assert!(
        rec.checkpoint_count() <= 1,
        "a non-rendering flood should not write checkpoints, got {}",
        rec.checkpoint_count()
    );
}

#[test]
fn recording_size_is_bounded() {
    let run = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let name = "rec-bounded";

    // Emit 2 MB of incompressible output with a 256 KiB cap. Unbounded, the
    // recording would be ~2 MB; bounded, old history is dropped at checkpoints.
    // A terminal reset (RIS) then a sentinel give a clean final screen to poll
    // for, so we wait on the flushed recording rather than racing a delisting.
    let out = Command::new(GHOST)
        .env("XDG_RUNTIME_DIR", run.path())
        .env("XDG_DATA_HOME", data.path())
        .args([
            "new",
            name,
            "-d",
            "--max-recording-size",
            "262144",
            "--",
            "sh",
            "-c",
            "head -c 2000000 /dev/urandom; printf '\\033c'; echo BOUNDED-DONE; kill -TERM $$",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let path = recording_path(data.path(), name);
    assert!(
        wait_for_recording_marker(&path, "BOUNDED-DONE"),
        "bounded recording did not finalize at {}",
        path.display()
    );
    let len = std::fs::metadata(&path).unwrap().len();
    assert!(
        len <= 1_000_000,
        "recording not bounded: {len} bytes for a 256 KiB cap"
    );
    // It is still a valid recording with at least one checkpoint after the
    // compaction rewrites.
    let rec = record::read(&path).unwrap();
    assert!(
        rec.checkpoint_count() >= 1,
        "bounded recording lost its checkpoints"
    );
}

#[test]
fn export_produces_valid_asciicast() {
    let run = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let name = "rec-export";

    let out = Command::new(GHOST)
        .env("XDG_RUNTIME_DIR", run.path())
        .env("XDG_DATA_HOME", data.path())
        .args([
            "new",
            name,
            "-d",
            "--",
            "sh",
            "-c",
            "printf 'HELLO-CAST\\n'; kill -TERM $$",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // Wait until the recording is flushed (its marker reconstructs) so the
    // export below reads a complete file.
    let path = recording_path(data.path(), name);
    assert!(
        wait_for_recording_marker(&path, "HELLO-CAST"),
        "recording did not finalize at {}",
        path.display()
    );

    let out = Command::new(GHOST)
        .env("XDG_RUNTIME_DIR", run.path())
        .env("XDG_DATA_HOME", data.path())
        .args(["export", name])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost export` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let cast = String::from_utf8(out.stdout).unwrap();
    let mut lines = cast.lines();

    // First line is a valid asciicast v2 header.
    let header: serde_json::Value = serde_json::from_str(lines.next().expect("header")).unwrap();
    assert_eq!(header["version"], 2);
    assert_eq!(header["width"], 80);
    assert_eq!(header["height"], 24);

    // Every remaining line is a valid [time, type, data] event, and the output
    // is present.
    let mut saw_marker = false;
    for line in lines {
        let ev: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(ev[0].is_number(), "event time not a number: {line}");
        assert!(ev[1].is_string(), "event type not a string: {line}");
        if ev[1] == "o" && ev[2].as_str().is_some_and(|s| s.contains("HELLO-CAST")) {
            saw_marker = true;
        }
    }
    assert!(
        saw_marker,
        "exported asciicast missing the output; got:\n{cast}"
    );
}
