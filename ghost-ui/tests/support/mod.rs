//! Shared fixtures for the shell's end-to-end tests.
//!
//! Two kinds of test share this: `shell.rs`, which drives the real [`App`] over a
//! [`HeadlessFrontend`] (everything real but winit and the GPU), and the
//! real-`sshd` remotes in `remote.rs`. Both are integration tests, so they can
//! reach `CARGO_BIN_EXE_ghost` and stand up genuine session hosts — that is what
//! the lib/bin split bought.

#![allow(dead_code)] // each test binary uses a different slice of this

pub mod remote;

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The `ghost` binary under test.
pub const GHOST: &str = env!("CARGO_BIN_EXE_ghost");

/// Run `f` with `$XDG_*` redirected into a throwaway dir, serialized against every
/// other shell test — the App reads its dirs from the process-global environment,
/// so isolation has to be process-wide. `f` gets the temp root.
///
/// The tempdir is kept alive for the whole call, so nothing the shell wrote (a
/// session's runtime dir, `groups.toml`, a recording) can leak into the
/// developer's real ghost state.
pub fn with_isolated_xdg<T>(f: impl FnOnce(&Path) -> T) -> T {
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    // SAFETY: single-threaded within the lock; every test that reads these vars
    // holds it too.
    unsafe {
        std::env::set_var("XDG_RUNTIME_DIR", tmp.path());
        std::env::set_var("XDG_DATA_HOME", tmp.path().join("data"));
        std::env::set_var("XDG_CONFIG_HOME", tmp.path().join("config"));
    }
    f(tmp.path())
}

/// Read-until-predicate with a timeout — the suite's only sync primitive (never a
/// fixed sleep). Returns whether `pred` came true.
pub fn wait_until(timeout: Duration, mut pred: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if pred() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Spawn a real, detached session host named `name` by running the `ghost` binary
/// — not `server::spawn` in-process: a host is a re-exec of the running
/// executable, and this test binary is not a ghost host. The child inherits our
/// (isolated) environment, so the session lands in the test's runtime dir.
pub fn spawn_session(name: &str) {
    let ok = std::process::Command::new(GHOST)
        .args(["new", name, "-d", "--", "cat"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    assert!(ok, "`ghost new {name} -d` failed");
    assert!(
        wait_until(Duration::from_secs(10), || {
            ghost_vt::session::list()
                .unwrap_or_default()
                .iter()
                .any(|i| i.name == name)
        }),
        "session {name} never appeared in the listing"
    );
}

/// Every session host this test spawned, killed on drop — a leaked `ghost __host`
/// holds an inotify instance, and the suite exhausts the per-user cap fast (see
/// the E2E inotify note in the project memory).
pub fn kill_session(name: &str) {
    let _ = std::process::Command::new(GHOST)
        .args(["kill", name])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Leave `name` looking like a session **attached in another window**: a live host
/// with a display client held by someone this `App` does not own. The returned
/// client must stay alive for the session to keep reading as attached.
pub fn attach_elsewhere(name: &str) -> ghost_vt::client::Session {
    let mut s = ghost_vt::client::Session::attach(name, 80, 24)
        .expect("attach a client from 'another window'");
    s.hello("ghost-ui:other")
        .expect("identify as another window");
    assert!(
        wait_until(Duration::from_secs(5), || {
            ghost_vt::session::list()
                .unwrap_or_default()
                .iter()
                .any(|i| i.name == name && i.attached)
        }),
        "{name} never came up as attached"
    );
    s
}

/// **Assert on what the user sees, never on what the model holds.** A `Scene` is
/// what the renderer draws: laid out, positioned, and post-cull — a tile the fleet
/// remembers but never places, or places below the fold, is absent here exactly as
/// it is absent from the screen. Everything below reads a `Scene` only.
///
/// Every line of text the window draws, in scene order.
pub fn visible_text(scene: &ghost_render::Scene) -> Vec<String> {
    scene
        .layers
        .iter()
        .flat_map(|l| &l.items)
        .filter_map(|item| match item {
            ghost_render::SceneItem::Text { runs, .. } => {
                Some(runs.iter().map(|r| r.text.as_str()).collect::<String>())
            }
            _ => None,
        })
        .collect()
}

/// Whether the window draws `needle` anywhere (chrome: a band, a chip, a label).
pub fn sees_text(scene: &ghost_render::Scene, needle: &str) -> bool {
    visible_text(scene).iter().any(|t| t.contains(needle))
}

/// Whether the user can see a card for session `name`: its label is drawn (a dead
/// or cold tile is a card with a title and no live preview), or its live preview is
/// (a `Terminal` keyed to that session). Culled and never-placed tiles are absent
/// from both, which is the point.
pub fn sees_tile(scene: &ghost_render::Scene, name: &str) -> bool {
    let key = ghost_render::scene::session_key(name);
    let live = scene.layers.iter().flat_map(|l| &l.items).any(|item| {
        matches!(
            item,
            ghost_render::SceneItem::Terminal { session, .. } if *session == key
        )
    });
    // Card titles are `name · cwd · state`, ellipsized to the card width, so match
    // on the leading name rather than the whole label.
    live || scene
        .layers
        .iter()
        .flat_map(|l| &l.items)
        .any(|item| match item {
            ghost_render::SceneItem::Text { id, runs, .. } => {
                matches!(id, ghost_render::SceneId::Label(_))
                    && runs
                        .iter()
                        .map(|r| r.text.as_str())
                        .collect::<String>()
                        .starts_with(name)
            }
            _ => false,
        })
}

/// Which of `candidates` the user can currently see a card for.
pub fn seen_tiles(scene: &ghost_render::Scene, candidates: &[&str]) -> Vec<String> {
    candidates
        .iter()
        .filter(|c| sees_tile(scene, c))
        .map(|c| (*c).to_string())
        .collect()
}

/// Leave behind exactly what a window that ran days ago leaves: a group in
/// `groups.toml` remembering a member that is no longer running, with the
/// descriptor and recording that make it resurrectable. This is the state that
/// accumulates in a real `~/.local/share/ghost` and the reason a new window
/// stopped spawning a session.
pub fn remember_dead_member(group_id: &str, name: &str) {
    ghost_vt::descriptor::write(
        name,
        &ghost_vt::descriptor::Descriptor {
            command: Vec::new(),
            cwd: Some(PathBuf::from("/tmp")),
            created_at: 0,
            display_name: String::new(),
            connection: None,
            policy: Default::default(),
        },
    )
    .expect("write descriptor");
    // A recording, so the dead tile has a last screen to show (as a real one would).
    let rec = ghost_vt::paths::recording_path(name);
    let mut w =
        ghost_vt::record::FileRecorder::create(&rec, 80, 24, &[], None).expect("open recording");
    w.output(b"$ echo remembered\r\nremembered\r\n")
        .expect("record");
    drop(w);

    let dir = ghost_vt::paths::data_dir();
    std::fs::create_dir_all(&dir).expect("data dir");
    let file = dir.join("groups.toml");
    let mut text = std::fs::read_to_string(&file).unwrap_or_default();
    text.push_str(&format!(
        "[[group]]\nid = \"{group_id}\"\nname = \"blue\"\ncolor = 0\nmembers = [\"{name}\"]\n\n"
    ));
    std::fs::write(&file, text).expect("write groups.toml");
}
