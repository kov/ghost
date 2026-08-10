//! The window at a **fractional** scale: does the frame we lay out still fit the
//! surface we were given?
//!
//! At 1.25 the CSD shadow margin (26 logical points a side) is 32.5 px, so the
//! two sides of an axis cannot both be rounded on their own — 33 + 33 claims 66
//! where the compositor grew the surface by `round(52 × 1.25) = 65`. A scene one
//! pixel wider than the swapchain is a scissor-rect validation error, and wgpu
//! calls that fatal from a destructor: the whole process aborts, taking every
//! window with it. That is the bug this test exists to keep dead, and it is
//! invisible at integer scale, where every rounding agrees.
//!
//! It brings its own compositor — a headless [`Synoik`] at the scale we name on
//! its command line, which is also the compositor ghost is really used on. The
//! scale change halfway through is one `synoik msg` call.
//!
//! The assertion is not "the process survived". `Graphics::render` now drops a
//! scene whose size disagrees with its surface rather than draw it, so a
//! regression would survive and merely go blank — the discriminating signals are
//! that **no** frame was dropped and that every frame measured itself as
//! `surface - geometry`.
#![cfg(target_os = "linux")]

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

mod support;

use ghost_test_compositor::{OUTPUT, Synoik, spawn_dying_with_us, wait_until};
use support::GHOST;

/// The virtual output's size, in device pixels.
const MONITOR: (u32, u32) = (1600, 1000);

/// One `frame measured` line: what a window last made of the surface it was given.
#[derive(Debug, PartialEq)]
struct Measured {
    window: String,
    scale: f64,
    surface: (u32, u32),
    geometry: (u32, u32),
    inset: (u32, u32),
}

/// Formatted tracing lines carry colour codes; strip them so fields parse.
fn plain(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        for c in chars.by_ref() {
            if c.is_ascii_alphabetic() {
                break;
            }
        }
    }
    out
}

/// Pull `key=value` out of a formatted tracing line.
fn field(line: &str, key: &str) -> Option<String> {
    line.split_whitespace().find_map(|tok| {
        let (k, v) = tok.split_once('=')?;
        (k == key).then(|| v.to_owned())
    })
}

/// A running `ghost`, and the dirs and log it was given.
struct Ghost {
    child: Option<Child>,
    log: PathBuf,
    display: PathBuf,
    dir: tempfile::TempDir,
}

