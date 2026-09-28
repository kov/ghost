//! **Windowed** harness test: drives a real F9 dive against a real window
//! swapchain surface, exercising the acquire→`present_scene`→present path that the
//! offscreen tests never touch. Same `Harness` driver as the headless tests — only
//! the render target is swapped for a `Target::Surface`.
//!
//! It brings its own compositor: a headless synoik on a private socket, started
//! and reaped by the test, so a plain `cargo test` runs it — no environment to
//! set, and no window on anyone's desktop. It used to be opt-in behind
//! `GHOST_UI_WINDOWED=1`, which meant nothing ran it and it rotted quietly
//! against a model change. The compositor is ours: `ghost-test-compositor`
//! clones and builds the pinned synoik rather than using whatever is installed,
//! so nothing about this test moves when the developer upgrades their desktop.
//!
//! Presenting needs dmabuf, and headless synoik only advertises it off a real
//! DRM render node — on lavapipe there is no dmabuf global at all, the client
//! falls back to shm and no Vulkan adapter is compatible with the surface. That
//! is an environment without a GPU, not a regression, so it is a skip too.
//!
//! It costs ~12s under headless synoik where it cost ~0.5s under headless
//! weston, for the same 13 presented frames, and the reason is upstream and
//! understood: synoik's headless backend never marks a surface as being on a
//! primary scanout output, so frame callbacks arrive only from its 995ms
//! overdue timer and Fifo waits that out on every present. A fix is in their
//! hands; the test measures the same thing either way, just slowly.
//!
//! To watch it instead, against your own session:
//!
//! ```sh
//! GHOST_UI_WINDOWED=1 cargo test -p ghost-ui-harness --test windowed -- --nocapture
//! ```
//!
//! Linux-only: it uses the Wayland `EventLoopBuilderExtWayland` and runs the
//! event loop off the main thread (`with_any_thread`), which winit only permits
//! on Linux — on macOS the loop must own the main thread, so this whole target is
//! compiled out there (keeping `cargo test --all-targets` green on macOS).
#![cfg(target_os = "linux")]

use std::sync::Arc;

use ghost_render::CellMetrics;
use ghost_renderer::{Gpu, Renderer, SurfaceTarget, Target, Theme};
use ghost_test_compositor::Synoik;
use ghost_ui_core::{Key, KeyEventKind, Mods, NamedKey, UiEvent};
use ghost_ui_harness::Harness;
use ghost_vt::session::SessionInfo;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::platform::wayland::EventLoopBuilderExtWayland;
use winit::window::{Window, WindowId};

/// The virtual output the compositor serves, in device pixels — bigger than the
/// window this test asks for, so nothing tiles or clamps it.
const MONITOR: (u32, u32) = (1400, 900);

const METRICS: CellMetrics = CellMetrics {
    advance: 9.0,
    line_height: 18.0,
};

fn info(name: &str, attached: bool, created_at: i64) -> SessionInfo {
    SessionInfo {
        name: name.to_string(),
        pid: created_at as i32,
        created_at: Some(created_at),
        title: String::new(),
        command: Vec::new(),
        attached,
        bell: false,
        display_name: String::new(),
        cwd: None,
        size: None,
        connection: None,
        holder: None,
        group: None,
    }
}

fn f9() -> UiEvent {
    UiEvent::Key {
        key: Key::Named(NamedKey::F9),
        mods: Mods::NONE,
        kind: KeyEventKind::Press,
        alts: None,
    }
}

/// Small dense block so each preview rasterises real content (not the focus here).
fn dense() -> Vec<u8> {
    let mut s = String::new();
    for row in 0..40 {
        for col in 0..120 {
            s.push(char::from(b'!' + ((row * 7 + col * 3) % 90) as u8));
        }
        s.push_str("\r\n");
    }
    s.into_bytes()
}

