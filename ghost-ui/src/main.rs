//! ghost's windowed GPU terminal frontend.
//!
//! A winit window backed by a wgpu surface that is a real ghost client. The
//! shell here is deliberately thin: it owns the I/O (the session socket, the
//! clipboard, the clock, the window) and nothing else. All behavior lives in a
//! pure [`TerminalModel`] (in `ghost-ui-core`): the shell translates each winit
//! event into a [`UiEvent`], runs `model.update` to get a list of [`Cmd`]
//! effects, executes them (socket writes, clipboard, redraw, …), and draws
//! `model.view()`'s `Scene` through `ghost-renderer`. Reads round-trip as data
//! (clipboard: `ReadClipboard` → `ClipboardText`; socket: pump → `SessionData`),
//! so the model never touches the world and stays headlessly testable.
//!
//! Modes:
//! - default: attach to `$GHOST_SESSION`, or spawn a fresh `$SHELL` session, and
//!   run it interactively in a window.
//! - `GHOST_CAPTURE=/path.png`: headless — spawn a session (a fixed banner, or
//!   `$GHOST_CMD`), drive the same model with scripted events, render its
//!   `view()` offscreen, write a PNG, and exit. The model/`Scene` path is the
//!   single source of truth, so this is a binary-level test of the contract.

mod bench;
mod config;
mod font;
mod from_winit;
mod groups;
mod instance;
mod menu;
mod pacer;
mod rendertrace;
mod resize;
mod windows;

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ghost_renderer::{FrameOutcome, Gpu, Rendered, Renderer, SceneCache, SurfaceTarget, Target};
use ghost_ui_core::{
    CellMetrics, Cmd, Key, KeyEventKind, Mods, NamedKey, PointPx, PointerButton, PointerPhase,
    RootModel, Scene, SessionPush, TerminalModel, UiEvent, WindowRecord,
};
use ghost_ui_harness::framestats;
use ghost_vt::client::{Session, Subscriber};
use ghost_vt::connection::ConnectionSpec;
use ghost_vt::screen;
use ghost_vt::server::{self, SpawnOpts};
use ghost_vt::session;
use menu::{ConnectOutcome, MenuIntent, UserEvent};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::ModifiersState;
use winit::window::{Window, WindowId};

/// The resolved font, base size, and cell metrics for this process, read once from
/// `ui.toml`. Resolving leaks the font bytes to `'static` (they live the whole run),
/// so it is memoised here — every window shares this one setup. See [`font::FontSetup`].
fn font_setup() -> &'static font::FontSetup {
    static SETUP: std::sync::OnceLock<font::FontSetup> = std::sync::OnceLock::new();
    SETUP.get_or_init(|| {
        let cfg = config::UiConfig::load();
        font::FontSetup::resolve(cfg.font_family(), cfg.font_size())
    })
}

/// The configured cell metrics (derived from the font at the base size).
fn metrics() -> CellMetrics {
    font_setup().metrics
}

/// The configured base glyph size in px, before zoom/DPI.
fn size_px() -> f32 {
    font_setup().size
}

const COLS: u16 = 80;
const ROWS: u16 = 24;
const POLL: Duration = Duration::from_millis(8);

/// Where a GUI-launched session should start. `server::spawn` captures the
/// process's working directory for the child, but a bundled launch (launchd on
/// macOS via the `.app`, a desktop file on Linux) starts us at `/` — so sessions
/// would open in `/`. In that case (or with no cwd at all) fall back to `home`; a
/// real working directory, e.g. when launched from a terminal, is kept. Returns
/// the directory to switch to, or `None` to leave the cwd as-is.
fn home_launch_dir(cwd: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    match cwd {
        Some(c) if c != Path::new("/") => None,
        _ => home.map(Path::to_path_buf),
    }
}

/// Map the `option_as_meta` preference to winit's macOS Option-key mode: `Both`
/// (both Option keys report as Alt, so the encoder ESC-prefixes them into Meta)
/// when on, `None` (let macOS compose accented characters) when off.
#[cfg(target_os = "macos")]
fn option_as_alt(option_as_meta: bool) -> winit::platform::macos::OptionAsAlt {
    use winit::platform::macos::OptionAsAlt;
    if option_as_meta {
        OptionAsAlt::Both
    } else {
        OptionAsAlt::None
    }
}

/// The index to cycle to among `count` windows from `current` — forward wraps to
/// the next, backward to the previous. `None` when there is nothing to cycle to
/// (fewer than two windows); a missing `current` starts from the first. Ported
/// from the retired ghost-gtk frontend, which drove the same Cmd-` cycling.
fn cycle_index(count: usize, current: Option<usize>, forward: bool) -> Option<usize> {
    if count < 2 {
        return None;
    }
    let idx = current.unwrap_or(0);
    Some(if forward {
        (idx + 1) % count
    } else {
        (idx + count - 1) % count
    })
}

fn main() {
    // MUST be first: re-execs into the session host when invoked as one.
    server::run_host_if_invoked();

    // `ghost <subcommand>` (ls/attach/new/…) is the CLI; it runs and exits. A bare
    // `ghost` has no subcommand and falls through to the windowed UI below, carrying
    // the `--fresh` flag (skip restoring the last-quit windows) into it.
    let fresh = match ghost_cli::run_subcommand() {
        ghost_cli::Launch::Handled => return,
        ghost_cli::Launch::Gui { fresh } => fresh,
    };

    // A bundled launch (Finder/launchd) lands us at `/`; point new GUI sessions at
    // the user's home instead. `server::spawn` reads our cwd when it starts each
    // session's child, so this must run before any session is spawned — and after
    // the CLI early-return above, so `ghost <subcommand>` keeps the shell's cwd.
    if let Some(dir) = home_launch_dir(
        std::env::current_dir().ok().as_deref(),
        std::env::var_os("HOME").map(PathBuf::from).as_deref(),
    ) {
        let _ = std::env::set_current_dir(dir);
    }

    // `GHOST_MENU_DUMP` verifies the native macOS menu bar: install it against a
    // running NSApplication (no window, no session), print its structure, and
    // exit. A native menu can't be click-driven under the test sandbox, so this
    // is how the menu is asserted end-to-end.
    #[cfg(target_os = "macos")]
    if std::env::var_os("GHOST_MENU_DUMP").is_some() {
        menu_dump();
        return;
    }

    if let Some(path) = std::env::var_os("GHOST_CAPTURE") {
        capture(PathBuf::from(path));
    } else {
        interactive(fresh);
    }
}

/// Drive a minimal event loop just far enough to install and print the native
/// macOS menu bar (the `GHOST_MENU_DUMP` probe). Installs against the shared
/// application winit sets up — no window and no session are created.
#[cfg(target_os = "macos")]
fn menu_dump() {
    struct DumpApp {
        proxy: winit::event_loop::EventLoopProxy<UserEvent>,
    }
    impl ApplicationHandler<UserEvent> for DumpApp {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            menu::install(self.proxy.clone());
            menu::dump();
            event_loop.exit();
        }
        fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
    }
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .expect("event loop");
    let proxy = event_loop.create_proxy();
    let _ = event_loop.run_app(&mut DumpApp { proxy });
}

/// Grid cell count for a surface of `w`×`h` physical pixels at `scale` (cells
/// are the base metrics scaled by the device factor, matching the model).
fn grid_from_pixels(w: u32, h: u32, scale: f32, pad: f32) -> (u16, u16) {
    let advance = metrics().advance * scale;
    let line_height = metrics().line_height * scale;
    // The grid fills the surface inset by the padding (logical px, DPI-scaled) on each
    // side; the border is left for the terminal background. Matches `RootModel::grid`.
    let pad_px = pad * scale;
    let cols = ((w as f32 - 2.0 * pad_px) / advance).floor().max(1.0) as u16;
    let rows = ((h as f32 - 2.0 * pad_px) / line_height).floor().max(1.0) as u16;
    (cols, rows)
}

/// Apply the `GHOST_ANIM_MS` override (the duration, in ms, of the UI animations —
/// the fleet dive and the session slide) to a fresh window, if set — for slowing
/// them right down while validating them.
fn apply_anim_ms(root: &mut RootModel) {
    if let Some(ms) = std::env::var("GHOST_ANIM_MS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        root.set_anim_ms(ms);
    }
}

fn map_button(b: MouseButton) -> Option<PointerButton> {
    match b {
        MouseButton::Left => Some(PointerButton::Left),
        MouseButton::Middle => Some(PointerButton::Middle),
        MouseButton::Right => Some(PointerButton::Right),
        _ => None,
    }
}

fn write_png(path: &Path, img: &Rendered) {
    // The renderer outputs premultiplied alpha, but PNG's RGBA is straight, so
    // un-premultiply (divide RGB by alpha). This is identity for opaque pixels
    // (alpha 255), leaving fully-opaque captures byte-for-byte unchanged.
    let mut straight = Vec::with_capacity(img.rgba.len());
    for p in img.rgba.chunks_exact(4) {
        let a = p[3];
        if a == 0 || a == 255 {
            straight.extend_from_slice(p);
        } else {
            let un = |c: u8| (u16::from(c) * 255 / u16::from(a)).min(255) as u8;
            straight.extend_from_slice(&[un(p[0]), un(p[1]), un(p[2]), a]);
        }
    }

    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), img.width, img.height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().expect("png header");
    writer.write_image_data(&straight).expect("png data");
}

/// Attach (deferred) to a named session and complete the handshake at
/// `cols`×`rows` — the first resize promotes us to the display client and
/// spawns the deferred child. The configured theme rides along so the host
/// answers color queries with it after we detach (last-attached colors), and
/// `identity` (the attaching window's, embedding its group id) via `Hello`
/// so other windows' fleets can bucket the session under its block.
fn attach(name: &str, cols: u16, rows: u16, identity: &str) -> io::Result<Session> {
    let mut s = Session::attach_deferred(name)?;
    // Non-blocking, not a 1 ms read timeout: `about_to_wait` pumps every attached
    // session each 8 ms frame, and the socket doesn't wake the winit loop, so a
    // blocking read only ever burns up to 1 ms per idle session per frame with no
    // latency gain. Mirror the observer pool, which is non-blocking for the same
    // reason (a whole pool's idle reads would otherwise add up on the loop).
    s.set_nonblocking(true)?;
    s.resize(cols, rows)?;
    s.report_theme(session_theme())?;
    s.hello(identity)?;
    Ok(s)
}

/// [`attach`] to a *remote* session over the SSH transport: `cmd` is the
/// `ssh … __pipe <name>` tunnel. The handshake is identical — only the transport
/// differs — so the window drives the returned [`Session`] like any local one.
fn attach_over_ssh(
    cmd: std::process::Command,
    name: &str,
    cols: u16,
    rows: u16,
    identity: &str,
) -> io::Result<Session> {
    let mut s = Session::attach_deferred_ssh(cmd, name)?;
    // Non-blocking, like the local [`attach`]: `about_to_wait` pumps this session
    // on the frame loop and the ssh pipe never wakes winit, so a bounded wait buys
    // no latency. (On the ssh transport `set_read_timeout(Some(_))` already maps to
    // non-blocking; spell it the same way as `attach` so the two don't drift.)
    s.set_nonblocking(true)?;
    s.resize(cols, rows)?;
    s.report_theme(session_theme())?;
    s.hello(identity)?;
    Ok(s)
}

/// A remote host reached over the ssh transport, retained so the fleet can poll
/// it. `remote` is shared with the watcher thread; `remote_ghost` is the negotiated
/// remote binary path both the poll and any attach reuse.
#[derive(Clone)]
struct RemoteHost {
    remote: Arc<ghost_vt::remote::RemoteSsh>,
    remote_ghost: String,
}

/// The unit separator (and the `is_remote_id` predicate) are canonical in
/// `ghost_ui_core` now — the fleet reasons about remote membership too — and
/// re-exported here so this module's id helpers read unchanged.
use ghost_ui_core::{REMOTE_ID_SEP, is_remote_id};

/// The fleet id for remote session `real` on `target` — the composite a remote
/// session is known by *locally* (window client key, `mine`, fleet tile id), so a
/// session this window drives over the transport and the same session the watcher
/// discovers share one identity. Recovered to `(target, real)` via
/// `App.remote_index`; only the transport layer uses the bare `real` id.
fn remote_fleet_id(target: &str, real: &str) -> String {
    format!("{target}{REMOTE_ID_SEP}{real}")
}

/// How a session id should be reached for a control action (rename/kill). A
/// remote id is *self-describing* — [`remote_fleet_id`] formats it as
/// `<target>␟<real>` — so its host and real name are recovered from the id itself,
/// with no dependence on `remote_index` staying populated. A remote id is thus
/// ALWAYS routed over the transport, never spoken to a local control socket (a
/// bogus local socket yields a misleading "hosted by an older ghost" error).
fn remote_id_parts(id: &str) -> Option<(&str, &str)> {
    id.split_once(REMOTE_ID_SEP)
}

/// Floor between reconnect attempts of a host's watch stream, so a host whose
/// `ghost __watch` exits at once can't spin.
const REMOTE_WATCH_RETRY: Duration = Duration::from_millis(1500);

/// Consecutive dropped watch streams (no listing pushed in between) before a
/// remote host's tiles are cleared — a grace period so a momentary blip doesn't
/// flicker the fleet.
const REMOTE_WATCH_MAX_FAILURES: u32 = 3;

/// Rewrite a remote host's listing for the local fleet: give each session a
/// fleet-unique id (`<target>␟<real id>`) so it never collides, keep its real id
/// (or display name) visible as the display name, and tag it with the host's
/// connection so it renders as a remote tile badged with the host.
fn namespace_remote_infos(
    target: &str,
    infos: Vec<ghost_vt::session::SessionInfo>,
) -> Vec<ghost_vt::session::SessionInfo> {
    let spec = ConnectionSpec::parse_target(target);
    infos
        .into_iter()
        .map(|mut i| {
            let display = if i.display_name.is_empty() {
                i.name.clone()
            } else {
                i.display_name.clone()
            };
            i.name = remote_fleet_id(target, &i.name);
            i.display_name = display;
            i.connection = spec.clone();
            i
        })
        .collect()
}

/// A live pushed session-set watch for one connected host: a background thread
/// runs `ghost __watch` over the (already-authenticated) transport and streams
/// each listing back as a [`UserEvent::RemoteSessions`], so the fleet updates the
/// instant a remote session changes rather than on a timer. Dropping the handle
/// stops it — the flag ends the loop and killing the in-flight ssh unwinds a read
/// blocked between listings — so a watcher lives exactly as long as its host is in
/// [`App::remotes`] (until the last window referencing it closes, or the app exits).
struct RemoteWatcher {
    stop: Arc<std::sync::atomic::AtomicBool>,
    /// The currently-running `ghost __watch` child, shared so a stop can kill it
    /// mid-read (the reader is otherwise blocked until the next listing).
    child: Arc<std::sync::Mutex<Option<std::process::Child>>>,
}

impl Drop for RemoteWatcher {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Ok(mut g) = self.child.lock()
            && let Some(c) = g.as_mut()
        {
            let _ = c.kill();
        }
    }
}

/// Start a [`RemoteWatcher`] for `host`: its thread reconnects the `ghost __watch`
/// stream (with a floor between attempts) until stopped, posting each fresh
/// listing and clearing the host's tiles once it has been unreachable for a grace
/// period. Off the event loop, so a slow or blocked ssh never stalls the UI.
fn start_remote_watcher(
    target: String,
    host: RemoteHost,
    proxy: winit::event_loop::EventLoopProxy<UserEvent>,
) -> RemoteWatcher {
    use std::sync::atomic::Ordering::Relaxed;
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let child: Arc<std::sync::Mutex<Option<std::process::Child>>> = Arc::default();
    let (t_stop, t_child) = (stop.clone(), child.clone());
    std::thread::spawn(move || {
        let mut failures: u32 = 0;
        while !t_stop.load(Relaxed) {
            let pushed = watch_stream_once(&target, &host, &proxy, &t_stop, &t_child);
            if t_stop.load(Relaxed) {
                break;
            }
            if pushed {
                failures = 0;
            } else {
                failures = failures.saturating_add(1);
                // Unreachable for a grace period: clear the host's stale tiles.
                if failures >= REMOTE_WATCH_MAX_FAILURES
                    && proxy
                        .send_event(UserEvent::RemoteSessions {
                            target: target.clone(),
                            infos: Vec::new(),
                        })
                        .is_err()
                {
                    break; // the event loop closed
                }
            }
            std::thread::sleep(REMOTE_WATCH_RETRY);
        }
    });
    RemoteWatcher { stop, child }
}

/// Run one `ghost __watch` stream to completion — it closes when the host exits,
/// the connection drops, or a stop kills the child — posting each JSON listing as
/// a namespaced [`UserEvent::RemoteSessions`]. Returns whether any listing was
/// pushed, so the caller tells a live host from a dead one.
fn watch_stream_once(
    target: &str,
    host: &RemoteHost,
    proxy: &winit::event_loop::EventLoopProxy<UserEvent>,
    stop: &std::sync::atomic::AtomicBool,
    child_slot: &std::sync::Mutex<Option<std::process::Child>>,
) -> bool {
    use std::io::BufRead;
    use std::sync::atomic::Ordering::Relaxed;
    let mut cmd = host.remote.watch_command(&host.remote_ghost);
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut proc = match cmd.spawn() {
        Ok(p) => p,
        Err(_) => return false,
    };
    let Some(stdout) = proc.stdout.take() else {
        return false;
    };
    if let Ok(mut g) = child_slot.lock() {
        *g = Some(proc);
    }
    let mut pushed = false;
    for line in std::io::BufReader::new(stdout).lines() {
        if stop.load(Relaxed) {
            break;
        }
        let Ok(line) = line else { break };
        let Ok(infos) = ghost_vt::watch::parse_listing(&line) else {
            continue;
        };
        pushed = true;
        let infos = namespace_remote_infos(target, infos);
        if proxy
            .send_event(UserEvent::RemoteSessions {
                target: target.to_string(),
                infos,
            })
            .is_err()
        {
            stop.store(true, Relaxed); // event loop gone: end the whole watcher
            break;
        }
    }
    // Reap our child (a concurrent stop may already have killed it).
    if let Ok(mut g) = child_slot.lock()
        && let Some(mut c) = g.take()
    {
        let _ = c.kill();
        let _ = c.wait();
    }
    pushed
}

/// Whether a connect worker's finished outcome should still be adopted: only when
/// its window still exists (`current_gen` is `Some`) and no cancel bumped that
/// window's connect generation past the value the worker stamped (`finished_gen`).
/// A closed window (`None`) or a mismatched generation means the connect was
/// superseded and its result must be discarded.
fn connect_outcome_wanted(current_gen: Option<u64>, finished_gen: u64) -> bool {
    current_gen == Some(finished_gen)
}

/// The off-loop half of an ssh connect: with the ControlMaster already open (the
/// PTY warm-up authenticated), negotiate a remote ghost — staging the ~126 MiB
/// binary if the host lacks it, the slow part — and spawn the detached host, then
/// post the [`ConnectOutcome`] back so the main loop attaches. Runs on its own
/// thread so the window stays responsive throughout (it shows "Connecting…").
fn spawn_connect_worker(
    proxy: winit::event_loop::EventLoopProxy<UserEvent>,
    wid: WindowId,
    generation: u64,
    spec: ConnectionSpec,
    name: String,
) {
    std::thread::spawn(move || {
        let outcome = match ghost_vt::remote::RemoteSsh::new(spec.clone()) {
            Ok(remote) => {
                // Forward staging byte-progress to the connect prompt's bar.
                let mut on_progress = |p: ghost_vt::remote::StageProgress| {
                    let _ = proxy.send_event(UserEvent::ConnectProgress {
                        wid,
                        sent: p.sent,
                        total: p.total,
                    });
                };
                match remote.negotiate_with_progress(&mut on_progress) {
                    Some(remote_ghost) => match remote.spawn_host(&remote_ghost, &name) {
                        Ok(()) => ConnectOutcome::Transport { remote_ghost },
                        Err(e) => {
                            ConnectOutcome::Error(format!("could not start the remote host: {e}"))
                        }
                    },
                    None => ConnectOutcome::Fallback,
                }
            }
            Err(e) => ConnectOutcome::Error(format!("could not open the ssh connection: {e}")),
        };
        let _ = proxy.send_event(UserEvent::ConnectFinished {
            wid,
            generation,
            spec,
            name,
            outcome,
        });
    });
}

/// Reconnect to `spec`'s host in the background for a startup restore: open the
/// ControlMaster non-interactively (key/agent auth only — a password or
/// unreachable host fails fast and is simply given up on, its tiles staying cold),
/// negotiate a usable remote ghost, then post [`UserEvent::RemoteReconnected`] so
/// the main loop re-adopts the host's remembered sessions. No PTY, no prompt.
fn spawn_remote_reconnect(
    proxy: winit::event_loop::EventLoopProxy<UserEvent>,
    spec: ConnectionSpec,
) {
    std::thread::spawn(move || {
        let Ok(remote) = ghost_vt::remote::RemoteSsh::new(spec.clone()) else {
            return;
        };
        if !remote.open_master_batch() {
            return; // password/unreachable — deferred, degrade to cold tiles
        }
        if let Some(remote_ghost) = remote.negotiate() {
            let _ = proxy.send_event(UserEvent::RemoteReconnected { spec, remote_ghost });
        }
    });
}

/// The retry floor / cap for a reconnecting session: doubles from 1s to 30s so a
/// drop of seconds or of days recovers, without hammering an unreachable host.
const RECONNECT_BACKOFF: Duration = Duration::from_secs(1);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Probe a dropped remote session's host in the background until it is reachable
/// again and the session `real` still exists, then post
/// [`UserEvent::RemoteReattachReady`] so the main loop re-attaches at the current
/// grid. Retries forever with a capped backoff — a partition of minutes or days
/// recovers when the host returns — reaping the wedged master each round (a silent
/// drop leaves one, and only a fresh probe can tell the host is back). Stops when
/// `stop` is set (the window closed or the session was reattached elsewhere). The
/// blocking `ssh` (which can hang on an unreachable host) must stay off the loop.
fn spawn_reconnect_probe(
    proxy: winit::event_loop::EventLoopProxy<UserEvent>,
    host: RemoteHost,
    wid: WindowId,
    name: String,
    real: String,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;
    std::thread::spawn(move || {
        let mut backoff = RECONNECT_BACKOFF;
        loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            host.remote.reap_wedged_master();
            match host.remote.list_sessions(&host.remote_ghost) {
                // Host reachable and the session survived: re-attach and resync.
                Ok(list) if list.iter().any(|i| i.name == real) => {
                    let _ = proxy.send_event(UserEvent::RemoteReattachReady { wid, name });
                    return;
                }
                // Host reachable but the session is GONE — the host rebooted and
                // wiped it (as opposed to a mere network partition). Waiting can't
                // bring it back, so end the hold; the tile falls to the fleet, where
                // the dead session can be relaunched.
                Ok(_) => {
                    let _ = proxy.send_event(UserEvent::RemoteSessionGone { wid, name });
                    return;
                }
                // Unreachable (still partitioned): keep waiting — this is the
                // survive-a-long-drop path.
                Err(_) => {}
            }
            // Sleep the backoff in short chunks so a stop is noticed promptly.
            let mut slept = Duration::ZERO;
            while slept < backoff {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                let chunk = Duration::from_millis(250).min(backoff - slept);
                std::thread::sleep(chunk);
                slept += chunk;
            }
            backoff = (backoff * 2).min(RECONNECT_BACKOFF_MAX);
        }
    });
}

/// Put a fd (a connect warm-up's PTY) into non-blocking mode so the event loop
/// can drain it without stalling.
fn set_nonblocking(fd: impl std::os::fd::AsFd) -> io::Result<()> {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    let flags = fcntl_getfl(&fd).map_err(io::Error::from)?;
    fcntl_setfl(&fd, flags | OFlags::NONBLOCK).map_err(io::Error::from)?;
    Ok(())
}

/// ssh's password/passphrase prompt, if the warm-up output `buf` ends on one:
/// the last non-empty line, when it mentions a password or passphrase. Used to
/// surface the connect prompt's password field labelled with ssh's own wording.
fn password_prompt(buf: &str) -> Option<String> {
    let tail = buf.rsplit(['\n', '\r']).find(|l| !l.trim().is_empty())?;
    let low = tail.to_ascii_lowercase();
    (low.contains("password:") || low.contains("passphrase")).then(|| tail.trim().to_string())
}

/// A concise failure message from a warm-up ssh's output: its "Permission
/// denied" line if present, else the last non-empty line, else a generic note.
fn auth_error_message(buf: &str) -> String {
    if let Some(l) = buf
        .lines()
        .rev()
        .map(str::trim)
        .find(|l| l.contains("Permission denied"))
    {
        return l.to_string();
    }
    buf.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "ssh connection failed".to_string())
}

/// The identity reported by attaches with no window behind them (the
/// headless bench harness); real windows report their group-derived identity
/// ([`ghost_ui_core::group::window_identity`]) instead.
fn client_identity() -> String {
    format!("ghost-ui:{}", std::process::id())
}

/// Watch the session runtime dir and raise `flag` on any change — the
/// set-change trigger that lets the fleet re-enumerate the moment a session
/// appears or vanishes instead of waiting for its slow floor tick. `None`
/// (nothing to watch, or no watch backend) degrades to floor-tick-only.
fn session_set_watcher(
    flag: Arc<std::sync::atomic::AtomicBool>,
) -> Option<notify::RecommendedWatcher> {
    use notify::Watcher;
    let dir = ghost_vt::paths::runtime_dir();
    // The dir may not exist before the first session; create it so the watch
    // can bind now (hosts create it on demand anyway).
    std::fs::create_dir_all(&dir).ok()?;
    let mut w = notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
        if res.is_ok() {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    })
    .ok()?;
    w.watch(&dir, notify::RecursiveMode::NonRecursive).ok()?;
    Some(w)
}

/// Watch the config dir and raise `flag` when `ui.toml` may have changed — the
/// trigger for a live config reload. Watches the DIRECTORY, not the file:
/// editors replace-on-save (write a temp, rename over), which would drop an
/// inode watch bound to the file. Filtered to the config filename so an
/// unrelated write in the dir doesn't reload (an event with no path — some
/// backends — falls through as a change, since a reload is cheap and
/// idempotent). `None` (no dir, no backend) leaves the config launch-only.
fn config_watcher(flag: Arc<std::sync::atomic::AtomicBool>) -> Option<notify::RecommendedWatcher> {
    use notify::Watcher;
    let dir = ghost_vt::paths::config_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let mut w = notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
        if let Ok(ev) = res {
            let touches_config = ev.paths.is_empty()
                || ev
                    .paths
                    .iter()
                    .any(|p| p.file_name() == Some(std::ffi::OsStr::new("ui.toml")));
            if touches_config {
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
    })
    .ok()?;
    w.watch(&dir, notify::RecursiveMode::NonRecursive).ok()?;
    Some(w)
}

/// The theme reported to session hosts at attach; fixed at startup, so read
/// from the config once.
fn session_theme() -> ghost_ui_core::ThemeColors {
    static THEME: std::sync::OnceLock<ghost_ui_core::ThemeColors> = std::sync::OnceLock::new();
    *THEME.get_or_init(|| theme_colors(&config::UiConfig::load().theme()))
}

