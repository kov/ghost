//! GPU renderer (wgpu) for `ghost-render` frames.
//!
//! A persistent [`Renderer`] owns the device, pipeline, glyph atlas and glyph
//! cache, and can draw a laid-out [`Frame`] either to an offscreen target (for
//! deterministic, windowless golden tests on a software adapter) or into a
//! window surface view. Cell backgrounds, the block cursor, and full ANSI/256
//! color resolution are handled here; glyph shaping (with ligatures) comes from
//! `ghost-shaper`.

use std::collections::HashMap;

use ghost_render::{
    BadgeKind, CellMetrics, CursorShape, Frame, Layer, RectPx, Run, Scene, SceneItem, Selection,
    Style,
};
use ghost_shaper::{FontRef, Synthesis};
use ghost_term::Color;
use unicode_width::UnicodeWidthChar;
use wgpu::util::DeviceExt;

/// An RGBA8 image read back from the GPU, tightly packed (`width * 4` per row).
pub struct Rendered {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// A GPU context: device + queue. Build it headless ([`Gpu::headless`], a
/// software adapter for reproducible tests) or wrap a windowed device directly.
pub struct Gpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

impl Gpu {
    /// Acquire a headless context, preferring a software fallback adapter so CI
    /// output is reproducible regardless of the host's real GPU.
    ///
    /// Under the crate's own unit tests this hands out clones of a single shared
    /// device: the software (lavapipe) adapter SIGSEGVs when many devices are live
    /// at once, which the default parallel test runner would otherwise trigger.
    /// `wgpu::Device`/`Queue` are `Arc`-backed and thread-safe, so sharing one is
    /// both safe and cheaper than one device per test.
    pub fn headless() -> Self {
        #[cfg(test)]
        {
            shared_test_gpu()
        }
        #[cfg(not(test))]
        {
            Self::request_headless()
        }
    }

    /// Build a fresh headless device + queue on the software fallback adapter.
    fn request_headless() -> Self {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = wgpu::Backends::VULKAN;
        let instance = wgpu::Instance::new(desc);
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            force_fallback_adapter: true,
            compatible_surface: None,
        }))
        .expect("no GPU adapter (lavapipe/llvmpipe expected)");
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
                .expect("request device");
        Gpu { device, queue }
    }
}

/// One process-wide headless device shared by the crate's unit tests (see
/// [`Gpu::headless`]). Built once on first use and never dropped.
#[cfg(test)]
fn shared_test_gpu() -> Gpu {
    use std::sync::OnceLock;
    static SHARED: OnceLock<(wgpu::Device, wgpu::Queue)> = OnceLock::new();
    let (device, queue) = SHARED.get_or_init(|| {
        let gpu = Gpu::request_headless();
        (gpu.device, gpu.queue)
    });
    Gpu {
        device: device.clone(),
        queue: queue.clone(),
    }
}

/// The offscreen color format. Plain (non-sRGB) so byte values are exactly the
/// colors we wrote — important for deterministic golden comparisons.
pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

fn offscreen_target(
    device: &wgpu::Device,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("ghost-renderer offscreen"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

/// Copy an RGBA8 texture back into a tightly packed CPU buffer.
fn read_back(gpu: &Gpu, texture: &wgpu::Texture, width: u32, height: u32) -> Vec<u8> {
    let unpadded = width * 4;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded = unpadded.div_ceil(align) * align;

    let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: u64::from(padded) * u64::from(height),
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let mut encoder = gpu
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
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    gpu.queue.submit([encoder.finish()]);

    let (tx, rx) = std::sync::mpsc::channel();
    buffer.slice(..).map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    gpu.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    rx.recv().expect("map channel").expect("map failed");

    let view = buffer.slice(..).get_mapped_range();
    let mut rgba = Vec::with_capacity((unpadded * height) as usize);
    for row in 0..height {
        let start = (row * padded) as usize;
        rgba.extend_from_slice(&view[start..start + unpadded as usize]);
    }
    drop(view);
    buffer.unmap();
    rgba
}

/// Clear an offscreen target to a solid color and read it back — a smoke test
/// of the headless device + render + readback path.
pub fn render_solid(width: u32, height: u32, color: [f64; 4]) -> Rendered {
    let gpu = Gpu::headless();
    let texture = offscreen_target(&gpu.device, width, height, FORMAT);
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("clear"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: &view,
            resolve_target: None,
            depth_slice: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color {
                    r: color[0],
                    g: color[1],
                    b: color[2],
                    a: color[3],
                }),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    });
    gpu.queue.submit([encoder.finish()]);

    let rgba = read_back(&gpu, &texture, width, height);
    Rendered {
        width,
        height,
        rgba,
    }
}

// ---- color resolution ---------------------------------------------------

/// Renderer color theme: the RGB used for cells whose pen leaves fg/bg unset.
#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    /// Selection highlight tint, drawn translucently over cell backgrounds.
    pub selection: [u8; 3],
    /// The 16 base ANSI colors (indices 0..=15). Color schemes replace these;
    /// the 256-color cube and grayscale ramp (16..=255) stay standard.
    pub palette: [[u8; 3]; 16],
    /// Opacity of the default background (the window clear), 0.0..=1.0. Cells
    /// with an explicit SGR background stay opaque; only the default-bg areas
    /// (where no quad is drawn) become translucent. 1.0 is fully opaque.
    pub bg_alpha: f32,
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            fg: [0xd8, 0xdb, 0xe0],
            bg: [0x10, 0x10, 0x12],
            selection: [0x3a, 0x53, 0x7a],
            palette: ANSI_16,
            bg_alpha: 1.0,
        }
    }
}

impl Theme {
    /// Resolve an xterm 256-color index, honoring this theme's palette for the
    /// 16 base colors (the cube and grayscale ramp are scheme-independent).
    fn ansi(&self, i: u8) -> [u8; 3] {
        if (i as usize) < 16 {
            self.palette[i as usize]
        } else {
            index_to_rgb(i)
        }
    }
}

/// Alpha of the selection tint — translucent so text stays readable beneath it.
const SELECTION_ALPHA: f32 = 0.45;

/// Standard xterm 16-color base palette (indices 0..=15).
#[rustfmt::skip]
const ANSI_16: [[u8; 3]; 16] = [
    [0x00, 0x00, 0x00], [0x80, 0x00, 0x00], [0x00, 0x80, 0x00], [0x80, 0x80, 0x00],
    [0x00, 0x00, 0x80], [0x80, 0x00, 0x80], [0x00, 0x80, 0x80], [0xc0, 0xc0, 0xc0],
    [0x80, 0x80, 0x80], [0xff, 0x00, 0x00], [0x00, 0xff, 0x00], [0xff, 0xff, 0x00],
    [0x00, 0x00, 0xff], [0xff, 0x00, 0xff], [0x00, 0xff, 0xff], [0xff, 0xff, 0xff],
];

/// The six channel levels of the 6x6x6 color cube (indices 16..=231).
const CUBE_STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// Resolve an xterm 256-color index to RGB.
fn index_to_rgb(i: u8) -> [u8; 3] {
    match i {
        0..=15 => ANSI_16[i as usize],
        16..=231 => {
            let i = i - 16;
            [
                CUBE_STEPS[(i / 36) as usize],
                CUBE_STEPS[((i / 6) % 6) as usize],
                CUBE_STEPS[(i % 6) as usize],
            ]
        }
        232..=255 => {
            let v = 8 + 10 * (i - 232);
            [v, v, v]
        }
    }
}

/// Bold brightens the 8 base ANSI colors to their bright variants (xterm-ish).
fn maybe_brighten(c: Option<Color>, bold: bool) -> Option<Color> {
    match (bold, c) {
        (true, Some(Color::Indexed(i))) if i < 8 => Some(Color::Indexed(i + 8)),
        _ => c,
    }
}

fn resolve(c: Option<Color>, default: [u8; 3], theme: &Theme) -> [u8; 3] {
    match c {
        None => default,
        Some(Color::Indexed(i)) => theme.ansi(i),
        Some(Color::RGB(c)) => [c.r, c.g, c.b],
    }
}

