// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The transfer curve, applied by hand, for a surface that cannot take it.
//!
//! A frame is drawn in an sRGB format, so the hardware writes every colour
//! with its transfer curve. A surface that offers only linear formats is
//! normally drawn through an sRGB *view* of itself — but a view in another
//! format needs `SURFACE_VIEW_FORMATS`, which OpenGL ES without
//! `EGL_KHR_gl_colorspace` (the Android emulator's translator, older
//! devices) does not have. There the frame is resolved into an sRGB texture
//! of its own, and one full-screen pass reads it back — decoded to linear by
//! the hardware — and writes it into the linear surface with the curve
//! applied in the shader. One extra pass, on that path only.
//!
//! The pass carries a depth buffer it never tests against. The emulator's
//! OpenGL ES encoder keeps the frame's multisampled depth buffer as attached
//! after wgpu detaches it, and then refuses every draw into a framebuffer whose
//! attachments disagree on their sample count (`GL_INVALID_FRAMEBUFFER_OPERATION`
//! once a frame, and nothing drawn: a black screen). A single-sampled depth
//! buffer of the pass's own replaces it.

use crate::context::{GpuContext, DEPTH_FORMAT};

const SHADER: &str = r"
@group(0) @binding(0) var frame: texture_2d<f32>;

@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    // One triangle over the whole target.
    let corner = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(corner * 2.0 - 1.0, 0.0, 1.0);
}

@fragment
fn fs(@builtin(position) at: vec4<f32>) -> @location(0) vec4<f32> {
    let linear = textureLoad(frame, vec2<i32>(at.xy), 0);
    let low = linear.rgb * 12.92;
    let high = 1.055 * pow(linear.rgb, vec3<f32>(1.0 / 2.4)) - 0.055;
    return vec4<f32>(select(high, low, linear.rgb <= vec3<f32>(0.0031308)), linear.a);
}
";

/// The sRGB texture a frame is resolved into, and the pass that writes it
/// into the linear surface.
pub(crate) struct Encode {
    /// The resolve target: single-sampled, in the frame's sRGB format.
    pub(crate) resolved: wgpu::TextureView,
    /// Single-sampled, never tested against: see the module's documentation.
    depth: wgpu::TextureView,
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
}

impl Encode {
    /// `render` is the sRGB format the frame is drawn in, `surface` the
    /// linear format the surface is configured with.
    pub(crate) fn new(
        gpu: &GpuContext,
        render: wgpu::TextureFormat,
        surface: wgpu::TextureFormat,
        size: (u32, u32),
    ) -> Self {
        let resolved = gpu
            .device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("tuile frame, resolved for encoding"),
                size: wgpu::Extent3d {
                    width: size.0,
                    height: size.1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: render,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            })
            .create_view(&wgpu::TextureViewDescriptor::default());
        let depth = gpu
            .device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("tuile srgb encode depth"),
                size: wgpu::Extent3d {
                    width: size.0,
                    height: size.1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: DEPTH_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            })
            .create_view(&wgpu::TextureViewDescriptor::default());
        let module = gpu
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("tuile srgb encode"),
                source: wgpu::ShaderSource::Wgsl(SHADER.into()),
            });
        let pipeline = gpu
            .device
            .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("tuile srgb encode"),
                layout: None,
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs"),
                    compilation_options: Default::default(),
                    targets: &[Some(surface.into())],
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(false),
                    depth_compare: Some(wgpu::CompareFunction::Always),
                    stencil: wgpu::StencilState::default(),
                    bias: wgpu::DepthBiasState::default(),
                }),
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            });
        let bind_group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("tuile srgb encode"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&resolved),
            }],
        });
        Self {
            resolved,
            depth,
            pipeline,
            bind_group,
        }
    }

    /// Writes the resolved frame into `drawable`, the surface's own view.
    pub(crate) fn encode(&self, encoder: &mut wgpu::CommandEncoder, drawable: &wgpu::TextureView) {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("tuile srgb encode"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: drawable,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    // Every pixel is written: nothing to load.
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &self.depth,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Discard,
                }),
                stencil_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(0),
                    store: wgpu::StoreOp::Discard,
                }),
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}