/// Pick a non-sRGB UNORM format the way the app does.
fn choose_format(formats: &[wgpu::TextureFormat]) -> wgpu::TextureFormat {
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

#[derive(Default)]
struct WindowedDive {
    done: bool,
    /// Set when there was no GPU to present through, so the test can say so
    /// rather than report a pass it never earned.
    no_gpu: bool,
}

impl ApplicationHandler for WindowedDive {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.done {
            return;
        }
        self.done = true;

        // --- real window + swapchain surface + device (the app's setup) ----------
        let window = Arc::new(
            event_loop
                .create_window(
                    Window::default_attributes()
                        .with_title("ghost-ui-harness windowed dive")
                        .with_inner_size(PhysicalSize::new(1000, 700)),
                )
                .expect("create window"),
        );
        let size = window.inner_size();
        let scale = window.scale_factor();

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = instance
            .create_surface(window.clone())
            .expect("create surface");
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
        }));
        // No adapter can be compatible with a surface the compositor has no
        // dmabuf for — a machine without a real GPU, which is not something this
        // test has anything to say about.
        let Ok(adapter) = adapter else {
            self.no_gpu = true;
            event_loop.exit();
            return;
        };
        let adapter_info = adapter.get_info();
        eprintln!(
            "windowed adapter: {} / {} ({:?})",
            adapter_info.name, adapter_info.driver, adapter_info.device_type
        );
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .expect("request device");
        let caps = surface.get_capabilities(&adapter);
        let format = choose_format(&caps.formats);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
        };
        surface.configure(&device, &config);

        // The renderer MUST share the surface's device — hand it to the harness along
        // with the surface target.
        let renderer = Renderer::new(
            Gpu {
                device: device.clone(),
                queue,
            },
            Theme::default(),
            format,
        );
        let target = Target::Surface(SurfaceTarget::new(surface, config, device, true));

        // --- the harness, at the window's size, driving the real shell ------------
        let names = ["edit", "build", "logs", "prod"];
        let mut h = Harness::fleet(METRICS, (size.width, size.height), scale as f32);
        let win = window.clone();
        h.set_surface(renderer, target, move |_, _| win.pre_present_notify());
        // Detached, but for the last: adopting a session held by another window
        // without taking it over would double-attach it, which the fleet's extract
        // guard refuses — so the ones this test opens come from the detached pool,
        // as in the headless `f9_round_trips_single_to_fleet_to_single`. The odd one
        // out stays attached elsewhere, keeping the grid's foreign band — a distinct
        // tile treatment — in the frames being presented.
        h.set_sessions(
            names
                .iter()
                .enumerate()
                .map(|(i, n)| info(n, i == names.len() - 1, i as i64 + 1))
                .collect(),
        );
        for n in &names[..2] {
            h.inject(UiEvent::AdoptSession((*n).into()));
            h.inject(UiEvent::SessionData {
                name: (*n).into(),
                bytes: dense(),
                ended: false,
            });
        }
        h.inject(UiEvent::AdoptSession(names[0].into())); // land in the single view

        // Drive a dive to completion against the surface, counting presented frames.
        let mut clock = 0u64;
        let drive = |h: &mut Harness, clock: &mut u64| -> usize {
            let mut presented = 0;
            for _ in 0..600 {
                if !h.is_animating() {
                    break;
                }
                if h.present().is_some() {
                    presented += 1;
                }
                *clock += 16;
                h.advance(*clock);
            }
            presented
        };

        drive(&mut h, &mut clock); // settle the foregrounding dive
        h.inject(f9()); // dive OUT to the fleet
        let presented = drive(&mut h, &mut clock);

        assert!(
            presented > 1,
            "the dive presented several real frames to the surface, not a snap ({presented})"
        );
        assert!(!h.is_animating(), "the dive settled");
        eprintln!("windowed dive: presented {presented} frames to the swapchain");

        // The dive's goal (real frames presented to the surface) is verified above.
        // Ask winit to exit the loop so `run_app` returns and the test tears down.
        // (Mesa < 26.1.3 SIGSEGV'd this teardown on venus inside libtest — the
        // `vn_tls_free` TSD-destructor bug; fixed upstream, workaround removed.)
        event_loop.exit();
    }

    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, _: WindowEvent) {}
}

#[test]
fn windowed_dive_presents_frames_to_a_real_surface() {
    // Normally the test supplies its own compositor. `GHOST_UI_WINDOWED=1` runs it
    // against the ambient session instead, so the dive can be watched happening.
    let watching = std::env::var("GHOST_UI_WINDOWED").is_ok();
    let _compositor = match watching {
        true => None,
        false => match Synoik::start(MONITOR, 1.0) {
            Ok(c) => {
                // SAFETY: winit and wgpu read these from the environment, and
                // there is no other way to aim them at our socket. Sound here
                // because this is the only test in the binary and it has not
                // started a thread yet, so nothing else can be reading the
                // environment concurrently.
                unsafe {
                    std::env::set_var("WAYLAND_DISPLAY", c.display());
                }
                Some(c)
            }
            Err(why) => {
                eprintln!("skipping windowed test: {why}");
                return;
            }
        },
    };
    if std::env::var("WAYLAND_DISPLAY").is_err() {
        eprintln!("skipping windowed test (no WAYLAND_DISPLAY)");
        return;
    }
    // winit normally requires the event loop on the main thread; `with_any_thread`
    // lets `cargo test`'s worker thread create it (Wayland/X11 only).
    let event_loop = EventLoop::builder()
        .with_any_thread(true)
        .build()
        .expect("event loop");
    let mut app = WindowedDive::default();
    event_loop.run_app(&mut app).expect("run app");
    if app.no_gpu {
        eprintln!(
            "skipping windowed test: no surface-compatible adapter — headless \
             synoik advertises dmabuf only off a real DRM render node"
        );
    }
}
