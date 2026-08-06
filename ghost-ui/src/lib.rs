//! ghost's windowed GPU terminal frontend.
//!
//! This is a **library** whose [`run`] entry point is the whole program; the
//! `ghost` binary (`src/bin/ghost.rs`) is a one-line shim over it. The split
//! exists for testing: an integration test in `tests/` can drive the real shell
//! ([`App`]) against real sessions and real remote hosts, with only winit and the
//! GPU replaced ([`HeadlessFrontend`]). A binary crate's internals are reachable
//! only from its own `#[cfg(test)]` module, which cannot use `CARGO_BIN_EXE_ghost`
//! — so shell-level behaviour (window startup choices, remote reconnects) had no
//! end-to-end coverage at all before the split.
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
/// What the desktop says a window should look like (GNOME `gsettings`).
#[cfg(target_os = "linux")]
pub mod desktop;
pub mod font;
mod from_winit;
mod groups;
mod instance;
pub mod menu;
mod pacer;
mod rendertrace;
mod resize;
/// The CSD frame's titlebar text, drawn with ghost's own font stack. Linux
/// only: every other platform draws its own titlebar.
#[cfg(target_os = "linux")]
pub mod title;
mod windows;

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ghost_renderer::{
    FrameOutcome, Gpu, Rendered, Renderer, SceneCache, SurfaceTarget, Target, WindowEdge,
};
use ghost_ui_core::{
    CellMetrics, Cmd, Key, KeyEventKind, Mods, NamedKey, PointPx, PointerButton, PointerPhase,
    RootModel, Scene, SessionPush, Sessions, TerminalModel, UiEvent, WheelDelta, WindowRecord,
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
use winit::window::{Fullscreen, Window, WindowId};

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

/// ghost's application identity: the macOS bundle's `CFBundleIdentifier`, the
/// freedesktop `.desktop` entry's file name, and the app id / WM_CLASS every
/// window carries — one string, so a platform can tie the running window back to
/// the installed application (icon, dock grouping, desktop actions). Packaging
/// lives in `xtask`, whose `APP_ID` must stay equal to this.
pub const APP_ID: &str = "dev.ghost.Terminal";

/// The whole program, behind a library entry point so the shell stays testable
/// (see the crate docs). `src/bin/ghost.rs` is just `ghost_ui::run()`.
pub fn run() {
    // MUST be first: re-execs into the session host when invoked as one.
    server::run_host_if_invoked();

    // `ghost <subcommand>` (ls/attach/new/…) is the CLI; it runs and exits. A bare
    // `ghost` has no subcommand and falls through to the windowed UI below, carrying
    // the `--fresh` flag (skip restoring the last-quit windows) into it.
    let (fresh, ssh_window) = match ghost_cli::run_subcommand() {
        ghost_cli::Launch::Handled => return,
        ghost_cli::Launch::Gui { fresh, ssh_window } => (fresh, ssh_window),
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

    // `GHOST_WINDOW_DUMP` verifies how a TRANSLUCENT window is configured: open
    // one the way the app does, print the compositing-relevant NSWindow state,
    // and exit. A native window's state can't be read from the test process, so
    // this is how it is asserted end-to-end (see `window_macos.rs`).
    #[cfg(target_os = "macos")]
    if std::env::var_os("GHOST_WINDOW_DUMP").is_some() {
        window_dump();
        return;
    }

    // `GHOST_ESCTEST` runs the terminal-conformance harness headlessly: spawn
    // the esctest child, drive the model over the PTY, and exit. See
    // [`esctest_host`] and `conformance/run.sh`.
    if std::env::var_os("GHOST_ESCTEST").is_some() {
        esctest_host();
        return;
    }

    if let Some(path) = std::env::var_os("GHOST_CAPTURE") {
        capture(PathBuf::from(path));
    } else {
        interactive(fresh, ssh_window);
    }
}

/// Open one translucent window and print the NSWindow state that decides what
/// the compositor has to do with it (the `GHOST_WINDOW_DUMP` probe), then exit.
/// No session, no renderer, nothing drawn.
///
/// The load-bearing line is `bg_alpha`. A translucent window whose NSWindow
/// background is `clearColor` — or ANY colour with alpha 0 — is recomposited by
/// WindowServer continuously, forever, even while the window draws nothing:
/// measured at roughly double this machine's idle GPU utilisation for a single
/// idle window, and visible in Quartz Debug as a window that never stops
/// flashing. Any non-zero alpha avoids it. kitty carries the same workaround for
/// its own reasons (`glfw/cocoa_window.m`: `colorWithWhite:0 alpha:0.001`).
#[cfg(target_os = "macos")]
fn window_dump() {
    use objc2::rc::Retained;
    use objc2_app_kit::{NSView, NSWindow};
    struct DumpApp;
    impl ApplicationHandler for DumpApp {
        fn resumed(&mut self, event_loop: &ActiveEventLoop) {
            // Created exactly as a translucent app window is — `with_transparent`
            // is what pulls in the background colour we are asserting about.
            let window = event_loop
                .create_window(Window::default_attributes().with_transparent(true))
                .expect("create window");
            let ns: Retained<NSWindow> = {
                use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
                let handle = window.window_handle().expect("window handle");
                let RawWindowHandle::AppKit(h) = handle.as_raw() else {
                    panic!("not an AppKit window");
                };
                // SAFETY: on macOS the AppKit handle's `ns_view` is a live NSView
                // owned by the window we just created.
                let view: &NSView = unsafe { &*h.ns_view.as_ptr().cast::<NSView>() };
                view.window().expect("the view is in a window")
            };
            let (opaque, bg_alpha) =
                unsafe { (ns.isOpaque(), ns.backgroundColor().alphaComponent()) };
            println!("opaque={opaque}");
            println!("bg_alpha={bg_alpha}");
            event_loop.exit();
        }
        fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
    }
    let event_loop = EventLoop::new().expect("event loop");
    let _ = event_loop.run_app(&mut DumpApp);
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
    // Report the theme and policy *before* the resize: the resize completes the
    // attach handshake and triggers the host's resync (and spawns a deferred child),
    // so reporting the policy first means that resync already carries scrubbed state
    // and the child's first queries are answered under this terminal's policy rather
    // than the host's previous one being applied and then walked back.
    s.report_theme(session_theme())?;
    s.report_policy(session_policy())?;
    s.resize(cols, rows)?;
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
    proto: u32,
) -> io::Result<Session> {
    let mut s = Session::attach_deferred_ssh(cmd, name, proto)?;
    // Non-blocking, like the local [`attach`]: `about_to_wait` pumps this session
    // on the frame loop and the ssh pipe never wakes winit, so a bounded wait buys
    // no latency. (On the ssh transport `set_read_timeout(Some(_))` already maps to
    // non-blocking; spell it the same way as `attach` so the two don't drift.)
    s.set_nonblocking(true)?;
    // Report the theme and policy *before* the resize: the resize completes the
    // attach handshake and triggers the host's resync (and spawns a deferred child),
    // so reporting the policy first means that resync already carries scrubbed state
    // and the child's first queries are answered under this terminal's policy rather
    // than the host's previous one being applied and then walked back.
    s.report_theme(session_theme())?;
    s.report_policy(session_policy())?;
    s.resize(cols, rows)?;
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

/// Where a background worker posts its result for the main loop to apply.
///
/// Everything ghost does off the event loop — watching a host's session set,
/// connecting, reconnecting, reattaching, spawning a remote session — is a thread
/// that ends by posting a [`UserEvent`]. In the real app that is winit's event-loop
/// proxy; behind this trait it can also be a queue a test drains
/// ([`QueuedEvents`]), which is what lets those workers — the whole of ghost's
/// remote recovery — be driven against a real host without a window server.
pub trait EventSink: Send + Sync + 'static {
    /// Post `event`, returning whether it will be delivered. A closed event loop
    /// (the app is exiting) answers `false`, which is a worker's cue to stop.
    fn post(&self, event: UserEvent) -> bool;
}

impl EventSink for winit::event_loop::EventLoopProxy<UserEvent> {
    fn post(&self, event: UserEvent) -> bool {
        self.send_event(event).is_ok()
    }
}

/// An [`EventSink`] that collects what the workers post, for tests: real threads
/// doing real work, with the main loop's `on_user_event` driven by the test instead
/// of by winit. [`take`](QueuedEvents::take) drains what has arrived so far.
#[derive(Default)]
pub struct QueuedEvents(std::sync::Mutex<Vec<UserEvent>>);

impl QueuedEvents {
    /// Everything posted since the last drain, in order.
    pub fn take(&self) -> Vec<UserEvent> {
        self.0
            .lock()
            .map(|mut q| std::mem::take(&mut *q))
            .unwrap_or_default()
    }
}

impl EventSink for QueuedEvents {
    fn post(&self, event: UserEvent) -> bool {
        match self.0.lock() {
            Ok(mut q) => {
                q.push(event);
                true
            }
            Err(_) => false,
        }
    }
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
    sink: Arc<dyn EventSink>,
) -> RemoteWatcher {
    use std::sync::atomic::Ordering::Relaxed;
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let child: Arc<std::sync::Mutex<Option<std::process::Child>>> = Arc::default();
    let (t_stop, t_child) = (stop.clone(), child.clone());
    std::thread::spawn(move || {
        let mut failures: u32 = 0;
        while !t_stop.load(Relaxed) {
            let pushed = watch_stream_once(&target, &host, &sink, &t_stop, &t_child);
            if t_stop.load(Relaxed) {
                break;
            }
            if pushed {
                failures = 0;
            } else {
                failures = failures.saturating_add(1);
                // Unreachable for a grace period: clear the host's stale tiles,
                // and its remembered-set with them — judging members by a stale
                // set could forget one whose descriptor outlived the fetch.
                if failures >= REMOTE_WATCH_MAX_FAILURES
                    && (!sink.post(UserEvent::RemoteSessions {
                        target: target.clone(),
                        infos: Vec::new(),
                    }) || !sink.post(UserEvent::RemoteRemembered {
                        target: target.clone(),
                        names: None,
                    }))
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
    sink: &Arc<dyn EventSink>,
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
    let mut warned_parse = false;
    let mut last_line: Option<String> = None;
    for line in std::io::BufReader::new(stdout).lines() {
        if stop.load(Relaxed) {
            break;
        }
        let Ok(line) = line else { break };
        let infos = match ghost_vt::watch::parse_listing(&line) {
            Ok(infos) => infos,
            // A parse failure means every line from this host fails the same way (a
            // field mismatch, not a torn line), so it silently costs the whole remote
            // fleet. Say so once per stream instead of dropping it without a trace.
            Err(e) => {
                if !warned_parse {
                    warned_parse = true;
                    eprintln!(
                        "ghost: cannot parse the session listing from {target} ({e}); \
                         its fleet will not update"
                    );
                }
                continue;
            }
        };
        pushed = true;
        // A *changed* listing may mean a session ended — refresh the host's
        // remembered-set (its descriptor names) so the dead-member sweep can
        // tell a clean exit (forget) from an unclean one (relaunchable). The
        // heartbeat re-emits of an unchanged listing skip the extra round trip.
        // A failed fetch posts `None`: unknown, never stale.
        let changed = last_line.as_deref() != Some(line.as_str());
        last_line = Some(line);
        let infos = namespace_remote_infos(target, infos);
        if !sink.post(UserEvent::RemoteSessions {
            target: target.to_string(),
            infos,
        }) {
            stop.store(true, Relaxed); // event loop gone: end the whole watcher
            break;
        }
        if changed
            && !sink.post(UserEvent::RemoteRemembered {
                target: target.to_string(),
                names: host.remote.remembered_sessions(&host.remote_ghost).ok(),
            })
        {
            stop.store(true, Relaxed);
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
    sink: Arc<dyn EventSink>,
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
                    let _ = sink.post(UserEvent::ConnectProgress {
                        wid,
                        sent: p.sent,
                        total: p.total,
                    });
                };
                match remote.negotiate_with_progress(&mut on_progress) {
                    Ok(remote_ghost) => match remote.spawn_host(&remote_ghost, &name) {
                        Ok(()) => ConnectOutcome::Transport { remote_ghost },
                        Err(e) => {
                            ConnectOutcome::Error(format!("could not start the remote host: {e}"))
                        }
                    },
                    Err(why) => ConnectOutcome::Fallback(why),
                }
            }
            Err(e) => ConnectOutcome::Error(format!("could not open the ssh connection: {e}")),
        };
        let _ = sink.post(UserEvent::ConnectFinished {
            wid,
            generation,
            spec,
            name,
            outcome,
        });
    });
}

/// Reconnect to `spec`'s host in the background, **retrying until it answers**:
/// open the ControlMaster non-interactively (key/agent auth only, no PTY and no
/// prompt), negotiate a usable remote ghost, then post
/// [`UserEvent::RemoteReconnected`] so the main loop re-adopts the host's
/// remembered sessions.
///
/// The retry is the point. A host that is rebooting, asleep, or off the network is
/// not a host that has lost its sessions — they are still there when it comes
/// back. So this waits, with the same capped backoff a dropped session's probe
/// uses (1s doubling to 30s: a drop of seconds or of days recovers), until `stop`
/// is set — which happens when nothing remembers a session on that host any more
/// (see [`App::retry_remembered_hosts`]). A password-only host never succeeds
/// under `BatchMode`; its tiles keep waiting, and the user can connect explicitly.
fn spawn_remote_reconnect(
    sink: Arc<dyn EventSink>,
    spec: ConnectionSpec,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;
    std::thread::spawn(move || {
        let mut backoff = RECONNECT_BACKOFF;
        loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            if let Ok(remote) = ghost_vt::remote::RemoteSsh::new(spec.clone()) {
                // A silent partition leaves a wedged master behind, and only a fresh
                // probe can tell the host is back (see `spawn_reconnect_probe`).
                remote.reap_wedged_master();
                if remote.open_master_batch() {
                    match remote.negotiate() {
                        Ok(remote_ghost) => {
                            sink.post(UserEvent::RemoteReconnected { spec, remote_ghost });
                            return;
                        }
                        // Reachable but unusable as a transport (no remote ghost, a
                        // staging failure). Say so once per round and keep waiting:
                        // the host may finish booting into a usable state.
                        Err(why) => eprintln!(
                            "ghost: reconnecting {} over ssh: {why}; still waiting",
                            spec.target()
                        ),
                    }
                }
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

/// The retry floor / cap for a reconnecting session: doubles from 1s to 30s so a
/// drop of seconds or of days recovers, without hammering an unreachable host.
const RECONNECT_BACKOFF: Duration = Duration::from_secs(1);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// A wake-to-wake gap in the event loop long enough to mean the process — or the
/// whole machine — was parked (a slept laptop, a SIGSTOP). Sleep kills remote TCP
/// with no FIN/RST reaching the local ssh master, so on such a resume the remote
/// transports are probed ([`App::probe_remote_transports`]) instead of letting the
/// user type into a dead pipe until ssh's ~45s keepalive notices. An idle-but-awake
/// loop can also park this long; the probe is bounded and a no-op on a healthy
/// master, so a false suspicion is cheap.
const SUSPEND_PROBE_GAP: Duration = Duration::from_secs(10);

/// How long input may sit in a session's queue before it counts as a real stall
/// rather than a frame of backpressure. A paste larger than the socket buffer
/// queues briefly and drains; below this, say nothing.
const INPUT_STALL_GRACE: Duration = Duration::from_millis(250);

/// How long a session's input queue may make NO progress at all before we stop
/// believing its write path. Keystrokes are a handful of bytes: a transport that
/// cannot take them in this long is not slow, it is wedged — and the user is
/// typing into a void with nothing on screen to say so.
const INPUT_STALL_PROBE: Duration = Duration::from_secs(3);

/// What [`InputStall::observe`] concluded about a session's input queue.
#[derive(Debug, PartialEq, Eq)]
enum StallEvent {
    /// The queue has not moved for [`INPUT_STALL_PROBE`]: the write path has
    /// stopped taking bytes. `bytes` is the backlog, `waited` how long it has sat.
    Wedged { bytes: usize, waited: Duration },
    /// The queue emptied after a wait worth reporting. `bytes` is the deepest the
    /// backlog got, `waited` the whole episode.
    Drained { bytes: usize, waited: Duration },
}

/// One session's view of input that [`Session::send_input`] accepted but the
/// transport has not written yet.
///
/// `Conn::send` queues whatever a non-blocking write refuses and reports success,
/// so from above a wedged write path is indistinguishable from a healthy one: the
/// tile renders, the keys vanish, and nothing says so. This watches the queue for
/// *progress* — a slow link keeps draining and is left alone; one that has stopped
/// entirely is named, and its transport probed (see [`App::note_input_queue`]).
#[derive(Debug, Default)]
struct InputStall {
    episode: Option<Episode>,
}

/// A single run of non-empty queue, from the first byte left unwritten to the
/// moment the backlog clears.
#[derive(Debug)]
struct Episode {
    /// When the queue first went non-empty — what the drain report accounts for.
    opened_at: Instant,
    /// When the backlog last got *smaller*. Progress restarts the wedged clock,
    /// so a slow transport is never mistaken for a stopped one.
    progress_at: Instant,
    /// The smallest backlog seen since `progress_at`: the yardstick for progress.
    low_water: usize,
    /// The deepest the backlog got, for the drain report.
    peak: usize,
    /// Whether this episode was already reported wedged — the pump observes every
    /// 8ms, and one stall is one line, not hundreds.
    named: bool,
}

impl InputStall {
    /// Fold this pump's queue depth in, and say whether it changed the verdict.
    fn observe(&mut self, pending: usize, now: Instant) -> Option<StallEvent> {
        let Some(ep) = &mut self.episode else {
            if pending > 0 {
                self.episode = Some(Episode {
                    opened_at: now,
                    progress_at: now,
                    low_water: pending,
                    peak: pending,
                    named: false,
                });
            }
            return None;
        };
        if pending == 0 {
            let ep = self.episode.take()?;
            let waited = now.saturating_duration_since(ep.opened_at);
            // A queue that cleared inside the grace was backpressure, not a stall;
            // one we called wedged is always accounted for, however it ended.
            return (ep.named || waited >= INPUT_STALL_GRACE).then_some(StallEvent::Drained {
                bytes: ep.peak,
                waited,
            });
        }
        ep.peak = ep.peak.max(pending);
        if pending < ep.low_water {
            ep.progress_at = now;
            ep.low_water = pending;
            return None;
        }
        let waited = now.saturating_duration_since(ep.progress_at);
        if ep.named || waited < INPUT_STALL_PROBE {
            return None;
        }
        ep.named = true;
        Some(StallEvent::Wedged {
            bytes: pending,
            waited,
        })
    }
}

/// Probe a dropped remote session's host in the background until it is reachable
/// again and the session `real` still exists, then post
/// [`UserEvent::RemoteReattachReady`] so the main loop re-attaches at the current
/// grid. Retries forever with a capped backoff — a partition of minutes or days
/// recovers when the host returns — reaping the wedged master each round (a silent
/// drop leaves one, and only a fresh probe can tell the host is back). Stops when
/// `stop` is set (the window closed or the session was reattached elsewhere). The
/// blocking `ssh` (which can hang on an unreachable host) must stay off the loop.
fn spawn_reconnect_probe(
    sink: Arc<dyn EventSink>,
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
                    let _ = sink.post(UserEvent::RemoteReattachReady { wid, name });
                    return;
                }
                // Host reachable but the session is GONE — the host rebooted and
                // wiped it (as opposed to a mere network partition). Waiting can't
                // bring it back, so end the hold; the tile falls to the fleet, where
                // the dead session can be relaunched.
                Ok(_) => {
                    let _ = sink.post(UserEvent::RemoteSessionGone { wid, name });
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
    session_set_watcher_in(&ghost_vt::paths::runtime_dir(), flag)
}

/// [`session_set_watcher`] over an explicit `dir`, so tests can drive it against
/// a tempdir without touching the real XDG location.
fn session_set_watcher_in(
    dir: &std::path::Path,
    flag: Arc<std::sync::atomic::AtomicBool>,
) -> Option<notify::RecommendedWatcher> {
    use notify::Watcher;
    // The dir may not exist before the first session; create it so the watch
    // can bind now (hosts create it on demand anyway).
    std::fs::create_dir_all(dir).ok()?;
    let mut w = notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
        if let Ok(ev) = res
            && fs_event_is_a_change(&ev)
        {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    })
    .ok()?;
    w.watch(dir, notify::RecursiveMode::NonRecursive).ok()?;
    Some(w)
}

/// Whether a filesystem notification reports the watched content actually
/// *changing* (create/write/rename/remove), as opposed to merely being *read*.
///
/// Access (read) events MUST not count as changes: the reaction to a change is
/// to re-read the watched thing (re-enumerate the session dir, reload the
/// config file), and on inotify that read itself raises `IN_ACCESS` — so
/// treating a read as a change turns one trigger into a self-sustaining loop
/// that re-fires at event-loop frequency forever. The config reload loop wiped
/// every cached session surface each iteration (`Renderer::set_theme`), which
/// blanked all terminal content for the whole length of any dive/slide — the
/// animation's deferred-raster path has no cached surface left to blit. macOS
/// was immune (FSEvents reports no reads), which is why the blank looked
/// Linux-specific. Same gotcha as the host-side `__watch` Access filter.
fn fs_event_is_a_change(ev: &notify::Event) -> bool {
    !matches!(ev.kind, notify::EventKind::Access(_))
}

/// Watch the config dir and raise `flag` when `ui.toml` may have changed — the
/// trigger for a live config reload. Watches the DIRECTORY, not the file:
/// editors replace-on-save (write a temp, rename over), which would drop an
/// inode watch bound to the file. Filtered to the config filename so an
/// unrelated write in the dir doesn't reload (an event with no path — some
/// backends — falls through as a change, since a reload is cheap and
/// idempotent). `None` (no dir, no backend) leaves the config launch-only.
fn config_watcher(flag: Arc<std::sync::atomic::AtomicBool>) -> Option<notify::RecommendedWatcher> {
    config_watcher_in(&ghost_vt::paths::config_dir(), flag)
}

/// [`config_watcher`] over an explicit `dir`, so tests can drive it against a
/// tempdir without touching the real XDG location.
fn config_watcher_in(
    dir: &std::path::Path,
    flag: Arc<std::sync::atomic::AtomicBool>,
) -> Option<notify::RecommendedWatcher> {
    use notify::Watcher;
    std::fs::create_dir_all(dir).ok()?;
    let mut w = notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
        if let Ok(ev) = res
            && fs_event_is_a_change(&ev)
        {
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
    w.watch(dir, notify::RecursiveMode::NonRecursive).ok()?;
    Some(w)
}

/// The theme reported to session hosts at attach; fixed at startup, so read
/// from the config once.
fn session_theme() -> ghost_ui_core::ThemeColors {
    static THEME: std::sync::OnceLock<ghost_ui_core::ThemeColors> = std::sync::OnceLock::new();
    *THEME.get_or_init(|| theme_colors(&config::UiConfig::load().theme()))
}

/// The policy reported to session hosts at attach — what a program on the tty may
/// change about the terminal (see `ghost_term::policy`).
///
/// Every field is on today: the seam is wired end to end, but nothing has decided
/// yet that a stranger's tty may not, say, retitle your window, and that decision
/// is the user's to make in config, not one to smuggle in here. When it lands, it
/// lands in this function.
fn session_policy() -> ghost_term::TerminalPolicy {
    session_policy_pair().terminal
}

/// The policy this terminal enforces: what a program on a session's tty may change
/// about the terminal, and what it may do outside it (see `ghost_term::policy`).
///
/// ONE source for both halves and for both places they have to reach — the session
/// host (reported at attach, `session_policy`) and this window's own emulators
/// (`RootModel::set_policy`). They must agree; a window that allowed what the host
/// refuses would show state that vanishes on the next reattach.
///
/// Every field is on today: the seam is wired end to end, but nothing has decided
/// yet that a stranger's tty may not, say, retitle your window, and that decision is
/// the user's to make in config, not one to smuggle in here. When it lands, it lands
/// in this function.
fn session_policy_pair() -> ghost_term::SessionPolicy {
    ghost_term::SessionPolicy::default()
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

// ---- esctest conformance host (headless) -------------------------------

/// Drive the terminal-conformance suite (`conformance/esctest2`, GPLv2, run as a
/// separate child — it links into nothing; see `conformance/README.md`).
///
/// `conformance/run.sh` sets `GHOST_ESCTEST` to the esctest invocation (a
/// `python3 …` command) and redirects the XDG dirs to a tempdir, then runs
/// `ghost`. Here we spawn that command as a real ghost session's child and drive
/// the SAME [`TerminalModel`] the GUI uses over the PTY: esctest writes control
/// sequences to its stdout (the model feeds on them) and reads our replies (CPR,
/// text-area size, later DECRQCRA) from its stdin — the model's `SendInput`
/// effects, written back here by [`exec_headless`]. esctest checks each reply
/// against xterm and writes a pass/fail report to its `--logfile`, which
/// `run.sh` greps. No renderer: esctest only observes what a program can.
fn esctest_host() {
    // The child command comes from run.sh (the esctest invocation), mirroring
    // `GHOST_CMD`. `1` is the bare on-marker used by the env gate below.
    let command = match std::env::var("GHOST_ESCTEST") {
        Ok(c) if !c.is_empty() && c != "1" => vec!["sh".to_string(), "-c".to_string(), c],
        _ => {
            eprintln!("GHOST_ESCTEST must hold the esctest command to run");
            std::process::exit(2);
        }
    };
    // esctest normalises the terminal to 25 rows x 80 cols and reads the size
    // back with `CSI 18 t`; spawn at that size so our truthful reply matches.
    const ECOLS: u16 = 80;
    const EROWS: u16 = 25;
    // Keep the name short: it becomes a path component of the control socket,
    // which must fit `sockaddr_un` (~104 bytes). The XDG runtime dir is a
    // private tempdir (run.sh), so `esct-<pid>` is already collision-free.
    let name = format!("esct-{}", std::process::id());
    server::spawn(SpawnOpts {
        name: name.clone(),
        command,
        size: (ECOLS, EROWS),
        cwd: None,
        record: None,
        seed_from: None,
        scrollback: screen::DEFAULT_SCROLLBACK,
        max_recording_bytes: None,
        start_on_attach: true,
        connection: None,
    })
    .expect("spawn esctest session");

    let mut session = attach_retry(&name, ECOLS, EROWS);
    let mut model = TerminalModel::new(name.clone(), ECOLS, EROWS, metrics());
    // esctest is measuring the terminal, not the user's preferences: it drives the
    // window ops, the title stack, the palette and the rest, and a conformance
    // number that quietly fell because we decided some of it was unsafe for a
    // stranger's tty would be a lie about the emulator. So the harness asks for
    // everything, explicitly, whatever the defaults become (see `ghost_term::policy`).
    model.set_terminal_policy(ghost_term::TerminalPolicy::allow_all());
    model.set_action_policy(ghost_term::ActionPolicy::allow_all());
    // The harness "window" is focused, so focus-reporting queries answer
    // consistently (and a DEC ?1004 enable reports focused).
    exec_headless(&mut session, &model.update(UiEvent::Focus(true)));

    // Ping-pong until esctest exits: feed each control-sequence burst into the
    // model and write every reply back. Unlike `capture`, never break on an
    // output lull — esctest pauses between hundreds of tests. A generous
    // wall-clock cap is the only backstop against a wedged child.
    let start = Instant::now();
    let deadline = Duration::from_secs(
        std::env::var("GHOST_ESCTEST_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(300),
    );
    loop {
        let (bytes, end) = pump(&mut session, 256);
        let ended = end.is_end();
        if !bytes.is_empty() || ended {
            let cmds = model.update(UiEvent::SessionData {
                name: name.clone(),
                bytes,
                ended,
            });
            exec_headless(&mut session, &cmds);
        }
        if model.ended() || ended {
            break;
        }
        if start.elapsed() > deadline {
            eprintln!("esctest host timed out after {}s", deadline.as_secs());
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }

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

/// Decide how to start: honour an explicit `$GHOST_SESSION` request; otherwise open
/// the fleet only when it has something the user can **return to**, and spawn a
/// fresh session otherwise.
///
/// "Return to" means a session that still exists and would be handed back with its
/// state: one that is live but detached (local or remote), or a remembered *remote*
/// member whose host is currently unreachable — that one reconnects when the host
/// comes back, so the fleet is where you wait for it.
///
/// A remembered **dead local** member deliberately does not count, though the fleet
/// does show it as a relaunchable tile (see
/// `a_remembered_dead_member_still_shows_a_tile_in_the_fleet`). Relaunching one
/// spawns `$SHELL` seeded with the old scrollback — no command is re-run — so it
/// offers nothing a new session doesn't, and it is *permanent*: every window that
/// ever ran leaves an auto-group in `groups.toml`, and detaching keeps membership,
/// so one long-dead member used to send every later launch and every Alt-N into a
/// fleet whose only other content was sessions held by other windows. Those are
/// reachable from a session view with F9 whenever the user actually wants them.
fn startup_choice(
    requested: Option<String>,
    sessions: &[session::SessionInfo],
    groups: &[ghost_ui_core::Group],
) -> StartupChoice {
    let listed = |name: &String| sessions.iter().any(|s| &s.name == name);
    // A remote member nothing lists right now: its host is away, and the tile holds
    // (and reconnects) rather than relaunching. See [`App::begin_reconnect`].
    let awaiting_remote = groups
        .iter()
        .flat_map(|g| &g.members)
        .any(|m| is_remote_id(m) && !listed(m));
    match requested {
        Some(name) => StartupChoice::Attach(name),
        None if sessions.iter().any(|s| !s.attached) || awaiting_remote => StartupChoice::Fleet,
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
    /// Open the "connect to a host" prompt (`ghost --ssh-window`): the launch-time
    /// twin of the new-ssh-window shortcut.
    Connect,
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

fn interactive(fresh: bool, ssh_window: bool) {
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

    // The CSD frame builds its title renderer when the first window is created,
    // so ghost's text stack has to be in place before that — see `title`.
    #[cfg(target_os = "linux")]
    title::install();

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
        let want = if ssh_window {
            instance::Request::NewSshWindow
        } else {
            instance::Request::NewWindow
        };
        match instance::acquire(want) {
            instance::Role::Secondary => return,
            instance::Role::Primary { _lock, listener } => (_lock, listener),
        }
    };
    let groups = groups::load();
    let workspace = windows::load();
    let startup = if harness.is_some() {
        Startup::Fleet // the harness populates and dives it
    } else if ssh_window {
        // `--ssh-window` is an explicit ask: skip the restore/reconnect choice and
        // open the connect prompt, exactly like the in-app shortcut.
        Startup::Connect
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
    let sink: Arc<dyn EventSink> = Arc::new(proxy.clone());
    if let Some(listener) = instance_listener {
        let sink = sink.clone();
        instance::serve(listener, move |req| {
            sink.post(match req {
                instance::Request::NewWindow => UserEvent::OpenWindow,
                instance::Request::NewSshWindow => UserEvent::OpenSshWindow,
            });
        });
    }
    let remotes: Arc<std::sync::Mutex<HashMap<String, RemoteHost>>> = Arc::default();
    let sessions_changed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let config_changed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let next_group_color = (groups.len() % ghost_ui_core::group::GROUP_PALETTE.len()) as u8;
    let mut app = App {
        windows: HashMap::new(),
        states: Sessions::new(),
        sessions: HashMap::new(),
        observers: HashMap::new(),
        dead_fed: HashSet::new(),
        clipboard: None,
        start: Instant::now(),
        startup,
        next_session_seq: 0,
        next_group_seq: 0,
        next_group_color,
        bench: harness,
        focused: None,
        sink: Some(sink.clone()),
        proxy: Some(proxy),
        remotes,
        remote_infos: HashMap::new(),
        remote_remembered: HashMap::new(),
        remote_index: HashMap::new(),
        remote_watchers: HashMap::new(),
        pending_remote_restores: HashMap::new(),
        reconnecting: HashMap::new(),
        input_stalls: HashMap::new(),
        remote_retries: HashMap::new(),
        probing_remotes: Arc::default(),
        last_wake_at: Instant::now(),
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

/// How a window realizes its glass: compositor backdrop-blur where the platform
/// offers it, self-drawn frost where it doesn't. See [`glass`].
#[derive(Clone, Copy, Debug, PartialEq)]
struct Glass {
    /// Ask the compositor to blur what's behind the window.
    blur: bool,
    /// Self-drawn frost density to bake into the renderer theme; 0.0 = none.
    frost: f32,
}

/// Resolve a window's glass treatment.
///
/// Blur and frost are one intent — make the translucent background read as glass
/// — with two realizations, so ghost picks between them rather than asking the
/// user to. A real backdrop blur already diffuses and dims what's behind, so
/// frosting on top of it only muddies: frost is the fallback for the compositors
/// that can't blur (X11, Windows, a Wayland compositor with neither
/// `ext_background_effect_v1` nor `org_kde_kwin_blur`), not a companion to it.
///
/// Both ride the same translucency gate as the window's alpha: behind an opaque
/// window there is no backdrop to blur and nothing shows through to frost.
///
/// The blur *request* does not depend on `blur_active`, because it can't: on
/// Wayland the answer is per-window and only arrives after the surface exists,
/// and asking a platform that can't blur is a documented no-op. `blur_active` is
/// what the platform came back with, and it only decides whether to frost.
fn glass(translucent: bool, blur_supported: bool, cfg_frost: f32) -> Glass {
    Glass {
        blur: translucent,
        frost: if translucent && !blur_supported {
            cfg_frost
        } else {
            0.0
        },
    }
}

/// Whether the platform will really blur behind `window` — the answer [`glass`]
/// needs to choose between compositor glass and self-drawn frost.
///
/// Polled rather than cached: on Wayland the compositor re-advertises its
/// capabilities whenever they change, so a desktop blur effect switched off
/// mid-session has to take the frost fallback with it.
#[cfg(target_os = "macos")]
fn backdrop_blur_supported(_window: &Window) -> bool {
    // macOS blurs behind any window that asks.
    true
}

#[cfg(all(unix, not(target_os = "macos")))]
fn backdrop_blur_supported(window: &Window) -> bool {
    // Wayland: `ext_background_effect_v1`'s live capability, or KDE's blur
    // global. X11 has no backdrop blur and always answers `false`.
    use winit::platform::wayland::WindowExtWayland;
    window.blur_supported()
}

#[cfg(not(unix))]
fn backdrop_blur_supported(_window: &Window) -> bool {
    // No backdrop blur on Windows.
    false
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

/// The toolkit's name for one of our [`ResizeEdge`](ghost_ui_core::ResizeEdge)s.
/// winit derives the matching cursor from it, so the mapping is only written once.
#[cfg(all(unix, not(target_os = "macos")))]
fn resize_direction(edge: ghost_ui_core::ResizeEdge) -> winit::window::ResizeDirection {
    use ghost_ui_core::ResizeEdge as E;
    use winit::window::ResizeDirection as D;
    match edge {
        E::North => D::North,
        E::South => D::South,
        E::East => D::East,
        E::West => D::West,
        E::NorthEast => D::NorthEast,
        E::NorthWest => D::NorthWest,
        E::SouthEast => D::SouthEast,
        E::SouthWest => D::SouthWest,
    }
}

/// Everything about a window that decides what its edge looks like — read off the
/// real window by [`Graphics::window_edge`], so the decision itself
/// ([`window_edge_for`]) is testable without one.
#[cfg(all(unix, not(target_os = "macos")))]
#[derive(Clone, Copy)]
struct EdgeState {
    /// The surface is opaque, so its alpha never reaches the compositor.
    opaque: bool,
    /// The window has keyboard focus; the frame's shadow lightens without it.
    focused: bool,
    /// Maximized, fullscreen or tiled: no free outside corner to round.
    boxed_in: bool,
    /// ghost draws the decorations, so the top edge is ours too.
    own_frame: bool,
}

/// The window edge [`EdgeState`] calls for — see [`Graphics::window_edge`], whose
/// documentation this implements.
#[cfg(all(unix, not(target_os = "macos")))]
fn window_edge_for(state: EdgeState) -> WindowEdge {
    // sctk-adwaita's `CORNER_RADIUS`, so our bottom corners continue the curve
    // its titlebar starts — and, once we draw the whole frame, the radius the
    // windows beside us on this desktop wear.
    const RADIUS: f32 = 10.0;
    let radius = if state.opaque || state.boxed_in {
        0.0
    } else {
        RADIUS
    };
    let corners = if radius <= 0.0 {
        ghost_renderer::Corners::NONE
    } else if state.own_frame {
        ghost_renderer::Corners::ALL
    } else {
        ghost_renderer::Corners::default()
    };
    // Rounding a corner cuts a notch out of the window that the frame's own
    // subsurfaces cannot reach into, so we finish its shadow ourselves —
    // sampled from the frame, at the depth this radius opens up.
    //
    // Our own decorations cast no shadow at all yet: there is no frame, and no
    // margin outside the window to cast into. Continuing one there paints a grey
    // scoop into the notch of a window that is otherwise shadowless — over a
    // light backdrop, a wedge hanging off the curve. The notch keeps whatever
    // the window itself has until the margins land.
    let (margins, shadow) = window_shadow(state);
    let reach = radius * (std::f32::consts::SQRT_2 - 1.0);
    let mut corner_shadow = [0.0; ghost_renderer::EDGE_SHADOW_STEPS];
    if !state.own_frame {
        for (i, a) in corner_shadow.iter_mut().enumerate() {
            let d = reach * i as f32 / (ghost_renderer::EDGE_SHADOW_STEPS - 1) as f32;
            *a = sctk_adwaita::shadow::bottom_corner_alpha(d, state.focused);
        }
    }
    // The dark ring the frame draws around the window, which stops short of
    // the corners we round — read from the same theme the frame uses so the
    // line we carry on with is the line it drew.
    let outline = if state.boxed_in {
        0.0
    } else {
        Graphics::frame_outline()
    };
    WindowEdge {
        radius,
        corners,
        // No inset highlight. libadwaita traces one — `outline: 1px solid
        // rgb(255 255 255/7%)` — but the windows we sit next to on this
        // desktop do not: gnome-terminal's edge is the dark ring and nothing
        // else, and measured against it ours read as glassy piping, a bright
        // line curving round the corner where theirs has a deep one. The
        // ring carries the edge on its own.
        highlight: 0.0,
        outline,
        // The frame's border column is a thing outside the window, and with our
        // own decorations there is nothing out there to draw on — so the ring
        // moves inside, where it still traces the same edge.
        outline_inside: state.own_frame,
        margins,
        shadow,
        corner_shadow,
    }
}

/// How far out of the window we keep room for its shadow, in logical pixels.
///
/// The falloff is spent well inside this — under 0.005 by ~25 logical pixels —
/// and past the shadow the same room is the resize grab area, which is why
/// sctk reserves a good deal more than the shadow strictly needs.
#[cfg(all(unix, not(target_os = "macos")))]
const SHADOW_MARGIN: f32 = 26.0;

/// The room a window keeps around itself, and the shadow it casts into it.
///
/// Only a window that draws its own frame has any: with the CSD frame above us
/// the shadow is its subsurfaces' job, and a window with no free outside corner
/// — maximized, fullscreen, tiled — casts none at all, exactly as GNOME's own
/// do, so the margin goes with it rather than sitting there as dead surface.
#[cfg(all(unix, not(target_os = "macos")))]
fn window_shadow(state: EdgeState) -> (ghost_renderer::EdgeMargins, ghost_renderer::ShadowProfile) {
    if !state.own_frame || state.boxed_in {
        return Default::default();
    }
    // Sampled from the same layers the frame we are replacing casts, so a
    // ghost-framed window sits in the same light as the rest of the desktop.
    let profile = |down: f32| {
        let mut lut = [0.0; ghost_renderer::EDGE_SHADOW_STEPS];
        for (i, a) in lut.iter_mut().enumerate() {
            let d = SHADOW_MARGIN * i as f32 / (ghost_renderer::EDGE_SHADOW_STEPS - 1) as f32;
            *a = sctk_adwaita::shadow::edge_alpha(d, down, state.focused);
        }
        lut
    };
    (
        ghost_renderer::EdgeMargins::all(SHADOW_MARGIN),
        ghost_renderer::ShadowProfile {
            up: profile(-1.0),
            side: profile(0.0),
            down: profile(1.0),
        },
    )
}

/// Per-window GPU state, valid only once the window (and surface) exist. The frame
/// production itself lives in [`Target`] (shared with the headless harness); this
/// just owns the window, the surface target, and the per-window render state.
pub struct Graphics {
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
            mut theme,
            option_as_meta,
            cols,
            rows,
            pad,
            decorations,
        } = spec;
        // Open sized to `cols`x`rows` cells at the base font, plus the padding border on
        // each side, so the configured grid fits inside it (padding surrounds, not eats
        // into, the grid). A LOGICAL size (not physical) so winit scales it by the monitor
        // DPI — the grid then works out to exactly `cols`x`rows` at any scale
        // (`grid_from_pixels` divides physical px by cell·scale), which a physical size
        // would only get right at 1x.
        let m = metrics();
        // Our own titlebar eats into the window rather than sitting above it (the
        // desktop's frame is drawn outside), so the window has to open that much
        // taller or the configured grid arrives one bar short.
        let bar = if decorations == config::Decorations::Ghost {
            f64::from(ghost_ui_core::frame::BAR_HEIGHT)
        } else {
            0.0
        };
        let size = LogicalSize::new(
            f64::from(cols) * f64::from(m.advance) + 2.0 * f64::from(pad),
            f64::from(rows) * f64::from(m.line_height) + 2.0 * f64::from(pad) + bar,
        );
        // Request a transparent window only when the theme is translucent, so an
        // opaque setup never pays the compositor's alpha-blending cost.
        let want_transparent = theme.bg_alpha < 1.0;
        // Bench mode measures the render path at a realistic size, so open maximized
        // (the small default grid would understate per-frame raster cost).
        let maximized = std::env::var_os("GHOST_BENCH").is_some();
        // Ask the compositor for backdrop-blur whenever the window is translucent:
        // blur behind an opaque window is a no-op state, and where the compositor
        // can't blur the request is ignored (see `glass`). The other half of the
        // glass decision — the self-drawn frost that stands in for blur — can only
        // be settled once the window exists and the platform can be asked whether
        // the request took, so it happens just below.
        let attrs = Window::default_attributes()
            .with_title("ghost")
            .with_inner_size(size)
            .with_maximized(maximized)
            .with_transparent(want_transparent)
            .with_blur(glass(want_transparent, false, 0.0).blur);
        // `[window] decorations = "ghost"`: drop the desktop's frame and draw our
        // own. Wayland only — there the frame is a client-side one anyway (mutter
        // offers no server-side decorations), so taking it over changes who draws
        // pixels we already own. On X11 the window manager's frame is real, and
        // replacing it would mean reimplementing what it does for us.
        #[cfg(all(unix, not(target_os = "macos")))]
        let wayland = {
            use winit::raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
            event_loop
                .display_handle()
                .is_ok_and(|d| matches!(d.as_raw(), RawDisplayHandle::Wayland(_)))
        };
        #[cfg(all(unix, not(target_os = "macos")))]
        let attrs = if decorations == config::Decorations::Ghost && wayland {
            attrs.with_decorations(false)
        } else {
            attrs
        };
        // Freedesktop platforms match a window to its `.desktop` entry (icon, dock
        // grouping, the entry's actions) by app id / WM_CLASS, so it must be
        // [`APP_ID`] — the name the installed entry is filed under.
        #[cfg(all(unix, not(target_os = "macos")))]
        let attrs = {
            use winit::platform::wayland::WindowAttributesExtWayland;
            use winit::platform::x11::WindowAttributesExtX11;
            let attrs = WindowAttributesExtWayland::with_name(attrs, APP_ID, "ghost");
            WindowAttributesExtX11::with_name(attrs, APP_ID, "ghost")
        };
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        // `theme.frost` arrives holding the configured density; keep it only where
        // the compositor isn't blurring for us, so glass is never drawn twice.
        theme.frost = glass(
            want_transparent,
            backdrop_blur_supported(&window),
            theme.frost,
        )
        .frost;
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
        // Window chrome draws in the DESKTOP's UI font, not the terminal's — a
        // monospaced titlebar reads as a mistake next to every other window.
        #[cfg(target_os = "linux")]
        {
            let ui = desktop::desktop_font();
            if let Some(face) = font::resolve_face(&ui.family, ui.style.as_deref()) {
                renderer.set_chrome_font(
                    ghost_shaper::FontSet::single(face),
                    font::style_weight(ui.style.as_deref()),
                );
            } else {
                eprintln!(
                    "ghost-ui: no font for the desktop's titlebar family {:?}; \
                     window chrome will draw no text",
                    ui.family
                );
            }
        }
        // Keep the frost grain a fixed logical size on HiDPI.
        renderer.set_scale_factor(window.scale_factor() as f32);
        let edge = Self::window_edge(&window, !want_transparent, true);
        renderer.set_window_edge(edge);
        Self::shape_backdrop(&window, edge);

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

    /// What the platform's window frame leaves for us to draw — see [`WindowEdge`].
    ///
    /// On Linux GNOME offers no server-side decorations, so sctk-adwaita's CSD frame
    /// draws the titlebar: it rounds the window's *top* corners itself and its shadow
    /// curves around all four, leaving the bottom two — and the hairline that separates
    /// the content from whatever shows behind it — to us. Everywhere else the window
    /// server owns the whole edge (on macOS our vendored winit rounds the content layer
    /// itself), so we draw none of it.
    ///
    /// With `[window] decorations = "ghost"` there is no frame above us at all, so
    /// the top edge is ours too and the curve runs all the way round — the window is
    /// undecorated exactly when we asked for that, so the window itself is the source
    /// of truth.
    ///
    /// Only a translucent surface has a corner to cut: an opaque one's alpha never
    /// reaches the compositor, and zeroing it would paint the corners black rather than
    /// clear. A window with no free outside corner — maximized, fullscreen, or tiled
    /// against a screen edge or a neighbour — squares off and drops its outline, as
    /// GNOME does with its own.
    #[cfg(all(unix, not(target_os = "macos")))]
    fn window_edge(window: &Window, opaque: bool, focused: bool) -> WindowEdge {
        use winit::platform::wayland::WindowExtWayland;
        window_edge_for(EdgeState {
            opaque,
            focused,
            // A half-snapped window is tiled but NOT maximized, so both are asked.
            boxed_in: window.is_maximized() || window.fullscreen().is_some() || window.is_tiled(),
            own_frame: !window.is_decorated(),
        })
    }

    #[cfg(not(all(unix, not(target_os = "macos"))))]
    fn window_edge(_window: &Window, _opaque: bool, _focused: bool) -> WindowEdge {
        WindowEdge::default()
    }

    /// Alpha of the frame's outer border, asked once.
    ///
    /// `ColorTheme::auto()` picks light or dark by *spawning `dbus-send`* and
    /// waiting up to 100ms for the portal, so it must not be on the path of
    /// something as ordinary as a resize. The frame reads it once for the same
    /// reason, and does not follow a live light/dark switch either.
    #[cfg(all(unix, not(target_os = "macos")))]
    fn frame_outline() -> f32 {
        static OUTLINE: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
        *OUTLINE.get_or_init(|| {
            sctk_adwaita::theme::ColorTheme::auto()
                .active
                .outer_border
                .alpha()
        })
    }

    /// Our titlebar's height in physical pixels, or 0 when the desktop draws the
    /// frame. Every place the bar shifts something — the model's size, the scene,
    /// the pointer, the IME box — takes it from here, so they cannot drift apart.
    fn bar_px(&self) -> u32 {
        ghost_ui_core::frame::bar_height_px(
            !self.window.is_decorated(),
            self.window.scale_factor() as f32,
        )
    }

    /// The room this window keeps around itself for its shadow, in physical
    /// pixels — the second thing, after the bar, that moves the window off the
    /// surface's origin. Read off the same [`WindowEdge`] the renderer draws
    /// from, so the shape, the shadow and the coordinates cannot disagree.
    fn margins_px(&self) -> ghost_ui_core::frame::FrameInset {
        let m = self.renderer.window_edge().margins;
        let scale = self.window.scale_factor() as f32;
        let px = |v: f32| (v * scale).max(0.0).round() as u32;
        ghost_ui_core::frame::FrameInset {
            top: px(m.top),
            right: px(m.right),
            bottom: px(m.bottom),
            left: px(m.left),
        }
    }

    /// The size of the window inside this surface: what the shell must lay the
    /// frame and the model out in.
    fn window_px(&self) -> (u32, u32) {
        self.margins_px().window(self.size())
    }

    /// The titlebar to draw over this window's content: its height, its colours
    /// for the window's current focus, and the title.
    ///
    /// The colours come from the same desktop theme the CSD frame we are
    /// replacing uses, so a ghost-framed window sits alongside the rest of the
    /// desktop rather than beside it. Read once, for the reason
    /// [`frame_outline`](Self::frame_outline) explains.
    fn titlebar(&self, w: &WindowState) -> ghost_ui_core::frame::Titlebar {
        let (bg, fg) = Self::titlebar_colors(w.focused);
        let scale = self.window.scale_factor() as f32;
        ghost_ui_core::frame::Titlebar {
            height_px: self.bar_px(),
            bg,
            fg,
            title: w.title.clone(),
            font_px: desktop::desktop_font().px_size(scale),
            buttons: desktop::button_layout(),
            hovered: w.hovered_button,
            pressed: w.pressed_button,
            maximized: self.window.is_maximized(),
            scale,
        }
    }

    /// The desktop theme's headerbar background and title colour, for a focused
    /// or backdropped window.
    #[cfg(all(unix, not(target_os = "macos")))]
    fn titlebar_colors(focused: bool) -> (ghost_render::scene::Rgba, ghost_render::scene::Rgba) {
        static THEME: std::sync::OnceLock<[[[f32; 4]; 2]; 2]> = std::sync::OnceLock::new();
        let t = THEME.get_or_init(|| {
            let theme = sctk_adwaita::theme::ColorTheme::auto();
            let rgba = |c: sctk_adwaita::theme::Color| [c.red(), c.green(), c.blue(), c.alpha()];
            [
                [
                    rgba(theme.inactive.headerbar),
                    rgba(theme.inactive.font_color),
                ],
                [rgba(theme.active.headerbar), rgba(theme.active.font_color)],
            ]
        });
        let pair = t[usize::from(focused)];
        (pair[0], pair[1])
    }

    #[cfg(not(all(unix, not(target_os = "macos"))))]
    fn titlebar_colors(_focused: bool) -> (ghost_render::scene::Rgba, ghost_render::scene::Rgba) {
        ([0.0, 0.0, 0.0, 1.0], [1.0, 1.0, 1.0, 1.0])
    }

    /// Re-decide the window edge after something it depends on moved: the window
    /// was maximized or restored, or focus came or went (the frame's shadow
    /// lightens in the backdrop, and the corner has to lighten with it).
    fn refresh_window_edge(&mut self, focused: bool) {
        let edge = Self::window_edge(&self.window, self.target.opaque(), focused);
        self.renderer.set_window_edge(edge);
        // Changing the margins resizes the SURFACE — a maximized window drops
        // them, a restored one takes them back — and no configure follows to
        // tell us. Catch it here, or the swapchain and the scene stay the size
        // the window was before the change and the window wears a band of dead
        // surface down the two edges the difference landed on.
        let surface = Self::shape_backdrop(&self.window, edge);
        tracing::trace!(
            target: "ghost::frame",
            was = ?self.size(),
            now = ?surface,
            inner = ?self.window.inner_size(),
            margins = ?edge.margins,
            radius = edge.radius,
            "edge refreshed"
        );
        if let Some((w, h)) = surface
            && (w, h) != self.size()
            && w > 0
            && h > 0
        {
            if let Target::Surface(s) = &mut self.target {
                s.resize(w, h);
            }
            self.scene_cache.invalidate();
        }
    }

    /// Cut the compositor's backdrop effect to the corners we round.
    ///
    /// The effect fills the surface's rectangle, and a corner we round is a
    /// corner we cut to fully transparent — which is precisely where the blur
    /// then shows at full strength, with none of our own content dimming it. It
    /// reads as a bright wedge poking out of the curve, which is the same way
    /// this went wrong on macOS (there AppKit's own blur and shadow trace the
    /// content layer, so clipping the layer fixed both at once).
    ///
    /// Which corners we round is the edge's to say: while a CSD frame draws the
    /// titlebar it rounds the top itself and only the bottom two are ours, but
    /// with our own decorations all four are — and a top corner left square here
    /// wears exactly the same wedge the bottom ones used to.
    /// Returns the surface's size, which the margins decide.
    #[cfg(all(unix, not(target_os = "macos")))]
    fn shape_backdrop(window: &Window, edge: WindowEdge) -> Option<(u32, u32)> {
        use winit::platform::wayland::{DecorationMargins, WindowExtWayland};
        // Ask for the room the shadow needs *first*: it resizes the surface, and
        // the effect region below is spelled out against the window inside it.
        // The room a *floating* window keeps, not the room this one has: winit
        // drops it while the window is maximized, fullscreen or tiled, from the
        // configure that says so. Zeroing it from here instead would size the
        // surface a second time for one state change, and the compositor would
        // get two differently-sized buffers in the middle of its own animation.
        // `edge.margins` is the room in force, which is what we lay out in.
        let room = if edge.outline_inside {
            SHADOW_MARGIN
        } else {
            0.0
        } as u32;
        let size = window.set_decoration_margins(DecorationMargins {
            top: room,
            right: room,
            bottom: room,
            left: room,
        });
        let radius = edge.radius.max(0.0) as u32;
        let of = |rounded: bool| if rounded { radius } else { 0 };
        window.set_blur_corner_radii(of(edge.corners.top), of(edge.corners.bottom));
        Some((size.width, size.height))
    }

    #[cfg(not(all(unix, not(target_os = "macos"))))]
    fn shape_backdrop(_window: &Window, _edge: WindowEdge) -> Option<(u32, u32)> {
        None
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
        // Maximizing squares the window's corners off — and every state change that
        // can do that reaches us as a resize.
        self.refresh_window_edge(self.window.has_focus());
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

    /// Blit the renderer's held resize snapshot to the surface, unstretched — immediate
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
            // What's on screen is the held snapshot, not a model scene; keep the
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
pub struct WindowSpec {
    /// The renderer theme. Its `frost` is the *configured* density; the realized
    /// window keeps it only where the compositor won't blur for us (see [`glass`]).
    theme: ghost_renderer::Theme,
    option_as_meta: bool,
    cols: u16,
    rows: u16,
    pad: f32,
    /// Who draws the window frame — see `ghost-ui/docs/window-decorations.md`.
    decorations: config::Decorations,
}

/// A realized window handed back by the [`Frontend`]: its id, physical size, and
/// scale (enough to size a model), plus its GPU graphics — `None` when the
/// frontend is headless (a behaviour-only window with no surface).
pub struct NewWindow {
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
pub trait Frontend {
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
///
/// Not `#[cfg(test)]`: the shell's end-to-end tests live in `tests/`, which link
/// the library as an ordinary dependency (that is the point of the lib/bin split
/// — only an integration test can reach `CARGO_BIN_EXE_ghost` and stand up real
/// session hosts). The cost in the shipped binary is this struct and a
/// constructor.
pub struct HeadlessFrontend {
    /// Mints distinct synthetic [`WindowId`]s for the surface-less windows.
    next_id: std::cell::Cell<u64>,
    /// Set when the App asks to quit ([`Frontend::exit`]).
    exited: std::cell::Cell<bool>,
}

impl HeadlessFrontend {
    pub fn new() -> Self {
        Self {
            next_id: std::cell::Cell::new(1),
            exited: std::cell::Cell::new(false),
        }
    }

    /// Whether the App asked to quit.
    pub fn exited(&self) -> bool {
        self.exited.get()
    }
}

impl Default for HeadlessFrontend {
    fn default() -> Self {
        Self::new()
    }
}

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

impl App {
    /// A behaviour-only App: no event loop, GPU, watcher, or watcher — the seam a
    /// [`HeadlessFrontend`] plugs into. Drive it with the App's own methods
    /// (`open_fleet_window`, `dispatch`, `on_*`) and assert on its state. Reachable
    /// from `tests/` for the same reason as [`HeadlessFrontend`].
    ///
    /// It does no off-loop work: with no [`EventSink`] the background workers are
    /// never started. Use [`headless_with_sink`](App::headless_with_sink) to run them
    /// for real (against a real remote host) and drain their results yourself.
    pub fn headless() -> Self {
        App {
            windows: HashMap::new(),
            states: Sessions::new(),
            sessions: HashMap::new(),
            observers: HashMap::new(),
            dead_fed: HashSet::new(),
            clipboard: None,
            start: Instant::now(),
            startup: Startup::Fleet,
            next_session_seq: 0,
            next_group_seq: 0,
            next_group_color: 0,
            bench: None,
            focused: None,
            sink: None,
            proxy: None,
            remotes: Arc::default(),
            remote_infos: HashMap::new(),
            remote_remembered: HashMap::new(),
            remote_index: HashMap::new(),
            remote_watchers: HashMap::new(),
            pending_remote_restores: HashMap::new(),
            reconnecting: HashMap::new(),
            input_stalls: HashMap::new(),
            remote_retries: HashMap::new(),
            probing_remotes: Arc::default(),
            last_wake_at: Instant::now(),
            subs: HashMap::new(),
            // From the (test-isolated) data dir, as `interactive` does — a shell test
            // that seeds `groups.toml` gets the registry the real launch would read.
            groups: groups::load(),
            _watcher: None,
            sessions_changed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            _config_watcher: None,
            config_changed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_workspace: windows::load(),
            workspace_dirty: false,
        }
    }

    /// A headless App that **does** run its background workers, posting into `sink`.
    ///
    /// This is what makes ghost's remote recovery testable: the watcher, the connect
    /// and reconnect workers, the reattach probe and the remote spawn all run for
    /// real — real `ssh`, a real host — and the test plays the event loop, draining
    /// [`QueuedEvents`] into [`on_user_event`](App::on_user_event) as winit would.
    pub fn headless_with_sink(sink: Arc<dyn EventSink>) -> Self {
        App {
            sink: Some(sink),
            ..App::headless()
        }
    }

    /// Register a connected remote host, exactly as a finished connect does: starts
    /// its watcher and re-adopts anything a restore queued for it. A test that
    /// negotiated a real transport itself hands the result in here.
    pub fn adopt_remote_host(
        &mut self,
        spec: ConnectionSpec,
        remote_ghost: String,
        fe: &dyn Frontend,
    ) {
        self.finish_remote_reconnect(spec, remote_ghost, fe);
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
        // The scheme's own 16 colors, so an OSC 4 query reports what the screen
        // actually paints for an index the app hasn't overridden.
        ansi: theme.palette,
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
    /// The window's title, as last set — the bar draws it, and the shell has to
    /// keep it because the model hands it over as a command and forgets it.
    title: String,
    /// Whether the window has keyboard focus; the titlebar dims without it.
    focused: bool,
    /// The titlebar button under the pointer, and the one a press landed on —
    /// a button acts on RELEASE, and only if the pointer is still on it, so a
    /// press dragged away is cancelled the way every toolkit's is.
    hovered_button: Option<ghost_ui_core::frame::WindowButton>,
    pressed_button: Option<ghost_ui_core::frame::WindowButton>,
    /// The resize edge the pointer was last over, so the cursor is put back when
    /// it leaves and a press knows which edge it is grabbing.
    #[cfg(all(unix, not(target_os = "macos")))]
    frame_edge: Option<ghost_ui_core::ResizeEdge>,
    /// Whether any pointer button is currently held. Whatever a press started —
    /// selecting text, dragging a tile — owns the pointer until it is let go, so
    /// the window frame must not grab a drag that wanders into its resize band.
    pointer_down: bool,
    /// Rate-limits repaints so output floods / held keys can't drive a software
    /// rasterizer at the 8 ms poll rate (see [`pacer`]).
    pacer: pacer::FramePacer,
    /// Foreground render-stall watchdog (see [`rendertrace`]). Timestamps the repaint
    /// pipeline and classifies a foreground that stops presenting while its session
    /// keeps feeding. Inert unless `RUST_LOG=ghost::render=trace`.
    render_trace: rendertrace::RenderTrace,
    /// Defers the costly relayout/reflow during an interactive resize, blitting
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
    /// dropped — so [`release_repaint_due`](WindowState::release_repaint_due) keeps the
    /// repaint armed and the pacer retries (at its budget cadence) until one lands.
    /// Otherwise a window created mid-run comes up blank (only its title bar) until an
    /// unrelated event forces a redraw. Set once, on the first successful present.
    presented_ok: bool,
    /// Whether the platform has told us this window is occluded (fully hidden: another
    /// Space, minimized, behind an opaque window). Tracked from `WindowEvent::Occluded`.
    /// An occluded surface can't present, so the window is parked: no repaint releases
    /// at all ([`release_repaint_due`](WindowState::release_repaint_due)), and the
    /// render-stall watchdog skips it — its "stall" is the platform withholding the
    /// drawable, not our bug. `Occluded(false)` forces a fresh full frame.
    occluded: bool,
    /// The platform's last-seen answer to "will you blur behind this window?",
    /// which is what decides whether ghost draws its own frost instead (see
    /// [`glass`]). Tracked because the answer can change while the window is up —
    /// a compositor's blur effect switched off mid-session withdraws the
    /// capability — and the window would otherwise sit there with neither
    /// treatment, showing bare alpha where it had glass.
    blur_supported: bool,
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
    /// The (spec, name) a connect had prepared, held while the prompt shows the
    /// transport-fallback choice screen: if the user picks plain ssh
    /// ([`Cmd::UsePlainSshFallback`]) the shell spawns that local `ssh <host>`
    /// child. Cleared on Retry (a fresh connect supersedes it) or Cancel.
    pending_fallback: Option<(ConnectionSpec, String)>,
}

impl WindowState {
    /// Request a repaint, if this window has a surface. A no-op for a headless
    /// window (no [`Graphics`]), so behaviour paths can call it unconditionally.
    fn request_redraw(&self) {
        if let Some(gfx) = &self.gfx {
            gfx.window.request_redraw();
        }
    }

    /// The once-per-pass repaint decision: should `about_to_wait` request a
    /// redraw for this window now?
    ///
    /// Until the opening frame lands the repaint stays armed — macOS can drop
    /// redraws while it finishes compositing a new window, and the pacer's
    /// release keeps retrying (paced) until [`FramePacer::painted`] confirms
    /// one. An occluded window is parked outright: its surface cannot be
    /// acquired, so a release would only spin render→acquire-fail→retry —
    /// the `Occluded(false)` handler re-arms a forced repaint the moment the
    /// window can present again. Both gates exist because their absence was a
    /// self-sustaining event-loop spin (each failed redraw wakes the loop,
    /// which released the still-pending repaint again): ~5k failed surface
    /// acquires/sec, ~70% of a core, and a leaked wgpu texture id each.
    fn release_repaint_due(&mut self, now_ms: u64) -> bool {
        if !self.presented_ok {
            self.pacer.request();
        }
        if self.occluded {
            return false;
        }
        self.pacer.release(now_ms)
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
pub struct App {
    /// Live windows by id; each owns its GPU surface and pure model.
    windows: HashMap<WindowId, WindowState>,
    /// The one emulator/terminal state per session in this process (the "one model,
    /// many views" collapse): what each session's screen looks like, keyed by id.
    /// Every window viewing a session borrows the SAME state from here, so a session
    /// driven in one window and previewed in another is emulated once and fanned to
    /// each view — never re-run per window. Distinct from [`Self::sessions`], the
    /// transport *clients*.
    states: Sessions,
    /// The process's session clients (the driving transport connections), keyed by
    /// id — exactly one per session regardless of how many windows view it. Input
    /// from any viewing window reaches the one client; the last viewer letting go
    /// drops it (the "close = detach" default). One of the three feed sources fanned
    /// into [`Self::states`]: the driven half.
    sessions: HashMap<String, Session>,
    /// Read-only mirrors of sessions previewed but driven nowhere in this process
    /// (`Cmd::Observe` — a session attached elsewhere, or on a remote host), keyed by
    /// id. Deduped against [`Self::sessions`]: a session with a local client is never
    /// also observed (that would double-feed its one emulator). The observed feed
    /// source; last-viewer-gated on teardown like the clients.
    observers: HashMap<String, Subscriber>,
    /// Dead sessions whose recording has been played into the shared state already,
    /// so the periodic sweep doesn't re-feed the same last screen every tick.
    /// Process-wide: the recording replays once, fanned to every window's tile.
    /// A name is cleared when its session lives again (a fresh death re-feeds).
    dead_fed: HashSet<String>,
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
    /// Where background workers post their results for the main loop to apply (see
    /// [`EventSink`]): the watcher's listings, connect/reconnect outcomes, reattach
    /// readiness, remote spawns. `None` leaves this App unable to do off-loop work at
    /// all; a test supplies [`QueuedEvents`] and drains it into
    /// [`on_user_event`](App::on_user_event), which is how the remote-recovery
    /// workers are driven against a real host without a window server.
    sink: Option<Arc<dyn EventSink>>,
    /// The winit proxy itself, needed by the native macOS menu (AppKit posts from its
    /// own thread and wants winit's own handle). `None` under a headless frontend.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    proxy: Option<winit::event_loop::EventLoopProxy<UserEvent>>,
    /// Remote hosts reached over the ssh transport, keyed by target — retained
    /// after a successful connect and shared with the watcher thread that lists
    /// their sessions. A host stays until its last window/session is gone.
    remotes: Arc<std::sync::Mutex<HashMap<String, RemoteHost>>>,
    /// The latest remote listing per host (fleet-namespaced ids), delivered by the
    /// watcher and merged into every `Cmd::ListSessions` reply.
    remote_infos: HashMap<String, Vec<ghost_vt::session::SessionInfo>>,
    /// The session names each connected host still holds a descriptor for (bare,
    /// un-namespaced) — its resurrection tickets, fetched by the watcher thread
    /// alongside each listing. `remembered_remotes` consults it to tell a member
    /// that exited cleanly on its host from one a reboot took down. No entry
    /// means "unknown" (an older remote ghost, or the fetch hasn't landed), and
    /// the sweep stays conservative: not-listed members remain relaunchable.
    remote_remembered: HashMap<String, HashSet<String>>,
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
    /// Per-session watch on input that was accepted but not yet written (see
    /// [`InputStall`]). Kept here rather than on the `Session` so the verdict is
    /// the shell's — it is the shell that can name it and probe the transport.
    /// Pruned with the sessions themselves each wake.
    input_stalls: HashMap<String, InputStall>,
    /// Remote **hosts** a group still remembers a session on but which we are not
    /// connected to, each with the stop flag of the background worker retrying it
    /// forever (see [`App::retry_remembered_hosts`]). Presence dedupes, so one
    /// worker per host no matter how many members or windows want it.
    ///
    /// This is what makes waiting durable: the hold outlives the drop that started
    /// it, and — because the members are remembered in `groups.toml` — a ghost that
    /// is quit and relaunched while the host is still down picks the wait back up
    /// instead of forgetting the sessions.
    remote_retries: HashMap<String, std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// Remote targets with a transport health probe in flight
    /// ([`App::probe_remote_transports`]). Presence dedupes — one prober per host
    /// no matter how many wake suspicions fire — and the prober thread clears its
    /// own entry when done.
    probing_remotes: Arc<std::sync::Mutex<HashSet<String>>>,
    /// When the event loop last ran, to spot a suspend: a wake-to-wake gap over
    /// [`SUSPEND_PROBE_GAP`] triggers a probe of the remote transports.
    last_wake_at: Instant,
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

    /// `wid` now holds `id`: tell every OTHER window that was driving it to let it go
    /// (`UiEvent::DriverLost`). A session shows in exactly one place, so the loser
    /// switches its foreground away — or drops to the fleet — rather than keeping a
    /// live view of a session it no longer owns; only the new holder re-grids the one
    /// shared child.
    ///
    /// A fresh attach (no prior driver) finds no losers; only a real cross-window
    /// take-over does. Both take-over routes must call this: `Cmd::Attach`, and the
    /// adopt-in-place branch of `Cmd::TakeOver` that skips attaching because this
    /// process already holds the client.
    fn hand_over(&mut self, wid: WindowId, id: &str, event_loop: &dyn Frontend) {
        let losers: Vec<WindowId> = self
            .windows
            .iter()
            .filter(|(owid, w)| **owid != wid && w.root.drives(id))
            .map(|(owid, _)| *owid)
            .collect();
        for owid in losers {
            self.dispatch(
                owid,
                UiEvent::DriverLost {
                    name: id.to_string(),
                },
                event_loop,
            );
        }
    }

    /// Keep one background reconnect running for every remote host a group still
    /// remembers a session on and that we are not connected to, and stop the ones
    /// nothing wants any more. Idempotent and cheap — called from the loop's
    /// once-per-wake pass, where it also covers a host that goes away mid-run.
    ///
    /// This is the durable half of "a remote reboot must not lose my sessions". The
    /// per-session probe ([`Self::begin_reconnect`]) only exists while a window
    /// holds a dropped client, and dies with the process; this one is keyed to what
    /// `groups.toml` remembers, so it survives the drop, the window closing, and
    /// ghost being quit and relaunched. The spec comes from the group's stored
    /// connection when it has one (port, identity, options), falling back to
    /// parsing the target.
    fn retry_remembered_hosts(&mut self) {
        use std::sync::atomic::Ordering;
        let Some(sink) = self.sink.clone() else {
            return; // nowhere to post the result (an App with no sink at all)
        };
        let connected: HashSet<String> = self
            .remotes
            .lock()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        let mut wanted: HashSet<String> = HashSet::new();
        let remembered = self.groups.iter().flat_map(|g| &g.members).chain(
            self.pending_remote_restores
                .values()
                .flatten()
                .map(|p| &p.composite),
        );
        for member in remembered {
            if let Some((target, _)) = remote_id_parts(member)
                && !connected.contains(target)
            {
                wanted.insert(target.to_string());
            }
        }
        // Stop retrying a host nothing remembers any more (its group was dissolved,
        // or it answered and is now connected).
        self.remote_retries.retain(|target, stop| {
            let keep = wanted.contains(target);
            if !keep {
                stop.store(true, Ordering::Relaxed);
            }
            keep
        });
        for target in wanted {
            if self.remote_retries.contains_key(&target) {
                continue;
            }
            let spec = self
                .groups
                .iter()
                .filter_map(|g| g.connection.clone())
                .find(|c| c.target() == target)
                .or_else(|| ConnectionSpec::parse_target(&target));
            let Some(spec) = spec else { continue };
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            self.remote_retries.insert(target, stop.clone());
            spawn_remote_reconnect(sink.clone(), spec, stop);
        }
    }

    /// Every remembered **remote** member that no host is currently serving, and
    /// why — the remote half of the dead sweep.
    ///
    /// A remote member (`<target>␟<real>`) has no local descriptor and no local
    /// recording, so the local sweep can never name it. Without this it had no tile
    /// at all whenever its host was unreachable: the window that was driving it
    /// showed an empty fleet, which is what "a remote reboot loses my sessions"
    /// actually looked like. Two cases, and the difference is what the card offers:
    ///
    /// - the host is **not connected**: it may still be running there, so the tile
    ///   waits ([`ghost_ui_core::DeadState::AwaitingHost`]) while
    ///   [`Self::retry_remembered_hosts`] keeps reconnecting.
    /// - the host **is** connected and does not list it: it is genuinely gone (the
    ///   host rebooted), so the tile becomes relaunchable — an explicit act, since
    ///   relaunching cannot bring back what was running.
    fn remembered_remotes(&self) -> Vec<ghost_ui_core::DeadSession> {
        let mut out: Vec<ghost_ui_core::DeadSession> = Vec::new();
        // Group members, plus what a startup restore is still waiting to re-adopt:
        // a restored window's members come from `windows.toml` and reach the group
        // registry only once something saves it, so a bare launch whose hosts are
        // all away would otherwise open with nothing on screen.
        let remembered = self.groups.iter().flat_map(|g| &g.members).chain(
            self.pending_remote_restores
                .values()
                .flatten()
                .map(|p| &p.composite),
        );
        for member in remembered {
            let Some((target, real)) = remote_id_parts(member) else {
                continue;
            };
            if out.iter().any(|d| &d.name == member) {
                continue;
            }
            match self.remote_infos.get(target) {
                // Connected and serving it: it is live, not remembered — the
                // listing carries it and the fleet has a real tile.
                Some(infos) if infos.iter().any(|i| &i.name == member) => continue,
                // Connected and NOT serving it. The host's remembered-set (its
                // descriptor names) tells the two dead cases apart: still
                // remembered means an unclean death (a reboot) — relaunchable;
                // no longer remembered means it exited cleanly (or was killed)
                // THERE, so it must be forgotten HERE too — not naming it is
                // what makes the sweep drop its membership and tile. With no
                // remembered-set (an older remote ghost, or the fetch hasn't
                // landed) stay conservative: relaunchable, as before.
                Some(_) => {
                    if self
                        .remote_remembered
                        .get(target)
                        .is_some_and(|names| !names.contains(real))
                    {
                        continue;
                    }
                    out.push(ghost_ui_core::DeadSession {
                        name: member.clone(),
                        display_name: real.to_string(),
                        command: Vec::new(),
                        cwd: None,
                        state: ghost_ui_core::DeadState::Exited,
                    })
                }
                None => out.push(ghost_ui_core::DeadSession {
                    name: member.clone(),
                    display_name: real.to_string(),
                    command: Vec::new(),
                    cwd: None,
                    state: ghost_ui_core::DeadState::AwaitingHost(target.to_string()),
                }),
            }
        }
        out
    }

    /// The descriptor sweep that runs with every session listing: tell the
    /// window which group members are dead-but-remembered (its fleet shows
    /// them as dead tiles), play each one's recording into its tile once (the
    /// last screen, via the ordinary Resized-push + output path), and prune
    /// descriptors nothing references any more — not live, in no group — so
    /// the data dir doesn't keep one per session ever spawned.
    ///
    /// `live` is the LOCAL listing (descriptors, recordings and the prune are all
    /// local notions). Remembered **remote** members are swept separately below,
    /// against their host's listing: they have no local descriptor, so judging them
    /// by this scan would leave them with no tile at all.
    fn sync_dead_sessions(
        &mut self,
        wid: WindowId,
        live: &[ghost_vt::session::SessionInfo],
        event_loop: &dyn Frontend,
    ) {
        let live_names: HashSet<&str> = live.iter().map(|i| i.name.as_str()).collect();
        let mut dead: Vec<ghost_ui_core::DeadSession> = self.remembered_remotes();
        for name in self.groups.iter().flat_map(|g| &g.members) {
            if live_names.contains(name.as_str())
                || is_remote_id(name)
                || dead.iter().any(|d| &d.name == name)
            {
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
                state: ghost_ui_core::DeadState::Exited,
            });
        }
        self.dispatch(wid, UiEvent::DeadSessions(dead.clone()), event_loop);
        // A session alive again may die again later: let it re-feed then. The mark is
        // process-wide now (the recording replays once into the one shared state,
        // fanned to every window's tile), so a live session clears it for all.
        self.dead_fed.retain(|n| !live_names.contains(n.as_str()));
        for d in dead {
            let fresh = self.dead_fed.insert(d.name.clone());
            if !fresh {
                continue;
            }
            let Ok(rec) = ghost_vt::record::read(&ghost_vt::paths::recording_path(&d.name)) else {
                continue; // never recorded: the tile stays a placeholder
            };
            let s = screen::Screen::from_recording(&rec, 0);
            let (cols, rows) = s.dimensions();
            // Seed the ONE shared state at the recording's grid (the fleet arm no
            // longer rebuilds it), push the grid to every window's dead tile so it
            // resets its preview view, then replay the last screen into the shared
            // state once and fan the render to all of them.
            self.states.resize_observed(&d.name, cols, rows);
            let viewers: Vec<WindowId> = self
                .windows
                .iter()
                .filter(|(_, w)| w.root.views(&d.name))
                .map(|(id, _)| *id)
                .collect();
            for v in &viewers {
                self.dispatch(
                    *v,
                    UiEvent::SessionPush {
                        name: d.name.clone(),
                        push: SessionPush::Event(ghost_vt::protocol::SessionEvent::Resized {
                            cols,
                            rows,
                        }),
                    },
                    event_loop,
                );
            }
            self.feed_observed_to_viewers(&d.name, &s.resync(), false, event_loop);
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
        let wids: Vec<WindowId> = self.windows.keys().copied().collect();
        for wid in wids {
            let cmds = {
                let Some(w) = self.windows.get_mut(&wid) else {
                    continue;
                };
                // Model side (headless-observable): the default colors and padding.
                let cmds = w.root.set_theme(&mut self.states, colors);
                w.root.set_padding(pad);
                // Gfx side (no model representation; absent under a headless
                // frontend): the renderer theme — opacity/frost/scheme colours bake
                // into cached surfaces, so `set_theme` drops them — the compositor
                // blur, and a forced repaint. The `Scene` is theme-independent, so
                // without invalidating the scene cache AND requesting a redraw an
                // identical scene would be skipped as unchanged and nothing would
                // repaint.
                if let Some(gfx) = w.gfx.as_mut() {
                    // Re-decide the glass per window: whether the compositor is
                    // blurring is its answer to give, not the config's, and it can
                    // differ from what it was when the window opened.
                    let blur_supported = backdrop_blur_supported(&gfx.window);
                    let g = glass(theme.bg_alpha < 1.0, blur_supported, theme.frost);
                    w.blur_supported = blur_supported;
                    let mut theme = theme;
                    theme.frost = g.frost;
                    gfx.renderer.set_theme(theme);
                    gfx.scene_cache.invalidate();
                    gfx.window.set_blur(g.blur);
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
                self.resize_model(wid, w_px, h_px, scale, event_loop);
            }
        }
        self.workspace_dirty = true;
    }

    /// Resize window `wid`'s model to a *surface* size. The model lays out under
    /// our titlebar, so its viewport is the surface less the bar — every caller
    /// holding a window size (as opposed to a content size) must come through
    /// here, or the composed scene ends up a bar taller than the swapchain.
    fn resize_model(
        &mut self,
        wid: WindowId,
        w_px: u32,
        h_px: u32,
        scale: f64,
        event_loop: &dyn Frontend,
    ) {
        let (bar, m, surface) = self
            .windows
            .get(&wid)
            .and_then(|w| w.gfx.as_ref())
            .map_or((0, ghost_ui_core::frame::FrameInset::NONE, None), |g| {
                (g.bar_px(), g.margins_px(), Some(g.size()))
            });
        // Taking or dropping the shadow's margins resizes the surface with no
        // configure to announce it, so the size this event carried can already be
        // stale — the surface itself is the truth.
        let event = (w_px, h_px);
        let (w_px, h_px) = surface.filter(|s| *s != (0, 0)).unwrap_or((w_px, h_px));
        let (w_px, h_px) = m.window((w_px, h_px));
        tracing::trace!(
            target: "ghost::frame",
            ?event,
            ?surface,
            ?m,
            bar,
            model = ?(w_px, h_px.saturating_sub(bar).max(1)),
            "model resized"
        );
        self.dispatch(
            wid,
            UiEvent::Resize {
                w_px,
                h_px: h_px.saturating_sub(bar).max(1),
                scale,
            },
            event_loop,
        );
    }

    /// Feed an event to window `wid`'s model and execute the effects it returns.
    pub fn dispatch(&mut self, wid: WindowId, ev: UiEvent, event_loop: &dyn Frontend) {
        // Name the window an OS focus event reached, and what it shows. The
        // per-session lines below can't: a swap between two windows logs an out for
        // one session and an in for another, with nothing tying either to a window,
        // and a window in the fleet has no session to report at all — so the swap
        // reads as a bare focus-out. This is the line that says whether the window
        // you focused is the one that got the focus.
        if let UiEvent::Focus(focused) = &ev
            && ghost_ui_core::focus_trace::enabled()
            && let Some(w) = self.windows.get(&wid)
        {
            let (mode, fg) = match w.root.single_foreground() {
                Some(fg) => ("single", fg.as_str()),
                None => ("fleet", "-"),
            };
            ghost_ui_core::focus_trace::log(
                fg,
                format_args!("os-focus win={wid:?} focused={focused} mode={mode}"),
            );
        }
        let cmds = match self.windows.get_mut(&wid) {
            Some(w) => w.root.update(&mut self.states, ev),
            None => return,
        };
        self.exec(wid, cmds, event_loop);
        // A handled event may have changed a window's foreground, view, grid, or
        // membership; mark the workspace for the loop's once-per-wake flush.
        self.workspace_dirty = true;
        self.assert_foreground_states_present("after dispatch");
    }

    /// Every window in the single view must have its foreground `SessionState` present
    /// in the process-wide registry: `RootModel::drive`/`live_scene` index it by id and
    /// abort (`expect("foreground session state present")`) on a miss — the crash a
    /// stray mouse-move or Cmd-` surfaces long after the fact. This catches a
    /// cross-window reaper (a shared state removed while another window still
    /// foregrounds it) AT THE MUTATION that caused it, naming the window and session,
    /// instead of at the eventual input event in an unrelated window. Debug-only, but
    /// the macOS dev build ships with debug-assertions on, so it runs where the crash
    /// was seen. `ctx` says which pass tripped it.
    fn assert_foreground_states_present(&self, ctx: &str) {
        #[cfg(debug_assertions)]
        for (wid, w) in &self.windows {
            if let Some(fg) = w.root.single_foreground() {
                debug_assert!(
                    self.states.contains(fg),
                    "window {wid:?} is Single on '{fg}' but its shared state is gone ({ctx}) \
                     — a cross-window reaper deleted a foregrounded session's state"
                );
            }
        }
        let _ = ctx;
    }

    pub fn exec(&mut self, wid: WindowId, cmds: Vec<Cmd>, event_loop: &dyn Frontend) {
        let now_ms = self.now_ms();
        for cmd in cmds {
            match cmd {
                Cmd::SendInput { session, bytes } => {
                    // For the focus trace: what a focus report actually did on the
                    // wire — sent, failed, or dropped for want of a client.
                    let report = if ghost_ui_core::focus_trace::enabled() {
                        ghost_ui_core::focus_trace::report_in(&bytes)
                    } else {
                        None
                    };
                    // Input from any viewing window reaches the one process-wide client.
                    match self.sessions.get_mut(&session) {
                        Some(s) => {
                            let res = s.send_input(&bytes);
                            if let Some(which) = report {
                                match &res {
                                    Ok(()) => ghost_ui_core::focus_trace::log(
                                        &session,
                                        format_args!("wire {which} ok"),
                                    ),
                                    Err(e) => ghost_ui_core::focus_trace::log(
                                        &session,
                                        format_args!("wire {which} WRITE FAILED: {e}"),
                                    ),
                                }
                            }
                            let _ = res;
                        }
                        None => {
                            if let Some(which) = report {
                                ghost_ui_core::focus_trace::log(
                                    &session,
                                    format_args!("wire {which} DROPPED (no client)"),
                                );
                            }
                        }
                    }
                }
                Cmd::Resize {
                    session,
                    cols,
                    rows,
                } => {
                    // Only the driving window emits `Cmd::Resize` (grid mutation is a
                    // drivership privilege), so this reaches the one shared client.
                    if let Some(s) = self.sessions.get_mut(&session) {
                        let _ = s.resize(cols, rows);
                    }
                }
                Cmd::ResizeWindow { w_px, h_px } => {
                    // A program re-gridded itself (DECCOLM): ask the window manager to
                    // resize the window to fit. `request_inner_size` answers `Some` when
                    // the platform applied it synchronously — no `Resized` event will
                    // follow, so re-grid against the granted (possibly clamped) size
                    // ourselves; `None` means the request is in flight and the event
                    // will arrive. A refused request simply produces neither, and the
                    // model stays on the program's grid until the window next resizes.
                    let granted = self
                        .windows
                        .get(&wid)
                        .and_then(|w| w.gfx.as_ref())
                        .and_then(|g| {
                            g.window
                                .request_inner_size(PhysicalSize::new(w_px, h_px))
                                .map(|s| (s, g.window.scale_factor()))
                        });
                    if let Some((s, scale)) = granted {
                        self.resize_step(wid, s.width.max(1), s.height.max(1), scale, event_loop);
                    }
                }
                // The other window ops a program asked for (XTWINOPS). Like the
                // resize above, these are *requests*: a window manager is free to
                // ignore them, and a tiling one usually does. Whatever it does with
                // the window, a size change comes back as a `Resized` event and
                // re-grids the model.
                Cmd::SetIconified(on) => {
                    if let Some(w) = self.windows.get(&wid).and_then(|w| w.gfx.as_ref()) {
                        w.window.set_minimized(on);
                    }
                }
                Cmd::SetMaximized(on) => {
                    if let Some(w) = self.windows.get(&wid).and_then(|w| w.gfx.as_ref()) {
                        w.window.set_maximized(on);
                    }
                }
                Cmd::SetFullscreen(on) => {
                    if let Some(w) = self.windows.get(&wid).and_then(|w| w.gfx.as_ref()) {
                        // Borderless on the window's current monitor: exclusive
                        // full-screen would need a video mode, which no terminal wants.
                        w.window
                            .set_fullscreen(on.then(|| Fullscreen::Borderless(None)));
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
                    // A session already driven in THIS process (a client in the shared
                    // map) is adopted in place: the core already took drivership
                    // (`mine`), so we open no second client and DON'T rebuild — the one
                    // shared emulator and its scrollback stay live. This is the
                    // same-process take-over / second-view path; a second client or a
                    // rebuild here would double-drive or blank a session another window
                    // still views (there'd be no resync to refill it).
                    if !self.sessions.contains_key(&id)
                        && let Some(w) = self.windows.get(&wid)
                    {
                        // Handshake at the window's real grid (see `attach_into`).
                        let (cols, rows) = w.root.grid();
                        let identity = w.root.client_identity();
                        if let Ok(s) = attach(&id, cols, rows, &identity) {
                            // A fresh transport means a resync (whole screen AND
                            // scrollback) is inbound: rebuild the shared mirror first so
                            // the replay lands on an empty emulator instead of doubling
                            // the history (W1, hoisted out of the core so it fires
                            // exactly when a resync is coming, never on an adopt).
                            self.states.resize_observed(&id, cols, rows);
                            self.drive_with_client(&id, s);
                        }
                    }
                    self.hand_over(wid, &id, event_loop);
                }
                Cmd::Observe(id) if self.remote_index.contains_key(&id) => {
                    // Live remote preview: observe the session over its host's
                    // transport, feeding the tile exactly like a local observer.
                    if self.bench.is_none()
                        // Never observe a session anything in THIS PROCESS already
                        // drives or observes: a second feed source double-feeds the one
                        // shared emulator, garbling it unhealably (finding #7). The maps
                        // are process-wide now, so this dedup is process-wide — the whole
                        // point of the collapse (a preview of an in-process-driven
                        // session borrows the driver's state, opens no mirror).
                        && !self.observers.contains_key(&id)
                        && !self.sessions.contains_key(&id)
                        && let Some((target, real)) = self.remote_index.get(&id).cloned()
                    {
                        match self.observe_remote(&target, &real) {
                            Some(sub) => {
                                self.observers.insert(id, sub);
                            }
                            // No live connection (host gone) or a failed channel:
                            // report the mirror dead so the tile reverts to a
                            // placeholder and a later reconcile retries.
                            //
                            // Deliberately single-window: this reaches only the window
                            // that emitted the Observe. If another window also previews
                            // this session, its tile keeps the last frame until the next
                            // `SessionList` reconcile drops it (in every window at once) —
                            // a sub-second, self-healing display lag, never a wrong shared
                            // state (the dedup above guarantees no second feed source). The
                            // multi-view fan (`end_session_in_views`) is not worth its cost
                            // here; see the "one model, many views" 5c notes (item 4).
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
                        // Process-wide dedup — see the remote arm above (finding #7).
                        // A session driven or already observed anywhere in this process
                        // is never given a second feed source.
                        && !self.observers.contains_key(&id)
                        && !self.sessions.contains_key(&id)
                    {
                        match Subscriber::observe(&id) {
                            Ok(sub) => {
                                self.observers.insert(id, sub);
                            }
                            // An old host or a dying session: report the
                            // mirror dead so the fleet reverts the tile to a
                            // placeholder and retries on a later reconcile.
                            // Single-window on purpose, same as the remote arm above: a
                            // co-previewer's tile self-heals on the next reconcile.
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
                    // Last-viewer-gate (W5): the observer is the ONE shared feed source
                    // for this session now, so a fleet closing must not kill it while
                    // another window still previews it. Drop it only when no window
                    // views `id` any more. (Symmetric to the Observe dedup: creation is
                    // deduped, destruction is refcounted by the live viewer set.)
                    if !self.windows.values().any(|w| w.root.views(&id)) {
                        self.observers.remove(&id);
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
                    // The core already dropped this window's ownership/mirror; reconcile
                    // the one shared client against the remaining viewers — dropped if no
                    // window views it (it keeps running under its host), downgraded to a
                    // read-only mirror if another window only previews it.
                    self.reconcile_source(&id);
                }
                Cmd::Kill(id) if is_remote_id(&id) => {
                    // Kill the remote session over its host's transport (off-loop),
                    // then reconcile the shared source; the watcher drops the tile.
                    // Route by the id itself, like Rename below: a remote id is
                    // self-describing, and the one most worth killing — a cold tile
                    // whose host dropped — is neither driven nor listed, so the
                    // index does not hold it and gating on it would misroute the
                    // kill to the local path (bogus socket, kill silently dropped).
                    if let Some((target, real)) = remote_id_parts(&id) {
                        self.spawn_remote_kill(target, real);
                    }
                    self.reconcile_source(&id);
                }
                Cmd::Kill(id) => {
                    // Kill the session and its process, then reconcile the shared source
                    // (the client is dropped; a dead tile falls back to its recording).
                    let _ = session::kill_session(&id);
                    self.reconcile_source(&id);
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
                        self.spawn_remote_session(wid, &target, &real);
                    } else if self.respawn_dead(&id) && self.attach_into(wid, &id) {
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
                        self.respawn_dead(&id);
                    }
                }
                Cmd::RestartRemote(id) => {
                    // Restart a live remote session's host under the current binary
                    // over the transport (off-loop), keeping its screen. The old
                    // host's death drops any client driving it; the existing
                    // reconnect path then re-attaches to the fresh host — reading its
                    // new (current) proto level. Route by the self-describing id.
                    if let Some((target, real)) = remote_id_parts(&id) {
                        self.spawn_remote_restart(target, real);
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
                    // Best-effort attach; a later reconcile re-attaches if it lost the
                    // race. A freshly-spawned name is new, so the shared client map has
                    // no entry — this window becomes its driver.
                    if !self.sessions.contains_key(&name)
                        && let Some(w) = self.windows.get(&wid)
                    {
                        // Handshake at the window's real grid (see `attach_into`).
                        let (cols, rows) = w.root.grid();
                        let identity = w.root.client_identity();
                        if let Ok(s) = attach(&name, cols, rows, &identity) {
                            self.drive_with_client(&name, s);
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
                        w.pending_fallback = None;
                        w.request_redraw();
                    }
                }
                Cmd::UsePlainSshFallback => self.use_plain_ssh_fallback(wid, event_loop),
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
                        Some(target) => self.spawn_remote_session(wid, &target, &name),
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
                        // Switch the window to `id`'s single view. Attach only if the
                        // process holds no client yet — a session already driven here
                        // (even by another window) is adopted in place (no second
                        // transport), the same-process take-over the shared map enables.
                        let held = self.sessions.contains_key(&id);
                        if held || self.attach_into(wid, &id) {
                            self.dispatch(wid, UiEvent::AdoptSession(id.clone()), event_loop);
                            // The adopt-in-place branch never went through `Cmd::Attach`,
                            // so without this the window that HAD the session kept
                            // showing it: taking over another window's foreground left
                            // two windows on one session.
                            self.hand_over(wid, &id, event_loop);
                        }
                    }
                }
                Cmd::UploadImage {
                    session,
                    id,
                    width,
                    height,
                    rgba,
                } => {
                    if let Some(gfx) = self.windows.get_mut(&wid).and_then(|w| w.gfx.as_mut()) {
                        // The same key the scene names this session's terminals by, so
                        // the image resolves against the session that transmitted it.
                        let key = ghost_render::scene::session_key(&session);
                        gfx.renderer.upload_image(key, id, width, height, &rgba);
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
                    if let Some(w) = self.windows.get_mut(&wid) {
                        if let Some(gfx) = w.gfx.as_ref() {
                            gfx.window.set_title(&t);
                        }
                        // Our own titlebar draws it too, and a bar showing the
                        // last window's title is worse than one showing none.
                        if w.title != t {
                            w.title = t;
                            w.request_redraw();
                        }
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

    /// What a window-space pointer position is over — see
    /// [`ghost_ui_core::frame::frame_hit`], which decides it (and in what order:
    /// the resize band lies *under* the titlebar and has to be asked first).
    /// Reading the window's state is all that happens here.
    fn frame_hit(&self, id: WindowId, pos: PointPx) -> ghost_ui_core::frame::FrameHit {
        use ghost_ui_core::frame::FrameHit;
        let Some(w) = self.windows.get(&id) else {
            return FrameHit::Content(pos);
        };
        // A headless window has no surface to frame, and no bar over its model.
        let Some(gfx) = w.gfx.as_ref() else {
            return FrameHit::Content(pos);
        };
        let size = gfx.window.inner_size();
        #[cfg(all(unix, not(target_os = "macos")))]
        let grab = {
            use winit::platform::wayland::WindowExtWayland;
            ghost_ui_core::FrameGrab {
                own_frame: !gfx.window.is_decorated(),
                boxed_in: gfx.window.is_maximized()
                    || gfx.window.fullscreen().is_some()
                    || gfx.window.is_tiled(),
                pointer_down: w.pointer_down,
            }
        };
        // Elsewhere the platform frames the window and owns its edges.
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        let grab = ghost_ui_core::FrameGrab {
            own_frame: false,
            boxed_in: true,
            pointer_down: w.pointer_down,
        };
        ghost_ui_core::frame::frame_hit_within(
            pos,
            (size.width, size.height),
            gfx.window.scale_factor() as f32,
            grab,
            gfx.bar_px(),
            gfx.margins_px(),
        )
    }

    /// Note the resize edge the pointer is over, showing that edge's own cursor
    /// while it is there and putting the plain arrow back when it leaves.
    #[cfg(all(unix, not(target_os = "macos")))]
    fn track_frame_edge(&mut self, id: WindowId, edge: Option<ghost_ui_core::ResizeEdge>) {
        let Some(w) = self.windows.get_mut(&id) else {
            return;
        };
        if w.frame_edge == edge {
            return;
        }
        w.frame_edge = edge;
        if let Some(gfx) = w.gfx.as_ref() {
            // Leaving the band restores the plain arrow; the model sets its own
            // shape (a link's hand) from the motion events it gets from here on.
            gfx.window.set_cursor(match edge {
                Some(e) => winit::window::CursorIcon::from(resize_direction(e)),
                None => winit::window::CursorIcon::Default,
            });
        }
    }

    /// The titlebar strip of a window whose frame is ours, in window space.
    /// `None` when the desktop draws the frame and there is no bar of ours.
    #[cfg(target_os = "linux")]
    fn bar_rect(&self, id: WindowId) -> Option<ghost_render::scene::RectPx> {
        let gfx = self.windows.get(&id)?.gfx.as_ref()?;
        let h = gfx.bar_px();
        let m = gfx.margins_px();
        let (ww, _) = gfx.window_px();
        (h > 0).then_some(ghost_render::scene::RectPx {
            x: m.left as f32,
            y: m.top as f32,
            w: ww as f32,
            h: h as f32,
        })
    }

    /// Track which titlebar button the pointer is over, repainting when it
    /// changes — the hover circle is the only thing that says a button is there.
    /// `pos` is the pointer in window space while it is on the bar, and `None`
    /// once it is anywhere else, which un-hovers whatever it left.
    #[cfg(target_os = "linux")]
    fn track_bar_hover(&mut self, id: WindowId, pos: Option<PointPx>) {
        let hovered = pos.zip(self.bar_rect(id)).and_then(|(pos, bar)| {
            let scale = self.windows.get(&id)?.gfx.as_ref()?.window.scale_factor() as f32;
            ghost_ui_core::frame::button_at(pos, &desktop::button_layout(), bar, scale)
        });
        if let Some(w) = self.windows.get_mut(&id)
            && w.hovered_button != hovered
        {
            w.hovered_button = hovered;
            w.request_redraw();
        }
    }

    /// Act on a titlebar press: a button arms (it fires on release), and the bar
    /// itself starts a window move, or performs the desktop's double-click
    /// action, or opens the window menu.
    ///
    /// Returns whether the press was the bar's — the model never sees those.
    #[cfg(target_os = "linux")]
    fn press_on_bar(
        &mut self,
        id: WindowId,
        pos: PointPx,
        button: PointerButton,
        clicks: u8,
    ) -> bool {
        let Some(bar) = self.bar_rect(id) else {
            return false;
        };
        if !bar.contains(pos.x as f32, pos.y as f32) {
            return false;
        }
        let Some(gfx) = self.windows.get(&id).and_then(|w| w.gfx.as_ref()) else {
            return false;
        };
        let scale = gfx.window.scale_factor() as f32;
        let on_button = ghost_ui_core::frame::button_at(pos, &desktop::button_layout(), bar, scale);
        match button {
            // The window menu is the right-click gesture on every desktop; the
            // compositor draws it, so it is the compositor's to open.
            PointerButton::Right => gfx
                .window
                .show_window_menu(PhysicalPosition::new(pos.x, pos.y)),
            PointerButton::Left if on_button.is_some() => {
                if let Some(w) = self.windows.get_mut(&id) {
                    w.pressed_button = on_button;
                    w.request_redraw();
                }
            }
            PointerButton::Left if clicks >= 2 => match desktop::double_click_action() {
                desktop::DoubleClick::ToggleMaximize => {
                    gfx.window.set_maximized(!gfx.window.is_maximized())
                }
                desktop::DoubleClick::Minimize => gfx.window.set_minimized(true),
                desktop::DoubleClick::Menu => gfx
                    .window
                    .show_window_menu(PhysicalPosition::new(pos.x, pos.y)),
                desktop::DoubleClick::None => {}
            },
            // Anywhere else on the bar drags the window. The compositor runs the
            // move and keeps the pointer, so there is no release to wait for.
            PointerButton::Left => {
                let _ = gfx.window.drag_window();
            }
            PointerButton::Middle => {}
        }
        true
    }

    /// Act on a titlebar release: a button fires only if the pointer is still on
    /// the one the press armed. Returns whether the release was the bar's.
    #[cfg(target_os = "linux")]
    fn release_on_bar(&mut self, id: WindowId, pos: PointPx, event_loop: &dyn Frontend) -> bool {
        let Some(armed) = self.windows.get(&id).and_then(|w| w.pressed_button) else {
            return false;
        };
        if let Some(w) = self.windows.get_mut(&id) {
            w.pressed_button = None;
            w.request_redraw();
        }
        let still_on = self
            .bar_rect(id)
            .zip(self.windows.get(&id).and_then(|w| w.gfx.as_ref()))
            .and_then(|(bar, gfx)| {
                ghost_ui_core::frame::button_at(
                    pos,
                    &desktop::button_layout(),
                    bar,
                    gfx.window.scale_factor() as f32,
                )
            })
            == Some(armed);
        if still_on {
            self.press_window_button(id, armed, event_loop);
        }
        true
    }

    /// Perform a window-control button.
    #[cfg(target_os = "linux")]
    fn press_window_button(
        &mut self,
        id: WindowId,
        button: ghost_ui_core::frame::WindowButton,
        event_loop: &dyn Frontend,
    ) {
        use ghost_ui_core::frame::WindowButton;
        let Some(gfx) = self.windows.get(&id).and_then(|w| w.gfx.as_ref()) else {
            return;
        };
        match button {
            WindowButton::Minimize => gfx.window.set_minimized(true),
            WindowButton::Maximize => gfx.window.set_maximized(!gfx.window.is_maximized()),
            // The same path the frame's own close button took: closing is
            // detaching, and the last window out shuts the app down.
            WindowButton::Close => self.close_requested(id, event_loop),
        }
    }

    /// A surface-space pointer position in the model's space — the surface less
    /// our shadow margins and our titlebar. `None` when the pointer is on either,
    /// which is chrome: the model has no coordinate for it, and letting one
    /// through as a negative row would land the click on the terminal's first
    /// line.
    fn in_content(&self, id: WindowId, pos: PointPx) -> Option<PointPx> {
        match self.frame_hit(id, pos) {
            ghost_ui_core::frame::FrameHit::Content(pos) => Some(pos),
            _ => None,
        }
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
    pub fn mint_group(&mut self) -> ghost_ui_core::Group {
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
    fn respawn_dead(&mut self, id: &str) -> bool {
        if !spawn_dead(id) {
            return false;
        }
        // Its tile previews the OLD recording; a fresh death after this new life
        // must re-feed (the mark is process-wide now).
        self.dead_fed.remove(id);
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
            // Freshly (re)created by the current staged binary → our own level.
            if self.attach_ssh_into(wid, composite, cmd, ghost_vt::protocol::PROTO_LEVEL) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Fold one session's queued-input depth into its [`InputStall`], and act on
    /// the verdict: name a wedged write path in the focus trace, and — for a
    /// remote — probe its transport, which reaps a wedged master and hands the
    /// session to the ordinary drop→hold→reconnect path.
    ///
    /// This is the difference between a stall of seconds and a stall of minutes.
    /// `send_input` reports success for bytes it only queued, so before this
    /// nothing at any layer noticed that a session had stopped accepting input:
    /// the tile kept rendering, the keystrokes kept "succeeding", and recovery
    /// waited on whatever happened to clear the path (ssh's own keepalive, the
    /// user quitting). A probe here is cheap, deduped, and off-loop, and its worst
    /// case is a reconnect that resyncs the same screen.
    fn note_input_queue(&mut self, name: &str, pending: usize, now: Instant) {
        let Some(event) = self
            .input_stalls
            .entry(name.to_string())
            .or_default()
            .observe(pending, now)
        else {
            return;
        };
        match event {
            StallEvent::Wedged { bytes, waited } => {
                ghost_ui_core::focus_trace::log(
                    name,
                    format_args!(
                        "input STALLED {bytes} bytes unwritten for {:.1}s -> probing transport",
                        waited.as_secs_f32()
                    ),
                );
                if is_remote_id(name) {
                    self.probe_remote_transports();
                }
            }
            StallEvent::Drained { bytes, waited } => {
                ghost_ui_core::focus_trace::log(
                    name,
                    format_args!(
                        "input DRAINED {bytes} bytes after {:.1}s",
                        waited.as_secs_f32()
                    ),
                );
            }
        }
    }

    /// Health-check every remote host's shared connection, off-thread, and reap
    /// the ones whose peer died *silently* — the laptop slept, the network moved
    /// underneath us — so the loss is noticed NOW rather than when ssh's ~45s
    /// keepalive gives up. Reaping a wedged master kills every channel multiplexed
    /// over it, so each driven session's pipe EOFs within a pump and the ordinary
    /// drop→hold→reconnect path (and the watcher's own retry) takes over; no
    /// session state is touched here. A healthy master answers the bounded probe
    /// in milliseconds, so a false suspicion costs a couple of ssh control
    /// commands. One prober per host at a time (`probing_remotes` dedupes).
    pub fn probe_remote_transports(&mut self) {
        let hosts: Vec<(String, RemoteHost)> = match self.remotes.lock() {
            Ok(m) => m.iter().map(|(t, h)| (t.clone(), h.clone())).collect(),
            Err(_) => return,
        };
        for (target, host) in hosts {
            {
                let Ok(mut probing) = self.probing_remotes.lock() else {
                    return;
                };
                if !probing.insert(target.clone()) {
                    continue;
                }
            }
            let probing = Arc::clone(&self.probing_remotes);
            std::thread::spawn(move || {
                host.remote.reap_wedged_master();
                if let Ok(mut probing) = probing.lock() {
                    probing.remove(&target);
                }
            });
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
        let (Some(host), Some(sink)) = (host, self.sink.clone()) else {
            return;
        };
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.reconnecting.insert((wid, name.clone()), stop.clone());
        spawn_reconnect_probe(sink, host, wid, name, real, stop);
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
        // A pre-existing session that dropped: honor its running host's level.
        let proto = host.remote.session_proto(&host.remote_ghost, &real);
        if self.attach_ssh_into(wid, &name, cmd, proto) {
            ghost_ui_core::focus_trace::log(
                &name,
                format_args!("transport REATTACHED (fresh emulator, resync inbound)"),
            );
            self.reconnecting.remove(&(wid, name.clone()));
            self.dispatch(wid, UiEvent::SessionReattached { name }, event_loop);
        } else if let Some(sink) = self.sink.clone() {
            // Raced: keep the hold, probe again from the floor.
            spawn_reconnect_probe(sink, host, wid, name, real, stop);
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
        // Already driven somewhere in this process → adopt in place: the caller's
        // AdoptSession takes drivership, and no second client / rebuild is opened.
        if self.sessions.contains_key(name) {
            return true;
        }
        let Some(w) = self.windows.get(&wid) else {
            return false;
        };
        // Complete the handshake at the window's real grid, never a provisional
        // 80×24: the host lays out its resync at the handshake size, so attaching
        // a maximized window at 80×24 would reflow a full-size screen down and
        // pin its cursor to that smaller bottom row — the next output then lands
        // mid-screen (see `RootModel::grid`).
        let (cols, rows) = w.root.grid();
        let identity = w.root.client_identity();
        match attach(name, cols, rows, &identity) {
            Ok(s) => {
                // Fresh transport → a resync (screen + scrollback) is inbound: rebuild
                // the shared mirror first so the replay lands clean (W1).
                self.states.resize_observed(name, cols, rows);
                self.drive_with_client(name, s);
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
    /// Attach a remote session over the transport. `proto` is the *running host's*
    /// level (see [`RemoteSsh::session_proto`]): a freshly-spawned session passes
    /// [`PROTO_LEVEL`] (spawned by the current staged binary, and reading its
    /// not-yet-written marker could race to 0); a discovered/pre-existing session
    /// passes the value read over its host's transport, so a post-marker message
    /// isn't sent to an older host that would drop the client.
    ///
    /// [`RemoteSsh::session_proto`]: ghost_vt::remote::RemoteSsh::session_proto
    /// [`PROTO_LEVEL`]: ghost_vt::protocol::PROTO_LEVEL
    fn attach_ssh_into(
        &mut self,
        wid: WindowId,
        name: &str,
        cmd: std::process::Command,
        proto: u32,
    ) -> bool {
        if self.sessions.contains_key(name) {
            return true;
        }
        let Some(w) = self.windows.get(&wid) else {
            return false;
        };
        let (cols, rows) = w.root.grid();
        let identity = w.root.client_identity();
        match attach_over_ssh(cmd, name, cols, rows, &identity, proto) {
            Ok(s) => {
                self.states.resize_observed(name, cols, rows);
                self.drive_with_client(name, s);
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
        // save persists the connection (sessions in it inherit it). A fresh connect
        // supersedes any held fallback choice (this may be a Retry off that screen).
        if let Some(w) = self.windows.get_mut(&wid) {
            w.root.set_group_connection(Some(spec.clone()));
            w.pending_fallback = None;
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
        // A fresh connect supersedes any held fallback choice (this may be a Retry).
        if let Some(w) = self.windows.get_mut(&wid) {
            w.pending_fallback = None;
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
                if let Some(sink) = self.sink.clone()
                    && let Some(setup) = self.windows.get_mut(&wid).and_then(|w| w.connect.take())
                {
                    spawn_connect_worker(
                        sink,
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
                // Just spawned by the current staged binary → our own level.
                if self.attach_ssh_into(
                    wid,
                    &local_id,
                    remote.pipe_command(&remote_ghost, &name),
                    ghost_vt::protocol::PROTO_LEVEL,
                ) {
                    if let Some(w) = self.windows.get_mut(&wid) {
                        w.root.end_connect();
                    }
                    self.dispatch(wid, UiEvent::AdoptSession(local_id), event_loop);
                } else {
                    self.connect_fail(wid, "could not attach to the remote session".into());
                }
            }
            // The remote can't host a protocol-matched ghost. Rather than silently
            // degrade to a local ssh child, stop the prompt on a choice screen that
            // names the reason and lets the user Retry, accept plain ssh
            // (`Cmd::UsePlainSshFallback` → [`use_plain_ssh_fallback`]), or Cancel.
            // The (spec, name) is held so the plain-ssh choice can spawn exactly the
            // session this connect had prepared.
            //
            // [`use_plain_ssh_fallback`]: App::use_plain_ssh_fallback
            ConnectOutcome::Fallback(why) => {
                let retryable = why.retryable();
                if let Some(w) = self.windows.get_mut(&wid) {
                    w.pending_fallback = Some((spec, name));
                    w.root.connect_offer_fallback(why.to_string(), retryable);
                    w.request_redraw();
                }
            }
            ConnectOutcome::Error(msg) => self.connect_fail(wid, msg),
        }
    }

    /// The user chose plain ssh on the transport-fallback screen: spawn the local
    /// `ssh <host>` child this connect had prepared (held in `pending_fallback`)
    /// and adopt it into the window, dismissing the prompt. Mirrors what the old
    /// silent fallback did — only now it's an explicit choice. A no-op if the
    /// pending context is gone (e.g. the window closed underneath the choice).
    fn use_plain_ssh_fallback(&mut self, wid: WindowId, event_loop: &dyn Frontend) {
        let Some((spec, name)) = self
            .windows
            .get_mut(&wid)
            .and_then(|w| w.pending_fallback.take())
        else {
            return;
        };
        if let Some(w) = self.windows.get_mut(&wid) {
            w.root.end_connect();
        }
        // The descriptor carries the `ConnectionSpec`, so the session is marked a
        // plain-ssh session by derivation (`foreground_connection`) — no stored
        // "is fallback" flag.
        spawn_session(&name, vec![], Some(spec));
        if self.attach_into(wid, &name) {
            self.dispatch(wid, UiEvent::AdoptSession(name), event_loop);
        }
    }

    /// Eagerly reconnect every host a startup restore is waiting on. The workers are
    /// the same never-give-up ones the loop keeps for any remembered host
    /// ([`Self::retry_remembered_hosts`]) — a restore just wants them started now
    /// rather than at the first wake, and the targets it queues are remembered
    /// members by construction. A no-op under a headless frontend (no proxy to post
    /// back on).
    fn reconnect_restored_remotes(&mut self) {
        self.retry_remembered_hosts();
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
            // A remembered session restored on its host: honor its running level.
            let proto = host.remote.session_proto(&host.remote_ghost, &real);
            if !self.attach_ssh_into(wid, &composite, cmd, proto)
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
        let Some(sink) = self.sink.clone() else {
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
        let watcher = start_remote_watcher(target.to_string(), host, sink);
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
        for id in self.sessions.keys() {
            if let Some((target, real)) = id.split_once(REMOTE_ID_SEP) {
                self.remote_index
                    .entry(id.clone())
                    .or_insert_with(|| (target.to_string(), real.to_string()));
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
        let held = self.sessions.contains_key(id);
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
        // Taking over a discovered session: honor its running host's level.
        let proto = host.remote.session_proto(&host.remote_ghost, real);
        if self.attach_ssh_into(wid, id, cmd, proto) {
            self.dispatch(wid, UiEvent::AdoptSession(id.to_string()), event_loop);
        }
    }

    /// Create a NEW session on a connected remote host (inheritance-over-remote):
    /// `ghost new -d <name>` over the transport, then attach it as this-window
    /// under the fleet-namespaced id — the same shape as a fresh connect or a
    /// take-over, so the new session is a full remote ghost session rather than a
    /// local `ssh` child. `target` must be a currently-connected host.
    ///
    /// The `ghost new -d` is a blocking `ssh` round trip, so it runs on a worker
    /// thread; the attach continues on the main loop in
    /// [`finish_remote_session_spawn`](Self::finish_remote_session_spawn) when the
    /// worker posts [`UserEvent::RemoteSessionSpawned`] back — never blocking the
    /// event loop, which a slow or wedged host would otherwise freeze for every
    /// window. Mirrors the connect worker ([`spawn_connect_worker`]).
    fn spawn_remote_session(&mut self, wid: WindowId, target: &str, name: &str) {
        let host = self
            .remotes
            .lock()
            .ok()
            .and_then(|m| m.get(target).cloned());
        let Some(host) = host else {
            eprintln!("ghost: no live connection to {target} to open a session on");
            return;
        };
        let Some(sink) = self.sink.clone() else {
            // Nothing to post the worker's result back to (an App with no sink).
            return;
        };
        let (target, name) = (target.to_string(), name.to_string());
        std::thread::spawn(move || {
            // Recovery (a dead remote tile's Recreate) reaches here after a reboot may
            // have wedged the shared master; clear it so the spawn opens a fresh
            // connection. A no-op on the healthy interactive new-session path.
            host.remote.reap_wedged_master();
            let result = host
                .remote
                .spawn_host(&host.remote_ghost, &name)
                .map_err(|e| e.to_string());
            sink.post(UserEvent::RemoteSessionSpawned {
                wid,
                target,
                name,
                result,
            });
        });
    }

    /// Attach a new remote session whose off-loop `ghost new -d` just finished (see
    /// [`spawn_remote_session`](Self::spawn_remote_session)): drive it as this-window
    /// under the composite id the watcher will discover it by. A window closed while
    /// the spawn ran drops the result — the created session persists on the host and
    /// surfaces in the fleet like any other detached one.
    fn finish_remote_session_spawn(
        &mut self,
        wid: WindowId,
        target: String,
        name: String,
        result: Result<(), String>,
        event_loop: &dyn Frontend,
    ) {
        if !self.windows.contains_key(&wid) {
            return;
        }
        if let Err(e) = result {
            eprintln!("ghost: could not open a session on {target}: {e}");
            return;
        }
        let host = self
            .remotes
            .lock()
            .ok()
            .and_then(|m| m.get(&target).cloned());
        let Some(host) = host else {
            eprintln!("ghost: lost the connection to {target} before attaching its new session");
            return;
        };
        // Drive it under the composite id the watcher will discover it by, so the
        // window owns its own new session in the fleet (the transport uses the bare
        // name); see [`finish_connect`](Self::finish_connect).
        let local_id = remote_fleet_id(&target, &name);
        self.remote_index
            .insert(local_id.clone(), (target.clone(), name.clone()));
        let cmd = host.remote.pipe_command(&host.remote_ghost, &name);
        // Just spawned by the current staged binary → our own level.
        if self.attach_ssh_into(wid, &local_id, cmd, ghost_vt::protocol::PROTO_LEVEL) {
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
        // Unlike an attach (which sends `Policy` and would be DROPPED by an older
        // host), an observe gains nothing from reading the host's real level: a host
        // that predates observation (`proto < PROTO_OBSERVE`) yields a cold preview
        // either way — refused locally if we pass its real level, or dropped by the
        // host if we pass ours. So skip the per-tile `__proto` round trip (this runs
        // once per remote tile as a fleet opens) and pass our own level; the visible
        // outcome is identical and the fleet-open stall stays flat.
        Subscriber::observe_ssh(cmd, ghost_vt::protocol::PROTO_LEVEL).ok()
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

    /// Restart remote session `real` on `target` under the current binary, over its
    /// host's transport, off the event loop (a blocking ssh round trip). The remote
    /// host is ended and respawned seeded from its recording, so the session returns
    /// speaking the current protocol level; a client driving it sees the transport
    /// drop (the old host died) and the reconnect path re-attaches to the new host.
    fn spawn_remote_restart(&self, target: &str, real: &str) {
        let Some(host) = self
            .remotes
            .lock()
            .ok()
            .and_then(|m| m.get(target).cloned())
        else {
            eprintln!("ghost: no live connection to {target} to restart its session");
            return;
        };
        let real = real.to_string();
        std::thread::spawn(move || {
            if let Err(e) = host.remote.restart_session(&host.remote_ghost, &real) {
                eprintln!("ghost: remote restart failed: {e}");
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
    /// reconfigures the surface and blits that snapshot for cheap feedback,
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
        let (step, display) = {
            let Some(w) = self.windows.get_mut(&wid) else {
                return;
            };
            let step = w.resize.note(now_ms, w_px, h_px, scale);
            // Described before `gfx` is borrowed mutably below: the bar is read
            // off both the window and the window's state.
            let bar = w.gfx.as_ref().map(|g| g.titlebar(w));
            let margins = w
                .gfx
                .as_ref()
                .map_or(ghost_ui_core::frame::FrameInset::NONE, |g| g.margins_px());
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
                    // blit it cheaply until the gesture settles (the real
                    // resize is committed from `about_to_wait`).
                    resize::Step::Defer => {
                        if !gfx.renderer.has_snapshot() {
                            let scene = ghost_ui_core::frame::with_frame(
                                w.root.view(&self.states),
                                bar.as_ref().expect("gfx implies a bar"),
                                margins,
                            );
                            let font_px = size_px() * w.root.render_scale();
                            gfx.renderer.capture_snapshot(&scene, gfx.fonts, font_px);
                        }
                        gfx.resize(w_px, h_px);
                        gfx.blit_snapshot();
                    }
                }
            }
            // The monitor this window is on — how far a program maximizing it can
            // grow the grid (`CSI 19 t`). Read here because this is where a window
            // learns it moved or changed size, monitor hops included.
            let display = w
                .gfx
                .as_ref()
                .and_then(|g| g.window.current_monitor())
                .map(|m| m.size());
            (step, display)
        };
        if let resize::Step::CommitNow((cw, ch, cs)) = step {
            if let Some(display) = display {
                self.dispatch(
                    wid,
                    UiEvent::DisplaySize {
                        w_px: display.width,
                        h_px: display.height,
                    },
                    event_loop,
                );
            }
            self.resize_model(wid, cw, ch, cs, event_loop);
        }
    }

    /// Open a new window in the fleet overview (owning no session yet), carrying
    /// `group` as its identity and opening at `size` cells (its configured default
    /// when `None`). The user spawns or takes over a session from there.
    pub fn open_fleet_window(
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
            cols: req_cols,
            rows: req_rows,
            pad: cfg.padding(),
            decorations: cfg.decorations(),
        });
        // Ask the realized window whether its compositor blurs; a headless window
        // has nothing to ask and needs no glass either way.
        let blur_supported = gfx
            .as_ref()
            .is_some_and(|g| backdrop_blur_supported(&g.window));
        // Everything below sizes the MODEL, which lays out under our titlebar.
        let h = h
            .saturating_sub(gfx.as_ref().map_or(0, |g| g.bar_px()))
            .max(1);
        let (mut root, states, init) = RootModel::fleet(metrics(), (w, h), scale as f32);
        // A fresh fleet owns no session, so its minted registry is empty; fold it in
        // for symmetry and stamp the shared registry so later mints take this theme.
        self.states.absorb(states);
        root.set_theme(&mut self.states, theme_colors(&cfg.theme()));
        // The same policy we report to every session host we attach to — the
        // window's emulators and the hosts' must agree, or an attached window would
        // honour what the host is refusing (see `ghost_term::policy`).
        root.set_policy(&mut self.states, session_policy_pair());
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
                mods: ModifiersState::empty(),
                pointer_pos: PointPx { x: 0.0, y: 0.0 },
                title: String::new(),
                focused: true,
                hovered_button: None,
                pressed_button: None,
                #[cfg(all(unix, not(target_os = "macos")))]
                frame_edge: None,
                pointer_down: false,
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
                blur_supported,
                connect: None,
                connect_gen: 0,
                pending_fallback: None,
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
    /// Open the windows this launch asked for (consuming [`App::startup`]).
    /// Returns whether anything opened — `false` means there is nothing to show
    /// and the process should exit.
    pub fn open_startup_windows(&mut self, event_loop: &dyn Frontend) -> bool {
        // Consumed once (the caller's guard keeps this from re-running); the
        // placeholder is never used.
        match std::mem::replace(&mut self.startup, Startup::Fleet) {
            Startup::Restore(records) => self.restore_workspace(event_loop, records),
            Startup::Fleet => {
                let group = self.mint_group();
                self.open_fleet_window(event_loop, group, None);
            }
            Startup::Connect => self.open_connect_window(event_loop),
            Startup::Single(name) => {
                let group = self.mint_group();
                if self
                    .open_single_window(event_loop, &name, group, None)
                    .is_none()
                {
                    return false;
                }
            }
        }
        true
    }

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
            // Supersede any in-flight connect on this window (a menu Cmd+G can reach
            // here while a worker runs): drop its warm-up, bump the generation so a
            // late worker is gen-rejected in `finish_connect` rather than hijacking
            // the fresh prompt, and clear a held fallback choice from a prior connect.
            w.connect = None;
            w.connect_gen = w.connect_gen.wrapping_add(1);
            w.pending_fallback = None;
            w.root.begin_connect_session();
            w.request_redraw();
        }
    }

    /// Open a new window that behaves exactly like a fresh launch (File > New Window
    /// / Cmd-N): reconnect through the fleet when it has a session to return to,
    /// otherwise spawn a fresh session and show it as a single view (see
    /// [`startup_choice`]). Runs in this same process, so the new window shares the
    /// clipboard, clock, and menu with the others.
    pub fn open_launch_window(&mut self, event_loop: &dyn Frontend) {
        // The listing a fleet window would reconcile against — local sessions plus
        // every connected host's — so a detached session on a remote host counts as
        // something to return to, and a remote member of a host we ARE connected to
        // isn't mistaken for one that is away.
        let mut sessions = session::list().unwrap_or_default();
        for r in self.remote_infos.values() {
            sessions.extend(r.iter().cloned());
        }
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

    /// Reconcile a session's ONE process-wide feed source against its live viewers —
    /// the seam that keeps "exactly one source per session, fanned to every viewer"
    /// true across every open/close. Run after any change to who drives or views `id`:
    /// - no window views it → drop the client, observer, shared state, and dead-replay
    ///   mark (the last-viewer prune; the session detaches on its host and lives on for
    ///   a later reattach).
    /// - a window drives it → the client is the source; drop any observer (a driver
    ///   plus an observer would double-feed the one emulator, finding #7).
    /// - viewed but driven nowhere → the source must be a read-only mirror: if a client
    ///   lingers (its driver just left/closed) detach it and downgrade to an observer so
    ///   previewers keep updating; open one if none exists yet. A remote session downgrades
    ///   to an observer over its host's transport (`observe_remote`), not a local socket —
    ///   the fleet's own reconcile can't heal it (it optimistically believes it already
    ///   observes a session it deduped away), so this is the one seam that re-sources it.
    fn reconcile_source(&mut self, id: &str) {
        let driven = self.windows.values().any(|w| w.root.drives(id));
        let viewed = self.windows.values().any(|w| w.root.views(id));
        if !viewed {
            self.sessions.remove(id);
            self.observers.remove(id);
            self.states.discard(id);
            self.dead_fed.remove(id);
            return;
        }
        if driven {
            self.observers.remove(id);
            return;
        }
        let had_client = self.sessions.remove(id).is_some();
        if had_client || !self.observers.contains_key(id) {
            let sub = if let Some((target, real)) = self.remote_index.get(id).cloned() {
                self.observe_remote(&target, &real)
            } else if !is_remote_id(id) {
                Subscriber::observe(id).ok()
            } else {
                // A remote id with no live index entry (its host dropped): nothing to
                // observe over. Leave the last frame; a later reconnect re-sources it.
                None
            };
            if let Some(sub) = sub {
                self.observers.insert(id.to_string(), sub);
            }
        }
    }

    /// Install `s` as the driving client for `id`, first dropping any read-only
    /// observer of the same session. A client is the authoritative feed source; an
    /// observer left in place would be a SECOND source into the one shared emulator —
    /// the finding-#7 double-feed the per-wake pump asserts against, which aborts the
    /// app on the next `wake`. This is the observed→driven UPGRADE, the mirror of the
    /// driven→observed downgrade in [`reconcile_source`](Self::reconcile_source): every
    /// path that gives this process a client for a session (attach, take-over, spawn,
    /// remote reconnect, window construction) routes the insert through here so the
    /// dedup can never be forgotten at one site. The observer's `Subscriber` drops with
    /// it, closing its transport.
    fn drive_with_client(&mut self, id: &str, s: Session) {
        self.observers.remove(id);
        self.sessions.insert(id.to_string(), s);
    }

    /// Drop shared states nothing references any more — a session that vanished (killed
    /// from another process, its tile gone) leaving no view and no feed source. The
    /// process-wide replacement for the fleet's old per-window `sessions.retain`, which
    /// under the shared registry would delete a state another window still uses. Pruning
    /// only a state that is BOTH unviewed AND has no client AND no observer is safe: a
    /// mid-construction or mid-feed state always has one of those, so this never races a
    /// state into deletion while a window is wiring it up.
    fn prune_orphan_states(&mut self) {
        for id in self.states.ids() {
            if !self.sessions.contains_key(&id)
                && !self.observers.contains_key(&id)
                && !self.windows.values().any(|w| w.root.views(&id))
            {
                self.states.discard(&id);
                self.dead_fed.remove(&id);
            }
        }
    }

    /// Remove a window; its session clients/observers/states are process-wide and
    /// outlive it, so a last-viewer prune ([`reconcile_source`](Self::reconcile_source))
    /// drops only those no surviving window views — the "close = detach" default,
    /// refcounted across windows. A session another window still drives or previews
    /// keeps its one live source (its driver downgraded to a mirror if this window was
    /// the driver and another only previews it).
    /// The window was asked to close — by its own button, or by the desktop.
    /// Closing is detaching: dropping the window drops its session clients and
    /// the hosts keep the sessions running. The last window out shuts down.
    fn close_requested(&mut self, wid: WindowId, event_loop: &dyn Frontend) {
        self.close_window(wid);
        if self.windows.is_empty() {
            self.shutdown(event_loop);
        }
    }

    fn close_window(&mut self, wid: WindowId) {
        self.windows.remove(&wid);
        let touched: Vec<String> = self
            .sessions
            .keys()
            .chain(self.observers.keys())
            .cloned()
            .collect();
        for id in touched {
            self.reconcile_source(&id);
        }
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
        }
        // A driven remote session's id is `<target>␟<real>`; read the target straight
        // off the process-wide client set (not via the index, which a poll failure can
        // clear).
        for name in self.sessions.keys() {
            if let Some((target, _)) = remote_id_parts(name) {
                targets.insert(target.to_string());
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
        self.remote_remembered.retain(|t, _| in_use.contains(t));
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
    pub fn save_workspace(&mut self) {
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

/// Read-only views into the shell's state, for the end-to-end tests in `tests/`
/// (see the crate docs on the lib/bin split). Assertions belong on observable
/// window behaviour — what mode a window is in, what it shows, what it drives —
/// so these hand out the window's model, never its internals.
impl App {
    /// Every open window, in unspecified order.
    pub fn window_ids(&self) -> Vec<WindowId> {
        self.windows.keys().copied().collect()
    }

    /// A window's model — the thing that decides what the window shows.
    pub fn root(&self, wid: WindowId) -> Option<&RootModel> {
        self.windows.get(&wid).map(|w| &w.root)
    }

    /// The one shared session-state registry (one emulator per session, fanned to
    /// every viewing window), for rendering a window's `Scene` in a test.
    pub fn states(&self) -> &Sessions {
        &self.states
    }

    /// The session-group registry as the shell currently holds it.
    pub fn groups(&self) -> &[ghost_ui_core::Group] {
        &self.groups
    }

    /// The saved workspace on disk — what a bare launch restores.
    pub fn saved_workspace() -> Vec<ghost_ui_core::WindowRecord> {
        windows::load()
    }

    /// Session ids this process holds a live client for (the driven set).
    pub fn driven_ids(&self) -> Vec<String> {
        self.sessions.keys().cloned().collect()
    }
}

impl App {
    /// Open a single-session view attached to `name`, carrying `group` as the
    /// window's identity and opening at `size` cells (its configured default when
    /// `None`; a restored window passes the grid it was last sized to). Returns
    /// the new window's id, or `None` if the attach fails.
    pub fn open_single_window(
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
            cols: req_cols,
            rows: req_rows,
            pad: cfg.padding(),
            decorations: cfg.decorations(),
        });
        // Ask the realized window whether its compositor blurs; a headless window
        // has nothing to ask and needs no glass either way.
        let blur_supported = gfx
            .as_ref()
            .is_some_and(|g| backdrop_blur_supported(&g.window));
        // Everything below sizes the MODEL, which lays out under our titlebar.
        let h = h
            .saturating_sub(gfx.as_ref().map_or(0, |g| g.bar_px()))
            .max(1);
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
        let (mut root, states) = RootModel::single(model, metrics(), (w, h));
        // Fold this window's minted foreground state into the one process-wide
        // registry (a no-op-keep if another window already drives it — this window
        // then borrows the shared emulator), then stamp the shared registry.
        self.states.absorb(states);
        root.set_theme(&mut self.states, theme_colors(&cfg.theme()));
        root.set_policy(&mut self.states, session_policy_pair());
        root.set_padding(cfg.padding());
        // Seed the persisted registry BEFORE the group claim, so the claim's
        // save extends it rather than clobbering it with just this window.
        root.update(&mut self.states, UiEvent::GroupsLoaded(self.groups.clone()));
        let claims = root.set_my_group(group);
        apply_anim_ms(&mut root);
        // The process holds exactly one client per session; this window drives it.
        // If one already exists (a second window onto a live session), keep it and
        // drop the extra attach rather than double-driving the one emulator. Either
        // way this window DRIVES `name`, so drop any read-only observer of it first —
        // a client plus an observer is the finding-#7 double-feed (see
        // [`drive_with_client`](Self::drive_with_client), the upgrade this open mirrors).
        self.observers.remove(name);
        self.sessions.entry(name.to_string()).or_insert(session);
        self.windows.insert(
            wid,
            WindowState {
                gfx,
                root,
                mods: ModifiersState::empty(),
                pointer_pos: PointPx { x: 0.0, y: 0.0 },
                title: String::new(),
                focused: true,
                hovered_button: None,
                pressed_button: None,
                #[cfg(all(unix, not(target_os = "macos")))]
                frame_edge: None,
                pointer_down: false,
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
                blur_supported,
                connect: None,
                connect_gen: 0,
                pending_fallback: None,
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
    pub fn restore_workspace(
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
    pub fn on_user_event(&mut self, fe: &dyn Frontend, event: UserEvent) {
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
            // The watcher also fetched the host's descriptor names: stash them and
            // re-sweep, so a member the host no longer remembers (a clean remote
            // exit) drops its relaunchable tile without any user action. A failed
            // fetch (`None`) clears the cache — unknown, not stale — and the
            // sweep stays conservative.
            UserEvent::RemoteRemembered { target, names } => {
                match names {
                    Some(names) => {
                        self.remote_remembered.insert(target, names);
                    }
                    None => {
                        self.remote_remembered.remove(&target);
                    }
                }
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
            // Same, for a launch that asked for an ssh window (`ghost --ssh-window`,
            // the desktop entry's action): open the connect prompt in a new window.
            UserEvent::OpenSshWindow => {
                self.open_connect_window(fe);
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
            // The off-loop `ghost new -d` on a connected remote finished: attach the
            // new session (or report the failure) on the main thread.
            UserEvent::RemoteSessionSpawned {
                wid,
                target,
                name,
                result,
            } => {
                self.finish_remote_session_spawn(wid, target, name, result, fe);
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
        if !self.open_startup_windows(&fe) {
            fe.exit();
            return;
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
        self.wake(&fe);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let fe = WinitFrontend { event_loop };
        match event {
            WindowEvent::CloseRequested => self.close_requested(id, &fe),
            WindowEvent::Resized(size) => {
                // Defer the costly relayout: capture + blit now, commit the
                // real resize once the drag settles (see `resize_step`).
                let Some(scale) = self
                    .windows
                    .get(&id)
                    .and_then(|w| w.gfx.as_ref())
                    .map(|g| g.window.scale_factor())
                else {
                    return;
                };
                tracing::trace!(target: "ghost::frame", ?size, "Resized");
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
                    let bar = win.gfx.as_ref().map(|g| g.titlebar(win));
                    let margins = win
                        .gfx
                        .as_ref()
                        .map_or(ghost_ui_core::frame::FrameInset::NONE, |g| g.margins_px());
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
                        let scene = win.root.view(&self.states);
                        // The model laid out in the space under our titlebar and
                        // inside our shadow margins; put that space where it belongs
                        // and draw the bar above it.
                        let bar_px = gfx.bar_px();
                        let scene = ghost_ui_core::frame::with_frame(
                            scene,
                            bar.as_ref().expect("gfx implies a bar"),
                            margins,
                        );
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
                        if let Some(a) = win.root.ime_cursor_area(&self.states) {
                            gfx.window.set_ime_cursor_area(
                                PhysicalPosition::new(
                                    a.x + margins.left as f32,
                                    a.y + (bar_px + margins.top) as f32,
                                ),
                                PhysicalSize::new(a.w, a.h),
                            );
                        }
                        match gfx.render(&scene, font_px) {
                            FrameOutcome::Presented { build, present } => {
                                // A frame landed: the pending repaint is satisfied, and
                                // the first-present retry loop below can stop.
                                win.pacer.painted(now_ms);
                                win.presented_ok = true;
                                tracing::trace!(target: "ghost::present", window = ?id, showing = ?win.root.showing(), t = now_ms, "presented");
                                // The foreground was just composited: reset its per-session
                                // damage baseline so the next `view` measures change from
                                // here (a Lost frame leaves the pending damage to fold into
                                // the next real present). See `RootModel::mark_presented`.
                                win.root.mark_presented(&mut self.states);
                                // Model-side cache line (fleet preview frames) under
                                // `RUST_LOG=ghost::cache=trace`, alongside the renderer's.
                                win.root.emit_cache_trace();
                                // Advance the watchdog's real-present baseline (always, so
                                // the self-heal in `about_to_wait` has an accurate view even
                                // without the trace flag). The kick oracle: a present that
                                // ends an armed stall reports the frozen state it just
                                // recovered — logged only under the trace flag to keep a
                                // normal run quiet.
                                let core = win.root.foreground_trace(&self.states);
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
                                tracing::trace!(target: "ghost::present", window = ?id, t = now_ms, "clean");
                                let core = win.root.foreground_trace(&self.states);
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
                                // Re-arm the repaint so `about_to_wait` retries (paced to
                                // the frame budget, parked while occluded) until a frame
                                // lands — this is what recovers a window whose redraws
                                // the platform dropped.
                                win.pacer.request();
                                let core = win.root.foreground_trace(&self.states);
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
                // can throttle the poll loop on top. Becoming visible again re-arms a
                // repaint — but the backing store macOS held may have been discarded
                // while we were hidden, so a plain repaint could come back `Clean`
                // against a stale texture and leave the window showing old content.
                // Force a full re-render (not a Clean skip) so the first visible frame
                // is provably fresh.
                if let Some(w) = self.windows.get_mut(&id) {
                    // Record it so the render-stall watchdog skips a window that can't
                    // present (its Lost-looping surface is the platform withholding the
                    // drawable, not our repaint bug).
                    w.occluded = occluded;
                    if !occluded {
                        if let Some(gfx) = w.gfx.as_mut() {
                            gfx.force_foreground_repaint();
                        }
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
                    // (see `Occluded` above): regaining focus forces a fresh full frame
                    // too, in case the backing store was discarded while unfocused.
                    if let Some(w) = self.windows.get_mut(&id) {
                        if let Some(gfx) = w.gfx.as_mut() {
                            gfx.force_foreground_repaint();
                        }
                        w.pacer.request();
                    }
                }
                // The frame's shadow lightens in the backdrop, so the corner it
                // hands us has to lighten with it — and it needs a frame to do
                // that in. Losing focus changes nothing else the shell draws, so
                // without asking here the old corner stays on the glass until
                // something unrelated redraws.
                if let Some(w) = self.windows.get_mut(&id) {
                    if let Some(gfx) = w.gfx.as_mut() {
                        gfx.refresh_window_edge(focused);
                    }
                    // A press whose release lands in another window leaves the
                    // button stuck "down" here, and a stuck button means the frame
                    // never offers a resize handle again.
                    if !focused {
                        w.pointer_down = false;
                    }
                    w.focused = focused;
                    w.pacer.request();
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
                // The frame gets first refusal on the pointer: its resize band
                // lies inside the window, over the padding and the first pixels
                // of the grid, and a model that saw it would be selecting text
                // under a resize cursor. Its titlebar is chrome the model has no
                // coordinate for at all.
                use ghost_ui_core::frame::FrameHit;
                let hit = self.frame_hit(id, pos);
                #[cfg(all(unix, not(target_os = "macos")))]
                self.track_frame_edge(
                    id,
                    match hit {
                        FrameHit::Resize(edge) => Some(edge),
                        _ => None,
                    },
                );
                #[cfg(target_os = "linux")]
                self.track_bar_hover(id, matches!(hit, FrameHit::Bar).then_some(pos));
                let FrameHit::Content(pos) = hit else {
                    return;
                };
                self.dispatch(
                    id,
                    UiEvent::Pointer {
                        phase: PointerPhase::Motion,
                        button: None,
                        pos,
                        mods,
                        wheel: WheelDelta::NONE,
                        clicks: 1,
                    },
                    &fe,
                );
            }
            WindowEvent::MouseInput { state, button, .. } => {
                self.note_input(id);
                if let Some(b) = map_button(button) {
                    let pressed = state == ElementState::Pressed;
                    // Grabbing one of our own frame's edges hands the resize to the
                    // compositor, which takes the pointer with it — there is no
                    // release to wait for, and the model never sees the press.
                    #[cfg(all(unix, not(target_os = "macos")))]
                    if pressed && b == PointerButton::Left {
                        let edge = self.windows.get(&id).and_then(|w| w.frame_edge);
                        if let Some(edge) = edge {
                            if let Some(gfx) = self.windows.get(&id).and_then(|w| w.gfx.as_ref()) {
                                let _ = gfx.window.drag_resize_window(resize_direction(edge));
                            }
                            return;
                        }
                    }
                    // Counted once, before anything can consume the press: the
                    // titlebar's double-click needs the same count the model
                    // would have seen, and counting again below would make every
                    // click on the bar a double one.
                    let Some((clicks, pos, mods)) = self.windows.get_mut(&id).map(|w| {
                        let clicks = if pressed { w.count_click(b) } else { 1 };
                        (clicks, w.pointer_pos, from_winit::mods(w.mods))
                    }) else {
                        return;
                    };
                    // The titlebar is chrome: its presses drive the window, not
                    // the model, and its buttons fire on release.
                    #[cfg(target_os = "linux")]
                    if pressed {
                        if self.press_on_bar(id, pos, b, clicks) {
                            return;
                        }
                    } else if self.release_on_bar(id, pos, &fe) {
                        return;
                    }
                    if let Some(w) = self.windows.get_mut(&id) {
                        w.pointer_down = pressed;
                    }
                    let phase = if pressed {
                        PointerPhase::Press
                    } else {
                        PointerPhase::Release
                    };
                    let Some(pos) = self.in_content(id, pos) else {
                        return;
                    };
                    self.dispatch(
                        id,
                        UiEvent::Pointer {
                            phase,
                            button: Some(b),
                            pos,
                            mods,
                            wheel: WheelDelta::NONE,
                            clicks,
                        },
                        &fe,
                    );
                }
            }
            WindowEvent::MouseWheel {
                delta, momentum, ..
            } => {
                self.note_input(id);
                // Keep the device's unit: a wheel click is a discrete notch, a
                // trackpad reports smooth pixel travel — the core paces each.
                // The OS's post-flick coasting is marked so the core can damp
                // it (our vendored-winit `momentum` patch).
                let wheel = match delta {
                    MouseScrollDelta::LineDelta(_, y) => WheelDelta::Notches(y as f64),
                    MouseScrollDelta::PixelDelta(p) if momentum => WheelDelta::Momentum(p.y),
                    MouseScrollDelta::PixelDelta(p) => WheelDelta::Pixels(p.y),
                };
                let Some((pos, mods)) = self
                    .windows
                    .get(&id)
                    .map(|w| (w.pointer_pos, from_winit::mods(w.mods)))
                else {
                    return;
                };
                let Some(pos) = self.in_content(id, pos) else {
                    return;
                };
                self.dispatch(
                    id,
                    UiEvent::Pointer {
                        phase: PointerPhase::Wheel,
                        button: None,
                        pos,
                        mods,
                        wheel,
                        clicks: 1,
                    },
                    &fe,
                );
            }
            _ => {}
        }
    }
}

impl App {
    /// Deterministically choose the ONE window that drives `name` — the geometry
    /// source and reconnect owner for its shared feed. Prefer the window showing it as
    /// its Single foreground (a take-over claimant foregrounds it), else any window
    /// that drives it, else none. Stable across wakes so a transient two-driver steal
    /// doesn't flip the child's query answers (HashMap order is nondeterministic).
    fn pick_driver(&self, name: &str) -> Option<WindowId> {
        let mut fallback = None;
        for (wid, w) in &self.windows {
            if w.root.drives(name) {
                if w.root.foregrounds(name) {
                    return Some(*wid);
                }
                fallback.get_or_insert(*wid);
            }
        }
        fallback
    }

    /// Feed a driven session's output into the ONE shared emulator once and fan the
    /// reaction to every window viewing it: the [`pick_driver`](Self::pick_driver)
    /// window supplies the ingest geometry and answers the child; the rest fold the
    /// same outcome as observers. A client with no driving view (transitional) is fed
    /// as observed so any previewer still updates. Commands are buffered while the
    /// window borrows are live, then executed.
    fn feed_driven_to_windows(&mut self, name: &str, bytes: &[u8], ended: bool, fe: &dyn Frontend) {
        let driver_wid = self.pick_driver(name);
        let mut buffered: Vec<(WindowId, Vec<Cmd>)> = Vec::new();
        {
            let App {
                windows, states, ..
            } = self;
            let mut driver: Option<&mut RootModel> = None;
            let mut obs_wids: Vec<WindowId> = Vec::new();
            let mut observers: Vec<&mut RootModel> = Vec::new();
            for (wid, w) in windows.iter_mut() {
                if Some(*wid) == driver_wid {
                    driver = Some(&mut w.root);
                } else if w.root.views(name) {
                    obs_wids.push(*wid);
                    observers.push(&mut w.root);
                }
            }
            match driver {
                Some(driver) => {
                    let (dc, oc) = ghost_ui_core::feed_shared(
                        states,
                        driver,
                        &mut observers,
                        name,
                        bytes,
                        ended,
                    );
                    if let Some(dw) = driver_wid {
                        buffered.push((dw, dc));
                    }
                    for (wid, cmds) in obs_wids.into_iter().zip(oc) {
                        buffered.push((wid, cmds));
                    }
                }
                None => {
                    let oc =
                        ghost_ui_core::feed_observed(states, &mut observers, name, bytes, ended);
                    for (wid, cmds) in obs_wids.into_iter().zip(oc) {
                        buffered.push((wid, cmds));
                    }
                }
            }
        }
        for (wid, cmds) in buffered {
            self.exec(wid, cmds, fe);
        }
    }

    /// Feed an observed session's output (a mirror no window in this process drives)
    /// into the shared emulator once and fan to every previewing tile — the child's
    /// own effects dropped, every view folded as an observer. Buffered-then-executed
    /// like [`feed_driven_to_windows`](Self::feed_driven_to_windows).
    fn feed_observed_to_viewers(
        &mut self,
        name: &str,
        bytes: &[u8],
        ended: bool,
        fe: &dyn Frontend,
    ) {
        let mut buffered: Vec<(WindowId, Vec<Cmd>)> = Vec::new();
        {
            let App {
                windows, states, ..
            } = self;
            let mut wids: Vec<WindowId> = Vec::new();
            let mut viewers: Vec<&mut RootModel> = Vec::new();
            for (wid, w) in windows.iter_mut() {
                if w.root.views(name) {
                    wids.push(*wid);
                    viewers.push(&mut w.root);
                }
            }
            let per = ghost_ui_core::feed_observed(states, &mut viewers, name, bytes, ended);
            for (wid, cmds) in wids.into_iter().zip(per) {
                buffered.push((wid, cmds));
            }
        }
        for (wid, cmds) in buffered {
            self.exec(wid, cmds, fe);
        }
    }

    /// Fan an ended session's lifecycle to every window viewing it — the foreground
    /// switches to the next session (or drops to the fleet), a warm mirror and its
    /// ownership are released — then prune the shared source once the last viewer let
    /// go. The final frame already rendered in each view via the feed that carried
    /// `ended`; this is only the reaction the per-window dispatch used to run inline.
    fn end_session_in_views(&mut self, name: &str, fe: &dyn Frontend) {
        let viewers: Vec<WindowId> = self
            .windows
            .iter()
            .filter(|(_, w)| w.root.views(name))
            .map(|(id, _)| *id)
            .collect();
        for wid in viewers {
            self.dispatch(
                wid,
                UiEvent::SessionEnded {
                    name: name.to_string(),
                },
                fe,
            );
        }
        self.reconcile_source(name);
    }

    /// The once-per-wake work behind [`ApplicationHandler::about_to_wait`], taken
    /// over the abstract [`Frontend`] rather than winit's `ActiveEventLoop` — so a
    /// headless test can drive the very same session pump/feed/tick/repaint pass the
    /// live loop runs, with no window server. Pump each session's one client and each
    /// read-only observer, fan the output into every viewing window, fan pushed
    /// session state, fire due ticks, and release paced repaints.
    pub fn wake(&mut self, fe: &dyn Frontend) {
        // A long wake-to-wake gap means we were parked — a slept laptop kills
        // remote TCP with nothing reaching the local master — so probe the
        // transports rather than letting keystrokes die in a dead pipe until
        // ssh's ~45s keepalive notices (see `SUSPEND_PROBE_GAP`).
        let now = Instant::now();
        let parked = now.duration_since(self.last_wake_at);
        if parked >= SUSPEND_PROBE_GAP {
            ghost_ui_core::focus_trace::log(
                "*",
                format_args!(
                    "suspend gap {}s -> probing remote transports",
                    parked.as_secs()
                ),
            );
            self.probe_remote_transports();
        }
        self.last_wake_at = now;
        // Flush the workspace snapshot once per wake if a handled event or a
        // window open/close marked it dirty (write-on-change guards the disk).
        if self.workspace_dirty {
            self.save_workspace();
        }
        // Keep waiting on every remote host a group still remembers a session on:
        // starts a worker for one that just went away, stops one nothing wants.
        self.retry_remembered_hosts();
        // Pump the process's session clients once each — one client per session no
        // matter how many windows view it — and fan each one's output into the ONE
        // shared emulator and out to every window showing it (the driven half of the
        // "one model, many views" feed).
        let driven: Vec<String> = self.sessions.keys().cloned().collect();
        let mut dropped: Vec<(String, Vec<u8>)> = Vec::new();
        let mut ended_driven: Vec<String> = Vec::new();
        for name in driven {
            // The pump is also where a `flush_pending` retries input the transport
            // refused, so the depth AFTER it is what is really stuck (see
            // `note_input_queue`).
            let (bytes, end, queued) = match self.sessions.get_mut(&name) {
                Some(s) => {
                    let (bytes, end) = pump(s, 32);
                    (bytes, end, s.pending_input())
                }
                None => continue,
            };
            self.note_input_queue(&name, queued, now);
            // A REMOTE session whose transport dropped is held and reconnected, not
            // torn down — its session may still be alive on the far side. A local EOF
            // (the host process is gone) is a genuine end, as before.
            if end == PumpEnd::Disconnected && is_remote_id(&name) {
                ghost_ui_core::focus_trace::log(
                    &name,
                    format_args!("transport DISCONNECTED (holding for reconnect)"),
                );
                self.sessions.remove(&name);
                dropped.push((name, bytes));
                continue;
            }
            let ended = end.is_end();
            if ended {
                // Drop the dead client before the fan so a stale query-reply is
                // ignored; whether a window itself ends is decided by `SessionEnded`.
                self.sessions.remove(&name);
            }
            if !bytes.is_empty() || ended {
                tracing::trace!(target: "ghost::present", session = %name, n = bytes.len(), "feed");
                self.feed_driven_to_windows(&name, &bytes, ended, fe);
            }
            if ended {
                ended_driven.push(name);
            }
        }
        // A session that ended or dropped takes its input watch with it: the next
        // client for that id starts from an empty queue, not a stale episode.
        if !self.input_stalls.is_empty() {
            let live = &self.sessions;
            self.input_stalls.retain(|name, _| live.contains_key(name));
        }
        // Fan the ended lifecycle: the final frame already rendered in every view via
        // the feed above; now switch each foreground away / drop each warm mirror, and
        // prune the shared state once its last viewer let go.
        for name in ended_driven {
            self.end_session_in_views(&name, fe);
        }
        // A dropped remote session: flush its last bytes, put its tile into the
        // reconnecting hold (frozen + dimmed, not torn down), and start retrying under
        // the window that was driving it.
        for (name, bytes) in dropped {
            if !bytes.is_empty() {
                self.feed_driven_to_windows(&name, &bytes, false, fe);
            }
            let driver = self.pick_driver(&name);
            let viewers: Vec<WindowId> = self
                .windows
                .iter()
                .filter(|(_, w)| w.root.views(&name))
                .map(|(id, _)| *id)
                .collect();
            for v in viewers {
                self.dispatch(v, UiEvent::SessionDisconnected { name: name.clone() }, fe);
            }
            if let Some(wid) = driver {
                self.begin_reconnect(wid, name);
            }
        }
        // Pump the process's read-only observers once each: a mirror's output feeds the
        // shared state once and fans to every previewing tile; its `Resized` re-seeds
        // the shared state at the new grid (only the shell may, keyed to this genuine
        // observer stream) before the resync that follows heals the content.
        let observed: Vec<String> = self.observers.keys().cloned().collect();
        debug_assert!(
            self.observers
                .keys()
                .all(|k| !self.sessions.contains_key(k)),
            "a session is both driven and observed — double-feed (finding #7)"
        );
        for name in observed {
            let p = match self.observers.get_mut(&name) {
                Some(sub) => sub.pump().unwrap_or_default(),
                None => continue,
            };
            for e in p.events {
                if let ghost_vt::protocol::SessionEvent::Resized { cols, rows } = e {
                    self.states.resize_observed(&name, cols, rows);
                    let viewers: Vec<WindowId> = self
                        .windows
                        .iter()
                        .filter(|(_, w)| w.root.views(&name))
                        .map(|(id, _)| *id)
                        .collect();
                    for v in viewers {
                        self.dispatch(
                            v,
                            UiEvent::SessionPush {
                                name: name.clone(),
                                push: SessionPush::Event(
                                    ghost_vt::protocol::SessionEvent::Resized { cols, rows },
                                ),
                            },
                            fe,
                        );
                    }
                }
            }
            if !p.output.is_empty() || p.ended {
                self.feed_observed_to_viewers(&name, &p.output, p.ended, fe);
            }
            if p.ended {
                self.observers.remove(&name);
                self.end_session_in_views(&name, fe);
            }
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
        self.pump_subscriptions(fe);
        // Reap shared states nothing references any more (a session that vanished with
        // its tile, leaving no view and no source) — the process-wide replacement for
        // the fleet's old per-window prune, run once all this wake's reconciles landed.
        self.prune_orphan_states();
        // A changed `ui.toml` (config-dir watch) hot-reloads the live-reloadable
        // settings into every window — the compositor blur, opacity, frost, color
        // scheme, and padding.
        // The compositor can also change the answer out from under us: a Wayland
        // blur effect switched off mid-session withdraws the capability, and the
        // frost fallback has to take over (and hand back) with it. The check is a
        // lock-free load per window; re-applying the whole config on a change is
        // fine because a change happens when a human toggles a desktop setting.
        let blur_changed = self.windows.values().any(|w| {
            w.gfx
                .as_ref()
                .is_some_and(|g| backdrop_blur_supported(&g.window) != w.blur_supported)
        });
        // Both flags are read every pass — `swap` must not be short-circuited away
        // by a blur change, or the pending config edit would be re-applied again
        // on the next pass instead of being consumed here.
        let config_changed = self
            .config_changed
            .swap(false, std::sync::atomic::Ordering::Relaxed);
        if blur_changed || config_changed {
            self.reload_config(&config::UiConfig::load(), fe);
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
            self.dispatch(wid, UiEvent::Tick { now_ms }, fe);
        }
        // Bench mode: advance the scripted animation (after ticks, so `is_animating`
        // reflects this turn's animation state).
        if self.bench.is_some() {
            self.drive_bench(fe);
        }
        // A session ending never closes its window: the model has already switched
        // to the next attached session (or the fleet), so the window lives on until
        // the user closes it. Windows are removed only on an explicit close.
        // Commit any interactive resize that has settled (drag paused/released) or
        // hit its max refresh interval: drop the blit snapshot and dispatch
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
            self.resize_model(wid, cw, ch, cs, fe);
        }
        // Release any paced repaint that the frame budget now allows. The loop
        // re-enters here every `POLL` (8 ms < the 16 ms budget), so a deferred
        // paint is always re-checked and fires within a frame of becoming due;
        // a keystroke's repaint, handled in this same pass, paints at once.
        for (id, w) in self.windows.iter_mut() {
            if w.release_repaint_due(now_ms) {
                w.render_trace.saw_release(now_ms);
                tracing::trace!(target: "ghost::present", window = ?id, t = now_ms, "release");
                w.request_redraw();
            }
            // Once per pass, fold the foreground gate state and classify. Runs always
            // (not just under the trace flag) so a stale-frame freeze can self-heal in
            // the wild — the fold/verdict is a few subtractions, and the diagnostic dump
            // self-filters through the `trace!` level. The window id separates concurrent
            // windows' tracks in a multi-window log.
            let core = w.root.foreground_trace(&self.states);
            let has_snapshot = w.gfx.as_ref().is_some_and(|g| g.renderer.has_snapshot());
            let pending = w.pacer.pending();
            let visible = !w.occluded;
            if let Some(report) = w
                .render_trace
                .poll(now_ms, core, pending, has_snapshot, visible)
            {
                tracing::trace!(target: "ghost::render", window = ?id, %report, "foreground render stall");
            }
            // Self-heal: when the watchdog sees a freeze a re-present can fix — a
            // stale-no-present (visible output streaming, but no real present reached the
            // glass — the Clean-over-stale texture staleness) or a synchronized hold
            // stuck a second past its backstop (its deferred release repaint never
            // landed) — force one full foreground re-present. Rate-limited to one per
            // HEAL_COOLDOWN_MS, so a persistent freeze becomes a one-frame glitch and a
            // false trigger just redraws identical pixels (no flicker). Warn so a
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
        self.assert_foreground_states_present("after wake");
        fe.set_control_flow(ControlFlow::WaitUntil(Instant::now() + POLL));
    }
}

#[cfg(test)]
mod tests {
    use super::menu::{ConnectOutcome, UserEvent};
    use super::{
        App, Glass, HeadlessFrontend, INPUT_STALL_GRACE, INPUT_STALL_PROBE, InputStall,
        PendingRemote, REMOTE_ID_SEP, StallEvent, StartupChoice, auth_error_message,
        choose_alpha_mode, choose_surface_format, config, connect_outcome_wanted, glass,
        home_launch_dir, inherited_connection, namespace_remote_infos, new_window_choice,
        password_prompt, remote_spawn_target, respawn_opts, restore_plan, should_restore,
        startup_choice, theme_colors,
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    use super::{EdgeState, window_edge_for};
    use ghost_ui_core::WindowRecord;
    use ghost_vt::connection::ConnectionSpec;
    use ghost_vt::session::SessionInfo;
    use std::collections::HashSet;
    use std::time::{Duration, Instant};
    use wgpu::CompositeAlphaMode::{Opaque, PostMultiplied, PreMultiplied};
    use wgpu::TextureFormat::{
        Bgra8Unorm, Bgra8UnormSrgb, Rgb10a2Unorm, Rgba8Unorm, Rgba8UnormSrgb, Rgba16Float,
    };

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn a_tiled_window_squares_off_like_a_maximized_one() {
        // A half-snapped window meets the screen edge and its neighbour along
        // three sides: rounding there cuts a see-through wedge into a corner
        // that has nothing behind it, and the outline traces an edge that isn't
        // one. GNOME squares off its own windows when they tile, and a snapped
        // window is tiled WITHOUT being maximized — so asking `is_maximized`
        // alone leaves ours curved against the wall.
        let floating = window_edge_for(EdgeState {
            opaque: false,
            focused: true,
            boxed_in: false,
            own_frame: false,
        });
        let tiled = window_edge_for(EdgeState {
            opaque: false,
            focused: true,
            boxed_in: true,
            own_frame: false,
        });
        assert!(floating.radius > 0.0, "a floating window keeps its curve");
        assert_eq!(tiled.radius, 0.0, "a tiled window has no corner to round");
        assert_eq!(tiled.corners, ghost_renderer::Corners::NONE);
        assert_eq!(tiled.outline, 0.0, "nor an outside edge to outline");
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn only_our_own_frame_rounds_the_top_corners() {
        // The frame above us rounds its own top corners; cutting there too takes
        // a bite out of its curve. Without it the whole edge is ours.
        let edge = |own_frame| {
            window_edge_for(EdgeState {
                opaque: false,
                focused: true,
                boxed_in: false,
                own_frame,
            })
            .corners
        };
        assert_eq!(edge(false), ghost_renderer::Corners::default());
        assert_eq!(edge(true), ghost_renderer::Corners::ALL);
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn an_opaque_window_cuts_no_corners_at_all() {
        // An opaque surface's alpha never reaches the compositor, so cutting a
        // corner paints it black instead of clear.
        let edge = window_edge_for(EdgeState {
            opaque: true,
            focused: true,
            boxed_in: false,
            own_frame: true,
        });
        assert_eq!(edge.radius, 0.0);
        assert_eq!(edge.corners, ghost_renderer::Corners::NONE);
    }

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

    /// Poll `flag` until it reads `true` or ~2s elapse; returns its final state.
    fn flag_within(flag: &std::sync::Arc<std::sync::atomic::AtomicBool>, ms: u64) -> bool {
        let deadline = Instant::now() + std::time::Duration::from_millis(ms);
        while Instant::now() < deadline {
            if flag.load(std::sync::atomic::Ordering::Relaxed) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        flag.load(std::sync::atomic::Ordering::Relaxed)
    }

    #[test]
    fn a_briefly_queued_write_is_ordinary_backpressure() {
        // A paste bigger than the socket buffer queues for a frame or two and
        // drains. That is the transport working, not failing — it must not cost a
        // line in the trace, or the real thing drowns in noise.
        let t0 = Instant::now();
        let mut s = InputStall::default();
        assert!(s.observe(64 * 1024, t0).is_none());
        assert!(
            s.observe(8 * 1024, t0 + Duration::from_millis(20))
                .is_none()
        );
        assert!(
            s.observe(0, t0 + INPUT_STALL_GRACE - Duration::from_millis(1))
                .is_none(),
            "a queue that clears within the grace is never reported"
        );
    }

    #[test]
    fn an_input_queue_that_never_moves_is_called_wedged_once_and_reports_its_drain() {
        // The incident: 34 bytes of typing accepted, none written, for 24s. The
        // depth never falls, so there is no progress to wait for — say so at the
        // threshold, say it ONCE (the pump observes every 8ms), and account for
        // the whole wait when it finally clears.
        let t0 = Instant::now();
        let mut s = InputStall::default();
        assert!(s.observe(34, t0).is_none());
        assert!(s.observe(34, t0 + Duration::from_secs(1)).is_none());
        let ev = s.observe(34, t0 + INPUT_STALL_PROBE);
        assert!(
            matches!(ev, Some(StallEvent::Wedged { bytes: 34, waited }) if waited >= INPUT_STALL_PROBE),
            "no progress for the threshold is a wedged write path, got {ev:?}"
        );
        assert!(
            s.observe(34, t0 + Duration::from_secs(6)).is_none(),
            "a wedged queue is named once per episode, not once per pump"
        );
        let ev = s.observe(0, t0 + Duration::from_secs(24));
        assert!(
            matches!(ev, Some(StallEvent::Drained { bytes: 34, waited }) if waited >= Duration::from_secs(24)),
            "the drain reports the whole episode, got {ev:?}"
        );
        assert!(
            s.observe(0, t0 + Duration::from_secs(25)).is_none(),
            "the episode is over"
        );
    }

    #[test]
    fn an_input_queue_that_keeps_draining_is_never_called_wedged() {
        // A slow link (a big paste over a thin ssh hop) keeps making progress.
        // Progress restarts the clock, so a transport that is merely slow is
        // never mistaken for one that has stopped.
        let t0 = Instant::now();
        let mut s = InputStall::default();
        assert!(s.observe(90_000, t0).is_none());
        for (i, left) in [60_000usize, 30_000, 10_000].into_iter().enumerate() {
            let at = t0 + INPUT_STALL_PROBE * (i as u32 + 1) - Duration::from_millis(1);
            assert!(
                s.observe(left, at).is_none(),
                "still draining at {left} bytes is not wedged"
            );
        }
        assert!(matches!(
            s.observe(0, t0 + INPUT_STALL_PROBE * 4),
            Some(StallEvent::Drained { .. })
        ));
    }

    #[test]
    fn a_wedged_input_queue_names_itself_in_the_focus_trace() {
        // What the next incident should read like without any reconstruction:
        // one line when the write path stops taking bytes, one when it resumes.
        let log = with_isolated_xdg(|| {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("focus-trace.log");
            // SAFETY: the suite serializes every env-touching App test on the
            // lock `with_isolated_xdg` holds; the trace re-reads the var per event.
            unsafe { std::env::set_var("GHOST_FOCUS_TRACE", &path) };
            let mut app = App::headless();
            let t0 = Instant::now();
            app.note_input_queue("s1", 34, t0);
            app.note_input_queue("s1", 34, t0 + INPUT_STALL_PROBE);
            app.note_input_queue("s1", 0, t0 + Duration::from_secs(24));
            unsafe { std::env::remove_var("GHOST_FOCUS_TRACE") };
            std::fs::read_to_string(&path).unwrap_or_default()
        });
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 2, "one line each way, got: {log}");
        assert!(
            lines[0].contains("s1 input STALLED 34 bytes unwritten"),
            "the stall names the session and the backlog: {}",
            lines[0]
        );
        assert!(
            lines[1].contains("s1 input DRAINED 34 bytes after 24"),
            "the drain accounts for the whole wait: {}",
            lines[1]
        );
    }

    #[test]
    fn reading_the_config_does_not_trigger_a_reload() {
        // The reload path itself READS ui.toml, and on inotify a read raises an
        // Access event on the watched dir. If the watcher counted reads as
        // changes, one reload would re-arm the flag and the shell would reload
        // forever at event-loop frequency — each iteration wiping every cached
        // session surface (`Renderer::set_theme`), which blanked all terminal
        // content for the whole length of any dive/slide animation.
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("ui.toml");
        std::fs::write(&cfg, "[window]\npadding = 4.0\n").unwrap();
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watcher = super::config_watcher_in(tmp.path(), flag.clone());
        if watcher.is_none() {
            return; // no notify backend on this platform/runner: nothing to guard
        }
        // Give the watch a beat to bind, then do what a reload does: read the file.
        std::thread::sleep(std::time::Duration::from_millis(100));
        let _ = std::fs::read(&cfg).unwrap();
        assert!(
            !flag_within(&flag, 400),
            "a read of ui.toml must not count as a config change"
        );
        // A real edit still triggers (the sanity half that keeps this test honest).
        std::fs::write(&cfg, "[window]\npadding = 9.0\n").unwrap();
        assert!(
            flag_within(&flag, 2000),
            "an actual write to ui.toml must trigger the reload flag"
        );
    }

    #[test]
    fn reading_the_session_dir_does_not_trigger_reenumeration() {
        // Session re-enumeration READS the runtime dir (opendir/readdir raises
        // Access on inotify) and each session's meta files. Counting those reads
        // as set-changes turns one reconcile into a permanent self-sustaining
        // churn loop, re-listing sessions at event-loop frequency.
        let tmp = tempfile::tempdir().unwrap();
        let entry = tmp.path().join("some-session");
        std::fs::write(&entry, "x").unwrap();
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watcher = super::session_set_watcher_in(tmp.path(), flag.clone());
        if watcher.is_none() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        // What an enumeration does: list the dir, read an entry.
        let _ = std::fs::read_dir(tmp.path()).unwrap().count();
        let _ = std::fs::read(&entry).unwrap();
        assert!(
            !flag_within(&flag, 400),
            "reading the session dir must not count as a set change"
        );
        // A session appearing still triggers.
        std::fs::write(tmp.path().join("new-session"), "x").unwrap();
        assert!(
            flag_within(&flag, 2000),
            "a new entry in the session dir must trigger the flag"
        );
    }

    #[test]
    fn an_occluded_window_releases_no_repaints() {
        // While macOS reports a window occluded (another Space, minimized, the
        // lock screen) its surface cannot be acquired, so releasing repaints
        // just spins render→acquire-fail→retry. The per-pass decision must
        // park the window: the repaint stays pending, nothing is released, and
        // becoming visible again releases it at once.
        with_isolated_xdg(|| {
            let mut app = App::headless();
            let fe = HeadlessFrontend::new();
            let group = app.mint_group();
            let wid = app.open_fleet_window(&fe, group, None);
            let win = app.windows.get_mut(&wid).expect("window exists");
            win.presented_ok = true; // a live window that has presented before
            win.pacer.request(); // streaming output wants a repaint...
            win.occluded = true; // ...but the window is on another Space
            for t in 0..10u64 {
                assert!(
                    !win.release_repaint_due(t * 160),
                    "parked while occluded (t={t})"
                );
            }
            assert!(win.pacer.pending(), "the repaint stays pending, not lost");
            win.occluded = false; // WindowEvent::Occluded(false)
            assert!(
                win.release_repaint_due(1600),
                "the parked repaint releases as soon as the window is visible"
            );
        });
    }

    #[test]
    fn a_window_awaiting_its_opening_frame_retries_at_the_paced_cadence() {
        // Until the opening frame lands, the repaint must stay armed (macOS can
        // drop redraws while it finishes compositing a new window). But the
        // retries go out at the pacer's budget — the old every-pass retry
        // self-sustained: each failed redraw woke the loop, which requested
        // another redraw, ~5k acquire attempts/sec against an unacquirable
        // surface.
        with_isolated_xdg(|| {
            let mut app = App::headless();
            let fe = HeadlessFrontend::new();
            let group = app.mint_group();
            let wid = app.open_fleet_window(&fe, group, None);
            let win = app.windows.get_mut(&wid).expect("window exists");
            assert!(!win.presented_ok, "a fresh window has not presented yet");
            assert!(
                win.release_repaint_due(0),
                "the first bootstrap attempt is immediate"
            );
            assert!(
                !win.release_repaint_due(0),
                "the pass a failed redraw wakes must not re-release"
            );
            assert!(!win.release_repaint_due(8), "a sub-budget pass holds");
            assert!(
                win.release_repaint_due(16),
                "retries continue each budget until a frame lands"
            );
        });
    }

    #[test]
    fn an_occluded_never_presented_window_is_fully_parked() {
        // The compounding of the two cases above is the one that hurt: a window
        // that never presented AND is occluded (created on / moved to another
        // Space and left there) must do nothing at all — its bootstrap retries
        // would fail forever, each one burning CPU and leaking a wgpu texture
        // id. Overnight that was ~300M failed acquires and 14GB of heap.
        with_isolated_xdg(|| {
            let mut app = App::headless();
            let fe = HeadlessFrontend::new();
            let group = app.mint_group();
            let wid = app.open_fleet_window(&fe, group, None);
            let win = app.windows.get_mut(&wid).expect("window exists");
            win.occluded = true;
            for t in 0..100u64 {
                assert!(
                    !win.release_repaint_due(t * 16),
                    "no bootstrap retry while occluded (t={t})"
                );
            }
        });
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
            let default_theme = app.windows[&w1].root.theme(&app.states);

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
                    root.theme(&app.states),
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
    fn launching_for_an_ssh_window_opens_the_connect_prompt() {
        // `ghost --ssh-window` (the desktop entry's "New SSH Window" action) with no
        // ghost running: the first window it opens is the connect prompt.
        with_isolated_xdg(|| {
            let mut app = App::headless();
            app.startup = crate::Startup::Connect;
            let fe = HeadlessFrontend::new();
            assert!(app.open_startup_windows(&fe));
            let wid = *app.windows.keys().next().expect("a window opened");
            assert!(
                app.windows[&wid].root.is_connecting(),
                "the launch lands on the connect prompt"
            );
        });
    }

    #[test]
    fn a_forwarded_ssh_window_request_opens_the_connect_prompt() {
        // With a ghost already running, the action's launch forwards its request to
        // the owner instead — which must open a *connect* window, not a plain one.
        with_isolated_xdg(|| {
            let mut app = App::headless();
            let fe = HeadlessFrontend::new();
            let group = app.mint_group();
            let existing = app.open_fleet_window(&fe, group, None);

            app.on_user_event(&fe, UserEvent::OpenSshWindow);

            assert_eq!(app.windows.len(), 2, "the request opens a new window");
            let wid = *app
                .windows
                .keys()
                .find(|w| **w != existing)
                .expect("the new window");
            assert!(
                app.windows[&wid].root.is_connecting(),
                "the forwarded ssh request opens the connect prompt"
            );
            assert!(
                !app.windows[&existing].root.is_connecting(),
                "the window that was already open is left alone"
            );
        });
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
    fn a_remote_member_its_connected_host_no_longer_remembers_is_forgotten() {
        // Typing `exit` in a remote session discards its descriptor ON ITS HOST —
        // but the local sweep cannot read remote descriptors, so it used to treat
        // every not-listed member of a connected host as "lost to a reboot" and
        // offer a relaunch forever. The host's remembered-set (its descriptor
        // names, fetched over the transport) is what tells the two apart.
        with_isolated_xdg(|| {
            let mut app = App::headless();
            let composite = format!("kov@box{REMOTE_ID_SEP}work");
            app.groups = vec![ghost_ui_core::Group {
                id: "w1".into(),
                name: "blue".into(),
                color: 0,
                members: vec![composite.clone()],
                connection: None,
            }];
            // The host is connected and its listing does not name the member.
            app.remote_infos.insert("kov@box".to_string(), Vec::new());

            // Until the host's remembered-set is known (an older remote ghost, or
            // the fetch hasn't landed), stay conservative: relaunchable, as before.
            let dead = app.remembered_remotes();
            assert_eq!(
                dead.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
                vec![composite.as_str()],
                "with no remembered-set, a not-listed member stays relaunchable"
            );
            assert_eq!(dead[0].state, ghost_ui_core::DeadState::Exited);

            // The host reports it remembers nothing: the session exited cleanly
            // there (or was killed) — there is nothing to resurrect, so the
            // sweep must not name it and its membership goes.
            app.remote_remembered
                .insert("kov@box".to_string(), std::collections::HashSet::new());
            assert!(
                app.remembered_remotes().is_empty(),
                "a member its connected host no longer remembers must be forgotten"
            );

            // A host that still holds the descriptor (a reboot killed the host
            // uncleanly) is the case that stays relaunchable.
            app.remote_remembered.insert(
                "kov@box".to_string(),
                std::iter::once("work".to_string()).collect(),
            );
            let dead = app.remembered_remotes();
            assert_eq!(
                dead.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
                vec![composite.as_str()],
                "a member the host still remembers is relaunchable"
            );
            assert_eq!(dead[0].state, ghost_ui_core::DeadState::Exited);
        });
    }

    #[test]
    fn a_remembered_set_delivery_is_stashed_and_hints_a_reenumeration() {
        // The watcher thread fetches the host's descriptor names alongside each
        // listing push; the shell stashes them and re-sweeps, so a remote clean
        // exit drops its tile without any user action.
        let mut app = App::headless();
        let fe = HeadlessFrontend::new();
        app.on_user_event(
            &fe,
            UserEvent::RemoteRemembered {
                target: "kov@box".to_string(),
                names: Some(std::iter::once("work".to_string()).collect()),
            },
        );
        assert_eq!(
            app.remote_remembered.get("kov@box"),
            Some(&std::iter::once("work".to_string()).collect()),
            "the host's remembered-set is stashed"
        );
        assert!(
            app.sessions_changed
                .load(std::sync::atomic::Ordering::Relaxed),
            "a re-enumeration is hinted so the sweep re-judges remembered members"
        );

        // A failed fetch (older remote ghost, dropped transport) clears the
        // cache: unknown must never be judged by a stale set.
        app.on_user_event(
            &fe,
            UserEvent::RemoteRemembered {
                target: "kov@box".to_string(),
                names: None,
            },
        );
        assert!(
            !app.remote_remembered.contains_key("kov@box"),
            "an unknown remembered-set clears the cached one"
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
            app.remote_remembered
                .insert("kov@a".to_string(), std::collections::HashSet::new());
            app.remote_remembered
                .insert("kov@b".to_string(), std::collections::HashSet::new());

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
            assert!(app.remote_remembered.contains_key("kov@a"));
            assert!(
                !app.remote_remembered.contains_key("kov@b"),
                "its cached remembered-set is dropped too"
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
            let adopted = app.sessions.contains_key(&composite);

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
            // Stand in for the off-loop spawn worker: create the detached remote
            // session (shim → local) exactly as `spawn_host` does, then hand the
            // main-loop continuation the result the worker posts back.
            let remote = ghost_vt::remote::RemoteSsh::new(spec.clone()).unwrap();
            remote
                .spawn_host(ghost_bin.to_str().unwrap(), name)
                .unwrap();
            app.finish_remote_session_spawn(
                wid,
                "kov@box".to_string(),
                name.to_string(),
                Ok(()),
                &fe,
            );

            let composite = format!("kov@box{REMOTE_ID_SEP}{name}");
            let held = app.sessions.contains_key(&composite);

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

    /// Two windows in ONE process, one session: window A drives a real local session
    /// X; window B opens the fleet and previews X. Because X is driven *in this very
    /// process*, B must NOT open a redundant read-only mirror of it — it shares A's one
    /// emulator — yet its preview must still show X's live content. Today B opens a
    /// `Subscriber` and emulates X a second time (this test is red on the observer
    /// assertion); the process-wide state collapse makes B borrow A's shared state and
    /// preview it with no second stream. The content half blocks a cheat that merely
    /// stops observing without wiring the shared feed (B would preview nothing).
    #[test]
    fn a_previewing_window_shares_the_drivers_state_without_a_second_mirror() {
        let Some(ghost_bin) = ghost_binary() else {
            eprintln!("skipping: no `ghost` binary next to the test binary");
            return;
        };
        with_isolated_xdg(|| {
            let name = "share-1";
            // A real local session that prints a marker then holds open on `cat`.
            let ok = std::process::Command::new(&ghost_bin)
                .args([
                    "new",
                    name,
                    "-d",
                    "--",
                    "sh",
                    "-c",
                    "printf 'SHARED-MARKER\\n'; exec cat",
                ])
                .status()
                .expect("spawn `ghost new -d`")
                .success();
            assert!(ok, "`ghost new -d` succeeded");

            let listed = || {
                ghost_vt::session::list()
                    .unwrap_or_default()
                    .iter()
                    .any(|s| s.name == name)
            };
            let mut spun = 0;
            while !listed() && spun < 100 {
                std::thread::sleep(std::time::Duration::from_millis(20));
                spun += 1;
            }
            assert!(listed(), "the session came up");

            let mut app = App::headless();
            let fe = HeadlessFrontend::new();

            // Window A drives X (a real attach into a single view).
            let group_a = app.mint_group();
            let a = app
                .open_single_window(&fe, name, group_a, None)
                .expect("window A attaches the session");
            assert!(
                app.sessions.contains_key(name),
                "window A drives the session it attached"
            );

            // Window B opens the fleet and reconciles the live listing — it sees X
            // attached elsewhere (to A, same process) and previews it.
            let group_b = app.mint_group();
            let b = app.open_fleet_window(&fe, group_b, None);
            let list = ghost_vt::session::list().unwrap_or_default();
            app.dispatch(a, ghost_ui_core::UiEvent::SessionList(list.clone()), &fe);
            app.dispatch(b, ghost_ui_core::UiEvent::SessionList(list), &fe);

            // Pump the real per-wake pass until the shared state holds the marker (A's
            // attach resync + the feed have to travel the sockets). Post-collapse the one
            // emulator lives in `app.states`; B's tile borrows it — no second stream.
            let b_sees_marker = |app: &App| {
                app.states
                    .text_of(name)
                    .is_some_and(|rows| rows.iter().any(|l| l.contains("SHARED-MARKER")))
            };
            let mut spun = 0;
            while !b_sees_marker(&app) && spun < 100 {
                app.wake(&fe);
                std::thread::sleep(std::time::Duration::from_millis(20));
                spun += 1;
            }

            let observers_len = app.observers.len();
            let saw_marker = b_sees_marker(&app);

            // Tear the real host down before asserting so a failure never leaks it.
            let _ = ghost_vt::session::kill_session(name);

            assert!(
                saw_marker,
                "B's preview shows the driver's live content ({spun} wakes)"
            );
            assert_eq!(
                observers_len, 0,
                "B previews A's same-process session with NO second mirror"
            );
        });
    }

    /// Guard for the shared-state killer: A drives X, B previews it in its fleet
    /// (sharing A's one emulator). The host echoes a `Resized` to every subscriber
    /// whenever the display client resizes — so B, which merely previews an
    /// in-process-driven session, receives one. The per-window fleet arm used to
    /// rebuild the shared state on that echo, blanking the session A drives with no
    /// resync coming. It must not any more: only the shell rebuilds, and only for a
    /// genuine observer stream.
    #[test]
    fn an_echoed_resize_does_not_blank_a_session_a_second_window_drives() {
        let Some(ghost_bin) = ghost_binary() else {
            eprintln!("skipping: no `ghost` binary next to the test binary");
            return;
        };
        with_isolated_xdg(|| {
            let name = "share-resize";
            let ok = std::process::Command::new(&ghost_bin)
                .args([
                    "new",
                    name,
                    "-d",
                    "--",
                    "sh",
                    "-c",
                    "printf 'SHARED-MARKER\\n'; exec cat",
                ])
                .status()
                .expect("spawn `ghost new -d`")
                .success();
            assert!(ok, "`ghost new -d` succeeded");
            let listed = || {
                ghost_vt::session::list()
                    .unwrap_or_default()
                    .iter()
                    .any(|s| s.name == name)
            };
            let mut spun = 0;
            while !listed() && spun < 100 {
                std::thread::sleep(std::time::Duration::from_millis(20));
                spun += 1;
            }
            assert!(listed(), "the session came up");

            let mut app = App::headless();
            let fe = HeadlessFrontend::new();
            let ga = app.mint_group();
            let a = app
                .open_single_window(&fe, name, ga, None)
                .expect("window A attaches");
            let gb = app.mint_group();
            let b = app.open_fleet_window(&fe, gb, None);
            let list = ghost_vt::session::list().unwrap_or_default();
            app.dispatch(a, ghost_ui_core::UiEvent::SessionList(list.clone()), &fe);
            app.dispatch(b, ghost_ui_core::UiEvent::SessionList(list), &fe);
            let sees = |app: &App| {
                app.states
                    .text_of(name)
                    .is_some_and(|rows| rows.iter().any(|l| l.contains("SHARED-MARKER")))
            };
            let mut spun = 0;
            while !sees(&app) && spun < 100 {
                app.wake(&fe);
                std::thread::sleep(std::time::Duration::from_millis(20));
                spun += 1;
            }
            let precondition = sees(&app);

            // The host's Resized echo lands on B (the app-wide subscription fan). Its
            // fleet believes it observes X (it emitted an Observe the shell deduped),
            // so pre-fix this rebuilt and blanked the shared emulator.
            app.dispatch(
                b,
                ghost_ui_core::UiEvent::SessionPush {
                    name: name.to_string(),
                    push: ghost_ui_core::SessionPush::Event(
                        ghost_vt::protocol::SessionEvent::Resized { cols: 30, rows: 10 },
                    ),
                },
                &fe,
            );
            app.wake(&fe);
            let survived = sees(&app);
            let observers_len = app.observers.len();

            let _ = ghost_vt::session::kill_session(name);

            assert!(
                precondition,
                "precondition: the shared state holds the marker"
            );
            assert!(
                survived,
                "B's echoed Resized must not blank the session A drives"
            );
            assert_eq!(
                observers_len, 0,
                "still one shared source, no second mirror"
            );
        });
    }

    /// Guard for the driver-close handoff: A drives X, B previews it sharing A's one
    /// emulator (no mirror). When A closes, X keeps running under its host and B still
    /// previews it — so the shared state must NOT be deleted (last-viewer prune only
    /// drops what nothing views), A's now-orphaned client is dropped, and because B
    /// previews a now-driverless local session the shell downgrades the source to a
    /// read-only observer so the preview keeps updating.
    #[test]
    fn closing_the_driver_keeps_a_previewed_session_and_downgrades_to_an_observer() {
        let Some(ghost_bin) = ghost_binary() else {
            eprintln!("skipping: no `ghost` binary next to the test binary");
            return;
        };
        with_isolated_xdg(|| {
            let name = "share-close";
            let ok = std::process::Command::new(&ghost_bin)
                .args([
                    "new",
                    name,
                    "-d",
                    "--",
                    "sh",
                    "-c",
                    "printf 'SHARED-MARKER\\n'; exec cat",
                ])
                .status()
                .expect("spawn `ghost new -d`")
                .success();
            assert!(ok, "`ghost new -d` succeeded");
            let listed = || {
                ghost_vt::session::list()
                    .unwrap_or_default()
                    .iter()
                    .any(|s| s.name == name)
            };
            let mut spun = 0;
            while !listed() && spun < 100 {
                std::thread::sleep(std::time::Duration::from_millis(20));
                spun += 1;
            }
            assert!(listed(), "the session came up");

            let mut app = App::headless();
            let fe = HeadlessFrontend::new();
            let ga = app.mint_group();
            let a = app
                .open_single_window(&fe, name, ga, None)
                .expect("window A attaches");
            let gb = app.mint_group();
            let b = app.open_fleet_window(&fe, gb, None);
            let list = ghost_vt::session::list().unwrap_or_default();
            app.dispatch(a, ghost_ui_core::UiEvent::SessionList(list.clone()), &fe);
            app.dispatch(b, ghost_ui_core::UiEvent::SessionList(list), &fe);
            let sees = |app: &App| {
                app.states
                    .text_of(name)
                    .is_some_and(|rows| rows.iter().any(|l| l.contains("SHARED-MARKER")))
            };
            let mut spun = 0;
            while !sees(&app) && spun < 100 {
                app.wake(&fe);
                std::thread::sleep(std::time::Duration::from_millis(20));
                spun += 1;
            }
            assert!(
                sees(&app),
                "precondition: the shared state holds the marker"
            );
            assert_eq!(app.observers.len(), 0, "precondition: no second mirror");
            assert!(app.sessions.contains_key(name), "precondition: A drives it");

            // A closes. B still previews X in its fleet.
            app.close_window(a);

            let survived = sees(&app);
            let dropped_client = !app.sessions.contains_key(name);
            let downgraded = app.observers.contains_key(name);

            let _ = ghost_vt::session::kill_session(name);

            assert!(
                survived,
                "closing the driver must not delete a session another window previews"
            );
            assert!(
                dropped_client,
                "the driver's now-orphaned client is dropped (close = detach)"
            );
            assert!(
                downgraded,
                "the previewed driverless session is downgraded to a read-only observer"
            );
        });
    }

    /// The reverse handoff: a session PREVIEWED first (a read-only observer) then
    /// DRIVEN. Attaching a client is the observed→driven UPGRADE — it must REPLACE the
    /// observer, never coexist with it. Two live feed sources into the one shared
    /// emulator is the finding-#7 double-feed the per-wake pump asserts against
    /// (`observers ∩ sessions = ∅`); leaving the observer in place aborts the app on the
    /// next `wake` (the intermittent macOS restore/connect crash — whichever of the
    /// preview-observe and the driver-attach won the race decided whether it fired).
    #[test]
    fn driving_a_previewed_session_replaces_its_observer_no_double_source() {
        let Some(ghost_bin) = ghost_binary() else {
            eprintln!("skipping: no `ghost` binary next to the test binary");
            return;
        };
        with_isolated_xdg(|| {
            let name = "preview-then-drive";
            let ok = std::process::Command::new(&ghost_bin)
                .args([
                    "new",
                    name,
                    "-d",
                    "--",
                    "sh",
                    "-c",
                    "printf 'MARK\\n'; exec cat",
                ])
                .status()
                .expect("spawn `ghost new -d`")
                .success();
            assert!(ok, "`ghost new -d` succeeded");
            let listed = || {
                ghost_vt::session::list()
                    .unwrap_or_default()
                    .iter()
                    .any(|s| s.name == name)
            };
            let mut spun = 0;
            while !listed() && spun < 100 {
                std::thread::sleep(std::time::Duration::from_millis(20));
                spun += 1;
            }
            assert!(listed(), "the session came up");

            let mut app = App::headless();
            let fe = HeadlessFrontend::new();

            // B previews X first, while NOTHING drives it → opens a read-only observer.
            let gb = app.mint_group();
            let b = app.open_fleet_window(&fe, gb, None);
            let list = ghost_vt::session::list().unwrap_or_default();
            app.dispatch(b, ghost_ui_core::UiEvent::SessionList(list), &fe);
            let observed_first = app.observers.contains_key(name);
            let undriven_first = !app.sessions.contains_key(name);

            // A now drives X (open_single_window attaches a client). The upgrade must
            // drop B's observer so the session has exactly one source: A's client,
            // fanned to B's tile from the shared state.
            let ga = app.mint_group();
            let a = app.open_single_window(&fe, name, ga, None);
            let driven = app.sessions.contains_key(name);
            let both = app.observers.contains_key(name) && app.sessions.contains_key(name);

            let _ = ghost_vt::session::kill_session(name);

            assert!(observed_first, "precondition: B previews X as an observer");
            assert!(undriven_first, "precondition: nobody drives X yet");
            assert!(a.is_some(), "A attaches X");
            assert!(driven, "A drives X after the attach");
            assert!(
                !both,
                "driving a previewed session must drop its observer — a session both \
                 driven and observed is the finding-#7 double-feed that aborts on wake"
            );
        });
    }

    /// The REMOTE twin of the driver-close downgrade (5c item 2): window A drives a
    /// remote session (a composite `<host>␟<name>` id over the ssh transport) and window
    /// B previews it in its fleet, sharing A's one client (no second mirror). When A
    /// closes, the shared remote client is orphaned — but B still previews it, so the
    /// shell downgrades the source to a read-only REMOTE observer (over the host's
    /// transport, not a bogus local socket) and keeps the host connected so that
    /// observer's stream stays live. `reconcile_source` used to skip remote ids here
    /// entirely, freezing the preview with no self-heal.
    #[test]
    fn closing_the_driver_of_a_remote_preview_downgrades_it_to_a_remote_observer() {
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

            // Window A is an ssh group for the host; spawn a session on it and drive it.
            let ga = app.mint_group();
            let a = app.open_fleet_window(&fe, ga, None);
            app.windows
                .get_mut(&a)
                .unwrap()
                .root
                .set_group_connection(Some(spec.clone()));
            let name = "rp-1";
            let remote = ghost_vt::remote::RemoteSsh::new(spec.clone()).unwrap();
            remote
                .spawn_host(ghost_bin.to_str().unwrap(), name)
                .unwrap();
            app.finish_remote_session_spawn(
                a,
                "kov@box".to_string(),
                name.to_string(),
                Ok(()),
                &fe,
            );
            let composite = format!("kov@box{REMOTE_ID_SEP}{name}");
            assert!(
                app.sessions.contains_key(&composite),
                "precondition: A drives the remote session over the transport"
            );

            // Window B previews it: a fleet reconciling the host's listing (as the watcher
            // pushes it) mints a foreign tile and rides A's one client — deduped, no mirror.
            let gb = app.mint_group();
            let b = app.open_fleet_window(&fe, gb, None);
            let listed = namespace_remote_infos("kov@box", vec![info(name, true)]);
            app.dispatch(b, ghost_ui_core::UiEvent::SessionList(listed), &fe);
            assert!(
                app.windows[&b].root.views(&composite),
                "precondition: B previews the remote session"
            );
            assert_eq!(
                app.observers.len(),
                0,
                "precondition: B shares A's client — no second mirror"
            );

            // A closes. B still previews the now-driverless remote session.
            app.close_window(a);

            let downgraded = app.observers.contains_key(&composite);
            let host_kept = app
                .remotes
                .lock()
                .map(|m| m.contains_key("kov@box"))
                .unwrap_or(false);
            let state_alive = app.states.text_of(&composite).is_some();

            // Tear the real (shimmed-local) session down and restore PATH before asserting.
            let _ = ghost_vt::session::kill_session(name);
            // SAFETY: still within the lock.
            unsafe {
                match orig_path {
                    Some(p) => std::env::set_var("PATH", p),
                    None => std::env::remove_var("PATH"),
                }
            }

            assert!(
                downgraded,
                "a previewed driverless REMOTE session downgrades to a remote observer"
            );
            assert!(
                host_kept,
                "its host stays connected so the observer's transport keeps feeding"
            );
            assert!(
                state_alive,
                "the shared state survives the remote driver leaving"
            );
        });
    }

    /// The idle-preview seed (5c item 1): window A drives session X, which then goes
    /// IDLE holding content. LONG AFTER X's last output, window B opens its fleet and
    /// previews X. Under the shared registry B's tile borrows A's one live emulator,
    /// and the reconcile's frame refresh builds the tile's *preview frame* straight
    /// from that live state — so the overview shows X's content immediately, with NO
    /// further output byte. (A blank-until-next-byte preview was the fear under the old
    /// per-window states, where B's own mirror started empty and only filled on a feed.)
    /// The assertion reads the tile's cached preview FRAME, not the shared screen, and
    /// takes no `wake` after B's `SessionList` — so the content can only have come from
    /// the reconcile seeding off the shared state, never from a feed.
    #[test]
    fn a_fleet_opened_over_an_idle_shared_session_previews_it_without_new_output() {
        let Some(ghost_bin) = ghost_binary() else {
            eprintln!("skipping: no `ghost` binary next to the test binary");
            return;
        };
        with_isolated_xdg(|| {
            let name = "idle-seed";
            let ok = std::process::Command::new(&ghost_bin)
                .args([
                    "new",
                    name,
                    "-d",
                    "--",
                    "sh",
                    "-c",
                    "printf 'IDLE-MARKER\\n'; exec cat",
                ])
                .status()
                .expect("spawn `ghost new -d`")
                .success();
            assert!(ok, "`ghost new -d` succeeded");
            let listed = || {
                ghost_vt::session::list()
                    .unwrap_or_default()
                    .iter()
                    .any(|s| s.name == name)
            };
            let mut spun = 0;
            while !listed() && spun < 100 {
                std::thread::sleep(std::time::Duration::from_millis(20));
                spun += 1;
            }
            assert!(listed(), "the session came up");

            let mut app = App::headless();
            let fe = HeadlessFrontend::new();

            // Window A drives X and pumps until the marker lands; then X is idle (cat).
            let ga = app.mint_group();
            let _a = app
                .open_single_window(&fe, name, ga, None)
                .expect("window A attaches");
            let a_has_marker = |app: &App| {
                app.states
                    .text_of(name)
                    .is_some_and(|rows| rows.iter().any(|l| l.contains("IDLE-MARKER")))
            };
            let mut spun = 0;
            while !a_has_marker(&app) && spun < 100 {
                app.wake(&fe);
                std::thread::sleep(std::time::Duration::from_millis(20));
                spun += 1;
            }
            assert!(
                a_has_marker(&app),
                "precondition: A's shared state holds the idle marker"
            );

            // B opens its fleet AFTER the fact. `open_fleet_window` runs the initial
            // `ListSessions` → reconcile → frame refresh synchronously before it returns,
            // so the preview is built on the open with NO further event and NO wake — the
            // strongest statement of the seed: the content can only have come from the
            // reconcile reading the shared state, never from a feed.
            let gb = app.mint_group();
            let b = app.open_fleet_window(&fe, gb, None);

            let preview = app.windows[&b].root.tile_frame_text(name);
            let observers_len = app.observers.len();

            let _ = ghost_vt::session::kill_session(name);

            // A missing frame (`None`) is the real regression — it must panic as
            // "unbuilt", never read as "marker absent".
            let preview =
                preview.expect("B's fleet built a preview frame for the shared idle session");
            assert!(
                preview.iter().any(|l| l.contains("IDLE-MARKER")),
                "B's fleet preview shows the idle session's content immediately: {preview:?}"
            );
            assert_eq!(
                observers_len, 0,
                "B previews A's same-process session with NO second mirror"
            );
        });
    }

    /// The take-over handoff (5c item 3): window A drives session X; window B previews
    /// it in its fleet (sharing A's one emulator). B takes over X in-process — adopt in
    /// place, no second client. Only ONE window may own the grid (the driver that
    /// re-grids the shared child on resize/zoom), so A must relinquish drivership: after
    /// the take-over A no longer `drives` X, while B does. A KEEPS its live view of X —
    /// input and rendering are drivership-independent by the shipped contract (see
    /// `RootModel::drives`); only grid ownership moves. The shared emulator is never
    /// blanked. Without the handoff both windows keep X in `mine`, both believe they
    /// drive it, and their resizes fight over the child's size.
    #[test]
    fn a_take_over_hands_grid_ownership_over_and_the_old_window_lets_go() {
        let Some(ghost_bin) = ghost_binary() else {
            eprintln!("skipping: no `ghost` binary next to the test binary");
            return;
        };
        with_isolated_xdg(|| {
            let name = "handoff-1";
            let ok = std::process::Command::new(&ghost_bin)
                .args([
                    "new",
                    name,
                    "-d",
                    "--",
                    "sh",
                    "-c",
                    "printf 'HANDOFF-MARKER\\n'; exec cat",
                ])
                .status()
                .expect("spawn `ghost new -d`")
                .success();
            assert!(ok, "`ghost new -d` succeeded");
            let listed = || {
                ghost_vt::session::list()
                    .unwrap_or_default()
                    .iter()
                    .any(|s| s.name == name)
            };
            let mut spun = 0;
            while !listed() && spun < 100 {
                std::thread::sleep(std::time::Duration::from_millis(20));
                spun += 1;
            }
            assert!(listed(), "the session came up");

            let mut app = App::headless();
            let fe = HeadlessFrontend::new();

            // A drives X; B opens a fleet and previews it. B's tile exists before the
            // pump, so A's inbound resync fans to it (making the preview live/`fed`, so
            // the take-over adopts immediately rather than deferring a dive).
            let ga = app.mint_group();
            let a = app
                .open_single_window(&fe, name, ga, None)
                .expect("window A attaches");
            let gb = app.mint_group();
            let b = app.open_fleet_window(&fe, gb, None);
            let list = ghost_vt::session::list().unwrap_or_default();
            app.dispatch(b, ghost_ui_core::UiEvent::SessionList(list), &fe);
            let sees = |app: &App| {
                app.states
                    .text_of(name)
                    .is_some_and(|rows| rows.iter().any(|l| l.contains("HANDOFF-MARKER")))
            };
            let mut spun = 0;
            while !sees(&app) && spun < 100 {
                app.wake(&fe);
                std::thread::sleep(std::time::Duration::from_millis(20));
                spun += 1;
            }
            assert!(
                sees(&app),
                "precondition: the shared state holds the marker"
            );
            assert!(
                app.windows[&a].root.drives(name),
                "precondition: A drives X"
            );
            assert!(
                !app.windows[&b].root.drives(name),
                "precondition: B only previews X"
            );

            // B takes over X through the real UI flow. X is attached elsewhere (to A), so
            // its tile sits folded under the "attached elsewhere" band — reveal it and
            // re-reconcile so the tile joins the layout and B focuses it. Enter then raises
            // the take-over confirm; Space confirms. `run_pending` claims the tile — flips
            // it to ThisWindow, moving X into B's group — before the adopt, and the
            // adopt-in-place reuses the one shared client.
            use ghost_ui_core::{Key, KeyEventKind, Mods, NamedKey};
            app.windows
                .get_mut(&b)
                .unwrap()
                .root
                .set_show_elsewhere(true);
            let list = ghost_vt::session::list().unwrap_or_default();
            app.dispatch(b, ghost_ui_core::UiEvent::SessionList(list), &fe);
            let key = |k| ghost_ui_core::UiEvent::Key {
                key: k,
                mods: Mods::NONE,
                kind: KeyEventKind::Press,
                alts: None,
            };
            app.dispatch(b, key(Key::Named(NamedKey::Enter)), &fe);
            app.dispatch(b, key(Key::Named(NamedKey::Space)), &fe);

            let a_drives = app.windows[&a].root.drives(name);
            let b_drives = app.windows[&b].root.drives(name);
            let a_still_views = app.windows[&a].root.views(name);
            let survived = sees(&app);

            let _ = ghost_vt::session::kill_session(name);

            assert!(
                !a_drives,
                "the old window relinquishes grid ownership when another takes over"
            );
            assert!(b_drives, "the new window drives the taken-over session");
            assert!(
                a_still_views,
                "the old window keeps its live view — only grid ownership moved"
            );
            assert!(survived, "the take-over must not blank the shared emulator");
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
            // Stand in for the off-loop spawn worker (as above), then run the
            // main-loop continuation that indexes and attaches the new session.
            let remote = ghost_vt::remote::RemoteSsh::new(spec.clone()).unwrap();
            remote
                .spawn_host(ghost_bin.to_str().unwrap(), name)
                .unwrap();
            app.finish_remote_session_spawn(
                wid,
                "kov@box".to_string(),
                name.to_string(),
                Ok(()),
                &fe,
            );
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

            assert!(
                app.windows[&wid].root.is_fleet(),
                "the fleet window stays in the overview"
            );
            assert!(
                app.sessions.is_empty(),
                "its remote members are observed, not driven: {:?}",
                app.sessions.keys().collect::<Vec<_>>()
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
            let held = app.sessions.contains_key(&composite);
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
            let held_fg = app.sessions.contains_key(&fg);
            let held_bg = app.sessions.contains_key(&bg);
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
    fn startup_ignores_a_group_remembering_a_dead_local_session() {
        // A remembered dead LOCAL member is not something to return to: relaunching
        // it runs `$SHELL` with the old scrollback, which is what spawning would give
        // anyway, and the memory is permanent — every window that ever ran leaves an
        // auto-group behind, so counting it sent every launch into the fleet forever.
        // The fleet still shows the tile; it just doesn't hijack a launch.
        let remembered = [group("g1", &["gone"])];
        assert!(matches!(
            startup_choice(None, &[], &remembered),
            StartupChoice::Spawn
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
    fn startup_opens_the_fleet_for_a_remembered_remote_member_whose_host_is_away() {
        // A remote member nothing lists is waiting on its host, not dead: it comes
        // back with its state when the host returns, so the fleet — where the tile
        // holds and reconnects — is the right place to land.
        let away = [group("g1", &[&format!("kov@box{REMOTE_ID_SEP}work")])];
        assert!(matches!(
            startup_choice(None, &[], &away),
            StartupChoice::Fleet
        ));
        // ...but not once that host is connected and the session is listed and held:
        // then it is an ordinary attached-elsewhere session.
        let listed = [info(&format!("kov@box{REMOTE_ID_SEP}work"), true)];
        assert!(matches!(
            startup_choice(None, &listed, &away),
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
        // decision — the fleet when there is a session to return to, a fresh session
        // otherwise — and never attaches to one specific session.
        assert!(matches!(
            new_window_choice(&[info("a", false)], &[]),
            StartupChoice::Fleet
        ));
        assert!(matches!(new_window_choice(&[], &[]), StartupChoice::Spawn));
        assert!(matches!(
            new_window_choice(&[info("a", true)], &[]),
            StartupChoice::Spawn
        ));
        // An old window's remembered dead member must not turn every Alt-N into a
        // fleet: this is the regression `tests/shell.rs` reproduces end-to-end.
        assert!(matches!(
            new_window_choice(&[], &[group("g1", &["gone"])]),
            StartupChoice::Spawn
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
    fn glass_prefers_compositor_blur_and_frosts_only_where_it_cannot() {
        // Blur and frost are one intent — make the translucent background read as
        // glass — with two realizations, and they must never both apply: a real
        // backdrop blur already diffuses what's behind, so self-drawn frost on top
        // of it only muddies. Ask the compositor first; frost only where the answer
        // is no.
        assert_eq!(
            glass(true, true, 0.4),
            Glass {
                blur: true,
                frost: 0.0
            },
            "a blurring compositor makes frost redundant"
        );
        assert_eq!(
            glass(true, false, 0.4),
            Glass {
                blur: true,
                frost: 0.4
            },
            "no compositor blur — self-draw the glass at the configured density"
        );
        // Neither means anything behind an opaque window: there is no backdrop to
        // blur and nothing shows through to frost. The request rides the same
        // translucency gate as the window's alpha.
        assert_eq!(
            glass(false, true, 0.4),
            Glass {
                blur: false,
                frost: 0.0
            }
        );
        assert_eq!(
            glass(false, false, 0.4),
            Glass {
                blur: false,
                frost: 0.0
            }
        );
    }

    #[test]
    fn glass_conjures_no_frost_that_was_not_configured() {
        // Falling back to frost is a fallback, not an invention: a window with
        // `frost = 0` (the default) that lands on a blurless compositor stays plain
        // translucent rather than being frosted on its behalf.
        assert_eq!(
            glass(true, false, 0.0),
            Glass {
                blur: true,
                frost: 0.0
            }
        );
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
