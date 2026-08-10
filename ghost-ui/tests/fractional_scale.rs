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
//! It brings its own compositor, like `ghost-ui-harness/tests/windowed.rs` does —
//! but **mutter**, not weston: weston 15 implements no `wp_fractional_scale_v1`
//! and its `--scale` is an integer, so it cannot put a client at 1.25 at all.
//! Mutter can, headless, over a private session bus: a virtual monitor, then
//! `ApplyMonitorsConfig` at the scale we want. Both children are reaped on every
//! way out (`Drop`, plus a kernel death-signal for the ways `Drop` cannot see).
//!
//! The assertion is not "the process survived". `Graphics::render` now drops a
//! scene whose size disagrees with its surface rather than draw it, so a
//! regression would survive and merely go blank — the discriminating signals are
//! that **no** frame was dropped and that every frame measured itself as
//! `surface - geometry`.
//!
//! Linux-and-mutter only: without mutter (or `gdbus`, or `dbus-daemon`) the test
//! says so and returns, because a fractional compositor is the one thing it
//! cannot supply itself.
#![cfg(target_os = "linux")]

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

mod support;

use support::{GHOST, wait_until};

/// The virtual monitor's size, in device pixels.
const MONITOR: (u32, u32) = (1600, 1000);

/// The Wayland socket mutter is asked to serve on, inside its own runtime dir.
const SOCKET: &str = "ghost-fractional";

/// A headless mutter and the private session bus it needs, alive as long as this
/// value is.
///
/// Both are children, so both have to be reaped on every way out of the test. A
/// panic unwinds, which `Drop` covers; an abort or a `^C` on cargo does not, so
/// each child is *also* told to die with us by the kernel. Nothing here looks a
/// process up by name — that would match every mutter this user is running,
/// including the one drawing their desktop.
struct Compositor {
    mutter: Option<Child>,
    bus: Child,
    /// Holds the runtime dir, the bus socket and the settings keyfile.
    dir: tempfile::TempDir,
}

