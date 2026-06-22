//! ghost's windowed GPU terminal frontend.
//!
//! A winit window backed by a wgpu surface, drawing a `ghost-vt` Screen through
//! the same [`ghost_renderer::Renderer`] used by the offscreen golden tests —
//! so what the window shows is exactly what the tests verify.
//!
//! Set `GHOST_CAPTURE=/path.png` to render one frame, read the surface texture
//! back, write it to a PNG, and exit. This gives a verifiable image of what the
//! window draws without depending on a compositor screenshot tool — useful in
//! headless/CI environments.
//!
//! This is a static demo for now (ligatures, ANSI/256 colors, attributes, the
//! block cursor); wiring keyboard input and a live PTY/session is the next step.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ghost_render::{CellMetrics, layout_frame};
use ghost_renderer::{Gpu, Renderer, Theme};
use ghost_vt::screen::Screen;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

const FIRA: &[u8] = include_bytes!("../../shaper/tests/assets/FiraCode-Regular.ttf");
const METRICS: CellMetrics = CellMetrics {
    advance: 9.0,
    line_height: 18.0,
};
const SIZE_PX: f32 = 15.0;
const COLS: u16 = 80;
const ROWS: u16 = 24;

/// Per-window GPU state, valid only once the window (and surface) exist.
struct Graphics {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    can_capture: bool,
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
        // Prefer a non-sRGB format: our color values are already sRGB, so writing
        // them to an sRGB target would double-encode and wash the colors out.
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|f| !f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let can_capture = caps.usages.contains(wgpu::TextureUsages::COPY_SRC);
        let usage = if can_capture {
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC
        } else {
            wgpu::TextureUsages::RENDER_ATTACHMENT
        };
        let win = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage,
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
            queue: queue.clone(),
        };
        let renderer = Renderer::new(gpu, Theme::default(), format);

        Graphics {
            window,
            surface,
            device,
            queue,
            config,
            can_capture,
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

    /// Read the just-rendered surface texture back and write it as a PNG.
    fn capture(&self, texture: &wgpu::Texture, path: &Path) {
        let (w, h) = (self.config.width, self.config.height);
        let unpadded = w * 4;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded = unpadded.div_ceil(align) * align;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("capture"),
            size: u64::from(padded) * u64::from(h),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit([encoder.finish()]);

        let (tx, rx) = std::sync::mpsc::channel();
        buffer.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        rx.recv().expect("map channel").expect("map failed");

        let bgra = matches!(
            self.config.format,
            wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
        );
        let view = buffer.slice(..).get_mapped_range();
        let mut rgba = Vec::with_capacity((unpadded * h) as usize);
        for row in 0..h {
            let start = (row * padded) as usize;
            let line = &view[start..start + unpadded as usize];
            if bgra {
                for px in line.chunks_exact(4) {
                    rgba.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
                }
            } else {
                rgba.extend_from_slice(line);
            }
        }
        drop(view);
        buffer.unmap();

        let file = std::fs::File::create(path).expect("create png");
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().expect("png header");
        writer.write_image_data(&rgba).expect("png data");
        eprintln!("captured surface to {}", path.display());
    }
}

struct App {
    gfx: Option<Graphics>,
    screen: Screen,
    capture: Option<PathBuf>,
}

impl App {
    fn new(capture: Option<PathBuf>) -> Self {
        let mut screen = Screen::new(COLS, ROWS, 1000);
        screen.feed(b"\x1b[1mghost\x1b[0m \xe2\x80\x94 our own GPU terminal renderer\r\n\r\n");
        screen.feed(b"ligatures:  if a != b && c == d => run(x);  // -> <= >= |> .. ===\r\n");
        screen.feed(
            b"colors:     \x1b[31mred \x1b[32mgreen \x1b[33myellow \x1b[34mblue \x1b[35mmagenta \x1b[36mcyan\x1b[0m\r\n",
        );
        screen.feed(
            b"256-color:  \x1b[38;5;208morange\x1b[0m \x1b[38;5;141mviolet\x1b[0m \x1b[48;5;22m on-green \x1b[0m \x1b[48;5;52m on-maroon \x1b[0m\r\n",
        );
        screen.feed(
            b"attributes: \x1b[1mbold\x1b[0m \x1b[2mfaint\x1b[0m \x1b[7minverse\x1b[0m  \x1b[1;34mbold-blue\x1b[0m\r\n",
        );
        screen.feed(b"\r\n$ ");
        App {
            gfx: None,
            screen,
            capture,
        }
    }

    fn draw(&mut self, event_loop: &ActiveEventLoop) {
        if self.gfx.is_none() {
            return;
        }
        // Lay out the live grid before borrowing the GPU state mutably.
        let frame = layout_frame(self.screen.vt(), METRICS);
        let capture = self.capture.clone();
        let gfx = self.gfx.as_mut().unwrap();

        let surface_tex = match gfx.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f)
            | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                gfx.surface.configure(&gfx.device, &gfx.config);
                return;
            }
            other => {
                eprintln!("surface frame unavailable: {other:?}");
                return;
            }
        };
        let view = surface_tex
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
        gfx.renderer.render_to_view(
            &view,
            gfx.config.width,
            gfx.config.height,
            &frame,
            font,
            SIZE_PX,
        );

        if let Some(path) = capture {
            if gfx.can_capture {
                gfx.capture(&surface_tex.texture, &path);
            } else {
                eprintln!("surface does not support COPY_SRC; cannot capture");
            }
            surface_tex.present();
            event_loop.exit();
            return;
        }

        gfx.window.pre_present_notify();
        surface_tex.present();
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gfx.is_some() {
            return;
        }
        let gfx = Graphics::new(event_loop);
        gfx.window.request_redraw();
        self.gfx = Some(gfx);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(gfx) = self.gfx.as_mut() {
                    gfx.resize(size.width, size.height);
                    gfx.window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested => self.draw(event_loop),
            _ => {}
        }
    }
}

fn main() {
    let capture = std::env::var_os("GHOST_CAPTURE").map(PathBuf::from);
    let event_loop = EventLoop::new().expect("event loop");
    event_loop.set_control_flow(ControlFlow::Wait);
    event_loop.run_app(&mut App::new(capture)).expect("run app");
}
