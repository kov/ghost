//! GPU renderer (wgpu) for `ghost-render` frames.
//!
//! A persistent [`Renderer`] owns the device, pipeline, glyph atlas and glyph
//! cache, and can draw a laid-out [`Frame`] either to an offscreen target (for
//! deterministic, windowless golden tests on a software adapter) or into a
//! window surface view. Cell backgrounds, the block cursor, and full ANSI/256
//! color resolution are handled here; glyph shaping (with ligatures) comes from
//! `ghost-shaper`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{BuildHasherDefault, Hasher};
use std::rc::Rc;

pub mod target;
pub use target::{SurfaceTarget, Target};

use ghost_render::{
    BadgeKind, CellMetrics, CursorShape, Frame, Layer, RectPx, Run, Scene, SceneId, SceneItem,
    Selection, Style, Transform,
};
use ghost_shaper::{FontRef, ShapedGlyph, Synthesis};
use ghost_term::Color;
use unicode_width::UnicodeWidthChar;
use wgpu::util::DeviceExt;

/// A fast, non-cryptographic hasher (the well-known "FxHash" — rotate-xor-multiply)
/// for the renderer's hot internal caches, whose keys are small integers/tuples and
/// short strings looked up thousands of times per frame. The default `SipHash` showed
/// up at a few percent of frame time in profiling; these maps are process-local and
/// never exposed, so HashDoS resistance is irrelevant. Vendored (it's ~15 lines)
/// rather than depending on `rustc-hash`, which is only in the tree at an old 1.x.
#[derive(Default)]
struct FxHasher {
    hash: u64,
}

impl FxHasher {
    const K: u64 = 0x51_7c_c1_b7_27_22_0a_95;

    #[inline]
    fn add(&mut self, i: u64) {
        self.hash = (self.hash.rotate_left(5) ^ i).wrapping_mul(Self::K);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, mut bytes: &[u8]) {
        while bytes.len() >= 8 {
            let mut b = [0u8; 8];
            b.copy_from_slice(&bytes[..8]);
            self.add(u64::from_le_bytes(b));
            bytes = &bytes[8..];
        }
        if bytes.len() >= 4 {
            let mut b = [0u8; 4];
            b.copy_from_slice(&bytes[..4]);
            self.add(u32::from_le_bytes(b) as u64);
            bytes = &bytes[4..];
        }
        for &b in bytes {
            self.add(b as u64);
        }
    }
    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(i as u64);
    }
    #[inline]
    fn write_u16(&mut self, i: u16) {
        self.add(i as u64);
    }
    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add(i as u64);
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

type FastMap<K, V> = HashMap<K, V, BuildHasherDefault<FxHasher>>;