impl Drop for Ghost {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // A session host outlives its client by design, so killing the UI strands
        // one; end them through the CLI, in the same dirs, before the dirs go.
        let _ = self
            .cli()
            .args(["kill", "--all"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl Ghost {
    /// Launch the real binary against `compositor`, with dirs of its own.
    fn start(compositor: &Synoik) -> Ghost {
        let dir = tempfile::tempdir().expect("a state dir");
        let mut g = Ghost {
            child: None,
            log: dir.path().join("frame.log"),
            display: compositor.display(),
            dir,
        };
        let out = std::fs::File::create(&g.log).expect("log file");
        g.child = Some(
            spawn_dying_with_us(
                g.cli()
                    .arg("--fresh")
                    // The frame's own measurements, which is what this test reads.
                    .env("RUST_LOG", "ghost::frame=debug")
                    .stdout(Stdio::null())
                    .stderr(Stdio::from(out)),
            )
            .expect("ghost starts"),
        );
        g
    }

    /// A `ghost` command in the same dirs and on the same display — the CLI, or
    /// another launch, which a running instance turns into a new window.
    fn cli(&self) -> Command {
        let mut cmd = Command::new(GHOST);
        cmd.env("WAYLAND_DISPLAY", &self.display)
            .env("XDG_RUNTIME_DIR", self.dir.path())
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .env("XDG_DATA_HOME", self.dir.path().join("data"))
            .stdin(Stdio::null());
        cmd
    }

    /// Ask the running instance for another window, the way a second launch does.
    fn open_another_window(&self) {
        let forwarded = self
            .cli()
            .arg("--fresh")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        assert!(forwarded, "a second launch forwards a new-window request");
    }

    fn log(&self) -> String {
        let mut s = String::new();
        if let Ok(mut f) = std::fs::File::open(&self.log) {
            let _ = f.read_to_string(&mut s);
        }
        s
    }

    /// Every measurement the windows have logged so far, oldest first.
    fn measurements(&self) -> Vec<Measured> {
        self.log()
            .lines()
            .map(plain)
            .filter(|l| l.contains("frame measured"))
            .filter_map(|line| {
                let n = |key: &str| -> Option<u32> { field(&line, key)?.parse().ok() };
                Some(Measured {
                    window: field(&line, "window")?,
                    scale: field(&line, "scale")?.parse().ok()?,
                    surface: (n("surface_w")?, n("surface_h")?),
                    geometry: (n("geometry_w")?, n("geometry_h")?),
                    inset: (n("inset_x")?, n("inset_y")?),
                })
            })
            .collect()
    }

    /// How many windows have measured a frame — the windows themselves, not the
    /// measurements, of which each window makes several.
    fn windows_measured(&self) -> usize {
        let mut seen: Vec<String> = self.measurements().into_iter().map(|m| m.window).collect();
        seen.sort();
        seen.dedup();
        seen.len()
    }

    /// Every frame the renderer refused to draw because it was laid out for a size
    /// its surface could not take — the regression signal.
    fn dropped_frames(&self) -> usize {
        self.log()
            .lines()
            .filter(|l| l.contains("dropped a frame"))
            .count()
    }

    fn alive(&mut self) -> bool {
        matches!(self.child.as_mut().map(Child::try_wait), Some(Ok(None)))
    }
}

/// Windows opened at a fractional scale — and windows carried across a change of
/// one — must never lay out a frame the surface they sit in cannot take.
#[test]
fn a_fractional_scale_never_lays_out_a_frame_the_surface_cannot_take() {
    // A fractional compositor is the one thing this test cannot supply itself.
    let compositor = match Synoik::start(MONITOR, 1.25) {
        Ok(c) => c,
        Err(why) => {
            eprintln!("skipping: {why}");
            return;
        }
    };

    let mut ghost = Ghost::start(&compositor);
    assert!(
        wait_until(Duration::from_secs(30), || ghost.windows_measured() >= 1),
        "the first window measures its frame"
    );
    // Two more windows — the ones the crash was reported against. The size a *new*
    // window opens at is worked out separately from the one a resize recomputes,
    // and both of those got this wrong.
    for want in 2..=3 {
        ghost.open_another_window();
        assert!(
            wait_until(Duration::from_secs(20), || ghost.windows_measured() >= want),
            "window {want} measures its frame"
        );
    }
    // Then move the ground under them: another scale resizes every surface, with
    // margins that round differently again (26 × 4/3 = 34.67).
    assert!(
        compositor.set_scale(4.0 / 3.0),
        "synoik takes a scale change on {OUTPUT}: {}",
        compositor.log()
    );
    assert!(
        wait_until(Duration::from_secs(20), || {
            ghost.measurements().iter().any(|m| m.scale > 1.3)
        }),
        "the windows re-measure themselves at the new scale"
    );

    let seen = ghost.measurements();
    let mut scales: Vec<String> = seen.iter().map(|m| m.scale.to_string()).collect();
    scales.sort();
    scales.dedup();
    eprintln!(
        "fractional: {} measurements across {} windows, at scales {}",
        seen.len(),
        ghost.windows_measured(),
        scales.join(", ")
    );
    assert!(
        seen.iter().any(|m| (m.scale - 1.25).abs() < 1e-6),
        "the rig really is fractional — measured at {:?}",
        seen.iter().map(|m| m.scale).collect::<Vec<_>>()
    );
    for m in &seen {
        assert_eq!(
            (
                m.surface.0 - m.geometry.0.min(m.surface.0),
                m.surface.1 - m.geometry.1.min(m.surface.1)
            ),
            m.inset,
            "the shadow's pixels are surface minus geometry, per axis: {m:?}"
        );
    }
    assert_eq!(
        ghost.dropped_frames(),
        0,
        "no frame was laid out for a size its surface could not take"
    );
    assert!(ghost.alive(), "ghost is still running");
}
