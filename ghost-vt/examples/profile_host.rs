//! Profiling harness for the session host's per-chunk output pipeline.
//!
//! Replays a corpus file through the exact work the host does on every PTY read
//! (see `server.rs`): feed the authoritative `Screen`, append to the recorder,
//! and write a checkpoint every `CHECKPOINT_INTERVAL_BYTES`. Bytes are fed in
//! `PTY_CHUNK`-sized slices to match the host's `read()` granularity.
//!
//! Usage:
//!   cargo build --profile profiling --example profile_host
//!   ./target/profiling/examples/profile_host <corpus> <mode> [reps]
//!
//! Modes isolate stages so cost can be attributed:
//!   screen      feed the Screen only
//!   record      feed Screen + append to recorder (no checkpoints)
//!   all         feed Screen + recorder + periodic checkpoints (the real host)
//!   checkpoint  like `all` but reports checkpoint count / dump bytes

use ghost_vt::record::FileRecorder;
use ghost_vt::screen::Screen;
use std::time::Instant;

const PTY_CHUNK: usize = 8192;
const MAX_RECORDING_BYTES: usize = 64 * 1024 * 1024;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let corpus_path = args
        .get(1)
        .map(String::as_str)
        .unwrap_or("/tmp/ghost_corpus.txt");
    let mode = args.get(2).map(String::as_str).unwrap_or("all");
    let reps: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(1);

    let corpus = std::fs::read(corpus_path).expect("read corpus");
    let total_bytes = corpus.len() * reps;

    let do_record = matches!(mode, "record" | "all" | "checkpoint");
    let do_checkpoint = matches!(mode, "all" | "checkpoint");
    // Checkpoint interval in KiB. Default mirrors the host's choice for the
    // default 64 MiB cap (see `server::checkpoint_interval`); override to sweep.
    let checkpoint_interval_bytes: usize = std::env::var("GHOST_CKPT_KB")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(2048)
        * 1024;

    let tmp = std::env::temp_dir().join("ghost_profile.ghostrec");
    let _ = std::fs::remove_file(&tmp);

    let mut screen = Screen::new(80, 24, 1000);
    let mut recorder = if do_record {
        Some(FileRecorder::create(&tmp, 80, 24, &[], Some(MAX_RECORDING_BYTES)).unwrap())
    } else {
        None
    };

    let mut bytes_since_checkpoint = 0usize;
    let mut checkpoints = 0usize;
    let mut dump_bytes_total = 0usize;

    let start = Instant::now();
    for _ in 0..reps {
        for chunk in corpus.chunks(PTY_CHUNK) {
            screen.feed(chunk);
            if let Some(r) = &mut recorder {
                let _ = r.output(chunk);
                if do_checkpoint {
                    bytes_since_checkpoint += chunk.len();
                    if bytes_since_checkpoint >= checkpoint_interval_bytes {
                        let (c, rws) = screen.dimensions();
                        let dump = screen.dump();
                        dump_bytes_total += dump.len();
                        let _ = r.checkpoint(c, rws, &dump);
                        checkpoints += 1;
                        bytes_since_checkpoint = 0;
                    }
                }
            }
        }
    }
    let elapsed = start.elapsed();

    let _ = std::fs::remove_file(&tmp);

    let mb = total_bytes as f64 / (1024.0 * 1024.0);
    let secs = elapsed.as_secs_f64();
    eprintln!(
        "mode={mode} bytes={total_bytes} ({mb:.1} MiB) elapsed={secs:.3}s throughput={:.1} MiB/s",
        mb / secs
    );
    if do_checkpoint {
        eprintln!(
            "  checkpoints={checkpoints} dump_bytes_total={dump_bytes_total} \
             (avg {:.0} B/dump)",
            dump_bytes_total as f64 / checkpoints.max(1) as f64
        );
    }
}
