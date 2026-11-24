// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What a frame keeps of its multisampled attachments: the resolve, and
//! nothing else unless a pass after it draws over the same samples.
//!
//! The frame's samples and its depth are written for one pass and resolved
//! once. Storing them costs a full-size write per frame — four samples a
//! pixel, colour and depth — that nothing reads; on a tiled GPU it is most of
//! what a frame costs the memory bus. So the frame discards them, and where
//! the device allows it they never leave the tile at all (transient
//! attachments, memoryless on Apple GPUs).
//!
//! What must survive is the picture, both when the frame stands alone and
//! when a pass after it (the overlays with a depth of their own) loads the
//! samples to draw over them. On native backends wgpu validates the two
//! ways of getting this wrong: a transient attachment stored, or loaded by
//! the pass after. On every backend the picture is read back.

use tuile_wgpu::{
    draw_to_texture, FrameOnTexture, FramePass, FrameTargets, GpuContext, Readback, NOTHING_AFTER,
    SAMPLES, TEXTURE_FORMAT,
};

const WIDTH: u32 = 64;
const HEIGHT: u32 = 32;

const RED: wgpu::Color = wgpu::Color {
    r: 1.0,
    g: 0.0,
    b: 0.0,
    a: 1.0,
};

fn gpu() -> Option<GpuContext> {
    match pollster::block_on(GpuContext::headless()) {
        Ok(gpu) => Some(gpu),
        Err(e) => {
            assert!(
                std::env::var("TUILE_REQUIRE_GPU").is_err(),
                "TUILE_REQUIRE_GPU is set and there is no usable adapter: {e}"
            );
            eprintln!("skipping GPU test: no usable adapter ({e})");
            None
        }
    }
}

fn texture(gpu: &GpuContext) -> wgpu::Texture {
    gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("frame attachments test"),
        size: wgpu::Extent3d {
            width: WIDTH,
            height: HEIGHT,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: TEXTURE_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

/// The pixel at `(x, y)`, RGBA8.
fn at(pixels: &[u8], x: u32, y: u32) -> [u8; 4] {
    let i = ((y * WIDTH + x) * 4) as usize;
    [pixels[i], pixels[i + 1], pixels[i + 2], pixels[i + 3]]
}

/// Green over a full-screen triangle, multisampled as the frame is: what the
/// pass after the frame draws.
fn green(gpu: &GpuContext) -> wgpu::RenderPipeline {
    let module = gpu
        .device
        .create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("frame attachments test"),
            source: wgpu::ShaderSource::Wgsl(
                "@vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
                     let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
                     return vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
                 }
                 @fragment fn fs() -> @location(0) vec4<f32> {
                     return vec4<f32>(0.0, 1.0, 0.0, 1.0);
                 }"
                .into(),
            ),
        });
    gpu.device
        .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("frame attachments test"),
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
                targets: &[Some(TEXTURE_FORMAT.into())],
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState {
                count: SAMPLES,
                ..Default::default()
            },
            multiview_mask: None,
            cache: None,
        })
}

/// A frame with nothing after it: its samples and depth are discarded, and
/// the picture is still the frame's.
#[test]
fn a_frame_alone_resolves_its_picture() {
    let Some(gpu) = gpu() else { return };
    let targets = FrameTargets::new(&gpu, TEXTURE_FORMAT, (WIDTH, HEIGHT));
    let texture = texture(&gpu);
    let mut readback = Readback::default();
    // Twice: the second frame draws into attachments the first discarded.
    for _ in 0..2 {
        draw_to_texture(
            FrameOnTexture {
                gpu: &gpu,
                texture: &texture,
                targets: &targets,
                clear: RED,
                capture: Some(&mut readback),
            },
            |_| {},
            NOTHING_AFTER,
        );
    }
    let pixels = readback.take(&gpu, (WIDTH, HEIGHT)).expect("read back");
    for (x, y) in [(0, 0), (WIDTH - 1, HEIGHT - 1), (WIDTH / 2, HEIGHT / 2)] {
        assert_eq!(at(&pixels, x, y), [255, 0, 0, 255], "pixel ({x}, {y})");
    }
}

/// A pass after the frame loads its samples and draws over half of them:
/// the other half is the frame's own picture, so the samples were kept for
/// it.
#[test]
fn a_pass_after_the_frame_draws_over_the_frames_samples() {
    let Some(gpu) = gpu() else { return };
    let targets = FrameTargets::new(&gpu, TEXTURE_FORMAT, (WIDTH, HEIGHT));
    let texture = texture(&gpu);
    let pipeline = green(&gpu);
    let mut readback = Readback::default();
    let after = |encoder: &mut wgpu::CommandEncoder, frame: &FramePass<'_>| {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("after the frame"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: frame.colour,
                resolve_target: Some(frame.resolve),
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        pass.set_scissor_rect(0, 0, WIDTH / 2, HEIGHT);
        pass.set_pipeline(&pipeline);
        pass.draw(0..3, 0..1);
    };
    for _ in 0..2 {
        draw_to_texture(
            FrameOnTexture {
                gpu: &gpu,
                texture: &texture,
                targets: &targets,
                clear: RED,
                capture: Some(&mut readback),
            },
            |_| {},
            Some(after),
        );
    }
    let pixels = readback.take(&gpu, (WIDTH, HEIGHT)).expect("read back");
    assert_eq!(at(&pixels, 1, 1), [0, 255, 0, 255], "the pass after drew");
    assert_eq!(
        at(&pixels, WIDTH - 2, HEIGHT - 2),
        [255, 0, 0, 255],
        "the frame's picture under the pass after"
    );
}

/// A frame without a pass after it, then one with, then without again, on
/// the same targets: the viewer's frame when a host's overlays come and go.
#[test]
fn a_pass_after_can_come_and_go() {
    let Some(gpu) = gpu() else { return };
    let targets = FrameTargets::new(&gpu, TEXTURE_FORMAT, (WIDTH, HEIGHT));
    let texture = texture(&gpu);
    let pipeline = green(&gpu);
    let after = |encoder: &mut wgpu::CommandEncoder, frame: &FramePass<'_>| {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("after the frame"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: frame.colour,
                resolve_target: Some(frame.resolve),
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        pass.set_scissor_rect(0, 0, WIDTH / 2, HEIGHT);
        pass.set_pipeline(&pipeline);
        pass.draw(0..3, 0..1);
    };
    for with_after in [false, true, false] {
        let mut readback = Readback::default();
        draw_to_texture(
            FrameOnTexture {
                gpu: &gpu,
                texture: &texture,
                targets: &targets,
                clear: RED,
                capture: Some(&mut readback),
            },
            |_| {},
            with_after.then_some(after),
        );
        let pixels = readback.take(&gpu, (WIDTH, HEIGHT)).expect("read back");
        let left = if with_after { [0, 255, 0, 255] } else { [255, 0, 0, 255] };
        assert_eq!(at(&pixels, 1, 1), left, "with a pass after: {with_after}");
        assert_eq!(at(&pixels, WIDTH - 2, HEIGHT - 2), [255, 0, 0, 255]);
    }
}
