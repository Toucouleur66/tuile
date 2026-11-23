// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! **The tiles' outlines sit on the real picture, imagery and all.**
//!
//! `view.params.z` (the viewer's `tuile://view?labels=on`) draws each tile's
//! edge — a dark pixel and a light one beside it — over the textured, lit
//! picture, so the levels written on the tiles can be read against the
//! imagery each one wears. The diagnostic views that colour the tiles flat
//! stay what they are, and are not this.
//!
//! Two claims, against read-back pixels of one quad wearing one red layer:
//!
//! | claim | what breaking it looks like |
//! |---|---|
//! | with the outlines, the tile's edge is drawn | no edge: the labels float over tiles that cannot be told apart |
//! | inside the edge the imagery is untouched, and without the outlines nothing changes | the picture recoloured, which is the diagnostic view again |
//!
//! A machine without a usable adapter skips, with a printed reason. Set
//! `TUILE_REQUIRE_GPU=1` to turn that skip into a failure.

mod support;

use support::{gpu_with, painted};

use glam::{DVec3, Mat4, Vec3};
use std::sync::Arc;
use tuile_core::content::{DecodedMesh, DecodedTileContent, MaterialDesc};
use tuile_core::raster::{ImageryCoord, ImageryLayer};
use tuile_wgpu::{
    prepare, GpuContext, TileRenderer, ViewUniform, DEPTH_FORMAT, SAMPLES, TEXTURE_FORMAT,
};

const SIZE: u32 = 256;

/// A unit quad in the z = 0 plane, facing the camera, with uv over `[0, 1]²`.
///
/// Deliberately not terrain. What is being measured is which *layer* reaches a
/// fragment, and a quantized mesh would bring its own relief, its own skirts and
/// its own reprojection into a question that has nothing to do with any of them.
fn quad() -> DecodedTileContent {
    DecodedTileContent {
        shed: Vec::new(),
        apart: None,
        withheld_drape: None,
        copyright: None,
        meshes: vec![DecodedMesh {
            positions: vec![
                [-1.0, -1.0, 0.0],
                [1.0, -1.0, 0.0],
                [1.0, 1.0, 0.0],
                [-1.0, 1.0, 0.0],
            ],
            normals: Some(vec![[0.0, 0.0, 1.0]; 4]),
            uvs: Some(vec![[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]]),
            indices: vec![0, 1, 2, 0, 2, 3],
            material: MaterialDesc {
                base_color_factor: [1.0, 1.0, 1.0, 1.0],
                base_color_texture: None,
                unlit: false,
            },
        }],
        textures: Vec::new(),
        imagery: Vec::new(),
        local_origin_ecef: DVec3::ZERO,
        transform_local: Mat4::IDENTITY,
    }
}

/// A layer of one flat colour over `coverage`, in the tile's own uv space:
/// 256², whole blocks, held as `gpu`'s plan says.
fn layer(gpu: &GpuContext, index: u32, colour: [u8; 3], coverage: [f32; 4]) -> ImageryLayer {
    ImageryLayer {
        source: 0,
        // Distinct per layer: the shared-imagery pool is keyed by coord, so two
        // layers claiming the same one would be the same texture and the test
        // would compare a colour with itself.
        coord: ImageryCoord {
            level: 1,
            x: u64::from(index),
            y: 0,
        },
        texture: Arc::new(painted(gpu, 256, 256, |_, _| {
            [colour[0], colour[1], colour[2], 255]
        })),
        coverage,
        // Maps the covered part of the tile onto the whole texture. Immaterial
        // for a flat colour, and written correctly anyway so the fixture does
        // not quietly depend on the placement being ignored.
        translation: [-coverage[0] / (coverage[2] - coverage[0]), 0.0],
        scale: [1.0 / (coverage[2] - coverage[0]), 1.0],
    }
}