fn to_rgba(c: [u8; 3]) -> [f32; 4] {
    [
        f32::from(c[0]) / 255.0,
        f32::from(c[1]) / 255.0,
        f32::from(c[2]) / 255.0,
        1.0,
    ]
}

/// Resolve a run's style to a foreground color and an optional background-rect
/// color. The background is `Some` only when it differs from the cleared theme
/// background (an explicit bg, or `inverse`), so default cells paint no rect.
fn run_colors(style: &Style, theme: Theme) -> ([f32; 4], Option<[f32; 4]>) {
    let mut fg = resolve(maybe_brighten(style.fg, style.bold), theme.fg, &theme);
    let mut bg = resolve(style.bg, theme.bg, &theme);
    let paint_bg = style.bg.is_some() || style.inverse;
    if style.inverse {
        std::mem::swap(&mut fg, &mut bg);
    }
    if style.faint {
        for i in 0..3 {
            fg[i] = ((u16::from(fg[i]) + u16::from(bg[i])) / 2) as u8;
        }
    }
    (to_rgba(fg), paint_bg.then(|| to_rgba(bg)))
}

/// Map each char's byte offset in a run's text to its starting cell column
/// within the run, so a shaped glyph (keyed by cluster byte offset) can be
/// snapped to the grid. Wide characters advance the column by two.
fn cell_starts(text: &str) -> HashMap<u32, usize> {
    let mut map = HashMap::new();
    let mut col = 0usize;
    for (byte, ch) in text.char_indices() {
        map.insert(byte as u32, col);
        col += UnicodeWidthChar::width(ch).unwrap_or(1).max(1);
    }
    map
}

// ---- GPU plumbing -------------------------------------------------------

/// Instanced textured quad: one per cell background and per visible glyph.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Instance {
    /// Screen rect in pixels: x, y, width, height (origin top-left).
    rect: [f32; 4],
    /// Atlas UV rect: u0, v0, u1, v1. Background rects point at the reserved
    /// opaque texel so they fill solid.
    uv: [f32; 4],
    /// Color, straight alpha.
    color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Uniforms {
    viewport: [f32; 2],
    _pad: [f32; 2],
}

const GLYPH_WGSL: &str = r#"
struct Uniforms { viewport: vec2<f32>, pad: vec2<f32> };
@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var atlas: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

struct InstanceIn {
    @location(0) rect: vec4<f32>,
    @location(1) uv: vec4<f32>,
    @location(2) color: vec4<f32>,
};
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
};

@vertex
fn vs(@builtin(vertex_index) vi: u32, inst: InstanceIn) -> VsOut {
    var corner = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(0.0, 1.0), vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0),
    );
    let c = corner[vi];
    let px = inst.rect.xy + c * inst.rect.zw;
    let clip = vec2<f32>(px.x / u.viewport.x * 2.0 - 1.0, 1.0 - px.y / u.viewport.y * 2.0);
    var out: VsOut;
    out.pos = vec4<f32>(clip, 0.0, 1.0);
    out.uv = mix(inst.uv.xy, inst.uv.zw, c);
    out.color = inst.color;
    return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    let a = textureSample(atlas, samp, in.uv).r;
    // Premultiplied output: the surface alpha modes our windows use (and Wayland
    // / macOS natively) expect colour already scaled by coverage. At full opacity
    // this is identity, so opaque rendering is unchanged.
    let cov = in.color.a * a;
    return vec4<f32>(in.color.rgb * cov, cov);
}

// Image variant: the bound texture is a full RGBA image, not the coverage atlas,
// so sample all four channels and premultiply (the image instance's colour is
// (1,1,1,1), leaving the texel unchanged) to match the premultiplied blending.
@fragment
fn fs_image(in: VsOut) -> @location(0) vec4<f32> {
    let texel = textureSample(atlas, samp, in.uv);
    let a = texel.a * in.color.a;
    return vec4<f32>(texel.rgb * a, a);
}
"#;

/// One kitty-graphics image to paint this frame: which uploaded image (by id),
/// its pixel rect already translated into viewport space, the scissor it clips
/// to, and its z-index (drawn low to high among images).
struct ImageDraw {
    image_id: u32,
    rect: [f32; 4],
    /// Source sub-rectangle of the image to sample, `[u0, v0, u1, v1]`.
    uv: [f32; 4],
    scissor: [u32; 4],
    z: i32,
}

/// A glyph's slot in the atlas plus its pen-relative placement.
#[derive(Clone, Copy)]
struct Slot {
    ax: u32,
    ay: u32,
    w: u32,
    h: u32,
    left: i32,
    top: i32,
}

const ATLAS_DIM: u32 = 2048;
/// UV of the reserved opaque texel at (0,0); background/cursor rects sample it.
const OPAQUE_UV: [f32; 4] = [
    0.5 / ATLAS_DIM as f32,
    0.5 / ATLAS_DIM as f32,
    0.5 / ATLAS_DIM as f32,
    0.5 / ATLAS_DIM as f32,
];

/// A contiguous run of instances and the scissor rect (`x, y, w, h`, framebuffer
/// pixels) it must be clipped to when drawn.
struct DrawGroup {
    scissor: [u32; 4],
    range: std::ops::Range<u32>,
}

/// Translate every instance's screen rect by `(dx, dy)`.
fn translate(insts: &mut [Instance], dx: f32, dy: f32) {
    for i in insts {
        i.rect[0] += dx;
        i.rect[1] += dy;
    }
}

/// Darken instance colors (RGB only) for an unfocused/dimmed tile.
fn dim_colors(insts: &mut [Instance]) {
    const DIM: f32 = 0.55;
    for i in insts {
        for c in i.color.iter_mut().take(3) {
            *c *= DIM;
        }
    }
}

/// A solid filled quad covering `rect`.
fn solid(rect: RectPx, color: [f32; 4]) -> Instance {
    Instance {
        rect: [rect.x, rect.y, rect.w, rect.h],
        uv: OPAQUE_UV,
        color,
    }
}

/// Push four thin solid quads outlining `rect`. Top/bottom span the full width;
/// the left/right quads are inset to the interior height so the four quads never
/// overlap — otherwise corners would be covered twice and blend darker for a
/// translucent border color.
fn push_border(out: &mut Vec<Instance>, rect: RectPx, color: [f32; 4], width: f32) {
    let RectPx { x, y, w, h } = rect;
    let inner_y = y + width;
    let inner_h = (h - 2.0 * width).max(0.0);
    out.push(solid(RectPx { x, y, w, h: width }, color)); // top
    out.push(solid(
        RectPx {
            x,
            y: y + h - width,
            w,
            h: width,
        },
        color,
    )); // bottom
    out.push(solid(
        RectPx {
            x,
            y: inner_y,
            w: width,
            h: inner_h,
        },
        color,
    )); // left
    out.push(solid(
        RectPx {
            x: x + w - width,
            y: inner_y,
            w: width,
            h: inner_h,
        },
        color,
    )); // right
}

fn badge_color(kind: BadgeKind) -> [f32; 4] {
    match kind {
        BadgeKind::Bell => [0.95, 0.75, 0.20, 1.0],
        BadgeKind::Activity => [0.30, 0.70, 0.95, 1.0],
    }
}

/// Clamp a float rect to a framebuffer-pixel scissor `[x, y, w, h]` within
/// `sw`×`sh`. The result always satisfies `x + w <= sw` and `y + h <= sh`.
fn clamp_scissor(r: RectPx, sw: u32, sh: u32) -> [u32; 4] {
    let x0 = (r.x.max(0.0).floor() as u32).min(sw);
    let y0 = (r.y.max(0.0).floor() as u32).min(sh);
    let x1 = ((r.x + r.w).max(0.0).ceil() as u32).min(sw);
    let y1 = ((r.y + r.h).max(0.0).ceil() as u32).min(sh);
    [x0, y0, x1.saturating_sub(x0), y1.saturating_sub(y0)]
}

