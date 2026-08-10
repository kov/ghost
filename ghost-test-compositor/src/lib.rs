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

/// The synoik to run: `$SYNOIK` if set, else whatever is on `PATH`.
///
/// The override is for working against a build of synoik that is newer than the
/// installed one — which is how this rig gets tested at all while a capability
/// is still fresh. Nothing committed here may know where anyone's checkout is.
fn binary() -> PathBuf {
    std::env::var_os("SYNOIK")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("synoik"))
}

/// Why a rig could not be stood up. Every one of these is a skip: the test is
/// not being told anything about ghost.
#[derive(Debug)]
pub enum Missing {
    /// No synoik on `PATH` (and no `$SYNOIK`).
    NotInstalled,
    /// A synoik that does not know the flags a test rig needs. `--wayland-display`
    /// is the newest of them, so its absence stands for all of them.
    TooOld,
    /// It was started and never served.
    NeverCameUp,
}

impl std::fmt::Display for Missing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Missing::NotInstalled => write!(f, "synoik is not installed"),
            Missing::TooOld => write!(
                f,
                "the installed synoik predates `--wayland-display`; update it \
                 (or point $SYNOIK at a newer build)"
            ),
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
        let synoik = binary();
        let help = Command::new(&synoik).arg("--help").output();
        let Ok(help) = help else {
            return Err(Missing::NotInstalled);
        };
        let help = String::from_utf8_lossy(&help.stdout);
        if !help.contains("--wayland-display") {
            return Err(Missing::TooOld);
        }

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
            return Err(Missing::NotInstalled);
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
        let out = Command::new(binary())
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
