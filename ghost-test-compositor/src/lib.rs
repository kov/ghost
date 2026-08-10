//! A **headless synoik**, stood up by a test and reaped by it.
//!
//! ghost's window-level bugs only exist because a compositor said something
//! specific about a surface, so the tests that catch them have to run the real
//! binary against a real compositor. This is the half that supplies one.
//!
//! It is synoik because synoik is the compositor ghost is actually used on, and
//! because it is the only one that does the whole job: weston 15 implements no
//! `wp_fractional_scale_v1` (so it cannot put a client at 1.25 at all) and
//! mutter needs a private session bus, a serial-guarded `ApplyMonitorsConfig`
//! call and `GIO_USE_VFS=local` to stop it activating services nobody can reap.
//! `synoik --headless --output 1600x1000@1.25 --wayland-display NAME` is the
//! whole of it, and `synoik msg` drives the rest. See
//! `docs/synoik-as-a-test-compositor.md` for how that came about.
//!
//! **It needs a real GPU.** Headless synoik advertises `zwp_linux_dmabuf_v1`
//! off the DRM render node its Vulkan device reports; on a driver with no
//! `VK_EXT_physical_device_drm` — lavapipe — it advertises none at all (by
//! design: a compositor offering dmabuf it cannot import hands the client a
//! blank window and no error), the client falls back to shm, and no Vulkan
//! adapter will be compatible with the surface. A test that needs to present
//! should treat that as a skip, not a failure.
//!
//! Linux-only, and a skip when synoik is missing or too old — [`Synoik::start`]
//! says which, because "not installed" and "installed but predates the flags a
//! rig needs" are different things to do something about.
#![cfg(target_os = "linux")]

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The socket name synoik is asked to serve on, inside its own runtime dir.
const SOCKET: &str = "ghost-test";

/// The name synoik gives the first `--output`.
pub const OUTPUT: &str = "headless-1";

/// Where synoik comes from, and which one.
///
/// `synoik.rev` is a full commit hash on purpose: a branch name would make the
/// suite's compositor change under it without a commit anyone can point at.
/// Moving the pin is a one-line change, made deliberately.
const REPO: &str = "https://github.com/kov/synoik.git";
const REV: &str = include_str!("../synoik.rev");

/// Built without default features — `systemd`, `xdp-gnome-screencast` and
/// `audio`, none of which a headless test rig uses, and the last two of which
/// drag in pipewire's development headers. Verified to build and to serve both
/// suites at [`REV`].
const FEATURES: &[&str] = &["--no-default-features"];

/// The synoik this rig runs: `$SYNOIK` if set, else our own build of [`REV`],
/// cloned and built on demand.
///
/// It is deliberately *not* whatever is on `PATH`. The developer's installed
/// synoik is the one drawing their desktop; it moves when they upgrade it, and
/// a test suite whose compositor changes underneath it reports on something
/// nobody chose. `$SYNOIK` stays as the override for working against a build
/// that is ahead of the pin — which is how the pin gets moved.
fn binary() -> Result<PathBuf, Missing> {
    if let Some(own) = std::env::var_os("SYNOIK") {
        return Ok(PathBuf::from(own));
    }
    provision()
}

/// Where our builds live: `target/synoik/<rev>/`, beside the workspace's own
/// build output, so `cargo clean` takes it and nothing else does.
///
/// Derived from the manifest, never from the environment the tests are running
/// in — this suite redirects `XDG_*` process-wide, and a cache path that
/// followed it would land in a tempdir and rebuild synoik on every run.
fn cache() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../target")))
        .join("synoik")
}

