//! A minimal wgpu pipeline for drawing solid-color rectangles.
//!
//! Used for terminal cell backgrounds and the cursor. Two independent
//! vertex buffers ("layers") share a single pipeline + uniform — the
//! background layer draws before the text pass and the overlay layer
//! draws after, so the cursor sits on top of glyphs.

use anyhow::Result;
use bytemuck::{Pod, Zeroable};
use wgpu::util::{BufferInitDescriptor, DeviceExt};

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct QuadVertex {
    pub pos: [f32; 2],
    pub color: [f32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct QuadUniform {
    resolution: [f32; 2],
    _pad: [f32; 2],
}

const SHADER_SRC: &str = r#"
struct Uniform {
    resolution: vec2<f32>,
    _pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> u: Uniform;

struct VertexIn {
    @location(0) pos: vec2<f32>,
    @location(1) color: vec4<f32>,
};

struct VertexOut {
    @builtin(position) clip_pos: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_main(in: VertexIn) -> VertexOut {
    var out: VertexOut;
    let ndc = vec2<f32>(
        in.pos.x / u.resolution.x * 2.0 - 1.0,
        1.0 - in.pos.y / u.resolution.y * 2.0
    );
    out.clip_pos = vec4<f32>(ndc, 0.0, 1.0);
    out.color = in.color;
    return out;
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    return in.color;
}
"#;

struct Layer {
    vertex_buffer: wgpu::Buffer,
    capacity: u64,
    count: u32,
}

impl Layer {
    fn new(device: &wgpu::Device, label: &str) -> Self {
        let initial_capacity = 256u64 * std::mem::size_of::<QuadVertex>() as u64;
        let vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: initial_capacity,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        Self {
            vertex_buffer,
            capacity: initial_capacity,
            count: 0,
        }
    }

    fn upload(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        verts: &[QuadVertex],
        label: &str,
    ) {
        if verts.is_empty() {
            self.count = 0;
            return;
        }
        let needed = (verts.len() * std::mem::size_of::<QuadVertex>()) as u64;
        if needed > self.capacity {
            let new_cap = needed.next_power_of_two();
            self.vertex_buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: new_cap,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.capacity = new_cap;
        }
        queue.write_buffer(&self.vertex_buffer, 0, bytemuck::cast_slice(verts));
        self.count = verts.len() as u32;
    }
}

/// One pipeline + uniform shared across two independent quad layers.
pub struct QuadRenderer {
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    uniform_buffer: wgpu::Buffer,
    bg_layer: Layer,
    overlay_layer: Layer,
}

impl QuadRenderer {
    pub fn new(device: &wgpu::Device, surface_format: wgpu::TextureFormat) -> Result<Self> {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("purrtty.quad.shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER_SRC.into()),
        });

        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("purrtty.quad.bgl"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                }],
            });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("purrtty.quad.layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("purrtty.quad.pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: "vs_main",
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<QuadVertex>() as wgpu::BufferAddress,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &[
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x2,
                            offset: 0,
                            shader_location: 0,
                        },
                        wgpu::VertexAttribute {
                            format: wgpu::VertexFormat::Float32x4,
                            offset: std::mem::size_of::<[f32; 2]>() as wgpu::BufferAddress,
                            shader_location: 1,
                        },
                    ],
                }],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: "fs_main",
                targets: &[Some(wgpu::ColorTargetState {
                    format: surface_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let uniform_buffer = device.create_buffer_init(&BufferInitDescriptor {
            label: Some("purrtty.quad.uniform"),
            contents: bytemuck::bytes_of(&QuadUniform {
                resolution: [1.0, 1.0],
                _pad: [0.0, 0.0],
            }),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("purrtty.quad.bg"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            }],
        });

        Ok(Self {
            pipeline,
            bind_group,
            uniform_buffer,
            bg_layer: Layer::new(device, "purrtty.quad.bg.vbo"),
            overlay_layer: Layer::new(device, "purrtty.quad.overlay.vbo"),
        })
    }

    pub fn update_resolution(&self, queue: &wgpu::Queue, width: u32, height: u32) {
        queue.write_buffer(
            &self.uniform_buffer,
            0,
            bytemuck::bytes_of(&QuadUniform {
                resolution: [width as f32, height as f32],
                _pad: [0.0, 0.0],
            }),
        );
    }

    pub fn upload_bg(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        verts: &[QuadVertex],
    ) {
        self.bg_layer.upload(device, queue, verts, "purrtty.quad.bg.vbo");
    }

    pub fn upload_overlay(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        verts: &[QuadVertex],
    ) {
        self.overlay_layer
            .upload(device, queue, verts, "purrtty.quad.overlay.vbo");
    }

    pub fn render_bg<'a>(&'a self, pass: &mut wgpu::RenderPass<'a>) {
        self.draw(pass, &self.bg_layer);
    }

    pub fn render_overlay<'a>(&'a self, pass: &mut wgpu::RenderPass<'a>) {
        self.draw(pass, &self.overlay_layer);
    }

    fn draw<'a>(&'a self, pass: &mut wgpu::RenderPass<'a>, layer: &'a Layer) {
        if layer.count == 0 {
            return;
        }
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, layer.vertex_buffer.slice(..));
        pass.draw(0..layer.count, 0..1);
    }

    /// Push a screen-space rectangle into the upload list.
    pub fn push_rect(
        verts: &mut Vec<QuadVertex>,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        color: [f32; 4],
    ) {
        let x2 = x + w;
        let y2 = y + h;
        let v = |px, py| QuadVertex {
            pos: [px, py],
            color,
        };
        verts.push(v(x, y));
        verts.push(v(x2, y));
        verts.push(v(x, y2));
        verts.push(v(x, y2));
        verts.push(v(x2, y));
        verts.push(v(x2, y2));
    }
}
