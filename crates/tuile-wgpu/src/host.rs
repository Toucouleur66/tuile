// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Drawing into a picture the host owns.
//!
//! A host that embeds tuile in a renderer of its own — a game engine's render
//! graph — owns the device, the colour target and the depth buffer, and has
//! usually drawn into them already. It hands tuile those three and the frame's
//! encoder; tuile records its pass and submits nothing, presents nothing, and
//! clears nothing it was not asked to.
//!
//! Our own frame ([`crate::draw_to_surface`], [`crate::draw_to_texture`]) is
//! one such host: it owns a [`crate::FrameTargets`] and asks for a clear. It
//! goes through the same [`HostPass`], so the pass a game engine gets is the
//! pass the viewer and every GPU test draws through.
//!
//! # A depth buffer without a stencil
//!
//! The ground's ownership mark — the one bit that keeps a fallback ancestor
//! from taking a pixel from the surface that owns it (see `TileRenderer`) — is
//! a stencil. A host whose depth format carries one lends it for the pass. A
//! host whose depth has none (a game engine's `Depth32Float`) cannot, and
//! drawing without the mark is not an option: it is the artefact
//! `tests/no_two_surfaces.rs` measured at every pixel.
//!
//! So there tuile brings a depth-stencil buffer of its own, the
//! **companion**, and the host's depth crosses into it and back:
//!
//! 1. the pass opens on the host's colour and the companion, and its first
//!    draw copies the host's depth into the companion, sample for sample;
//! 2. the ground is drawn — tested against the host's depth, so what the host
//!    drew in front of it stays in front, and marking ownership in the
//!    companion's stencil;
//! 3. a second pass writes the companion's depth back into the host's
//!    wherever it is nearer: whatever the host draws next is hidden by the
//!    ground.
//!
//! Which path a host takes follows from its depth format, not from a setting.
//!
//! The host's depth must then be bindable: created with `TEXTURE_BINDING`
//! beside `RENDER_ATTACHMENT`. A multisampled one is crossed sample for
//! sample, which reads a multisampled depth texture and shades per sample —
//! WebGPU has both; WebGL2 has neither, so on WebGL2 a stencil-less host
//! draws single-sampled (the single-sampled crossing translates to GLSL ES
//! 3.00: `webgl2.rs`).
//!
//! Ground drawn by a fallback ancestor tests depth and never writes it — in
//! a host's frame as in our own — so it does not reach the host's depth.

use crate::context::{GpuContext, DEPTH_FORMAT, SAMPLES};
use glam::{DMat4, DVec3};

/// Which way a host's depth runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DepthDirection {
    /// 0 at the near plane, 1 at the far one: our own frame's.
    Forward,
    /// 1 at the near plane, 0 at the far one (or at infinity): what an
    /// engine with an infinite far plane uses, for its precision far away.
    Reverse,
}

impl DepthDirection {
    /// The depth a cleared buffer holds: nothing drawn yet, as far as can be.
    pub fn far(self) -> f32 {
        match self {
            Self::Forward => 1.0,
            Self::Reverse => 0.0,
        }
    }

    /// Passes when the fragment is strictly nearer than what is there.
    pub(crate) fn nearer(self) -> wgpu::CompareFunction {
        match self {
            Self::Forward => wgpu::CompareFunction::Less,
            Self::Reverse => wgpu::CompareFunction::Greater,
        }
    }

    /// Passes when the fragment is nearer than, or level with, what is there.
    pub(crate) fn nearer_or_level(self) -> wgpu::CompareFunction {
        match self {
            Self::Forward => wgpu::CompareFunction::LessEqual,
            Self::Reverse => wgpu::CompareFunction::GreaterEqual,
        }
    }
}

/// The shape of the targets a frame is drawn into: what every pipeline drawing
/// into them is built against, and must match exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TargetLayout {
    /// The colour attachment's format.
    pub colour: wgpu::TextureFormat,
    /// The host's depth format, with or without a stencil.
    pub depth: wgpu::TextureFormat,
    /// Samples per pixel of both attachments.
    pub samples: u32,
    pub direction: DepthDirection,
}

impl TargetLayout {
    /// Our own frame's layout ([`crate::FrameTargets`]): forward depth with a
    /// stencil, [`SAMPLES`] samples.
    pub fn own(colour: wgpu::TextureFormat) -> Self {
        Self {
            colour,
            depth: DEPTH_FORMAT,
            samples: SAMPLES,
            direction: DepthDirection::Forward,
        }
    }

