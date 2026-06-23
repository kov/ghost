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

mod from_winit;

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ghost_renderer::{Gpu, Rendered, Renderer, Theme};
use ghost_ui_core::{
    CellMetrics, Cmd, PointPx, PointerButton, PointerPhase, RootModel, Scene, TerminalModel,
    UiEvent,
};
use ghost_vt::client::Session;
use ghost_vt::screen;
use ghost_vt::server::{self, SpawnOpts};
use ghost_vt::session;
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::ModifiersState;
use winit::window::{Window, WindowId};

const FIRA: &[u8] = include_bytes!("../../shaper/tests/assets/FiraCode-Regular.ttf");
const METRICS: CellMetrics = CellMetrics {
    advance: 9.0,
    line_height: 18.0,
};
const SIZE_PX: f32 = 15.0;
const COLS: u16 = 80;
const ROWS: u16 = 24;
const POLL: Duration = Duration::from_millis(8);

fn main() {
    // MUST be first: re-execs into the session host when invoked as one.
    server::run_host_if_invoked();

    if let Some(path) = std::env::var_os("GHOST_CAPTURE") {
        capture(PathBuf::from(path));
    } else {
        interactive();
    }
}

/// Grid cell count for a surface of `w`×`h` physical pixels at `scale` (cells
/// are the base metrics scaled by the device factor, matching the model).
fn grid_from_pixels(w: u32, h: u32, scale: f32) -> (u16, u16) {
    let advance = METRICS.advance * scale;
    let line_height = METRICS.line_height * scale;
    let cols = (w as f32 / advance).floor().max(1.0) as u16;
    let rows = (h as f32 / line_height).floor().max(1.0) as u16;
    (cols, rows)
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
    let file = std::fs::File::create(path).expect("create png");
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), img.width, img.height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().expect("png header");
    writer.write_image_data(&img.rgba).expect("png data");
}

/// Attach (deferred) to a named session and complete the handshake at
/// `cols`×`rows` — the first resize promotes us to the display client and
/// spawns the deferred child.
fn attach(name: &str, cols: u16, rows: u16) -> io::Result<Session> {
    let mut s = Session::attach_deferred(name)?;
    s.set_read_timeout(Some(Duration::from_millis(1)))?;
    s.resize(cols, rows)?;
    Ok(s)
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
    let mut model = TerminalModel::new(name.clone(), COLS, ROWS, METRICS);

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

    let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
    let scene = model.view();
    let img = Renderer::headless(Theme::default()).render_offscreen_scene(&scene, font, SIZE_PX);
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

fn interactive() {
    let name = match std::env::var("GHOST_SESSION") {
        Ok(n) => n, // attach to an existing session
        Err(_) => {
            let n = format!("ghost-ui-{}", std::process::id());
            spawn_session(&n, vec![]);
            n
        }
    };

    let event_loop = EventLoop::new().expect("event loop");
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App {
        gfx: None,
        sessions: HashMap::new(),
        root: None,
        name,
        mods: ModifiersState::empty(),
        pointer_pos: PointPx { x: 0.0, y: 0.0 },
        clipboard: None,
        start: Instant::now(),
        next_tick: None,
    };
    event_loop.run_app(&mut app).expect("run app");
}

/// Per-window GPU state, valid only once the window (and surface) exist.
struct Graphics {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    config: wgpu::SurfaceConfiguration,
    renderer: Renderer,
}

impl Graphics {
    fn new(event_loop: &ActiveEventLoop) -> Self {
        let size = PhysicalSize::new(
            u32::from(COLS) * METRICS.advance as u32,
            u32::from(ROWS) * METRICS.line_height as u32,
        );
        let attrs = Window::default_attributes()
            .with_title("ghost")
            .with_inner_size(size);
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        window.set_ime_allowed(true);

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
        // Prefer a non-sRGB format: our colors are already sRGB, so an sRGB
        // target would double-encode and wash them out.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let win = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: win.width.max(1),
            height: win.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&device, &config);

        let gpu = Gpu {
            device: device.clone(),
            queue,
        };
        let renderer = Renderer::new(gpu, Theme::default(), format);

        Graphics {
            window,
            surface,
            device,
            config,
            renderer,
        }
    }

    fn resize(&mut self, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        self.config.width = w;
        self.config.height = h;
        self.surface.configure(&self.device, &self.config);
    }

    /// Draw a scene into the surface. `scene.size_px` must equal the surface
    /// size, and `font_px` the glyph size the scene was laid out for (the model
    /// keeps both in sync via `UiEvent::Resize` and its render scale).
    fn render(&mut self, scene: &Scene, font_px: f32) {
        let frame_tex = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f)
            | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return;
            }
            _ => return,
        };
        let target = frame_tex
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
        self.renderer
            .render_scene_to_view(&target, scene, font, font_px);
        self.window.pre_present_notify();
        frame_tex.present();
    }
}

