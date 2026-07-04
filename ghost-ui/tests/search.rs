//! End-to-end tests for `ghost search`, driving the real binary against
//! recordings seeded on disk.
//!
//! Recordings are written straight to an isolated `XDG_DATA_HOME` via the
//! `ghost-vt` recorder (deterministic — no dependence on a live host flushing
//! its buffered writer), then the real `ghost search` command is run over them.

use std::path::Path;
use std::process::Command;

fn ghost(data: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_ghost"));
    c.env("XDG_DATA_HOME", data);
    // Isolate session resolution too, so `--session` never sees a real session.
    c.env("XDG_RUNTIME_DIR", data);
    c
}

/// Write a recording for `name` under `data`'s recordings dir from a raw output
/// stream (escape sequences and all). Dropping the recorder flushes it.
fn seed(data: &Path, name: &str, output: &[u8]) {
    let path = data
        .join("ghost")
        .join("recordings")
        .join(format!("{name}.ghostrec"));
    let mut rec =
        ghost_vt::record::FileRecorder::create(&path, 80, 24, &[], None).expect("create recording");
    rec.output(output).expect("record output");
}

#[test]
fn search_greps_rendered_output_across_recordings() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path();

    // The needle straddles SGR escapes in the raw stream; search must match the
    // rendered line, not the escape soup.
    seed(
        data,
        "alpha",
        b"\x1b[32mbuild ok\x1b[0m\r\nWidget compiled\r\n",
    );
    seed(data, "beta", b"nothing to see here\r\n");

    let out = ghost(data).args(["search", "Widget"]).output().unwrap();
    assert!(out.status.success(), "search failed: {out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("alpha:2: Widget compiled"),
        "expected `alpha:2: Widget compiled`, got: {stdout:?}"
    );
    assert!(!stdout.contains("beta"), "beta has no match: {stdout:?}");

    // Case-insensitive.
    let out = ghost(data)
        .args(["search", "-i", "widget"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("alpha:2:"),
        "case-insensitive search should match"
    );

    // No match → empty output, still a success exit.
    let out = ghost(data).args(["search", "zzz-absent"]).output().unwrap();
    assert!(out.status.success());
    assert!(
        out.stdout.is_empty(),
        "no matches → no output: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );

    // `--session` limits the search to one recording.
    seed(data, "gamma", b"Widget elsewhere\r\n");
    let out = ghost(data)
        .args(["search", "--session", "gamma", "Widget"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("gamma:"),
        "expected a gamma hit: {stdout:?}"
    );
    assert!(
        !stdout.contains("alpha"),
        "the search was limited to gamma: {stdout:?}"
    );
}