/// Clone and build [`REV`], unless we already have it.
///
/// Two test binaries run concurrently under `cargo test --workspace` and both
/// want this, so the whole thing is behind a lock file that outlives any one
/// checkout — the loser waits for the winner's build rather than starting a
/// second one on top of it.
fn provision() -> Result<PathBuf, Missing> {
    let rev = REV.trim();
    let cache = cache();
    let checkout = cache.join(rev);
    let built = checkout.join("target/debug/synoik");
    if built.exists() {
        return Ok(built);
    }
    std::fs::create_dir_all(&cache).map_err(|e| Missing::Build(e.to_string()))?;

    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(cache.join("provision.lock"))
        .map_err(|e| Missing::Build(e.to_string()))?;
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive)
        .map_err(|e| Missing::Build(e.to_string()))?;
    // Whoever held the lock may have been building exactly this.
    if built.exists() {
        return Ok(built);
    }

    let log_path = cache.join(format!("{rev}.log"));
    eprintln!(
        "ghost-test-compositor: building synoik {} — first run only, a couple of \
         minutes; log in {}",
        &rev[..12],
        log_path.display()
    );
    let log = || {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map(Stdio::from)
            .unwrap_or_else(|_| Stdio::null())
    };

    if !checkout.join(".git").is_dir() {
        // A half-finished clone from an interrupted run is worse than none.
        let _ = std::fs::remove_dir_all(&checkout);
        let cloned = Command::new("git")
            .args(["clone", "--filter=blob:none", REPO])
            .arg(&checkout)
            // Never stop for credentials: a `cargo test` that has silently
            // parked on a password prompt looks exactly like a hung test.
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .stdout(log())
            .stderr(log())
            .status();
        if !cloned.is_ok_and(|s| s.success()) {
            return Err(Missing::Clone(log_path.display().to_string()));
        }
    }
    let checked_out = Command::new("git")
        .args(["checkout", "--detach", rev])
        .current_dir(&checkout)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(log())
        .stderr(log())
        .status();
    if !checked_out.is_ok_and(|s| s.success()) {
        return Err(Missing::Clone(log_path.display().to_string()));
    }

    let build = Command::new("cargo")
        .args(["build", "--locked", "--bin", "synoik"])
        .args(FEATURES)
        .current_dir(&checkout)
        // Explicit, because an ambient one would send synoik's build output into
        // ghost's target dir — where the next `cargo build` would fight it.
        .env("CARGO_TARGET_DIR", checkout.join("target"))
        .stdin(Stdio::null())
        .stdout(log())
        .stderr(log())
        .status();
    if !build.is_ok_and(|s| s.success()) {
        return Err(Missing::Build(log_path.display().to_string()));
    }
    Ok(built)
}

/// Why a rig could not be stood up. Every one of these is a skip: the test is
/// not being told anything about ghost.
#[derive(Debug)]
pub enum Missing {
    /// synoik could not be fetched — no network, most likely. Carries the log.
    Clone(String),
    /// It was fetched and would not build. Carries the log.
    Build(String),
    /// It was built and never served.
    NeverCameUp,
}

impl std::fmt::Display for Missing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Missing::Clone(log) => write!(f, "could not clone {REPO} — see {log}"),
            Missing::Build(log) => write!(f, "synoik would not build — see {log}"),
            Missing::NeverCameUp => write!(f, "synoik never came up headless"),
        }
    }
}

/// A running headless synoik, alive for as long as this value is.
///
/// It is a child process, so it has to be reaped on every way out of a test, and
/// there are two kinds of way. A panic unwinds, which [`Drop`] covers; an abort,
/// a SIGKILL or a `^C` on cargo itself does not, so synoik is *also* told to die
/// with us by the kernel. Nothing here ever looks a process up by name — that
/// would match the synoik drawing the developer's desktop.
pub struct Synoik {
    child: Option<Child>,
    /// Holds the runtime dir, the config dir and synoik's log.
    dir: tempfile::TempDir,
    /// Its IPC socket, as synoik itself reported it.
    ipc: PathBuf,
}

