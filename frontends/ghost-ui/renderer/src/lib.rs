//! Offscreen GPU renderer (wgpu) for `ghost-render` frames.
//!
//! This slice establishes the headless path: a device on a software adapter
//! (lavapipe), render-to-texture, and pixel readback — the foundation for
//! deterministic, windowless golden tests. Glyph rendering layers on next.

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

/// A headless GPU context.
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

fn offscreen_target(device: &wgpu::Device, width: u32, height: u32) -> wgpu::Texture {
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
        format: FORMAT,
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
    let texture = offscreen_target(&gpu.device, width, height);
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

// ---- glyph rendering ----------------------------------------------------

/// Instanced textured quad: one per visible glyph.
#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Instance {
    /// Screen rect in pixels: x, y, width, height (origin top-left).
    rect: [f32; 4],
    /// Atlas UV rect: u0, v0, u1, v1.
    uv: [f32; 4],
    /// Foreground color, straight alpha.
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

/// Resolve a run's foreground to an RGBA color. Indexed/default colors map to a
/// light foreground for now; full palette resolution comes with theming.
fn resolve_fg(style: &Style) -> [f32; 4] {
    match style.fg {
        Some(Color::RGB(c)) => [
            f32::from(c.r) / 255.0,
            f32::from(c.g) / 255.0,
            f32::from(c.b) / 255.0,
            1.0,
        ],
        _ => [0.85, 0.86, 0.88, 1.0],
    }
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

const ATLAS_W: u32 = 1024;

/// Render a laid-out [`Frame`]'s glyphs to an offscreen image: shape each run
/// (ligatures applied), rasterize glyphs into an atlas, and draw one textured
/// quad per glyph. Background is cleared to `bg`.
pub fn render_frame(frame: &Frame, font: FontRef, size_px: f32, bg: [f64; 4]) -> Rendered {
    let metrics = frame.metrics;
    let width = (frame.cols as f32 * metrics.advance).ceil().max(1.0) as u32;
    let height = (frame.rows as f32 * metrics.line_height).ceil().max(1.0) as u32;
    // A reasonable baseline within the cell until real font ascent is wired in.
    let baseline = metrics.line_height * 0.8;

    // 1. Walk the frame, shape each run, and record each glyph's pen position.
    struct Placed {
        x: f32,
        baseline_y: f32,
        id: u16,
        color: [f32; 4],
    }
    let mut placed: Vec<Placed> = Vec::new();
    for (row, layout) in frame.rows_layout.iter().enumerate() {
        let baseline_y = row as f32 * metrics.line_height + baseline;
        for run in &layout.runs {
            let color = resolve_fg(&run.style);
            let mut pen = run.start_col as f32 * metrics.advance;
            for g in ghost_shaper::shape(font, &run.text, size_px) {
                placed.push(Placed {
                    x: pen,
                    baseline_y,
                    id: g.id,
                    color,
                });
                pen += g.advance;
            }
        }
    }

    // 2. Rasterize each distinct glyph once and shelf-pack it into the atlas.
    let mut slots: HashMap<u16, Option<Slot>> = HashMap::new();
    let mut to_blit: Vec<(Slot, ghost_shaper::GlyphBitmap)> = Vec::new();
    let (mut cx, mut cy, mut shelf) = (0u32, 0u32, 0u32);
    for p in &placed {
        if slots.contains_key(&p.id) {
            continue;
        }
        match ghost_shaper::rasterize(font, p.id, size_px) {
            Some(bmp) if bmp.width > 0 && bmp.height > 0 => {
                if cx + bmp.width > ATLAS_W {
                    cx = 0;
                    cy += shelf;
                    shelf = 0;
                }
                let slot = Slot {
                    ax: cx,
                    ay: cy,
                    w: bmp.width,
                    h: bmp.height,
                    left: bmp.left,
                    top: bmp.top,
                };
                slots.insert(p.id, Some(slot));
                cx += bmp.width + 1;
                shelf = shelf.max(bmp.height);
                to_blit.push((slot, bmp));
            }
            _ => {
                slots.insert(p.id, None);
            }
        }
    }
    let atlas_h = (cy + shelf).max(1);
    let mut atlas = vec![0u8; (ATLAS_W * atlas_h) as usize];
    for (slot, bmp) in &to_blit {
        for row in 0..bmp.height {
            let src = (row * bmp.width) as usize;
            let dst = ((slot.ay + row) * ATLAS_W + slot.ax) as usize;
            atlas[dst..dst + bmp.width as usize]
                .copy_from_slice(&bmp.coverage[src..src + bmp.width as usize]);
        }
    }

    // 3. Build one instance per placed glyph that has an atlas slot.
    let mut instances: Vec<Instance> = Vec::new();
    for p in &placed {
        let Some(Some(slot)) = slots.get(&p.id) else {
            continue;
        };
        instances.push(Instance {
            rect: [
                p.x + slot.left as f32,
                p.baseline_y - slot.top as f32,
                slot.w as f32,
                slot.h as f32,
            ],
            uv: [
                slot.ax as f32 / ATLAS_W as f32,
                slot.ay as f32 / atlas_h as f32,
                (slot.ax + slot.w) as f32 / ATLAS_W as f32,
                (slot.ay + slot.h) as f32 / atlas_h as f32,
            ],
            color: p.color,
        });
    }

    // 4. GPU: upload the atlas, build the pipeline, draw, read back.
    let gpu = Gpu::headless();
    let target = offscreen_target(&gpu.device, width, height);
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());

    let atlas_tex = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("glyph atlas"),
        size: wgpu::Extent3d {
            width: ATLAS_W,
            height: atlas_h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    gpu.queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &atlas_tex,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        &atlas,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(ATLAS_W),
            rows_per_image: Some(atlas_h),
        },
        wgpu::Extent3d {
            width: ATLAS_W,
            height: atlas_h,
            depth_or_array_layers: 1,
        },
    );
    let atlas_view = atlas_tex.create_view(&wgpu::TextureViewDescriptor::default());
    let sampler = gpu.device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("glyph sampler"),
        mag_filter: wgpu::FilterMode::Nearest,
        min_filter: wgpu::FilterMode::Nearest,
        ..Default::default()
    });

    let uniforms = Uniforms {
        viewport: [width as f32, height as f32],
        _pad: [0.0, 0.0],
    };
    let uniform_buf = gpu
        .device
        .create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("uniforms"),
            contents: bytemuck::bytes_of(&uniforms),
            usage: wgpu::BufferUsages::UNIFORM,
        });
    let instance_buf = gpu
        .device
        .create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("instances"),
            contents: bytemuck::cast_slice(&instances),
            usage: wgpu::BufferUsages::VERTEX,
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
                    format: FORMAT,
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

    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("glyphs"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: bg[0],
                        g: bg[1],
                        b: bg[2],
                        a: bg[3],
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        if !instances.is_empty() {
            pass.set_pipeline(&pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.set_vertex_buffer(0, instance_buf.slice(..));
            pass.draw(0..6, 0..instances.len() as u32);
        }
    }
    gpu.queue.submit([encoder.finish()]);

    let rgba = read_back(&gpu, &target, width, height);
    Rendered {
        width,
        height,
        rgba,
    }
}