fn attach_retry(name: &str, cols: u16, rows: u16) -> Session {
    let start = Instant::now();
    loop {
        match attach(name, cols, rows, &client_identity()) {
            Ok(s) => return s,
            Err(e) => {
                if start.elapsed() > Duration::from_secs(5) {
                    panic!("could not attach to session '{name}': {e}");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// How a [`pump`] drain finished: still live, the child exited, or the transport
/// dropped (a lost connection whose remote session may still be alive — the cue to
/// hold and reconnect rather than tear the tile down).
#[derive(Clone, Copy, PartialEq, Eq)]
enum PumpEnd {
    Live,
    Exited,
    Disconnected,
}

impl PumpEnd {
    /// Whether the session is over on this transport (either way, drop the client).
    fn is_end(self) -> bool {
        !matches!(self, PumpEnd::Live)
    }
}

/// Drain up to `max` pending reads off a session, returning the accumulated output
/// and how it ended. A read error is a transport failure, i.e. `Disconnected`.
fn pump(session: &mut Session, max: usize) -> (Vec<u8>, PumpEnd) {
    let mut bytes = Vec::new();
    for _ in 0..max {
        match session.pump() {
            Ok(p) => {
                let empty = p.output.is_empty();
                if !empty {
                    bytes.extend_from_slice(&p.output);
                }
                if p.ended {
                    return (
                        bytes,
                        if p.disconnected {
                            PumpEnd::Disconnected
                        } else {
                            PumpEnd::Exited
                        },
                    );
                }
                if empty {
                    break;
                }
            }
            Err(_) => return (bytes, PumpEnd::Disconnected),
        }
    }
    (bytes, PumpEnd::Live)
}

// ---- capture mode (headless) -------------------------------------------

/// Execute the model's effects without a window: only `SendInput` matters
/// headlessly (it writes the keystrokes/paste/query-replies back to the child).
fn exec_headless(session: &mut Session, cmds: &[Cmd]) {
    for cmd in cmds {
        if let Cmd::SendInput { bytes, .. } = cmd {
            let _ = session.send_input(bytes);
        }
    }
}

fn capture(path: PathBuf) {
    let name = format!("{}-cap-{}", ghost_vt::paths::host_tag(), std::process::id());
    let command = match std::env::var("GHOST_CMD") {
        Ok(c) => vec!["sh".to_string(), "-c".to_string(), c],
        Err(_) => vec![
            "sh".to_string(),
            "-c".to_string(),
            "printf 'ghost \\033[1mlive\\033[0m session: a != b => c   \
             \\033[31mred\\033[0m \\033[44m blue-bg \\033[0m\\n'"
                .to_string(),
        ],
    };
    server::spawn(SpawnOpts {
        name: name.clone(),
        command,
        size: (COLS, ROWS),
        cwd: None,
        record: None,
        seed_from: None,
        scrollback: screen::DEFAULT_SCROLLBACK,
        max_recording_bytes: None,
        start_on_attach: true,
        connection: None,
    })
    .expect("spawn session");

    let mut session = attach_retry(&name, COLS, ROWS);
    let mut model = TerminalModel::new(name.clone(), COLS, ROWS, metrics());

    // Optionally feed input first, to exercise the keystroke path (the child is
    // typically `cat`, which echoes it back through the PTY).
    if let Ok(feed) = std::env::var("GHOST_FEED") {
        let cmds = model.update(UiEvent::Text(feed));
        exec_headless(&mut session, &cmds);
    }

    // Drive until the child ends or output settles.
    let start = Instant::now();
    let mut last_change = Instant::now();
    loop {
        let (bytes, end) = pump(&mut session, 64);
        let ended = end.is_end();
        if !bytes.is_empty() || ended {
            last_change = if bytes.is_empty() {
                last_change
            } else {
                Instant::now()
            };
            let cmds = model.update(UiEvent::SessionData {
                name: name.clone(),
                bytes,
                ended,
            });
            exec_headless(&mut session, &cmds);
        }
        if model.ended() {
            break;
        }
        let has_text = model.screen().text().iter().any(|l| !l.trim().is_empty());
        if has_text && last_change.elapsed() > Duration::from_millis(250) {
            break;
        }
        if start.elapsed() > Duration::from_secs(10) {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    eprintln!("--- captured screen ---");
    for line in model.screen().text() {
        let t = line.trim_end();
        if !t.is_empty() {
            eprintln!("{t}");
        }
    }

    let scene = model.view();
    let mut renderer = Renderer::headless(config::UiConfig::load().theme());
    renderer.set_fallback(Box::new(font::SystemFallback::new()));
    let img = renderer.render_offscreen_scene(&scene, font_setup().fonts, size_px());
    write_png(&path, &img);
    eprintln!(
        "captured {}x{} to {}",
        img.width,
        img.height,
        path.display()
    );

    let _ = session::kill_session(&name);
}

// ---- interactive mode (window) -----------------------------------------

fn spawn_session(name: &str, command: Vec<String>, connection: Option<ConnectionSpec>) {
    server::spawn(SpawnOpts {
        name: name.to_string(),
        command, // empty => $SHELL (unless `connection` derives an `ssh …` child)
        size: (COLS, ROWS),
        cwd: None,
        // Record like the CLI does (`--no-record` is its opt-out): the
        // recording is what lets a dead session's card preview its last
        // screen, and what seeds a recreate with its predecessor's history.
        record: Some(ghost_vt::paths::recording_path(name)),
        seed_from: None,
        scrollback: screen::DEFAULT_SCROLLBACK,
        max_recording_bytes: Some(ghost_vt::record::DEFAULT_MAX_RECORDING_BYTES),
        start_on_attach: true,
        connection,
    })
    .expect("spawn session");
}

/// The connection a new terminal in a window inherits: the session it was spawned
/// from (the foreground) wins — a new terminal is a sibling of what you're looking
/// at — else the window group's own connection (an explicit "ssh group"), else none
/// (a local `$SHELL`). Read only from stored data, never scraped from a live command
/// line.
///
/// Foreground-first matters after a cross-host fleet take-over: adopting another
/// host's session into a window leaves the group's stored `connection` naming the
/// OLD host (it is "never inferred from adopted members", see [`Group::connection`]),
/// so letting it win would spawn the next session on the wrong host. A local
/// foreground carries no connection, so an ssh group still spawns onto its host.
fn inherited_connection(
    group: Option<&ConnectionSpec>,
    foreground: Option<&ConnectionSpec>,
) -> Option<ConnectionSpec> {
    foreground.or(group).cloned()
}

/// The connected remote host a new inheriting session should be created *on*, if
/// any — the inherited `connection`'s target when we already hold a live
/// transport to it (`connected` = the currently-connected targets). `Some(target)`
/// routes the spawn onto the remote (a real remote ghost session over the
/// transport); `None` keeps it local — a plain `$SHELL`, or an `ssh` child for an
/// ssh connection to a host we are not transported to.
fn remote_spawn_target(
    connection: Option<&ConnectionSpec>,
    connected: &HashSet<String>,
) -> Option<String> {
    let target = connection?.target();
    connected.contains(&target).then_some(target)
}

/// How a freshly-launched window should start.
enum StartupChoice {
    /// Attach to a specific, explicitly-requested session (single view).
    Attach(String),
    /// Spawn a fresh session and show it (single view) — nothing to reconnect to.
    Spawn,
    /// Open the fleet so the user can reconnect, rather than piling up sessions.
    Fleet,
}

/// Decide how to start: honour an explicit `$GHOST_SESSION` request; otherwise
/// open the fleet whenever there is something to return to — a detached live
/// session, or a group remembering a session that is no longer running (its
/// closed block relaunches it) — and only spawn a fresh session when there is
/// nothing to reconnect. Launching must not pile new sessions on top of
/// forgotten ones.
fn startup_choice(
    requested: Option<String>,
    sessions: &[session::SessionInfo],
    groups: &[ghost_ui_core::Group],
) -> StartupChoice {
    let remembered_dead = groups
        .iter()
        .flat_map(|g| &g.members)
        .any(|m| !sessions.iter().any(|s| &s.name == m));
    match requested {
        Some(name) => StartupChoice::Attach(name),
        None if sessions.iter().any(|s| !s.attached) || remembered_dead => StartupChoice::Fleet,
        None => StartupChoice::Spawn,
    }
}

/// The startup decision for a window opened at runtime via File > New Window / Cmd-N.
/// A new window "acts like the first one", but carries no `$GHOST_SESSION` request
/// (that is a launch-only override), so it always takes the plain-launch decision.
fn new_window_choice(
    sessions: &[session::SessionInfo],
    groups: &[ghost_ui_core::Group],
) -> StartupChoice {
    startup_choice(None, sessions, groups)
}

/// Whether a bare launch should recreate the windows open at last quit: only
/// when there is a saved workspace, no explicit `$GHOST_SESSION` request (which
/// opens just that session), and `--fresh` was not passed to start clean.
fn should_restore(fresh: bool, requested: Option<&str>, workspace: &[WindowRecord]) -> bool {
    !fresh && requested.is_none() && !workspace.is_empty()
}

/// One member a restored window should drive.
struct PlanMember {
    id: String,
    /// The session's host is not currently alive, so it must be relaunched
    /// (shell + seeded recording) before attaching.
    dead: bool,
}

/// One window to recreate at startup: its reclaimed group, the grid to open at,
/// its view mode, and the members to drive — each list ordered foreground-LAST so
/// adopting in order leaves the right one focused. Local and remote members are
/// split because they restore by different paths: locals are attached (dead ones
/// relaunched) synchronously; remotes are reconnected to their host and re-adopted
/// asynchronously, never spawned locally.
struct WindowPlan {
    group: ghost_ui_core::Group,
    cols: u16,
    rows: u16,
    fleet: bool,
    /// The window's saved foreground session, if it was one of the driven set —
    /// so a remote reconnect knows whether to foreground it or keep it warm.
    foreground: Option<String>,
    /// Local members to attach now (dead ones relaunched first).
    locals: Vec<PlanMember>,
    /// Remote (transport) member ids (`<target>␟<real>`) to reconnect + re-adopt.
    remotes: Vec<String>,
}

/// A remote member a startup restore is waiting to re-adopt into a window once
/// its host reconnects (queued in [`App::pending_remote_restores`], drained by
/// [`App::finish_remote_reconnect`]).
struct PendingRemote {
    wid: WindowId,
    /// The composite id `<target>␟<real>`.
    composite: String,
    /// The window was saved in the fleet overview → observe the tile in place
    /// (the fleet's own observe path); else drive it into the single view.
    fleet: bool,
    /// This session was the window's saved foreground → drive+foreground it; else
    /// attach it as a background (warm) mirror without stealing the live foreground.
    foreground: bool,
}

/// How the app should open its first window(s), decided at launch.
enum Startup {
    /// Recreate the saved workspace: one window per record (via [`restore_plan`]).
    Restore(Vec<ghost_ui_core::WindowRecord>),
    /// Open a single view attached to this session (an explicit `$GHOST_SESSION`
    /// request or a freshly-spawned one).
    Single(String),
    /// Open the fleet overview — something to reconnect to, or nothing saved.
    Fleet,
}

/// Turn the saved workspace into a per-window restore plan. A record whose group
/// is gone from the registry (all its members were killed/forgotten) can't be
/// restored, so it is dropped. Members are the window's attached set with the
/// foreground moved last (adopting in order then leaves it foreground), each
/// flagged dead when no live session by that name exists.
fn restore_plan(
    records: &[ghost_ui_core::WindowRecord],
    sessions: &[session::SessionInfo],
    groups: &[ghost_ui_core::Group],
) -> Vec<WindowPlan> {
    let alive = |id: &str| sessions.iter().any(|s| s.name == id);
    records
        .iter()
        .filter_map(|rec| {
            let group = groups.iter().find(|g| g.id == rec.group_id)?.clone();
            let mut ids: Vec<String> = rec.attached.clone();
            // Foreground last, but only if it was actually one of the driven set.
            if let Some(fg) = &rec.foreground
                && ids.iter().any(|a| a == fg)
            {
                ids.retain(|id| id != fg);
                ids.push(fg.clone());
            }
            // Split by transport: a remote member reconnects to its host and is
            // re-adopted asynchronously (never spawned locally); locals attach now,
            // dead ones relaunched. `partition` preserves the foreground-last order
            // within each list.
            let (remotes, locals): (Vec<String>, Vec<String>) =
                ids.into_iter().partition(|id| is_remote_id(id));
            let locals: Vec<PlanMember> = locals
                .into_iter()
                .map(|id| PlanMember {
                    dead: !alive(&id),
                    id,
                })
                .collect();
            // Nothing to restore at all → drop the window; a remote-only window is
            // kept so its host is reconnected and its sessions re-adopted.
            if locals.is_empty() && remotes.is_empty() {
                return None;
            }
            Some(WindowPlan {
                group,
                cols: rec.cols,
                rows: rec.rows,
                fleet: rec.fleet,
                foreground: rec.foreground.clone(),
                locals,
                remotes,
            })
        })
        .collect()
}

/// The spawn options for relaunching a dead session `id` from its descriptor.
///
/// A relaunch restores the session's *substrate*, never its *workload*: it
/// always drops the recorded `descriptor.command` (so a reboot doesn't re-run
/// dev servers), and seeds the last screen and scrollback from the recording so
/// you land at a prompt below them. For a local session that substrate is a
/// fresh `$SHELL` (empty command); for a connection session it is a fresh login
/// to the same host — the connection is carried forward so the relaunch
/// reconnects rather than dropping to a useless local shell over frozen remote
/// scrollback. The child is deferred to the first attach.
fn respawn_opts(id: &str, d: &ghost_vt::descriptor::Descriptor, recording: PathBuf) -> SpawnOpts {
    let seed_from = recording.exists().then(|| recording.clone());
    SpawnOpts {
        name: id.to_string(),
        command: Vec::new(),
        size: (COLS, ROWS),
        cwd: d.cwd.clone(),
        record: Some(recording),
        seed_from,
        scrollback: screen::DEFAULT_SCROLLBACK,
        max_recording_bytes: Some(ghost_vt::record::DEFAULT_MAX_RECORDING_BYTES),
        start_on_attach: true,
        // Carry the connection forward: a dead ssh session reconnects on
        // relaunch (substrate), while a local session stays `None` → `$SHELL`.
        connection: d.connection.clone(),
    }
}

/// Relaunch a dead session `id`'s host from its descriptor (see [`respawn_opts`]).
/// Best-effort: a failed spawn is logged and the caller simply skips it.
fn spawn_dead(id: &str) -> bool {
    // A remote session belongs to its host; it can never be a local process. Guard
    // the one chokepoint every relaunch/restore path funnels through, so no bogus
    // local shell is ever spawned under a composite id (see `is_remote_id`).
    if is_remote_id(id) {
        eprintln!("ghost: refusing to locally relaunch remote session '{id}'");
        return false;
    }
    let d = ghost_vt::descriptor::read(id).unwrap_or_default();
    let recording = ghost_vt::paths::recording_path(id);
    match server::spawn(respawn_opts(id, &d, recording)) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("ghost: relaunching '{id}' failed: {e}");
            false
        }
    }
}

fn interactive(fresh: bool) {
    // Route instrumentation (cache stats, ...) to stderr under `RUST_LOG`. Off unless
    // asked — e.g. `RUST_LOG=ghost::cache=trace` watches cache hit-rates live — so the
    // instrumented code stays free in normal runs.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .try_init();

    // Bench mode (`GHOST_BENCH=dive`/`slide`) drives a scripted animation against
    // this same real path with a synthetic session list, so it opens with no host.
    let harness = bench::Harness::from_env();
    // Single-instance guard (skipped in bench mode, a dev tool that must run in
    // its own process): if a ghost UI already owns the runtime dir, forward a
    // new-window request to it and exit — BEFORE the session enumeration below,
    // which would otherwise adopt (steal) the running instance's sessions.
    let (_instance_lock, instance_listener) = if harness.is_some() {
        (None, None)
    } else {
        match instance::acquire() {
            instance::Role::Secondary => return,
            instance::Role::Primary { _lock, listener } => (_lock, listener),
        }
    };
    let groups = groups::load();
    let workspace = windows::load();
    let startup = if harness.is_some() {
        Startup::Fleet // the harness populates and dives it
    } else {
        let requested = std::env::var("GHOST_SESSION").ok();
        let sessions = session::list().unwrap_or_default();
        // A bare launch with saved windows recreates them, taking precedence over
        // the reconnect-through-the-fleet default below; `--fresh` or an explicit
        // `$GHOST_SESSION` skip that and open just what was asked for.
        if should_restore(fresh, requested.as_deref(), &workspace) {
            Startup::Restore(workspace.clone())
        } else {
            match requested {
                Some(name) => Startup::Single(name),
                None => match startup_choice(None, &sessions, &groups) {
                    StartupChoice::Attach(name) => Startup::Single(name),
                    StartupChoice::Fleet => Startup::Fleet,
                    StartupChoice::Spawn => {
                        let n = format!("{}-{}", ghost_vt::paths::host_tag(), std::process::id());
                        spawn_session(&n, vec![], None);
                        Startup::Single(n)
                    }
                },
            }
        }
    };

    // A user-event loop so the native macOS menu can post `UserEvent::Menu` back
    // from AppKit's main thread (see [`menu`]). The type parameter is inert on
    // platforms without a menu.
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .expect("event loop");
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    // As the owner, accept new-window requests forwarded by later launches and
    // turn each into a fresh window (like File > New Window) on the event loop.
    if let Some(listener) = instance_listener {
        let proxy = proxy.clone();
        instance::serve(listener, move || {
            let _ = proxy.send_event(UserEvent::OpenWindow);
        });
    }
    let remotes: Arc<std::sync::Mutex<HashMap<String, RemoteHost>>> = Arc::default();
    let sessions_changed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let config_changed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let next_group_color = (groups.len() % ghost_ui_core::group::GROUP_PALETTE.len()) as u8;
    let mut app = App {
        windows: HashMap::new(),
        clipboard: None,
        start: Instant::now(),
        startup,
        next_session_seq: 0,
        next_group_seq: 0,
        next_group_color,
        bench: harness,
        focused: None,
        proxy: Some(proxy),
        remotes,
        remote_infos: HashMap::new(),
        remote_index: HashMap::new(),
        remote_watchers: HashMap::new(),
        pending_remote_restores: HashMap::new(),
        reconnecting: HashMap::new(),
        subs: HashMap::new(),
        groups,
        _watcher: session_set_watcher(sessions_changed.clone()),
        sessions_changed,
        _config_watcher: config_watcher(config_changed.clone()),
        config_changed,
        // Seed the write-on-change baseline with what's already persisted, so the
        // first save only rewrites the file once the live windows diverge from it.
        last_workspace: workspace,
        workspace_dirty: false,
    };
    // Each host gets a pushed `ghost __watch` stream started on connect (see
    // `App::register_remote`); nothing to poll here.
    event_loop.run_app(&mut app).expect("run app");
}

/// Pick a surface alpha mode. Our pipeline emits premultiplied alpha, so for a
/// translucent window we want `PreMultiplied` (and `Inherit`/`Auto`, which defer
/// to a premultiplied compositor); `PostMultiplied` would expect straight alpha
/// and wash the colours, so it is normally declined.
///
/// Metal is the exception: its capability list is exactly
/// `[Opaque, PostMultiplied]`, and choosing `PostMultiplied` does nothing but
/// `CAMetalLayer.isOpaque = false` (wgpu-hal performs no conversion) — while
/// Core Animation *always* composites layer content as premultiplied. So on
/// that backend `PostMultiplied` is a mislabel for the premultiplied semantics
/// we want, and refusing it is what kept macOS windows opaque.
///
/// A capability list always has at least one entry, and an opaque window just
/// takes the first (usually Opaque).
fn choose_alpha_mode(
    modes: &[wgpu::CompositeAlphaMode],
    want_transparent: bool,
    backend: wgpu::Backend,
) -> wgpu::CompositeAlphaMode {
    use wgpu::CompositeAlphaMode::{Auto, Inherit, PostMultiplied, PreMultiplied};
    if want_transparent {
        for preferred in [PreMultiplied, Inherit, Auto] {
            if modes.contains(&preferred) {
                return preferred;
            }
        }
        if backend == wgpu::Backend::Metal && modes.contains(&PostMultiplied) {
            return PostMultiplied;
        }
        eprintln!("ghost-ui: no premultiplied alpha mode; window will stay opaque");
    }
    modes[0]
}

/// Whether to request compositor backdrop-blur for a window. Blur is only
/// meaningful behind a translucent surface (the compositor blurs what shows
/// through), so it rides on the same `want_transparent` gate as the window's
/// alpha — an opaque window never asks for it, even if the config opts in.
fn want_blur(want_transparent: bool, cfg_blur: bool) -> bool {
    want_transparent && cfg_blur
}

/// Pick the surface (swapchain) format. Our shader writes colours that are
/// already sRGB-encoded 8-bit bytes — the offscreen golden target is
/// [`ghost_renderer::FORMAT`] (`Rgba8Unorm`) — so the swapchain must be a plain
/// (non-sRGB) 8-bit UNORM BGRA/RGBA format: an sRGB target would re-encode and
/// wash the colours out, and an HDR / high-bit-depth target (`Rgba16Float`,
/// `Rgb10a2Unorm`, all of which report `is_srgb() == false`) would reinterpret
/// the bytes. Prefer `Bgra8Unorm` (the native swapchain format on most platforms
/// and the one WebGPU guarantees), then `Rgba8Unorm`; the explicit order makes
/// the choice deterministic rather than dependent on driver enumeration order.
/// Only if neither is offered do we fall back to the first non-sRGB format, and
/// finally — degraded — to the first format. A capability list is never empty.
fn choose_surface_format(formats: &[wgpu::TextureFormat]) -> wgpu::TextureFormat {
    use wgpu::TextureFormat::{Bgra8Unorm, Rgba8Unorm};
    for preferred in [Bgra8Unorm, Rgba8Unorm] {
        if formats.contains(&preferred) {
            return preferred;
        }
    }
    formats
        .iter()
        .copied()
        .find(|f| !f.is_srgb())
        .unwrap_or(formats[0])
}

/// Per-window GPU state, valid only once the window (and surface) exist. The frame
/// production itself lives in [`Target`] (shared with the headless harness); this
/// just owns the window, the surface target, and the per-window render state.
struct Graphics {
    window: Arc<Window>,
    /// The window's swapchain surface, wrapped as a swappable render target.
    target: Target,
    renderer: Renderer,
    /// Skips re-drawing a scene identical to the last presented, and computes the
    /// changed band for a partial redraw.
    scene_cache: SceneCache,
    /// The resolved faces for this window (regular + any real bold/italic), built
    /// once. Building a `FontRef` per-frame would mint a fresh swash `CacheKey` each
    /// time (a global atomic), re-parse the font header, and — before the shape cache
    /// was keyed on stable font data — silently defeat that cache. Reuse it everywhere.
    fonts: ghost_shaper::FontSet<'static>,
}

impl Graphics {
    fn new(event_loop: &ActiveEventLoop, spec: WindowSpec) -> Self {
        let WindowSpec {
            theme,
            option_as_meta,
            blur,
            cols,
            rows,
            pad,
        } = spec;
        // Open sized to `cols`x`rows` cells at the base font, plus the padding border on
        // each side, so the configured grid fits inside it (padding surrounds, not eats
        // into, the grid). A LOGICAL size (not physical) so winit scales it by the monitor
        // DPI — the grid then works out to exactly `cols`x`rows` at any scale
        // (`grid_from_pixels` divides physical px by cell·scale), which a physical size
        // would only get right at 1x.
        let m = metrics();
        let size = LogicalSize::new(
            f64::from(cols) * f64::from(m.advance) + 2.0 * f64::from(pad),
            f64::from(rows) * f64::from(m.line_height) + 2.0 * f64::from(pad),
        );
        // Request a transparent window only when the theme is translucent, so an
        // opaque setup never pays the compositor's alpha-blending cost.
        let want_transparent = theme.bg_alpha < 1.0;
        // Bench mode measures the render path at a realistic size, so open maximized
        // (the small default grid would understate per-frame raster cost).
        let maximized = std::env::var_os("GHOST_BENCH").is_some();
        // Ask the compositor for backdrop-blur only when the window is both
        // translucent and configured for it — blur without transparency is a
        // no-op state (see `want_blur`), and honoured only on KWin/macOS anyway.
        let attrs = Window::default_attributes()
            .with_title("ghost")
            .with_inner_size(size)
            .with_maximized(maximized)
            .with_transparent(want_transparent)
            .with_blur(want_blur(want_transparent, blur));
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        window.set_ime_allowed(true);
        // On macOS, optionally treat Option as Meta (ESC-prefix) rather than
        // letting it compose accented characters — the terminal-standard
        // behaviour, controlled by `[input] option_as_meta`. Off macOS, Alt is
        // already Meta, so the preference is inert there.
        #[cfg(target_os = "macos")]
        {
            use winit::platform::macos::WindowExtMacOS;
            window.set_option_as_alt(option_as_alt(option_as_meta));
        }
        #[cfg(not(target_os = "macos"))]
        let _ = option_as_meta;

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = instance
            .create_surface(window.clone())
            .expect("create surface");
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
        }))
        .expect("no surface-compatible adapter");
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .expect("request device");

        let caps = surface.get_capabilities(&adapter);
        let format = choose_surface_format(&caps.formats);
        let win = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: win.width.max(1),
            height: win.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: choose_alpha_mode(
                &caps.alpha_modes,
                want_transparent,
                adapter.get_info().backend,
            ),
            view_formats: vec![],
        };
        surface.configure(&device, &config);

        let gpu = Gpu {
            device: device.clone(),
            queue,
        };
        let mut renderer = Renderer::new(gpu, theme, format);
        // Draw characters outside the configured family (symbols, box-drawing, arrows)
        // from a covering system font instead of the tofu box.
        renderer.set_fallback(Box::new(font::SystemFallback::new()));
        // Keep the frost grain a fixed logical size on HiDPI.
        renderer.set_scale_factor(window.scale_factor() as f32);

        Graphics {
            window,
            target: Target::Surface(SurfaceTarget::new(
                surface,
                config,
                device,
                !want_transparent,
            )),
            renderer,
            scene_cache: SceneCache::default(),
            fonts: font_setup().fonts,
        }
    }

    /// Physical pixel size of the window surface. (App windows are always
    /// surface-backed; the offscreen variant exists only for the headless harness.)
    fn size(&self) -> (u32, u32) {
        match &self.target {
            Target::Surface(s) => s.size(),
            Target::Offscreen => (0, 0),
        }
    }

    fn resize(&mut self, w: u32, h: u32) {
        if let Target::Surface(s) = &mut self.target {
            s.resize(w, h);
        }
        // The reconfigured surface holds no drawn frame; force the next redraw.
        self.scene_cache.invalidate();
    }

    /// Force the next present to fully re-render and re-raster the foreground, dropping
    /// both the scene-equality skip and the "cached texture is current" assumption. The
    /// self-heal calls this when the watchdog detects a stale-frame freeze (a present we
    /// recorded that never reached the glass): the next paint is a full one, so the
    /// stale texture is replaced. Re-renders rather than reconfiguring the swapchain, so
    /// a false trigger just redraws identical pixels — no flicker.
    fn force_foreground_repaint(&mut self) {
        self.scene_cache.invalidate();
        self.renderer.invalidate_foreground();
    }

    /// Stretch-blit the renderer's held resize snapshot to the surface — immediate
    /// feedback during an interactive resize, without the relayout/re-raster of a
    /// full scene render. Returns whether a frame actually landed (`false` if there is
    /// no snapshot or the surface acquire failed), so the caller paces honestly rather
    /// than marking a dropped blit as painted.
    fn blit_snapshot(&mut self) -> bool {
        let Target::Surface(s) = &mut self.target else {
            return false;
        };
        let landed = s.blit_snapshot(&mut self.renderer, || self.window.pre_present_notify());
        if landed {
            // What's on screen is the stretched snapshot, not a model scene; keep the
            // scene cache invalid so the eventual crisp commit always redraws.
            self.scene_cache.invalidate();
        }
        landed
    }

    /// Draw a scene to the window. `scene.size_px` must equal the surface size, and
    /// `font_px` the glyph size the scene was laid out for (the model keeps both in
    /// sync via `UiEvent::Resize` and its render scale). Delegates the damage→draw→
    /// present glue to [`Target::render_frame`] — the same code the headless harness
    /// runs — and returns its [`FrameOutcome`], which decides the pacer bookkeeping.
    fn render(&mut self, scene: &Scene, font_px: f32) -> FrameOutcome {
        let outcome = self.target.render_frame(
            &mut self.renderer,
            &mut self.scene_cache,
            scene,
            self.fonts,
            font_px,
            || self.window.pre_present_notify(),
        );
        // Per-frame cache-efficiency line under `RUST_LOG=ghost::cache=trace`; free otherwise.
        self.renderer.emit_cache_trace();
        outcome
    }
}

/// What a new window should open as, handed to [`Frontend::open_window`].
struct WindowSpec {
    theme: ghost_renderer::Theme,
    option_as_meta: bool,
    /// Request compositor backdrop-blur ("frosted glass") behind the translucent
    /// background. Inert unless the theme is translucent; see [`want_blur`].
    blur: bool,
    cols: u16,
    rows: u16,
    pad: f32,
}

/// A realized window handed back by the [`Frontend`]: its id, physical size, and
/// scale (enough to size a model), plus its GPU graphics — `None` when the
/// frontend is headless (a behaviour-only window with no surface).
struct NewWindow {
    id: WindowId,
    gfx: Option<Graphics>,
    size_px: (u32, u32),
    scale: f64,
}

/// The windowing backend the [`App`] drives, behind a seam so its behaviour can
/// run without winit. The production impl ([`WinitFrontend`]) wraps a winit
/// `ActiveEventLoop`; the test impl ([`HeadlessFrontend`]) mints surface-less
/// windows so `App` logic (sessions, remotes, the fleet) is exercised offscreen
/// and deterministically. The App threads `&dyn Frontend` where it used to thread
/// `&ActiveEventLoop`.
trait Frontend {
    /// Realize a new window (a real OS window + GPU surface, or a headless stub).
    fn open_window(&self, spec: WindowSpec) -> NewWindow;
    /// Leave the event loop (quit).
    fn exit(&self);
    /// Set when the loop next wakes.
    fn set_control_flow(&self, flow: ControlFlow);
}

/// The production [`Frontend`]: a thin wrapper over the live winit event loop,
/// constructed fresh at each `ApplicationHandler` entry point.
struct WinitFrontend<'a> {
    event_loop: &'a ActiveEventLoop,
}