impl Drop for Compositor {
    fn drop(&mut self) {
        for child in self.mutter.iter_mut().chain(std::iter::once(&mut self.bus)) {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Compositor {
    /// Start a headless mutter with one virtual monitor, on a socket and a bus of
    /// its own. `None` when the tools are not installed, or refuse to come up.
    fn start() -> Option<Compositor> {
        for tool in ["mutter", "dbus-daemon", "gdbus"] {
            let missing = Command::new(tool)
                .arg("--version")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_err();
            if missing {
                eprintln!("skipping: {tool} is not installed");
                return None;
            }
        }
        let dir = tempfile::tempdir().expect("a runtime dir");
        // Mutter needs no settings from us: fractional monitor scales stopped
        // being an experimental feature (`scale-monitor-framebuffer`, which
        // mutter 50 no longer knows) and a virtual monitor offers 1.25 outright.
        // It does need a config dir it can write, and it must not be the user's.
        let cfg = dir.path().join("config");
        std::fs::create_dir_all(&cfg).expect("config dir");

        let bus_path = dir.path().join("bus");
        let bus_addr = format!("unix:path={}", bus_path.display());
        let bus = spawn_dying_with_us(
            Command::new("dbus-daemon")
                .args(["--session", "--nofork", "--nosyslog"])
                .arg(format!("--address={bus_addr}")),
        )?;
        // Held from here on, so every early return below still reaps the bus.
        let mut c = Compositor {
            mutter: None,
            bus,
            dir,
        };
        if !wait_until(Duration::from_secs(10), || bus_path.exists()) {
            eprintln!("skipping: the private session bus never came up");
            return None;
        }

        c.mutter = Some(spawn_dying_with_us(
            Command::new("mutter")
                .args(["--headless", "--virtual-monitor"])
                .arg(format!("{}x{}", MONITOR.0, MONITOR.1))
                .arg(format!("--wayland-display={SOCKET}"))
                .env("XDG_CONFIG_HOME", &cfg)
                .env("XDG_RUNTIME_DIR", c.dir.path())
                .env("DBUS_SESSION_BUS_ADDRESS", &bus_addr),
        )?);
        if !wait_until(Duration::from_secs(20), || c.display().exists()) {
            eprintln!("skipping: mutter never came up headless");
            return None;
        }
        Some(c)
    }

    /// The Wayland socket, as an absolute path — which is how a client is pointed
    /// at it without also inheriting mutter's runtime dir (it needs its own, for
    /// its sessions).
    fn display(&self) -> PathBuf {
        self.dir.path().join(SOCKET)
    }

    fn bus_address(&self) -> String {
        format!("unix:path={}", self.dir.path().join("bus").display())
    }

    /// Ask mutter to put the monitor at `scale`. Returns whether it took — a scale
    /// this mutter will not offer is a skip, not a failure.
    fn set_scale(&self, scale: f64) -> bool {
        // Mutter takes the display-config name on the bus a moment after it is
        // serving Wayland, so the first read is a wait, not a question.
        let mut state = None;
        wait_until(Duration::from_secs(20), || {
            state = self.display_config("GetCurrentState", &[]);
            state.is_some()
        });
        let Some(state) = state else {
            return false;
        };
        // `(uint32 <serial>, [...` — the serial is mutter's guard against
        // configuring a monitor layout the caller has not seen.
        let serial = state
            .strip_prefix("(uint32 ")
            .and_then(|s| s.split(',').next())
            .and_then(|s| s.trim().parse::<u32>().ok());
        // The mode is quoted in that same reply, as `1600x1000@60.000`.
        let mode = state
            .split('\'')
            .find(|s| s.starts_with(&format!("{}x{}@", MONITOR.0, MONITOR.1)))
            .map(str::to_owned);
        let (Some(serial), Some(mode)) = (serial, mode) else {
            eprintln!("could not read mutter's monitor state: {state}");
            return false;
        };
        self.display_config(
            "ApplyMonitorsConfig",
            &[
                &serial.to_string(),
                // 1 = apply now, without writing it to any monitor config file.
                "1",
                &format!(
                    "[(0, 0, {scale}, uint32 0, true, [('Meta-0', '{mode}', @a{{sv}} {{}})])]"
                ),
                "@a{sv} {}",
            ],
        )
        .is_some()
    }

    fn display_config(&self, method: &str, args: &[&str]) -> Option<String> {
        let out = Command::new("gdbus")
            .args([
                "call",
                "--session",
                "--dest",
                "org.gnome.Mutter.DisplayConfig",
                "--object-path",
                "/org/gnome/Mutter/DisplayConfig",
                "--method",
            ])
            .arg(format!("org.gnome.Mutter.DisplayConfig.{method}"))
            .args(args)
            .env("DBUS_SESSION_BUS_ADDRESS", self.bus_address())
            .output()
            .ok()?;
        // Quiet on failure: `set_scale` polls this while mutter is still taking
        // its name on the bus, and says so itself if it never arrives.
        if !out.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }
}

/// Spawn a child the kernel kills when this test process dies, however it dies —
/// the backstop for every exit `Drop` cannot see. Set between fork and exec,
/// because it is a property of the child.
fn spawn_dying_with_us(cmd: &mut Command) -> Option<Child> {
    use std::os::unix::process::CommandExt;
    // SAFETY: `set_parent_process_death_signal` is a single syscall, which is all
    // that may run between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            rustix::process::set_parent_process_death_signal(Some(rustix::process::Signal::TERM))
                .map_err(std::io::Error::from)
        });
    }
    cmd.spawn().ok()
}

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
    fn start(compositor: &Compositor) -> Ghost {
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
    let Some(compositor) = Compositor::start() else {
        return;
    };
    if !compositor.set_scale(1.25) {
        eprintln!("skipping: mutter would not scale the monitor by 1.25");
        return;
    }

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
    if compositor.set_scale(4.0 / 3.0) {
        assert!(
            wait_until(Duration::from_secs(20), || {
                ghost.measurements().iter().any(|m| m.scale > 1.3)
            }),
            "the windows re-measure themselves at the new scale"
        );
    }

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
