// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! GPU in the loop: render a frame and count the black pixels.
//!
//! # Why this exists
//!
//! Black ground is the project's one forbidden output (`CLAUDE.md`), and it is
//! the one thing the unit tests could not see. They asked "did every *selected*
//! tile resolve to something", which is a question about bookkeeping; the screen
//! answers a different one, and three separate changes passed every test while
//! putting large black rectangles on the globe. Two of them were shipped to a
//! human who had to photograph them.
//!
//! So this test renders. It is slower and clumsier than a pure one, and it is
//! the only kind that can fail for the reason that actually matters.
//!
//! # The gate
//!
//! A machine without a usable adapter skips, with a printed reason — a laptop,
//! a container and a CI runner do not all have a GPU. Set `TUILE_REQUIRE_GPU=1`
//! to turn that skip into a failure, which is what CI should do wherever a GPU
//! *is* expected: a test that silently skips for a year is a test that does not
//! exist.

mod support;

use glam::{DVec3, Vec3};

use support::{
    drape, encoded_as_planned, exact, fallback, gpu, gpu_with, mesh, painted, select, wearing,
    whole_drape, Scripted, PLANS,
};
use tuile_core::budget::{FrameDeadline, LayerId, ResourceId, UvTransform};
use tuile_core::content::DecodedTileContent;
use tuile_core::geo::{geodetic_to_ecef, Geodetic};
use tuile_core::source::TileId;
use tuile_terrain::{fill_content, GeographicTilingScheme, TileCoord};
use tuile_wgpu::{
    ContentPump, GpuContext, TileRenderer, ViewUniform, DEPTH_FORMAT, SAMPLES, TEXTURE_FORMAT,
};

/// Frame size. Small enough to read back in a test, large enough that a missing
/// tile is thousands of pixels rather than a rounding error.
const SIZE: u32 = 256;

fn content_for(tile: TileId) -> DecodedTileContent {
    let (z, x, y) = tile.terrain_coord();
    let rect = GeographicTilingScheme::default().tile_rect(TileCoord::new(z, x, y));
    // Flat at sea level and untextured: what is under test is *whether ground is
    // drawn*, not what it looks like. An untextured tile samples the white 1×1,
    // so anything drawn is bright and anything missing is the clear colour.
    fill_content(&rect, [Some(0.0); 4], 0.0)
}

fn centre_of(tile: TileId) -> DVec3 {
    let (z, x, y) = tile.terrain_coord();
    let rect = GeographicTilingScheme::default().tile_rect(TileCoord::new(z, x, y));
    geodetic_to_ecef(Geodetic {
        lon: 0.5 * (rect.west + rect.east),
        lat: 0.5 * (rect.south + rect.north),
        height: 0.0,
    })
}