    /// Whether the host's depth carries the stencil the ownership mark needs,
    /// so tuile draws in it directly.
    pub fn lends_stencil(&self) -> bool {
        self.depth.has_stencil_aspect()
    }

    /// The depth format tuile's own passes are built against: the host's when
    /// it carries a stencil, the companion's otherwise — an exact float depth
    /// where the device has it, so the host's depth crosses back unchanged.
    pub fn drawn_depth(&self, device: &wgpu::Device) -> wgpu::TextureFormat {
        if self.lends_stencil() {
            self.depth
        } else if device
            .features()
            .contains(wgpu::Features::DEPTH32FLOAT_STENCIL8)
        {
            wgpu::TextureFormat::Depth32FloatStencil8
        } else {
            wgpu::TextureFormat::Depth24PlusStencil8
        }
    }
}

/// The view-projection the tiles are drawn with, from a host's camera.
///
/// `clip_from_world` is the host's projection times its view, in f64 and in the
/// frame tuile's tiles are in (ECEF for a globe); `render_origin` is the pump's
/// ([`crate::ContentPump::render_origin`]). The translation to the origin is
/// folded in **before** the cast to f32, so the matrix the GPU sees holds
/// metres from the origin, not from the Earth's centre — the f32 cast of a raw
/// ECEF matrix is the jitter this project forbids.
pub fn view_proj_from_world(clip_from_world: DMat4, render_origin: DVec3) -> [f32; 16] {
    (clip_from_world * DMat4::from_translation(render_origin))
        .as_mat4()
        .to_cols_array()
}

/// One frame's attachments, the host's.
pub struct HostFrame<'a> {
    /// Drawn into, with [`TargetLayout::colour`] and
    /// [`TargetLayout::samples`].
    pub colour: &'a wgpu::TextureView,
    /// What a multisampled `colour` resolves into, if anything.
    pub resolve: Option<&'a wgpu::TextureView>,
    /// The host's depth, [`TargetLayout::depth`]. Without a stencil it is
    /// also read: it needs `TEXTURE_BINDING` beside `RENDER_ATTACHMENT`.
    pub depth: &'a wgpu::TextureView,
    /// Both attachments' size, device pixels.
    pub size: (u32, u32),
    /// `None` keeps the host's picture and depth and draws over them. `Some`
    /// starts the frame afresh: the colour cleared to it, the depth to
    /// [`DepthDirection::far`].
    pub clear: Option<wgpu::Color>,
}

/// tuile's pass in a host's frame, for one [`TargetLayout`].
///
/// Built once per layout and kept: it holds the companion depth and the two
/// small pipelines that cross the depth over, when the host's depth has no
/// stencil.
pub struct HostPass {
    layout: TargetLayout,
    crossing: Option<Crossing>,
}

impl HostPass {
    pub fn new(gpu: &GpuContext, layout: TargetLayout) -> Self {
        let crossing = (!layout.lends_stencil()).then(|| Crossing::new(gpu, layout));
        Self { layout, crossing }
    }

    pub fn layout(&self) -> TargetLayout {
        self.layout
    }

