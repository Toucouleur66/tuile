// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The textures a frame is drawn into, before it reaches a surface.
//!
//! Every wgpu host writes these two by hand and writes them the same way, and
//! the one detail that is easy to get wrong is the one that fails loudly: the
//! sample count of the colour target, the depth target and the pipelines must
//! agree exactly, or the first draw call fails validation rather than merely
//! looking wrong. Four separate test harnesses in this repository were silently
//! failing that check for weeks, each having been written before MSAA reached
//! the renderer, and each reporting a wgpu validation panic instead of whatever
//! it was actually there to measure.
//!
//! Owning the pair here is what makes that impossible to get wrong: the count
//! comes from [`crate::SAMPLES`], which is the same constant the pipelines are
//! built against.
//!
//! # Kept for one pass
//!
//! Both are written by one pass and read by nothing after it but the
//! resolve: the frame's pass discards them ([`crate::HostPass`]'s `Kept`).
//! Where wgpu can, they are also **transient**: on a tiled GPU (Apple's, a
//! phone's) they then live in tile memory alone and are never allocated at
//! all — about 110 MB of depth and stencil and 55 MB of colour at
//! 2400×1436 with four samples. The one exception is a pass after the frame that loads the
//! samples to draw over them (the overlays with a depth of their own): that
//! frame draws into a second, kept colour target, made the first time one
//! is asked for ([`FrameTargets::samples`]). A pass after the frame that
//! reads its depth in a shader (the mist) likewise gets a second, kept and
//! bindable depth, made the first time one is asked for.
//!
//! The surface itself is **not** here. This crate creates no window and no
//! surface — the host owns those, and owns the resolve into them.

use crate::context::{GpuContext, DEPTH_FORMAT, SAMPLES};
use crate::encode::Encode;
use crate::surface::SurfaceFormat;

/// The multisampled colour target and its depth buffer.
pub struct FrameTargets {
    /// Draw into this; resolve into the surface texture. Transient where the
    /// device allows it: cleared, drawn, resolved and discarded by one pass.
    pub colour: wgpu::TextureView,
    /// Transient like `colour`, and discarded by the frame's pass.
    pub depth: wgpu::TextureView,
    /// The colour a frame draws into when a pass after it loads its samples:
    /// made when first asked for, and `colour` itself where that is not
    /// transient.
    kept: std::sync::OnceLock<wgpu::TextureView>,
    /// The depth a frame draws into when a pass after it reads the depth in
    /// a shader (the mist, [`crate::MistRenderer`]): made when first asked
    /// for, never transient, and bindable. Never made where the device
    /// cannot bind a multisampled depth, which WebGL2 cannot.
    kept_depth: std::sync::OnceLock<KeptDepth>,
    format: wgpu::TextureFormat,
    size: (u32, u32),
    /// The surface's format, when the frame reaches it through an encoding
    /// pass rather than straight from the resolve: see [`crate::encode`].
    surface: Option<wgpu::TextureFormat>,
    pub(crate) encode: Option<Encode>,
}

impl FrameTargets {
    /// `size` is in **device** pixels — what the surface is configured with,
    /// not what a stylesheet would call a point.
    pub fn new(gpu: &GpuContext, format: wgpu::TextureFormat, size: (u32, u32)) -> Self {
        let size = (size.0.max(1), size.1.max(1));
        Self {
            colour: attachment(gpu, format, size, "tuile frame colour", TRANSIENT),
            depth: attachment(gpu, DEPTH_FORMAT, size, "tuile frame depth", TRANSIENT),
            kept: std::sync::OnceLock::new(),
            kept_depth: std::sync::OnceLock::new(),
            format,
            size,
            surface: None,
            encode: None,
        }
    }

    /// The targets for drawing onto a surface of `format`: those of
    /// [`FrameTargets::new`], plus — where the surface can neither be sRGB
    /// nor be viewed as sRGB — the texture the frame is resolved into and the
    /// pass that encodes it into the surface.
    pub fn for_surface(gpu: &GpuContext, format: SurfaceFormat, size: (u32, u32)) -> Self {
        let mut targets = Self::new(gpu, format.render, size);
        if format.encode {
            targets.surface = Some(format.surface);
            targets.encode = Some(Encode::new(
                gpu,
                format.render,
                format.surface,
                targets.size,
            ));
        }
        targets
    }

