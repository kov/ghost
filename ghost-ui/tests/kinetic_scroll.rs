//! Trackpad kinetic scrolling through a real compositor: a flick keeps the
//! view coasting after the fingers lift, and fingers coming to rest stop it.
//!
//! On Linux the coasting is ghost's own — libinput stops dead at the lift — so
//! the whole chain is ours to get right: `wl_pointer` finger-source axis
//! events into winit, `axis_stop` as the lift, the flick's speed measured from
//! when the events arrived, the glide on the window's clock, and
//! `zwp_pointer_gesture_hold_v1` (bound by our vendored winit) to catch it.
//! The unit tests replay recorded gestures through the core; this drives the
//! real binary with input injected into a headless [`Synoik`], and watches what
//! the window draws (`ghost::view`: its top row at each present), as the user
//! would. Presenting needs a real GPU; without one this is a skip.
#![cfg(target_os = "linux")]

use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

mod support;

use ghost_test_compositor::{Synoik, spawn_dying_with_us, wait_until};
use support::GHOST;

/// The virtual output's size, in device pixels.
const MONITOR: (u32, u32) = (1600, 1000);

/// A "shell" that prints `L0`..`L499`, one a line, and then waits: scrollback
/// with every line telling where it is.
const LINES: &str =
    "#!/bin/sh\ni=0\nwhile [ $i -lt 500 ]; do echo \"L$i\"; i=$((i+1)); done\nexec sleep 100000\n";

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
        // A session host outlives its client by design; end it through the CLI,
        // in the same dirs, before the dirs go.
        let _ = self
            .cli()
            .args(["kill", "--all"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

impl Ghost {
    fn start(compositor: &Synoik) -> Ghost {
        let dir = tempfile::tempdir().expect("a state dir");
        let shell = dir.path().join("lines.sh");
        std::fs::write(&shell, LINES).expect("shell script");
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755))
            .expect("executable");
        let mut g = Ghost {
            child: None,
            log: dir.path().join("view.log"),
            display: compositor.display(),
            dir,
        };
        let out = std::fs::File::create(&g.log).expect("log file");
        g.child = Some(
            spawn_dying_with_us(
                g.cli()
                    .arg("--fresh")
                    .env("SHELL", &shell)
                    .env("RUST_LOG", "ghost::view=debug")
                    .stdout(Stdio::null())
                    .stderr(Stdio::from(out)),
            )
            .expect("ghost starts"),
        );
        g
    }

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

    /// Where the view has been, oldest first: for each present, the line at
    /// its top, in rows. Resting between rows the drawn top line is the
    /// partial one slid in from history; that position reads as half a row
    /// below it (the row height is not in the trace, and no assertion here
    /// needs sub-row precision).
    fn views(&self) -> Vec<f64> {
        self.log()
            .lines()
            .map(plain)
            .filter(|l| l.contains("view presented"))
            .filter_map(|l| {
                let top: f64 = field(&l, "top")?.strip_prefix('L')?.parse().ok()?;
                let frac: f64 = field(&l, "frac")?.parse().ok()?;
                Some(if frac > 0.0 { top + 0.5 } else { top })
            })
            .collect()
    }

    /// The row the view currently has at its top.
    fn top(&self) -> Option<f64> {
        self.views().last().copied()
    }
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

fn field(line: &str, key: &str) -> Option<String> {
    line.split_whitespace().find_map(|tok| {
        let (k, v) = tok.split_once('=')?;
        (k == key).then(|| v.to_owned())
    })
}

/// Inject one input event (`synoik msg input …`).
fn input(compositor: &Synoik, args: &[&str]) {
    let mut all = vec!["input"];
    all.extend_from_slice(args);
    assert!(
        compositor.msg(&all).is_some(),
        "synoik injects `{}`: {}",
        args.join(" "),
        compositor.log()
    );
}

/// Headless synoik with a ghost window scrolled to the end of its output, and
/// the pointer over that window; `None` when there is no GPU to present with.
fn window_with_scrollback() -> Option<(Synoik, Ghost)> {
    let compositor = match Synoik::start(MONITOR, 1.0) {
        Ok(c) => c,
        Err(why) => {
            eprintln!("skipping: {why}");
            return None;
        }
    };
    let ghost = Ghost::start(&compositor);
    assert!(
        wait_until(Duration::from_secs(30), || ghost
            .top()
            .is_some_and(|t| t > 400.0)),
        "the window shows the end of its output: {}",
        ghost.log()
    );
    // The middle of the screen, where a lone window opens. Absolute: a relative
    // slam into the corner would park the pointer on the hot corner, and the
    // overview it opens swallows every scroll.
    input(&compositor, &["pointer-move-to", "800", "500"]);
    Some((compositor, ghost))
}

/// Two-finger flick up (into history): six 20px frames at 60 Hz, then the
/// lift. The fingers alone move the view 120px — under ten rows at any
/// sensible font size.
fn flick(compositor: &Synoik) {
    for _ in 0..6 {
        input(compositor, &["finger-scroll", "0", "-20"]);
        std::thread::sleep(Duration::from_millis(16));
    }
    input(compositor, &["scroll-stop"]);
}

/// Wait until the view has not moved for `quiet`, and return where it rests.
fn settled(ghost: &Ghost, quiet: Duration) -> f64 {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last = ghost.top();
    let mut since = Instant::now();
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
        let now = ghost.top();
        if now != last {
            last = now;
            since = Instant::now();
        } else if since.elapsed() >= quiet {
            break;
        }
    }
    last.expect("the view was drawn")
}

#[test]
fn a_flick_keeps_coasting_after_the_fingers_lift() {
    let Some((compositor, ghost)) = window_with_scrollback() else {
        return;
    };
    let start = ghost.top().unwrap();
    flick(&compositor);
    let rest = settled(&ghost, Duration::from_millis(400));
    let travel = start - rest;
    // What macOS does with this flick, and so what the glide is fitted to:
    // about five times the finger's own travel further (see `kinetic`).
    assert!(
        travel >= 20.0,
        "the view coasts well past the fingers' own travel: moved {travel} rows \
         ({start} -> {rest}); views {:?}",
        ghost.views()
    );
    let views = ghost.views();
    assert!(
        views.windows(2).all(|w| w[1] <= w[0]),
        "and only ever onwards, into history: {views:?}"
    );
}

#[test]
fn resting_fingers_on_the_pad_stop_the_glide() {
    let Some((compositor, ghost)) = window_with_scrollback() else {
        return;
    };
    flick(&compositor);
    // Mid-glide — it runs for about a second — the fingers come down.
    std::thread::sleep(Duration::from_millis(150));
    let before = ghost.top().unwrap();
    input(&compositor, &["hold-begin"]);
    // A frame already in flight may still land.
    std::thread::sleep(Duration::from_millis(100));
    let caught = ghost.top().unwrap();
    std::thread::sleep(Duration::from_millis(1000));
    input(&compositor, &["hold-end"]);
    assert_eq!(
        ghost.top().unwrap(),
        caught,
        "the view stops where the fingers caught it (it was still gliding at \
         {before}); views {:?}",
        ghost.views()
    );
}