    /// Records tuile's pass into `encoder`: `paint` receives a render pass on
    /// the host's colour, depth-tested against the host's depth, with the
    /// stencil the tile pipelines expect. Nothing is submitted.
    pub fn encode(
        &mut self,
        gpu: &GpuContext,
        encoder: &mut wgpu::CommandEncoder,
        frame: HostFrame<'_>,
        paint: impl FnOnce(&mut wgpu::RenderPass<'_>),
    ) {
        let layout = self.layout;
        let colour_load = match frame.clear {
            Some(c) => wgpu::LoadOp::Clear(c),
            None => wgpu::LoadOp::Load,
        };
        let depth_load = match frame.clear {
            Some(_) => wgpu::LoadOp::Clear(layout.direction.far()),
            None => wgpu::LoadOp::Load,
        };
        let colour = |load| {
            [Some(wgpu::RenderPassColorAttachment {
                view: frame.colour,
                resolve_target: frame.resolve,
                depth_slice: None,
                ops: wgpu::Operations {
                    load,
                    store: wgpu::StoreOp::Store,
                },
            })]
        };
        // Cleared to 0, and never read back: the mark says "a surface that
        // owns this ground drew here", which is only true within one frame.
        let stencil = Some(wgpu::Operations {
            load: wgpu::LoadOp::Clear(0),
            store: wgpu::StoreOp::Discard,
        });
        let Some(crossing) = self.crossing.as_mut() else {
            // The host lends its stencil: one pass, straight into its depth.
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("tuile frame"),
                color_attachments: &colour(colour_load),
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: frame.depth,
                    depth_ops: Some(wgpu::Operations {
                        load: depth_load,
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: stencil,
                }),
                ..Default::default()
            });
            paint(&mut pass);
            return;
        };
        crossing.fit(gpu, frame.size);
        let companion = crossing.companion.as_ref().expect("fitted");
        {
            let import = frame
                .clear
                .is_none()
                .then(|| crossing.import_group(gpu, frame.depth));
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("tuile frame"),
                color_attachments: &colour(colour_load),
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &companion.attachment,
                    depth_ops: Some(wgpu::Operations {
                        // Written in full by the import below, or — on a
                        // fresh frame — cleared to far.
                        load: wgpu::LoadOp::Clear(layout.direction.far()),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: stencil,
                }),
                ..Default::default()
            });
            if let Some(group) = &import {
                pass.set_pipeline(&crossing.import);
                pass.set_bind_group(0, group, &[]);
                pass.draw(0..3, 0..1);
            }
            paint(&mut pass);
        }
        // The ground back into the host's depth, where it is nearer.
        let group = crossing.export_group(gpu);
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("tuile depth to host"),
            color_attachments: &[],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: frame.depth,
                depth_ops: Some(wgpu::Operations {
                    load: depth_load,
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            ..Default::default()
        });
        pass.set_pipeline(&crossing.export);
        pass.set_bind_group(0, &group, &[]);
        pass.draw(0..3, 0..1);
    }
}

/// The companion depth-stencil, and the two passes that carry a depth across.
struct Crossing {
    layout: TargetLayout,
    format: wgpu::TextureFormat,
    group_layout: wgpu::BindGroupLayout,
    /// Host depth → companion, inside tuile's pass.
    import: wgpu::RenderPipeline,
    /// Companion → host depth, where nearer.
    export: wgpu::RenderPipeline,
    companion: Option<Companion>,
}

struct Companion {
    size: (u32, u32),
    attachment: wgpu::TextureView,
    /// Its depth alone, as the export reads it.
    depth_only: wgpu::TextureView,
}

impl Crossing {
    fn new(gpu: &GpuContext, layout: TargetLayout) -> Self {
        let format = layout.drawn_depth(&gpu.device);
        let multisampled = layout.samples > 1;
        let group_layout = gpu
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("tuile depth crossing"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        // Read as plain floats, not as a depth texture:
                        // the same texels, and what GLSL ES 3.00 can fetch.
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled,
                    },
                    count: None,
                }],
            });
        let shader = gpu
            .device
            .create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("tuile depth crossing"),
                source: wgpu::ShaderSource::Wgsl(crossing_wgsl(multisampled).into()),
            });
        let pipeline_layout = gpu
            .device
            .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("tuile depth crossing"),
                bind_group_layouts: &[Some(&group_layout)],
                immediate_size: 0,
            });
        let pipeline = |label, depth_format, compare, colour: Option<wgpu::TextureFormat>| {
            let targets = [colour.map(|format| wgpu::ColorTargetState {
                format,
                blend: None,
                // The import shares tuile's pass with the host's colour and
                // must not touch it.
                write_mask: wgpu::ColorWrites::empty(),
            })];
            gpu.device
                .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some(label),
                    layout: Some(&pipeline_layout),
                    vertex: wgpu::VertexState {
                        module: &shader,
                        entry_point: Some("vs_main"),
                        compilation_options: Default::default(),
                        buffers: &[],
                    },
                    primitive: Default::default(),
                    depth_stencil: Some(wgpu::DepthStencilState {
                        format: depth_format,
                        depth_write_enabled: Some(true),
                        depth_compare: Some(compare),
                        stencil: Default::default(),
                        bias: Default::default(),
                    }),
                    multisample: wgpu::MultisampleState {
                        count: layout.samples,
                        ..Default::default()
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &shader,
                        entry_point: Some("fs_main"),
                        compilation_options: Default::default(),
                        targets: if colour.is_some() { &targets } else { &[] },
                    }),
                    multiview_mask: None,
                    cache: None,
                })
        };
        let import = pipeline(
            "tuile depth from host",
            format,
            wgpu::CompareFunction::Always,
            Some(layout.colour),
        );
        let export = pipeline(
            "tuile depth to host",
            layout.depth,
            layout.direction.nearer(),
            None,
        );
        Self {
            layout,
            format,
            group_layout,
            import,
            export,
            companion: None,
        }
    }

    /// The companion at the frame's size, rebuilt only when it changes.
    fn fit(&mut self, gpu: &GpuContext, size: (u32, u32)) {
        let size = (size.0.max(1), size.1.max(1));
        if self.companion.as_ref().is_some_and(|c| c.size == size) {
            return;
        }
        let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("tuile companion depth"),
            size: wgpu::Extent3d {
                width: size.0,
                height: size.1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: self.layout.samples,
            dimension: wgpu::TextureDimension::D2,
            format: self.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        self.companion = Some(Companion {
            size,
            attachment: texture.create_view(&Default::default()),
            depth_only: texture.create_view(&wgpu::TextureViewDescriptor {
                aspect: wgpu::TextureAspect::DepthOnly,
                ..Default::default()
            }),
        });
    }

    fn group(&self, gpu: &GpuContext, view: &wgpu::TextureView) -> wgpu::BindGroup {
        gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("tuile depth crossing"),
            layout: &self.group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(view),
            }],
        })
    }

    /// Made each frame: the host may hand a different depth every frame (a
    /// resize, a ping-pong of its own), and a bind group is cheap.
    fn import_group(&self, gpu: &GpuContext, host_depth: &wgpu::TextureView) -> wgpu::BindGroup {
        self.group(gpu, host_depth)
    }

    fn export_group(&self, gpu: &GpuContext) -> wgpu::BindGroup {
        let companion = self.companion.as_ref().expect("fitted");
        self.group(gpu, &companion.depth_only)
    }
}

