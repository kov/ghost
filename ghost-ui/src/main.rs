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
mod menu;
mod pacer;
mod resize;

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ghost_renderer::{Gpu, Rendered, Renderer, SceneCache, SurfaceTarget, Target};
use ghost_ui_core::{
    CellMetrics, Cmd, KeyEventKind, PointPx, PointerButton, PointerPhase, RootModel, Scene,
    TerminalModel, UiEvent,
};
use ghost_ui_harness::framestats;
use ghost_vt::client::Session;
use ghost_vt::screen;
use ghost_vt::server::{self, SpawnOpts};
use ghost_vt::session;
use menu::{MenuIntent, UserEvent};
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
    // `ghost` has no subcommand and falls through to the windowed UI below.
    if ghost_cli::run_subcommand() {
        return;
    }

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
        interactive();
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
fn grid_from_pixels(w: u32, h: u32, scale: f32) -> (u16, u16) {
    let advance = metrics().advance * scale;
    let line_height = metrics().line_height * scale;
    let cols = (w as f32 / advance).floor().max(1.0) as u16;
    let rows = (h as f32 / line_height).floor().max(1.0) as u16;
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
/// answers color queries with it after we detach (last-attached colors).
fn attach(name: &str, cols: u16, rows: u16) -> io::Result<Session> {
    let mut s = Session::attach_deferred(name)?;
    s.set_read_timeout(Some(Duration::from_millis(1)))?;
    s.resize(cols, rows)?;
    s.report_theme(session_theme())?;
    Ok(s)
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
        match attach(name, cols, rows) {
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

/// Drain up to `max` pending reads off a session, returning the accumulated
/// output and whether it ended.
fn pump(session: &mut Session, max: usize) -> (Vec<u8>, bool) {
    let mut bytes = Vec::new();
    for _ in 0..max {
        match session.pump() {
            Ok(p) => {
                let empty = p.output.is_empty();
                if !empty {
                    bytes.extend_from_slice(&p.output);
                }
                if p.ended {
                    return (bytes, true);
                }
                if empty {
                    break;
                }
            }
            Err(_) => return (bytes, true),
        }
    }
    (bytes, false)
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
    let name = format!("ghost-ui-cap-{}", std::process::id());
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
        record: None,
        scrollback: screen::DEFAULT_SCROLLBACK,
        max_recording_bytes: None,
        start_on_attach: true,
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
        let (bytes, ended) = pump(&mut session, 64);
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

fn spawn_session(name: &str, command: Vec<String>) {
    server::spawn(SpawnOpts {
        name: name.to_string(),
        command, // empty => $SHELL
        size: (COLS, ROWS),
        record: None,
        scrollback: screen::DEFAULT_SCROLLBACK,
        max_recording_bytes: None,
        start_on_attach: true,
    })
    .expect("spawn session");
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
/// open the fleet whenever any session is detached (so launching reconnects
/// instead of accumulating new sessions), and only spawn a fresh session when
/// there is nothing detached to return to.
fn startup_choice(requested: Option<String>, sessions: &[session::SessionInfo]) -> StartupChoice {
    match requested {
        Some(name) => StartupChoice::Attach(name),
        None if sessions.iter().any(|s| !s.attached) => StartupChoice::Fleet,
        None => StartupChoice::Spawn,
    }
}

/// The startup decision for a window opened at runtime via File > New Window / Cmd-N.
/// A new window "acts like the first one", but carries no `$GHOST_SESSION` request
/// (that is a launch-only override), so it always takes the plain-launch decision.
fn new_window_choice(sessions: &[session::SessionInfo]) -> StartupChoice {
    startup_choice(None, sessions)
}

fn interactive() {
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
    let initial_name = if harness.is_some() {
        None // open the fleet; the harness populates and dives it
    } else {
        let requested = std::env::var("GHOST_SESSION").ok();
        let sessions = session::list().unwrap_or_default();
        match startup_choice(requested, &sessions) {
            StartupChoice::Attach(name) => Some(name),
            StartupChoice::Fleet => None,
            StartupChoice::Spawn => {
                let n = format!("ghost-ui-{}", std::process::id());
                spawn_session(&n, vec![]);
                Some(n)
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
    #[cfg(target_os = "macos")]
    let proxy = event_loop.create_proxy();
    let mut app = App {
        windows: HashMap::new(),
        clipboard: None,
        start: Instant::now(),
        initial_name,
        next_session_seq: 0,
        bench: harness,
        focused: None,
        #[cfg(target_os = "macos")]
        proxy,
    };
    event_loop.run_app(&mut app).expect("run app");
}

/// Pick a surface alpha mode. Our pipeline emits premultiplied alpha, so for a
/// translucent window we want `PreMultiplied` (and `Inherit`/`Auto`, which defer
/// to a premultiplied compositor); `PostMultiplied` would expect straight alpha
/// and wash the colours, so it is never chosen. A capability list always has at
/// least one entry, and an opaque window just takes the first (usually Opaque).
fn choose_alpha_mode(
    modes: &[wgpu::CompositeAlphaMode],
    want_transparent: bool,
) -> wgpu::CompositeAlphaMode {
    use wgpu::CompositeAlphaMode::{Auto, Inherit, PreMultiplied};
    if want_transparent {
        for preferred in [PreMultiplied, Inherit, Auto] {
            if modes.contains(&preferred) {
                return preferred;
            }
        }
        eprintln!("ghost-ui: no premultiplied alpha mode; window will stay opaque");
    }
    modes[0]
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
    fn new(
        event_loop: &ActiveEventLoop,
        theme: ghost_renderer::Theme,
        option_as_meta: bool,
        cols: u16,
        rows: u16,
    ) -> Self {
        // Open sized to `cols`x`rows` cells at the base font. A LOGICAL size (not
        // physical) so winit scales it by the monitor DPI — the grid then works out to
        // exactly `cols`x`rows` at any scale (`grid_from_pixels` divides physical px by
        // cell·scale), which a physical size would only get right at 1x.
        let m = metrics();
        let size = LogicalSize::new(
            f64::from(cols) * f64::from(m.advance),
            f64::from(rows) * f64::from(m.line_height),
        );
        // Request a transparent window only when the theme is translucent, so an
        // opaque setup never pays the compositor's alpha-blending cost.
        let want_transparent = theme.bg_alpha < 1.0;
        // Bench mode measures the render path at a realistic size, so open maximized
        // (the small default grid would understate per-frame raster cost).
        let maximized = std::env::var_os("GHOST_BENCH").is_some();
        let attrs = Window::default_attributes()
            .with_title("ghost")
            .with_inner_size(size)
            .with_maximized(maximized)
            .with_transparent(want_transparent);
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
            alpha_mode: choose_alpha_mode(&caps.alpha_modes, want_transparent),
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

    /// Stretch-blit the renderer's held resize snapshot to the surface — immediate
    /// feedback during an interactive resize, without the relayout/re-raster of a
    /// full scene render. No-op if the renderer holds no snapshot.
    fn blit_snapshot(&mut self) {
        let Target::Surface(s) = &mut self.target else {
            return;
        };
        if s.blit_snapshot(&mut self.renderer, || self.window.pre_present_notify()) {
            // What's on screen is the stretched snapshot, not a model scene; keep the
            // scene cache invalid so the eventual crisp commit always redraws.
            self.scene_cache.invalidate();
        }
    }

    /// Draw a scene to the window. `scene.size_px` must equal the surface size, and
    /// `font_px` the glyph size the scene was laid out for (the model keeps both in
    /// sync via `UiEvent::Resize` and its render scale). Delegates the damage→draw→
    /// present glue to [`Target::render_frame`] — the same code the headless harness
    /// runs — and returns its `Some((build, present))` split (or `None` when nothing
    /// was drawn). [`FrameStats`](framestats::FrameStats) consumes the split.
    fn render(&mut self, scene: &Scene, font_px: f32) -> Option<(Duration, Duration)> {
        let split = self.target.render_frame(
            &mut self.renderer,
            &mut self.scene_cache,
            scene,
            self.fonts,
            font_px,
            || self.window.pre_present_notify(),
        );
        // Per-frame cache-efficiency line under `RUST_LOG=ghost::cache=trace`; free otherwise.
        self.renderer.emit_cache_trace();
        split
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

/// Per-window state: the GPU surface and pure model, plus the input bookkeeping
/// that is inherently per-window (focus modifiers, pointer position, click
/// detection, and the model's scheduled tick).
struct WindowState {
    gfx: Graphics,
    root: RootModel,
    /// This window's own session clients (the single-view session plus any fleet
    /// previews). Dropping the window drops these, which detaches every session
    /// it held — the "close = detach" default, with no shared-pool bookkeeping.
    sessions: HashMap<String, Session>,
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
    /// Defers the costly relayout/reflow during an interactive resize, stretching
    /// the last crisp frame in the meantime (see [`resize`]).
    resize: resize::ResizeCoalescer,
    /// Per-frame timing during animations, printed on dive end when
    /// `GHOST_FRAME_STATS` is set (see [`framestats`]). Inert otherwise.
    stats: framestats::FrameStats,
}

impl WindowState {
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
    /// How the first window starts, set at construction and consumed by the first
    /// `resumed`: `Some(name)` opens a single view attached to that session; `None`
    /// opens the fleet (chosen when detached sessions exist to reconnect to).
    initial_name: Option<String>,
    /// Per-process counter making spawned session names unique.
    next_session_seq: u64,
    /// Frame-pacing bench harness (`GHOST_BENCH=dive`/`slide`): scripts animations
    /// against the real render path and synthesises the session list. `None` in
    /// normal use.
    bench: Option<bench::Harness>,
    /// The window that last gained focus — the target for menu actions that act on
    /// "the current window" (New Session, Copy, Paste, Zoom, Toggle Fleet). Kept
    /// across focus loss; a stale id is filtered out at use (see `focused_window`).
    focused: Option<WindowId>,
    /// Proxy for posting native-menu selections into the event loop from AppKit's
    /// main thread. Held only to hand to [`menu::install`].
    #[cfg(target_os = "macos")]
    proxy: winit::event_loop::EventLoopProxy<UserEvent>,
}

impl App {
    /// Feed an event to window `wid`'s model and execute the effects it returns.
    fn dispatch(&mut self, wid: WindowId, ev: UiEvent, event_loop: &ActiveEventLoop) {
        let cmds = match self.windows.get_mut(&wid) {
            Some(w) => w.root.update(ev),
            None => return,
        };
        self.exec(wid, cmds, event_loop);
    }

    fn exec(&mut self, wid: WindowId, cmds: Vec<Cmd>, event_loop: &ActiveEventLoop) {
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
                    self.dispatch(wid, UiEvent::SessionList(infos), event_loop);
                }
                Cmd::Attach(id) => {
                    if let Some(w) = self.windows.get_mut(&wid)
                        && !w.sessions.contains_key(&id)
                    {
                        // Handshake at the window's real grid (see `attach_into`).
                        let (cols, rows) = w.root.grid();
                        if let Ok(s) = attach(&id, cols, rows) {
                            w.sessions.insert(id, s);
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
                Cmd::Kill(id) => {
                    // Kill the session and its process, then drop any client we held.
                    let _ = session::kill_session(&id);
                    if let Some(w) = self.windows.get_mut(&wid) {
                        w.sessions.remove(&id);
                    }
                }
                Cmd::Rename {
                    session: target,
                    name,
                } => {
                    // A control connection rename — works whether or not this
                    // window holds the session.
                    let _ = ghost_vt::client::rename(&target, &name);
                }
                Cmd::Spawn { name, command } => {
                    spawn_session(&name, command);
                    // Best-effort attach; a later reconcile re-attaches if it lost the race.
                    if let Some(w) = self.windows.get_mut(&wid) {
                        // Handshake at the window's real grid (see `attach_into`).
                        let (cols, rows) = w.root.grid();
                        if let Ok(s) = attach(&name, cols, rows) {
                            w.sessions.insert(name, s);
                        }
                    }
                }
                Cmd::NewWindow => self.open_launch_window(event_loop),
                Cmd::CloseWindow => {
                    self.close_window(wid);
                    if self.windows.is_empty() {
                        event_loop.exit();
                    }
                }
                Cmd::SpawnSession => {
                    let name = self.unique_session_name();
                    spawn_session(&name, vec![]);
                    if self.attach_into(wid, &name) {
                        self.dispatch(wid, UiEvent::AdoptSession(name), event_loop);
                    }
                }
                Cmd::TakeOver(id) => {
                    // Switch the window to `id`'s single view. Attach if we don't
                    // already hold it — stealing the display from another window for
                    // a confirmed take-over of a session attached elsewhere.
                    let held = self
                        .windows
                        .get(&wid)
                        .is_some_and(|w| w.sessions.contains_key(&id));
                    if held || self.attach_into(wid, &id) {
                        self.dispatch(wid, UiEvent::AdoptSession(id), event_loop);
                    }
                }
                Cmd::UploadImage {
                    id,
                    width,
                    height,
                    rgba,
                } => {
                    if let Some(w) = self.windows.get_mut(&wid) {
                        w.gfx.renderer.upload_image(id, width, height, &rgba);
                    }
                }
                Cmd::Redraw => {
                    // Don't paint inline — record the request and let the pacer
                    // release it within the frame budget (coalescing bursts).
                    if let Some(w) = self.windows.get_mut(&wid) {
                        w.pacer.request();
                    }
                }
                Cmd::SetTitle(t) => {
                    if let Some(w) = self.windows.get(&wid) {
                        w.gfx.window.set_title(&t);
                    }
                }
                Cmd::OpenUrl(url) => open_url(&url),
                Cmd::PointerIcon(icon) => {
                    if let Some(w) = self.windows.get(&wid) {
                        w.gfx.window.set_cursor(match icon {
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
                    }
                }
                Cmd::Quit => event_loop.exit(),
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

    /// Advance the bench harness one turn: fire the next scripted animation when the
    /// last has settled, or exit when the run is done. The single bench window's
    /// `is_animating` gates the script (so one only starts once the prior finishes);
    /// dispatched F9 / tile-selects / Ctrl-Tabs drive the real render+present path.
    fn drive_bench(&mut self, event_loop: &ActiveEventLoop) {
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
        format!("ghost-ui-{}-{}", std::process::id(), seq)
    }

    /// Attach window `wid`'s own client to `name` (no-op if it already holds one).
    /// Returns whether the window now has a client for it.
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
        match attach(name, cols, rows) {
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
        event_loop: &ActiveEventLoop,
    ) {
        let now_ms = self.now_ms();
        let step = {
            let Some(w) = self.windows.get_mut(&wid) else {
                return;
            };
            let step = w.resize.note(now_ms, w_px, h_px, scale);
            match step {
                // Isolated resize (maximize / snap / un-maximize / a drag's first
                // grab): drop any snapshot and resize the surface now; the real
                // relayout is dispatched below, crisply.
                resize::Step::CommitNow((cw, ch, _)) => {
                    w.gfx.renderer.clear_snapshot();
                    w.gfx.resize(cw, ch);
                }
                // A drag is streaming: capture the last crisp frame once, then
                // stretch-blit it cheaply until the gesture settles (the real
                // resize is committed from `about_to_wait`).
                resize::Step::Defer => {
                    if !w.gfx.renderer.has_snapshot() {
                        let scene = w.root.view();
                        let font_px = size_px() * w.root.render_scale();
                        w.gfx
                            .renderer
                            .capture_snapshot(&scene, w.gfx.fonts, font_px);
                    }
                    w.gfx.resize(w_px, h_px);
                    w.gfx.blit_snapshot();
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

    /// Open a new window in the fleet overview (owning no session yet). The user
    /// spawns or takes over a session from there.
    fn open_fleet_window(&mut self, event_loop: &ActiveEventLoop) {
        let cfg = config::UiConfig::load();
        let gfx = Graphics::new(
            event_loop,
            cfg.theme(),
            cfg.option_as_meta(),
            cfg.columns(),
            cfg.rows(),
        );
        let wid = gfx.window.id();
        let scale = gfx.window.scale_factor();
        let (w, h) = gfx.size();
        let (mut root, init) = RootModel::fleet(metrics(), (w, h), scale as f32);
        root.set_theme(theme_colors(&cfg.theme()));
        apply_anim_ms(&mut root);
        self.windows.insert(
            wid,
            WindowState {
                gfx,
                root,
                sessions: HashMap::new(),
                mods: ModifiersState::empty(),
                pointer_pos: PointPx { x: 0.0, y: 0.0 },
                next_tick: None,
                last_click: None,
                click_count: 0,
                pacer: pacer::FramePacer::new(pacer::FRAME_BUDGET_MS),
                resize: resize::ResizeCoalescer::new(
                    resize::SETTLE_MS,
                    resize::MAX_MS,
                    resize::DRAG_GAP_MS,
                ),
                stats: framestats::FrameStats::from_env(),
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
    }

    /// Open a new window that behaves exactly like a fresh launch (File > New Window
    /// / Cmd-N): reconnect through the fleet when any session is detached, otherwise
    /// spawn a fresh session and show it as a single view. Runs in this same process,
    /// so the new window shares the clipboard, clock, and menu with the others.
    fn open_launch_window(&mut self, event_loop: &ActiveEventLoop) {
        let sessions = session::list().unwrap_or_default();
        match new_window_choice(&sessions) {
            StartupChoice::Fleet => self.open_fleet_window(event_loop),
            StartupChoice::Spawn => {
                let name = self.unique_session_name();
                spawn_session(&name, vec![]);
                self.open_single_window(event_loop, &name);
            }
            // new_window_choice never asks to attach a specific session, but keep the
            // match exhaustive: an explicit name would open that session's single view.
            StartupChoice::Attach(name) => {
                self.open_single_window(event_loop, &name);
            }
        }
    }

    /// Remove a window; dropping its [`WindowState`] drops its session clients,
    /// which detaches them (the hosts keep the sessions running for reattach) —
    /// the "close = detach" default.
    fn close_window(&mut self, wid: WindowId) {
        self.windows.remove(&wid);
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
            && let Some(w) = self.windows.get(&ids[next])
        {
            w.gfx.window.focus_window();
        }
    }
}

impl App {
    /// Open the first window as a single-session view attached to `name`. Returns
    /// false if the attach fails (the caller exits the app).
    fn open_single_window(&mut self, event_loop: &ActiveEventLoop, name: &str) -> bool {
        let cfg = config::UiConfig::load();
        let gfx = Graphics::new(
            event_loop,
            cfg.theme(),
            cfg.option_as_meta(),
            cfg.columns(),
            cfg.rows(),
        );
        let wid = gfx.window.id();
        let scale = gfx.window.scale_factor();
        let (w, h) = gfx.size();
        let (cols, rows) = grid_from_pixels(w, h, scale as f32);
        let session = match attach(name, cols, rows) {
            Ok(session) => session,
            Err(e) => {
                eprintln!("could not attach to session '{name}': {e}");
                return false;
            }
        };
        let model = TerminalModel::new(name.to_string(), cols, rows, metrics());
        let mut root = RootModel::single(model, metrics(), (w, h));
        root.set_theme(theme_colors(&cfg.theme()));
        apply_anim_ms(&mut root);
        let mut sessions = HashMap::new();
        sessions.insert(name.to_string(), session);
        self.windows.insert(
            wid,
            WindowState {
                gfx,
                root,
                sessions,
                mods: ModifiersState::empty(),
                pointer_pos: PointPx { x: 0.0, y: 0.0 },
                next_tick: None,
                last_click: None,
                click_count: 0,
                pacer: pacer::FramePacer::new(pacer::FRAME_BUDGET_MS),
                resize: resize::ResizeCoalescer::new(
                    resize::SETTLE_MS,
                    resize::MAX_MS,
                    resize::DRAG_GAP_MS,
                ),
                stats: framestats::FrameStats::from_env(),
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
        true
    }
}

impl ApplicationHandler<UserEvent> for App {
    /// A native menu selection posted from AppKit's main thread. Each action is
    /// turned back into the effect a keystroke would have produced, so the pure
    /// core stays the single source of truth (see [`menu::menu_intent`]).
    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        let UserEvent::Menu(action) = event;
        match menu::menu_intent(action) {
            // Opening a window needs no focused target — it always works.
            MenuIntent::NewWindow => self.open_launch_window(event_loop),
            MenuIntent::FocusedCmd(cmd) => {
                if let Some(wid) = self.focused_window() {
                    self.exec(wid, vec![cmd], event_loop);
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
                        event_loop,
                    );
                }
            }
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if !self.windows.is_empty() {
            return;
        }
        // `Some(name)` → single view of that session; `None` → fleet (chosen at
        // launch when there were detached sessions to reconnect to).
        match self.initial_name.take() {
            None => self.open_fleet_window(event_loop),
            Some(name) => {
                if !self.open_single_window(event_loop, &name) {
                    event_loop.exit();
                    return;
                }
            }
        }
        // Install the native macOS menu bar once the app is running (it appends
        // ghost's File / Edit / View / Window submenus to the App submenu winit
        // set up in applicationDidFinishLaunching).
        #[cfg(target_os = "macos")]
        menu::install(self.proxy.clone());
        // Bench mode: populate the fleet and load every preview before any animation.
        if self.bench.is_some()
            && let Some(wid) = self.windows.keys().next().copied()
        {
            for ev in self.bench.as_ref().expect("bench present").setup_events() {
                self.dispatch(wid, ev, event_loop);
            }
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + POLL));
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Pump each window's own session clients and route the output back to
        // that window (a window only holds clients for sessions it's showing).
        let mut pumped: Vec<(WindowId, String, Vec<u8>, bool)> = Vec::new();
        for (wid, w) in self.windows.iter_mut() {
            let mut dead = Vec::new();
            for (name, s) in w.sessions.iter_mut() {
                let (bytes, ended) = pump(s, 32);
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
            self.dispatch(wid, UiEvent::SessionData { name, bytes, ended }, event_loop);
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
            if let Some(w) = self.windows.get_mut(&wid) {
                w.next_tick = None;
            }
            let now_ms = self.now_ms();
            self.dispatch(wid, UiEvent::Tick { now_ms }, event_loop);
        }
        // Bench mode: advance the scripted animation (after ticks, so `is_animating`
        // reflects this turn's animation state).
        if self.bench.is_some() {
            self.drive_bench(event_loop);
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
            if let Some(w) = self.windows.get_mut(&wid) {
                w.gfx.renderer.clear_snapshot();
            }
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
        // Release any paced repaint that the frame budget now allows. The loop
        // re-enters here every `POLL` (8 ms < the 16 ms budget), so a deferred
        // paint is always re-checked and fires within a frame of becoming due;
        // a keystroke's repaint, handled in this same pass, paints at once.
        for w in self.windows.values_mut() {
            if w.pacer.poll(now_ms) == pacer::Pace::PaintNow {
                w.gfx.window.request_redraw();
                w.pacer.painted(now_ms);
            }
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + POLL));
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                // Close = detach: dropping the window drops its session clients
                // (the hosts keep the sessions running). Exit with the last one.
                self.close_window(id);
                if self.windows.is_empty() {
                    event_loop.exit();
                }
            }
            WindowEvent::Resized(size) => {
                // Defer the costly relayout: capture + stretch-blit now, commit the
                // real resize once the drag settles (see `resize_step`).
                let Some(scale) = self.windows.get(&id).map(|w| w.gfx.window.scale_factor()) else {
                    return;
                };
                self.resize_step(id, size.width.max(1), size.height.max(1), scale, event_loop);
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                // The display's DPI changed (e.g. the window moved to another
                // monitor). Treat it like a resize step against the window's actual
                // new physical size, deferring the re-grid at the new scale.
                let Some(s) = self.windows.get(&id).map(|w| w.gfx.window.inner_size()) else {
                    return;
                };
                self.resize_step(
                    id,
                    s.width.max(1),
                    s.height.max(1),
                    scale_factor,
                    event_loop,
                );
            }
            WindowEvent::RedrawRequested => {
                if let Some(win) = self.windows.get_mut(&id) {
                    if win.gfx.renderer.has_snapshot() {
                        // A resize is in flight: blit the snapshot to the current
                        // surface rather than render a scene whose size no longer
                        // matches it (the model resize is deferred until settle).
                        win.gfx.blit_snapshot();
                    } else {
                        let t_model = Instant::now();
                        let scene = win.root.view();
                        let model = t_model.elapsed();
                        // During a dive/slide, DEFER session surface rasters off the frame
                        // loop: a mid-animation tile that needs a full raster blits the best
                        // cached surface as a placeholder and is warmed one-per-frame below,
                        // so the animation never stalls on a slow session's raster.
                        let animating = win.root.is_animating();
                        win.gfx.renderer.set_deferring(animating);
                        // Rasterize at the model's render scale (device × zoom) so
                        // glyph size matches the grid the scene was laid out for.
                        let font_px = size_px() * win.root.render_scale();
                        // Keep the IME candidate window pinned to the text cursor.
                        if let Some(a) = win.root.ime_cursor_area() {
                            win.gfx.window.set_ime_cursor_area(
                                PhysicalPosition::new(a.x, a.y),
                                PhysicalSize::new(a.w, a.h),
                            );
                        }
                        if let Some((build, present)) = win.gfx.render(&scene, font_px) {
                            // The foreground was just composited: reset its per-session
                            // damage baseline so the next `view` measures change from here
                            // (a skipped/no-op frame returns `None` and leaves the pending
                            // damage to fold into the next real present). See
                            // `RootModel::mark_presented`.
                            win.root.mark_presented();
                            // Model-side cache line (fleet preview frames) under
                            // `RUST_LOG=ghost::cache=trace`, alongside the renderer's.
                            win.root.emit_cache_trace();
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
                                event_loop.exit();
                            }
                        }
                        // Warm ONE deferred surface off the just-finished frame's slack, so
                        // the fleet fills in over the animation's frames without any single
                        // frame rasterizing a heavy session inline. The animation's own
                        // ticks drive the redraws that keep draining this.
                        if animating {
                            win.gfx.renderer.warm_next(win.gfx.fonts);
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
                    event_loop,
                );
            }
            WindowEvent::Ime(Ime::Commit(text)) => {
                self.dispatch(id, UiEvent::Text(text), event_loop);
            }
            WindowEvent::Ime(Ime::Preedit(text, _cursor)) => {
                // Track the in-progress composition so the model suppresses the
                // raw keystrokes driving it; an empty string ends it.
                self.dispatch(id, UiEvent::Preedit(text), event_loop);
            }
            WindowEvent::Ime(Ime::Disabled) => {
                // Composition aborted (focus lost, IME toggled off): clear it.
                self.dispatch(id, UiEvent::Preedit(String::new()), event_loop);
            }
            WindowEvent::Ime(Ime::Enabled) => {}
            WindowEvent::Focused(focused) => {
                // Remember the last-focused window as the target for menu actions;
                // keep the previous one on blur (a stale id is filtered at use).
                if focused {
                    self.focused = Some(id);
                }
                self.dispatch(id, UiEvent::Focus(focused), event_loop);
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
                    event_loop,
                );
            }
            WindowEvent::MouseInput { state, button, .. } => {
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
                        event_loop,
                    );
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
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
                    event_loop,
                );
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        StartupChoice, choose_alpha_mode, choose_surface_format, home_launch_dir,
        new_window_choice, startup_choice,
    };
    use ghost_vt::session::SessionInfo;
    use wgpu::CompositeAlphaMode::{Opaque, PostMultiplied, PreMultiplied};
    use wgpu::TextureFormat::{
        Bgra8Unorm, Bgra8UnormSrgb, Rgb10a2Unorm, Rgba8Unorm, Rgba8UnormSrgb, Rgba16Float,
    };

    fn info(name: &str, attached: bool) -> SessionInfo {
        SessionInfo {
            name: name.to_string(),
            pid: 1,
            created_at: None,
            title: String::new(),
            command: Vec::new(),
            attached,
            bell: false,
        }
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
            startup_choice(Some("x".into()), &sessions),
            StartupChoice::Attach(n) if n == "x"
        ));
    }

    #[test]
    fn startup_opens_the_fleet_when_any_session_is_detached() {
        let sessions = [info("a", true), info("b", false)];
        assert!(matches!(
            startup_choice(None, &sessions),
            StartupChoice::Fleet
        ));
    }

    #[test]
    fn startup_spawns_when_nothing_is_detached() {
        // No sessions at all, or only sessions attached elsewhere → fresh session.
        assert!(matches!(startup_choice(None, &[]), StartupChoice::Spawn));
        let attached_elsewhere = [info("a", true)];
        assert!(matches!(
            startup_choice(None, &attached_elsewhere),
            StartupChoice::Spawn
        ));
    }

    #[test]
    fn new_window_mirrors_a_plain_launch() {
        // File > New Window / Cmd-N opens a window that "acts like the first one":
        // it carries no `$GHOST_SESSION` request, so it always takes the plain-launch
        // decision — the fleet when anything is detached (reconnect), a fresh session
        // otherwise — and never attaches to one specific session.
        assert!(matches!(
            new_window_choice(&[info("a", false)]),
            StartupChoice::Fleet
        ));
        assert!(matches!(new_window_choice(&[]), StartupChoice::Spawn));
        assert!(matches!(
            new_window_choice(&[info("a", true)]),
            StartupChoice::Spawn
        ));
    }

    #[test]
    fn alpha_mode_prefers_premultiplied_when_transparent() {
        // The compositor offers premultiplied: take it.
        assert_eq!(
            choose_alpha_mode(&[Opaque, PreMultiplied], true),
            PreMultiplied
        );
        // Only straight (post) alpha is offered — it would wash our premultiplied
        // output, so we decline and stay opaque (the first mode) instead.
        assert_eq!(choose_alpha_mode(&[Opaque, PostMultiplied], true), Opaque);
        // An opaque window ignores transparency entirely.
        assert_eq!(choose_alpha_mode(&[Opaque, PreMultiplied], false), Opaque);
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