/// Remembers the last scene a window actually drew, so an identical redraw
/// request (a query-only PTY feed, a cursor-blink tick that changed nothing, a
/// fleet tick where no tile moved) can be skipped before the GPU surface is even
/// acquired — leaving the previously presented frame untouched on screen.
///
/// Kept free of any GPU handle so the decision is unit-testable without a device.
#[derive(Default)]
pub struct SceneCache {
    last: Option<(Scene, f32)>,
}

impl SceneCache {
    /// Whether `(scene, font_px)` differs from the last frame accepted here. When
    /// it differs this records it as the new last and returns `true` (the caller
    /// should draw); when identical it returns `false` (the caller may skip the
    /// whole acquire/draw/present cycle).
    ///
    /// The comparison is exact `PartialEq` on the scene — never a hash — so an
    /// equal verdict can never be a false positive that strands a stale frame.
    pub fn needs_redraw(&mut self, scene: &Scene, font_px: f32) -> bool {
        if matches!(&self.last, Some((s, px)) if s == scene && *px == font_px) {
            return false;
        }
        self.last = Some((scene.clone(), font_px));
        true
    }

    /// Forget the last scene so the next [`needs_redraw`](Self::needs_redraw)
    /// always draws. The caller invalidates whenever it accepted a scene here but
    /// then failed to actually present it (e.g. the surface was lost/outdated and
    /// had to be reconfigured), so the recorded scene never gets ahead of what is
    /// really on screen.
    pub fn invalidate(&mut self) {
        self.last = None;
    }
}

/// A persistent terminal renderer: device, pipeline, glyph atlas and cache are
/// built once and reused across frames.
pub struct Renderer {
    gpu: Gpu,
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    uniform_buf: wgpu::Buffer,
    atlas: wgpu::Texture,
    theme: Theme,
    /// Color-attachment format the pipeline targets (offscreen vs surface).
    format: wgpu::TextureFormat,
    /// glyph cache keyed by (glyph id, font size bits, synthesis); `None` = no bitmap.
    cache: HashMap<(u16, u32, Synthesis), Option<Slot>>,
    // shelf-packing cursor into the atlas.
    pack_x: u32,
    pack_y: u32,
    shelf: u32,
    // per-frame instance buffer.
    instances: Option<wgpu::Buffer>,
    instance_count: u32,
    /// Current text selection to highlight, in viewport cell coordinates.
    selection: Option<Selection>,
    /// Bind-group layout shared by the glyph and image pipelines (uniform, a
    /// filterable texture, a sampler) — kept so per-image bind groups can be
    /// built when an image is uploaded.
    bind_layout: wgpu::BindGroupLayout,
    /// Sampler shared by both pipelines.
    sampler: wgpu::Sampler,
    /// RGBA pipeline for kitty-graphics image quads (samples `.rgba`, unlike the
    /// glyph pipeline which reads the `.r` coverage atlas).
    image_pipeline: wgpu::RenderPipeline,
    /// Uploaded image bind groups, keyed by image id; the blob is sent once and
    /// the bind group (which owns its texture) lives until eviction.
    image_bind_groups: HashMap<u32, wgpu::BindGroup>,
    /// Per-frame resolved image draws (z-sorted within a terminal) and the
    /// one-quad-per-draw instance buffer they index.
    image_draws: Vec<ImageDraw>,
    image_instances: Option<wgpu::Buffer>,
}

impl Renderer {
    /// Build a headless renderer (software adapter) for offscreen rendering and
    /// golden tests.
    pub fn headless(theme: Theme) -> Self {
        Self::new(Gpu::headless(), theme, FORMAT)
    }