/// An RGBA8 image read back from the GPU, tightly packed (`width * 4` per row).
pub struct Rendered {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl Rendered {
    /// Write the pixels to a PNG file — the first-class way to eyeball what the
    /// renderer actually drew, from a golden test or a headless debug tool.
    pub fn save_png(&self, path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
        let file = std::fs::File::create(path)?;
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), self.width, self.height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().map_err(std::io::Error::other)?;
        writer
            .write_image_data(&self.rgba)
            .map_err(std::io::Error::other)?;
        Ok(())
    }
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
/// Fill `buf[byte] = starting column` for every byte of `text`, so a shaped glyph's
/// cluster (a byte offset into the run) maps to its grid column with a plain index —
/// no per-run `HashMap`. `buf` is reused across runs, so it allocates only to grow.
/// A wide char fills both of its byte..byte+len slots with its (single) start column;
/// the next char's column then jumps by the width, matching the fixed terminal grid.
fn fill_cell_cols(buf: &mut Vec<u16>, text: &str) {
    buf.clear();
    buf.resize(text.len(), 0);
    let mut col: u16 = 0;
    for (byte, ch) in text.char_indices() {
        let end = byte + ch.len_utf8();
        for slot in &mut buf[byte..end] {
            *slot = col;
        }
        col = col.saturating_add(UnicodeWidthChar::width(ch).unwrap_or(1).max(1) as u16);
    }
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

// Blit variant for cached previews: the bound texture was rendered with the glyph
// pipeline, so it is *already* premultiplied — composite it as-is (scaled by the
// instance alpha, which stays valid premultiplied) rather than premultiplying again.
@fragment
fn fs_blit(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(atlas, samp, in.uv) * in.color.a;
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
/// One unit of the scene's main render pass, in painter order. Glyph/solid
/// content draws from the shared instance buffer; a preview blits its cached
/// texture. Walking these in order (rather than all glyphs then all images) keeps
/// z-order correct — e.g. the take-over overlay and focus border, emitted after a
/// tile, draw over its preview.
enum Draw {
    /// A run of glyph/solid instances (chrome, the single view, dim overlays).
    Glyphs {
        scissor: [u32; 4],
        range: std::ops::Range<u32>,
    },
    /// Blit cached preview `id`'s texture (quad `quad` of the preview buffer).
    Preview {
        scissor: [u32; 4],
        quad: u32,
        id: SceneId,
    },
}

/// A fleet tile's preview rendered once to its own texture, then blitted on every
/// repaint until its content or size changes. Caching the pixels means navigation
/// (and unchanged tiles while another is busy) re-rasterizes nothing — the win
/// that matters on a software rasterizer.
struct CachedPreview {
    /// Kept alive for the bind group that samples it.
    _texture: wgpu::Texture,
    /// Samples `_texture` in the main pass (uniform + view + sampler).
    bind_group: wgpu::BindGroup,
    /// The content this texture was rendered from; a different frame re-renders.
    frame: Frame,
    /// Texture (= tile) pixel size; a resize/relayout re-renders.
    size: (u32, u32),
}

/// The last fully-rendered scene, captured to a texture so an interactive resize
/// can stretch-blit it to the changing surface instead of re-laying-out and
/// re-rasterizing the whole window at every drag step — ruinous in the fleet view
/// on a software rasterizer. Captured at the gesture's first resize event and
/// dropped when it settles and a crisp scene is rendered.
struct Snapshot {
    /// Kept alive for the bind group that samples it.
    _texture: wgpu::Texture,
    /// Samples `_texture` with the linear sampler, for a smooth stretch.
    bind_group: wgpu::BindGroup,
    /// The captured pixel size (the surface size at capture time).
    size: (u32, u32),
}

/// Our own full-window render target, composited onto the swapchain each present.
/// A banded (partial) redraw is only valid against a target whose previous contents
/// survive — and Vulkan leaves an *acquired swapchain image*'s contents UNDEFINED,
/// so painting a partial frame straight onto it loses everything outside the band on
/// drivers that don't happen to preserve it. We own this texture and never recycle
/// it, so `LoadOp::Load` here always sees the last complete frame; each present
/// renders the scene (full or banded) into it, then blits the WHOLE texture onto the
/// acquired image, so the displayed frame is always complete regardless of what the
/// swapchain handed back.
struct Backbuffer {
    w: u32,
    h: u32,
    texture: wgpu::Texture,
    /// Samples `texture` (nearest, 1:1) for the composite blit.
    bind_group: wgpu::BindGroup,
}

/// Per-row instance offsets for a steady single view, so a damaged (banded)
/// redraw can draw only the band's rows instead of the whole instance buffer.
/// llvmpipe processes every *drawn* instance's vertices at submit time regardless
/// of the scissor (the scissor only culls at the raster stage), so at 4K the win
/// is culling the draw itself — not just the rasterization.
///
/// Each `Vec` has `rows + 1` entries: `seg[r]..seg[r + 1]` is row `r`'s slice of
/// that segment, as ABSOLUTE indices into the scene instance buffer. The buffer
/// concatenates the three segments — backgrounds, selection tints, glyphs — in that
/// painter order, and these offsets already fold in each segment's start, so the
/// three ranges drawn in order reproduce the global order exactly. `origin_y` /
/// `line_height` map a band's pixel span back to row indices.
struct RowIndex {
    line_height: f32,
    origin_y: f32,
    bg: Vec<u32>,
    sel: Vec<u32>,
    glyph: Vec<u32>,
}

/// Translate every instance's screen rect by `(dx, dy)`.
fn translate(insts: &mut [Instance], dx: f32, dy: f32) {
    for i in insts {
        i.rect[0] += dx;
        i.rect[1] += dy;
    }
}

/// Scale instance geometry (position and size) about the origin. Used to shrink
/// a real-size preview frame so it fits a smaller tile before translating it.
fn scale_instances(insts: &mut [Instance], s: f32) {
    for i in insts {
        for v in &mut i.rect {
            *v *= s;
        }
    }
}

/// Bring a layer's emitted geometry into screen space and fade it as one unit:
/// scale + translate every instance/blit/image rect by the camera, and multiply
/// each instance's alpha by the layer opacity. Identity + full opacity is a no-op
/// (the common case), so untransformed layers pay nothing. Textured previews and
/// images aren't faded — the spatial-nav camera zooms tiles, it doesn't dissolve
/// them; only instance-based chrome (rects/text/borders/badges) carries opacity.
fn apply_layer(
    t: Transform,
    opacity: f32,
    insts: &mut [Instance],
    blits: &mut [RectPx],
    images: &mut [ImageDraw],
) {
    let identity = t == Transform::IDENTITY;
    if identity && opacity >= 1.0 {
        return;
    }
    for inst in insts.iter_mut() {
        if !identity {
            inst.rect[0] = inst.rect[0] * t.scale + t.tx;
            inst.rect[1] = inst.rect[1] * t.scale + t.ty;
            inst.rect[2] *= t.scale;
            inst.rect[3] *= t.scale;
        }
        inst.color[3] *= opacity;
    }
    if !identity {
        for r in blits.iter_mut() {
            *r = t.apply_rect(*r);
        }
        for im in images.iter_mut() {
            let r = t.apply_rect(RectPx {
                x: im.rect[0],
                y: im.rect[1],
                w: im.rect[2],
                h: im.rect[3],
            });
            im.rect = [r.x, r.y, r.w, r.h];
        }
    }
}

/// The camera-independent rect to size a tile's preview texture from. A dive zooms
/// a tile by animating its layer's camera every frame; sizing the texture to the
/// live *on-screen* rect re-renders it on every frame (O(tiles) texture rebuilds
/// per frame — the cost that made the dive sluggish with more than one live
/// preview). Instead, size it once: the tile's world `rect` scaled up uniformly
/// until the frame lands at its native resolution inside it — the most detail any
/// zoom can show (a larger on-screen rect is [`Renderer::preview_size`]-capped to
/// native anyway). The blit quad still carries the live camera, so the tile
/// visually zooms; only the texture is stable, so a whole dive (re)rasterizes each
/// preview at most once. Uniform scaling preserves the world rect's aspect, so the
/// contain-fit/letterbox the blit reproduces is exactly the steady single-view's.
fn preview_source_rect(frame: &Frame, rect: RectPx) -> RectPx {
    let nw = frame.cols as f32 * frame.metrics.advance;
    let nh = frame.rows as f32 * frame.metrics.line_height;
    let k = (nw / rect.w.max(1.0)).max(nh / rect.h.max(1.0));
    RectPx {
        w: rect.w * k,
        h: rect.h * k,
        ..rect
    }
}

/// "Contain" downscale to fit `frame`'s true pixel size inside `rect`, clamped
/// to 1.0 so a frame no larger than its rect (e.g. the full-window single view)
/// is never magnified. Returns 1.0 for a degenerate (zero-size) frame.
fn preview_scale(frame: &Frame, rect: RectPx) -> f32 {
    let fw = frame.cols as f32 * frame.metrics.advance;
    let fh = frame.rows as f32 * frame.metrics.line_height;
    if fw <= 0.0 || fh <= 0.0 {
        return 1.0;
    }
    (rect.w / fw).min(rect.h / fh).min(1.0)
}

/// Translucent black overlaid on an unfocused preview tile to dim it. The alpha
/// matches [`dim_colors`]' 0.55 RGB factor (a `1.0 - 0.55` black "over"); colors
/// are straight-alpha here (the fragment shader premultiplies).
const DIM_OVERLAY: [f32; 4] = [0.0, 0.0, 0.0, 0.45];

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

/// The overlap of two `[x, y, w, h]` scissor rects (empty `w`/`h` ⇒ no overlap).
/// A damaged redraw clips every scene draw to the damage band with this.
fn intersect_scissor(a: [u32; 4], b: [u32; 4]) -> [u32; 4] {
    let x0 = a[0].max(b[0]);
    let y0 = a[1].max(b[1]);
    let x1 = (a[0] + a[2]).min(b[0] + b[2]);
    let y1 = (a[1] + a[3]).min(b[1] + b[3]);
    [x0, y0, x1.saturating_sub(x0), y1.saturating_sub(y0)]
}

/// What changed between the last presented scene and the next one — the verdict a
/// window uses to skip, partially redraw, or fully redraw.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Damage {
    /// Identical to the last presented frame — skip the whole acquire/draw/present
    /// cycle, leaving that frame untouched on screen.
    None,
    /// Redraw the entire surface (no prior frame, a structural change, an animation,
    /// or the accumulated change covers ~the whole window).
    Full,
    /// Redraw only this pixel band; the rest of the surface is preserved as-is.
    Band(RectPx),
}

/// A safe upper bound on the surface swapchain length (Fifo at
/// `desired_maximum_frame_latency = 2` is typically 2–3 images; this leaves margin
/// for compositors that report a higher minimum). A just-acquired image was last
/// presented up to its swapchain-length frames ago, so a partial redraw repaints
/// everything that changed across the last `SWAPCHAIN_DEPTH` presents — the union —
/// so whichever image we land on comes out whole. Over-estimating only over-draws a
/// few extra rows; under-estimating would strand stale ones, so err high.
const SWAPCHAIN_DEPTH: usize = 6;

/// Remembers the last scene a window actually drew, so an identical redraw request
/// (a query-only PTY feed, a cursor-blink tick that changed nothing, a fleet tick
/// where no tile moved) can be skipped before the GPU surface is even acquired, and
/// so a steady single view that changed only a row or two can be redrawn partially
/// instead of repainting the whole (at 4K, expensive) surface.
///
/// Kept free of any GPU handle so the decision is unit-testable without a device.
#[derive(Default)]
pub struct SceneCache {
    last: Option<(Scene, f32)>,
    /// Damage bands of the last few PRESENTED frames, unioned to cover a stale
    /// swapchain image (see [`SWAPCHAIN_DEPTH`]).
    history: VecDeque<RectPx>,
}

impl SceneCache {
    /// The [`Damage`] between the last presented scene and `(scene, font_px)`. On a
    /// real change it records the new scene as last and (for a partial) folds this
    /// frame's band into the swapchain-depth window, returning the union to redraw.
    /// The structural comparison is exact `PartialEq` — never a hash — so `None` can
    /// never be a false positive that strands a stale frame.
    pub fn damage(&mut self, scene: &Scene, font_px: f32) -> Damage {
        let raw = match &self.last {
            Some((s, px)) if *px == font_px => scene_damage(s, scene),
            _ => RawDamage::Full,
        };
        match raw {
            RawDamage::Identical => Damage::None,
            RawDamage::Full => {
                self.last = Some((scene.clone(), font_px));
                // A full redraw fixes only the image we land on; the other swapchain
                // images are still stale, so keep forcing full for the next
                // SWAPCHAIN_DEPTH presents (the window-sized band ages out of the
                // union after that).
                self.history.clear();
                self.history.push_back(full_band(scene));
                Damage::Full
            }
            RawDamage::Band(b) => {
                self.last = Some((scene.clone(), font_px));
                self.history.push_back(b);
                while self.history.len() > SWAPCHAIN_DEPTH {
                    self.history.pop_front();
                }
                let union = self.history.iter().copied().reduce(union_rect).unwrap_or(b);
                let (sw, sh) = (scene.size_px.0 as f32, scene.size_px.1 as f32);
                if union.x <= 0.0 && union.y <= 0.0 && union.w >= sw && union.h >= sh {
                    Damage::Full
                } else {
                    Damage::Band(union)
                }
            }
        }
    }

    /// Forget the last scene (and damage window) so the next [`damage`](Self::damage)
    /// is `Full`. The caller invalidates whenever it accepted a scene here but then
    /// failed to actually present it (surface lost/outdated and reconfigured), so the
    /// recorded scene never gets ahead of what is really on screen, and so the freshly
    /// reconfigured swapchain (all images undefined) is fully repainted.
    pub fn invalidate(&mut self) {
        self.last = None;
        self.history.clear();
    }
}

/// Raw per-frame damage before the swapchain-depth union is applied.
enum RawDamage {
    Identical,
    Full,
    Band(RectPx),
}

/// A band covering the whole window (a `Full` redraw expressed as a rect).
fn full_band(scene: &Scene) -> RectPx {
    RectPx {
        x: 0.0,
        y: 0.0,
        w: scene.size_px.0 as f32,
        h: scene.size_px.1 as f32,
    }
}

/// The smallest rect covering both `a` and `b`.
fn union_rect(a: RectPx, b: RectPx) -> RectPx {
    let x0 = a.x.min(b.x);
    let y0 = a.y.min(b.y);
    let x1 = (a.x + a.w).max(b.x + b.w);
    let y1 = (a.y + a.h).max(b.y + b.h);
    RectPx {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
    }
}

/// Damage between two scenes, BEFORE the swapchain-depth union. Only the steady
/// single view qualifies for a partial band: scenes of the same size whose layers
/// are all identity-transformed, fully opaque, and contain only `Terminal` items
/// (so there is no chrome a band repaint could erase), differing solely in those
/// terminals' row content (same id/rect/selection/dim/grid/metrics, no images). The
/// band is the changed rows ∪ the cursor's old/new rows. Anything else ⇒ `Full`.
fn scene_damage(prev: &Scene, new: &Scene) -> RawDamage {
    if prev == new {
        return RawDamage::Identical;
    }
    if prev.size_px != new.size_px || prev.layers.len() != new.layers.len() {
        return RawDamage::Full;
    }
    let mut band: Option<RectPx> = None;
    for (lp, ln) in prev.layers.iter().zip(&new.layers) {
        if lp == ln {
            continue;
        }
        if lp.z != ln.z
            || lp.opacity != 1.0
            || ln.opacity != 1.0
            || lp.transform != Transform::IDENTITY
            || ln.transform != Transform::IDENTITY
            || lp.items.len() != ln.items.len()
            || lp
                .items
                .iter()
                .chain(&ln.items)
                .any(|it| !matches!(it, SceneItem::Terminal { .. }))
        {
            return RawDamage::Full;
        }
        for (ip, in_) in lp.items.iter().zip(&ln.items) {
            if ip == in_ {
                continue;
            }
            let (
                SceneItem::Terminal {
                    id: i1,
                    rect: r1,
                    frame: f1,
                    selection: s1,
                    dim: d1,
                },
                SceneItem::Terminal {
                    id: i2,
                    rect: r2,
                    frame: f2,
                    selection: s2,
                    dim: d2,
                },
            ) = (ip, in_)
            else {
                return RawDamage::Full;
            };
            if i1 != i2
                || r1 != r2
                || s1 != s2
                || d1 != d2
                || f1.cols != f2.cols
                || f1.rows != f2.rows
                || f1.metrics != f2.metrics
                || !f1.images.is_empty()
                || !f2.images.is_empty()
            {
                return RawDamage::Full;
            }
            let lh = f1.metrics.line_height;
            let row_band = |row: usize| RectPx {
                x: r1.x,
                y: r1.y + row as f32 * lh,
                w: r1.w,
                h: lh,
            };
            let rowcount = f1.rows_layout.len().max(f2.rows_layout.len());
            for row in 0..rowcount {
                if f1.rows_layout.get(row) != f2.rows_layout.get(row) {
                    let rb = row_band(row);
                    band = Some(band.map_or(rb, |b| union_rect(b, rb)));
                }
            }
            // A cursor shape change (or hide/show) need not alter rows_layout, so
            // fold in the cursor's old and new rows explicitly.
            if f1.cursor != f2.cursor {
                for cur in [f1.cursor.as_ref(), f2.cursor.as_ref()]
                    .into_iter()
                    .flatten()
                {
                    let rb = row_band(cur.row);
                    band = Some(band.map_or(rb, |b| union_rect(b, rb)));
                }
            }
        }
    }
    match band {
        Some(b) => RawDamage::Band(b),
        // The scenes differ but no localizable band emerged (shouldn't happen given
        // the equality short-circuit above) — repaint fully to stay correct.
        None => RawDamage::Full,
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
    cache: FastMap<(u16, u32, Synthesis), Option<Slot>>,
    /// Shaped-run cache keyed by (font key, font size bits, run text). Shaping
    /// dominates per-frame CPU, and a run's text is identical across redraws
    /// (navigation, unchanged tiles), so caching it makes a repaint nearly free.
    shape_cache: FastMap<(u64, u32, String), Rc<Vec<ShapedGlyph>>>,
    /// Count of actual shaping calls (cache misses) — never bumped on a hit. Lets
    /// a test prove a repaint of unchanged text re-shapes nothing.
    shape_misses: u32,
    // shelf-packing cursor into the atlas.
    pack_x: u32,
    pack_y: u32,
    shelf: u32,
    // per-frame instance buffer, reused across frames and grown only when a
    // frame needs more instances than it currently holds.
    instances: Option<wgpu::Buffer>,
    instance_count: u32,
    /// A single reused instance holding the background quad for a damaged (partial)
    /// redraw: when we skip the full clear and only redraw a band of changed rows
    /// (`LoadOp::Load`), this repaints the band's background first, since
    /// default-background cells emit no quad of their own.
    bg_fill: wgpu::Buffer,
    /// Reused target for [`Self::render_to_cached_target`] (headless present
    /// benchmarking) — kept across frames so its contents persist for `LoadOp::Load`.
    bench_target: Option<(u32, u32, wgpu::Texture)>,
    /// The owned full-window backbuffer (see [`Backbuffer`]); the partial redraw lands
    /// here and is then blitted whole onto the swapchain.
    backbuffer: Option<Backbuffer>,
    /// Whether the backbuffer currently holds the complete last-presented frame. A
    /// full redraw goes straight to the swapchain (covering every pixel, so it needs
    /// no backbuffer) and leaves this `false`; the next banded present then repaints
    /// the backbuffer fully before resuming cheap band updates.
    backbuffer_valid: bool,
    /// The fullscreen quad that blits the backbuffer onto the surface, reused.
    backbuffer_quad: Option<wgpu::Buffer>,
    /// Per-row instance offsets for the last prepared scene, when it was a steady
    /// single view (one identity-transformed full-window Terminal). `Some` lets a
    /// damaged redraw draw only the band's rows; `None` (chrome, fleet, multi-item)
    /// makes the banded path fall back to drawing all instances scissored.
    single_row_index: Option<RowIndex>,
    /// Instances drawn by the last damaged (banded) redraw — 0 for a full redraw.
    /// Lets a test prove the banded path draws only the band's rows, not the lot.
    band_instances: u32,
    /// Count of instance-buffer (re)allocations — bumped only when a buffer is
    /// created or grown, never on a reuse. Lets a test prove the reuse path.
    buffer_allocs: u32,
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
    /// Blit pipeline for cached preview textures — like `image_pipeline` but with
    /// the passthrough `fs_blit` (the texture is already premultiplied).
    preview_pipeline: wgpu::RenderPipeline,
    /// Cached preview textures by tile id (see [`CachedPreview`]).
    preview_cache: HashMap<SceneId, CachedPreview>,
    /// One blit quad per preview drawn this scene, parallel to the `Draw::Preview`
    /// quad indices.
    preview_instances: Option<wgpu::Buffer>,
    /// Count of preview textures actually (re)rendered — a cache miss. A test
    /// asserts an unchanged repaint re-renders none.
    preview_renders: u32,
    /// Uploaded image bind groups, keyed by image id; the blob is sent once and
    /// the bind group (which owns its texture) lives until eviction.
    image_bind_groups: HashMap<u32, wgpu::BindGroup>,
    /// Per-frame resolved image draws (z-sorted within a terminal) and the
    /// one-quad-per-draw instance buffer they index.
    image_draws: Vec<ImageDraw>,
    image_instances: Option<wgpu::Buffer>,
    /// Linear sampler for the resize snapshot blit, so stretching the last crisp
    /// frame while dragging looks smooth (the glyph/preview sampler is nearest).
    linear_sampler: wgpu::Sampler,
    /// The crisp scene captured for an in-flight interactive resize, if any (see
    /// [`Snapshot`]); stretch-blitted each drag step until the resize commits.
    snapshot: Option<Snapshot>,
    /// The single full-surface quad that blits `snapshot`, reused across steps.
    snapshot_instances: Option<wgpu::Buffer>,
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
        // Linear, for the resize snapshot blit: stretching the last crisp frame to
        // a different surface size reads smooth rather than blocky.
        let linear_sampler = gpu.device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("snapshot sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let uniform_buf = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("uniforms"),
            size: std::mem::size_of::<Uniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bg_fill = gpu.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("damage bg fill"),
            size: std::mem::size_of::<Instance>() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
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

        // The preview-blit pipeline: identical to the image pipeline but with the
        // passthrough fragment, since a cached preview texture is already
        // premultiplied (rendered through the glyph pipeline).
        let preview_pipeline = gpu
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("preview pipeline"),
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
                    entry_point: Some("fs_blit"),
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
            cache: FastMap::default(),
            shape_cache: FastMap::default(),
            shape_misses: 0,
            bg_fill,
            bench_target: None,
            backbuffer: None,
            backbuffer_valid: false,
            backbuffer_quad: None,
            single_row_index: None,
            band_instances: 0,
            pack_x: 1,
            pack_y: 0,
            shelf: 1,
            instances: None,
            instance_count: 0,
            buffer_allocs: 0,
            selection: None,
            bind_layout,
            sampler,
            image_pipeline,
            preview_pipeline,
            preview_cache: HashMap::new(),
            preview_instances: None,
            preview_renders: 0,
            image_bind_groups: HashMap::new(),
            image_draws: Vec::new(),
            image_instances: None,
            linear_sampler,
            snapshot: None,
            snapshot_instances: None,
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
        scale: f32,
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
                    ox + img.col as f32 * m.advance * scale,
                    oy + img.row as f32 * m.line_height * scale,
                    img.cols as f32 * m.advance * scale,
                    img.rows as f32 * m.line_height * scale,
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
        Self::upload_instances(
            &self.gpu.device,
            &self.gpu.queue,
            &mut self.image_instances,
            &mut self.buffer_allocs,
            "image instances",
            &insts,
        );
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

    /// Pixel size of a tile's preview texture for `rect`, which the caller has
    /// already sized via [`preview_source_rect`] (a camera-independent rect, not the
    /// live on-screen one), capped so the frame contain-fit inside never exceeds its
    /// native resolution (`cols×advance` by `rows×line_height`) — past 1:1 there's no
    /// more detail, and the cap bounds the texture's cost. The cap is UNIFORM so the
    /// rect's aspect is preserved: a per-axis cap would squash the preview and render
    /// the tile at a different aspect than the steady single view — a visible pop.
    fn preview_size(frame: &Frame, rect: RectPx) -> (u32, u32) {
        let nw = (frame.cols as f32 * frame.metrics.advance).max(1.0);
        let nh = (frame.rows as f32 * frame.metrics.line_height).max(1.0);
        // How much the frame would scale to contain-fit `rect`; if that would magnify
        // past native (>1), shrink the texture uniformly so the frame lands at 1:1.
        let contain = (rect.w / nw).min(rect.h / nh);
        let k = if contain > 1.0 { 1.0 / contain } else { 1.0 };
        (
            ((rect.w * k).ceil() as u32).max(1),
            ((rect.h * k).ceil() as u32).max(1),
        )
    }

    /// Ensure tile `id`'s preview texture is current for `frame` at `rect`,
    /// (re)rendering only on a content or size change. The main pass then blits
    /// the cached texture, so an unchanged tile re-rasterizes nothing — the win
    /// that keeps fleet navigation cheap on a software rasterizer.
    fn ensure_preview(
        &mut self,
        id: SceneId,
        frame: &Frame,
        rect: RectPx,
        font: FontRef,
        size_px: f32,
    ) {
        let size = Self::preview_size(frame, rect);
        if self
            .preview_cache
            .get(&id)
            .is_some_and(|c| c.size == size && c.frame == *frame)
        {
            return; // cache hit: blit the existing texture
        }
        let preview = self.render_preview_texture(frame, rect, font, size_px);
        self.preview_renders += 1;
        self.preview_cache.insert(id, preview);
    }

    /// Render `frame` (scaled to "contain" within `rect`) to its own texture and
    /// build the bind group that samples it. The shared viewport uniform is set to
    /// the texture size for this sub-pass; `prepare_scene` resets it to the scene
    /// size before the main pass, which always runs after every preview sub-pass.
    fn render_preview_texture(
        &mut self,
        frame: &Frame,
        rect: RectPx,
        font: FontRef,
        size_px: f32,
    ) -> CachedPreview {
        let size = Self::preview_size(frame, rect);
        let (tw, th) = size;

        // Full-size glyphs scaled to the texture (the same GPU minification the
        // inline preview used), left at the texture origin; the blit applies the
        // offset. `rect` is the camera-independent native-resolution source rect
        // (see `preview_source_rect`), so `preview_scale` lands the frame at 1:1 and
        // the texture holds crisp glyphs the blit then minifies to the on-screen tile.
        let (mut insts, _) =
            self.build_instances(frame, font, size_px, None, 0..frame.rows_layout.len());
        let s = preview_scale(frame, rect);
        if s < 1.0 {
            scale_instances(&mut insts, s);
        }

        let texture = self.gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("preview"),
            size: wgpu::Extent3d {
                width: tw,
                height: th,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        // Point the shared viewport uniform at this texture for the sub-pass.
        let uniforms = Uniforms {
            viewport: [tw as f32, th as f32],
            _pad: [0.0, 0.0],
        };
        self.gpu
            .queue
            .write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(&uniforms));

        let buf = (!insts.is_empty()).then(|| {
            self.gpu
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("preview instances"),
                    contents: bytemuck::cast_slice(&insts),
                    usage: wgpu::BufferUsages::VERTEX,
                })
        });

        let mut encoder = self
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("preview"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("preview"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        // Transparent clear: default-background cells emit no quad,
                        // so the card behind the blit shows through, matching the
                        // inline preview (which drew straight over the card).
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if let Some(buf) = &buf {
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &self.bind_group, &[]);
                pass.set_vertex_buffer(0, buf.slice(..));
                pass.draw(0..6, 0..insts.len() as u32);
            }
        }
        self.gpu.queue.submit([encoder.finish()]);

        let bind_group = self
            .gpu
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("preview bind group"),
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

        CachedPreview {
            _texture: texture,
            bind_group,
            frame: frame.clone(),
            size,
        }
    }

    /// Count of preview textures (re)rendered (cache misses). A test asserts an
    /// unchanged repaint re-renders none.
    pub fn preview_renders(&self) -> u32 {
        self.preview_renders
    }

    /// Shape `text` at `size_px`, caching the result. Shaping (swash GSUB/GPOS)
    /// is the dominant per-frame cost, and a run's text is identical across
    /// redraws, so the cache turns a repaint into a hash lookup + `Rc` clone.
    fn shape_cached(&mut self, font: FontRef, text: &str, size_px: f32) -> Rc<Vec<ShapedGlyph>> {
        // Bound long-term growth from scrollback churn; the working set (the
        // currently-visible runs) is tiny next to this, so we clear rarely.
        if self.shape_cache.len() > 8192 {
            self.shape_cache.clear();
        }
        let key = (font.key.value(), size_px.to_bits(), text.to_string());
        let mut missed = false;
        let rc = Rc::clone(self.shape_cache.entry(key).or_insert_with(|| {
            missed = true;
            Rc::new(ghost_shaper::shape(font, text, size_px))
        }));
        if missed {
            self.shape_misses += 1;
        }
        rc
    }

    /// Total shaping calls (cache misses) — see [`Self::shape_misses`](Self::shape_misses).
    pub fn shape_misses(&self) -> u32 {
        self.shape_misses
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
    /// Build the instances for `rows` of `frame` (a sub-range, or `0..rows_layout.len()`
    /// for the whole frame), in painter order (backgrounds, selection, glyphs). Each
    /// instance is positioned at its absolute frame-local cell, so a sub-range builds
    /// exactly the geometry it would have in a full build — the incremental banded path
    /// builds just the band's rows and relies on the backbuffer holding the rest. The
    /// returned [`RowIndex`] is indexed from `rows.start` (so a whole-frame build indexes
    /// it by absolute row, as the banded-draw fast path expects).
    fn build_instances(
        &mut self,
        frame: &Frame,
        font: FontRef,
        size_px: f32,
        selection: Option<Selection>,
        rows: std::ops::Range<usize>,
    ) -> (Vec<Instance>, RowIndex) {
        let metrics = frame.metrics;
        let baseline = metrics.line_height * 0.8;
        let cursor = frame.cursor;
        let n = frame.rows_layout.len();
        let rows = rows.start.min(n)..rows.end.min(n);
        let nrows = rows.len();

        let mut backgrounds: Vec<Instance> = Vec::new();
        let mut selection_rects: Vec<Instance> = Vec::new();
        let mut glyphs: Vec<Instance> = Vec::new();
        // Per-row offsets within each segment (`nrows + 1` entries each), so a damaged
        // redraw can draw just the band's rows. Backgrounds/glyphs are filled in the
        // main loop, selection here.
        let mut bg_pre: Vec<u32> = Vec::with_capacity(nrows + 1);
        let mut glyph_pre: Vec<u32> = Vec::with_capacity(nrows + 1);
        let mut sel_pre: Vec<u32> = vec![0; nrows + 1];

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
            for row in rows.clone() {
                sel_pre[row - rows.start] = selection_rects.len() as u32;
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
        sel_pre[nrows] = selection_rects.len() as u32;

        // Reused across runs so the per-run column lookup allocates only to grow.
        let mut col_of_byte: Vec<u16> = Vec::new();
        for row in rows.clone() {
            let layout = &frame.rows_layout[row];
            bg_pre.push(backgrounds.len() as u32);
            glyph_pre.push(glyphs.len() as u32);
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
                fill_cell_cols(&mut col_of_byte, &run.text);
                let shaped = self.shape_cached(font, &run.text, size_px);
                for g in shaped.iter() {
                    let cell = col_of_byte.get(g.cluster as usize).copied().unwrap_or(0) as usize;
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

        bg_pre.push(backgrounds.len() as u32);
        glyph_pre.push(glyphs.len() as u32);

        // Fold each segment's start into absolute offsets for the concatenated
        // buffer (backgrounds ++ selection ++ glyphs). `origin_y` is set by the
        // scene caller (which knows the terminal's on-screen rect).
        let n_bg = backgrounds.len() as u32;
        let n_sel = selection_rects.len() as u32;
        let ri = RowIndex {
            line_height: metrics.line_height,
            origin_y: 0.0,
            bg: bg_pre,
            sel: sel_pre.iter().map(|&o| n_bg + o).collect(),
            glyph: glyph_pre.iter().map(|&o| n_bg + n_sel + o).collect(),
        };

        backgrounds.extend(selection_rects); // tint over cell backgrounds
        backgrounds.extend(glyphs); // glyphs stay crisp on top
        (backgrounds, ri)
    }

    /// Number of instance-buffer (re)allocations so far — see
    /// [`buffer_allocs`](Self::buffer_allocs).
    pub fn buffer_allocs(&self) -> u32 {
        self.buffer_allocs
    }

    /// Instances uploaded for the last prepared scene (the whole buffer).
    pub fn instance_count(&self) -> u32 {
        self.instance_count
    }

    /// Instances actually drawn by the last damaged (banded) redraw — see
    /// [`band_instances`](Self::band_instances).
    pub fn band_instances(&self) -> u32 {
        self.band_instances
    }

    /// Upload `data` into `*slot`, reusing the existing buffer when it is already
    /// large enough (a cheap `queue.write_buffer`) and only reallocating to grow.
    /// The buffer carries `COPY_DST` so it can be rewritten in place; callers draw
    /// exactly the instance count they uploaded, so any unused tail is ignored.
    /// Bumps `*allocs` only on a real (re)allocation.
    fn upload_instances(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        slot: &mut Option<wgpu::Buffer>,
        allocs: &mut u32,
        label: &str,
        data: &[Instance],
    ) {
        let bytes: &[u8] = bytemuck::cast_slice(data);
        match slot.as_ref() {
            Some(buf) if buf.size() as usize >= bytes.len() => {
                queue.write_buffer(buf, 0, bytes);
            }
            _ => {
                *allocs += 1;
                *slot = Some(
                    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some(label),
                        contents: bytes,
                        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    }),
                );
            }
        }
    }

    /// Prepare GPU state for one frame: pack glyphs, upload instances, set the
    /// viewport uniform.
    fn prepare(&mut self, frame: &Frame, font: FontRef, size_px: f32, vw: u32, vh: u32) {
        let (instances, _) = self.build_instances(
            frame,
            font,
            size_px,
            self.selection,
            0..frame.rows_layout.len(),
        );
        // This path (render_to_view / render_offscreen) never does a banded redraw.
        self.single_row_index = None;
        self.instance_count = instances.len() as u32;
        Self::upload_instances(
            &self.gpu.device,
            &self.gpu.queue,
            &mut self.instances,
            &mut self.buffer_allocs,
            "instances",
            &instances,
        );
        let uniforms = Uniforms {
            viewport: [vw as f32, vh as f32],
            _pad: [0.0, 0.0],
        };
        self.gpu
            .queue
            .write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(&uniforms));
        let mut imgs = Vec::new();
        self.collect_image_draws(frame, 0.0, 0.0, 1.0, [0, 0, vw, vh], &mut imgs);
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

    /// Build a scene's draws in painter order. Glyph/solid content accumulates in
    /// one instance list (returned, drawn by range); a scaled `Terminal` (a fleet
    /// preview) instead renders to a cached texture and emits a blit, whose tile
    /// rect is collected into `blits`. Layers are walked low `z` to high, items in
    /// insertion order within a layer. Terminals clip to their rect (so neighbours
    /// don't bleed); chrome may draw anywhere. The full-window single view
    /// (`preview_scale == 1.0`) still builds glyphs inline, byte-identical to before.
    fn build_scene(
        &mut self,
        scene: &Scene,
        font: FontRef,
        size_px: f32,
    ) -> (Vec<Instance>, Vec<Draw>, Vec<ImageDraw>, Vec<RectPx>) {
        let (sw, sh) = scene.size_px;
        // A lone identity-transformed full-window terminal is the only shape a
        // damaged (banded) redraw applies to; capture its per-row index so the band
        // can draw just its rows. Anything else leaves it None (full-scissored draw).
        let single_eligible = scene.layers.len() == 1
            && scene.layers[0].items.len() == 1
            && scene.layers[0].transform == Transform::IDENTITY;
        let mut single_ri: Option<RowIndex> = None;
        let mut all: Vec<Instance> = Vec::new();
        let mut draws: Vec<Draw> = Vec::new();
        let mut images: Vec<ImageDraw> = Vec::new();
        let mut blits: Vec<RectPx> = Vec::new();
        let mut seen: HashSet<SceneId> = HashSet::new();
        let mut order: Vec<&Layer> = scene.layers.iter().collect();
        order.sort_by_key(|l| l.z); // stable: keeps insertion order within a z

        // Push `insts` into `all` as one glyph draw under `scissor`, if non-empty.
        let push_glyphs =
            |all: &mut Vec<Instance>, draws: &mut Vec<Draw>, scissor, insts: Vec<Instance>| {
                let start = all.len() as u32;
                all.extend(insts);
                let end = all.len() as u32;
                if end > start {
                    draws.push(Draw::Glyphs {
                        scissor,
                        range: start..end,
                    });
                }
            };

        for layer in order {
            // Ranges this layer pushes into the shared buffers, so its camera +
            // opacity can be applied to exactly its own geometry afterwards.
            let (inst0, blit0, img0) = (all.len(), blits.len(), images.len());
            for item in &layer.items {
                let scissor = match item {
                    // A tile's scissor is computed in screen space (camera applied,
                    // then clamped) so clipping follows where the tile is drawn; the
                    // tile's geometry is brought into the same space below.
                    SceneItem::Terminal { rect, .. } => {
                        clamp_scissor(layer.transform.apply_rect(*rect), sw, sh)
                    }
                    _ => [0, 0, sw, sh],
                };
                match item {
                    SceneItem::Terminal {
                        id,
                        rect,
                        frame,
                        selection,
                        dim,
                    } => {
                        if preview_scale(frame, *rect) < 1.0 {
                            // Preview: blit a cached texture (re-rendered only when
                            // its content or size changes) instead of re-rasterizing
                            // the glyphs every frame. Selection isn't shown in the
                            // read-only previews, so the cache keys on frame alone.
                            // Size the texture to a camera-INDEPENDENT native-resolution
                            // rect, not the live on-screen rect: a dive animates the
                            // camera every frame, so on-screen sizing would re-render
                            // every tile every frame. The blit (below) carries the
                            // camera, so the tile still zooms; the texture stays put.
                            let src = preview_source_rect(frame, *rect);
                            self.ensure_preview(*id, frame, src, font, size_px);
                            seen.insert(*id);
                            draws.push(Draw::Preview {
                                scissor,
                                quad: blits.len() as u32,
                                id: *id,
                            });
                            blits.push(*rect);
                            if *dim {
                                // Darken the unfocused tile with a translucent
                                // overlay over the blit (matches `dim_colors`'
                                // 0.55 RGB factor: black at 1.0 - 0.55 alpha).
                                push_glyphs(
                                    &mut all,
                                    &mut draws,
                                    scissor,
                                    vec![solid(*rect, DIM_OVERLAY)],
                                );
                            }
                        } else {
                            // Full-window single view: glyphs inline, as before.
                            let (mut insts, mut ri) = self.build_instances(
                                frame,
                                font,
                                size_px,
                                *selection,
                                0..frame.rows_layout.len(),
                            );
                            translate(&mut insts, rect.x, rect.y);
                            if *dim {
                                dim_colors(&mut insts);
                            }
                            if single_eligible {
                                // `insts` lands at the start of `all` (the lone item),
                                // so its offsets are already absolute; only the band→row
                                // mapping needs the on-screen origin.
                                ri.origin_y = rect.y;
                                single_ri = Some(ri);
                            }
                            push_glyphs(&mut all, &mut draws, scissor, insts);
                            self.collect_image_draws(
                                frame,
                                rect.x,
                                rect.y,
                                1.0,
                                scissor,
                                &mut images,
                            );
                        }
                    }
                    SceneItem::Rect { rect, color, .. } => {
                        push_glyphs(&mut all, &mut draws, scissor, vec![solid(*rect, *color)])
                    }
                    SceneItem::Border {
                        rect, color, width, ..
                    } => {
                        let mut insts = Vec::new();
                        push_border(&mut insts, *rect, *color, *width);
                        push_glyphs(&mut all, &mut draws, scissor, insts);
                    }
                    SceneItem::Badge { rect, kind, .. } => push_glyphs(
                        &mut all,
                        &mut draws,
                        scissor,
                        vec![solid(*rect, badge_color(*kind))],
                    ),
                    SceneItem::Text {
                        rect,
                        runs,
                        metrics,
                        color,
                        ..
                    } => {
                        let t = self.text_instances(*rect, runs, *metrics, *color, font, size_px);
                        push_glyphs(&mut all, &mut draws, scissor, t);
                    }
                }
            }
            // Move/scale and fade everything this layer emitted as one unit.
            apply_layer(
                layer.transform,
                layer.opacity,
                &mut all[inst0..],
                &mut blits[blit0..],
                &mut images[img0..],
            );
        }
        // Drop textures for tiles no longer on screen, bounding cache memory.
        self.preview_cache.retain(|id, _| seen.contains(id));
        self.single_row_index = single_ri;
        (all, draws, images, blits)
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
        let mut col_of_byte: Vec<u16> = Vec::new();
        for run in runs {
            fill_cell_cols(&mut col_of_byte, &run.text);
            let shaped = self.shape_cached(font, &run.text, size_px);
            for g in shaped.iter() {
                let cell = col_of_byte.get(g.cluster as usize).copied().unwrap_or(0) as usize;
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

    /// One blit quad per preview, parallel to the `Draw::Preview` quad indices.
    fn build_preview_instances(&mut self, blits: &[RectPx]) {
        if blits.is_empty() {
            self.preview_instances = None;
            return;
        }
        let insts: Vec<Instance> = blits
            .iter()
            .map(|r| Instance {
                rect: [r.x, r.y, r.w, r.h],
                uv: [0.0, 0.0, 1.0, 1.0], // sample the whole preview texture
                color: [1.0, 1.0, 1.0, 1.0],
            })
            .collect();
        Self::upload_instances(
            &self.gpu.device,
            &self.gpu.queue,
            &mut self.preview_instances,
            &mut self.buffer_allocs,
            "preview blits",
            &insts,
        );
    }

    /// Upload a scene's instances and viewport uniform; return the ordered draws.
    /// `build_scene` runs every preview sub-pass first (each repointing the shared
    /// uniform), so the scene-size uniform write here must come last.
    fn prepare_scene(&mut self, scene: &Scene, font: FontRef, size_px: f32) -> Vec<Draw> {
        let (instances, draws, images, blits) = self.build_scene(scene, font, size_px);
        self.image_draws = images;
        self.build_image_instances();
        self.build_preview_instances(&blits);
        self.instance_count = instances.len() as u32;
        Self::upload_instances(
            &self.gpu.device,
            &self.gpu.queue,
            &mut self.instances,
            &mut self.buffer_allocs,
            "scene instances",
            &instances,
        );
        let (vw, vh) = scene.size_px;
        let uniforms = Uniforms {
            viewport: [vw as f32, vh as f32],
            _pad: [0.0, 0.0],
        };
        self.gpu
            .queue
            .write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(&uniforms));
        draws
    }

    /// Clear once, then walk `draws` in painter order, switching pipeline as each
    /// glyph run or preview blit requires (so chrome drawn after a tile — focus
    /// border, take-over overlay — composites over its preview). Kitty images draw
    /// last, as before.
    fn encode_scene(&self, view: &wgpu::TextureView, draws: &[Draw]) -> wgpu::CommandBuffer {
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
            // An empty scene (e.g. a blank screen, hidden cursor) emits no draws;
            // the clear above still yields the solid background, byte-identical to
            // `render_offscreen` on the same empty frame.
            for d in draws {
                match d {
                    Draw::Glyphs { scissor, range } => {
                        if scissor[2] == 0 || scissor[3] == 0 {
                            continue; // fully off-screen
                        }
                        let Some(buf) = &self.instances else { continue };
                        pass.set_pipeline(&self.pipeline);
                        pass.set_bind_group(0, &self.bind_group, &[]);
                        pass.set_vertex_buffer(0, buf.slice(..));
                        pass.set_scissor_rect(scissor[0], scissor[1], scissor[2], scissor[3]);
                        pass.draw(0..6, range.clone());
                    }
                    Draw::Preview { scissor, quad, id } => {
                        if scissor[2] == 0 || scissor[3] == 0 {
                            continue;
                        }
                        let (Some(buf), Some(cached)) =
                            (&self.preview_instances, self.preview_cache.get(id))
                        else {
                            continue; // texture evicted/missing: skip rather than err
                        };
                        pass.set_pipeline(&self.preview_pipeline);
                        pass.set_bind_group(0, &cached.bind_group, &[]);
                        pass.set_vertex_buffer(0, buf.slice(..));
                        pass.set_scissor_rect(scissor[0], scissor[1], scissor[2], scissor[3]);
                        pass.draw(0..6, *quad..*quad + 1);
                    }
                }
            }
            self.draw_images(&mut pass);
        }
        encoder.finish()
    }

    /// Like [`Self::encode_scene`] but for a DAMAGED partial redraw: preserve the
    /// attachment (`LoadOp::Load` — no full clear), repaint just the `band`'s
    /// background (the caller wrote `bg_fill` for it, since default-background cells
    /// emit no quad), then replay the scene's draws clipped to `band`. The unchanged
    /// rest of the surface keeps the pixels already there. The partial path is never
    /// taken when the frame has images (the caller falls back to a full redraw), so
    /// this skips the image pass.
    fn encode_scene_banded(
        &self,
        view: &wgpu::TextureView,
        draws: &[Draw],
        band: [u32; 4],
    ) -> wgpu::CommandBuffer {
        let mut encoder = self
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("scene (damaged)"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("scene (damaged)"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            // Repaint the band's background first (no clear ran).
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.set_vertex_buffer(0, self.bg_fill.slice(..));
            pass.set_scissor_rect(band[0], band[1], band[2], band[3]);
            pass.draw(0..6, 0..1);
            for d in draws {
                match d {
                    Draw::Glyphs { scissor, range } => {
                        let sc = intersect_scissor(*scissor, band);
                        if sc[2] == 0 || sc[3] == 0 {
                            continue;
                        }
                        let Some(buf) = &self.instances else { continue };
                        pass.set_pipeline(&self.pipeline);
                        pass.set_bind_group(0, &self.bind_group, &[]);
                        pass.set_vertex_buffer(0, buf.slice(..));
                        pass.set_scissor_rect(sc[0], sc[1], sc[2], sc[3]);
                        pass.draw(0..6, range.clone());
                    }
                    Draw::Preview { scissor, quad, id } => {
                        let sc = intersect_scissor(*scissor, band);
                        if sc[2] == 0 || sc[3] == 0 {
                            continue;
                        }
                        let (Some(buf), Some(cached)) =
                            (&self.preview_instances, self.preview_cache.get(id))
                        else {
                            continue;
                        };
                        pass.set_pipeline(&self.preview_pipeline);
                        pass.set_bind_group(0, &cached.bind_group, &[]);
                        pass.set_vertex_buffer(0, buf.slice(..));
                        pass.set_scissor_rect(sc[0], sc[1], sc[2], sc[3]);
                        pass.draw(0..6, *quad..*quad + 1);
                    }
                }
            }
        }
        encoder.finish()
    }

    /// The instance sub-ranges to draw for a damaged redraw of pixel band `band`
    /// (`[x, y, w, h]`), from the steady single view's per-row index: only the band's
    /// rows, expanded one row each way (a glyph — or a sub-pixel background sliver —
    /// from an adjacent row can reach into the band). Returned in painter order
    /// (backgrounds, selection, glyphs). `None` when no single-view index is held, so
    /// the caller falls back to drawing every instance scissored — always correct,
    /// just not the fast path.
    fn banded_row_ranges(&self, band: [u32; 4]) -> Option<Vec<std::ops::Range<u32>>> {
        let ri = self.single_row_index.as_ref()?;
        let rows = ri.bg.len().saturating_sub(1);
        if rows == 0 || ri.line_height <= 0.0 {
            return Some(Vec::new());
        }
        let lh = ri.line_height;
        let last = rows - 1;
        let top = ((band[1] as f32 - ri.origin_y) / lh).floor().max(0.0) as usize;
        let bot = (((band[1] + band[3]) as f32 - ri.origin_y) / lh).ceil() as usize;
        let a = top.min(last);
        let b = bot.saturating_sub(1).min(last);
        let lo = a.saturating_sub(1);
        let hi = (b + 1).min(last);
        let mut out = Vec::with_capacity(3);
        for seg in [&ri.bg, &ri.sel, &ri.glyph] {
            let (s, e) = (seg[lo], seg[hi + 1]);
            if e > s {
                out.push(s..e);
            }
        }
        Some(out)
    }

    /// Like [`Self::encode_scene_banded`] but driving the single view's per-row
    /// `ranges` directly: preserve the attachment (`LoadOp::Load`), repaint the band's
    /// background (default-bg cells emit no quad of their own), then draw only those
    /// ranges from the scene instance buffer — all under the one band scissor. No
    /// previews/images: the banded path is single-view only.
    fn encode_scene_banded_rows(
        &self,
        view: &wgpu::TextureView,
        ranges: &[std::ops::Range<u32>],
        band: [u32; 4],
    ) -> wgpu::CommandBuffer {
        let mut encoder = self
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("scene (damaged rows)"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("scene (damaged rows)"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.set_scissor_rect(band[0], band[1], band[2], band[3]);
            // Repaint the band's background first (no clear ran).
            pass.set_vertex_buffer(0, self.bg_fill.slice(..));
            pass.draw(0..6, 0..1);
            if let Some(buf) = &self.instances {
                pass.set_vertex_buffer(0, buf.slice(..));
                for r in ranges {
                    if !r.is_empty() {
                        pass.draw(0..6, r.clone());
                    }
                }
            }
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
        self.render_scene_to_view_damaged(view, scene, font, size_px, None);
    }

    /// Render a scene into a surface view, optionally as a DAMAGED partial redraw.
    /// `band = Some(rect)` redraws only that pixel band (preserving the rest via
    /// `LoadOp::Load`); `None` is a full clear-and-draw. The caller computes the band
    /// (see `SceneCache::damage`) and guarantees the view already holds the frames the
    /// band was diffed against. Used for the steady single view, where typically only
    /// a row or two changes between frames.
    pub fn render_scene_to_view_damaged(
        &mut self,
        view: &wgpu::TextureView,
        scene: &Scene,
        font: FontRef,
        size_px: f32,
        band: Option<RectPx>,
    ) {
        let groups = self.prepare_scene(scene, font, size_px);
        let cb = match band {
            Some(b) => {
                let (sw, sh) = scene.size_px;
                let scissor = clamp_scissor(b, sw, sh);
                if scissor[2] == 0 || scissor[3] == 0 {
                    self.band_instances = 0;
                    return; // nothing visible to redraw
                }
                // Background quad for the band, in the premultiplied clear color so it
                // matches `encode_scene`'s `LoadOp::Clear` exactly (opaque only — a
                // translucent bg would blend with the loaded pixels instead of
                // replacing, so the caller passes `None` for translucent themes).
                let a = self.theme.bg_alpha;
                let fill = solid(
                    b,
                    [
                        f32::from(self.theme.bg[0]) / 255.0 * a,
                        f32::from(self.theme.bg[1]) / 255.0 * a,
                        f32::from(self.theme.bg[2]) / 255.0 * a,
                        a,
                    ],
                );
                self.gpu
                    .queue
                    .write_buffer(&self.bg_fill, 0, bytemuck::bytes_of(&fill));
                match self.banded_row_ranges(scissor) {
                    // Fast path: draw only the band's rows from the single view's
                    // per-row index, not every instance scissored.
                    Some(ranges) => {
                        self.band_instances = ranges.iter().map(|r| r.len() as u32).sum();
                        self.encode_scene_banded_rows(view, &ranges, scissor)
                    }
                    // No single-view index (chrome/fleet shouldn't reach the banded
                    // path, but be safe): draw every instance, scissored to the band.
                    None => {
                        self.band_instances = self.instance_count;
                        self.encode_scene_banded(view, &groups, scissor)
                    }
                }
            }
            None => {
                self.band_instances = 0;
                self.encode_scene(view, &groups)
            }
        };
        self.gpu.queue.submit([cb]);
    }

    /// Render `scene` to an internal reused target (no per-frame allocation, no
    /// readback) and block until the GPU finishes — a windowless stand-in for the
    /// surface present path, for benchmarking the real cost of a full vs. damaged
    /// redraw. The target persists across calls so `LoadOp::Load` sees the prior
    /// frame, exactly like a swapchain image. `band` is as in
    /// [`Self::render_scene_to_view_damaged`].
    pub fn render_to_cached_target(
        &mut self,
        scene: &Scene,
        font: FontRef,
        size_px: f32,
        band: Option<RectPx>,
    ) {
        let (w, h) = (scene.size_px.0.max(1), scene.size_px.1.max(1));
        // Take the target out so the &mut self render call below doesn't alias it;
        // (re)allocate only when the size changed, then put it back.
        let (tw, th, tex) = self
            .bench_target
            .take()
            .filter(|(tw, th, _)| *tw == w && *th == h)
            .unwrap_or_else(|| (w, h, offscreen_target(&self.gpu.device, w, h, self.format)));
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        self.present_scene(&view, (w, h), scene, font, size_px, band);
        drop(view);
        let _ = self.gpu.device.poll(wgpu::PollType::wait_indefinitely());
        self.bench_target = Some((tw, th, tex));
    }

    /// (Re)create the owned [`Backbuffer`] when absent or resized, returning `true`
    /// if it was just created — its contents are then undefined, so the caller must
    /// redraw it fully (not banded) this frame. Sampled nearest at 1:1 for the blit.
    fn ensure_backbuffer(&mut self, w: u32, h: u32) -> bool {
        if self
            .backbuffer
            .as_ref()
            .is_some_and(|b| b.w == w && b.h == h)
        {
            return false;
        }
        let texture = self.gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("backbuffer"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = self
            .gpu
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("backbuffer bind group"),
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
        self.backbuffer = Some(Backbuffer {
            w,
            h,
            texture,
            bind_group,
        });
        true
    }

    /// Point the shared uniform at `w`×`h` and (re)build the fullscreen quad that
    /// blits the backbuffer onto the surface.
    fn prepare_blit_quad(&mut self, w: u32, h: u32) {
        let uniforms = Uniforms {
            viewport: [w as f32, h as f32],
            _pad: [0.0, 0.0],
        };
        self.gpu
            .queue
            .write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(&uniforms));
        let quad = [Instance {
            rect: [0.0, 0.0, w as f32, h as f32],
            uv: [0.0, 0.0, 1.0, 1.0],
            color: [1.0, 1.0, 1.0, 1.0],
        }];
        Self::upload_instances(
            &self.gpu.device,
            &self.gpu.queue,
            &mut self.backbuffer_quad,
            &mut self.buffer_allocs,
            "backbuffer blit",
            &quad,
        );
    }

    /// Clear `view` and blit the whole backbuffer over it. The backbuffer is
    /// premultiplied, so "over" a transparent clear reproduces it exactly (keeping a
    /// translucent theme see-through); every pixel is written, so the surface holds a
    /// complete frame regardless of the acquired image's prior contents.
    fn encode_backbuffer_blit(
        &self,
        view: &wgpu::TextureView,
        bb: &Backbuffer,
    ) -> wgpu::CommandBuffer {
        let mut encoder = self
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("backbuffer blit"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("backbuffer blit"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if let Some(buf) = &self.backbuffer_quad {
                pass.set_pipeline(&self.preview_pipeline);
                pass.set_bind_group(0, &bb.bind_group, &[]);
                pass.set_vertex_buffer(0, buf.slice(..));
                pass.draw(0..6, 0..1);
            }
        }
        encoder.finish()
    }

    /// Present `scene` onto `view` (the acquired swapchain image) at `surf` pixels.
    ///
    /// A full redraw (`band == None`) covers every pixel, so it goes STRAIGHT to the
    /// swapchain — it needs nothing of the image's prior contents, which Vulkan leaves
    /// undefined. A banded redraw, however, relies on the rest of the frame already
    /// being present; the swapchain can't guarantee that, so it is rendered into a
    /// backbuffer WE own (where `LoadOp::Load` always sees the last complete frame) and
    /// the whole backbuffer is then blitted onto the swapchain. This keeps the cheap
    /// banded raster for typing while keeping bulk output (all full redraws) blit-free,
    /// and is correct regardless of swapchain content persistence. The caller owns
    /// acquiring/presenting the surface texture.
    pub fn present_scene(
        &mut self,
        view: &wgpu::TextureView,
        surf: (u32, u32),
        scene: &Scene,
        font: FontRef,
        size_px: f32,
        band: Option<RectPx>,
    ) {
        let Some(b) = band else {
            // Full redraw: straight to the swapchain. The backbuffer (if any) is now
            // stale — the next banded present repaints it fully before banding.
            self.render_scene_to_view_damaged(view, scene, font, size_px, None);
            self.backbuffer_valid = false;
            return;
        };
        let (bw, bh) = (scene.size_px.0.max(1), scene.size_px.1.max(1));
        // The backbuffer must hold the complete current frame for the band to land on
        // top of it. If it was just (re)created, or a full redraw left it stale, repaint
        // it fully this frame; otherwise update just the band.
        let fresh = self.ensure_backbuffer(bw, bh);
        let bb_band = if fresh || !self.backbuffer_valid {
            None
        } else {
            Some(b)
        };
        // Take the backbuffer out so the `&mut self` render call doesn't alias it.
        let bb = self.backbuffer.take().expect("backbuffer ensured above");
        let bb_view = bb
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        match bb_band {
            // Refresh the whole backbuffer (it was just (re)created or left stale).
            None => self.render_scene_to_view_damaged(&bb_view, scene, font, size_px, None),
            // Steady band: build + upload + draw ONLY the band's rows into the
            // backbuffer, which already holds the rest.
            Some(bp) => self.render_band_incremental(&bb_view, scene, font, size_px, bp),
        }
        drop(bb_view);
        // Composite the whole backbuffer onto the surface (resets the uniform, which
        // the render above set to the scene size, to the surface size for the quad).
        self.prepare_blit_quad(surf.0, surf.1);
        let cb = self.encode_backbuffer_blit(view, &bb);
        self.gpu.queue.submit([cb]);
        self.backbuffer = Some(bb);
        self.backbuffer_valid = true;
    }

    /// Redraw just `band` into the backbuffer view, building + uploading ONLY the
    /// band's rows (±1 for spill) rather than the whole frame — the rest is already in
    /// the backbuffer. Byte-identical to a full build drawn band-restricted, since the
    /// band rows carry the same geometry; it just avoids rebuilding/re-uploading ~all
    /// the other rows. Falls back to a full build + band-restricted draw for any scene
    /// that isn't a lone identity-transformed terminal (the banded path's invariant, but
    /// be safe).
    fn render_band_incremental(
        &mut self,
        bb_view: &wgpu::TextureView,
        scene: &Scene,
        font: FontRef,
        size_px: f32,
        band: RectPx,
    ) {
        let eligible = scene.layers.len() == 1
            && scene.layers[0].items.len() == 1
            && scene.layers[0].transform == Transform::IDENTITY;
        let Some(SceneItem::Terminal {
            frame,
            rect,
            selection,
            dim,
            ..
        }) = eligible.then(|| &scene.layers[0].items[0])
        else {
            self.render_scene_to_view_damaged(bb_view, scene, font, size_px, Some(band));
            return;
        };
        let n = frame.rows_layout.len();
        let lh = frame.metrics.line_height;
        if n == 0 || lh <= 0.0 {
            self.render_scene_to_view_damaged(bb_view, scene, font, size_px, Some(band));
            return;
        }
        // Band pixel span → rows, expanded one row each way for glyphs / sub-pixel
        // backgrounds spilling across a row boundary (matches `banded_row_ranges`).
        let last = n - 1;
        let a = (((band.y - rect.y) / lh).floor().max(0.0) as usize).min(last);
        let b = ((((band.y + band.h - rect.y) / lh).ceil() as i64 - 1).max(0) as usize).min(last);
        let (lo, hi) = (a.saturating_sub(1), (b + 1).min(last));

        let (mut insts, _) = self.build_instances(frame, font, size_px, *selection, lo..hi + 1);
        translate(&mut insts, rect.x, rect.y);
        if *dim {
            dim_colors(&mut insts);
        }
        self.instance_count = insts.len() as u32;
        Self::upload_instances(
            &self.gpu.device,
            &self.gpu.queue,
            &mut self.instances,
            &mut self.buffer_allocs,
            "band instances",
            &insts,
        );
        self.band_instances = self.instance_count;
        // The build set no uniform; point it at the backbuffer (scene) size for NDC.
        let (vw, vh) = scene.size_px;
        let uniforms = Uniforms {
            viewport: [vw as f32, vh as f32],
            _pad: [0.0, 0.0],
        };
        self.gpu
            .queue
            .write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(&uniforms));
        let scissor = clamp_scissor(band, vw, vh);
        if scissor[2] == 0 || scissor[3] == 0 {
            return;
        }
        // Band background fill (premultiplied, matching `encode_scene`'s clear).
        let alpha = self.theme.bg_alpha;
        let fill = solid(
            band,
            [
                f32::from(self.theme.bg[0]) / 255.0 * alpha,
                f32::from(self.theme.bg[1]) / 255.0 * alpha,
                f32::from(self.theme.bg[2]) / 255.0 * alpha,
                alpha,
            ],
        );
        self.gpu
            .queue
            .write_buffer(&self.bg_fill, 0, bytemuck::bytes_of(&fill));
        // One contiguous range: the whole band-rows buffer (drawn under the band scissor).
        let all = 0..self.instance_count;
        let cb = self.encode_scene_banded_rows(bb_view, std::slice::from_ref(&all), scissor);
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

    /// Capture `scene` to a texture so an interactive resize can stretch-blit it
    /// (see [`Self::blit_snapshot_to_view`]) instead of re-laying-out and
    /// re-rasterizing the whole window at every drag step. Renders the scene
    /// exactly as the surface path would; the texture is kept (premultiplied, in
    /// the surface format) and later sampled with linear filtering so the stretch
    /// is smooth. Replaces any previously held snapshot.
    pub fn capture_snapshot(&mut self, scene: &Scene, font: FontRef, size_px: f32) {
        let w = scene.size_px.0.max(1);
        let h = scene.size_px.1.max(1);
        let draws = self.prepare_scene(scene, font, size_px);
        let texture = self.gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("resize snapshot"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: self.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let cb = self.encode_scene(&view, &draws);
        self.gpu.queue.submit([cb]);
        let bind_group = self
            .gpu
            .device
            .create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("snapshot bind group"),
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
                        resource: wgpu::BindingResource::Sampler(&self.linear_sampler),
                    },
                ],
            });
        self.snapshot = Some(Snapshot {
            _texture: texture,
            bind_group,
            size: (w, h),
        });
    }

    /// Whether a resize snapshot is currently held.
    pub fn has_snapshot(&self) -> bool {
        self.snapshot.is_some()
    }

    /// The captured snapshot's pixel size, if one is held.
    pub fn snapshot_size(&self) -> Option<(u32, u32)> {
        self.snapshot.as_ref().map(|s| s.size)
    }

    /// Drop the resize snapshot — call once the resize commits so subsequent
    /// frames render the crisp scene again.
    pub fn clear_snapshot(&mut self) {
        self.snapshot = None;
    }

    /// Point the shared viewport uniform at the `w`×`h` blit target and (re)build
    /// the single full-surface quad that samples the snapshot. No-op if no
    /// snapshot is held.
    fn prepare_snapshot_blit(&mut self, w: u32, h: u32) {
        if self.snapshot.is_none() {
            return;
        }
        let uniforms = Uniforms {
            viewport: [w as f32, h as f32],
            _pad: [0.0, 0.0],
        };
        self.gpu
            .queue
            .write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(&uniforms));
        let quad = [Instance {
            rect: [0.0, 0.0, w as f32, h as f32],
            uv: [0.0, 0.0, 1.0, 1.0],
            color: [1.0, 1.0, 1.0, 1.0],
        }];
        Self::upload_instances(
            &self.gpu.device,
            &self.gpu.queue,
            &mut self.snapshot_instances,
            &mut self.buffer_allocs,
            "snapshot blit",
            &quad,
        );
    }

    /// Clear to transparent, then blit the snapshot quad over the whole target.
    /// The snapshot is premultiplied, so "over" transparent reproduces it exactly
    /// (preserving a translucent theme's see-through background). Falls back to a
    /// bare clear when no snapshot is held, so a stray call paints nothing rather
    /// than reading an empty buffer.
    fn encode_snapshot(&self, view: &wgpu::TextureView) -> wgpu::CommandBuffer {
        let mut encoder = self
            .gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("snapshot blit"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("snapshot blit"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if let (Some(snap), Some(buf)) = (&self.snapshot, &self.snapshot_instances) {
                pass.set_pipeline(&self.preview_pipeline);
                pass.set_bind_group(0, &snap.bind_group, &[]);
                pass.set_vertex_buffer(0, buf.slice(..));
                pass.draw(0..6, 0..1);
            }
        }
        encoder.finish()
    }

    /// Stretch-blit the captured snapshot to fill `view` (a window surface) at
    /// `w`×`h`. Cheap — a single textured quad — so an interactive resize stays
    /// smooth regardless of how much content the window holds. The caller owns
    /// acquiring/presenting the surface texture. No-op if no snapshot is held.
    pub fn blit_snapshot_to_view(&mut self, view: &wgpu::TextureView, w: u32, h: u32) {
        if self.snapshot.is_none() {
            return;
        }
        self.prepare_snapshot_blit(w, h);
        let cb = self.encode_snapshot(view);
        self.gpu.queue.submit([cb]);
    }

    /// Stretch-blit the captured snapshot to a fresh `w`×`h` offscreen target and
    /// read it back — the windowless counterpart of [`Self::blit_snapshot_to_view`]
    /// for tests and `ghost-shot`.
    pub fn blit_snapshot_offscreen(&mut self, w: u32, h: u32) -> Rendered {
        let w = w.max(1);
        let h = h.max(1);
        self.prepare_snapshot_blit(w, h);
        let target = offscreen_target(&self.gpu.device, w, h, self.format);
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let cb = self.encode_snapshot(&view);
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

    const FIRA: &[u8] = include_bytes!("../../ghost-shaper/tests/assets/FiraCode-Regular.ttf");
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
    fn instance_buffer_is_reused_until_a_frame_must_grow_it() {
        let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
        let mut r = Renderer::headless(Theme::default());

        // First frame allocates the instance buffer.
        let _ = r.render_offscreen(&frame(20, 4, "hi"), font, SIZE_PX);
        let after_first = r.buffer_allocs();
        assert_eq!(after_first, 1, "first frame allocates once");

        // A frame of the same shape reuses the buffer in place — no reallocation.
        let _ = r.render_offscreen(&frame(20, 4, "yo"), font, SIZE_PX);
        assert_eq!(
            r.buffer_allocs(),
            after_first,
            "a same-size frame must reuse the buffer"
        );

        // A much larger frame needs more instances than the buffer holds, so it
        // reallocates to grow.
        let big = "x".repeat(80 * 24);
        let _ = r.render_offscreen(&frame(80, 24, &big), font, SIZE_PX);
        assert!(
            r.buffer_allocs() > after_first,
            "a larger frame must grow the buffer"
        );

        // Shrinking back fits in the now-larger buffer: reuse again.
        let grown = r.buffer_allocs();
        let _ = r.render_offscreen(&frame(20, 4, "hi"), font, SIZE_PX);
        assert_eq!(
            r.buffer_allocs(),
            grown,
            "a smaller frame fits the grown buffer and reuses it"
        );
    }

    #[test]
    fn scene_cache_skips_identical_redraws_until_invalidated() {
        let mut cache = SceneCache::default();
        let a = Scene::new((100, 50));
        let mut b = Scene::new((100, 50));
        b.layers.push(Layer::new(
            0,
            vec![SceneItem::Rect {
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
        ));

        // These chrome scenes carry no Terminal, so any real change is a full redraw
        // and an identical follow-up is a skip (`Damage::None`).
        let draws = |d: Damage| d != Damage::None;

        // First ever scene must draw; an identical follow-up is skipped.
        assert!(draws(cache.damage(&a, 16.0)));
        assert!(!draws(cache.damage(&a, 16.0)));

        // A different scene draws, then is itself cached.
        assert!(draws(cache.damage(&b, 16.0)));
        assert!(!draws(cache.damage(&b, 16.0)));

        // Same scene at a different font size must redraw (the raster differs).
        assert!(draws(cache.damage(&b, 20.0)));
        assert!(!draws(cache.damage(&b, 20.0)));

        // After invalidation (e.g. a surface reconfigure that dropped the frame)
        // the very next call redraws even though the scene is unchanged.
        cache.invalidate();
        assert!(draws(cache.damage(&b, 20.0)));
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
            layers: vec![Layer::new(
                0,
                vec![SceneItem::Terminal {
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
            )],
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
            layers: vec![Layer::new(
                0,
                vec![SceneItem::Terminal {
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
            )],
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
            layers: vec![Layer::new(
                0,
                vec![SceneItem::Terminal {
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
            )],
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
            layers: vec![Layer::new(
                0,
                vec![SceneItem::Terminal {
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
            )],
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
            layers: vec![Layer::new(
                0,
                vec![SceneItem::Terminal {
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
            )],
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
            layers: vec![Layer::new(
                0,
                vec![SceneItem::Border {
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
            )],
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
            layers: vec![Layer::new(
                0,
                vec![SceneItem::Rect {
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
            )],
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

    /// A full-window single-`Terminal` scene for `f`, at `w`×`h` px.
    fn single_scene(f: Frame, w: u32, h: u32) -> Scene {
        Scene {
            size_px: (w, h),
            layers: vec![Layer::new(
                0,
                vec![SceneItem::Terminal {
                    id: SceneId::Root,
                    rect: RectPx {
                        x: 0.0,
                        y: 0.0,
                        w: w as f32,
                        h: h as f32,
                    },
                    frame: f,
                    selection: None,
                    dim: false,
                }],
            )],
        }
    }

    #[test]
    fn damaged_render_matches_a_full_render() {
        let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
        let (cols, rows) = (20usize, 5usize);
        let (w, h) = (cols as u32 * 9, rows as u32 * 18); // TM metrics: 180x90

        // Frame A, then frame B differing only in row 2 — and SHORTER there, so a
        // correct damaged redraw must ERASE the old longer text (repaint the band's
        // background), not merely overdraw it.
        let a = frame(
            cols,
            rows,
            "AAAAAAAA\r\nBBBBBBBB\r\nHELLO WORLD\r\nDDDDDDDD\r\nEEEEEEEE",
        );
        let b = frame(
            cols,
            rows,
            "AAAAAAAA\r\nBBBBBBBB\r\nhi\r\nDDDDDDDD\r\nEEEEEEEE",
        );

        let mut r = Renderer::headless(Theme::default());
        // One persistent target both frames render into (COPY_SRC for readback).
        let target = offscreen_target(&r.gpu.device, w, h, r.format);
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());

        // Full render of A, then a DAMAGED redraw of B (band = row 2) onto the same
        // target, so rows 0,1,3,4 survive from A (LoadOp::Load) and row 2 is repainted.
        r.render_scene_to_view(&view, &single_scene(a, w, h), font, SIZE_PX);
        let band = RectPx {
            x: 0.0,
            y: 2.0 * TM.line_height,
            w: w as f32,
            h: TM.line_height,
        };
        r.render_scene_to_view_damaged(
            &view,
            &single_scene(b.clone(), w, h),
            font,
            SIZE_PX,
            Some(band),
        );
        let damaged = read_back(&r.gpu, &target, w, h);

        // Ground truth: a from-scratch full render of B.
        let full = r.render_offscreen_scene(&single_scene(b, w, h), font, SIZE_PX);

        assert_eq!(damaged.len(), full.rgba.len());
        assert_eq!(
            damaged, full.rgba,
            "a damaged partial redraw must be byte-identical to a full render of the new frame"
        );
    }

    /// Clear `tex` to a solid colour — models a swapchain image handing back
    /// arbitrary, non-persisted contents on acquire.
    fn fill_target(gpu: &Gpu, tex: &wgpu::Texture, c: [f64; 4]) {
        let view = tex.create_view(&wgpu::TextureViewDescriptor::default());
        let mut enc = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("garbage fill"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: c[0],
                        g: c[1],
                        b: c[2],
                        a: c[3],
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        gpu.queue.submit([enc.finish()]);
    }

    #[test]
    fn present_survives_a_nonpersistent_swapchain() {
        // Vulkan leaves an acquired swapchain image's contents UNDEFINED, so a damaged
        // present must still display the COMPLETE frame — not just the band — even when
        // the image held unrelated content. Model that: corrupt the "swapchain image"
        // before each present, then assert the result equals a full render of the new
        // frame. (This is the regression from rendering the band straight onto the
        // surface: everything outside the band was lost.)
        let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
        let (cols, rows) = (20usize, 5usize);
        let (w, h) = (cols as u32 * 9, rows as u32 * 18);
        let row_band = |row: usize| RectPx {
            x: 0.0,
            y: row as f32 * TM.line_height,
            w: w as f32,
            h: TM.line_height,
        };
        // Three frames, each changing one more row than the last (a cumulative edit, as
        // typing does), so a band carries exactly the changed row and the backbuffer
        // must accumulate the rest.
        let a = frame(cols, rows, "r0\r\nr1\r\nr2\r\nr3\r\nr4");
        let b = frame(cols, rows, "r0\r\nB1\r\nr2\r\nr3\r\nr4"); // row 1 changed
        let c = frame(cols, rows, "r0\r\nB1\r\nC2\r\nr3\r\nr4"); // row 2 changed

        let mut r = Renderer::headless(Theme::default());
        // Two images the "swapchain" cycles between; NEITHER persists its contents.
        let imgs = [
            offscreen_target(&r.gpu.device, w, h, r.format),
            offscreen_target(&r.gpu.device, w, h, r.format),
        ];
        let views: Vec<_> = imgs
            .iter()
            .map(|t| t.create_view(&wgpu::TextureViewDescriptor::default()))
            .collect();

        // Present A full, then B and C as single-row bands — each onto a freshly
        // corrupted image, cycling buffers, so a band that only updated the backbuffer
        // (not the whole surface) would leave garbage behind.
        let presents = [
            (a, None),
            (b, Some(row_band(1))),
            (c.clone(), Some(row_band(2))),
        ];
        let n = presents.len();
        for (i, (f, band)) in presents.into_iter().enumerate() {
            let idx = i % imgs.len();
            fill_target(&r.gpu, &imgs[idx], [0.0, 1.0, 0.0, 1.0]); // garbage green
            r.present_scene(
                &views[idx],
                (w, h),
                &single_scene(f, w, h),
                font,
                SIZE_PX,
                band,
            );
        }

        // The last present (frame C, a steady band) must have produced the COMPLETE
        // frame on its image despite the garbage and the band.
        let last = (n - 1) % imgs.len();
        let got = read_back(&r.gpu, &imgs[last], w, h);
        let full = r.render_offscreen_scene(&single_scene(c, w, h), font, SIZE_PX);
        assert_eq!(
            got, full.rgba,
            "a banded present must display the complete frame even when the swapchain image held unrelated content"
        );
    }

    #[test]
    fn steady_band_present_builds_only_its_rows() {
        // The incremental banded present builds + uploads ONLY the band's rows (the
        // backbuffer holds the rest), so instance_count after a steady band must be a
        // small fraction of a full build of the same frame.
        let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
        let (cols, rows) = (20usize, 24usize);
        let (w, h) = (cols as u32 * 9, rows as u32 * 18);
        // Cumulative single-row edits: A base, B changes row 11, C changes row 12.
        let scene_of = |changed: &[usize]| {
            let mut s = String::new();
            for row in 0..rows {
                if row > 0 {
                    s.push_str("\r\n");
                }
                s.push_str(if changed.contains(&row) {
                    "CHANGED ROW"
                } else {
                    "static line"
                });
            }
            single_scene(frame(cols, rows, &s), w, h)
        };
        let band_of = |row: usize| RectPx {
            x: 0.0,
            y: row as f32 * TM.line_height,
            w: w as f32,
            h: TM.line_height,
        };

        let mut r = Renderer::headless(Theme::default());
        let img = offscreen_target(&r.gpu.device, w, h, r.format);
        let v = img.create_view(&wgpu::TextureViewDescriptor::default());

        // A full; B band (fresh backbuffer → full refresh); C band (steady → incremental).
        r.present_scene(&v, (w, h), &scene_of(&[]), font, SIZE_PX, None);
        r.present_scene(
            &v,
            (w, h),
            &scene_of(&[11]),
            font,
            SIZE_PX,
            Some(band_of(11)),
        );
        r.present_scene(
            &v,
            (w, h),
            &scene_of(&[11, 12]),
            font,
            SIZE_PX,
            Some(band_of(12)),
        );
        let incremental = r.instance_count();

        // A full build of the same frame, for the reference count.
        let _ = r.render_offscreen_scene(&scene_of(&[11, 12]), font, SIZE_PX);
        let full = r.instance_count();
        assert!(incremental > 0, "the band still builds its rows");
        assert!(
            incremental * 4 < full,
            "a steady band must build only its rows ({incremental}) vs a full build ({full})"
        );
    }

    #[test]
    fn damaged_redraw_draws_only_the_bands_rows() {
        // A one-row band must submit only that row's instances (±1 for spill), not
        // the whole buffer — llvmpipe processes every drawn instance at submit time
        // regardless of scissor, so culling the draw is the win at 4K.
        let font = ghost_shaper::font_from_bytes(FIRA).expect("font");
        let (cols, rows) = (20usize, 24usize);
        let (w, h) = (cols as u32 * 9, rows as u32 * 18);

        // Every row carries text (so the full buffer is large); B differs from A
        // only at row 12.
        let lines = |row12: &str| {
            let mut s = String::new();
            for r in 0..rows {
                if r > 0 {
                    s.push_str("\r\n");
                }
                s.push_str(if r == 12 { row12 } else { "static text here" });
            }
            s
        };
        let a = frame(cols, rows, &lines("AAAAAAAAAAAA"));
        let b = frame(cols, rows, &lines("bbb"));

        let mut r = Renderer::headless(Theme::default());
        let target = offscreen_target(&r.gpu.device, w, h, r.format);
        let view = target.create_view(&wgpu::TextureViewDescriptor::default());

        r.render_scene_to_view(&view, &single_scene(a, w, h), font, SIZE_PX);
        let band = RectPx {
            x: 0.0,
            y: 12.0 * TM.line_height,
            w: w as f32,
            h: TM.line_height,
        };
        r.render_scene_to_view_damaged(&view, &single_scene(b, w, h), font, SIZE_PX, Some(band));

        let full = r.instance_count();
        let drawn = r.band_instances();
        assert!(
            drawn > 0,
            "the band must still draw its rows (drew {drawn})"
        );
        assert!(
            drawn * 4 < full,
            "a one-row band should draw far fewer than all {full} instances, drew {drawn}"
        );
    }

    #[test]
    fn scene_cache_partial_redraws_a_steady_single_view() {
        let (w, h) = (180u32, 90u32);
        let scene = |s: &str| single_scene(frame(20, 5, s), w, h);
        let mut c = SceneCache::default();

        // The first present is always Full (no prior frame); re-presenting the same
        // scene is a skip.
        assert_eq!(
            c.damage(&scene("a\r\nb\r\nROW2\r\nd\r\ne"), 15.0),
            Damage::Full
        );
        assert_eq!(
            c.damage(&scene("a\r\nb\r\nROW2\r\nd\r\ne"), 15.0),
            Damage::None
        );

        // Change only row 2 each frame. The initial full band has to age out of the
        // swapchain-depth window first; after that we get a partial band at row 2.
        let mut last = Damage::Full;
        for i in 0..SWAPCHAIN_DEPTH + 2 {
            last = c.damage(&scene(&format!("a\r\nb\r\nr{i}\r\nd\r\ne")), 15.0);
        }
        let Damage::Band(rect) = last else {
            panic!("expected a partial band once the view settled, got {last:?}");
        };
        // Row 2 spans y∈[36,54) at line-height 18; the band covers it and excludes
        // the unchanged rows above.
        assert!(
            rect.y >= 18.0 && rect.y + rect.h >= 54.0 && rect.y <= 36.0,
            "band should cover row 2 only: {rect:?}"
        );
    }

    #[test]
    fn scene_cache_full_redraws_on_resize_and_chrome() {
        let mut c = SceneCache::default();
        let a = single_scene(frame(20, 5, "hi"), 180, 90);
        assert_eq!(c.damage(&a, 15.0), Damage::Full);
        assert_eq!(c.damage(&a, 15.0), Damage::None);

        // A resize (different surface size) can never be a partial band.
        let resized = single_scene(frame(20, 5, "hi"), 200, 90);
        assert_eq!(c.damage(&resized, 15.0), Damage::Full);

        // A scene carrying chrome (a non-Terminal item) never partials, even if only
        // a terminal row changed — a band repaint could erase chrome over that row.
        let with_chrome = |s: &str| {
            let mut sc = single_scene(frame(20, 5, s), 180, 90);
            sc.layers[0].items.push(SceneItem::Rect {
                id: SceneId::Tile(7),
                rect: RectPx {
                    x: 0.0,
                    y: 0.0,
                    w: 180.0,
                    h: 18.0,
                },
                color: [1.0, 1.0, 1.0, 1.0],
                radius: 0.0,
            });
            sc
        };
        let mut c2 = SceneCache::default();
        assert_eq!(c2.damage(&with_chrome("x\r\ny"), 15.0), Damage::Full);
        assert_eq!(c2.damage(&with_chrome("x\r\nY"), 15.0), Damage::Full);
    }

    #[test]
    fn cell_cols_snaps_glyphs_to_the_grid() {
        let mut buf = Vec::new();

        // ASCII: one cell per char.
        fill_cell_cols(&mut buf, "ab");
        assert_eq!(buf[0], 0);
        assert_eq!(buf[1], 1);

        // Wide char occupies two columns: 'a'@b0->col0, '世'@b1(3 bytes)->col1,
        // 'b'@b4->col3 (skips the wide char's second column). Reusing `buf` (it was
        // longer) must not leak stale columns.
        fill_cell_cols(&mut buf, "a世b");
        assert_eq!(buf.len(), 5);
        assert_eq!(buf[0], 0); // 'a'
        assert_eq!(buf[1], 1); // '世' start byte
        assert_eq!(buf[4], 3); // 'b'
    }
}