impl Frontend for WinitFrontend<'_> {
    fn open_window(&self, spec: WindowSpec) -> NewWindow {
        let gfx = Graphics::new(self.event_loop, spec);
        let id = gfx.window.id();
        let scale = gfx.window.scale_factor();
        let size_px = gfx.size();
        NewWindow {
            id,
            gfx: Some(gfx),
            size_px,
            scale,
        }
    }

    fn exit(&self) {
        self.event_loop.exit();
    }

    fn set_control_flow(&self, flow: ControlFlow) {
        self.event_loop.set_control_flow(flow);
    }
}

/// A headless [`Frontend`] for tests: mints surface-less windows so the App's
/// behaviour runs offscreen and deterministically, and records a quit so a test
/// can assert on it. No winit, no GPU.
#[cfg(test)]
struct HeadlessFrontend {
    /// Mints distinct synthetic [`WindowId`]s for the surface-less windows.
    next_id: std::cell::Cell<u64>,
    /// Set when the App asks to quit ([`Frontend::exit`]).
    exited: std::cell::Cell<bool>,
}

#[cfg(test)]
impl HeadlessFrontend {
    fn new() -> Self {
        Self {
            next_id: std::cell::Cell::new(1),
            exited: std::cell::Cell::new(false),
        }
    }
}

#[cfg(test)]
impl Frontend for HeadlessFrontend {
    fn open_window(&self, spec: WindowSpec) -> NewWindow {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        // Physical size == logical at scale 1.0, sized exactly as `Graphics::new`.
        let m = metrics();
        let w = (f64::from(spec.cols) * f64::from(m.advance) + 2.0 * f64::from(spec.pad)) as u32;
        let h =
            (f64::from(spec.rows) * f64::from(m.line_height) + 2.0 * f64::from(spec.pad)) as u32;
        NewWindow {
            id: WindowId::from(id),
            gfx: None,
            size_px: (w.max(1), h.max(1)),
            scale: 1.0,
        }
    }

    fn exit(&self) {
        self.exited.set(true);
    }

    fn set_control_flow(&self, _flow: ControlFlow) {}
}

#[cfg(test)]
impl App {
    /// A behaviour-only App: no event loop, GPU, watcher, or watcher — the seam a
    /// [`HeadlessFrontend`] plugs into. Drive it with the App's own methods
    /// (`open_fleet_window`, `dispatch`, `on_*`) and assert on its state.
    fn headless() -> Self {
        App {
            windows: HashMap::new(),
            clipboard: None,
            start: Instant::now(),
            startup: Startup::Fleet,
            next_session_seq: 0,
            next_group_seq: 0,
            next_group_color: 0,
            bench: None,
            focused: None,
            proxy: None,
            remotes: Arc::default(),
            remote_infos: HashMap::new(),
            remote_index: HashMap::new(),
            remote_watchers: HashMap::new(),
            pending_remote_restores: HashMap::new(),
            reconnecting: HashMap::new(),
            subs: HashMap::new(),
            groups: Vec::new(),
            _watcher: None,
            sessions_changed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            _config_watcher: None,
            config_changed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_workspace: Vec::new(),
            workspace_dirty: false,
        }
    }
}

/// The scheme's default fg/bg handed to the models, so apps that query their
/// terminal colors (OSC 10/11/12 — vim, fzf) see the configured theme. Ghost
/// paints the cursor with the theme foreground, so that is its query color.
fn theme_colors(theme: &ghost_renderer::Theme) -> ghost_ui_core::ThemeColors {
    ghost_ui_core::ThemeColors {
        fg: theme.fg,
        bg: theme.bg,
        cursor: theme.fg,
    }
}