    /// Build a renderer on an existing device/queue (e.g. a windowed device).
    /// `format` is the color-attachment format the pipeline must target — the
    /// offscreen [`FORMAT`] for tests, or a window surface's format.
    pub fn new(gpu: Gpu, theme: Theme, format: wgpu::TextureFormat) -> Self {
        let atlas = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("glyph atlas"),
            size: wgpu::Extent3d {
                width: ATLAS_DIM,
                height: ATLAS_DIM,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        // Reserve the (0,0) texel as fully opaque so background/cursor rects can
        // sample a coverage of 1.0 and fill solid through the same pipeline.
        gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &atlas,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &[255u8],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(1),
                rows_per_image: Some(1),
            },
            wgpu::Extent3d {
                width: 1,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        let atlas_view = atlas.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = gpu.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("glyph sampler"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let uniform_buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_layout = gpu
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("glyph bind layout"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::VERTEX,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::FRAGMENT,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                ],
            });
        let bind_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("glyph bind group"),
            layout: &bind_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: uniform_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&atlas_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&sampler),
                },
            ],
        });

        let shader = gpu
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("glyph shader"),
                source: wgpu::ShaderSource::Wgsl(GLYPH_WGSL.into()),
            });
        let pipeline_layout = gpu
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("glyph pipeline layout"),
                bind_group_layouts: &[Some(&bind_layout)],
                immediate_size: 0,
            });
        const ATTRS: [wgpu::VertexAttribute; 3] =
            wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4];
        let pipeline = gpu
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("glyph pipeline"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<Instance>() as u64,
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes: &ATTRS,
                    }],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        // Premultiplied "over": the fragment shader already
                        // scales colour by alpha, so the source factor is One.
                        blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });

        // The image pipeline is the glyph pipeline with the RGBA-sampling fragment
        // entry: same layout, vertex format and premultiplied blending, so image
        // quads share the instance vertex shader and the viewport uniform.
        let image_pipeline = gpu
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("image pipeline"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &shader,
                    entry_point: Some("vs"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    buffers: &[wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<Instance>() as u64,
                        step_mode: wgpu::VertexStepMode::Instance,
                        attributes: &ATTRS,
                    }],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &shader,
                    entry_point: Some("fs_image"),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });

        Renderer {
            gpu,
            pipeline,
            bind_group,
            uniform_buf,
            atlas,
            theme,
            format,
            cache: HashMap::new(),
            pack_x: 1,
            pack_y: 0,
            shelf: 1,
            instances: None,
            instance_count: 0,
            selection: None,
            bind_layout,
            sampler,
            image_pipeline,
            image_bind_groups: HashMap::new(),
            image_draws: Vec::new(),
            image_instances: None,
        }
    }

    /// Set (or clear) the text selection to highlight on subsequent frames.
    pub fn set_selection(&mut self, selection: Option<Selection>) {
        self.selection = selection;
    }

    /// Cache a kitty-graphics image's pixels on the GPU as its own RGBA texture,
    /// keyed by `id`, so the (potentially large) upload happens once. `rgba` is
    /// straight-alpha `Rgba8` packed row-major, `width * height * 4` bytes.
    /// Idempotent per id for now — re-upload of a replaced id lands with LRU
    /// eviction in a later phase. A zero dimension or short buffer is ignored.
    pub fn upload_image(&mut self, id: u32, width: u32, height: u32, rgba: &[u8]) {
        // A single dimension can exceed the device's max texture size while the
        // image's total bytes stay within ghost-term's budget (e.g. 9000x1 RGBA is
        // ~36 KiB), and create_texture treats that as an unrecoverable validation
        // error that aborts the process. That's reachable from ordinary terminal
        // output, so skip such an image rather than crash; scaling it down to fit
        // is a later refinement.
        let max = self.gpu.device.limits().max_texture_dimension_2d;
        if width == 0
            || height == 0
            || width > max
            || height > max
            || self.image_bind_groups.contains_key(&id)
            || (rgba.len() as u64) < u64::from(width) * u64::from(height) * 4
        {
            return;
        }
        let texture = self.gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("kitty image"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // Linear, matching the renderer's direct treatment of colour (FORMAT
            // is also non-sRGB), so a stored texel reaches the attachment unchanged.
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        self.gpu.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(width * 4),
                rows_per_image: Some(height),
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        // The bind group owns strong references to the texture/view, so the
        // local handles can drop while the cached entry keeps them alive.
        let bind_group = self
            .gpu
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("image bind group"),
                layout: &self.bind_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: self.uniform_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(&view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            });
        self.image_bind_groups.insert(id, bind_group);
    }

    /// Resolve `frame`'s image placements into pixel-space draws, offset to
    /// `(ox, oy)` and clipped to `scissor`. Only images already uploaded are
    /// emitted; an unknown id has no texture to sample yet, so it is skipped.
    fn collect_image_draws(
        &self,
        frame: &Frame,
        ox: f32,
        oy: f32,
        scissor: [u32; 4],
        out: &mut Vec<ImageDraw>,
    ) {
        let m = frame.metrics;
        for img in &frame.images {
            if !self.image_bind_groups.contains_key(&img.image_id) {
                continue;
            }
            out.push(ImageDraw {
                image_id: img.image_id,
                rect: [
                    ox + img.col as f32 * m.advance,
                    oy + img.row as f32 * m.line_height,
                    img.cols as f32 * m.advance,
                    img.rows as f32 * m.line_height,
                ],
                uv: img.uv,
                scissor,
                z: img.z,
            });
        }
    }

    /// Upload the one-quad-per-draw instance buffer for `self.image_draws`,
    /// painter-ordered low z to high (a stable sort, so equal-z images keep their
    /// placement order). The instance buffer and [`Self::draw_images`] then share
    /// this order, so quad index `i` is draw `i`.
    fn build_image_instances(&mut self) {
        if self.image_draws.is_empty() {
            self.image_instances = None;
            return;
        }
        self.image_draws.sort_by_key(|d| d.z);
        let insts: Vec<Instance> = self
            .image_draws
            .iter()
            .map(|d| Instance {
                rect: d.rect,
                uv: d.uv,
                color: [1.0, 1.0, 1.0, 1.0],
            })
            .collect();
        self.image_instances = Some(self.gpu.device.create_buffer_init(
            &wgpu::util::BufferInitDescriptor {
                label: Some("image instances"),
                contents: bytemuck::cast_slice(&insts),
                usage: wgpu::BufferUsages::VERTEX,
            },
        ));
    }

    /// Draw the prepared image quads over the glyph layer, each under its own
    /// scissor and bound to its image's texture. Mirrors the glyph draw's empty
    /// guard. Images currently paint over text (z relative to glyphs is a later
    /// refinement); within a terminal they are ordered by their placement z.
    fn draw_images<'p>(&'p self, pass: &mut wgpu::RenderPass<'p>) {
        let Some(buf) = &self.image_instances else {
            return;
        };
        pass.set_pipeline(&self.image_pipeline);
        pass.set_vertex_buffer(0, buf.slice(..));
        for (i, d) in self.image_draws.iter().enumerate() {
            if d.scissor[2] == 0 || d.scissor[3] == 0 {
                continue; // fully off-screen tile
            }
            // Defense-in-depth: collect_image_draws already drops un-uploaded ids,
            // so this only fires if that invariant breaks — never in normal use.
            let Some(bg) = self.image_bind_groups.get(&d.image_id) else {
                continue;
            };
            pass.set_bind_group(0, bg, &[]);
            pass.set_scissor_rect(d.scissor[0], d.scissor[1], d.scissor[2], d.scissor[3]);
            let i = i as u32;
            pass.draw(0..6, i..i + 1);
        }
    }

    /// The pixel dimensions a frame renders to at its cell metrics.
    pub fn frame_size(frame: &Frame) -> (u32, u32) {
        let w = (frame.cols as f32 * frame.metrics.advance).ceil().max(1.0) as u32;
        let h = (frame.rows as f32 * frame.metrics.line_height)
            .ceil()
            .max(1.0) as u32;
        (w, h)
    }

    /// Rasterize (if needed) and pack a glyph into the atlas, returning its slot.
    fn ensure_glyph(
        &mut self,
        font: FontRef,
        id: u16,
        size_px: f32,
        synth: Synthesis,
    ) -> Option<Slot> {
        let key = (id, size_px.to_bits(), synth);
        if let Some(slot) = self.cache.get(&key) {
            return *slot;
        }
        let resolved = match ghost_shaper::rasterize(font, id, size_px, synth) {
            Some(bmp) if bmp.width > 0 && bmp.height > 0 => {
                if self.pack_x + bmp.width + 1 > ATLAS_DIM {
                    self.pack_x = 1;
                    self.pack_y += self.shelf + 1;
                    self.shelf = 0;
                }
                if self.pack_y + bmp.height > ATLAS_DIM {
                    None // atlas full; skip drawing this glyph
                } else {
                    let slot = Slot {
                        ax: self.pack_x,
                        ay: self.pack_y,
                        w: bmp.width,
                        h: bmp.height,
                        left: bmp.left,
                        top: bmp.top,
                    };
                    self.gpu.queue.write_texture(
                        wgpu::TexelCopyTextureInfo {
                            texture: &self.atlas,
                            mip_level: 0,
                            origin: wgpu::Origin3d {
                                x: slot.ax,
                                y: slot.ay,
                                z: 0,
                            },
                            aspect: wgpu::TextureAspect::All,
                        },
                        &bmp.coverage,
                        wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(bmp.width),
                            rows_per_image: Some(bmp.height),
                        },
                        wgpu::Extent3d {
                            width: bmp.width,
                            height: bmp.height,
                            depth_or_array_layers: 1,
                        },
                    );
                    self.pack_x += bmp.width + 1;
                    self.shelf = self.shelf.max(bmp.height);
                    Some(slot)
                }
            }
            _ => None,
        };
        self.cache.insert(key, resolved);
        resolved
    }

    fn slot_uv(slot: &Slot) -> [f32; 4] {
        let dim = ATLAS_DIM as f32;
        [
            slot.ax as f32 / dim,
            slot.ay as f32 / dim,
            (slot.ax + slot.w) as f32 / dim,
            (slot.ay + slot.h) as f32 / dim,
        ]
    }

    /// Build the per-frame instance list: cell backgrounds + cursor block first,
    /// then glyph quads on top. Coordinates are frame-local (origin `(0, 0)`);
    /// the scene path translates them to the tile's rect. `selection` is passed
    /// explicitly so each terminal in a scene carries its own.
    fn build_instances(
        &mut self,
        frame: &Frame,
        font: FontRef,
        size_px: f32,
        selection: Option<Selection>,
    ) -> Vec<Instance> {
        let metrics = frame.metrics;
        let baseline = metrics.line_height * 0.8;
        let cursor = frame.cursor;

        let mut backgrounds: Vec<Instance> = Vec::new();
        let mut selection_rects: Vec<Instance> = Vec::new();
        let mut glyphs: Vec<Instance> = Vec::new();

        // Selection highlight: one translucent rect per selected row, computed
        // straight from cell geometry (trimmed trailing blanks carry no run, so
        // it can't be derived from runs). Drawn over backgrounds, under glyphs.
        if let Some(sel) = selection {
            let [r, g, b] = self.theme.selection;
            let color = [
                f32::from(r) / 255.0,
                f32::from(g) / 255.0,
                f32::from(b) / 255.0,
                SELECTION_ALPHA,
            ];
            for row in 0..frame.rows_layout.len() {
                if let Some((c0, c1)) = sel.row_span(row, frame.cols) {
                    selection_rects.push(Instance {
                        rect: [
                            c0 as f32 * metrics.advance,
                            row as f32 * metrics.line_height,
                            (c1 - c0) as f32 * metrics.advance,
                            metrics.line_height,
                        ],
                        uv: OPAQUE_UV,
                        color,
                    });
                }
            }
        }

        for (row, layout) in frame.rows_layout.iter().enumerate() {
            let row_y = row as f32 * metrics.line_height;
            let baseline_y = row_y + baseline;
            for run in &layout.runs {
                let cursor_here = cursor.filter(|c| c.row == row && c.col == run.start_col);
                let (fg, bg_opt) = run_colors(&run.style, self.theme);
                let x = run.start_col as f32 * metrics.advance;
                let w = run.width_cols as f32 * metrics.advance;

                // Reverse-video block cursor: fill the cell with the displayed
                // foreground and draw its glyph in the displayed background.
                // `fg`/`bg_opt` are already post-inverse and post-faint, so on an
                // inverse or faint cell the cursor reflects that — the standard
                // xterm behaviour where the cursor reverses whatever is shown.
                // Underline and bar cursors leave the glyph in its normal colour
                // and instead get a thin rule drawn after the glyphs (below).
                let block_cursor = matches!(cursor_here.map(|c| c.shape), Some(CursorShape::Block));
                let (block, glyph_color) = if block_cursor {
                    (Some(fg), bg_opt.unwrap_or(to_rgba(self.theme.bg)))
                } else {
                    (bg_opt, fg)
                };
                if let Some(color) = block {
                    backgrounds.push(Instance {
                        rect: [x, row_y, w, metrics.line_height],
                        uv: OPAQUE_UV,
                        color,
                    });
                }

                // Place each shaped glyph at its cluster's CELL origin, not by
                // accumulating font advance — a terminal is a fixed grid, so a
                // ligature spans its cells naturally and a wide char occupies two
                // columns regardless of the font's reported advance.
                let starts = cell_starts(&run.text);
                for g in ghost_shaper::shape(font, &run.text, size_px) {
                    let cell = starts.get(&g.cluster).copied().unwrap_or(0);
                    let pen = (run.start_col + cell) as f32 * metrics.advance;
                    let synth = Synthesis {
                        italic: run.style.italic,
                        bold: run.style.bold,
                    };
                    if let Some(slot) = self.ensure_glyph(font, g.id, size_px, synth) {
                        glyphs.push(Instance {
                            rect: [
                                pen + slot.left as f32,
                                baseline_y - slot.top as f32,
                                slot.w as f32,
                                slot.h as f32,
                            ],
                            uv: Self::slot_uv(&slot),
                            color: glyph_color,
                        });
                    }
                }

                // Underline / strikethrough: solid rules in the text color,
                // spanning the run, drawn with the glyphs so they sit on top of
                // the cell background. Thickness scales with the (physical) cell.
                if run.style.underline || run.style.strikethrough {
                    let thickness = (metrics.line_height / 14.0).max(1.0);
                    let line = |y: f32| {
                        solid(
                            RectPx {
                                x,
                                y,
                                w,
                                h: thickness,
                            },
                            glyph_color,
                        )
                    };
                    if run.style.underline {
                        let y =
                            (baseline_y + thickness).min(row_y + metrics.line_height - thickness);
                        glyphs.push(line(y));
                    }
                    if run.style.strikethrough {
                        glyphs.push(line(row_y + metrics.line_height * 0.5 - thickness * 0.5));
                    }
                }

                // Underline / bar cursor: a solid rule in the foreground colour
                // over the unmodified glyph (the block path above handles its own
                // fill). Underline = a thick rule along the cell bottom; bar = a
                // thin rule down the cell's leading edge.
                match cursor_here.map(|c| c.shape) {
                    Some(CursorShape::Underline) => {
                        let thickness = (metrics.line_height / 8.0).max(2.0);
                        glyphs.push(solid(
                            RectPx {
                                x,
                                y: row_y + metrics.line_height - thickness,
                                w,
                                h: thickness,
                            },
                            fg,
                        ));
                    }
                    Some(CursorShape::Bar) => {
                        let width = (metrics.advance / 8.0).max(2.0);
                        glyphs.push(solid(
                            RectPx {
                                x,
                                y: row_y,
                                w: width,
                                h: metrics.line_height,
                            },
                            fg,
                        ));
                    }
                    _ => {}
                }
            }
        }

        backgrounds.extend(selection_rects); // tint over cell backgrounds
        backgrounds.extend(glyphs); // glyphs stay crisp on top
        backgrounds
    }

    /// Prepare GPU state for one frame: pack glyphs, upload instances, set the
    /// viewport uniform.
    fn prepare(&mut self, frame: &Frame, font: FontRef, size_px: f32, vw: u32, vh: u32) {
        let instances = self.build_instances(frame, font, size_px, self.selection);
        self.instance_count = instances.len() as u32;
        self.instances = Some(self.gpu.device.create_buffer_init(
            &wgpu::util::BufferInitDescriptor {
                label: Some("instances"),
                contents: bytemuck::cast_slice(&instances),
                usage: wgpu::BufferUsages::VERTEX,
            },
        ));
        let uniforms = Uniforms {
            viewport: [vw as f32, vh as f32],
            _pad: [0.0, 0.0],
        };
        self.gpu
            .queue
            .write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(&uniforms));
        let mut imgs = Vec::new();
        self.collect_image_draws(frame, 0.0, 0.0, [0, 0, vw, vh], &mut imgs);
        self.image_draws = imgs;
        self.build_image_instances();
    }

    /// Record a clear-and-draw pass into `view`, returning the command buffer.
    fn encode(&self, view: &wgpu::TextureView) -> wgpu::CommandBuffer {
        let mut encoder = self
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("frame"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("frame"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            // Premultiplied clear: RGB scaled by the bg opacity.
                            r: f64::from(self.theme.bg[0]) / 255.0 * f64::from(self.theme.bg_alpha),
                            g: f64::from(self.theme.bg[1]) / 255.0 * f64::from(self.theme.bg_alpha),
                            b: f64::from(self.theme.bg[2]) / 255.0 * f64::from(self.theme.bg_alpha),
                            a: f64::from(self.theme.bg_alpha),
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if let Some(buf) = &self.instances
                && self.instance_count > 0
            {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_vertex_buffer(0, buf.slice(..));
                pass.draw(0..6, 0..self.instance_count);
            }
            self.draw_images(&mut pass);
        }
        encoder.finish()
    }

    /// Render a frame into a window surface's texture view. The caller owns
    /// acquiring/presenting the surface texture.
    pub fn render_to_view(
        &mut self,
        view: &wgpu::TextureView,
        vw: u32,
        vh: u32,
        frame: &Frame,
        font: FontRef,
        size_px: f32,
    ) {
        self.prepare(frame, font, size_px, vw, vh);
        let cb = self.encode(view);
        self.gpu.queue.submit([cb]);
    }

    /// Render a frame to an offscreen target and read the pixels back.
    pub fn render_offscreen(&mut self, frame: &Frame, font: FontRef, size_px: f32) -> Rendered {
        let (w, h) = Self::frame_size(frame);
        self.prepare(frame, font, size_px, w, h);
        let target = offscreen_target(&self.gpu.device, w, h, self.format);
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let cb = self.encode(&view);
        self.gpu.queue.submit([cb]);
        let rgba = read_back(&self.gpu, &target, w, h);
        Rendered {
            width: w,
            height: h,
            rgba,
        }
    }

    /// Build a scene's combined instance list plus the per-item draw groups
    /// (each carrying the scissor rect it must be clipped to). Layers are walked
    /// low `z` to high; items keep insertion order within a layer. A `Terminal`
    /// reuses [`Self::build_instances`] translated to its rect and clipped to it
    /// (without the clip, neighbouring tiles would bleed into each other).
    fn build_scene(
        &mut self,
        scene: &Scene,
        font: FontRef,
        size_px: f32,
    ) -> (Vec<Instance>, Vec<DrawGroup>, Vec<ImageDraw>) {
        let (sw, sh) = scene.size_px;
        let mut all: Vec<Instance> = Vec::new();
        let mut groups: Vec<DrawGroup> = Vec::new();
        let mut images: Vec<ImageDraw> = Vec::new();
        let mut order: Vec<&Layer> = scene.layers.iter().collect();
        order.sort_by_key(|l| l.z); // stable: keeps insertion order within a z

        for layer in order {
            for item in &layer.items {
                let start = all.len() as u32;
                // Only terminals clip to their rect; chrome may legitimately draw
                // anywhere (e.g. a border one pixel outside its content box).
                let scissor = match item {
                    SceneItem::Terminal { rect, .. } => clamp_scissor(*rect, sw, sh),
                    _ => [0, 0, sw, sh],
                };
                match item {
                    SceneItem::Terminal {
                        rect,
                        frame,
                        selection,
                        dim,
                        ..
                    } => {
                        let mut insts = self.build_instances(frame, font, size_px, *selection);
                        translate(&mut insts, rect.x, rect.y);
                        if *dim {
                            dim_colors(&mut insts);
                        }
                        all.extend(insts);
                        self.collect_image_draws(frame, rect.x, rect.y, scissor, &mut images);
                    }
                    SceneItem::Rect { rect, color, .. } => all.push(solid(*rect, *color)),
                    SceneItem::Border {
                        rect, color, width, ..
                    } => push_border(&mut all, *rect, *color, *width),
                    SceneItem::Badge { rect, kind, .. } => {
                        all.push(solid(*rect, badge_color(*kind)))
                    }
                    SceneItem::Text {
                        rect,
                        runs,
                        metrics,
                        color,
                        ..
                    } => {
                        let t = self.text_instances(*rect, runs, *metrics, *color, font, size_px);
                        all.extend(t);
                    }
                }
                let end = all.len() as u32;
                if end > start {
                    groups.push(DrawGroup {
                        scissor,
                        range: start..end,
                    });
                }
            }
        }
        (all, groups, images)
    }

    /// Glyph instances for a text item: its runs laid out as one line from
    /// `rect`'s origin, all glyphs in the item's color (chrome labels ignore
    /// per-run fg).
    fn text_instances(
        &mut self,
        rect: RectPx,
        runs: &[Run],
        metrics: CellMetrics,
        color: [f32; 4],
        font: FontRef,
        size_px: f32,
    ) -> Vec<Instance> {
        let baseline = rect.y + metrics.line_height * 0.8;
        let mut out = Vec::new();
        for run in runs {
            let starts = cell_starts(&run.text);
            for g in ghost_shaper::shape(font, &run.text, size_px) {
                let cell = starts.get(&g.cluster).copied().unwrap_or(0);
                let pen = rect.x + (run.start_col + cell) as f32 * metrics.advance;
                let synth = Synthesis {
                    italic: run.style.italic,
                    bold: run.style.bold,
                };
                if let Some(slot) = self.ensure_glyph(font, g.id, size_px, synth) {
                    out.push(Instance {
                        rect: [
                            pen + slot.left as f32,
                            baseline - slot.top as f32,
                            slot.w as f32,
                            slot.h as f32,
                        ],
                        uv: Self::slot_uv(&slot),
                        color,
                    });
                }
            }
        }
        out
    }

    /// Upload a scene's instances and viewport uniform; return the draw groups.
    fn prepare_scene(&mut self, scene: &Scene, font: FontRef, size_px: f32) -> Vec<DrawGroup> {
        let (instances, groups, images) = self.build_scene(scene, font, size_px);
        self.image_draws = images;
        self.build_image_instances();
        self.instance_count = instances.len() as u32;
        self.instances = Some(self.gpu.device.create_buffer_init(
            &wgpu::util::BufferInitDescriptor {
                label: Some("scene instances"),
                contents: bytemuck::cast_slice(&instances),
                usage: wgpu::BufferUsages::VERTEX,
            },
        ));
        let (vw, vh) = scene.size_px;
        let uniforms = Uniforms {
            viewport: [vw as f32, vh as f32],
            _pad: [0.0, 0.0],
        };
        self.gpu
            .queue
            .write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(&uniforms));
        groups
    }

    /// Clear once, then draw each group under its own scissor rect.
    fn encode_scene(&self, view: &wgpu::TextureView, groups: &[DrawGroup]) -> wgpu::CommandBuffer {
        let mut encoder = self
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("scene"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("scene"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            // Premultiplied clear: RGB scaled by the bg opacity.
                            r: f64::from(self.theme.bg[0]) / 255.0 * f64::from(self.theme.bg_alpha),
                            g: f64::from(self.theme.bg[1]) / 255.0 * f64::from(self.theme.bg_alpha),
                            b: f64::from(self.theme.bg[2]) / 255.0 * f64::from(self.theme.bg_alpha),
                            a: f64::from(self.theme.bg_alpha),
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            // The `> 0` guard mirrors `encode`: an empty scene (e.g. a blank
            // screen with a hidden cursor) produces no instances, and slicing a
            // zero-size vertex buffer panics. The clear above still happens, so
            // an empty scene reads back as the solid background — byte-identical
            // to `render_offscreen` on the same empty frame.
            if let Some(buf) = &self.instances
                && self.instance_count > 0
            {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_vertex_buffer(0, buf.slice(..));
                for g in groups {
                    if g.scissor[2] == 0 || g.scissor[3] == 0 {
                        continue; // fully off-screen tile: nothing to draw
                    }
                    pass.set_scissor_rect(g.scissor[0], g.scissor[1], g.scissor[2], g.scissor[3]);
                    pass.draw(0..6, g.range.clone());
                }
            }
            self.draw_images(&mut pass);
        }
        encoder.finish()
    }

    /// Render a scene into a window surface's texture view. `scene.size_px` must
    /// equal `view`'s dimensions: it drives both the NDC viewport and the
    /// scissor clamp, so a mismatch (e.g. mid-resize) would scissor past the
    /// attachment. The caller owns acquiring/presenting the surface texture.
    pub fn render_scene_to_view(
        &mut self,
        view: &wgpu::TextureView,
        scene: &Scene,
        font: FontRef,
        size_px: f32,
    ) {
        let groups = self.prepare_scene(scene, font, size_px);
        let cb = self.encode_scene(view, &groups);
        self.gpu.queue.submit([cb]);
    }

    /// Render a scene to an offscreen target and read the pixels back. For a
    /// single full-window `Terminal` this is byte-identical to
    /// [`Self::render_offscreen`] (pinned by a golden test).
    pub fn render_offscreen_scene(
        &mut self,
        scene: &Scene,
        font: FontRef,
        size_px: f32,
    ) -> Rendered {
        let w = scene.size_px.0.max(1);
        let h = scene.size_px.1.max(1);
        let groups = self.prepare_scene(scene, font, size_px);
        let target = offscreen_target(&self.gpu.device, w, h, self.format);
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let cb = self.encode_scene(&view, &groups);
        self.gpu.queue.submit([cb]);
        let rgba = read_back(&self.gpu, &target, w, h);
        Rendered {
            width: w,
            height: h,
            rgba,
        }
    }
}