/// A full-screen triangle whose fragments take their depth from a depth
/// texture of the same size and sample count, sample for sample.
pub(crate) fn crossing_wgsl(multisampled: bool) -> String {
    let (texture, sample_in, sample) = if multisampled {
        (
            "texture_multisampled_2d<f32>",
            ", @builtin(sample_index) s: u32",
            "i32(s)",
        )
    } else {
        ("texture_2d<f32>", "", "0")
    };
    format!(
        "@group(0) @binding(0) var depth_in: {texture};

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> @builtin(position) vec4f {{
    let uv = vec2f(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4f(uv * 2.0 - 1.0, 0.0, 1.0);
}}

@fragment
fn fs_main(@builtin(position) p: vec4f{sample_in}) -> @builtin(frag_depth) f32 {{
    return textureLoad(depth_in, vec2i(p.xy), {sample}).r;
}}
"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Our own frame lends its stencil; a stencil-less depth gets a companion
    /// that has one.
    #[test]
    fn only_a_stencil_less_depth_needs_a_companion() {
        assert!(TargetLayout::own(wgpu::TextureFormat::Rgba8UnormSrgb).lends_stencil());
        let engine = TargetLayout {
            colour: wgpu::TextureFormat::Rgba16Float,
            depth: wgpu::TextureFormat::Depth32Float,
            samples: 4,
            direction: DepthDirection::Reverse,
        };
        assert!(!engine.lends_stencil());
    }

    /// Reverse-Z flips every comparison and the far value; forward keeps what
    /// our own pipelines always used.
    #[test]
    fn the_direction_decides_nearer_and_far() {
        assert_eq!(DepthDirection::Forward.far(), 1.0);
        assert_eq!(DepthDirection::Reverse.far(), 0.0);
        assert_eq!(
            DepthDirection::Forward.nearer(),
            wgpu::CompareFunction::Less
        );
        assert_eq!(
            DepthDirection::Reverse.nearer_or_level(),
            wgpu::CompareFunction::GreaterEqual
        );
    }

    /// The origin is folded in at f64: a point at the origin lands where the
    /// host's camera puts the world point, however far from the centre.
    #[test]
    fn the_view_projection_is_relative_to_the_render_origin() {
        let origin = DVec3::new(4_000_000.0, 3_000_000.0, 3_500_000.0);
        let clip_from_world = glam::dcamera::rh::proj::directx::perspective(1.0, 1.0, 1.0, 1e8)
            * glam::dcamera::rh::view::look_at_mat4(origin * 1.1, origin, DVec3::Z);
        let m = glam::Mat4::from_cols_array(&view_proj_from_world(clip_from_world, origin));
        let expected = clip_from_world * origin.extend(1.0);
        let got = (m * glam::Vec4::new(0.0, 0.0, 0.0, 1.0)).as_dvec4();
        let ndc = |c: glam::DVec4| c.truncate() / c.w;
        assert!((ndc(got) - ndc(expected)).abs().max_element() < 1e-5);
    }
}