/// Open a hyperlink in the system handler (`Cmd::OpenUrl` — the model has
/// already allowlisted the scheme). Spawned detached, with a reaper thread so
/// the handler process never lingers as a zombie.
fn open_url(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let child = std::process::Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if let Ok(mut child) = child {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

/// A GUI ssh connect in flight for a window: the warm-up `ssh … true` running in
/// a PTY, which opens (and authenticates) the shared ControlMaster the later
/// transport steps reuse. Pumped from [`about_to_wait`](App::about_to_wait): its
/// output is scanned for ssh's password/passphrase prompt (surfaced to the connect
/// prompt so the user types into the window), and its exit drives the connect to
/// completion (negotiate → spawn → attach) or failure. Dropping it kills the ssh.
struct ConnectSetup {
    /// The target to connect to; handed to the connect worker once auth succeeds.
    spec: ConnectionSpec,
    /// The remote session name to spawn and attach once auth succeeds.
    name: String,
    pty: pty_process::blocking::Pty,
    child: std::process::Child,
    /// Warm-up output accumulated so far, scanned for the ssh password prompt.
    buf: String,
    /// True once the current prompt has been surfaced to the window, so echoed
    /// bytes don't re-ask; cleared when the user submits a password (a re-prompt
    /// then means the password was wrong and asks again).
    asked: bool,
}

impl Drop for ConnectSetup {
    fn drop(&mut self) {
        // Cancelled mid-auth (window closed / Escape): kill the warm-up ssh so it
        // doesn't linger prompting on a PTY nothing reads.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Per-window state: the GPU surface and pure model, plus the input bookkeeping
/// that is inherently per-window (focus modifiers, pointer position, click
/// detection, and the model's scheduled tick).
struct WindowState {
    /// The window's GPU graphics (surface + winit window), or `None` when driven
    /// by a headless [`Frontend`] — a behaviour-only window with no surface, so
    /// the render/redraw paths are inert (see [`WindowState::request_redraw`]).
    gfx: Option<Graphics>,
    root: RootModel,
    /// This window's own session clients (the single-view session plus any fleet
    /// previews). Dropping the window drops these, which detaches every session
    /// it held — the "close = detach" default, with no shared-pool bookkeeping.
    sessions: HashMap<String, Session>,
    /// Read-only mirrors of the sessions this window's fleet shows but does
    /// not drive (`Cmd::Observe`). Live only while the overview is open; their
    /// output feeds the tiles as `SessionData`, and only their `Resized` event
    /// is forwarded (the app-wide subscription already delivers the rest).
    observers: HashMap<String, Subscriber>,
    /// Dead sessions whose recording has been played into their tile already,
    /// so the periodic sweep doesn't re-feed the same last screen every tick.
    /// A name is cleared when its session lives again (a fresh death re-feeds).
    dead_fed: HashSet<String>,
    mods: ModifiersState,
    /// Last pointer position in physical pixels (winit reports it only on move,
    /// so we cache it for button/wheel events).
    pointer_pos: PointPx,
    /// When this window's next scheduled `Tick` is due, if any.
    next_tick: Option<Instant>,
    /// Most recent left/middle/right press (time, button, pos) for detecting
    /// double/triple clicks, and the running click count.
    last_click: Option<(Instant, PointerButton, PointPx)>,
    click_count: u8,
    /// Rate-limits repaints so output floods / held keys can't drive a software
    /// rasterizer at the 8 ms poll rate (see [`pacer`]).
    pacer: pacer::FramePacer,
    /// Foreground render-stall watchdog (see [`rendertrace`]). Timestamps the repaint
    /// pipeline and classifies a foreground that stops presenting while its session
    /// keeps feeding. Inert unless `RUST_LOG=ghost::render=trace`.
    render_trace: rendertrace::RenderTrace,
    /// Defers the costly relayout/reflow during an interactive resize, stretching
    /// the last crisp frame in the meantime (see [`resize`]).
    resize: resize::ResizeCoalescer,
    /// Per-frame timing during animations, printed on dive end when
    /// `GHOST_FRAME_STATS` is set (see [`framestats`]). Inert otherwise.
    stats: framestats::FrameStats,
    /// A window created mid-run (File > New Window / Cmd-N / the Dock item) can have
    /// its Metal drawable configured before the window is on screen, so its very first
    /// present lands nowhere and it comes up blank until the user resizes it. Set true
    /// at creation; the first `RedrawRequested` reconfigures the surface (now that the
    /// window is realized) and clears it, so the opening frame is actually visible.
    needs_surface_sync: bool,
    /// Whether this window has ever presented a frame. Until it has, its drawable may
    /// not be ready — `get_current_texture` returns nothing and the present is silently
    /// dropped — so [`about_to_wait`](App::about_to_wait) keeps requesting redraws every
    /// pass (not the pacer's single request) until one lands. Otherwise a window created
    /// mid-run comes up blank (only its title bar) until an unrelated event forces a
    /// redraw. Set once, on the first successful present.
    presented_ok: bool,
    /// Whether the platform has told us this window is occluded (fully hidden: another
    /// Space, minimized, behind an opaque window). Tracked from `WindowEvent::Occluded`
    /// so the foreground render-stall watchdog can skip it — an occluded surface can't
    /// present, so its "stall" is the platform withholding the drawable, not our bug.
    occluded: bool,
    /// A GUI ssh connect in flight (the window is showing the connect prompt).
    /// Present from the `Cmd::ConnectSshWindow` handler until auth resolves; its
    /// PTY is pumped each `about_to_wait` pass.
    connect: Option<ConnectSetup>,
    /// Monotonic generation for in-flight connects, bumped whenever one is
    /// cancelled ([`Cmd::CancelConnect`]). The off-thread connect worker stamps
    /// the current value when it starts; [`finish_connect`](App::finish_connect)
    /// adopts its result only if the stamp still matches, so a cancel that lands
    /// during staging drops (and kills) the now-unwanted remote session instead of
    /// adopting it.
    connect_gen: u64,
}

impl WindowState {
    /// Request a repaint, if this window has a surface. A no-op for a headless
    /// window (no [`Graphics`]), so behaviour paths can call it unconditionally.
    fn request_redraw(&self) {
        if let Some(gfx) = &self.gfx {
            gfx.window.request_redraw();
        }
    }

    /// Click count for a press of `button` at the current pointer position: a
    /// repeat of the same button within 400ms and a few pixels increments the
    /// count (double-, triple-click), otherwise it resets to 1.
    fn count_click(&mut self, button: PointerButton) -> u8 {
        const WINDOW: Duration = Duration::from_millis(400);
        const SLOP: f64 = 4.0;
        let now = Instant::now();
        let count = match self.last_click {
            Some((t, b, p))
                if b == button
                    && now.duration_since(t) < WINDOW
                    && (p.x - self.pointer_pos.x).abs() < SLOP
                    && (p.y - self.pointer_pos.y).abs() < SLOP =>
            {
                self.click_count.saturating_add(1)
            }
            _ => 1,
        };
        self.click_count = count;
        self.last_click = Some((now, button, self.pointer_pos));
        count
    }
}

/// The thin imperative shell: owns the world (live windows, the clipboard, the
/// clock), holds the pure models, and shuttles `UiEvent`s in and `Cmd`s out.
/// Each window owns its own session clients (see [`WindowState::sessions`]).
struct App {
    /// Live windows by id; each owns its GPU surface, pure model, and sessions.
    windows: HashMap<WindowId, WindowState>,
    /// Lazily-opened system clipboard for copy/paste (shared).
    clipboard: Option<arboard::Clipboard>,
    /// Start of the monotonic clock injected into models via `Tick`.
    start: Instant,
    /// How the first window(s) start, set at construction and consumed by the
    /// first `resumed`: restore the saved workspace, attach a single session, or
    /// open the fleet.
    startup: Startup,
    /// Per-process counter making spawned session names unique.
    next_session_seq: u64,
    /// Per-process counter making minted window-group ids unique, and the
    /// palette color the next window's group takes (seeded past the loaded
    /// registry so fresh windows keep cycling where it left off).
    next_group_seq: u64,
    next_group_color: u8,
    /// Frame-pacing bench harness (`GHOST_BENCH=dive`/`slide`): scripts animations
    /// against the real render path and synthesises the session list. `None` in
    /// normal use.
    bench: Option<bench::Harness>,
    /// The window that last gained focus — the target for menu actions that act on
    /// "the current window" (New Session, Copy, Paste, Zoom, Toggle Fleet). Kept
    /// across focus loss; a stale id is filtered out at use (see `focused_window`).
    focused: Option<WindowId>,
    /// Proxy for posting messages into the event loop from another thread: native
    /// menu selections (AppKit's main thread, macOS) and the remote-fleet watcher's
    /// listings ([`UserEvent::RemoteSessions`]). `None` under a headless
    /// [`Frontend`], where no threads post back into the loop.
    proxy: Option<winit::event_loop::EventLoopProxy<UserEvent>>,
    /// Remote hosts reached over the ssh transport, keyed by target — retained
    /// after a successful connect and shared with the watcher thread that lists
    /// their sessions. A host stays until its last window/session is gone.
    remotes: Arc<std::sync::Mutex<HashMap<String, RemoteHost>>>,
    /// The latest remote listing per host (fleet-namespaced ids), delivered by the
    /// watcher and merged into every `Cmd::ListSessions` reply.
    remote_infos: HashMap<String, Vec<ghost_vt::session::SessionInfo>>,
    /// Maps a namespaced remote fleet id back to `(target, real id)`, so a
    /// take-over/observe of a remote tile reaches the right host and session.
    /// Rebuilt whenever `remote_infos` changes.
    remote_index: HashMap<String, (String, String)>,
    /// One live `ghost __watch` stream per connected host, keyed by target: the
    /// push that keeps `remote_infos` fresh. Dropping an entry stops its thread,
    /// so a watcher ends exactly when its host leaves `remotes` (window close /
    /// app exit).
    remote_watchers: HashMap<String, RemoteWatcher>,
    /// Remote members a startup restore is waiting to re-adopt, keyed by target
    /// (see [`PendingRemote`], [`App::reconnect_restored_remotes`] /
    /// [`App::finish_remote_reconnect`]). Each carries the window's SAVED view mode
    /// and foreground flag, which can't be read back from the live window: a
    /// restored remote-only window always opens as a fleet (no local tile to dive
    /// into, so F9 can't force it single), so the saved intent must ride along here.
    /// An entry for a host that never reconnects (password/unreachable) just lingers,
    /// drained on a successful reconnect or when its window closes.
    pending_remote_restores: HashMap<String, Vec<PendingRemote>>,
    /// Remote sessions whose transport dropped and are holding in the reconnecting
    /// state, keyed by `(window, composite id)`. Each value is the stop flag for its
    /// background probe thread (`spawn_reconnect_probe`): set it and drop the entry
    /// to cancel (the window closed, or the session reattached). Presence also
    /// dedupes — a repeated drop won't start a second probe. See
    /// [`begin_reconnect`](App::begin_reconnect) / [`finish_reattach`](App::finish_reattach).
    reconnecting: HashMap<(WindowId, String), std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// App-wide state subscriptions, one per session whose host serves them
    /// (reconciled against every session list). Pushed snapshots/events are
    /// fanned out to every window; sessions on older hosts simply stay covered
    /// by the fleet's slow floor tick.
    subs: HashMap<String, Subscriber>,
    /// The authoritative user-defined session groups: loaded from the data dir
    /// at startup, updated (and persisted) on every `Cmd::SaveGroups`, and
    /// broadcast to windows as `UiEvent::GroupsLoaded` so they stay in step.
    groups: Vec<ghost_ui_core::Group>,
    /// Set by the runtime-dir watcher thread when the session *set* may have
    /// changed; drained on the loop to hint an immediate re-enumeration.
    sessions_changed: Arc<std::sync::atomic::AtomicBool>,
    /// The watch itself; dropping it stops event delivery. `None` when the
    /// runtime dir cannot be watched — the floor tick still reconciles.
    _watcher: Option<notify::RecommendedWatcher>,
    /// Set by the config-dir watcher when `ui.toml` may have changed; drained on
    /// the loop to hot-reload the live-reloadable settings (see [`reload_config`]).
    config_changed: Arc<std::sync::atomic::AtomicBool>,
    /// The config watch itself; dropping it stops delivery. `None` when the
    /// config dir cannot be watched — the config then only applies at launch.
    _config_watcher: Option<notify::RecommendedWatcher>,
    /// The workspace snapshot last written to disk, so a rebuild that matches it
    /// skips the write. Kept current as windows change so a crash or reboot still
    /// restores what was open (see [`App::save_workspace`]).
    last_workspace: Vec<ghost_ui_core::WindowRecord>,
    /// Set when a window's set or state may have changed; the loop flushes the
    /// workspace snapshot once per wake rather than on every nested dispatch.
    workspace_dirty: bool,
}

impl App {
    /// Keep the app-wide subscription pool matched to the session set: drop
    /// subscriptions for sessions that vanished, open one for each newcomer
    /// whose host serves them. A host that predates subscriptions (or a
    /// connect race with a dying session) is simply skipped — its state stays
    /// covered by the fleet's slow reconcile.
    fn sync_subscriptions(&mut self, infos: &[ghost_vt::session::SessionInfo]) {
        let names: std::collections::HashSet<&str> =
            infos.iter().map(|i| i.name.as_str()).collect();
        self.subs.retain(|name, _| names.contains(name.as_str()));
        for info in infos {
            if !self.subs.contains_key(&info.name)
                && let Ok(sub) = Subscriber::connect(&info.name)
            {
                self.subs.insert(info.name.clone(), sub);
            }
        }
    }

    /// The descriptor sweep that runs with every session listing: tell the
    /// window which group members are dead-but-remembered (its fleet shows
    /// them as dead tiles), play each one's recording into its tile once (the
    /// last screen, via the ordinary Resized-push + output path), and prune
    /// descriptors nothing references any more — not live, in no group — so
    /// the data dir doesn't keep one per session ever spawned.
    fn sync_dead_sessions(
        &mut self,
        wid: WindowId,
        live: &[ghost_vt::session::SessionInfo],
        event_loop: &dyn Frontend,
    ) {
        let live_names: HashSet<&str> = live.iter().map(|i| i.name.as_str()).collect();
        let mut dead: Vec<ghost_ui_core::DeadSession> = Vec::new();
        for name in self.groups.iter().flat_map(|g| &g.members) {
            if live_names.contains(name.as_str()) || dead.iter().any(|d| &d.name == name) {
                continue;
            }
            // The descriptor is the resurrection ticket: a member without one
            // was discarded (killed, or its child exited — possibly from
            // another process, whose registry save we never saw). Not naming
            // it here is what tells the fleet to drop its membership.
            let Some(d) = ghost_vt::descriptor::read(name) else {
                continue;
            };
            dead.push(ghost_ui_core::DeadSession {
                name: name.clone(),
                display_name: d.display_name,
                command: d.command,
                cwd: d.cwd.as_deref().map(session::display_path),
            });
        }
        self.dispatch(wid, UiEvent::DeadSessions(dead.clone()), event_loop);
        // A session alive again may die again later: let it re-feed then.
        if let Some(w) = self.windows.get_mut(&wid) {
            w.dead_fed.retain(|n| !live_names.contains(n.as_str()));
        }
        for d in dead {
            let fresh = self
                .windows
                .get_mut(&wid)
                .is_some_and(|w| w.dead_fed.insert(d.name.clone()));
            if !fresh {
                continue;
            }
            let Ok(rec) = ghost_vt::record::read(&ghost_vt::paths::recording_path(&d.name)) else {
                continue; // never recorded: the tile stays a placeholder
            };
            let s = screen::Screen::from_recording(&rec, 0);
            let (cols, rows) = s.dimensions();
            self.dispatch(
                wid,
                UiEvent::SessionPush {
                    name: d.name.clone(),
                    push: SessionPush::Event(ghost_vt::protocol::SessionEvent::Resized {
                        cols,
                        rows,
                    }),
                },
                event_loop,
            );
            self.dispatch(
                wid,
                UiEvent::SessionData {
                    name: d.name,
                    bytes: s.resync(),
                    ended: false,
                },
                event_loop,
            );
        }
        let grouped: HashSet<&String> = self.groups.iter().flat_map(|g| &g.members).collect();
        for name in ghost_vt::descriptor::all_names() {
            if !live_names.contains(name.as_str()) && !grouped.contains(&name) {
                ghost_vt::descriptor::remove(&name);
            }
        }
        // Recordings follow the same referencing rule: one whose session is
        // neither live nor remembered by a group seeds and previews nothing —
        // remove it rather than keep one per session ever spawned.
        if let Ok(entries) = std::fs::read_dir(ghost_vt::paths::data_dir().join("recordings")) {
            for e in entries.flatten() {
                let p = e.path();
                let name = match (p.extension(), p.file_stem().and_then(|s| s.to_str())) {
                    (Some(ext), Some(stem)) if ext == "ghostrec" => stem.to_string(),
                    _ => continue,
                };
                if !live_names.contains(name.as_str()) && !grouped.contains(&name) {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
    }

    /// Drain every subscription and fan its pushes out to all windows (each
    /// window's fleet keeps its own tiles). A subscription ending usually
    /// means the session died: drop it and hint a re-enumeration.
    fn pump_subscriptions(&mut self, event_loop: &dyn Frontend) {
        let mut pushes: Vec<(String, SessionPush)> = Vec::new();
        let mut any_ended = false;
        self.subs.retain(|name, sub| {
            let p = sub.pump().unwrap_or_default();
            if let Some(state) = p.snapshot {
                pushes.push((name.clone(), SessionPush::Snapshot(state)));
            }
            for e in p.events {
                pushes.push((name.clone(), SessionPush::Event(e)));
            }
            any_ended |= p.ended;
            !p.ended
        });
        let changed = any_ended
            || self
                .sessions_changed
                .swap(false, std::sync::atomic::Ordering::Relaxed);
        if pushes.is_empty() && !changed {
            return;
        }
        let wids: Vec<WindowId> = self.windows.keys().copied().collect();
        for (name, push) in pushes {
            for wid in &wids {
                self.dispatch(
                    *wid,
                    UiEvent::SessionPush {
                        name: name.clone(),
                        push: push.clone(),
                    },
                    event_loop,
                );
            }
        }
        if changed {
            for wid in &wids {
                self.dispatch(*wid, UiEvent::SessionsChanged, event_loop);
            }
        }
    }

    /// Re-read `ui.toml` and re-apply the live-reloadable settings to every open
    /// window: color scheme / opacity / frost (the renderer theme), compositor
    /// blur, inner padding, and the model's default colors. Font and initial grid
    /// are deliberately NOT reloaded — font setup is process-global, and
    /// columns/rows are open-time only (re-gridding would fight the user's manual
    /// resize). Triggered by the config watcher; takes `cfg` explicitly so tests
    /// drive it directly.
    fn reload_config(&mut self, cfg: &config::UiConfig, event_loop: &dyn Frontend) {
        let theme = cfg.theme();
        let colors = theme_colors(&theme);
        let pad = cfg.padding();
        let blur = want_blur(theme.bg_alpha < 1.0, cfg.blur());
        let wids: Vec<WindowId> = self.windows.keys().copied().collect();
        for wid in wids {
            let cmds = {
                let Some(w) = self.windows.get_mut(&wid) else {
                    continue;
                };
                // Model side (headless-observable): the default colors and padding.
                let cmds = w.root.set_theme(colors);
                w.root.set_padding(pad);
                // Gfx side (no model representation; absent under a headless
                // frontend): the renderer theme — opacity/frost/scheme colours bake
                // into cached surfaces, so `set_theme` drops them — the compositor
                // blur, and a forced repaint. The `Scene` is theme-independent, so
                // without invalidating the scene cache AND requesting a redraw an
                // identical scene would be skipped as unchanged and nothing would
                // repaint.
                if let Some(gfx) = w.gfx.as_mut() {
                    gfx.renderer.set_theme(theme);
                    gfx.scene_cache.invalidate();
                    gfx.window.set_blur(blur);
                    gfx.window.request_redraw();
                }
                cmds
            };
            self.exec(wid, cmds, event_loop);
            // Padding feeds the grid geometry, which only recomputes on a resize:
            // re-drive the window's current size so a padding change takes effect
            // now rather than on the next manual resize. Headless windows have no
            // surface/size, so they skip this (their model padding is already set).
            if let Some((w_px, h_px, scale)) = self
                .windows
                .get(&wid)
                .and_then(|w| w.gfx.as_ref())
                .map(|g| {
                    let (w_px, h_px) = g.size();
                    (w_px, h_px, g.window.scale_factor())
                })
            {
                self.dispatch(wid, UiEvent::Resize { w_px, h_px, scale }, event_loop);
            }
        }
        self.workspace_dirty = true;
    }

    /// Feed an event to window `wid`'s model and execute the effects it returns.
    fn dispatch(&mut self, wid: WindowId, ev: UiEvent, event_loop: &dyn Frontend) {
        let cmds = match self.windows.get_mut(&wid) {
            Some(w) => w.root.update(ev),
            None => return,
        };
        self.exec(wid, cmds, event_loop);
        // A handled event may have changed a window's foreground, view, grid, or
        // membership; mark the workspace for the loop's once-per-wake flush.
        self.workspace_dirty = true;
    }

    fn exec(&mut self, wid: WindowId, cmds: Vec<Cmd>, event_loop: &dyn Frontend) {
        let now_ms = self.now_ms();
        for cmd in cmds {
            match cmd {
                Cmd::SendInput { session, bytes } => {
                    if let Some(w) = self.windows.get_mut(&wid)
                        && let Some(s) = w.sessions.get_mut(&session)
                    {
                        let _ = s.send_input(&bytes);
                    }
                }
                Cmd::Resize {
                    session,
                    cols,
                    rows,
                } => {
                    if let Some(w) = self.windows.get_mut(&wid)
                        && let Some(s) = w.sessions.get_mut(&session)
                    {
                        let _ = s.resize(cols, rows);
                    }
                }
                Cmd::ReadClipboard => {
                    let text = self.read_clipboard();
                    self.dispatch(wid, UiEvent::ClipboardText(text), event_loop);
                }
                Cmd::WriteClipboard(text) => self.write_clipboard(text),
                Cmd::ReadPrimary => {
                    let text = self.read_primary();
                    self.dispatch(wid, UiEvent::ClipboardText(text), event_loop);
                }
                Cmd::WritePrimary(text) => self.write_primary(text),
                Cmd::ListSessions => {
                    // In bench mode the host isn't running; answer from the harness so
                    // a reconcile keeps the synthetic fleet populated.
                    let infos = match &self.bench {
                        Some(h) => h.session_list(),
                        None => session::list().unwrap_or_default(),
                    };
                    let live = self.bench.is_none();
                    if live {
                        // Subscriptions and the dead-session sweep are local-only:
                        // remote sessions have no local socket/descriptor/recording.
                        self.sync_subscriptions(&infos);
                    }
                    // Merge the connected hosts' latest listings (watcher-fed) so
                    // the fleet shows local and remote sessions together.
                    let mut combined = infos.clone();
                    for r in self.remote_infos.values() {
                        combined.extend(r.iter().cloned());
                    }
                    self.dispatch(wid, UiEvent::SessionList(combined), event_loop);
                    if live {
                        self.sync_dead_sessions(wid, &infos, event_loop);
                    }
                }
                Cmd::Attach(id) => {
                    if let Some(w) = self.windows.get_mut(&wid)
                        && !w.sessions.contains_key(&id)
                    {
                        // Handshake at the window's real grid (see `attach_into`).
                        let (cols, rows) = w.root.grid();
                        if let Ok(s) = attach(&id, cols, rows, &w.root.client_identity()) {
                            w.sessions.insert(id, s);
                        }
                    }
                }
                Cmd::Observe(id) if self.remote_index.contains_key(&id) => {
                    // Live remote preview: observe the session over its host's
                    // transport, feeding the tile exactly like a local observer.
                    if self.bench.is_none()
                        && self
                            .windows
                            .get(&wid)
                            .is_some_and(|w| !w.observers.contains_key(&id))
                        && let Some((target, real)) = self.remote_index.get(&id).cloned()
                    {
                        match self.observe_remote(&target, &real) {
                            Some(sub) => {
                                if let Some(w) = self.windows.get_mut(&wid) {
                                    w.observers.insert(id, sub);
                                }
                            }
                            // No live connection (host gone) or a failed channel:
                            // report the mirror dead so the tile reverts to a
                            // placeholder and a later reconcile retries.
                            None => self.dispatch(
                                wid,
                                UiEvent::SessionData {
                                    name: id,
                                    bytes: Vec::new(),
                                    ended: true,
                                },
                                event_loop,
                            ),
                        }
                    }
                }
                Cmd::Observe(id) => {
                    if self.bench.is_none()
                        && let Some(w) = self.windows.get_mut(&wid)
                        && !w.observers.contains_key(&id)
                    {
                        match Subscriber::observe(&id) {
                            Ok(sub) => {
                                w.observers.insert(id, sub);
                            }
                            // An old host or a dying session: report the
                            // mirror dead so the fleet reverts the tile to a
                            // placeholder and retries on a later reconcile.
                            Err(_) => self.dispatch(
                                wid,
                                UiEvent::SessionData {
                                    name: id,
                                    bytes: Vec::new(),
                                    ended: true,
                                },
                                event_loop,
                            ),
                        }
                    }
                }
                Cmd::Unobserve(id) => {
                    if let Some(w) = self.windows.get_mut(&wid) {
                        w.observers.remove(&id);
                    }
                }
                Cmd::SaveGroups(new_groups) => {
                    // Write-on-change: reclaiming the same memberships (every
                    // window during a multi-window restore re-asserts the groups
                    // it loaded) yields identical state, so skip the redundant
                    // disk write and rebroadcast. Only a real change persists,
                    // then rebroadcasts to the *other* windows so every open fleet
                    // agrees (the sender already holds this state).
                    if new_groups != self.groups {
                        // Persist the full membership, remote sessions included, so a
                        // group is remembered across a restart and its remote members
                        // rejoin it on reconnect (see restore).
                        groups::save(&new_groups);
                        self.groups = new_groups.clone();
                        let others: Vec<WindowId> = self
                            .windows
                            .keys()
                            .copied()
                            .filter(|&other| other != wid)
                            .collect();
                        for other in others {
                            self.dispatch(
                                other,
                                UiEvent::GroupsLoaded(new_groups.clone()),
                                event_loop,
                            );
                        }
                    }
                }
                Cmd::Detach(id) => {
                    // Drop this window's client for the session (it keeps running
                    // under its host); other windows' clients are unaffected.
                    if let Some(w) = self.windows.get_mut(&wid) {
                        w.sessions.remove(&id);
                    }
                }
                Cmd::Kill(id) if is_remote_id(&id) => {
                    // Kill the remote session over its host's transport (off-loop),
                    // then drop any client we hold; the watcher drops the tile.
                    // Route by the id itself, like Rename below: a remote id is
                    // self-describing, and the one most worth killing — a cold tile
                    // whose host dropped — is neither driven nor listed, so the
                    // index does not hold it and gating on it would misroute the
                    // kill to the local path (bogus socket, kill silently dropped).
                    if let Some((target, real)) = remote_id_parts(&id) {
                        self.spawn_remote_kill(target, real);
                    }
                    if let Some(w) = self.windows.get_mut(&wid) {
                        w.sessions.remove(&id);
                    }
                }
                Cmd::Kill(id) => {
                    // Kill the session and its process, then drop any client we held.
                    let _ = session::kill_session(&id);
                    if let Some(w) = self.windows.get_mut(&wid) {
                        w.sessions.remove(&id);
                    }
                }
                Cmd::Recreate(id) => {
                    // Bring a dead session back and step into it. A REMOTE tile is
                    // recreated on ITS HOST over the transport, never as a local
                    // shell — route by the self-describing composite id (like
                    // Cmd::Kill below); `spawn_remote_session` recreates + attaches +
                    // adopts, the remote counterpart of the local branch here.
                    if let Some((target, real)) =
                        remote_id_parts(&id).map(|(t, r)| (t.to_string(), r.to_string()))
                    {
                        self.spawn_remote_session(wid, &target, &real, event_loop);
                    } else if self.respawn_dead(wid, &id) && self.attach_into(wid, &id) {
                        self.dispatch(wid, UiEvent::AdoptSession(id), event_loop);
                    }
                }
                Cmd::Resurrect(id) => {
                    // The background half of a group relaunch: the host comes
                    // back (serving its seeded screen), but nothing attaches —
                    // the child command starts when the user first opens the
                    // session, and the runtime-dir watcher's re-list revives
                    // the tile. A failed spawn just leaves the tile dead. A REMOTE
                    // member is recreated on its host over the transport, not locally
                    // (a remote reboot wipes the host's sessions — this is what brings
                    // them back).
                    if let Some((target, real)) =
                        remote_id_parts(&id).map(|(t, r)| (t.to_string(), r.to_string()))
                    {
                        self.respawn_remote_dead(&target, &real);
                    } else {
                        self.respawn_dead(wid, &id);
                    }
                }
                Cmd::Rename { session, name } => {
                    // A remote session renames over its host's transport (off-loop);
                    // a local one over its control connection. Either works whether
                    // or not this window holds it. Route by the id itself: a remote
                    // id carries its own host+name, so it never falls through to the
                    // local path (whose bogus socket would report a misleading "older
                    // ghost" error). On refusal the fleet's optimistic label reverts.
                    if let Some((target, real)) = remote_id_parts(&session) {
                        self.spawn_remote_rename(target, real, &name);
                    } else if let Err(e) = ghost_vt::client::rename(&session, &name) {
                        eprintln!("ghost: rename failed: {e}");
                    }
                }
                Cmd::Spawn { name, command } => {
                    spawn_session(&name, command, None);
                    // Best-effort attach; a later reconcile re-attaches if it lost the race.
                    if let Some(w) = self.windows.get_mut(&wid) {
                        // Handshake at the window's real grid (see `attach_into`).
                        let (cols, rows) = w.root.grid();
                        if let Ok(s) = attach(&name, cols, rows, &w.root.client_identity()) {
                            w.sessions.insert(name, s);
                        }
                    }
                }
                Cmd::NewWindow => self.open_launch_window(event_loop),
                Cmd::NewSshWindow => self.open_connect_window(event_loop),
                Cmd::NewSshSession => self.open_connect_session(wid),
                Cmd::ConnectSshWindow { spec } => {
                    self.connect_ssh_window(wid, spec);
                }
                Cmd::ConnectSshSession { spec } => {
                    self.connect_ssh_session(wid, spec);
                }
                Cmd::ConnectPassword(password) => {
                    self.connect_feed_password(wid, &password);
                }
                Cmd::CancelConnect => {
                    // Drop the in-flight connect without closing the window; the
                    // `ConnectSetup`'s `Drop` kills the warm-up ssh. The core already
                    // dismissed the prompt, so the window returns to its session.
                    // Bump the connect generation so a worker already past auth (its
                    // remote session spawned) is recognized as stale in `finish_connect`
                    // and its orphan is killed rather than adopted.
                    if let Some(w) = self.windows.get_mut(&wid) {
                        w.connect = None;
                        w.connect_gen = w.connect_gen.wrapping_add(1);
                        w.request_redraw();
                    }
                }
                Cmd::CloseWindow => {
                    self.close_window(wid);
                    if self.windows.is_empty() {
                        self.shutdown(event_loop);
                    }
                }
                Cmd::SpawnSession => {
                    let name = self.unique_session_name();
                    // Inherit the window's ssh connection (group's own, else the
                    // foreground session's) so a new terminal follows the one it
                    // branches off onto the same host.
                    let connection = self.inherited_spawn_connection(wid);
                    // Inheritance-over-remote: if the inherited host is one we already
                    // hold a live transport to, create the session ON it (a real
                    // remote ghost session), matching the group's other sessions —
                    // not a local `ssh` child.
                    let connected: HashSet<String> = self
                        .remotes
                        .lock()
                        .map(|m| m.keys().cloned().collect())
                        .unwrap_or_default();
                    match remote_spawn_target(connection.as_ref(), &connected) {
                        Some(target) => self.spawn_remote_session(wid, &target, &name, event_loop),
                        None => {
                            spawn_session(&name, vec![], connection);
                            if self.attach_into(wid, &name) {
                                self.dispatch(wid, UiEvent::AdoptSession(name), event_loop);
                            }
                        }
                    }
                }
                Cmd::TakeOver(id) => {
                    // A remote tile attaches over its host's transport; a local one
                    // over its unix socket.
                    if let Some((target, real)) = self.remote_index.get(&id).cloned() {
                        self.take_over_remote(wid, &id, &target, &real, event_loop);
                    } else {
                        // Switch the window to `id`'s single view. Attach if we don't
                        // already hold it — stealing the display from another window
                        // for a confirmed take-over of a session attached elsewhere.
                        let held = self
                            .windows
                            .get(&wid)
                            .is_some_and(|w| w.sessions.contains_key(&id));
                        if held || self.attach_into(wid, &id) {
                            self.dispatch(wid, UiEvent::AdoptSession(id), event_loop);
                        }
                    }
                }
                Cmd::UploadImage {
                    id,
                    width,
                    height,
                    rgba,
                } => {
                    if let Some(gfx) = self.windows.get_mut(&wid).and_then(|w| w.gfx.as_mut()) {
                        gfx.renderer.upload_image(id, width, height, &rgba);
                    }
                }
                Cmd::RequestAttention => {
                    if let Some(gfx) = self.windows.get(&wid).and_then(|w| w.gfx.as_ref()) {
                        gfx.window.request_user_attention(Some(
                            winit::window::UserAttentionType::Informational,
                        ));
                    }
                }
                Cmd::Redraw => {
                    // Don't paint inline — record the request and let the pacer
                    // release it within the frame budget (coalescing bursts).
                    if let Some(w) = self.windows.get_mut(&wid) {
                        w.pacer.request();
                        w.render_trace.saw_redraw_cmd(now_ms);
                    }
                }
                Cmd::SetTitle(t) => {
                    if let Some(gfx) = self.windows.get(&wid).and_then(|w| w.gfx.as_ref()) {
                        gfx.window.set_title(&t);
                    }
                }
                Cmd::OpenUrl(url) => open_url(&url),
                Cmd::PointerIcon(icon) => {
                    if let Some(gfx) = self.windows.get(&wid).and_then(|w| w.gfx.as_ref()) {
                        gfx.window.set_cursor(match icon {
                            ghost_ui_core::PointerIcon::Pointer => {
                                winit::window::CursorIcon::Pointer
                            }
                            ghost_ui_core::PointerIcon::Default => {
                                winit::window::CursorIcon::Default
                            }
                        });
                    }
                }
                Cmd::ScheduleTick { after_ms } => {
                    if let Some(w) = self.windows.get_mut(&wid) {
                        // Keep the earliest pending deadline: two schedulers can
                        // coexist (animation frames vs the synchronized-output
                        // release backstop), and models tolerate early ticks but
                        // an overwritten-later one would stall the first caller.
                        let due = Instant::now() + Duration::from_millis(after_ms);
                        w.next_tick = Some(match w.next_tick {
                            Some(t) if t < due => t,
                            _ => due,
                        });
                        w.render_trace.saw_tick_scheduled(now_ms);
                    }
                }
                Cmd::Quit => self.shutdown(event_loop),
            }
        }
    }

    fn read_clipboard(&mut self) -> Option<String> {
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        self.clipboard.as_mut().and_then(|cb| cb.get_text().ok())
    }

    fn write_clipboard(&mut self, text: String) {
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        if let Some(cb) = self.clipboard.as_mut() {
            let _ = cb.set_text(text);
        }
    }

    /// Read the primary selection (middle-click paste). Only X11/Wayland have a
    /// primary selection; elsewhere this is a no-op so middle-click does nothing.
    #[cfg(target_os = "linux")]
    fn read_primary(&mut self) -> Option<String> {
        use arboard::{GetExtLinux, LinuxClipboardKind};
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        self.clipboard
            .as_mut()
            .and_then(|cb| cb.get().clipboard(LinuxClipboardKind::Primary).text().ok())
    }

    #[cfg(not(target_os = "linux"))]
    fn read_primary(&mut self) -> Option<String> {
        None
    }

    /// Publish a selection to the primary selection. No-op off X11/Wayland.
    #[cfg(target_os = "linux")]
    fn write_primary(&mut self, text: String) {
        use arboard::{LinuxClipboardKind, SetExtLinux};
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        if let Some(cb) = self.clipboard.as_mut() {
            let _ = cb.set().clipboard(LinuxClipboardKind::Primary).text(text);
        }
    }

    #[cfg(not(target_os = "linux"))]
    fn write_primary(&mut self, _text: String) {}

    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }

    /// Timestamp user input on a window's render trace — the "kick" label that lets a
    /// recovered-stall report say whether a present self-recovered or needed an input
    /// (a scroll). Gated so a normal run does nothing.
    fn note_input(&mut self, id: WindowId) {
        if tracing::enabled!(target: "ghost::render", tracing::Level::TRACE) {
            let now_ms = self.now_ms();
            if let Some(w) = self.windows.get_mut(&id) {
                w.render_trace.saw_input(now_ms);
            }
        }
    }

    /// Advance the bench harness one turn: fire the next scripted animation when the
    /// last has settled, or exit when the run is done. The single bench window's
    /// `is_animating` gates the script (so one only starts once the prior finishes);
    /// dispatched F9 / tile-selects / Ctrl-Tabs drive the real render+present path.
    fn drive_bench(&mut self, event_loop: &dyn Frontend) {
        let Some(wid) = self.windows.keys().next().copied() else {
            return;
        };
        let now_ms = self.now_ms();
        let animating = self
            .windows
            .get(&wid)
            .is_some_and(|w| w.root.is_animating());
        // Collect first (releases the `&mut self.bench` borrow) so dispatch can run.
        let actions = match self.bench.as_mut() {
            Some(h) => h.step(now_ms, animating),
            None => return,
        };
        for action in actions {
            match action {
                bench::Action::Dispatch(ev) => self.dispatch(wid, ev, event_loop),
                bench::Action::Exit => {
                    eprintln!("ghost bench: scripted dives complete");
                    event_loop.exit();
                }
            }
        }
    }

    /// A fresh, process-unique session name for a spawned session.
    fn unique_session_name(&mut self) -> String {
        let seq = self.next_session_seq;
        self.next_session_seq += 1;
        format!(
            "{}-{}-{}",
            ghost_vt::paths::host_tag(),
            std::process::id(),
            seq
        )
    }

    /// Mint a new window's group identity: a process-unique durable id and
    /// the next palette color (whose name it carries until renamed).
    fn mint_group(&mut self) -> ghost_ui_core::Group {
        let seq = self.next_group_seq;
        self.next_group_seq += 1;
        let color = self.next_group_color;
        self.next_group_color =
            (self.next_group_color + 1) % ghost_ui_core::group::GROUP_PALETTE.len() as u8;
        ghost_ui_core::Group::auto(format!("win-{}-{}", std::process::id(), seq), color)
    }

    /// Respawn a dead session under its old name: a fresh shell seeded from the
    /// previous life's recording, so its last screen and scrollback come back and
    /// you land at a prompt below them. Deliberately a shell, never
    /// `descriptor.command` — a relaunch restores context, it does not re-run what
    /// died (which could be anything, and re-running it unbidden is the surprise
    /// we avoid). The child is deferred to the first attach (`start_on_attach`).
    fn respawn_dead(&mut self, wid: WindowId, id: &str) -> bool {
        if !spawn_dead(id) {
            return false;
        }
        // Its tile previews the OLD recording; a fresh death after this new life
        // must re-feed.
        if let Some(w) = self.windows.get_mut(&wid) {
            w.dead_fed.remove(id);
        }
        true
    }

    /// Relaunch a dead REMOTE session on its host over the transport (`ghost new
    /// -d <real>`), the remote counterpart of [`respawn_dead`](Self::respawn_dead):
    /// a remote reboot wipes the host's tmpfs sessions, so recovery must recreate
    /// them on the host, never as a local shell (which `spawn_dead` refuses).
    /// Best-effort and attach-free — the watcher's next listing revives the tile
    /// (the background half of a group relaunch); the interactive Recreate uses
    /// [`spawn_remote_session`](Self::spawn_remote_session), which also steps in.
    fn respawn_remote_dead(&self, target: &str, real: &str) {
        let host = self
            .remotes
            .lock()
            .ok()
            .and_then(|m| m.get(target).cloned());
        let Some(host) = host else {
            eprintln!("ghost: no live connection to {target} to relaunch '{real}'");
            return;
        };
        // A silent remote reboot leaves the shared master wedged (TCP dead, process
        // kept by ControlPersist); clear it so the relaunch opens a fresh connection
        // instead of multiplexing onto the corpse.
        host.remote.reap_wedged_master();
        if let Err(e) = host.remote.spawn_host(&host.remote_ghost, real) {
            eprintln!("ghost: could not relaunch remote session '{real}' on {target}: {e}");
        }
    }

    /// A single-view restored remote session whose host is reachable but whose
    /// session is GONE — the host rebooted while ghost was off, wiping its tmpfs
    /// sessions. Relaunch it on the host and re-attach, the startup-restore
    /// counterpart of respawning a dead LOCAL session (a local dead session is
    /// relaunched+seeded on restore; a remote one must be recreated on its host).
    /// `ghost new -d` returns before the session is fully listening, so re-attach
    /// is retried on a tight budget; a lasting failure returns false and leaves the
    /// tile cold. Blocking, but bounded and only on the restore path — which already
    /// does synchronous ssh handshakes here. Returns whether the tile is now driven.
    fn relaunch_remote_and_attach(
        &mut self,
        wid: WindowId,
        host: &RemoteHost,
        composite: &str,
        real: &str,
    ) -> bool {
        host.remote.reap_wedged_master();
        if host.remote.spawn_host(&host.remote_ghost, real).is_err() {
            return false;
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let cmd = host.remote.pipe_command(&host.remote_ghost, real);
            if self.attach_ssh_into(wid, composite, cmd) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// A driven remote session's transport just dropped: start (or keep) the
    /// reconnecting hold. Spawns a background probe that waits for the host to come
    /// back and the session to still exist, then posts
    /// [`UserEvent::RemoteReattachReady`]. Idempotent — a session already holding
    /// (a repeated drop before the probe finishes) is left alone. The tile's visible
    /// hold is set by the `SessionDisconnected` the caller already dispatched.
    fn begin_reconnect(&mut self, wid: WindowId, name: String) {
        if self.reconnecting.contains_key(&(wid, name.clone())) {
            return;
        }
        let Some((target, real)) =
            remote_id_parts(&name).map(|(t, r)| (t.to_string(), r.to_string()))
        else {
            return;
        };
        let host = self
            .remotes
            .lock()
            .ok()
            .and_then(|m| m.get(&target).cloned());
        let (Some(host), Some(proxy)) = (host, self.proxy.clone()) else {
            return;
        };
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.reconnecting.insert((wid, name.clone()), stop.clone());
        spawn_reconnect_probe(proxy, host, wid, name, real, stop);
    }

    /// The probe says a reconnecting session's host is back and the session still
    /// exists: re-attach at the window's CURRENT grid (so the host resyncs to the
    /// size we'll show) and clear the hold with [`UiEvent::SessionReattached`], whose
    /// resync repaints the recovered screen. If the attach races and fails (the
    /// session vanished in the gap), keep holding and re-probe. Stale readys — the
    /// window closed, or we're no longer holding this — are dropped.
    fn finish_reattach(&mut self, wid: WindowId, name: String, event_loop: &dyn Frontend) {
        let Some(stop) = self.reconnecting.get(&(wid, name.clone())).cloned() else {
            return;
        };
        let dead_end = |app: &mut Self| {
            app.reconnecting.remove(&(wid, name.clone()));
        };
        let Some((target, real)) =
            remote_id_parts(&name).map(|(t, r)| (t.to_string(), r.to_string()))
        else {
            dead_end(self);
            return;
        };
        let host = self
            .remotes
            .lock()
            .ok()
            .and_then(|m| m.get(&target).cloned());
        let Some(host) = host else {
            dead_end(self);
            return;
        };
        let cmd = host.remote.pipe_command(&host.remote_ghost, &real);
        if self.attach_ssh_into(wid, &name, cmd) {
            self.reconnecting.remove(&(wid, name.clone()));
            self.dispatch(wid, UiEvent::SessionReattached { name }, event_loop);
        } else if let Some(proxy) = self.proxy.clone() {
            // Raced: keep the hold, probe again from the floor.
            spawn_reconnect_probe(proxy, host, wid, name, real, stop);
        }
    }

    /// The probe reached the host but the session is gone (a reboot wiped it): end
    /// the reconnecting hold as a normal exit, so the window falls back to the fleet
    /// where the now-dead session can be relaunched. Waiting couldn't recover it.
    fn end_reconnect_gone(&mut self, wid: WindowId, name: String, event_loop: &dyn Frontend) {
        if let Some(stop) = self.reconnecting.remove(&(wid, name.clone())) {
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.dispatch(
            wid,
            UiEvent::SessionData {
                name,
                bytes: Vec::new(),
                ended: true,
            },
            event_loop,
        );
    }

    fn attach_into(&mut self, wid: WindowId, name: &str) -> bool {
        let Some(w) = self.windows.get_mut(&wid) else {
            return false;
        };
        if w.sessions.contains_key(name) {
            return true;
        }
        // Complete the handshake at the window's real grid, never a provisional
        // 80×24: the host lays out its resync at the handshake size, so attaching
        // a maximized window at 80×24 would reflow a full-size screen down and
        // pin its cursor to that smaller bottom row — the next output then lands
        // mid-screen (see `RootModel::grid`).
        let (cols, rows) = w.root.grid();
        match attach(name, cols, rows, &w.root.client_identity()) {
            Ok(s) => {
                w.sessions.insert(name.to_string(), s);
                true
            }
            Err(e) => {
                eprintln!("could not attach to session '{name}': {e}");
                false
            }
        }
    }

    /// [`attach_into`](Self::attach_into) over the SSH transport: attach a remote
    /// session (reached by `cmd`, an `ssh … __pipe`) into window `wid`.
    fn attach_ssh_into(&mut self, wid: WindowId, name: &str, cmd: std::process::Command) -> bool {
        let Some(w) = self.windows.get_mut(&wid) else {
            return false;
        };
        if w.sessions.contains_key(name) {
            return true;
        }
        let (cols, rows) = w.root.grid();
        match attach_over_ssh(cmd, name, cols, rows, &w.root.client_identity()) {
            Ok(s) => {
                w.sessions.insert(name.to_string(), s);
                true
            }
            Err(e) => {
                eprintln!("could not attach to remote session '{name}': {e}");
                false
            }
        }
    }

    /// Begin connecting this window to a remote host over the SSH transport (the
    /// connect prompt's host was submitted): open a PTY and start the warm-up
    /// `ssh … true` in it. ssh authenticates there — prompting for a password on
    /// the tty, which the user types into the window and [`about_to_wait`] feeds
    /// through ([`pump_connect`](Self::pump_connect)). When it exits the connect
    /// finishes over the now-open ControlMaster ([`finish_connect`]).
    ///
    /// [`about_to_wait`]: App::about_to_wait
    /// [`finish_connect`]: App::finish_connect
    fn connect_ssh_window(&mut self, wid: WindowId, spec: ConnectionSpec) {
        // Mark the window's group an ssh group first, so a later adopt's registry
        // save persists the connection (sessions in it inherit it).
        if let Some(w) = self.windows.get_mut(&wid) {
            w.root.set_group_connection(Some(spec.clone()));
        }
        let name = self.unique_session_name();

        let remote = match ghost_vt::remote::RemoteSsh::new(spec.clone()) {
            Ok(r) => r,
            Err(e) => return self.connect_fail(wid, format!("could not prepare ssh: {e}")),
        };
        match Self::start_connect(remote, spec, name) {
            Ok(setup) => {
                if let Some(w) = self.windows.get_mut(&wid) {
                    w.connect = Some(setup);
                }
            }
            Err(e) => self.connect_fail(wid, format!("could not start ssh: {e}")),
        }
    }

    /// Begin an ssh connect that lands as a new *session* in this window (Cmd+G).
    /// Identical to [`connect_ssh_window`](Self::connect_ssh_window) except it does
    /// NOT mark the window's group an ssh group: the window keeps its identity and
    /// simply gains a remote tab when the shared connect path
    /// ([`pump_connect`](Self::pump_connect) → [`finish_connect`](Self::finish_connect))
    /// attaches and adopts the session.
    fn connect_ssh_session(&mut self, wid: WindowId, spec: ConnectionSpec) {
        let name = self.unique_session_name();
        let remote = match ghost_vt::remote::RemoteSsh::new(spec.clone()) {
            Ok(r) => r,
            Err(e) => return self.connect_fail(wid, format!("could not prepare ssh: {e}")),
        };
        match Self::start_connect(remote, spec, name) {
            Ok(setup) => {
                if let Some(w) = self.windows.get_mut(&wid) {
                    w.connect = Some(setup);
                }
            }
            Err(e) => self.connect_fail(wid, format!("could not start ssh: {e}")),
        }
    }

    /// Open a PTY and spawn the warm-up `ssh … true` on it (set non-blocking so
    /// the event loop can pump it), returning the in-flight [`ConnectSetup`].
    fn start_connect(
        remote: ghost_vt::remote::RemoteSsh,
        spec: ConnectionSpec,
        name: String,
    ) -> io::Result<ConnectSetup> {
        // A stale/wedged master would derail the warm-up: ssh "disables
        // multiplexing" against a dead socket, authenticating a one-shot connection
        // and leaving NO master for the PTY-less worker probes that follow (fatal
        // on a password-auth host). Clear it first — the same guard
        // `open_master_batch` and `negotiate` apply to their flows — so the warm-up
        // itself opens the fresh master under the user's PTY auth. Cheap on the
        // event loop: a healthy master answers `-O check` in milliseconds and a
        // dead socket refuses instantly.
        remote.reap_wedged_master();
        let (pty, pts) = pty_process::blocking::open().map_err(io::Error::other)?;
        pty.resize(pty_process::Size::new(24, 80))
            .map_err(io::Error::other)?;
        set_nonblocking(&pty)?;
        let argv = remote.warmup_argv();
        let child = pty_process::blocking::Command::new(&argv[0])
            .args(&argv[1..])
            .spawn(pts)
            .map_err(io::Error::other)?;
        Ok(ConnectSetup {
            spec,
            name,
            pty,
            child,
            buf: String::new(),
            asked: false,
        })
    }

    /// Feed the password the user typed into the connect prompt to the in-flight
    /// warm-up ssh over its PTY. Clears the scan buffer and re-arms prompt
    /// detection so a re-prompt (a wrong password) asks again.
    fn connect_feed_password(&mut self, wid: WindowId, password: &str) {
        use std::io::Write as _;
        if let Some(setup) = self.windows.get_mut(&wid).and_then(|w| w.connect.as_mut()) {
            let mut pty = &setup.pty;
            let _ = pty.write_all(password.as_bytes());
            let _ = pty.write_all(b"\n");
            setup.buf.clear();
            setup.asked = false;
        }
    }

    /// Pump a window's in-flight connect once (called each `about_to_wait` pass):
    /// drain the warm-up ssh's PTY, surface a password prompt to the window when
    /// ssh asks, and on the ssh exit hand off to the connect worker (success) or
    /// show the error (failure).
    fn pump_connect(&mut self, wid: WindowId) {
        use std::io::Read as _;
        enum Step {
            Wait,
            Redraw,
            Done,
            Failed(String),
        }
        let step = {
            let Some(w) = self.windows.get_mut(&wid) else {
                return;
            };
            let Some(setup) = w.connect.as_mut() else {
                return;
            };
            let mut redraw = false;
            let mut b = [0u8; 4096];
            loop {
                match (&setup.pty).read(&mut b) {
                    Ok(0) => break,
                    Ok(n) => setup.buf.push_str(&String::from_utf8_lossy(&b[..n])),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
            if !setup.asked
                && let Some(prompt) = password_prompt(&setup.buf)
            {
                setup.asked = true;
                w.root.connect_request_password(prompt);
                redraw = true;
            }
            match setup.child.try_wait() {
                Ok(Some(status)) if status.success() => Step::Done,
                Ok(Some(_)) => Step::Failed(auth_error_message(&setup.buf)),
                Ok(None) if redraw => Step::Redraw,
                Ok(None) => Step::Wait,
                Err(e) => Step::Failed(format!("ssh error: {e}")),
            }
        };
        match step {
            Step::Wait => {}
            Step::Redraw => {
                if let Some(w) = self.windows.get_mut(&wid) {
                    w.request_redraw();
                }
            }
            Step::Failed(msg) => self.connect_fail(wid, msg),
            Step::Done => {
                // Auth succeeded and the shared ControlMaster is open. Run the rest —
                // negotiate, a possible 126 MiB stage, spawn — OFF the event loop so
                // the window stays live; the worker posts `ConnectFinished` back and
                // `finish_connect` attaches on the main thread. The prompt stays in
                // its "Connecting" phase meanwhile.
                let generation = self.windows.get(&wid).map(|w| w.connect_gen).unwrap_or(0);
                if let Some(proxy) = self.proxy.clone()
                    && let Some(setup) = self.windows.get_mut(&wid).and_then(|w| w.connect.take())
                {
                    spawn_connect_worker(
                        proxy,
                        wid,
                        generation,
                        setup.spec.clone(),
                        setup.name.clone(),
                    );
                    // `setup` drops here — the warm-up PTY/child are done with.
                }
            }
        }
    }

    /// Finish an ssh connect on the main thread once its worker reported the
    /// outcome ([`ConnectOutcome`]): attach the window over the transport (the fast,
    /// main-thread part), fall back to a local ssh child, or show the error.
    ///
    /// If the connect was superseded while the worker ran — its window closed, or a
    /// cancel bumped the window's connect generation past `gen` — the outcome is
    /// dropped; and because a `Transport` worker already spawned the detached remote
    /// session, that now-orphaned session is killed so it doesn't linger.
    fn finish_connect(
        &mut self,
        wid: WindowId,
        generation: u64,
        spec: ConnectionSpec,
        name: String,
        outcome: ConnectOutcome,
        event_loop: &dyn Frontend,
    ) {
        let current_gen = self.windows.get(&wid).map(|w| w.connect_gen);
        if !connect_outcome_wanted(current_gen, generation) {
            // Cancelled or closed mid-staging. Only a `Transport` worker got as far
            // as spawning a remote session; kill that orphan. `Fallback` spawns its
            // local child here (skipped by returning), and `Error` created nothing.
            if let ConnectOutcome::Transport { remote_ghost } = outcome {
                self.kill_orphaned_remote(spec, name, remote_ghost);
            }
            return;
        }
        match outcome {
            ConnectOutcome::Transport { remote_ghost } => {
                // Retain the host so the fleet polls its other sessions too.
                self.register_remote(&spec, &remote_ghost);
                // Drive it under the SAME composite id the watcher will discover it
                // by (`<target>␟<name>`), so the window recognizes its own session
                // as this-window in the fleet instead of as a foreign duplicate. The
                // transport still addresses the bare remote name.
                let target = spec.target();
                let local_id = remote_fleet_id(&target, &name);
                self.remote_index
                    .insert(local_id.clone(), (target, name.clone()));
                let Ok(remote) = ghost_vt::remote::RemoteSsh::new(spec) else {
                    return self.connect_fail(wid, "could not open the ssh connection".into());
                };
                if self.attach_ssh_into(wid, &local_id, remote.pipe_command(&remote_ghost, &name)) {
                    if let Some(w) = self.windows.get_mut(&wid) {
                        w.root.end_connect();
                    }
                    self.dispatch(wid, UiEvent::AdoptSession(local_id), event_loop);
                } else {
                    self.connect_fail(wid, "could not attach to the remote session".into());
                }
            }
            // The remote can't host ghost: fall back to a local ssh child (it runs
            // in its own PTY view and prompts for the password there).
            ConnectOutcome::Fallback => {
                if let Some(w) = self.windows.get_mut(&wid) {
                    w.root.end_connect();
                }
                spawn_session(&name, vec![], Some(spec));
                if self.attach_into(wid, &name) {
                    self.dispatch(wid, UiEvent::AdoptSession(name), event_loop);
                }
            }
            ConnectOutcome::Error(msg) => self.connect_fail(wid, msg),
        }
    }

    /// Eagerly reconnect every host a startup restore is waiting on (the targets
    /// queued in `pending_remote_restores`), one background worker each. The full
    /// spec comes from a matching ssh-group `connection`, else is reconstructed
    /// from the target (`user@host`); a per-target spec sidecar would also carry a
    /// custom port/identity for a Cmd+G tab whose host isn't an ssh group — a
    /// deferred refinement. A no-op under a headless frontend (no proxy to post
    /// back on).
    fn reconnect_restored_remotes(&self) {
        let Some(proxy) = self.proxy.clone() else {
            return;
        };
        for target in self.pending_remote_restores.keys() {
            let spec = self
                .groups
                .iter()
                .filter_map(|g| g.connection.clone())
                .find(|c| &c.target() == target)
                .or_else(|| ConnectionSpec::parse_target(target));
            if let Some(spec) = spec {
                spawn_remote_reconnect(proxy.clone(), spec);
            }
        }
    }

    /// A background restore reconnect reached `spec`'s host: register it (starting
    /// its watcher) and attach every remembered session queued for it in
    /// `pending_remote_restores` into its restored window, adopting so the window
    /// shows it. A session gone from the remote just fails to attach (its tile
    /// stays cold); the drain clears the target either way.
    fn finish_remote_reconnect(
        &mut self,
        spec: ConnectionSpec,
        remote_ghost: String,
        event_loop: &dyn Frontend,
    ) {
        self.register_remote(&spec, &remote_ghost);
        let target = spec.target();
        let Some(pending) = self.pending_remote_restores.remove(&target) else {
            return;
        };
        let host = self
            .remotes
            .lock()
            .ok()
            .and_then(|m| m.get(&target).cloned());
        let Some(host) = host else {
            return;
        };
        for PendingRemote {
            wid,
            composite,
            fleet: saved_fleet,
            foreground,
        } in pending
        {
            if !self.windows.contains_key(&wid) {
                continue;
            }
            let Some((_, real)) = composite.split_once(REMOTE_ID_SEP) else {
                continue;
            };
            let real = real.to_string();
            // Index the composite id either way so its tile can route over the
            // transport (the fleet's observe path, or a later take-over dive).
            self.remote_index
                .insert(composite.clone(), (target.clone(), real.clone()));
            // A window SAVED in the fleet overview comes back in it: its tile goes
            // live through the fleet's own observe path (`register_remote` above
            // started the watcher; `reconcile` will `Cmd::Observe` this foreign
            // tile). Do NOT attach+adopt here — adopting dives out of the fleet
            // into the session, and driving without adopting double-feeds the tile
            // (owned pump + observer). Only a single-view window (a lone remote
            // session) drives+foregrounds it. We key on the SAVED mode, not the
            // live one: a remote-only window is always restored into a fleet (it
            // owns no tile to dive into, so F9 can't force it single), so the
            // single-view intent rides in from `pending_remote_restores`; the adopt
            // then dives it out.
            if saved_fleet {
                continue;
            }
            let cmd = host.remote.pipe_command(&host.remote_ghost, &real);
            if !self.attach_ssh_into(wid, &composite, cmd)
                && !self.relaunch_remote_and_attach(wid, &host, &composite, &real)
            {
                // Host reachable but the session is gone AND could not be
                // relaunched — leave the tile cold, as before.
                continue;
            }
            if foreground {
                // The window's saved foreground: adopt it to the front.
                self.dispatch(wid, UiEvent::AdoptSession(composite), event_loop);
            } else {
                // A background member (a lone remote in a window that also drives a
                // local foreground, or another remote): adopt it into the window's
                // warm set, then re-adopt the previous foreground so the reconnect
                // doesn't yank focus off it. Single→single adopts don't animate, so
                // the round-trip is invisible. If the window has no foreground yet
                // (nothing else driven), the adopt simply brings it to the front.
                let keep = self
                    .windows
                    .get(&wid)
                    .and_then(|w| w.root.foreground().cloned());
                self.dispatch(wid, UiEvent::AdoptSession(composite), event_loop);
                if let Some(keep) = keep {
                    self.dispatch(wid, UiEvent::AdoptSession(keep), event_loop);
                }
            }
        }
    }

    /// Abandon a window's in-flight connect and show `msg` on the prompt (Enter
    /// then retries from the host field). Dropping the [`ConnectSetup`] kills the
    /// warm-up ssh.
    fn connect_fail(&mut self, wid: WindowId, msg: String) {
        eprintln!("ghost: ssh connect failed: {msg}");
        if let Some(w) = self.windows.get_mut(&wid) {
            w.connect = None;
            w.root.connect_failed(msg);
            w.request_redraw();
        }
    }

    /// The ssh connection a new session spawned into `wid` inherits: the foreground
    /// session's connection wins, else the window group's own ("ssh group") connection.
    /// `None` ⇒ a plain local `$SHELL`. This is what makes a new terminal follow
    /// the one it branches off onto the same host (see [`inherited_connection`]).
    fn inherited_spawn_connection(&self, wid: WindowId) -> Option<ConnectionSpec> {
        let w = self.windows.get(&wid)?;
        let group = w.root.group_connection().cloned();
        // Clone the foreground id out first so the `&self.windows` borrow ends
        // before `foreground_connection` takes its own `&self`.
        let fg_id = w.root.foreground().cloned();
        let foreground = fg_id
            .as_deref()
            .and_then(|id| self.foreground_connection(id));
        inherited_connection(group.as_ref(), foreground.as_ref())
    }

    /// The ssh connection an owned foreground session `id` carries, if any — read
    /// from stored data, never a live command line. A session driven over the
    /// transport (`<target>␟<real>`) has no local descriptor, so its spec comes
    /// from the live remote host; a local session (including an `ssh` child) reads
    /// its stored descriptor.
    fn foreground_connection(&self, id: &str) -> Option<ConnectionSpec> {
        if let Some((target, _)) = id.split_once(REMOTE_ID_SEP) {
            return self
                .remotes
                .lock()
                .ok()?
                .get(target)
                .map(|h| h.remote.spec().clone());
        }
        ghost_vt::descriptor::read(id).and_then(|d| d.connection)
    }

    /// Retain a connected host so the fleet tracks its sessions, and start its
    /// pushed `ghost __watch` stream. Builds a fresh `RemoteSsh` from the spec — its
    /// control-socket path is deterministic, so it shares the ControlMaster the
    /// connect already opened (no re-auth).
    fn register_remote(&mut self, spec: &ConnectionSpec, remote_ghost: &str) {
        let Ok(remote) = ghost_vt::remote::RemoteSsh::new(spec.clone()) else {
            return;
        };
        if let Ok(mut m) = self.remotes.lock() {
            m.insert(
                spec.target(),
                RemoteHost {
                    remote: Arc::new(remote),
                    remote_ghost: remote_ghost.to_string(),
                },
            );
        }
        self.ensure_remote_watcher(&spec.target());
    }

    /// Start a host's pushed `ghost __watch` stream if it isn't already running —
    /// the push that keeps its fleet tiles fresh. A no-op without an event-loop
    /// proxy (a headless test posts `RemoteSessions` itself).
    fn ensure_remote_watcher(&mut self, target: &str) {
        let Some(proxy) = self.proxy.clone() else {
            return;
        };
        if self.remote_watchers.contains_key(target) {
            return;
        }
        let host = self
            .remotes
            .lock()
            .ok()
            .and_then(|m| m.get(target).cloned());
        let Some(host) = host else {
            return;
        };
        let watcher = start_remote_watcher(target.to_string(), host, proxy);
        self.remote_watchers.insert(target.to_string(), watcher);
    }

    /// Rebuild the namespaced-id → `(target, real id)` index from the current
    /// remote listings, so a take-over of a remote tile reaches the right session.
    fn rebuild_remote_index(&mut self) {
        self.remote_index.clear();
        for (target, infos) in &self.remote_infos {
            let prefix = format!("{target}{REMOTE_ID_SEP}");
            for i in infos {
                if let Some(real) = i.name.strip_prefix(&prefix) {
                    self.remote_index
                        .insert(i.name.clone(), (target.clone(), real.to_string()));
                }
            }
        }
        // Keep every remote session a window is actively driving, even one its
        // host hasn't listed yet (a fresh connect/spawn indexes it before the
        // watcher reports it). Without this, a rebuild triggered by another host's
        // push would drop the driven id and its rename/kill/observe would misroute
        // to the local path. The composite id carries its own (target, real).
        for w in self.windows.values() {
            for id in w.sessions.keys() {
                if let Some((target, real)) = id.split_once(REMOTE_ID_SEP) {
                    self.remote_index
                        .entry(id.clone())
                        .or_insert_with(|| (target.to_string(), real.to_string()));
                }
            }
        }
    }

    /// Take over a remote session (a fleet tile on a connected host) into window
    /// `wid`: attach it over the host's transport — reusing the open master — and
    /// switch the window to its single view. `id` is the fleet-namespaced id;
    /// `real` is the session's id on the host.
    fn take_over_remote(
        &mut self,
        wid: WindowId,
        id: &str,
        target: &str,
        real: &str,
        event_loop: &dyn Frontend,
    ) {
        let held = self
            .windows
            .get(&wid)
            .is_some_and(|w| w.sessions.contains_key(id));
        if held {
            self.dispatch(wid, UiEvent::AdoptSession(id.to_string()), event_loop);
            return;
        }
        let host = self
            .remotes
            .lock()
            .ok()
            .and_then(|m| m.get(target).cloned());
        let Some(host) = host else {
            eprintln!("ghost: no live connection to {target} to open its session");
            return;
        };
        let cmd = host.remote.pipe_command(&host.remote_ghost, real);
        if self.attach_ssh_into(wid, id, cmd) {
            self.dispatch(wid, UiEvent::AdoptSession(id.to_string()), event_loop);
        }
    }

    /// Create a NEW session on a connected remote host (inheritance-over-remote):
    /// `ghost new -d <name>` over the transport, then attach it as this-window
    /// under the fleet-namespaced id — the same shape as a fresh connect or a
    /// take-over, so the new session is a full remote ghost session rather than a
    /// local `ssh` child. `target` must be a currently-connected host.
    fn spawn_remote_session(
        &mut self,
        wid: WindowId,
        target: &str,
        name: &str,
        event_loop: &dyn Frontend,
    ) {
        let host = self
            .remotes
            .lock()
            .ok()
            .and_then(|m| m.get(target).cloned());
        let Some(host) = host else {
            eprintln!("ghost: no live connection to {target} to open a session on");
            return;
        };
        // Recovery (a dead remote tile's Recreate) reaches here after a reboot may
        // have wedged the shared master; clear it so the spawn opens a fresh
        // connection. A no-op on the healthy interactive new-session path.
        host.remote.reap_wedged_master();
        if let Err(e) = host.remote.spawn_host(&host.remote_ghost, name) {
            eprintln!("ghost: could not open a session on {target}: {e}");
            return;
        }
        // Drive it under the composite id the watcher will discover it by, so the
        // window owns its own new session in the fleet (the transport uses the bare
        // name); see [`finish_connect`](Self::finish_connect).
        let local_id = remote_fleet_id(target, name);
        self.remote_index
            .insert(local_id.clone(), (target.to_string(), name.to_string()));
        let cmd = host.remote.pipe_command(&host.remote_ghost, name);
        if self.attach_ssh_into(wid, &local_id, cmd) {
            self.dispatch(wid, UiEvent::AdoptSession(local_id), event_loop);
        } else {
            self.remote_index.remove(&local_id);
            eprintln!("ghost: opened a session on {target} but could not attach to it");
        }
    }

    /// Open a read-only observation of remote session `real` on `target` over its
    /// host's transport (a live fleet preview). `None` if the host isn't connected
    /// or the observe channel couldn't open.
    fn observe_remote(&self, target: &str, real: &str) -> Option<Subscriber> {
        let host = self
            .remotes
            .lock()
            .ok()
            .and_then(|m| m.get(target).cloned())?;
        let cmd = host.remote.pipe_command(&host.remote_ghost, real);
        Subscriber::observe_ssh(cmd).ok()
    }

    /// Kill remote session `real` on `target` over its host's transport, off the
    /// event loop (one ssh command over the open master). The watcher reflects the
    /// removal within a poll.
    fn spawn_remote_kill(&self, target: &str, real: &str) {
        let Some(host) = self
            .remotes
            .lock()
            .ok()
            .and_then(|m| m.get(target).cloned())
        else {
            // No live transport to the host — the kill can't be delivered. Say so
            // (like the rename twin below) rather than dropping it silently; the
            // fleet has already forgotten the tile either way.
            eprintln!("ghost: no live connection to {target} to kill its session");
            return;
        };
        let real = real.to_string();
        std::thread::spawn(move || {
            if let Err(e) = host.remote.kill_session(&host.remote_ghost, &real) {
                eprintln!("ghost: remote kill failed: {e}");
            }
        });
    }

    /// Kill a detached remote session that a connect worker spawned but that we no
    /// longer want — the connect was cancelled, or its window closed, while staging
    /// ran (see [`finish_connect`](Self::finish_connect)). Off the event loop and
    /// best-effort; a fresh [`RemoteSsh`] reuses the spec's still-open ControlMaster
    /// (no re-auth), so this works even though the host was never registered.
    fn kill_orphaned_remote(&self, spec: ConnectionSpec, name: String, remote_ghost: String) {
        std::thread::spawn(move || match ghost_vt::remote::RemoteSsh::new(spec) {
            Ok(remote) => {
                if let Err(e) = remote.kill_session(&remote_ghost, &name) {
                    eprintln!("ghost: could not kill orphaned remote session '{name}': {e}");
                }
            }
            Err(e) => {
                eprintln!("ghost: could not open ssh to kill orphaned session '{name}': {e}")
            }
        });
    }

    /// Rename remote session `real` on `target` to `new` over its host's transport,
    /// off the event loop. The watcher reflects the new label on the next push.
    fn spawn_remote_rename(&self, target: &str, real: &str, new: &str) {
        let Some(host) = self
            .remotes
            .lock()
            .ok()
            .and_then(|m| m.get(target).cloned())
        else {
            // No live transport to the host — the rename can't be delivered. Say so
            // (the fleet's optimistic label will revert once the timeout lapses)
            // rather than dropping it silently.
            eprintln!("ghost: no live connection to {target} to rename its session");
            return;
        };
        let (real, new) = (real.to_string(), new.to_string());
        std::thread::spawn(move || {
            if let Err(e) = host.remote.rename_session(&host.remote_ghost, &real, &new) {
                eprintln!("ghost: remote rename failed: {e}");
            }
        });
    }

    /// Handle one interactive resize step for window `wid`. An isolated resize
    /// (maximize / snap / un-maximize / a drag's first grab) is applied immediately
    /// and crisply; a rapid drag stream captures the crisp scene once, then
    /// reconfigures the surface and stretch-blits that snapshot for cheap feedback,
    /// deferring the expensive real resize (relayout/reflow/PTY-resize/re-raster) to
    /// `about_to_wait`, which commits it once the drag settles.
    fn resize_step(
        &mut self,
        wid: WindowId,
        w_px: u32,
        h_px: u32,
        scale: f64,
        event_loop: &dyn Frontend,
    ) {
        let now_ms = self.now_ms();
        let step = {
            let Some(w) = self.windows.get_mut(&wid) else {
                return;
            };
            let step = w.resize.note(now_ms, w_px, h_px, scale);
            // A headless window has no surface to resize; the model still re-grids
            // below via the dispatched `Resize`.
            if let Some(gfx) = w.gfx.as_mut() {
                // A ScaleFactorChanged also routes here — keep the frost grain sized.
                gfx.renderer.set_scale_factor(scale as f32);
                match step {
                    // Isolated resize (maximize / snap / un-maximize / a drag's first
                    // grab): drop any snapshot and resize the surface now; the real
                    // relayout is dispatched below, crisply.
                    resize::Step::CommitNow((cw, ch, _)) => {
                        gfx.renderer.clear_snapshot();
                        gfx.resize(cw, ch);
                    }
                    // A drag is streaming: capture the last crisp frame once, then
                    // stretch-blit it cheaply until the gesture settles (the real
                    // resize is committed from `about_to_wait`).
                    resize::Step::Defer => {
                        if !gfx.renderer.has_snapshot() {
                            let scene = w.root.view();
                            let font_px = size_px() * w.root.render_scale();
                            gfx.renderer.capture_snapshot(&scene, gfx.fonts, font_px);
                        }
                        gfx.resize(w_px, h_px);
                        gfx.blit_snapshot();
                    }
                }
            }
            step
        };
        if let resize::Step::CommitNow((cw, ch, cs)) = step {
            self.dispatch(
                wid,
                UiEvent::Resize {
                    w_px: cw,
                    h_px: ch,
                    scale: cs,
                },
                event_loop,
            );
        }
    }

    /// Open a new window in the fleet overview (owning no session yet), carrying
    /// `group` as its identity and opening at `size` cells (its configured default
    /// when `None`). The user spawns or takes over a session from there.
    fn open_fleet_window(
        &mut self,
        event_loop: &dyn Frontend,
        group: ghost_ui_core::Group,
        size: Option<(u16, u16)>,
    ) -> WindowId {
        let cfg = config::UiConfig::load();
        let (req_cols, req_rows) = size.unwrap_or((cfg.columns(), cfg.rows()));
        let NewWindow {
            id: wid,
            gfx,
            size_px: (w, h),
            scale,
        } = event_loop.open_window(WindowSpec {
            theme: cfg.theme(),
            option_as_meta: cfg.option_as_meta(),
            blur: cfg.blur(),
            cols: req_cols,
            rows: req_rows,
            pad: cfg.padding(),
        });
        let (mut root, init) = RootModel::fleet(metrics(), (w, h), scale as f32);
        root.set_theme(theme_colors(&cfg.theme()));
        root.set_padding(cfg.padding());
        // A fleet window owns nothing yet, so reclaiming a group here just adopts
        // its identity — the members come from the loaded registry below.
        let claims = root.set_my_group(group);
        debug_assert!(claims.is_empty());
        apply_anim_ms(&mut root);
        self.windows.insert(
            wid,
            WindowState {
                gfx,
                root,
                sessions: HashMap::new(),
                observers: HashMap::new(),
                dead_fed: HashSet::new(),
                mods: ModifiersState::empty(),
                pointer_pos: PointPx { x: 0.0, y: 0.0 },
                next_tick: None,
                last_click: None,
                click_count: 0,
                pacer: pacer::FramePacer::new(pacer::FRAME_BUDGET_MS),
                render_trace: rendertrace::RenderTrace::new(),
                resize: resize::ResizeCoalescer::new(
                    resize::SETTLE_MS,
                    resize::MAX_MS,
                    resize::DRAG_GAP_MS,
                ),
                stats: framestats::FrameStats::from_env(),
                needs_surface_sync: true,
                presented_ok: false,
                occluded: false,
                connect: None,
                connect_gen: 0,
            },
        );
        // Size the model to the surface, then run the fleet's initial enumeration.
        self.dispatch(
            wid,
            UiEvent::Resize {
                w_px: w,
                h_px: h,
                scale,
            },
            event_loop,
        );
        self.exec(wid, init, event_loop);
        self.dispatch(wid, UiEvent::SetZoom(cfg.zoom()), event_loop);
        // Seed the persisted groups so the overview shows them from the start.
        let groups = self.groups.clone();
        self.dispatch(wid, UiEvent::GroupsLoaded(groups), event_loop);
        wid
    }

    /// Open a new window showing the "connect to a host" prompt (Cmd+S /
    /// Ctrl+Shift+S): a fresh fleet window on its own group, flipped into the
    /// connect state so it captures a `[user@]host` and, on submit, becomes an
    /// ssh window (see the `Cmd::ConnectSshWindow` handler).
    fn open_connect_window(&mut self, event_loop: &dyn Frontend) {
        let group = self.mint_group();
        let wid = self.open_fleet_window(event_loop, group, None);
        if let Some(w) = self.windows.get_mut(&wid) {
            w.root.begin_connect();
            w.request_redraw();
        }
    }

    /// Open the "connect to a host" prompt in *this* window (Cmd+G / Ctrl+Shift+G /
    /// Alt+G): no new window — the current window shows the prompt and, on submit,
    /// adopts the remote session as an additional tab (see `Cmd::ConnectSshSession`).
    fn open_connect_session(&mut self, wid: WindowId) {
        if let Some(w) = self.windows.get_mut(&wid) {
            w.root.begin_connect_session();
            w.request_redraw();
        }
    }

    /// Open a new window that behaves exactly like a fresh launch (File > New Window
    /// / Cmd-N): reconnect through the fleet when any session is detached or a group
    /// remembers a dead one, otherwise spawn a fresh session and show it as a single
    /// view. Runs in this same process, so the new window shares the clipboard,
    /// clock, and menu with the others.
    fn open_launch_window(&mut self, event_loop: &dyn Frontend) {
        let sessions = session::list().unwrap_or_default();
        match new_window_choice(&sessions, &self.groups) {
            StartupChoice::Fleet => {
                let group = self.mint_group();
                self.open_fleet_window(event_loop, group, None);
            }
            StartupChoice::Spawn => {
                let name = self.unique_session_name();
                // A fresh window starts a local session (no foreground to inherit
                // an ssh connection from; a P5 ssh group would set one here).
                spawn_session(&name, vec![], None);
                let group = self.mint_group();
                self.open_single_window(event_loop, &name, group, None);
            }
            // new_window_choice never asks to attach a specific session, but keep the
            // match exhaustive: an explicit name would open that session's single view.
            StartupChoice::Attach(name) => {
                let group = self.mint_group();
                self.open_single_window(event_loop, &name, group, None);
            }
        }
    }

    /// Remove a window; dropping its [`WindowState`] drops its session clients,
    /// which detaches them (the hosts keep the sessions running for reattach) —
    /// the "close = detach" default.
    fn close_window(&mut self, wid: WindowId) {
        self.windows.remove(&wid);
        // Drop any remote reconnects still queued for this window: it is gone, so a
        // late host reconnect has nowhere to land (`finish_remote_reconnect` would
        // skip it), and a host that never returns would leak the entry forever.
        for queued in self.pending_remote_restores.values_mut() {
            queued.retain(|p| p.wid != wid);
        }
        self.pending_remote_restores.retain(|_, q| !q.is_empty());
        // Cancel any reconnect probes for this window (stop their threads) — the tile
        // they'd reattach into is gone.
        self.reconnecting.retain(|(w, _), stop| {
            if *w == wid {
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
                false
            } else {
                true
            }
        });
        // A closed window drops out of the restorable set.
        self.workspace_dirty = true;
        // It may have been the last window referencing a remote host; stop polling
        // (and drop the tiles for) any host nothing points at now.
        self.prune_remotes();
    }

    /// The set of remote targets still referenced by a live window — the window is an
    /// ssh group for it, it drives a session on it, or a group still remembers a
    /// (possibly cold) session on it. The last keeps a host's watcher retrying across
    /// an outage, so a dropped connection reconnects and its remembered members go
    /// live again rather than being pruned and orphaned.
    fn in_use_targets(&self) -> HashSet<String> {
        let mut targets = HashSet::new();
        for w in self.windows.values() {
            if let Some(spec) = w.root.group_connection() {
                targets.insert(spec.target());
            }
            // A driven remote session's id is `<target>␟<real>`; read the target
            // straight off it (not via the index, which a poll failure can clear).
            for name in w.sessions.keys() {
                if let Some((target, _)) = remote_id_parts(name) {
                    targets.insert(target.to_string());
                }
            }
        }
        // A group still remembering a remote member keeps its host in use even when no
        // window drives it right now (the session went cold during an outage).
        for g in &self.groups {
            for m in &g.members {
                if let Some((target, _)) = remote_id_parts(m) {
                    targets.insert(target.to_string());
                }
            }
        }
        // A reconnecting session's client is dropped while it holds, so it no longer
        // appears in `w.sessions` — but its host must stay so the probe can reach it
        // and `finish_reattach` can find it. Keep it in use until the hold clears.
        for (_, name) in self.reconnecting.keys() {
            if let Some((target, _)) = remote_id_parts(name) {
                targets.insert(target.to_string());
            }
        }
        targets
    }

    /// Drop remote hosts (and their cached listings) that no live window
    /// references any more, so the watcher stops listing them and their fleet tiles
    /// disappear.
    fn prune_remotes(&mut self) {
        let in_use = self.in_use_targets();
        if let Ok(mut m) = self.remotes.lock() {
            m.retain(|t, _| in_use.contains(t));
        }
        self.remote_infos.retain(|t, _| in_use.contains(t));
        // Dropping a watcher stops its thread and kills its `ghost __watch` ssh.
        self.remote_watchers.retain(|t, _| in_use.contains(t));
        self.rebuild_remote_index();
    }

    /// The single quit path: record the open windows, then leave the event loop.
    /// Every user-initiated quit (Cmd/Ctrl+Q, closing the last window) funnels
    /// through here so the workspace is flushed before exit.
    fn shutdown(&mut self, event_loop: &dyn Frontend) {
        self.save_workspace();
        event_loop.exit();
    }

    /// Rebuild the workspace snapshot from the live windows and persist it if it
    /// changed. Idempotent and cheap (a dirty flag flushes it once per loop
    /// wake). Skips bench runs, whose synthetic sessions must never overwrite
    /// the real workspace.
    fn save_workspace(&mut self) {
        self.workspace_dirty = false;
        if self.bench.is_some() {
            return;
        }
        let mut records: Vec<ghost_ui_core::WindowRecord> = self
            .windows
            .values()
            .map(|w| w.root.window_record())
            .collect();
        // Stable order so an unchanged workspace serialises identically and the
        // write-on-change guard holds.
        records.sort_by(|a, b| a.group_id.cmp(&b.group_id));
        if records != self.last_workspace {
            windows::save(&records);
            self.last_workspace = records;
        }
    }

    /// The window a "current window" menu action should target: the last-focused
    /// one if it still exists, otherwise any live window (so an action still lands
    /// after the focused window closed). `None` only when no window is open.
    fn focused_window(&self) -> Option<WindowId> {
        self.focused
            .filter(|w| self.windows.contains_key(w))
            .or_else(|| self.windows.keys().next().copied())
    }

    /// Cycle focus among the app's windows (Cmd-` forward, Cmd-Shift-` backward),
    /// in a stable [`WindowId`] order so the cycle is deterministic. A lone window
    /// has nothing to cycle to. On macOS this is a fallback for when the system's
    /// own "cycle windows" shortcut is disabled — when it's on, AppKit consumes
    /// the key first and this never runs, so the two never double up.
    fn cycle_windows(&self, current: WindowId, forward: bool) {
        let mut ids: Vec<WindowId> = self.windows.keys().copied().collect();
        ids.sort();
        let cur = ids.iter().position(|w| *w == current);
        if let Some(next) = cycle_index(ids.len(), cur, forward)
            && let Some(gfx) = self.windows.get(&ids[next]).and_then(|w| w.gfx.as_ref())
        {
            gfx.window.focus_window();
        }
    }
}

impl App {
    /// Open a single-session view attached to `name`, carrying `group` as the
    /// window's identity and opening at `size` cells (its configured default when
    /// `None`; a restored window passes the grid it was last sized to). Returns
    /// the new window's id, or `None` if the attach fails.
    fn open_single_window(
        &mut self,
        event_loop: &dyn Frontend,
        name: &str,
        group: ghost_ui_core::Group,
        size: Option<(u16, u16)>,
    ) -> Option<WindowId> {
        let cfg = config::UiConfig::load();
        let (req_cols, req_rows) = size.unwrap_or((cfg.columns(), cfg.rows()));
        let NewWindow {
            id: wid,
            gfx,
            size_px: (w, h),
            scale,
        } = event_loop.open_window(WindowSpec {
            theme: cfg.theme(),
            option_as_meta: cfg.option_as_meta(),
            blur: cfg.blur(),
            cols: req_cols,
            rows: req_rows,
            pad: cfg.padding(),
        });
        let (cols, rows) = grid_from_pixels(w, h, scale as f32, cfg.padding());
        // The window's group identity — reclaimed for a restored window, freshly
        // minted otherwise — so the very first attach reports the right group.
        let identity = ghost_ui_core::group::window_identity(&group.id);
        let session = match attach(name, cols, rows, &identity) {
            Ok(session) => session,
            Err(e) => {
                eprintln!("could not attach to session '{name}': {e}");
                return None;
            }
        };
        let mut model = TerminalModel::new(name.to_string(), cols, rows, metrics());
        // Seed the display name so a labeled session titles the window with its
        // label from the first frame (best-effort; a reconcile would fix it too).
        if let Ok(sessions) = session::list()
            && let Some(info) = sessions.iter().find(|s| s.name == name)
        {
            model.set_display_name(info.display_name.clone());
        }
        // Title the window with the session up front (its label or name until the
        // app sets an OSC title), so the initial view follows the foreground like
        // every switch does — not a static "ghost".
        if let Some(g) = &gfx {
            g.window.set_title(&model.title());
        }
        let mut root = RootModel::single(model, metrics(), (w, h));
        root.set_theme(theme_colors(&cfg.theme()));
        root.set_padding(cfg.padding());
        // Seed the persisted registry BEFORE the group claim, so the claim's
        // save extends it rather than clobbering it with just this window.
        root.update(UiEvent::GroupsLoaded(self.groups.clone()));
        let claims = root.set_my_group(group);
        apply_anim_ms(&mut root);
        let mut sessions = HashMap::new();
        sessions.insert(name.to_string(), session);
        self.windows.insert(
            wid,
            WindowState {
                gfx,
                root,
                sessions,
                observers: HashMap::new(),
                dead_fed: HashSet::new(),
                mods: ModifiersState::empty(),
                pointer_pos: PointPx { x: 0.0, y: 0.0 },
                next_tick: None,
                last_click: None,
                click_count: 0,
                pacer: pacer::FramePacer::new(pacer::FRAME_BUDGET_MS),
                render_trace: rendertrace::RenderTrace::new(),
                resize: resize::ResizeCoalescer::new(
                    resize::SETTLE_MS,
                    resize::MAX_MS,
                    resize::DRAG_GAP_MS,
                ),
                stats: framestats::FrameStats::from_env(),
                needs_surface_sync: true,
                presented_ok: false,
                occluded: false,
                connect: None,
                connect_gen: 0,
            },
        );
        // Sync the model's viewport to the real surface size *and* device scale
        // before the first paint — this drives the NDC mapping, the scissor
        // clamp, and the cell metrics, and its `Cmd::Redraw` requests that paint.
        // (No earlier `request_redraw`: it would race a frame at the default 1x
        // scale against glyphs the renderer rasterizes at `size_px() * scale`.)
        self.dispatch(
            wid,
            UiEvent::Resize {
                w_px: w,
                h_px: h,
                scale,
            },
            event_loop,
        );
        // Apply the persisted zoom now that the viewport is known, so it re-grids
        // against the real surface size (the model clamps to its bounds).
        self.dispatch(wid, UiEvent::SetZoom(cfg.zoom()), event_loop);
        // Persist (and broadcast) the initial session joining this window's
        // group — the registry itself was seeded before the claim.
        self.exec(wid, claims, event_loop);
        Some(wid)
    }

    /// Recreate the saved workspace: one window per restorable record. Falls back
    /// to a normal launch if nothing could be restored (every group was pruned, or
    /// an empty workspace slipped through), so the app never comes up windowless.
    fn restore_workspace(
        &mut self,
        event_loop: &dyn Frontend,
        records: Vec<ghost_ui_core::WindowRecord>,
    ) {
        let sessions = session::list().unwrap_or_default();
        for plan in restore_plan(&records, &sessions, &self.groups) {
            self.restore_window(event_loop, plan);
        }
        // Every restored window has queued its remote members; reconnect their
        // hosts now so the sessions come back live and re-adopt into their windows.
        self.reconnect_restored_remotes();
        if self.windows.is_empty() {
            self.open_launch_window(event_loop);
        }
    }

    /// Recreate one window from its plan: open it on the group it reclaims, at the
    /// grid it was sized to; relaunch dead members (shell + seeded recording) then
    /// attach every member, adopting them in order so the foreground (ordered last)
    /// ends up focused; and restore the fleet overview if that is how it was left.
    fn restore_window(&mut self, event_loop: &dyn Frontend, plan: WindowPlan) {
        let WindowPlan {
            group,
            cols,
            rows,
            fleet,
            foreground,
            locals,
            remotes,
        } = plan;
        let size = Some((cols, rows));
        let had_locals = !locals.is_empty();
        let mut locals = locals.into_iter();
        // Open on the first LOCAL member (a single view); a window with no local
        // member (remote-only) comes back as a fleet on its group — its remote
        // members come alive once their host reconnects. Clone the group for the
        // first attach so a failure still falls back to a fleet rather than losing
        // the group's identity.
        let wid = match locals.next() {
            Some(first) => {
                if first.dead {
                    spawn_dead(&first.id);
                }
                match self.open_single_window(event_loop, &first.id, group.clone(), size) {
                    Some(wid) => {
                        for m in locals {
                            if m.dead {
                                spawn_dead(&m.id);
                            }
                            if self.attach_into(wid, &m.id) {
                                self.dispatch(wid, UiEvent::AdoptSession(m.id), event_loop);
                            }
                        }
                        wid
                    }
                    None => self.open_fleet_window(event_loop, group, size),
                }
            }
            None => self.open_fleet_window(event_loop, group, size),
        };
        // Queue remote members to attach once their host reconnects (kicked by
        // `reconnect_restored_remotes`, drained by `finish_remote_reconnect`).
        for id in remotes {
            let Some((target, _)) = id.split_once(REMOTE_ID_SEP) else {
                continue;
            };
            let is_foreground = foreground.as_deref() == Some(id.as_str());
            self.pending_remote_restores
                .entry(target.to_string())
                .or_default()
                .push(PendingRemote {
                    wid,
                    composite: id,
                    fleet,
                    foreground: is_foreground,
                });
        }
        // End in the overview iff the window was left in it — but only for a window
        // that opened on a LOCAL member. That branch opens a single view, so F9
        // (a toggle) reaches the fleet, and the owned tile makes it dive back. A
        // remote-only window has no owned tile: it opens as a fleet and F9 can't
        // dive it, so its final view is decided when its remote reconnects — a
        // saved-fleet one stays put, a saved-single one is driven out to single by
        // `finish_remote_reconnect` (which carries the saved mode). The window is
        // off-screen, so no transient view shows.
        if had_locals {
            let in_fleet_now = self.windows.get(&wid).is_some_and(|w| w.root.is_fleet());
            if fleet != in_fleet_now {
                self.dispatch(
                    wid,
                    UiEvent::Key {
                        key: Key::Named(NamedKey::F9),
                        mods: Mods::NONE,
                        kind: KeyEventKind::Press,
                        alts: None,
                    },
                    event_loop,
                );
            }
        }
    }
}

impl App {
    /// The `user_event` handler, over the [`Frontend`] seam so a headless test can
    /// inject a `UserEvent` directly: a native menu selection (turned back into the
    /// effect a keystroke would have produced, keeping the pure core the single
    /// source of truth — see [`menu::menu_intent`]), a remote host's latest listing
    /// from the watcher, or a connect worker's result.
    fn on_user_event(&mut self, fe: &dyn Frontend, event: UserEvent) {
        let action = match event {
            UserEvent::Menu(action) => action,
            // The watcher thread delivered a remote host's latest listing: stash it
            // and hint a re-enumeration so the fleet merges it in.
            UserEvent::RemoteSessions { target, infos } => {
                self.remote_infos.insert(target, infos);
                self.rebuild_remote_index();
                self.sessions_changed
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                return;
            }
            // The connect worker finished (negotiate/stage/spawn ran off-loop):
            // attach the window over the result on the main thread.
            UserEvent::ConnectFinished {
                wid,
                generation,
                spec,
                name,
                outcome,
            } => {
                self.finish_connect(wid, generation, spec, name, outcome, fe);
                return;
            }
            // Staging byte-progress from the connect worker: update the bar.
            UserEvent::ConnectProgress { wid, sent, total } => {
                if let Some(w) = self.windows.get_mut(&wid) {
                    w.root.connect_progress(sent, total);
                    w.request_redraw();
                }
                return;
            }
            // A second launch forwarded a new-window request to us (the owner):
            // open one (exactly like File > New Window) and bring the app forward,
            // so the new window lands in front even if we were buried.
            UserEvent::OpenWindow => {
                self.open_launch_window(fe);
                menu::activate();
                return;
            }
            // A startup restore reconnect reached a host: register it and re-adopt
            // its remembered sessions into their restored windows.
            UserEvent::RemoteReconnected { spec, remote_ghost } => {
                self.finish_remote_reconnect(spec, remote_ghost, fe);
                return;
            }
            // A dropped remote session's host is back: re-attach it at the current
            // grid and clear the reconnecting hold.
            UserEvent::RemoteReattachReady { wid, name } => {
                self.finish_reattach(wid, name, fe);
                return;
            }
            // The host is back but the session is gone (rebooted): end the hold.
            UserEvent::RemoteSessionGone { wid, name } => {
                self.end_reconnect_gone(wid, name, fe);
                return;
            }
        };
        match menu::menu_intent(action) {
            // Opening a window needs no focused target — it always works.
            MenuIntent::NewWindow => self.open_launch_window(fe),
            MenuIntent::FocusedCmd(cmd) => {
                if let Some(wid) = self.focused_window() {
                    self.exec(wid, vec![cmd], fe);
                }
            }
            MenuIntent::FocusedKey(key, mods) => {
                if let Some(wid) = self.focused_window() {
                    self.dispatch(
                        wid,
                        UiEvent::Key {
                            key,
                            mods,
                            kind: KeyEventKind::Press,
                            alts: None,
                        },
                        fe,
                    );
                }
            }
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    /// A native menu selection posted from AppKit's main thread — delegated to
    /// [`App::on_user_event`] over the production frontend.
    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        self.on_user_event(&WinitFrontend { event_loop }, event);
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let fe = WinitFrontend { event_loop };
        if !self.windows.is_empty() {
            return;
        }
        // Consumed once (this guard keeps `resumed` from re-running); the
        // placeholder is never used.
        match std::mem::replace(&mut self.startup, Startup::Fleet) {
            Startup::Restore(records) => self.restore_workspace(&fe, records),
            Startup::Fleet => {
                let group = self.mint_group();
                self.open_fleet_window(&fe, group, None);
            }
            Startup::Single(name) => {
                let group = self.mint_group();
                if self.open_single_window(&fe, &name, group, None).is_none() {
                    fe.exit();
                    return;
                }
            }
        }
        // Install the native macOS menu bar once the app is running (it appends
        // ghost's File / Edit / View / Window submenus to the App submenu winit
        // set up in applicationDidFinishLaunching).
        #[cfg(target_os = "macos")]
        if let Some(proxy) = self.proxy.clone() {
            menu::install(proxy);
        }
        // Bench mode: populate the fleet and load every preview before any animation.
        if self.bench.is_some()
            && let Some(wid) = self.windows.keys().next().copied()
        {
            for ev in self.bench.as_ref().expect("bench present").setup_events() {
                self.dispatch(wid, ev, &fe);
            }
        }
        fe.set_control_flow(ControlFlow::WaitUntil(Instant::now() + POLL));
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let fe = WinitFrontend { event_loop };
        // Flush the workspace snapshot once per wake if a handled event or a
        // window open/close marked it dirty (write-on-change guards the disk).
        if self.workspace_dirty {
            self.save_workspace();
        }
        // Pump each window's own session clients and route the output back to
        // that window (a window only holds clients for sessions it's showing).
        let mut pumped: Vec<(WindowId, String, Vec<u8>, bool)> = Vec::new();
        let mut dropped: Vec<(WindowId, String, Vec<u8>)> = Vec::new();
        for (wid, w) in self.windows.iter_mut() {
            let mut dead = Vec::new();
            for (name, s) in w.sessions.iter_mut() {
                let (bytes, end) = pump(s, 32);
                // A REMOTE session whose transport dropped is held and reconnected,
                // not torn down — its session may still be alive on the far side. A
                // local EOF (the host process is gone) is a genuine end, as before.
                if end == PumpEnd::Disconnected && is_remote_id(name) {
                    dropped.push((*wid, name.clone(), bytes));
                    dead.push(name.clone());
                    continue;
                }
                let ended = end.is_end();
                if !bytes.is_empty() || ended {
                    pumped.push((*wid, name.clone(), bytes, ended));
                }
                if ended {
                    dead.push(name.clone());
                }
            }
            // Drop dead clients before dispatch so a stale query-reply is ignored;
            // whether the window itself ends is decided below via `root.ended()`.
            for name in dead {
                w.sessions.remove(&name);
            }
        }
        for (wid, name, bytes, ended) in pumped {
            self.dispatch(wid, UiEvent::SessionData { name, bytes, ended }, &fe);
        }
        // A dropped remote session: flush its last bytes, put its tile into the
        // reconnecting hold (frozen + dimmed, not torn down), and start retrying.
        for (wid, name, bytes) in dropped {
            if !bytes.is_empty() {
                let ev = UiEvent::SessionData {
                    name: name.clone(),
                    bytes,
                    ended: false,
                };
                self.dispatch(wid, ev, &fe);
            }
            self.dispatch(
                wid,
                UiEvent::SessionDisconnected { name: name.clone() },
                &fe,
            );
            self.begin_reconnect(wid, name);
        }
        // Pump each window's read-only observers: mirrored output feeds its
        // fleet tiles as `SessionData`, and only the `Resized` event is
        // forwarded (the app-wide subscription already delivers the rest).
        // Within one pump the event/output interleaving is lost; dispatching
        // Resized first is safe because the host follows every re-grid with a
        // resync, which heals any pre-resize bytes fed to the new mirror.
        let mut observed: Vec<(WindowId, UiEvent)> = Vec::new();
        for (wid, w) in self.windows.iter_mut() {
            let mut dead = Vec::new();
            for (name, sub) in w.observers.iter_mut() {
                let p = sub.pump().unwrap_or_default();
                for e in p.events {
                    if matches!(e, ghost_vt::protocol::SessionEvent::Resized { .. }) {
                        observed.push((
                            *wid,
                            UiEvent::SessionPush {
                                name: name.clone(),
                                push: SessionPush::Event(e),
                            },
                        ));
                    }
                }
                if !p.output.is_empty() || p.ended {
                    observed.push((
                        *wid,
                        UiEvent::SessionData {
                            name: name.clone(),
                            bytes: p.output,
                            ended: p.ended,
                        },
                    ));
                }
                if p.ended {
                    dead.push(name.clone());
                }
            }
            for name in dead {
                w.observers.remove(&name);
            }
        }
        for (wid, ev) in observed {
            self.dispatch(wid, ev, &fe);
        }
        // Pump any in-flight ssh connects: drain the warm-up ssh's PTY, surface a
        // password prompt when ssh asks, and finish (or fail) the connect on exit.
        let connecting: Vec<WindowId> = self
            .windows
            .iter()
            .filter(|(_, w)| w.connect.is_some())
            .map(|(id, _)| *id)
            .collect();
        for wid in connecting {
            self.pump_connect(wid);
        }
        // Pushed session state (subscriptions) and set-change hints (the
        // runtime-dir watch), fanned out to every window.
        self.pump_subscriptions(&fe);
        // A changed `ui.toml` (config-dir watch) hot-reloads the live-reloadable
        // settings into every window — the compositor blur, opacity, frost, color
        // scheme, and padding.
        if self
            .config_changed
            .swap(false, std::sync::atomic::Ordering::Relaxed)
        {
            self.reload_config(&config::UiConfig::load(), &fe);
        }
        // Fire any per-window ticks that are now due.
        let now = Instant::now();
        let due: Vec<WindowId> = self
            .windows
            .iter()
            .filter(|(_, w)| w.next_tick.is_some_and(|t| now >= t))
            .map(|(id, _)| *id)
            .collect();
        for wid in due {
            let now_ms = self.now_ms();
            if let Some(w) = self.windows.get_mut(&wid) {
                w.next_tick = None;
                w.render_trace.saw_tick_fired(now_ms);
            }
            self.dispatch(wid, UiEvent::Tick { now_ms }, &fe);
        }
        // Bench mode: advance the scripted animation (after ticks, so `is_animating`
        // reflects this turn's animation state).
        if self.bench.is_some() {
            self.drive_bench(&fe);
        }
        // A session ending never closes its window: the model has already switched
        // to the next attached session (or the fleet), so the window lives on until
        // the user closes it. Windows are removed only on an explicit close.
        // Commit any interactive resize that has settled (drag paused/released) or
        // hit its max refresh interval: drop the stretch-blit snapshot and dispatch
        // the real resize, whose relayout/reflow/PTY-resize/re-raster we deferred
        // while dragging. Its `Cmd::Redraw` then paints the crisp scene.
        let now_ms = self.now_ms();
        let commits: Vec<(WindowId, u32, u32, f64)> = self
            .windows
            .iter_mut()
            .filter_map(|(id, w)| w.resize.poll(now_ms).map(|(cw, ch, cs)| (*id, cw, ch, cs)))
            .collect();
        for (wid, cw, ch, cs) in commits {
            if let Some(gfx) = self.windows.get_mut(&wid).and_then(|w| w.gfx.as_mut()) {
                gfx.renderer.clear_snapshot();
            }
            self.dispatch(
                wid,
                UiEvent::Resize {
                    w_px: cw,
                    h_px: ch,
                    scale: cs,
                },
                &fe,
            );
        }
        // Release any paced repaint that the frame budget now allows. The loop
        // re-enters here every `POLL` (8 ms < the 16 ms budget), so a deferred
        // paint is always re-checked and fires within a frame of becoming due;
        // a keystroke's repaint, handled in this same pass, paints at once.
        for (id, w) in self.windows.iter_mut() {
            if !w.presented_ok {
                // The opening frame hasn't landed yet: a window created mid-run can drop
                // its first present(s) while macOS finishes compositing it (the drawable
                // isn't acquirable, so the present is silently skipped). Keep asking every
                // pass until one lands, rather than trusting the pacer's single request —
                // else the window sits blank (title bar only) until an unrelated event.
                w.request_redraw();
            } else if w.pacer.release(now_ms) {
                w.render_trace.saw_release(now_ms);
                w.request_redraw();
            }
            // Once per pass, fold the foreground gate state and classify. Runs always
            // (not just under the trace flag) so a stale-frame freeze can self-heal in
            // the wild — the fold/verdict is a few subtractions, and the diagnostic dump
            // self-filters through the `trace!` level. The window id separates concurrent
            // windows' tracks in a multi-window log.
            let core = w.root.foreground_trace();
            let has_snapshot = w.gfx.as_ref().is_some_and(|g| g.renderer.has_snapshot());
            let pending = w.pacer.pending();
            let visible = !w.occluded;
            if let Some(report) = w
                .render_trace
                .poll(now_ms, core, pending, has_snapshot, visible)
            {
                tracing::trace!(target: "ghost::render", window = ?id, %report, "foreground render stall");
            }
            // Self-heal: when the watchdog sees a stale-no-present freeze (visible output
            // streaming, but no real present reached the glass — the Clean-over-stale
            // texture staleness), force one full foreground re-present. Rate-limited to
            // one per HEAL_COOLDOWN_MS, so a persistent freeze becomes a one-frame glitch
            // and a false trigger just redraws identical pixels (no flicker). Warn so a
            // recovery leaves a breadcrumb even without the trace flag.
            if w.render_trace.self_heal_due(now_ms) {
                if let Some(gfx) = w.gfx.as_mut() {
                    gfx.force_foreground_repaint();
                }
                w.pacer.request();
                tracing::warn!(
                    target: "ghost::render",
                    window = ?id,
                    "forced a foreground re-present (watchdog: suspected stale frame)"
                );
            }
        }
        fe.set_control_flow(ControlFlow::WaitUntil(Instant::now() + POLL));
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let fe = WinitFrontend { event_loop };
        match event {
            WindowEvent::CloseRequested => {
                // Close = detach: dropping the window drops its session clients
                // (the hosts keep the sessions running). Exit with the last one.
                self.close_window(id);
                if self.windows.is_empty() {
                    self.shutdown(&fe);
                }
            }
            WindowEvent::Resized(size) => {
                // Defer the costly relayout: capture + stretch-blit now, commit the
                // real resize once the drag settles (see `resize_step`).
                let Some(scale) = self
                    .windows
                    .get(&id)
                    .and_then(|w| w.gfx.as_ref())
                    .map(|g| g.window.scale_factor())
                else {
                    return;
                };
                self.resize_step(id, size.width.max(1), size.height.max(1), scale, &fe);
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                // The display's DPI changed (e.g. the window moved to another
                // monitor). Treat it like a resize step against the window's actual
                // new physical size, deferring the re-grid at the new scale.
                let Some(s) = self
                    .windows
                    .get(&id)
                    .and_then(|w| w.gfx.as_ref())
                    .map(|g| g.window.inner_size())
                else {
                    return;
                };
                self.resize_step(id, s.width.max(1), s.height.max(1), scale_factor, &fe);
            }
            WindowEvent::RedrawRequested => {
                let now_ms = self.now_ms();
                let trace_on = tracing::enabled!(target: "ghost::render", tracing::Level::TRACE);
                if let Some(win) = self.windows.get_mut(&id) {
                    if trace_on {
                        win.render_trace.saw_redraw_event(now_ms);
                    }
                    // A headless window has no surface; there is nothing to paint.
                    let Some(gfx) = win.gfx.as_mut() else {
                        return;
                    };
                    // First paint of a window created mid-run: recreate the swapchain
                    // before drawing. The initial configure in `Graphics::new` can run
                    // before the window is on screen, leaving a Metal drawable whose
                    // contents never composite — the window shows only its title bar until
                    // a resize. Reconfiguring to the SAME size here (SurfaceTarget::resize
                    // configures unconditionally, so a fresh swapchain is created and the
                    // cache invalidated) makes the opening frame visible. Same size keeps
                    // the surface matching the model's layout, so no re-grid is needed.
                    if win.needs_surface_sync {
                        win.needs_surface_sync = false;
                        let (w, h) = gfx.size();
                        gfx.resize(w, h);
                    }
                    if gfx.renderer.has_snapshot() {
                        // A resize is in flight: blit the snapshot to the current
                        // surface rather than render a scene whose size no longer
                        // matches it (the model resize is deferred until settle).
                        let landed = gfx.blit_snapshot();
                        // Keep the blits paced during the drag; the commit at settle
                        // dispatches the real resize, whose Redraw re-arms the pacer.
                        // A blit whose acquire failed did NOT land — stay pending and
                        // retry, rather than marking a dropped frame painted (which
                        // would freeze the window on a stale blit until the next event).
                        win.pacer.settle(landed, now_ms);
                    } else {
                        let t_model = Instant::now();
                        let scene = win.root.view();
                        let model = t_model.elapsed();
                        // During a dive/slide, DEFER session surface rasters off the frame
                        // loop: a mid-animation tile that needs a full raster blits the best
                        // cached surface as a placeholder and is warmed one-per-frame below,
                        // so the animation never stalls on a slow session's raster.
                        let animating = win.root.is_animating();
                        gfx.renderer.set_deferring(animating);
                        // Rasterize at the model's render scale (device × zoom) so
                        // glyph size matches the grid the scene was laid out for.
                        let font_px = size_px() * win.root.render_scale();
                        // Keep the IME candidate window pinned to the text cursor.
                        if let Some(a) = win.root.ime_cursor_area() {
                            gfx.window.set_ime_cursor_area(
                                PhysicalPosition::new(a.x, a.y),
                                PhysicalSize::new(a.w, a.h),
                            );
                        }
                        match gfx.render(&scene, font_px) {
                            FrameOutcome::Presented { build, present } => {
                                // A frame landed: the pending repaint is satisfied, and
                                // the first-present retry loop below can stop.
                                win.pacer.painted(now_ms);
                                win.presented_ok = true;
                                // The foreground was just composited: reset its per-session
                                // damage baseline so the next `view` measures change from
                                // here (a Lost frame leaves the pending damage to fold into
                                // the next real present). See `RootModel::mark_presented`.
                                win.root.mark_presented();
                                // Model-side cache line (fleet preview frames) under
                                // `RUST_LOG=ghost::cache=trace`, alongside the renderer's.
                                win.root.emit_cache_trace();
                                // Advance the watchdog's real-present baseline (always, so
                                // the self-heal in `about_to_wait` has an accurate view even
                                // without the trace flag). The kick oracle: a present that
                                // ends an armed stall reports the frozen state it just
                                // recovered — logged only under the trace flag to keep a
                                // normal run quiet.
                                let core = win.root.foreground_trace();
                                let pending = win.pacer.pending();
                                if let Some(report) = win.render_trace.saw_outcome(
                                    rendertrace::Outcome::Presented,
                                    now_ms,
                                    core,
                                    pending,
                                ) && trace_on
                                {
                                    tracing::warn!(
                                        target: "ghost::render",
                                        window = ?id,
                                        %report,
                                        "foreground render stall recovered"
                                    );
                                }
                                // Frame-pacing instrumentation (GHOST_FRAME_STATS): record
                                // this frame and print a summary when a dive ends.
                                if let Some(summary) = win.stats.record(
                                    win.root.is_animating(),
                                    model,
                                    build,
                                    present,
                                    Instant::now(),
                                ) {
                                    eprintln!("{}", summary.report());
                                }
                                // Stream bench: accumulate this bulk-output frame; exit when
                                // the run is complete (a no-op outside `GHOST_BENCH=stream`).
                                if self
                                    .bench
                                    .as_mut()
                                    .is_some_and(|h| h.record_stream_present(build, present))
                                {
                                    fe.exit();
                                }
                            }
                            FrameOutcome::Clean => {
                                // Nothing to draw: what's on screen already matches the
                                // scene, so the pending repaint is satisfied. Record the
                                // Clean (always): it does NOT advance the real-present
                                // baseline, so a Clean loop over a stale frame stays visible
                                // to the self-heal.
                                win.pacer.painted(now_ms);
                                let core = win.root.foreground_trace();
                                let pending = win.pacer.pending();
                                win.render_trace.saw_outcome(
                                    rendertrace::Outcome::Clean,
                                    now_ms,
                                    core,
                                    pending,
                                );
                            }
                            FrameOutcome::Lost => {
                                // The surface wasn't acquirable, so nothing was presented.
                                // Re-arm the repaint so `about_to_wait` retries until a
                                // frame lands — this is what recovers a window whose
                                // redraws the platform dropped while it was occluded.
                                win.pacer.request();
                                let core = win.root.foreground_trace();
                                let pending = win.pacer.pending();
                                win.render_trace.saw_outcome(
                                    rendertrace::Outcome::Lost,
                                    now_ms,
                                    core,
                                    pending,
                                );
                            }
                        }
                        // Warm ONE deferred surface off the just-finished frame's slack, so
                        // the fleet fills in over the animation's frames without any single
                        // frame rasterizing a heavy session inline. The animation's own
                        // ticks drive the redraws that keep draining this.
                        if animating {
                            gfx.renderer.warm_next(gfx.fonts);
                        }
                    }
                }
            }
            WindowEvent::ModifiersChanged(m) => {
                if let Some(w) = self.windows.get_mut(&id) {
                    w.mods = m.state();
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                self.note_input(id);
                let Some(mods_state) = self.windows.get(&id).map(|w| w.mods) else {
                    return;
                };
                // Cmd-` / Cmd-Shift-` cycles the app's windows (the macOS
                // convention). Handled here, not in the pure core: it is
                // cross-window and keys off the physical Backquote so it survives
                // dead-grave layouts. Swallow the whole transition (press, repeat
                // and release) so no literal backtick ever leaks to the child.
                if let Some(forward) = from_winit::window_cycle_dir(event.physical_key, mods_state)
                {
                    if event.state == ElementState::Pressed && !event.repeat {
                        self.cycle_windows(id, forward);
                    }
                    return;
                }
                let key = from_winit::key(&event.logical_key, event.physical_key);
                let mods = from_winit::mods(mods_state);
                let alts = from_winit::alternates(&event, mods_state);
                let kind = match event.state {
                    ElementState::Pressed if event.repeat => KeyEventKind::Repeat,
                    ElementState::Pressed => KeyEventKind::Press,
                    ElementState::Released => KeyEventKind::Release,
                };
                self.dispatch(
                    id,
                    UiEvent::Key {
                        key,
                        mods,
                        kind,
                        alts,
                    },
                    &fe,
                );
            }
            WindowEvent::Ime(Ime::Commit(text)) => {
                self.dispatch(id, UiEvent::Text(text), &fe);
            }
            WindowEvent::Ime(Ime::Preedit(text, _cursor)) => {
                // Track the in-progress composition so the model suppresses the
                // raw keystrokes driving it; an empty string ends it.
                self.dispatch(id, UiEvent::Preedit(text), &fe);
            }
            WindowEvent::Ime(Ime::Disabled) => {
                // Composition aborted (focus lost, IME toggled off): clear it.
                self.dispatch(id, UiEvent::Preedit(String::new()), &fe);
            }
            WindowEvent::Ime(Ime::Enabled) => {}
            WindowEvent::Occluded(occluded) => {
                // While a window is occluded (another Space/virtual desktop, the lock
                // screen) the platform may drop our redraw requests, and macOS App Nap
                // can throttle the poll loop on top. Becoming visible again therefore
                // re-arms a repaint: if content really did change while hidden it
                // paints, and an unchanged scene is a cheap `Clean` skip.
                if let Some(w) = self.windows.get_mut(&id) {
                    // Record it so the render-stall watchdog skips a window that can't
                    // present (its Lost-looping surface is the platform withholding the
                    // drawable, not our repaint bug).
                    w.occluded = occluded;
                    if !occluded {
                        w.pacer.request();
                    }
                }
            }
            WindowEvent::Focused(focused) => {
                // Remember the last-focused window as the target for menu actions;
                // keep the previous one on blur (a stale id is filtered at use).
                if focused {
                    self.focused = Some(id);
                    // Belt and braces for platforms/WMs that don't report occlusion
                    // (see `Occluded` above): regaining focus re-arms a repaint too.
                    if let Some(w) = self.windows.get_mut(&id) {
                        w.pacer.request();
                    }
                }
                self.dispatch(id, UiEvent::Focus(focused), &fe);
            }
            WindowEvent::CursorMoved { position, .. } => {
                let Some((pos, mods)) = self.windows.get_mut(&id).map(|w| {
                    w.pointer_pos = PointPx {
                        x: position.x,
                        y: position.y,
                    };
                    (w.pointer_pos, from_winit::mods(w.mods))
                }) else {
                    return;
                };
                self.dispatch(
                    id,
                    UiEvent::Pointer {
                        phase: PointerPhase::Motion,
                        button: None,
                        pos,
                        mods,
                        wheel_dy: 0.0,
                        clicks: 1,
                    },
                    &fe,
                );
            }
            WindowEvent::MouseInput { state, button, .. } => {
                self.note_input(id);
                if let Some(b) = map_button(button) {
                    let pressed = state == ElementState::Pressed;
                    let phase = if pressed {
                        PointerPhase::Press
                    } else {
                        PointerPhase::Release
                    };
                    let Some((clicks, pos, mods)) = self.windows.get_mut(&id).map(|w| {
                        let clicks = if pressed { w.count_click(b) } else { 1 };
                        (clicks, w.pointer_pos, from_winit::mods(w.mods))
                    }) else {
                        return;
                    };
                    self.dispatch(
                        id,
                        UiEvent::Pointer {
                            phase,
                            button: Some(b),
                            pos,
                            mods,
                            wheel_dy: 0.0,
                            clicks,
                        },
                        &fe,
                    );
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                self.note_input(id);
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64,
                    MouseScrollDelta::PixelDelta(p) => p.y,
                };
                let Some((pos, mods)) = self
                    .windows
                    .get(&id)
                    .map(|w| (w.pointer_pos, from_winit::mods(w.mods)))
                else {
                    return;
                };
                self.dispatch(
                    id,
                    UiEvent::Pointer {
                        phase: PointerPhase::Wheel,
                        button: None,
                        pos,
                        mods,
                        wheel_dy: dy,
                        clicks: 1,
                    },
                    &fe,
                );
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::menu::{ConnectOutcome, UserEvent};
    use super::{
        App, HeadlessFrontend, PendingRemote, REMOTE_ID_SEP, StartupChoice, auth_error_message,
        choose_alpha_mode, choose_surface_format, config, connect_outcome_wanted, home_launch_dir,
        inherited_connection, namespace_remote_infos, new_window_choice, password_prompt,
        remote_spawn_target, respawn_opts, restore_plan, should_restore, startup_choice,
        theme_colors, want_blur,
    };
    use ghost_ui_core::WindowRecord;
    use ghost_vt::connection::ConnectionSpec;
    use ghost_vt::session::SessionInfo;
    use std::collections::HashSet;
    use wgpu::CompositeAlphaMode::{Opaque, PostMultiplied, PreMultiplied};
    use wgpu::TextureFormat::{
        Bgra8Unorm, Bgra8UnormSrgb, Rgb10a2Unorm, Rgba8Unorm, Rgba8UnormSrgb, Rgba16Float,
    };

    /// Run `f` with `$XDG_*` redirected to a throwaway dir, serialized against
    /// other App tests (the env is process-global). So the shell's disk writes
    /// (groups, workspace) never touch the developer's real ghost state.
    fn with_isolated_xdg<T>(f: impl FnOnce() -> T) -> T {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: single-threaded within the lock; no other thread reads the env
        // concurrently (App tests are the only ones that touch XDG, and they hold
        // this same lock).
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", tmp.path());
            std::env::set_var("XDG_DATA_HOME", tmp.path().join("data"));
            std::env::set_var("XDG_CONFIG_HOME", tmp.path().join("config"));
        }
        f()
    }

    #[test]
    fn headless_frontend_opens_a_surfaceless_fleet_window() {
        // The Phase-1 proof: the real App shell runs offscreen. Opening a fleet
        // window through the headless frontend creates a live, surface-less window
        // whose model is in the fleet overview — no GPU, no event loop.
        with_isolated_xdg(|| {
            let mut app = App::headless();
            let fe = HeadlessFrontend::new();
            let group = app.mint_group();
            let wid = app.open_fleet_window(&fe, group, None);

            let win = app.windows.get(&wid).expect("the window was inserted");
            assert!(win.gfx.is_none(), "a headless window carries no surface");
            assert!(win.root.is_fleet(), "it opened in the fleet overview");
            assert!(!fe.exited.get(), "opening a window does not quit the app");
        });
    }

    #[test]
    fn reload_config_reapplies_theme_and_padding_to_every_window() {
        // A config hot-reload fans the new model-side settings out to EVERY open
        // window. (The gfx-side keys — opacity/frost/blur — have no headless seam;
        // this covers the plumbing and the multi-window fan-out, which is the logic
        // worth guarding. Surface/composite behaviour is a ghost-renderer golden.)
        with_isolated_xdg(|| {
            let mut app = App::headless();
            let fe = HeadlessFrontend::new();
            let g1 = app.mint_group();
            let w1 = app.open_fleet_window(&fe, g1, None);
            let g2 = app.mint_group();
            let w2 = app.open_fleet_window(&fe, g2, None);

            // They open at the built-in defaults (no ui.toml under the isolated XDG).
            let default_pad = app.windows[&w1].root.padding();
            let default_theme = app.windows[&w1].root.theme();

            // Reload a config that changes both padding and the color scheme.
            let cfg = config::UiConfig::parse(
                "[window]\npadding = 21.0\n\n[colors]\nscheme = \"tango-dark\"\n",
            )
            .expect("parse");
            assert_ne!(cfg.padding(), default_pad, "precondition: config differs");
            assert_ne!(
                theme_colors(&cfg.theme()),
                default_theme,
                "precondition: scheme differs"
            );

            app.reload_config(&cfg, &fe);

            for wid in [w1, w2] {
                let root = &app.windows[&wid].root;
                assert_eq!(root.padding(), 21.0, "reload updates padding on {wid:?}");
                assert_eq!(
                    root.theme(),
                    theme_colors(&cfg.theme()),
                    "reload updates theme colors on {wid:?}"
                );
            }
        });
    }

    #[test]
    fn unique_session_name_prefixes_the_creator_host_and_increments() {
        // Session names are namespaced by *this* machine's host tag (so two
        // ghosts on different hosts sharing a home can't clash) followed by
        // `<pid>-<seq>`, where seq increments per mint.
        let mut app = App::headless();
        let host = ghost_vt::paths::host_tag();
        let pid = std::process::id();

        let first = app.unique_session_name();
        let second = app.unique_session_name();

        assert_eq!(first, format!("{host}-{pid}-0"));
        assert_eq!(second, format!("{host}-{pid}-1"));
        assert_ne!(first, second);
        // The legacy hardcoded prefix is gone.
        assert!(!first.starts_with("ghost-ui-"));
    }

    #[test]
    fn new_ssh_session_opens_the_prompt_in_this_window_not_a_new_one() {
        // Cmd+G opens the connect prompt in the *current* window — no new window —
        // unlike Cmd+S / `open_connect_window`, which mints a fresh ssh window. Drive
        // the real shell: one window, then a new-ssh-session request must reuse it.
        with_isolated_xdg(|| {
            let mut app = App::headless();
            let fe = HeadlessFrontend::new();
            let group = app.mint_group();
            let wid = app.open_fleet_window(&fe, group, None);
            assert_eq!(app.windows.len(), 1);

            app.open_connect_session(wid);

            assert_eq!(
                app.windows.len(),
                1,
                "a new ssh session reuses this window — it opens no new window"
            );
            assert!(
                app.windows.get(&wid).unwrap().root.is_connecting(),
                "this window now shows the connect prompt"
            );
        });
    }

    #[test]
    fn start_connect_reaps_a_stale_control_socket_before_the_warmup() {
        // A stale control socket (a crashed or rebooted master — the runtime dir is
        // durable on macOS, so the file survives) makes the warm-up ssh "disable
        // multiplexing": it authenticates a one-shot connection and leaves NO
        // master, so the connect worker's PTY-less probes cannot re-auth on a
        // password host and the transport silently degrades to the ssh child. The
        // interactive connect must clear a dead socket up front — the same guard
        // `open_master_batch` and `negotiate` apply to their flows — so the warm-up
        // itself opens the fresh master under the user's PTY auth.
        with_isolated_xdg(|| {
            let spec = ConnectionSpec::parse_target("ghost-reap-test.invalid").unwrap();
            let remote = ghost_vt::remote::RemoteSsh::new(spec.clone()).unwrap();
            // The per-target control path is deterministic (non-alphanumerics
            // become `_`; see `control_path_sanitizes_the_target` in ghost-vt).
            let ctl = ghost_vt::paths::runtime_dir().join("ssh-ghost_reap_test_invalid.ctl");
            std::fs::write(&ctl, b"stale").unwrap();
            // The warm-up targets an unresolvable host so it exits on its own;
            // the setup's Drop kills it regardless.
            let setup = App::start_connect(remote, spec, "s".into());
            assert!(
                !ctl.exists(),
                "the stale control socket was reaped before the warm-up spawned"
            );
            drop(setup);
        });
    }

    #[test]
    fn on_user_event_merges_a_remote_listing_under_composite_ids() {
        // The real shell handles a watcher delivery headlessly: a host's listing is
        // stashed and its ids resolve back to (target, real) — the identity path a
        // past bug broke (a window mistook its own remote session for a foreign
        // one). No window, disk, or network needed.
        let mut app = App::headless();
        let fe = HeadlessFrontend::new();
        // The watcher posts already-namespaced infos (see `watch_stream_once`).
        let infos = namespace_remote_infos("kov@box", vec![info("work", false)]);
        app.on_user_event(
            &fe,
            UserEvent::RemoteSessions {
                target: "kov@box".to_string(),
                infos,
            },
        );

        assert!(
            app.remote_infos.contains_key("kov@box"),
            "the host's listing is stashed"
        );
        let composite = format!("kov@box{REMOTE_ID_SEP}work");
        assert_eq!(
            app.remote_index.get(&composite),
            Some(&("kov@box".to_string(), "work".to_string())),
            "the namespaced fleet id resolves back to (target, real id)"
        );
        assert!(
            app.sessions_changed
                .load(std::sync::atomic::Ordering::Relaxed),
            "a re-enumeration is hinted so the fleet merges the remote sessions"
        );
    }

    #[test]
    fn prune_remotes_drops_hosts_no_live_window_references() {
        // Closing the window onto a host must stop polling it. Drive the real shell:
        // two connected hosts, one referenced by an ssh-group window, the other by
        // nothing — prune keeps the first and drops the second (and its listing).
        with_isolated_xdg(|| {
            let mut app = App::headless();
            let fe = HeadlessFrontend::new();

            let a = ConnectionSpec::parse_target("kov@a").unwrap();
            let b = ConnectionSpec::parse_target("kov@b").unwrap();
            app.register_remote(&a, "ghost");
            app.register_remote(&b, "ghost");
            app.remote_infos.insert("kov@a".to_string(), Vec::new());
            app.remote_infos.insert("kov@b".to_string(), Vec::new());

            // A window that is an ssh group for host A references it; B is orphaned.
            let group = app.mint_group();
            let wid = app.open_fleet_window(&fe, group, None);
            app.windows
                .get_mut(&wid)
                .unwrap()
                .root
                .set_group_connection(Some(a.clone()));

            app.prune_remotes();

            let remotes = app.remotes.lock().unwrap();
            assert!(remotes.contains_key("kov@a"), "the referenced host stays");
            assert!(
                !remotes.contains_key("kov@b"),
                "the unreferenced host is dropped"
            );
            assert!(app.remote_infos.contains_key("kov@a"));
            assert!(
                !app.remote_infos.contains_key("kov@b"),
                "its cached listing is dropped too"
            );
        });
    }

    #[test]
    fn in_use_targets_keeps_a_host_a_group_still_remembers() {
        // A group that still remembers a cold remote member must keep its host "in
        // use", so a dropped connection keeps its watcher retrying and the remembered
        // session reappears on reconnect — rather than being pruned and orphaned.
        with_isolated_xdg(|| {
            let mut app = App::headless();
            // A group whose only member is a remote session on kov@c — no ssh-group
            // connection to it, no window driving it (the outage went that far).
            app.groups = vec![ghost_ui_core::Group {
                id: "w1".into(),
                name: "blue".into(),
                color: 0,
                members: vec![format!("kov@c{REMOTE_ID_SEP}work")],
                connection: None,
            }];
            assert!(
                app.in_use_targets().contains("kov@c"),
                "a host a group still remembers stays in use: {:?}",
                app.in_use_targets()
            );
        });
    }

    /// The built `ghost` binary sitting next to this test binary
    /// (`target/<profile>/ghost`, sibling of `deps/ghost-<hash>`), or `None` if it
    /// isn't there — `cargo test` builds it, so it normally is.
    fn ghost_binary() -> Option<std::path::PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let bin = exe.parent()?.parent()?.join("ghost");
        bin.exists().then_some(bin)
    }

    /// A fake `ssh` in a fresh dir: strips ssh options + the destination, then runs
    /// the remaining (quoted) remote words locally through a shell — space-joining
    /// like real ssh. Prepended to `PATH`, it makes `RemoteSsh`'s `ssh …` invocations
    /// run against a real local host with no network. Mirrors `tests/ssh_transport.rs`.
    fn write_ssh_shim() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let ssh = dir.path().join("ssh");
        std::fs::write(
            &ssh,
            "#!/bin/sh\n\
             while [ $# -gt 0 ]; do\n\
             \x20 case \"$1\" in\n\
             \x20   -p|-i|-J|-o) shift 2 ;;\n\
             \x20   -*) shift ;;\n\
             \x20   *) shift; break ;;\n\
             \x20 esac\n\
             done\n\
             [ $# -eq 0 ] && exec sh\n\
             exec sh -c \"$*\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        dir
    }

    #[test]
    fn a_superseded_or_orphaned_connect_outcome_is_not_wanted() {
        // A live window whose connect generation still matches → adopt.
        assert!(connect_outcome_wanted(Some(3), 3));
        // Window-flow cancel: the window was closed while staging ran (no window) →
        // the outcome is stale, must not be adopted.
        assert!(!connect_outcome_wanted(None, 0));
        // Session-flow cancel: the window lives on, but Escape bumped its connect
        // generation past the one the worker stamped → stale, must not be adopted.
        assert!(!connect_outcome_wanted(Some(1), 0));
    }

    #[test]
    fn a_cancelled_connect_kills_the_orphaned_remote_session() {
        // The worker spawns the detached remote session before it reports back, so a
        // cancel that lands during staging would otherwise leave it running. Drive
        // the real shell over the shim: create the remote session, cancel (bump the
        // window's connect gen), then deliver the stale outcome — finish_connect must
        // neither adopt the tab nor leave the session alive.
        let Some(ghost_bin) = ghost_binary() else {
            eprintln!("skipping: no `ghost` binary next to the test binary");
            return;
        };
        with_isolated_xdg(|| {
            let shim = write_ssh_shim();
            let orig_path = std::env::var_os("PATH");
            let mut dirs = vec![shim.path().to_path_buf()];
            if let Some(p) = &orig_path {
                dirs.extend(std::env::split_paths(p));
            }
            let joined = std::env::join_paths(dirs).unwrap();
            // SAFETY: single-threaded within `with_isolated_xdg`'s lock.
            unsafe { std::env::set_var("PATH", &joined) };

            let mut app = App::headless();
            let fe = HeadlessFrontend::new();
            let spec = ConnectionSpec::parse_target("kov@box").unwrap();
            let group = app.mint_group();
            let wid = app.open_fleet_window(&fe, group, None);

            let listed = |n: &str| {
                ghost_vt::session::list()
                    .unwrap_or_default()
                    .iter()
                    .any(|s| s.name == n)
            };
            // Poll a condition on the session listing (spawn/kill are async).
            let wait_until = |want: bool, n: &str| {
                for _ in 0..100 {
                    if listed(n) == want {
                        return true;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                false
            };

            // Stand in for the worker: create the detached remote (shim → local)
            // session, exactly as `spawn_host` does when a connect commits.
            let name = "orphan-1";
            let remote = ghost_vt::remote::RemoteSsh::new(spec.clone()).unwrap();
            remote
                .spawn_host(ghost_bin.to_str().unwrap(), name)
                .unwrap();
            let created = wait_until(true, name);

            // The user cancelled while staging ran: bump the window's connect gen so
            // the worker's (pre-cancel) outcome is now stale.
            app.windows.get_mut(&wid).unwrap().connect_gen += 1;

            app.finish_connect(
                wid,
                0, // the generation the worker stamped, before the cancel bumped it
                spec.clone(),
                name.to_string(),
                ConnectOutcome::Transport {
                    remote_ghost: ghost_bin.to_str().unwrap().to_string(),
                },
                &fe,
            );

            let composite = format!("kov@box{REMOTE_ID_SEP}{name}");
            let adopted = app.windows[&wid].sessions.contains_key(&composite);

            // The orphan kill is best-effort off-thread; poll until it's gone.
            let gone = wait_until(false, name);

            // Cleanup + restore PATH before asserting so a failure never leaks state.
            let _ = ghost_vt::session::kill_session(name);
            // SAFETY: still within the lock; restore PATH for later tests.
            unsafe {
                match orig_path {
                    Some(p) => std::env::set_var("PATH", p),
                    None => std::env::remove_var("PATH"),
                }
            }

            assert!(created, "the remote session was created");
            assert!(!adopted, "a cancelled connect does not adopt its tab");
            assert!(
                gone,
                "the orphaned remote session was killed, not left running"
            );
        });
    }

    #[test]
    fn spawn_remote_session_opens_a_real_session_on_the_host_over_the_shim() {
        // The full inheritance-over-remote flow, end to end and offscreen: the real
        // shell creates a session ON a host over the (shimmed) transport and drives
        // it as this-window under the composite id — a real `ghost new -d` + attach
        // through `ghost __pipe`, no GPU and no network.
        let Some(ghost_bin) = ghost_binary() else {
            eprintln!("skipping: no `ghost` binary next to the test binary");
            return;
        };
        with_isolated_xdg(|| {
            let shim = write_ssh_shim();
            let orig_path = std::env::var_os("PATH");
            let mut dirs = vec![shim.path().to_path_buf()];
            if let Some(p) = &orig_path {
                dirs.extend(std::env::split_paths(p));
            }
            let joined = std::env::join_paths(dirs).unwrap();
            // SAFETY: single-threaded within `with_isolated_xdg`'s lock.
            unsafe { std::env::set_var("PATH", &joined) };

            let mut app = App::headless();
            let fe = HeadlessFrontend::new();
            let spec = ConnectionSpec::parse_target("kov@box").unwrap();
            // Register the host with the real ghost binary as its remote ghost, and a
            // fleet window that is an ssh group for it.
            app.register_remote(&spec, ghost_bin.to_str().unwrap());
            let group = app.mint_group();
            let wid = app.open_fleet_window(&fe, group, None);
            app.windows
                .get_mut(&wid)
                .unwrap()
                .root
                .set_group_connection(Some(spec.clone()));

            let name = "hr-attach-1";
            app.spawn_remote_session(wid, "kov@box", name, &fe);

            let composite = format!("kov@box{REMOTE_ID_SEP}{name}");
            let held = app.windows[&wid].sessions.contains_key(&composite);

            // Tear the real host down before asserting, so a failure never leaks it.
            let _ = ghost_vt::session::kill_session(name);
            // SAFETY: still within the lock; restore PATH for later tests.
            unsafe {
                match orig_path {
                    Some(p) => std::env::set_var("PATH", p),
                    None => std::env::remove_var("PATH"),
                }
            }

            assert!(
                held,
                "the window drives the new remote session over the transport"
            );
            assert_eq!(
                app.remote_index.get(&composite),
                Some(&("kov@box".to_string(), name.to_string())),
                "the driven session is indexed back to its host"
            );
        });
    }

    #[test]
    fn a_driven_remote_session_stays_indexed_across_another_hosts_rebuild() {
        // `rebuild_remote_index` rebuilds from the watcher's listings. A freshly
        // spawned/connected remote session is driven (in `window.sessions`) and
        // indexed before its OWN host has listed it — so a push from another host
        // (or an empty listing) that triggers a rebuild must not drop it, or its
        // rename/kill/observe would misroute to the local path and fail.
        let Some(ghost_bin) = ghost_binary() else {
            eprintln!("skipping: no `ghost` binary next to the test binary");
            return;
        };
        with_isolated_xdg(|| {
            let shim = write_ssh_shim();
            let orig_path = std::env::var_os("PATH");
            let mut dirs = vec![shim.path().to_path_buf()];
            if let Some(p) = &orig_path {
                dirs.extend(std::env::split_paths(p));
            }
            let joined = std::env::join_paths(dirs).unwrap();
            // SAFETY: single-threaded within `with_isolated_xdg`'s lock.
            unsafe { std::env::set_var("PATH", &joined) };

            let mut app = App::headless();
            let fe = HeadlessFrontend::new();
            let spec = ConnectionSpec::parse_target("kov@box").unwrap();
            app.register_remote(&spec, ghost_bin.to_str().unwrap());
            let group = app.mint_group();
            let wid = app.open_fleet_window(&fe, group, None);
            app.windows
                .get_mut(&wid)
                .unwrap()
                .root
                .set_group_connection(Some(spec.clone()));

            let name = "hr-route-1";
            app.spawn_remote_session(wid, "kov@box", name, &fe);
            let composite = format!("kov@box{REMOTE_ID_SEP}{name}");

            // Another host pushes a listing before kov@box has listed our session,
            // triggering a rebuild of the index.
            app.on_user_event(
                &fe,
                UserEvent::RemoteSessions {
                    target: "kov@other".to_string(),
                    infos: Vec::new(),
                },
            );
            let indexed = app.remote_index.get(&composite).cloned();

            let _ = ghost_vt::session::kill_session(name);
            // SAFETY: still within the lock; restore PATH for later tests.
            unsafe {
                match orig_path {
                    Some(p) => std::env::set_var("PATH", p),
                    None => std::env::remove_var("PATH"),
                }
            }

            assert_eq!(
                indexed,
                Some(("kov@box".to_string(), name.to_string())),
                "a driven remote session must stay indexed across another host's rebuild"
            );
        });
    }

    #[test]
    fn a_remote_id_always_routes_control_actions_over_the_transport() {
        // A plain id renames/kills over its local control socket.
        assert!(
            super::remote_id_parts("plain-session").is_none(),
            "a local id has no host parts"
        );
        // A namespaced remote id is self-describing: its host + real name come from
        // the id itself, so a rename or kill ALWAYS routes over the transport even
        // if the index has since dropped it — never the local path (whose bogus
        // socket reports a misleading "older ghost" error). Kill matters most for a
        // COLD remote tile (its host dropped, so it is neither driven nor listed —
        // exactly the ids the index does not hold), whose manual kill is the one
        // cleanup for a lingering dead remote member.
        let composite = format!("kov@box{REMOTE_ID_SEP}work");
        assert_eq!(
            super::remote_id_parts(&composite),
            Some(("kov@box", "work")),
            "a remote id recovers (target, real) from the composite itself"
        );
    }

    #[test]
    fn a_new_session_routes_onto_a_connected_remote_host_only() {
        let spec = ConnectionSpec::parse_target("kov@box").expect("valid target");
        let inherited = inherited_connection(Some(&spec), None);
        assert!(inherited.is_some(), "the group connection is inherited");

        let mut connected = HashSet::new();
        // An ssh connection to a host we are NOT transported to → local (ssh child).
        assert_eq!(remote_spawn_target(inherited.as_ref(), &connected), None);

        // Once we hold a live transport to that host → route the spawn onto it.
        connected.insert("kov@box".to_string());
        assert_eq!(
            remote_spawn_target(inherited.as_ref(), &connected),
            Some("kov@box".to_string())
        );

        // No inherited connection → a plain local `$SHELL`.
        assert_eq!(remote_spawn_target(None, &connected), None);
    }

    fn info(name: &str, attached: bool) -> SessionInfo {
        SessionInfo {
            name: name.to_string(),
            pid: 1,
            created_at: None,
            title: String::new(),
            command: Vec::new(),
            attached,
            bell: false,
            display_name: String::new(),
            cwd: None,
            size: None,
            connection: None,
        }
    }

    #[test]
    fn password_prompt_matches_ssh_password_and_passphrase_asks() {
        // ssh writes the prompt with no trailing newline; the tail line is it.
        assert_eq!(
            password_prompt("Warning: blah\r\nclaude@host's password: ").as_deref(),
            Some("claude@host's password:")
        );
        assert_eq!(
            password_prompt("Enter passphrase for key '/home/k/.ssh/id_ed25519': ").as_deref(),
            Some("Enter passphrase for key '/home/k/.ssh/id_ed25519':")
        );
        // Ordinary output (or nothing yet) is not a prompt.
        assert_eq!(password_prompt("Last login: Tue\r\n"), None);
        assert_eq!(password_prompt("   \n\n"), None);
        assert_eq!(password_prompt(""), None);
    }

    #[test]
    fn auth_error_message_prefers_the_permission_denied_line() {
        assert_eq!(
            auth_error_message("foo\r\nPermission denied, please try again.\r\nbar\r\n"),
            "Permission denied, please try again."
        );
        // No denial line: the last non-empty line stands in.
        assert_eq!(
            auth_error_message("ssh: connect: no route\r\n"),
            "ssh: connect: no route"
        );
        // Nothing at all: a generic note, never an empty string.
        assert_eq!(auth_error_message(""), "ssh connection failed");
    }

    #[test]
    fn namespacing_a_remote_listing_makes_ids_unique_and_tags_the_host() {
        let base = SessionInfo {
            name: "work".into(),
            pid: 7,
            created_at: None,
            title: String::new(),
            command: vec!["vim".into()],
            attached: false,
            bell: false,
            display_name: String::new(),
            cwd: None,
            size: None,
            connection: None, // the remote host reports it as local-to-itself
        };
        let renamed = SessionInfo {
            name: "raw-id".into(),
            display_name: "editor".into(),
            ..base.clone()
        };
        let out = namespace_remote_infos("kov@box", vec![base, renamed]);

        // The id is prefixed with the target (so it can't collide with a local
        // session or another host), and the connection is set to this host.
        assert_eq!(out[0].name, format!("kov@box{REMOTE_ID_SEP}work"));
        assert_eq!(out[0].connection.as_ref().unwrap().target(), "kov@box");
        // A session with no display name shows its real id; a renamed one keeps
        // its label — never the namespaced id.
        assert_eq!(out[0].display_name, "work");
        assert_eq!(out[1].name, format!("kov@box{REMOTE_ID_SEP}raw-id"));
        assert_eq!(out[1].display_name, "editor");
    }

    fn group(id: &str, members: &[&str]) -> ghost_ui_core::Group {
        ghost_ui_core::Group {
            id: id.to_string(),
            name: "blue".to_string(),
            color: 0,
            members: members.iter().map(|m| m.to_string()).collect(),
            connection: None,
        }
    }

    fn record(
        group_id: &str,
        cols: u16,
        rows: u16,
        fleet: bool,
        fg: Option<&str>,
        att: &[&str],
    ) -> WindowRecord {
        WindowRecord {
            group_id: group_id.into(),
            cols,
            rows,
            fleet,
            foreground: fg.map(str::to_string),
            attached: att.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn restore_plan_reclaims_groups_orders_foreground_last_and_flags_dead() {
        let records = [
            record("win-1", 120, 40, false, Some("beta"), &["alpha", "beta"]),
            // Group pruned from the registry → this window can't be restored.
            record("win-9", 80, 24, false, Some("ghost"), &["ghost"]),
            record("win-2", 90, 30, true, Some("gamma"), &["gamma"]),
        ];
        let sessions = [info("alpha", false), info("beta", false)]; // gamma is dead
        let groups = [
            group("win-1", &["alpha", "beta"]),
            group("win-2", &["gamma"]),
        ];

        let plans = restore_plan(&records, &sessions, &groups);
        assert_eq!(plans.len(), 2, "the pruned-group window is dropped");

        let w1 = &plans[0];
        assert_eq!(w1.group.id, "win-1");
        assert_eq!((w1.cols, w1.rows), (120, 40));
        assert!(!w1.fleet);
        let ids: Vec<&str> = w1.locals.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["alpha", "beta"], "foreground (beta) ordered last");
        assert!(w1.locals.iter().all(|m| !m.dead), "both sessions are alive");

        let w2 = &plans[1];
        assert_eq!(w2.group.id, "win-2");
        assert!(w2.fleet);
        assert_eq!(w2.locals.len(), 1);
        assert!(w2.locals[0].dead, "gamma has no live session → relaunch");
    }

    fn remote(sess: &str) -> String {
        format!("kov@box{REMOTE_ID_SEP}{sess}")
    }

    #[test]
    fn restore_plan_splits_local_and_remote_members() {
        // A window with a local session and a remote (transport) one: the local is
        // planned for local restore; the remote is planned SEPARATELY (reconnected
        // and re-adopted asynchronously, never spawned locally). A window whose only
        // member is remote is kept (not dropped) so its host is reconnected.
        let rem = remote("work");
        let rem2 = remote("build");
        let records = [
            record("win-1", 80, 24, false, Some(&rem), &["alpha", &rem]),
            record("win-2", 80, 24, true, None, &[&rem2]),
        ];
        let sessions = [info("alpha", false)];
        let groups = [group("win-1", &["alpha", &rem]), group("win-2", &[&rem2])];

        let plans = restore_plan(&records, &sessions, &groups);
        assert_eq!(
            plans.len(),
            2,
            "the remote-only window is kept, not dropped"
        );

        let w1 = &plans[0];
        let locals: Vec<&str> = w1.locals.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            locals,
            vec!["alpha"],
            "only the local session is a local member"
        );
        assert_eq!(
            w1.remotes,
            vec![rem.clone()],
            "the remote session is a remote member"
        );

        let w2 = &plans[1];
        assert!(
            w2.locals.is_empty(),
            "the remote-only window has no local members"
        );
        assert_eq!(
            w2.remotes,
            vec![rem2.clone()],
            "its remote member is planned"
        );
    }

    #[test]
    fn a_remote_session_and_its_group_are_remembered_across_a_save() {
        // Remote (transport) sessions used to be stripped from persistence, so a
        // restart forgot them and — worse — the groups they belonged to. They are
        // now first-class: adopting one records it in the window's group (→ persisted
        // groups.toml) and as the window's foreground (→ persisted windows.toml).
        with_isolated_xdg(|| {
            let mut app = App::headless();
            let fe = HeadlessFrontend::new();
            let group = app.mint_group();
            let wid = app.open_fleet_window(&fe, group, None);
            let rem = format!("kov@box{REMOTE_ID_SEP}work");
            app.dispatch(wid, ghost_ui_core::UiEvent::AdoptSession(rem.clone()), &fe);
            app.save_workspace();

            let groups = super::groups::load();
            assert!(
                groups.iter().any(|g| g.members.contains(&rem)),
                "the remote session is remembered as a group member: {groups:?}"
            );
            let records = super::windows::load();
            assert!(
                records
                    .iter()
                    .any(|r| r.foreground.as_deref() == Some(rem.as_str())
                        || r.attached.contains(&rem)),
                "the remote session is remembered in its window: {records:?}"
            );
        });
    }

    #[test]
    fn restore_queues_a_remote_only_window_for_reconnect() {
        // A window whose only member is a remote (transport) session is remembered:
        // restore opens it (a fleet on its group) and QUEUES the remote member for
        // its host's reconnect — it is never spawned locally.
        with_isolated_xdg(|| {
            let mut app = App::headless();
            let fe = HeadlessFrontend::new();
            let rem = remote("work"); // kov@box␟work
            app.groups = vec![group("g1", &[&rem])];
            let records = vec![record("g1", 80, 24, true, None, &[&rem])];
            app.restore_workspace(&fe, records);

            assert_eq!(app.windows.len(), 1, "the remote-only window is opened");
            assert!(
                app.windows.values().next().unwrap().root.is_fleet(),
                "a remote-only window left in the fleet overview stays in it"
            );
            let pending = app
                .pending_remote_restores
                .get("kov@box")
                .expect("its host is queued for reconnect");
            assert!(
                pending.iter().any(|p| p.composite == rem),
                "the remote member is queued, not spawned locally"
            );
        });
    }

    #[test]
    fn closing_a_window_drops_its_queued_remote_reconnects() {
        // A window waiting on a remote host to reconnect is closed before the host
        // comes back: its queued reconnect must be dropped — the session has nowhere
        // to land now — not left to linger for a host that may never return.
        with_isolated_xdg(|| {
            let mut app = App::headless();
            let fe = HeadlessFrontend::new();
            let rem = remote("work"); // kov@box␟work
            app.groups = vec![group("g1", &[&rem])];
            let records = vec![record("g1", 80, 24, true, None, &[&rem])];
            app.restore_workspace(&fe, records);

            let wid = *app.windows.keys().next().unwrap();
            assert!(
                app.pending_remote_restores.contains_key("kov@box"),
                "the remote reconnect is queued while the window is open"
            );
            app.close_window(wid);
            assert!(
                !app.pending_remote_restores.contains_key("kov@box"),
                "closing the window drops its queued remote reconnect"
            );
        });
    }

    #[test]
    fn a_fleet_reconnect_observes_its_remote_group_without_diving() {
        // A window left in the fleet overview reconnects its remote group: it stays
        // in the overview (no dive), and its members become *observed* tiles — the
        // window does NOT drive them (driving without adopting would double-feed the
        // tile; adopting would dive out). The host is registered so the observe path
        // and later take-overs can route. No transport is opened, so this is a pure
        // headless test.
        with_isolated_xdg(|| {
            let mut app = App::headless();
            let fe = HeadlessFrontend::new();
            let spec = ConnectionSpec::parse_target("kov@box").unwrap();
            let group = app.mint_group();
            let wid = app.open_fleet_window(&fe, group, None);
            let one = remote("one");
            let two = remote("two");
            // Saved in the fleet overview (fleet: true) → observed in place, not driven.
            app.pending_remote_restores.insert(
                "kov@box".to_string(),
                vec![
                    PendingRemote {
                        wid,
                        composite: one.clone(),
                        fleet: true,
                        foreground: false,
                    },
                    PendingRemote {
                        wid,
                        composite: two.clone(),
                        fleet: true,
                        foreground: false,
                    },
                ],
            );

            app.finish_remote_reconnect(spec, "ghost".to_string(), &fe);

            let w = &app.windows[&wid];
            assert!(w.root.is_fleet(), "the fleet window stays in the overview");
            assert!(
                w.sessions.is_empty(),
                "its remote members are observed, not driven: {:?}",
                w.sessions.keys().collect::<Vec<_>>()
            );
            assert!(
                app.remote_index.contains_key(&one) && app.remote_index.contains_key(&two),
                "both members are indexed so their tiles can route over the transport"
            );
            assert!(
                app.remotes.lock().unwrap().contains_key("kov@box"),
                "the host is registered (its watcher/observe path is live)"
            );
            assert!(
                !app.pending_remote_restores.contains_key("kov@box"),
                "the target is drained from the pending set"
            );
        });
    }

    #[test]
    fn a_single_remote_window_drives_its_session_on_reconnect() {
        // The single-view counterpart: a lone remote-session window SAVED in single
        // view reconnects its one session and DRIVES + foregrounds it, diving out of
        // the fleet it was restored into (a real `ghost new -d` on the host + attach
        // through `ghost __pipe` over the shim). A remote-only window owns no tile, so
        // F9 can't force it single — the saved single mode rides in the pending entry
        // (false) and `finish_remote_reconnect` drives + dives on it.
        let Some(ghost_bin) = ghost_binary() else {
            eprintln!("skipping: no `ghost` binary next to the test binary");
            return;
        };
        with_isolated_xdg(|| {
            let shim = write_ssh_shim();
            let orig_path = std::env::var_os("PATH");
            let mut dirs = vec![shim.path().to_path_buf()];
            if let Some(p) = &orig_path {
                dirs.extend(std::env::split_paths(p));
            }
            let joined = std::env::join_paths(dirs).unwrap();
            // SAFETY: single-threaded within `with_isolated_xdg`'s lock.
            unsafe { std::env::set_var("PATH", &joined) };

            let mut app = App::headless();
            let fe = HeadlessFrontend::new();
            let spec = ConnectionSpec::parse_target("kov@box").unwrap();

            // The session survived the restart on the host (shim → a real local one).
            let real = "restored-1";
            let remote = ghost_vt::remote::RemoteSsh::new(spec.clone()).unwrap();
            remote
                .spawn_host(ghost_bin.to_str().unwrap(), real)
                .unwrap();

            // A restored remote-only window is waiting to re-adopt it: opened as a
            // fleet on its group (no local member to open a single view on), with the
            // remote queued carrying its SAVED single mode (false = drive, not observe).
            let group = app.mint_group();
            let wid = app.open_fleet_window(&fe, group, None);
            let composite = format!("kov@box{REMOTE_ID_SEP}{real}");
            app.pending_remote_restores.insert(
                "kov@box".to_string(),
                vec![PendingRemote {
                    wid,
                    composite: composite.clone(),
                    fleet: false,
                    foreground: true,
                }],
            );

            app.finish_remote_reconnect(spec, ghost_bin.to_str().unwrap().to_string(), &fe);
            let held = app.windows[&wid].sessions.contains_key(&composite);
            let single = !app.windows[&wid].root.is_fleet();
            let drained = !app.pending_remote_restores.contains_key("kov@box");

            let _ = ghost_vt::session::kill_session(real);
            // SAFETY: still within the lock; restore PATH for later tests.
            unsafe {
                match orig_path {
                    Some(p) => std::env::set_var("PATH", p),
                    None => std::env::remove_var("PATH"),
                }
            }

            assert!(
                held,
                "the saved-single window drives the remembered remote session over the transport"
            );
            assert!(
                single,
                "driving the reconnected session dove the window out of the fleet into its single view"
            );
            assert!(drained, "the target is drained from the pending set");
        });
    }

    #[test]
    fn a_reconnecting_background_remote_does_not_steal_the_foreground() {
        // A window driving two remote sessions, saved in single view with ONE of them
        // foreground. When the host reconnects, the foreground session is driven to the
        // front and the OTHER attaches as a warm background mirror — it must NOT yank
        // the foreground onto itself. (A mixed local-foreground + remote-background
        // window takes the identical branch in `finish_remote_reconnect`.)
        let Some(ghost_bin) = ghost_binary() else {
            eprintln!("skipping: no `ghost` binary next to the test binary");
            return;
        };
        with_isolated_xdg(|| {
            let shim = write_ssh_shim();
            let orig_path = std::env::var_os("PATH");
            let mut dirs = vec![shim.path().to_path_buf()];
            if let Some(p) = &orig_path {
                dirs.extend(std::env::split_paths(p));
            }
            let joined = std::env::join_paths(dirs).unwrap();
            // SAFETY: single-threaded within `with_isolated_xdg`'s lock.
            unsafe { std::env::set_var("PATH", &joined) };

            let mut app = App::headless();
            let fe = HeadlessFrontend::new();
            let spec = ConnectionSpec::parse_target("kov@box").unwrap();

            // Two sessions survived on the host.
            let remote = ghost_vt::remote::RemoteSsh::new(spec.clone()).unwrap();
            remote
                .spawn_host(ghost_bin.to_str().unwrap(), "fg-1")
                .unwrap();
            remote
                .spawn_host(ghost_bin.to_str().unwrap(), "bg-1")
                .unwrap();

            let group = app.mint_group();
            let wid = app.open_fleet_window(&fe, group, None);
            let fg = format!("kov@box{REMOTE_ID_SEP}fg-1");
            let bg = format!("kov@box{REMOTE_ID_SEP}bg-1");
            // Saved single (drive): fg is the foreground, bg a background member.
            app.pending_remote_restores.insert(
                "kov@box".to_string(),
                vec![
                    PendingRemote {
                        wid,
                        composite: fg.clone(),
                        fleet: false,
                        foreground: true,
                    },
                    PendingRemote {
                        wid,
                        composite: bg.clone(),
                        fleet: false,
                        foreground: false,
                    },
                ],
            );

            app.finish_remote_reconnect(spec, ghost_bin.to_str().unwrap().to_string(), &fe);
            let held_fg = app.windows[&wid].sessions.contains_key(&fg);
            let held_bg = app.windows[&wid].sessions.contains_key(&bg);
            let foreground = app.windows[&wid].root.foreground().cloned();

            let _ = ghost_vt::session::kill_session("fg-1");
            let _ = ghost_vt::session::kill_session("bg-1");
            // SAFETY: still within the lock; restore PATH for later tests.
            unsafe {
                match orig_path {
                    Some(p) => std::env::set_var("PATH", p),
                    None => std::env::remove_var("PATH"),
                }
            }

            assert!(
                held_fg && held_bg,
                "both remote sessions are attached over the transport"
            );
            assert_eq!(
                foreground.as_deref(),
                Some(fg.as_str()),
                "the saved foreground stays in front; the background reconnect must not steal it"
            );
        });
    }

    #[test]
    fn should_restore_only_on_a_bare_launch_with_a_saved_workspace() {
        let saved = [record("win-1", 80, 24, false, Some("alpha"), &["alpha"])];

        // The one case that restores: bare launch, not fresh, workspace present.
        assert!(should_restore(false, None, &saved));

        // --fresh always starts clean, even with a saved workspace.
        assert!(!should_restore(true, None, &saved));
        // An explicit $GHOST_SESSION opens just that session, skipping restore.
        assert!(!should_restore(false, Some("alpha"), &saved));
        // Nothing to restore.
        assert!(!should_restore(false, None, &[]));
    }

    #[test]
    fn a_relaunch_runs_a_shell_seeded_from_the_recording_not_the_old_command() {
        use ghost_vt::descriptor::Descriptor;
        use std::path::{Path, PathBuf};
        let d = Descriptor {
            command: vec!["vim".into(), "notes.md".into()],
            cwd: Some(PathBuf::from("/home/kov/proj")),
            ..Default::default()
        };
        // No recording on disk → no seed, but it's still a shell in the old cwd.
        let opts = respawn_opts(
            "phoenix",
            &d,
            PathBuf::from("/nonexistent/phoenix.ghostrec"),
        );
        assert!(
            opts.command.is_empty(),
            "a relaunch runs the shell, not the recorded command"
        );
        assert_eq!(opts.cwd.as_deref(), Some(Path::new("/home/kov/proj")));
        assert!(
            opts.start_on_attach,
            "the child is deferred to first attach"
        );
        assert!(
            opts.seed_from.is_none(),
            "a missing recording seeds nothing"
        );
        assert_eq!(opts.name, "phoenix");
        assert!(
            opts.connection.is_none(),
            "a local session's relaunch carries no connection"
        );
    }

    #[test]
    fn a_remote_foreground_inherits_ssh_from_the_live_host() {
        // A session driven over the transport is keyed by a composite id and has NO
        // local descriptor, so `descriptor::read` finds nothing. Its connection —
        // what a new session (Cmd+T) branching off it inherits — must resolve from
        // the live remote host instead, so a non-ssh window whose foreground is a
        // remote tab still spawns its next session ON that host, not a local shell.
        with_isolated_xdg(|| {
            let mut app = App::headless();
            let spec = ConnectionSpec::parse_target("kov@box").unwrap();
            app.register_remote(&spec, "ghost");
            let composite = format!("kov@box{REMOTE_ID_SEP}work");
            assert_eq!(
                app.foreground_connection(&composite),
                Some(spec),
                "the remote foreground's host resolves from the live transport"
            );
            // A remote id for a host we hold no transport to → nothing to inherit.
            assert_eq!(
                app.foreground_connection(&format!("gone@host{REMOTE_ID_SEP}x")),
                None
            );
            // A plain local id with no descriptor → nothing (the pre-existing path).
            assert_eq!(app.foreground_connection("local-only"), None);
        });
    }

    #[test]
    fn inherited_connection_prefers_foreground_then_group_then_local() {
        use super::inherited_connection;
        use ghost_vt::connection::ConnectionSpec;
        let group = ConnectionSpec::parse_target("ops@gateway");
        let foreground = ConnectionSpec::parse_target("dev@box");
        // The session you're branching off (the foreground) wins: a new terminal is a
        // sibling of what you're looking at, even when the group's own connection names
        // a different host. This is what keeps a "new session" off the wrong host after
        // a cross-host fleet take-over adopted a session whose group connection is stale.
        assert_eq!(
            inherited_connection(group.as_ref(), foreground.as_ref())
                .unwrap()
                .target(),
            "dev@box"
        );
        // A local foreground carries no connection, so an explicit "ssh group" still
        // spawns its next session onto the group's host, not a local shell.
        assert_eq!(
            inherited_connection(group.as_ref(), None).unwrap().target(),
            "ops@gateway"
        );
        // The foreground alone (an ordinary window whose foreground is a remote tab).
        assert_eq!(
            inherited_connection(None, foreground.as_ref())
                .unwrap()
                .target(),
            "dev@box"
        );
        // Neither: a plain local session.
        assert_eq!(inherited_connection(None, None), None);
    }

    #[test]
    fn a_dead_ssh_session_relaunches_by_reconnecting() {
        // The substrate-not-workload rule: a connection session relaunches by
        // re-establishing the connection (not a local shell), still seeded from
        // the recording so the old screen shows above the fresh login.
        use ghost_vt::descriptor::Descriptor;
        use std::path::PathBuf;
        let d = Descriptor {
            command: Vec::new(),
            connection: ghost_vt::connection::ConnectionSpec::parse_target("kov@box"),
            ..Default::default()
        };
        let opts = respawn_opts(
            "ssh-box",
            &d,
            PathBuf::from("/nonexistent/ssh-box.ghostrec"),
        );
        assert!(opts.command.is_empty(), "a relaunch never sets a command");
        let spec = opts
            .connection
            .expect("the connection is carried into the relaunch");
        assert_eq!(spec.target(), "kov@box");
    }

    #[test]
    fn gui_launch_falls_back_to_home_only_without_a_real_cwd() {
        use std::path::{Path, PathBuf};

        let home = Path::new("/Users/kov");
        // Bundled launch (launchd/Finder) starts us at `/`: fall back to home.
        assert_eq!(
            home_launch_dir(Some(Path::new("/")), Some(home)),
            Some(PathBuf::from("/Users/kov"))
        );
        // No cwd at all: also fall back to home.
        assert_eq!(home_launch_dir(None, Some(home)), Some(PathBuf::from(home)));
        // A real working directory (e.g. launched from a terminal) is kept as-is.
        assert_eq!(
            home_launch_dir(Some(Path::new("/Users/kov/Projects/ghost")), Some(home)),
            None
        );
        // Nothing to fall back to: leave cwd untouched rather than guess.
        assert_eq!(home_launch_dir(Some(Path::new("/")), None), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn option_as_alt_maps_the_meta_preference() {
        use super::option_as_alt;
        use winit::platform::macos::OptionAsAlt;
        // Meta-on makes both Option keys report as Alt (so the encoder ESC-prefixes
        // them); Meta-off leaves macOS to compose accented characters.
        assert_eq!(option_as_alt(true), OptionAsAlt::Both);
        assert_eq!(option_as_alt(false), OptionAsAlt::None);
    }

    #[test]
    fn window_cycle_index_wraps_both_ways_and_needs_two() {
        use super::cycle_index;
        // Forward and backward wrap around.
        assert_eq!(cycle_index(3, Some(2), true), Some(0));
        assert_eq!(cycle_index(3, Some(0), false), Some(2));
        // A missing current starts from the first (so forward lands on index 1).
        assert_eq!(cycle_index(3, None, true), Some(1));
        // Fewer than two windows: nothing to cycle to.
        assert_eq!(cycle_index(1, Some(0), true), None);
        assert_eq!(cycle_index(0, None, true), None);
    }

    #[test]
    fn startup_attaches_to_an_explicitly_requested_session() {
        // `$GHOST_SESSION` wins regardless of what else is around.
        let sessions = [info("a", false)];
        assert!(matches!(
            startup_choice(Some("x".into()), &sessions, &[]),
            StartupChoice::Attach(n) if n == "x"
        ));
    }

    #[test]
    fn startup_opens_the_fleet_when_any_session_is_detached() {
        let sessions = [info("a", true), info("b", false)];
        assert!(matches!(
            startup_choice(None, &sessions, &[]),
            StartupChoice::Fleet
        ));
    }

    #[test]
    fn startup_opens_the_fleet_when_a_group_remembers_a_dead_session() {
        // No live sessions, but the registry remembers a group whose member
        // is gone: launch into the fleet, where the group renders as a
        // reopenable block — not a fresh session piled on top of it.
        let remembered = [group("g1", &["gone"])];
        assert!(matches!(
            startup_choice(None, &[], &remembered),
            StartupChoice::Fleet
        ));
        // A group whose members are all live and attached remembers nothing
        // reconnectable — a plain launch still spawns.
        let sessions = [info("a", true)];
        let live = [group("g1", &["a"])];
        assert!(matches!(
            startup_choice(None, &sessions, &live),
            StartupChoice::Spawn
        ));
    }

    #[test]
    fn startup_spawns_when_nothing_is_detached() {
        // No sessions at all, or only sessions attached elsewhere → fresh session.
        assert!(matches!(
            startup_choice(None, &[], &[]),
            StartupChoice::Spawn
        ));
        let attached_elsewhere = [info("a", true)];
        assert!(matches!(
            startup_choice(None, &attached_elsewhere, &[]),
            StartupChoice::Spawn
        ));
    }

    #[test]
    fn new_window_mirrors_a_plain_launch() {
        // File > New Window / Cmd-N opens a window that "acts like the first one":
        // it carries no `$GHOST_SESSION` request, so it always takes the plain-launch
        // decision — the fleet when anything is detached (reconnect) or remembered
        // (a closed group), a fresh session otherwise — and never attaches to one
        // specific session.
        assert!(matches!(
            new_window_choice(&[info("a", false)], &[]),
            StartupChoice::Fleet
        ));
        assert!(matches!(new_window_choice(&[], &[]), StartupChoice::Spawn));
        assert!(matches!(
            new_window_choice(&[info("a", true)], &[]),
            StartupChoice::Spawn
        ));
        assert!(matches!(
            new_window_choice(&[], &[group("g1", &["gone"])]),
            StartupChoice::Fleet
        ));
    }

    #[test]
    fn alpha_mode_prefers_premultiplied_when_transparent() {
        use wgpu::Backend::{Metal, Vulkan};
        // The compositor offers premultiplied: take it.
        assert_eq!(
            choose_alpha_mode(&[Opaque, PreMultiplied], true, Vulkan),
            PreMultiplied
        );
        // Only straight (post) alpha is offered — it would wash our premultiplied
        // output, so we decline and stay opaque (the first mode) instead.
        assert_eq!(
            choose_alpha_mode(&[Opaque, PostMultiplied], true, Vulkan),
            Opaque
        );
        // Metal is the exception: Core Animation always composites layer content
        // as premultiplied, and wgpu's Metal "PostMultiplied" merely un-opaques
        // the layer — so it IS our premultiplied mode there (Metal never offers
        // PreMultiplied at all: [Opaque, PostMultiplied] is its whole list).
        assert_eq!(
            choose_alpha_mode(&[Opaque, PostMultiplied], true, Metal),
            PostMultiplied
        );
        // An opaque window ignores transparency entirely.
        assert_eq!(
            choose_alpha_mode(&[Opaque, PreMultiplied], false, Metal),
            Opaque
        );
    }

    #[test]
    fn blur_requested_only_when_translucent_and_configured() {
        // Blur rides on transparency: it's requested only when the window is
        // both translucent AND the config opts in. The full 2×2 truth table
        // pins the AND against a future edit that drops the transparency gate.
        assert!(want_blur(true, true));
        assert!(!want_blur(true, false));
        assert!(!want_blur(false, true));
        assert!(!want_blur(false, false));
    }

    #[test]
    fn surface_format_prefers_bgra8_unorm() {
        // Bgra8Unorm is the native swapchain format on most platforms and the one
        // WebGPU guarantees; take it ahead of Rgba8Unorm even when both are offered.
        assert_eq!(choose_surface_format(&[Rgba8Unorm, Bgra8Unorm]), Bgra8Unorm);
    }

    #[test]
    fn surface_format_is_deterministic_regardless_of_order() {
        // The result must not depend on driver enumeration order: an sRGB or HDR
        // format appearing first must not shadow the 8-bit UNORM target.
        assert_eq!(
            choose_surface_format(&[Bgra8UnormSrgb, Rgba16Float, Bgra8Unorm, Rgba8Unorm]),
            Bgra8Unorm
        );
        assert_eq!(
            choose_surface_format(&[Rgba16Float, Rgba8Unorm, Bgra8Unorm]),
            Bgra8Unorm
        );
    }

    #[test]
    fn surface_format_falls_back_to_rgba8_unorm() {
        // No Bgra8Unorm offered: the other plain 8-bit UNORM target still beats any
        // non-sRGB HDR/high-bit-depth format.
        assert_eq!(
            choose_surface_format(&[Rgba16Float, Rgb10a2Unorm, Rgba8Unorm]),
            Rgba8Unorm
        );
    }

    #[test]
    fn surface_format_avoids_srgb_and_hdr_when_no_unorm8() {
        // Neither 8-bit UNORM BGRA/RGBA is offered. Prefer any non-sRGB format
        // (here the HDR one) over an sRGB target that would double-encode.
        assert_eq!(
            choose_surface_format(&[Rgba8UnormSrgb, Rgba16Float]),
            Rgba16Float
        );
        // Only sRGB formats remain: nothing good to pick, take the first.
        assert_eq!(
            choose_surface_format(&[Rgba8UnormSrgb, Bgra8UnormSrgb]),
            Rgba8UnormSrgb
        );
    }
}
