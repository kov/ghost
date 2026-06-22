//! ghost's windowed GPU terminal frontend.
//!
//! A winit window backed by a wgpu surface that is a real ghost client: it
//! attaches to a session, streams the child's output into a local
//! `ghost_vt::screen::Screen`, draws it through `ghost-renderer`, and sends
//! keystrokes / resizes back — the same protocol ghost-gtk speaks, rendered by
//! our own GPU renderer instead of VTE.
//!
//! Modes:
//! - default: attach to `$GHOST_SESSION`, or spawn a fresh `$SHELL` session, and
//!   run it interactively in a window.
//! - `GHOST_CAPTURE=/path.png`: headless — spawn a session (a fixed banner, or
//!   `$GHOST_CMD`), drive it to completion, render the result offscreen, write a
//!   PNG, and exit. Deterministic verification with no display.

mod encode;
mod mouse;
mod session_view;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ghost_render::{CellMetrics, layout_frame};
use ghost_renderer::{Gpu, Rendered, Renderer, Theme};
use ghost_vt::screen;
use ghost_vt::server::{self, SpawnOpts};
use ghost_vt::session;
use session_view::SessionView;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState};
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

/// Grid cell count for a surface of `w`×`h` pixels at our cell metrics.
fn grid_from_pixels(w: u32, h: u32) -> (u16, u16) {
    let cols = (w as f32 / METRICS.advance).floor().max(1.0) as u16;
    let rows = (h as f32 / METRICS.line_height).floor().max(1.0) as u16;
    (cols, rows)
}

/// The 1-based cell under a pointer at physical pixel `(x, y)`.
fn point_to_cell(x: f64, y: f64) -> (u16, u16) {
    let col = (x / METRICS.advance as f64).floor().max(0.0) as u16 + 1;
    let row = (y / METRICS.line_height as f64).floor().max(0.0) as u16 + 1;
    (col, row)
}

fn map_button(b: MouseButton) -> Option<mouse::Button> {
    match b {
        MouseButton::Left => Some(mouse::Button::Left),
        MouseButton::Middle => Some(mouse::Button::Middle),
        MouseButton::Right => Some(mouse::Button::Right),
        _ => None,
    }
}

/// A frontend-handled key combo (Super+key, or Ctrl+Shift+key) we intercept
/// before encoding so it reaches the app, not the child.
enum Shortcut {
    Paste,
    Copy,
}

