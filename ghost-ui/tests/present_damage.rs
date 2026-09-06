//! What a ghost window tells the compositor it changed.
//!
//! `vkQueuePresentKHR` with no `VkPresentRegionsKHR` makes Mesa's Wayland WSI
//! post `wl_surface.damage_buffer(0, 0, INT32_MAX, INT32_MAX)` on every commit.
//! That is the whole surface, for a frame whose only change is one glyph — and
//! on the compositor a translucent ghost window is composited twice, itself plus
//! the `ext-background-effect-v1` backdrop behind it, neither able to declare an
//! opaque region. A spinner tick repaints the window, its backdrop, and the
//! wallpaper under both.
//!
//! So the assertion is the one the report that opened this ends on: run a
//! one-cell spinner and watch the protocol. Every rect full-surface is the bug;
//! partial rects appearing there is the fix.
//!
//! It reads ghost's own `WAYLAND_DEBUG=1` stderr rather than asking the
//! compositor, because that is where the WSI's requests are legible and it is
//! exactly how the bug was found. It brings its own headless [`Synoik`] so it
//! runs under plain `cargo test`; presenting needs a real GPU (see
//! `ghost-test-compositor`), and no GPU is a skip, not a failure.
#![cfg(target_os = "linux")]

use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

mod support;

use ghost_test_compositor::{Synoik, spawn_dying_with_us, wait_until};
use support::GHOST;

/// The virtual output's size, in device pixels.
const MONITOR: (u32, u32) = (1600, 1000);

/// A "shell" that rewrites column 0 of one row, forever, about ten times a second
/// — the spinner the report measured, with nothing else on screen moving.
///
/// It arrives as `$SHELL`, which is what a window runs when it opens. Typing it
/// into the window instead would need key injection, which headless synoik does
/// not have; `ghost new` is no use either, because that attaches over the
/// terminal the CLI was run from, not in a window.
const SPINNER: &str =
    "#!/bin/sh\nwhile :; do printf '\\rX'; sleep 0.1; printf '\\ro'; sleep 0.1; done\n";

/// One `wl_surface.damage_buffer` request, as the protocol carried it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rect {
    x: i64,
    y: i64,
    w: i64,
    h: i64,
}

impl Rect {
    /// Whether this rect claims everything. The WSI's no-regions default is
    /// literally `INT32_MAX`, but a rect merely covering the whole surface is
    /// the same cost to the compositor, so anything output-sized counts too.
    fn is_full(&self) -> bool {
        self.x <= 0
            && self.y <= 0
            && self.w >= i64::from(MONITOR.0)
            && self.h >= i64::from(MONITOR.1)
    }
}

/// Every `damage_buffer` request in a `WAYLAND_DEBUG=1` log, in order.
///
/// The lines look like
/// `{mesa vk display queue}  -> wl_surface#29.damage_buffer(0, 0, 2147483647, 2147483647)`.
fn damage_rects(log: &str) -> Vec<Rect> {
    log.lines()
        .filter_map(|line| {
            let (_, rest) = line.split_once(".damage_buffer(")?;
            let (args, _) = rest.split_once(')')?;
            let n: Vec<i64> = args
                .split(',')
                .map(|a| a.trim().parse().ok())
                .collect::<Option<_>>()?;
            let [x, y, w, h] = n[..] else { return None };
            Some(Rect { x, y, w, h })
        })
        .collect()
}

/// A running `ghost`, and the dirs and protocol log it was given.
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
    /// Launch the real binary against `compositor`, showing a window whose only
    /// moving pixel is the spinner, with the Wayland protocol traced to a file.
    fn start(compositor: &Synoik) -> Ghost {
        let dir = tempfile::tempdir().expect("a state dir");
        let mut g = Ghost {
            child: None,
            log: dir.path().join("wayland.log"),
            display: compositor.display(),
            dir,
        };
        let shell = g.dir.path().join("spin.sh");
        std::fs::write(&shell, SPINNER).expect("the spinner shell");
        std::fs::set_permissions(&shell, PermissionsExt::from_mode(0o755)).expect("executable");
        let out = std::fs::File::create(&g.log).expect("log file");
        g.child = Some(
            spawn_dying_with_us(
                g.cli()
                    .arg("--fresh")
                    .env("SHELL", &shell)
                    // The WSI's own requests, which is what this test reads.
                    .env("WAYLAND_DEBUG", "1")
                    .stdout(Stdio::null())
                    .stderr(Stdio::from(out)),
            )
            .expect("ghost starts"),
        );
        g
    }

    /// A `ghost` command in the same dirs and on the same display.
    fn cli(&self) -> Command {
        let mut cmd = Command::new(GHOST);
        cmd.env("WAYLAND_DISPLAY", &self.display)
            .env("XDG_RUNTIME_DIR", self.dir.path())
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .env("XDG_DATA_HOME", self.dir.path().join("data"))
            .stdin(Stdio::null());
        cmd
    }

    fn log(&self) -> String {
        let mut s = String::new();
        if let Ok(mut f) = std::fs::File::open(&self.log) {
            let _ = f.read_to_string(&mut s);
        }
        s
    }

    fn rects(&self) -> Vec<Rect> {
        damage_rects(&self.log())
    }
}

/// A window whose only change is one cell must not tell the compositor the whole
/// surface changed.
#[test]
fn a_one_cell_change_damages_less_than_the_whole_surface() {
    // A compositor able to hand us a presentable surface is the one thing this
    // test cannot supply itself.
    let compositor = match Synoik::start(MONITOR, 1.0) {
        Ok(c) => c,
        Err(why) => {
            eprintln!("skipping: {why}");
            return;
        }
    };

    let ghost = Ghost::start(&compositor);
    // Opening a window is several full-surface presents on any implementation —
    // first paint, configure, the swapchain's own images. Wait past them for the
    // steady spinner, which is the only thing still moving.
    if !wait_until(Duration::from_secs(30), || ghost.rects().len() >= 30) {
        eprintln!(
            "skipping: no presents on this compositor/GPU pair ({} damage requests)",
            ghost.rects().len()
        );
        return;
    }
    // Let the spinner run well past the opening burst.
    let opening = ghost.rects().len();
    assert!(
        wait_until(Duration::from_secs(30), || ghost.rects().len()
            >= opening + 40),
        "the spinner keeps presenting"
    );

    let rects = ghost.rects();
    let steady = &rects[opening..];
    let partial = steady.iter().filter(|r| !r.is_full()).count();
    eprintln!(
        "present damage: {} requests, {} of them partial in the steady {} \
         (first steady rect {:?})",
        rects.len(),
        partial,
        steady.len(),
        steady.first(),
    );
    assert!(
        partial >= steady.len() / 2,
        "a spinner tick should damage a band, not the whole surface: \
         {partial} of {} steady requests were partial, e.g. {:?}",
        steady.len(),
        &steady[..steady.len().min(4)],
    );
}