/// The thin imperative shell: owns the world, holds the pure model, and shuttles
/// `UiEvent`s in and `Cmd`s out.
struct App {
    gfx: Option<Graphics>,
    /// Attached sessions by id. One in single view; the whole fleet when the
    /// overview is open.
    sessions: HashMap<String, Session>,
    root: Option<RootModel>,
    name: String,
    mods: ModifiersState,
    /// Last pointer position in physical pixels (winit reports it only on move,
    /// so we cache it for button/wheel events).
    pointer_pos: PointPx,
    /// Lazily-opened system clipboard for copy/paste.
    clipboard: Option<arboard::Clipboard>,
    /// Start of the monotonic clock injected into the model via `Tick`.
    start: Instant,
    /// When the next scheduled `Tick` is due, if any.
    next_tick: Option<Instant>,
}

impl App {
    /// Feed an event to the model and execute the effects it returns.
    fn dispatch(&mut self, ev: UiEvent, event_loop: &ActiveEventLoop) {
        let cmds = match self.root.as_mut() {
            Some(r) => r.update(ev),
            None => return,
        };
        self.exec(cmds, event_loop);
    }

    fn exec(&mut self, cmds: Vec<Cmd>, event_loop: &ActiveEventLoop) {
        for cmd in cmds {
            match cmd {
                Cmd::SendInput { session, bytes } => {
                    if let Some(s) = self.sessions.get_mut(&session) {
                        let _ = s.send_input(&bytes);
                    }
                }
                Cmd::Resize {
                    session,
                    cols,
                    rows,
                } => {
                    if let Some(s) = self.sessions.get_mut(&session) {
                        let _ = s.resize(cols, rows);
                    }
                }
                Cmd::ReadClipboard => {
                    let text = self.read_clipboard();
                    self.dispatch(UiEvent::ClipboardText(text), event_loop);
                }
                Cmd::WriteClipboard(text) => self.write_clipboard(text),
                Cmd::ListSessions => {
                    let infos = session::list().unwrap_or_default();
                    self.dispatch(UiEvent::SessionList(infos), event_loop);
                }
                Cmd::Attach(id) => {
                    if !self.sessions.contains_key(&id)
                        && let Ok(s) = attach(&id, COLS, ROWS)
                    {
                        self.sessions.insert(id, s);
                    }
                }
                Cmd::Detach(id) => {
                    // Never drop the window's own session out from under us.
                    if id != self.name {
                        self.sessions.remove(&id);
                    }
                }
                Cmd::Spawn { name, command } => {
                    spawn_session(&name, command);
                    // Best-effort attach; a later reconcile re-attaches if it lost the race.
                    if let Ok(s) = attach(&name, COLS, ROWS) {
                        self.sessions.insert(name, s);
                    }
                }
                Cmd::Redraw => self.request_redraw(),
                Cmd::SetTitle(t) => {
                    if let Some(g) = &self.gfx {
                        g.window.set_title(&t);
                    }
                }
                Cmd::ScheduleTick { after_ms } => {
                    self.next_tick = Some(Instant::now() + Duration::from_millis(after_ms));
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

    fn request_redraw(&self) {
        if let Some(g) = &self.gfx {
            g.window.request_redraw();
        }
    }

    fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gfx.is_some() {
            return;
        }
        let gfx = Graphics::new(event_loop);
        let scale = gfx.window.scale_factor();
        let (cols, rows) = grid_from_pixels(gfx.config.width, gfx.config.height, scale as f32);
        match attach(&self.name, cols, rows) {
            Ok(session) => {
                self.sessions.insert(self.name.clone(), session);
            }
            Err(e) => {
                eprintln!("could not attach to session '{}': {e}", self.name);
                event_loop.exit();
                return;
            }
        }
        let model = TerminalModel::new(self.name.clone(), cols, rows, METRICS);
        self.root = Some(RootModel::single(
            model,
            METRICS,
            (gfx.config.width, gfx.config.height),
        ));
        let (w, h) = (gfx.config.width, gfx.config.height);
        self.gfx = Some(gfx);
        // Sync the model's viewport to the real surface size *and* device scale
        // before the first paint — this drives the NDC mapping, the scissor
        // clamp, and the cell metrics, and its `Cmd::Redraw` requests that paint.
        // (No earlier `request_redraw`: it would race a frame at the default 1x
        // scale against glyphs the renderer rasterizes at `SIZE_PX * scale`.)
        self.dispatch(
            UiEvent::Resize {
                w_px: w,
                h_px: h,
                scale,
            },
            event_loop,
        );
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + POLL));
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Pump every attached session (one in single view, the whole fleet in
        // overview), then route each session's output to the model by id.
        let mut pumped: Vec<(String, Vec<u8>, bool)> = Vec::new();
        for (name, s) in self.sessions.iter_mut() {
            let (bytes, ended) = pump(s, 32);
            if !bytes.is_empty() || ended {
                pumped.push((name.clone(), bytes, ended));
            }
        }
        for (name, bytes, ended) in pumped {
            // A dead session is dropped from the pump map regardless of whether
            // it is the window's own; app-exit is decided separately below via
            // RootModel::ended(). (Done before dispatch so a stale query-reply to
            // it is simply ignored.)
            if ended {
                self.sessions.remove(&name);
            }
            self.dispatch(UiEvent::SessionData { name, bytes, ended }, event_loop);
        }
        if let Some(t) = self.next_tick
            && Instant::now() >= t
        {
            self.next_tick = None;
            let now_ms = self.now_ms();
            self.dispatch(UiEvent::Tick { now_ms }, event_loop);
        }
        if self.root.as_ref().is_some_and(|r| r.ended()) {
            event_loop.exit();
            return;
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + POLL));
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.sessions.clear(); // drop every session connection (detach)
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(g) = self.gfx.as_mut() {
                    g.resize(size.width, size.height);
                }
                let scale = self.gfx.as_ref().map_or(1.0, |g| g.window.scale_factor());
                self.dispatch(
                    UiEvent::Resize {
                        w_px: size.width.max(1),
                        h_px: size.height.max(1),
                        scale,
                    },
                    event_loop,
                );
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                // The display's DPI changed (e.g. the window moved to another
                // monitor). Reconfigure the surface to the window's *actual* new
                // physical size and re-derive the grid at the new scale, so a
                // redraw arriving before the (usual) following Resized still
                // renders with matching metrics rather than the stale config size.
                let size = self.gfx.as_mut().map(|g| {
                    let s = g.window.inner_size();
                    g.resize(s.width, s.height);
                    (s.width, s.height)
                });
                if let Some((w, h)) = size {
                    self.dispatch(
                        UiEvent::Resize {
                            w_px: w.max(1),
                            h_px: h.max(1),
                            scale: scale_factor,
                        },
                        event_loop,
                    );
                }
            }
            WindowEvent::RedrawRequested => {
                if let (Some(g), Some(r)) = (self.gfx.as_mut(), self.root.as_ref()) {
                    let scene = r.view();
                    // Rasterize at the model's render scale (device × zoom) so
                    // glyph size matches the grid the scene was laid out for.
                    let font_px = SIZE_PX * r.render_scale();
                    // Keep the IME candidate window pinned to the text cursor.
                    if let Some(a) = r.ime_cursor_area() {
                        g.window.set_ime_cursor_area(
                            PhysicalPosition::new(a.x, a.y),
                            PhysicalSize::new(a.w, a.h),
                        );
                    }
                    g.render(&scene, font_px);
                }
            }
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::KeyboardInput { event, .. } => {
                let key = from_winit::key(&event.logical_key);
                let mods = from_winit::mods(self.mods);
                let pressed = event.state == ElementState::Pressed;
                self.dispatch(UiEvent::Key { key, mods, pressed }, event_loop);
            }
            WindowEvent::Ime(Ime::Commit(text)) => {
                self.dispatch(UiEvent::Text(text), event_loop);
            }
            WindowEvent::Ime(Ime::Preedit(text, _cursor)) => {
                // Track the in-progress composition so the model suppresses the
                // raw keystrokes driving it; an empty string ends it.
                self.dispatch(UiEvent::Preedit(text), event_loop);
            }
            WindowEvent::Ime(Ime::Disabled) => {
                // Composition aborted (focus lost, IME toggled off): clear it.
                self.dispatch(UiEvent::Preedit(String::new()), event_loop);
            }
            WindowEvent::Ime(Ime::Enabled) => {}
            WindowEvent::Focused(focused) => {
                self.dispatch(UiEvent::Focus(focused), event_loop);
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.pointer_pos = PointPx {
                    x: position.x,
                    y: position.y,
                };
                let mods = from_winit::mods(self.mods);
                self.dispatch(
                    UiEvent::Pointer {
                        phase: PointerPhase::Motion,
                        button: None,
                        pos: self.pointer_pos,
                        mods,
                        wheel_dy: 0.0,
                    },
                    event_loop,
                );
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(b) = map_button(button) {
                    let phase = if state == ElementState::Pressed {
                        PointerPhase::Press
                    } else {
                        PointerPhase::Release
                    };
                    let mods = from_winit::mods(self.mods);
                    self.dispatch(
                        UiEvent::Pointer {
                            phase,
                            button: Some(b),
                            pos: self.pointer_pos,
                            mods,
                            wheel_dy: 0.0,
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
                let mods = from_winit::mods(self.mods);
                self.dispatch(
                    UiEvent::Pointer {
                        phase: PointerPhase::Wheel,
                        button: None,
                        pos: self.pointer_pos,
                        mods,
                        wheel_dy: dy,
                    },
                    event_loop,
                );
            }
            _ => {}
        }
    }
}
