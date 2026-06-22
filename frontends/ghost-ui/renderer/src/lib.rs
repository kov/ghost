//! GPU renderer (wgpu) for `ghost-render` frames.
//!
//! A persistent [`Renderer`] owns the device, pipeline, glyph atlas and glyph
//! cache, and can draw a laid-out [`Frame`] either to an offscreen target (for
//! deterministic, windowless golden tests on a software adapter) or into a
//! window surface view. Cell backgrounds, the block cursor, and full ANSI/256
//! color resolution are handled here; glyph shaping (with ligatures) comes from
//! `ghost-shaper`.

use std::collections::HashMap;

use ghost_render::{Frame, Style};
use ghost_shaper::FontRef;
use ghost_term::Color;
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
    pub fn headless() -> Self {
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
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            fg: [0xd8, 0xdb, 0xe0],
            bg: [0x10, 0x10, 0x12],
        }
    }
}

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

fn resolve(c: Option<Color>, default: [u8; 3]) -> [u8; 3] {
    match c {
        None => default,
        Some(Color::Indexed(i)) => index_to_rgb(i),
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
    let mut fg = resolve(maybe_brighten(style.fg, style.bold), theme.fg);
    let mut bg = resolve(style.bg, theme.bg);
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
    return vec4<f32>(in.color.rgb, in.color.a * a);
}
"#;

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
    /// glyph cache keyed by (glyph id, font size bits); `None` = no bitmap.
    cache: HashMap<(u16, u32), Option<Slot>>,
    // shelf-packing cursor into the atlas.
    pack_x: u32,
    pack_y: u32,
    shelf: u32,
    // per-frame instance buffer.
    instances: Option<wgpu::Buffer>,
    instance_count: u32,
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
                        blend: Some(wgpu::BlendState {
                            color: wgpu::BlendComponent {
                                src_factor: wgpu::BlendFactor::SrcAlpha,
                                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                                operation: wgpu::BlendOperation::Add,
                            },
                            alpha: wgpu::BlendComponent {
                                src_factor: wgpu::BlendFactor::One,
                                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                                operation: wgpu::BlendOperation::Add,
                            },
                        }),
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
    fn ensure_glyph(&mut self, font: FontRef, id: u16, size_px: f32) -> Option<Slot> {
        let key = (id, size_px.to_bits());
        if let Some(slot) = self.cache.get(&key) {
            return *slot;
        }
        let resolved = match ghost_shaper::rasterize(font, id, size_px) {
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
    /// then glyph quads on top.
    fn build_instances(&mut self, frame: &Frame, font: FontRef, size_px: f32) -> Vec<Instance> {
        let metrics = frame.metrics;
        let baseline = metrics.line_height * 0.8;
        let cursor = frame.cursor;

        let mut backgrounds: Vec<Instance> = Vec::new();
        let mut glyphs: Vec<Instance> = Vec::new();

        for (row, layout) in frame.rows_layout.iter().enumerate() {
            let row_y = row as f32 * metrics.line_height;
            let baseline_y = row_y + baseline;
            for run in &layout.runs {
                let is_cursor = cursor.is_some_and(|c| c.row == row && c.col == run.start_col);
                let (fg, bg_opt) = run_colors(&run.style, self.theme);
                let x = run.start_col as f32 * metrics.advance;
                let w = run.width_cols as f32 * metrics.advance;

                // The cursor cell renders as a block in the foreground color with
                // its glyph inverted to the background color.
                let (block, glyph_color) = if is_cursor {
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

                let mut pen = x;
                for g in ghost_shaper::shape(font, &run.text, size_px) {
                    let advance = g.advance;
                    if let Some(slot) = self.ensure_glyph(font, g.id, size_px) {
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
                    pen += advance;
                }
            }
        }

        backgrounds.extend(glyphs);
        backgrounds
    }

    /// Prepare GPU state for one frame: pack glyphs, upload instances, set the
    /// viewport uniform.
    fn prepare(&mut self, frame: &Frame, font: FontRef, size_px: f32, vw: u32, vh: u32) {
        let instances = self.build_instances(frame, font, size_px);
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
                            r: f64::from(self.theme.bg[0]) / 255.0,
                            g: f64::from(self.theme.bg[1]) / 255.0,
                            b: f64::from(self.theme.bg[2]) / 255.0,
                            a: 1.0,
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
}

/// Convenience: render a single frame offscreen on a fresh headless renderer.
pub fn render_frame(frame: &Frame, font: FontRef, size_px: f32, theme: Theme) -> Rendered {
    Renderer::headless(theme).render_offscreen(frame, font, size_px)
}