    /// Rebuilds both targets for a new size, and says whether it had to.
    ///
    /// Cheap to call every frame: a resize that is not a resize does nothing,
    /// which means a host can simply hand over the current window size instead
    /// of tracking whether it changed.
    pub fn resize(&mut self, gpu: &GpuContext, size: (u32, u32)) -> bool {
        let size = (size.0.max(1), size.1.max(1));
        if size == self.size {
            return false;
        }
        *self = match self.surface {
            Some(surface) => Self::for_surface(
                gpu,
                SurfaceFormat {
                    surface,
                    render: self.format,
                    encode: true,
                    // Not the targets' business: only the surface's
                    // configuration reads it.
                    alpha: wgpu::CompositeAlphaMode::Auto,
                },
                size,
            ),
            None => Self::new(gpu, self.format, size),
        };
        true
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// The multisampled colour a frame draws into: [`FrameTargets::colour`],
    /// or — when a pass after the frame loads its samples (`kept`) — a target
    /// that outlives the frame's pass.
    pub(crate) fn samples(&self, gpu: &GpuContext, kept: bool) -> &wgpu::TextureView {
        if !kept || !TRANSIENT {
            return &self.colour;
        }
        self.kept.get_or_init(|| {
            attachment(
                gpu,
                self.format,
                self.size,
                "tuile frame colour (kept)",
                false,
            )
        })
    }

    /// The depth a frame draws into when a pass after it reads that depth
    /// in a shader, with its depth-only view for binding — or `None` where
    /// the device cannot bind a multisampled depth ([`crate::MistRenderer::supported`]).
    pub(crate) fn readable_depth(&self, gpu: &GpuContext) -> Option<&KeptDepth> {
        if !crate::MistRenderer::supported(gpu) {
            return None;
        }
        Some(self.kept_depth.get_or_init(|| {
            let depth = texture(
                gpu,
                DEPTH_FORMAT,
                self.size,
                "tuile frame depth (kept)",
                wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            );
            KeptDepth {
                attachment: depth.create_view(&wgpu::TextureViewDescriptor::default()),
                binding: depth.create_view(&wgpu::TextureViewDescriptor {
                    label: Some("tuile frame depth (read)"),
                    aspect: wgpu::TextureAspect::DepthOnly,
                    ..Default::default()
                }),
            }
        }))
    }

    /// The colour format the frame is drawn in — and so the format the
    /// drawable it resolves into is viewed as.
    pub fn format(&self) -> wgpu::TextureFormat {
        self.format
    }
}

/// A depth that outlives the frame's pass: drawn into as its attachment,
/// read by a pass after it through `binding`, its depth aspect alone.
pub(crate) struct KeptDepth {
    pub attachment: wgpu::TextureView,
    pub binding: wgpu::TextureView,
}

/// Whether the frame's attachments are made transient.
///
/// Every native backend accepts the usage — memoryless on Apple GPUs, lazily
/// allocated where Vulkan has such memory, a plain texture elsewhere — and
/// validates the discard it requires, so a stored or loaded frame attachment
/// fails on every native test run rather than only on a phone. A browser's
/// WebGPU may not know the usage bit yet, and a texture it refuses is a black
/// frame: on the web the attachments stay plain (and still discarded).
const TRANSIENT: bool = !cfg!(target_arch = "wasm32");

fn attachment(
    gpu: &GpuContext,
    format: wgpu::TextureFormat,
    size: (u32, u32),
    label: &str,
    transient: bool,
) -> wgpu::TextureView {
    texture(
        gpu,
        format,
        size,
        label,
        if transient {
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TRANSIENT_ATTACHMENT
        } else {
            wgpu::TextureUsages::RENDER_ATTACHMENT
        },
    )
    .create_view(&wgpu::TextureViewDescriptor::default())
}

fn texture(
    gpu: &GpuContext,
    format: wgpu::TextureFormat,
    size: (u32, u32),
    label: &str,
    usage: wgpu::TextureUsages,
) -> wgpu::Texture {
    gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: size.0,
            height: size.1,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        // The one number that must match the pipelines. See the module doc.
        sample_count: SAMPLES,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage,
        view_formats: &[],
    })
}