/// Renders the pump's current resolution and returns the fraction of the middle
/// of the frame that came out black.
///
/// The middle, not the whole frame: the edges are where the globe's limb and the
/// tiles' own boundaries fall, and a test that counted those would be measuring
/// framing rather than coverage.
fn black_fraction_in_the_middle(gpu: &GpuContext, pump: &mut ContentPump, eye: DVec3) -> f64 {
    let target = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("test target"),
        size: wgpu::Extent3d {
            width: SIZE,
            height: SIZE,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: TEXTURE_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    // Multisampled, as the session is. The tile pipeline is built for `SAMPLES`
    // and a single-sample pass will not accept it — which is how this test came
    // to fail validation on every run without anyone noticing, MSAA having been
    // added to the renderer long after the test was written.
    let msaa = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("test msaa"),
        size: wgpu::Extent3d {
            width: SIZE,
            height: SIZE,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: SAMPLES,
        dimension: wgpu::TextureDimension::D2,
        format: TEXTURE_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let msaa_view = msaa.create_view(&wgpu::TextureViewDescriptor::default());
    let depth = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("test depth"),
        size: wgpu::Extent3d {
            width: SIZE,
            height: SIZE,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: SAMPLES,
        dimension: wgpu::TextureDimension::D2,
        format: DEPTH_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());

    let renderer = TileRenderer::new(gpu, TEXTURE_FORMAT);
    // Everything is rebased onto the eye, as in a session: the f32 the GPU sees
    // then describes metres from the camera rather than from the Earth's centre.
    pump.rebase(&gpu.queue, eye);

    let down = -eye.normalize();
    let up = DVec3::Z.cross(down).cross(down).normalize_or(DVec3::Y);
    let view_m = glam::camera::rh::view::look_to_mat4(Vec3::ZERO, down.as_vec3(), up.as_vec3());
    let proj = glam::camera::rh::proj::directx::perspective(
        60f32.to_radians(),
        1.0,
        1_000.0,
        40_000_000.0,
    );
    renderer.set_view(
        &gpu.queue,
        &ViewUniform {
            view_proj: (proj * view_m).to_cols_array(),
            // Ambient only. Lighting is not what this test is about, and a sun
            // angle that happened to graze the surface would make ground that
            // *was* drawn read as black.
            params: [1.0, 0.0, 0.0, 0.0],
            ..Default::default()
        },
    );

    let (drawn, _) = pump.resolve(gpu);

    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("test pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &msaa_view,
                depth_slice: None,
                resolve_target: Some(&view),
                ops: wgpu::Operations {
                    // Black, deliberately: the clear colour must be the thing
                    // being looked for, or "nothing was drawn" would come out
                    // some other shade and the count would miss it.
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
        // A fallback ancestor is backdrop, not geometry: it stands in for ground
        // that is not its own and must never win a pixel from a surface that
        // owns it. See `tuile_wgpu::Drawn`.
        renderer.render(&mut pass, drawn.exact.into_iter(), false);
        renderer.render_fallback(&mut pass, drawn.fallback.into_iter());
    }

    // 256 px × 4 bytes is already the 256-byte row alignment `copy_texture_to_buffer`
    // requires, so no padding arithmetic is needed here.
    let bytes = (SIZE * SIZE * 4) as u64;
    let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: bytes,
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
    let data = slice.get_mapped_range().expect("map range");

    let (lo, hi) = (SIZE / 4, SIZE * 3 / 4);
    let (mut black, mut total) = (0u64, 0u64);
    for row in lo..hi {
        for col in lo..hi {
            let i = ((row * SIZE + col) * 4) as usize;
            total += 1;
            if data[i] < 8 && data[i + 1] < 8 && data[i + 2] < 8 {
                black += 1;
            }
        }
    }
    drop(data);
    readback.unmap();
    black as f64 / total as f64
}

/// **Ground the camera is looking at is never left black.**
///
/// Two sibling tiles wanted, only one of which has arrived, and their
/// grandparent held. The budget draws the one that arrived as itself and the
/// grandparent for the other, under the fallback pass — even though the
/// grandparent also spans the sibling that did arrive. The pump must draw
/// both, the ancestor never winning a pixel from the surface that owns it.
///
/// That trade is the point. Two attempts to remove the overlap — stop climbing
/// to ancestors; refuse only the ancestors that overlap — each passed every
/// unit test and put large black rectangles on the globe at both zoom-in and
/// zoom-out. This test fails on either of them, because it looks at pixels.
#[test]
fn ground_under_the_camera_is_never_black() {
    let Some(gpu) = gpu() else { return };

    let here = TileId::from_terrain(3, 4, 4);
    let neighbour = TileId::from_terrain(3, 5, 4);
    let grandparent = TileId::from_terrain(1, 1, 1);

    let mut stream = Scripted::new([
        mesh(here, 1, content_for(here)),
        // The grandparent is held; the neighbour has not arrived and is the
        // ground that must not go black.
        mesh(grandparent, 2, content_for(grandparent)),
        select(vec![exact(here), fallback(grandparent, &[neighbour])], 1),
    ]);

    // High above the middle of the pair, looking straight down, so the frame is
    // ground from edge to edge.
    let eye = centre_of(grandparent) * 1.25;
    let mut pump = ContentPump::new(eye);
    pump.pump(&mut stream, &gpu, FrameDeadline::UNBOUNDED);

    let black = black_fraction_in_the_middle(&gpu, &mut pump, eye);
    assert!(
        black < 0.01,
        "{:.1}% of the middle of the frame is black — ground the camera is \
         looking at was not drawn at all",
        black * 100.0
    );
}

/// **A fallback is drawn, not refused**: without it, the same frame is
/// black where the neighbour is — the measurement the test above stands on,
/// so that it cannot pass for the wrong reason.
#[test]
fn without_the_fallback_the_neighbour_is_black() {
    let Some(gpu) = gpu() else { return };

    let here = TileId::from_terrain(3, 4, 4);
    let grandparent = TileId::from_terrain(1, 1, 1);
    let mut stream = Scripted::new([
        mesh(here, 1, content_for(here)),
        mesh(grandparent, 2, content_for(grandparent)),
        select(vec![exact(here)], 1),
    ]);
    let eye = centre_of(grandparent) * 1.25;
    let mut pump = ContentPump::new(eye);
    pump.pump(&mut stream, &gpu, FrameDeadline::UNBOUNDED);

    let black = black_fraction_in_the_middle(&gpu, &mut pump, eye);
    assert!(
        black > 0.05,
        "only {:.1}% black with the neighbour's ground drawn by nothing — the \
         frame does not see what it is meant to",
        black * 100.0
    );
}

/// **Draped ground under the camera is never black, under every imagery
/// plan** — RGBA8, and each block format the GPU encodes into.
///
/// The scenario above, dressed: the tile that arrived and the grandparent
/// standing in for its neighbour each wear a 256² drape, encoded at upload
/// under a block plan. A texture the encoder left empty, a swap that bound
/// it before it was written, or a format the sampler reads as zeros is black
/// here; the drape's own colours never are.
#[test]
fn draped_ground_under_the_camera_is_never_black_under_every_plan() {
    for (plan, compression) in PLANS {
        let Some(gpu) = gpu_with(compression) else {
            continue;
        };
        let here = TileId::from_terrain(3, 4, 4);
        let neighbour = TileId::from_terrain(3, 5, 4);
        let grandparent = TileId::from_terrain(1, 1, 1);
        // A gradient, never dark: whatever is drawn is far from the clear colour.
        let ground = |x: u32, y: u32| [80 + (x % 160) as u8, 120, 80 + (y % 160) as u8, 255];
        let here_drape = whole_drape(here, 3, painted(&gpu, 256, 256, ground));
        let grand_drape = whole_drape(grandparent, 1, painted(&gpu, 256, 256, ground));
        let worn = |tile, level| {
            (
                ResourceId::drape(tile, LayerId(0), level),
                level,
                UvTransform::IDENTITY,
            )
        };
        let (here_r, here_l, here_uv) = worn(here, 3);
        let (grand_r, grand_l, grand_uv) = worn(grandparent, 1);
        let mut stream = Scripted::new([
            mesh(here, 1, content_for(here)),
            mesh(grandparent, 2, content_for(grandparent)),
            drape(here, 3, here_drape),
            drape(grandparent, 4, grand_drape),
            select(
                vec![
                    wearing(exact(here), here_r, here_l, here_uv),
                    wearing(
                        fallback(grandparent, &[neighbour]),
                        grand_r,
                        grand_l,
                        grand_uv,
                    ),
                ],
                1,
            ),
        ]);
        let eye = centre_of(grandparent) * 1.25;
        let mut pump = ContentPump::new(eye);
        // Two frames: under a block plan the second encode may wait for one.
        pump.pump(&mut stream, &gpu, FrameDeadline::UNBOUNDED);
        pump.pump(&mut stream, &gpu, FrameDeadline::UNBOUNDED);
        assert_eq!(pump.pending_uploads(), 0, "{plan}: every upload landed");
        encoded_as_planned(plan, &gpu, 2);

        let black = black_fraction_in_the_middle(&gpu, &mut pump, eye);
        assert!(
            black < 0.01,
            "{plan}: {:.1}% of the middle of the frame is black — draped ground \
             the camera is looking at was not drawn",
            black * 100.0
        );
    }
}