fn classify_shortcut(key: &Key, mods: ModifiersState) -> Option<Shortcut> {
    let combo = mods.super_key() || (mods.control_key() && mods.shift_key());
    if !combo {
        return None;
    }
    match key {
        Key::Character(s) if s.eq_ignore_ascii_case("v") => Some(Shortcut::Paste),
        Key::Character(s) if s.eq_ignore_ascii_case("c") => Some(Shortcut::Copy),
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

fn attach_retry(name: &str, cols: u16, rows: u16) -> SessionView {
    let start = Instant::now();
    loop {
        match SessionView::attach(name, cols, rows) {
            Ok(view) => return view,
            Err(e) => {
                if start.elapsed() > Duration::from_secs(5) {
                    panic!("could not attach to session '{name}': {e}");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

// ---- capture mode (headless) -------------------------------------------

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

    let mut view = attach_retry(&name, COLS, ROWS);

    // Optionally feed input first, to exercise the keystroke path (the child is
    // typically `cat`, which echoes it back through the PTY).
    if let Ok(feed) = std::env::var("GHOST_FEED") {
        view.send_input(feed.as_bytes()).ok();
    }

    // Drive until the child ends or output settles.
    let start = Instant::now();
    let mut last_change = Instant::now();
    loop {
        let p = view.drain(64).unwrap_or(session_view::Pumped {
            dirty: false,
            ended: true,
        });
        if p.dirty {
            last_change = Instant::now();
        }
        if p.ended {
            break;
        }
        let has_text = view.screen().text().iter().any(|l| !l.trim().is_empty());
        if has_text && last_change.elapsed() > Duration::from_millis(250) {
            break;
        }
        if start.elapsed() > Duration::from_secs(10) {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    eprintln!("--- captured screen ---");
    for line in view.screen().text() {
        let t = line.trim_end();
        if !t.is_empty() {
            eprintln!("{t}");
        }
    }

    let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
    let frame = layout_frame(view.screen().vt(), METRICS);
    let img = Renderer::headless(Theme::default()).render_offscreen(&frame, font, SIZE_PX);
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

fn spawn_shell(name: &str) {
    server::spawn(SpawnOpts {
        name: name.to_string(),
        command: vec![], // empty => $SHELL
        size: (COLS, ROWS),
        record: None,
        scrollback: screen::DEFAULT_SCROLLBACK,
        max_recording_bytes: None,
        start_on_attach: true,
    })
    .expect("spawn shell session");
}

fn interactive() {
    let name = match std::env::var("GHOST_SESSION") {
        Ok(n) => n, // attach to an existing session
        Err(_) => {
            let n = format!("ghost-ui-{}", std::process::id());
            spawn_shell(&n);
            n
        }
    };

    let event_loop = EventLoop::new().expect("event loop");
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App {
        gfx: None,
        view: None,
        mods: ModifiersState::empty(),
        name,
        cursor_cell: (1, 1),
        held: None,
        clipboard: None,
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

    fn render(&mut self, view: &SessionView) {
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
        let frame = layout_frame(view.screen().vt(), METRICS);
        self.renderer.render_to_view(
            &target,
            self.config.width,
            self.config.height,
            &frame,
            font,
            SIZE_PX,
        );
        self.window.pre_present_notify();
        frame_tex.present();
    }
}

struct App {
    gfx: Option<Graphics>,
    view: Option<SessionView>,
    mods: ModifiersState,
    name: String,
    /// Last 1-based cell the pointer was over (for clicks/wheel without a fresh move).
    cursor_cell: (u16, u16),
    /// The button currently held down, if any (distinguishes drag from hover).
    held: Option<mouse::Button>,
    /// Lazily-opened system clipboard for paste.
    clipboard: Option<arboard::Clipboard>,
}

impl App {
    /// Read the system clipboard and paste it into the session (best-effort —
    /// a clipboard that won't open or is empty is silently ignored).
    fn paste_from_clipboard(&mut self) {
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().ok();
        }
        if let (Some(cb), Some(v)) = (self.clipboard.as_mut(), self.view.as_mut())
            && let Ok(text) = cb.get_text()
        {
            let _ = v.paste(&text);
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gfx.is_some() {
            return;
        }
        let gfx = Graphics::new(event_loop);
        let (cols, rows) = grid_from_pixels(gfx.config.width, gfx.config.height);
        match SessionView::attach(&self.name, cols, rows) {
            Ok(view) => self.view = Some(view),
            Err(e) => {
                eprintln!("could not attach to session '{}': {e}", self.name);
                event_loop.exit();
                return;
            }
        }
        gfx.window.request_redraw();
        self.gfx = Some(gfx);
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + POLL));
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(view) = self.view.as_mut() {
            match view.drain(64) {
                Ok(p) => {
                    if p.dirty
                        && let Some(g) = &self.gfx
                    {
                        g.window.request_redraw();
                    }
                    if p.ended {
                        event_loop.exit();
                        return;
                    }
                }
                Err(_) => {
                    event_loop.exit();
                    return;
                }
            }
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + POLL));
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.view = None; // drop the session connection (detach)
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(g) = self.gfx.as_mut() {
                    g.resize(size.width, size.height);
                }
                let (cols, rows) = grid_from_pixels(size.width.max(1), size.height.max(1));
                if let Some(v) = self.view.as_mut() {
                    let _ = v.resize(cols, rows);
                }
                if let Some(g) = &self.gfx {
                    g.window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => {
                if let (Some(g), Some(v)) = (self.gfx.as_mut(), self.view.as_ref()) {
                    g.render(v);
                }
            }
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed {
                    return;
                }
                match classify_shortcut(&event.logical_key, self.mods) {
                    Some(Shortcut::Paste) => self.paste_from_clipboard(),
                    // Copy needs a selection model (not built yet); swallow it so
                    // Ctrl+Shift+C doesn't reach the shell as ^C (SIGINT).
                    Some(Shortcut::Copy) => {}
                    None => {
                        if let Some(v) = self.view.as_mut() {
                            let _ = v.key(&event.logical_key, self.mods);
                        }
                    }
                }
            }
            WindowEvent::Ime(Ime::Commit(text)) => {
                if let Some(v) = self.view.as_mut() {
                    let _ = v.send_input(text.as_bytes());
                }
            }
            WindowEvent::Focused(focused) => {
                if let Some(v) = self.view.as_mut() {
                    let _ = v.focus(focused);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let cell = point_to_cell(position.x, position.y);
                if cell != self.cursor_cell {
                    self.cursor_cell = cell;
                    let held = self.held;
                    if let Some(v) = self.view.as_mut() {
                        let _ = v.mouse(
                            mouse::Kind::Motion,
                            held,
                            held.is_some(),
                            cell.0,
                            cell.1,
                            self.mods,
                        );
                    }
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(b) = map_button(button) {
                    let pressed = state == ElementState::Pressed;
                    self.held = if pressed { Some(b) } else { None };
                    let (col, row) = self.cursor_cell;
                    let kind = if pressed {
                        mouse::Kind::Press
                    } else {
                        mouse::Kind::Release
                    };
                    let held = self.held.is_some();
                    if let Some(v) = self.view.as_mut() {
                        let _ = v.mouse(kind, Some(b), held, col, row, self.mods);
                    }
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64,
                    MouseScrollDelta::PixelDelta(p) => p.y,
                };
                if dy != 0.0 {
                    let b = if dy > 0.0 {
                        mouse::Button::WheelUp
                    } else {
                        mouse::Button::WheelDown
                    };
                    let (col, row) = self.cursor_cell;
                    let held = self.held.is_some();
                    if let Some(v) = self.view.as_mut() {
                        let _ = v.mouse(mouse::Kind::Press, Some(b), held, col, row, self.mods);
                    }
                }
            }
            _ => {}
        }
    }
}
