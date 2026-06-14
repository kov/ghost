//! End-to-end tests for session recording: the host writes a durable,
//! framed-zstd recording of session output that can be decoded back.

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

#[test]
fn session_output_is_recorded() {
    let run = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let name = "rec-basic";

    // A short-lived session that prints a marker and exits, so the host flushes
    // and closes the recording on its own.
    let out = Command::new(GHOST)
        .env("XDG_RUNTIME_DIR", run.path())
        .env("XDG_DATA_HOME", data.path())
        .args(["new", name, "--", "sh", "-c", "echo RECORDED-MARKER"])
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
        .args(["new", name, "--no-record", "--", "sh", "-c", "echo NOPE"])
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

    // Emit far more than the host's checkpoint interval, then a sentinel, then
    // exit. The sentinel lets us know the whole recording has been flushed
    // before we inspect it (the file is written concurrently as the session
    // runs, and a mid-write read would be incomplete).
    let out = Command::new(GHOST)
        .env("XDG_RUNTIME_DIR", run.path())
        .env("XDG_DATA_HOME", data.path())
        .args(["new", name, "--", "sh", "-c", "seq 1 60000; echo DONE-CHK"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "`ghost new` failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let path = recording_path(data.path(), name);
    // Wait until the recording is complete (sentinel present) and reconstructs
    // to the true final screen, with at least one mid-session checkpoint.
    assert!(
        wait_until(Duration::from_secs(10), || {
            let Ok(rec) = record::read(&path) else {
                return false;
            };
            if rec.checkpoint_count() < 1 {
                return false;
            }
            let screen = ghost_vt::screen::Screen::from_recording(&rec, 1000);
            screen.text().iter().any(|l| l.contains("DONE-CHK"))
        }),
        "recording lacked a checkpoint or did not reconstruct to completion at {}",
        path.display()
    );
}