impl Drop for Synoik {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Synoik {
    /// Start a headless synoik with one output of `size` at `scale`, on a socket
    /// of its own.
    pub fn start(size: (u32, u32), scale: f64) -> Result<Synoik, Missing> {
        let synoik = binary()?;

        let dir = tempfile::tempdir().expect("a runtime dir");
        // Its own config dir, and not the developer's: synoik reads
        // `monitors.xml`, which sits *above* `--output` in the precedence chain
        // and would override the scale this rig asked for with no error anywhere.
        let cfg = dir.path().join("config");
        std::fs::create_dir_all(&cfg).expect("config dir");
        let log = dir.path().join("synoik.log");
        let out = std::fs::File::create(&log).expect("log file");

        let mut rig = Synoik {
            child: None,
            dir,
            ipc: PathBuf::new(),
        };
        rig.child = spawn_dying_with_us(
            Command::new(&synoik)
                .arg("--headless")
                .arg("--output")
                .arg(format!("{}x{}@{scale}", size.0, size.1))
                .arg("--wayland-display")
                .arg(SOCKET)
                .env("XDG_RUNTIME_DIR", rig.dir.path())
                .env("XDG_CONFIG_HOME", &cfg)
                .env("XDG_DATA_HOME", rig.dir.path().join("data"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::from(out)),
        );
        if rig.child.is_none() {
            return Err(Missing::NeverCameUp);
        }

        // Ready when it is there to connect to *and* it has said where its IPC
        // socket is. The socket's name embeds a pid; synoik logs it rather than
        // asking anyone to predict it.
        let display = rig.display();
        let mut ipc = None;
        let served = wait_until(Duration::from_secs(20), || {
            ipc = read(&log)
                .lines()
                .find_map(|l| l.split_once("IPC listening on: "))
                .map(|(_, path)| PathBuf::from(path.trim()));
            display.exists() && ipc.is_some()
        });
        match ipc {
            Some(ipc) if served => {
                rig.ipc = ipc;
                Ok(rig)
            }
            _ => Err(Missing::NeverCameUp),
        }
    }

    /// The Wayland socket, as an absolute path — which is how a client is pointed
    /// at it without also inheriting the compositor's runtime dir (a ghost needs
    /// its own, for its sessions).
    pub fn display(&self) -> PathBuf {
        self.dir.path().join(SOCKET)
    }

    /// Run `synoik msg …` against *this* compositor.
    ///
    /// `SYNOIK_SOCKET` is set explicitly on every call, and that is not
    /// belt-and-braces: a developer's shell often has one pointing at their live
    /// desktop, and an inherited one would aim `msg action maximize` at whatever
    /// window they are actually using.
    pub fn msg(&self, args: &[&str]) -> Option<String> {
        let out = Command::new(binary().ok()?)
            .arg("msg")
            .args(args)
            .env("SYNOIK_SOCKET", &self.ipc)
            .env("XDG_RUNTIME_DIR", self.dir.path())
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    /// Put the output at `scale`, under whatever is already on it.
    pub fn set_scale(&self, scale: f64) -> bool {
        self.msg(&["output", OUTPUT, "scale", &scale.to_string()])
            .is_some()
    }

    /// What synoik logged, for a failure message worth reading.
    pub fn log(&self) -> String {
        read(&self.dir.path().join("synoik.log"))
    }
}

/// Spawn a child the kernel kills when this test process dies, however it dies —
/// the backstop for every exit [`Drop`] cannot see. Set between fork and exec,
/// because it is a property of the child.
pub fn spawn_dying_with_us(cmd: &mut Command) -> Option<Child> {
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

fn read(path: &Path) -> String {
    let mut s = String::new();
    if let Ok(mut f) = std::fs::File::open(path) {
        let _ = f.read_to_string(&mut s);
    }
    s
}

/// Poll `f` until it holds or `timeout` runs out — a compositor is ready when it
/// is ready, and a sleep long enough to cover the slowest machine is a sleep
/// every other machine pays.
pub fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    f()
}