/// Convenience: render a single frame offscreen on a fresh headless renderer.
pub fn render_frame(frame: &Frame, font: FontRef, size_px: f32, theme: Theme) -> Rendered {
    Renderer::headless(theme).render_offscreen(frame, font, size_px)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ghost_render::{Layer, RectPx, Scene, SceneId, SceneItem, layout_frame};
    use ghost_term::Vt;

    const FIRA: &[u8] = include_bytes!("../../shaper/tests/assets/FiraCode-Regular.ttf");
    const SIZE_PX: f32 = 15.0;
    const TM: CellMetrics = CellMetrics {
        advance: 9.0,
        line_height: 18.0,
    };

    fn frame(cols: usize, rows: usize, s: &str) -> Frame {
        let mut v = Vt::new(cols, rows);
        v.feed_str(s);
        layout_frame(&v, TM)
    }

    fn px(img: &Rendered, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * img.width + x) * 4) as usize;
        [
            img.rgba[i],
            img.rgba[i + 1],
            img.rgba[i + 2],
            img.rgba[i + 3],
        ]
    }

    fn is_red(p: [u8; 4]) -> bool {
        p[0] > 0x60 && p[1] < 0x20 && p[2] < 0x20
    }

    fn is_green(p: [u8; 4]) -> bool {
        p[0] < 0x20 && p[1] > 0x60 && p[2] < 0x20
    }

    #[test]
    fn scene_cache_skips_identical_redraws_until_invalidated() {
        let mut cache = SceneCache::default();
        let a = Scene::new((100, 50));
        let mut b = Scene::new((100, 50));
        b.layers.push(Layer {
            z: 0,
            items: vec![SceneItem::Rect {
                id: SceneId::Root,
                rect: RectPx {
                    x: 0.0,
                    y: 0.0,
                    w: 1.0,
                    h: 1.0,
                },
                color: [0.0, 0.0, 0.0, 1.0],
                radius: 0.0,
            }],
        });

        // First ever scene must draw; an identical follow-up is skipped.
        assert!(cache.needs_redraw(&a, 16.0));
        assert!(!cache.needs_redraw(&a, 16.0));

        // A different scene draws, then is itself cached.
        assert!(cache.needs_redraw(&b, 16.0));
        assert!(!cache.needs_redraw(&b, 16.0));

        // Same scene at a different font size must redraw (the raster differs).
        assert!(cache.needs_redraw(&b, 20.0));
        assert!(!cache.needs_redraw(&b, 20.0));

        // After invalidation (e.g. a surface reconfigure that dropped the frame)
        // the very next call redraws even though the scene is unchanged.
        cache.invalidate();
        assert!(cache.needs_redraw(&b, 20.0));
    }

    #[test]
    fn kitty_graphics_image_paints_at_its_cell() {
        let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
        // Transmit + display a 2x1 RGB image (id 7): one red then one green pixel,
        // with an explicit 2x1-cell footprint, at the cursor (top-left).
        let mut v = Vt::new(20, 4);
        v.feed_str("\x1b_Gi=7,a=T,f=24,s=2,v=1,c=2,r=1;/wAAAP8A\x1b\\");
        let f = layout_frame(&v, TM);
        assert_eq!(
            f.images.len(),
            1,
            "the placement is laid out into the frame"
        );

        // Upload the decoded pixels out of band, as the core's Cmd::UploadImage would.
        let mut r = Renderer::headless(Theme::default());
        let img = v.graphics_image(7).expect("image stored");
        r.upload_image(7, img.width, img.height, &img.pixels);

        let (w, h) = Renderer::frame_size(&f);
        let scene = Scene {
            size_px: (w, h),
            layers: vec![Layer {
                z: 0,
                items: vec![SceneItem::Terminal {
                    id: SceneId::Root,
                    rect: RectPx {
                        x: 0.0,
                        y: 0.0,
                        w: w as f32,
                        h: h as f32,
                    },
                    frame: f.clone(),
                    selection: None,
                    dim: false,
                }],
            }],
        };
        let out = r.render_offscreen_scene(&scene, font, SIZE_PX);

        // The image spans cells (0,0)+(1,0): 18px wide (advance 9), 18px tall
        // (line_height 18). Nearest sampling stretches the 2px image so the left
        // cell shows red and the right cell green.
        assert!(is_red(px(&out, 4, 9)), "left half shows the red pixel");
        assert!(
            is_green(px(&out, 13, 9)),
            "right half shows the green pixel"
        );
        // Below the one-row footprint is plain background, neither red nor green.
        let below = px(&out, 4, 40);
        assert!(
            !is_red(below) && !is_green(below),
            "below the image is background"
        );
    }

    #[test]
    fn unicode_placeholder_image_paints_at_its_cells() {
        let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
        // Transmit a 2x1 image as id 7 (store, no direct placement), then display
        // it via two Unicode-placeholder cells whose foreground encodes id 7.
        let mut v = Vt::new(20, 4);
        v.feed_str("\x1b_Gi=7,a=t,f=24,s=2,v=1;/wAAAP8A\x1b\\");
        v.feed_str("\x1b[38;2;0;0;7m\u{10eeee}\u{10eeee}");
        let f = layout_frame(&v, TM);
        assert_eq!(
            f.images.len(),
            2,
            "the 2x1 placeholder block becomes one per-cell placement each"
        );

        let mut r = Renderer::headless(Theme::default());
        let img = v.graphics_image(7).expect("image stored");
        r.upload_image(7, img.width, img.height, &img.pixels);

        let (w, h) = Renderer::frame_size(&f);
        let scene = Scene {
            size_px: (w, h),
            layers: vec![Layer {
                z: 0,
                items: vec![SceneItem::Terminal {
                    id: SceneId::Root,
                    rect: RectPx {
                        x: 0.0,
                        y: 0.0,
                        w: w as f32,
                        h: h as f32,
                    },
                    frame: f.clone(),
                    selection: None,
                    dim: false,
                }],
            }],
        };
        let out = r.render_offscreen_scene(&scene, font, SIZE_PX);
        // Image stretched across the 2x1 cell block (18px wide): left cell red,
        // right cell green — the same result as a direct placement, via placeholders.
        assert!(is_red(px(&out, 4, 9)), "left placeholder cell shows red");
        assert!(
            is_green(px(&out, 13, 9)),
            "right placeholder cell shows green"
        );
    }

    #[test]
    fn image_not_uploaded_is_skipped_not_panicking() {
        // A placement whose pixels were never uploaded must produce no image draw
        // (and not panic on a missing texture) — the frontend uploads lazily.
        let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
        let mut v = Vt::new(20, 4);
        v.feed_str("\x1b_Gi=9,a=T,f=24,s=2,v=1,c=2,r=1;/wAAAP8A\x1b\\");
        let f = layout_frame(&v, TM);
        assert_eq!(f.images.len(), 1);
        let mut r = Renderer::headless(Theme::default());
        let out = r.render_offscreen(&f, font, SIZE_PX);
        // The collect-time skip drops it before any quad is built — assert that
        // directly, so a regression that removes the skip is caught even though
        // draw_images would still guard against a missing texture downstream.
        assert!(
            r.image_draws.is_empty(),
            "an un-uploaded image produces no image draw"
        );
        // And nothing red/green is painted anywhere.
        let any_colored = (0..out.width).step_by(3).any(|x| {
            (0..out.height)
                .step_by(3)
                .any(|y| is_red(px(&out, x, y)) || is_green(px(&out, x, y)))
        });
        assert!(!any_colored, "an un-uploaded image paints nothing");
    }

    #[test]
    fn upload_image_skips_oversize_dimensions_without_panicking() {
        // An image one pixel wider than the device allows must be skipped, not
        // sent to create_texture (a validation error there aborts the process).
        // Reachable from ordinary input: total bytes stay tiny while a side blows
        // past the limit.
        let mut r = Renderer::headless(Theme::default());
        let over = r.gpu.device.limits().max_texture_dimension_2d + 1;
        let rgba = vec![0xFFu8; (over as usize) * 4]; // over x 1 RGBA
        r.upload_image(5, over, 1, &rgba);
        assert!(
            !r.image_bind_groups.contains_key(&5),
            "an oversize image is not cached"
        );
        // A placement of that id then draws nothing rather than crashing.
        let mut v = Vt::new(20, 4);
        v.feed_str("\x1b_Gi=5,a=T,f=24,s=2,v=1,c=2,r=1;/wAAAP8A\x1b\\");
        let f = layout_frame(&v, TM);
        let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
        let _ = r.render_offscreen(&f, font, SIZE_PX);
        assert!(
            r.image_draws.is_empty(),
            "the oversize image produces no draw"
        );
    }

    fn render_text(s: &str) -> Rendered {
        let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
        let f = frame(20, 2, s);
        Renderer::headless(Theme::default()).render_offscreen(&f, font, SIZE_PX)
    }

    #[test]
    fn underline_draws_a_red_line_below_the_glyph() {
        // Red 'E' with and without SGR 4. Below the baseline (~14.4 at an 18px
        // line) the glyph has no ink, so any red there is the underline.
        let plain = render_text("\x1b[31mE");
        let under = render_text("\x1b[4;31mE");
        let lower_red = |img: &Rendered| (0..9).any(|x| (15..18).any(|y| is_red(px(img, x, y))));
        assert!(
            !lower_red(&plain),
            "plain E paints no ink below its baseline"
        );
        assert!(
            lower_red(&under),
            "SGR 4 paints a red underline below the glyph"
        );
    }

    #[test]
    fn strikethrough_draws_a_red_line_through_mid_cell() {
        // A leading red SPACE (kept from trimming by the following 'X'); the
        // space has no glyph ink, so mid-cell red is the strikethrough rule.
        let plain = render_text("\x1b[31m X");
        let strike = render_text("\x1b[9;31m X");
        let mid_red = |img: &Rendered| (0..9).any(|x| (8..11).any(|y| is_red(px(img, x, y))));
        assert!(!mid_red(&plain), "a plain space cell has no mid-cell ink");
        assert!(mid_red(&strike), "SGR 9 paints a red rule through the cell");
    }

    #[test]
    fn bold_renders_a_heavier_glyph() {
        // SGR 1 must route the cell through the emboldened raster. With the
        // default foreground (which the color-brighten path leaves untouched),
        // any pixel difference proves the heavier strokes are actually drawn.
        let roman = render_text("W");
        let bold = render_text("\x1b[1mW");
        assert_ne!(
            roman.rgba, bold.rgba,
            "a bold cell must render heavier strokes, not the regular glyph"
        );
    }

    #[test]
    fn italic_renders_a_sheared_glyph() {
        // SGR 3 must route the cell through the faux-oblique raster: the same
        // letter renders to different pixels upright vs italic. 'W' has plenty of
        // off-baseline ink, so the shear is unmistakable.
        let roman = render_text("W");
        let italic = render_text("\x1b[3mW");
        assert_ne!(
            roman.rgba, italic.rgba,
            "an italic cell must render a sheared glyph, not the upright one"
        );
    }

    #[test]
    fn single_terminal_scene_matches_render_offscreen() {
        let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
        let f = frame(20, 3, "hello\r\nworld\x1b[1;7mX");
        let (w, h) = Renderer::frame_size(&f);

        let direct = Renderer::headless(Theme::default()).render_offscreen(&f, font, SIZE_PX);

        let scene = Scene {
            size_px: (w, h),
            layers: vec![Layer {
                z: 0,
                items: vec![SceneItem::Terminal {
                    id: SceneId::Root,
                    rect: RectPx {
                        x: 0.0,
                        y: 0.0,
                        w: w as f32,
                        h: h as f32,
                    },
                    frame: f.clone(),
                    selection: None,
                    dim: false,
                }],
            }],
        };
        let via_scene =
            Renderer::headless(Theme::default()).render_offscreen_scene(&scene, font, SIZE_PX);

        assert_eq!(
            (direct.width, direct.height),
            (via_scene.width, via_scene.height)
        );
        assert_eq!(
            direct.rgba, via_scene.rgba,
            "a single full-window Terminal scene must be byte-identical to render_offscreen"
        );
    }

    #[test]
    fn scissor_clips_terminal_to_its_rect() {
        let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
        // A full red-background row ~180px wide.
        let f = frame(20, 1, "\x1b[41mXXXXXXXXXXXXXXXXXXXX");
        let mut r = Renderer::headless(Theme::default());
        let scene = Scene {
            size_px: (200, 40),
            layers: vec![Layer {
                z: 0,
                items: vec![SceneItem::Terminal {
                    id: SceneId::Tile(1),
                    rect: RectPx {
                        x: 0.0,
                        y: 0.0,
                        w: 50.0, // clip the 180px of content to the left 50px
                        h: 40.0,
                    },
                    frame: f,
                    selection: None,
                    dim: false,
                }],
            }],
        };
        let img = r.render_offscreen_scene(&scene, font, SIZE_PX);

        let red_inside = (0..50).any(|x| (0..40).any(|y| is_red(px(&img, x, y))));
        assert!(
            red_inside,
            "the tile must render its red background inside its rect"
        );
        let red_outside = (60..200).any(|x| (0..40).any(|y| is_red(px(&img, x, y))));
        assert!(
            !red_outside,
            "red must not bleed past the tile's scissor rect (x >= 60)"
        );
    }

    #[test]
    fn empty_scene_does_not_panic_and_matches_render_offscreen() {
        let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
        // A blank screen with the cursor hidden: no runs, no cursor => zero
        // instances. Slicing a zero-size vertex buffer used to panic here.
        let f = frame(20, 3, "\x1b[?25l");
        let (w, h) = Renderer::frame_size(&f);
        let direct = Renderer::headless(Theme::default()).render_offscreen(&f, font, SIZE_PX);
        let scene = Scene {
            size_px: (w, h),
            layers: vec![Layer {
                z: 0,
                items: vec![SceneItem::Terminal {
                    id: SceneId::Root,
                    rect: RectPx {
                        x: 0.0,
                        y: 0.0,
                        w: w as f32,
                        h: h as f32,
                    },
                    frame: f.clone(),
                    selection: None,
                    dim: false,
                }],
            }],
        };
        let via_scene =
            Renderer::headless(Theme::default()).render_offscreen_scene(&scene, font, SIZE_PX);
        assert_eq!(
            direct.rgba, via_scene.rgba,
            "empty scene == cleared background"
        );
    }

    #[test]
    fn translucent_border_corners_match_edges() {
        let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
        let mut r = Renderer::headless(Theme::default());
        let scene = Scene {
            size_px: (60, 60),
            layers: vec![Layer {
                z: 0,
                items: vec![SceneItem::Border {
                    id: SceneId::Tile(1),
                    rect: RectPx {
                        x: 0.0,
                        y: 0.0,
                        w: 60.0,
                        h: 60.0,
                    },
                    color: [1.0, 0.0, 0.0, 0.5], // translucent red
                    width: 6.0,
                }],
            }],
        };
        let img = r.render_offscreen_scene(&scene, font, SIZE_PX);
        // A corner pixel and a top-edge-midpoint pixel must blend identically:
        // with overlapping quads the corner would be drawn (and darkened) twice.
        assert_eq!(
            px(&img, 1, 1),
            px(&img, 30, 1),
            "corner and edge must blend the same (no double-coverage)"
        );
    }

    #[test]
    fn scene_draws_a_solid_rect() {
        let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
        let mut r = Renderer::headless(Theme::default());
        let scene = Scene {
            size_px: (40, 40),
            layers: vec![Layer {
                z: 0,
                items: vec![SceneItem::Rect {
                    id: SceneId::Sidebar,
                    rect: RectPx {
                        x: 10.0,
                        y: 10.0,
                        w: 20.0,
                        h: 20.0,
                    },
                    color: [0.0, 1.0, 0.0, 1.0], // opaque green
                    radius: 0.0,
                }],
            }],
        };
        let img = r.render_offscreen_scene(&scene, font, SIZE_PX);
        assert_eq!(px(&img, 20, 20), [0, 255, 0, 255], "rect interior is green");
        assert_eq!(
            px(&img, 2, 2),
            [0x10, 0x10, 0x12, 255],
            "outside the rect is the clear background"
        );
    }

    #[test]
    fn theme_palette_recolors_ansi_indices() {
        let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
        // Leading spaces (kept by the trailing 'X') with ANSI bg index 4 (blue).
        let f = frame(10, 1, "\x1b[44m  X");
        let mut theme = Theme::default();
        theme.palette[4] = [0x00, 0xff, 0x00]; // remap "blue" to green
        let img = Renderer::headless(theme).render_offscreen(&f, font, SIZE_PX);
        // The background of the first cell now paints the palette's green.
        let p = px(&img, 2, 9);
        assert!(
            p[1] > 0x80 && p[2] < 0x40,
            "ANSI index 4 must resolve through the theme palette, got {p:?}"
        );
    }

    #[test]
    fn index_to_rgb_matches_xterm() {
        assert_eq!(index_to_rgb(1), [0x80, 0x00, 0x00]); // ANSI red
        assert_eq!(index_to_rgb(9), [0xff, 0x00, 0x00]); // bright red
        assert_eq!(index_to_rgb(16), [0, 0, 0]); // cube origin
        assert_eq!(index_to_rgb(196), [0xff, 0, 0]); // cube pure red (5,0,0)
        assert_eq!(index_to_rgb(231), [0xff, 0xff, 0xff]); // cube white
        assert_eq!(index_to_rgb(232), [8, 8, 8]); // grayscale start
        assert_eq!(index_to_rgb(255), [238, 238, 238]); // grayscale end
    }

    #[test]
    fn cell_starts_snaps_glyphs_to_the_grid() {
        // ASCII: one cell per char.
        let m = cell_starts("ab");
        assert_eq!(m.get(&0), Some(&0));
        assert_eq!(m.get(&1), Some(&1));

        // Wide char occupies two columns: 'a'@b0->col0, '世'@b1(3 bytes)->col1,
        // 'b'@b4->col3 (skips the wide char's second column).
        let m = cell_starts("a世b");
        assert_eq!(m.get(&0), Some(&0));
        assert_eq!(m.get(&1), Some(&1));
        assert_eq!(m.get(&4), Some(&3));
    }
}