/// Renders the quad head-on with the view's `params` and returns the
/// resolved frame, RGBA8.
fn render(gpu: &GpuContext, content: &DecodedTileContent, params: [f32; 4]) -> Vec<u8> {
    let make = |label, format, usage, samples| {
        gpu.device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d {
                width: SIZE,
                height: SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: samples,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage,
            view_formats: &[],
        })
    };
    // Multisampled, because the pipelines are built for `SAMPLES` and a
    // single-sample pass will not accept them.
    let msaa = make(
        "passes msaa",
        TEXTURE_FORMAT,
        wgpu::TextureUsages::RENDER_ATTACHMENT,
        SAMPLES,
    );
    let msaa_view = msaa.create_view(&wgpu::TextureViewDescriptor::default());
    let target = make(
        "passes target",
        TEXTURE_FORMAT,
        wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        1,
    );
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let depth = make(
        "passes depth",
        DEPTH_FORMAT,
        wgpu::TextureUsages::RENDER_ATTACHMENT,
        SAMPLES,
    );
    let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());

    let renderer = TileRenderer::new(gpu, TEXTURE_FORMAT);
    let view_m =
        glam::camera::rh::view::look_at_mat4(Vec3::new(0.0, 0.0, 2.0), Vec3::ZERO, Vec3::Y);
    let proj = glam::camera::rh::proj::directx::perspective(60f32.to_radians(), 1.0, 0.1, 10.0);
    renderer.set_view(
        &gpu.queue,
        &ViewUniform {
            view_proj: (proj * view_m).to_cols_array(),
            params,
            ..Default::default()
        },
    );

    let tile = prepare(gpu, content, DVec3::ZERO);
    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("passes"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &msaa_view,
                depth_slice: None,
                resolve_target: Some(&view),
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &depth_view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Store,
                }),
                stencil_ops: Some(wgpu::Operations {
                    // Cleared to 0, and never read back: the mark says "a
                    // surface that owns this ground drew here", which is only
                    // true within one frame.
                    load: wgpu::LoadOp::Clear(0),
                    store: wgpu::StoreOp::Discard,
                }),
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        renderer.render(&mut pass, std::iter::once(&tile), false);
    }

    let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("passes readback"),
        size: (SIZE * SIZE * 4) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(SIZE * 4),
                rows_per_image: Some(SIZE),
            },
        },
        wgpu::Extent3d {
            width: SIZE,
            height: SIZE,
            depth_or_array_layers: 1,
        },
    );
    gpu.queue.submit([encoder.finish()]);

    let slice = readback.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    gpu.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    let pixels = slice.get_mapped_range().expect("map range").to_vec();
    readback.unmap();
    pixels
}

const RED: [u8; 3] = [220, 20, 20];

fn red_quad(gpu: &GpuContext) -> DecodedTileContent {
    let mut content = quad();
    content.imagery = vec![layer(gpu, 0, RED, [0.0, 0.0, 1.0, 1.0])];
    content
}

fn at(pixels: &[u8], col: u32, row: u32) -> [u8; 3] {
    let i = ((row * SIZE + col) * 4) as usize;
    [pixels[i], pixels[i + 1], pixels[i + 2]]
}

fn close(got: [u8; 3], want: [u8; 3]) -> bool {
    (0..3).all(|c| (i32::from(got[c]) - i32::from(want[c])).abs() <= 12)
}

/// Along the middle row, from the left: the first pixels of the quad (the
/// first not black), five of them.
fn quad_edge(pixels: &[u8]) -> Vec<[u8; 3]> {
    let row = SIZE / 2;
    let first = (0..SIZE)
        .find(|&c| at(pixels, c, row).iter().any(|v| *v > 8))
        .expect("the quad is drawn");
    (first..first + 5).map(|c| at(pixels, c, row)).collect()
}

/// A light pixel: every channel high — never the red imagery.
fn light(p: [u8; 3]) -> bool {
    p.iter().all(|v| *v > 150)
}

#[test]
fn the_outlines_are_drawn_over_the_imagery_and_leave_it_alone() {
    let Some(gpu) = gpu_with(tuile_wgpu::ImageryCompression::Off) else {
        return;
    };
    let content = red_quad(&gpu);
    let plain = render(&gpu, &content, [1.0, 0.0, 0.0, 0.0]);
    let outlined = render(&gpu, &content, [1.0, 0.0, 1.0, 0.0]);
    // The imagery inside the tile, the same with or without the outlines.
    let middle = (SIZE / 2, SIZE / 2);
    assert!(
        close(at(&plain, middle.0, middle.1), RED),
        "the quad wears its imagery: {:?}",
        at(&plain, middle.0, middle.1)
    );
    assert_eq!(
        at(&plain, middle.0, middle.1),
        at(&outlined, middle.0, middle.1),
        "the outlines recoloured the inside of the tile"
    );
    // The edge: drawn with the outlines, the imagery without.
    let edge = quad_edge(&outlined);
    assert!(
        edge.iter().copied().any(light),
        "no outline on the tile's edge: {edge:?}"
    );
    let bare = quad_edge(&plain);
    assert!(
        !bare.iter().copied().any(light),
        "an outline without asking for one: {bare:?}"
    );
}
