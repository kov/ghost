//! A swappable destination for a rendered frame.
//!
//! [`Target::Surface`] draws into a window's wgpu swapchain (vsync-presented — the
//! live app); [`Target::Offscreen`] draws into the renderer's own texture, sized to
//! the scene, with no window (headless tests and benchmarks). [`Target::render_frame`]
//! runs the *same* damage→draw glue against either, so the windowed and headless
//! paths can never drift — there is one definition of "produce one frame", used by
//! both `ghost-ui`'s `Graphics` and `ghost-ui-harness`.

use std::time::{Duration, Instant};

use ghost_render::Scene;
use ghost_shaper::FontRef;

use crate::{Damage, Renderer, SceneCache};

/// Where a frame is drawn. See the module docs.
pub enum Target {
    /// A window's swapchain surface.
    Surface(SurfaceTarget),
    /// The renderer's internal cached texture, sized to the scene. Always opaque, so
    /// it accepts banded partial redraws.
    Offscreen,
}

/// A configured window surface plus the handle state needed to reconfigure it when
/// the swapchain is lost. Holds its own `Device` clone (cheap, `Arc`-backed).
pub struct SurfaceTarget {
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    device: wgpu::Device,
    /// Opaque windows accept a partial (banded) redraw; a translucent one must always
    /// repaint in full — a band would blend with the preserved pixels, not replace
    /// them. Mirrors the compositor alpha decision the app makes at window creation.
    opaque: bool,
}

impl SurfaceTarget {
    /// Wrap an already-created, already-configured surface. `opaque` should be `false`
    /// for a translucent window (forces full redraws).
    pub fn new(
        surface: wgpu::Surface<'static>,
        config: wgpu::SurfaceConfiguration,
        device: wgpu::Device,
        opaque: bool,
    ) -> Self {
        Self {
            surface,
            config,
            device,
            opaque,
        }
    }

    /// Current surface size in physical pixels.
    pub fn size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    /// Reconfigure for a new physical size. A no-op on a zero dimension.
    pub fn resize(&mut self, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        self.config.width = w;
        self.config.height = h;
        self.surface.configure(&self.device, &self.config);
    }

    /// Acquire the next swapchain image's view, reconfiguring and returning `None` if
    /// it was lost/outdated (the caller should treat that as "nothing presented").
    fn acquire(&mut self) -> Option<(wgpu::SurfaceTexture, wgpu::TextureView)> {
        let frame_tex = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f)
            | wgpu::CurrentSurfaceTexture::Suboptimal(f) => f,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return None;
            }
            _ => return None,
        };
        let view = frame_tex
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        Some((frame_tex, view))
    }

    /// Stretch-blit the renderer's held resize snapshot onto the surface — immediate
    /// feedback during an interactive resize, skipping the relayout/re-raster. Returns
    /// `true` if it presented. `pre_present` runs just before the present.
    pub fn blit_snapshot(&mut self, renderer: &mut Renderer, pre_present: impl FnOnce()) -> bool {
        if !renderer.has_snapshot() {
            return false;
        }
        let Some((frame_tex, view)) = self.acquire() else {
            return false;
        };
        renderer.blit_snapshot_to_view(&view, self.config.width, self.config.height);
        pre_present();
        frame_tex.present();
        true
    }
}

impl Target {
    /// Whether banded partial redraws are allowed for this target.
    pub fn opaque(&self) -> bool {
        match self {
            Target::Surface(s) => s.opaque,
            Target::Offscreen => true,
        }
    }

    /// Damage-gate, draw `scene` into the target, and present. Returns
    /// `Some((build, present))` durations when a frame was presented — `build` is the
    /// scene build + submit, `present` the (vsync-blocking, surface-only) present — or
    /// `None` when nothing was drawn (an identical scene, or a lost surface).
    ///
    /// `pre_present` runs just before a surface present (the app's
    /// `window.pre_present_notify()`); it is ignored for an offscreen target.
    pub fn render_frame(
        &mut self,
        renderer: &mut Renderer,
        cache: &mut SceneCache,
        scene: &Scene,
        font: FontRef,
        font_px: f32,
        pre_present: impl FnOnce(),
    ) -> Option<(Duration, Duration)> {
        // Decide what to redraw vs the last presented frame: skip an identical scene,
        // redraw only the changed band (opaque targets), or repaint in full.
        let band = match cache.damage(scene, font_px) {
            Damage::None => return None,
            Damage::Full => None,
            Damage::Band(b) if self.opaque() => Some(b),
            Damage::Band(_) => None,
        };
        match self {
            Target::Offscreen => {
                let t = Instant::now();
                renderer.render_to_cached_target(scene, font, font_px, band);
                Some((t.elapsed(), Duration::ZERO))
            }
            Target::Surface(s) => {
                let Some((frame_tex, view)) = s.acquire() else {
                    // Accepted the scene above but couldn't present it; forget it so
                    // the next request fully redraws onto the reconfigured surface.
                    cache.invalidate();
                    return None;
                };
                let size = (s.config.width, s.config.height);
                let t_build = Instant::now();
                renderer.present_scene(&view, size, scene, font, font_px, band);
                let build = t_build.elapsed();
                let t_present = Instant::now();
                pre_present();
                frame_tex.present();
                Some((build, t_present.elapsed()))
            }
        }
    }
}
