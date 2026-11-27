// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! **Held relief stands under the ground the selection has not reached.**
//!
//! The selection lags a camera turned fast: the ground near the eye was not
//! drawn at all, and the skirts of the tiles around the hole stood as walls.
//! The underlay (`ContentPump::set_underlay_level`) draws, under that ground,
//! the meshes the pump still holds at its level and finer: only those the
//! draw set does not draw, wearing a drape of their own, near enough to the
//! eye and in view, the finest first — and, like every tile drawn, placed
//! against this frame's render origin.
//!
//! A machine without a usable adapter skips, with a printed reason. Set
//! `TUILE_REQUIRE_GPU=1` to turn that skip into a failure.

mod support;

use glam::{DVec3, Mat4, Vec3};

use support::{drape, exact, gpu, mesh, painted, select, wearing, whole_drape, Scripted};
use tuile_core::budget::{FrameDeadline, LayerId, ResourceId, UvTransform};
use tuile_core::content::{DecodedTileContent, DrapeContent};
use tuile_core::geo::{geodetic_to_ecef, Geodetic};
use tuile_core::protocol::ServerMessage;
use tuile_core::source::TileId;
use tuile_terrain::{
    skirt_height, to_decoded, GeoRect, GeographicTilingScheme, Header, QuantizedMesh, TileCoord,
};
use tuile_wgpu::{
    ContentPump, GpuContext, PreparedTile, TileRenderer, UnderlayOptions, ViewUniform,
    DEPTH_FORMAT, SAMPLES, TEXTURE_FORMAT,
};

const SIZE: u32 = 256;

fn rect_of(tile: TileId) -> GeoRect {
    let (z, x, y) = tile.terrain_coord();
    GeographicTilingScheme::default().tile_rect(TileCoord::new(z, x, y))
}

/// Where `tile`'s mesh stands: the middle of its rectangle, on the ellipsoid.
fn centre(tile: TileId) -> DVec3 {
    let (lon, lat) = rect_of(tile).center();
    geodetic_to_ecef(Geodetic {
        lon,
        lat,
        height: 0.0,
    })
}

/// `tile` as flat ground, `steps × steps`, with its skirts.
fn flat(tile: TileId) -> DecodedTileContent {
    let rect = rect_of(tile);
    let c = centre(tile);
    let steps = 4;
    let n = steps + 1;
    let (mut u, mut v) = (Vec::new(), Vec::new());
    for row in 0..n {
        for col in 0..n {
            u.push(col as f64 / steps as f64);
            v.push(row as f64 / steps as f64);
        }
    }
    let mut indices = Vec::new();
    for row in 0..steps {
        for col in 0..steps {
            let i = (row * n + col) as u32;
            let (a, b, cc, d) = (i, i + 1, i + n as u32, i + n as u32 + 1);
            indices.extend_from_slice(&[a, b, cc, cc, b, d]);
        }
    }
    let at = |col: usize, row: usize| (row * n + col) as u32;
    let quantized = QuantizedMesh {
        header: Header {
            center: [c.x, c.y, c.z],
            min_height: 0.0,
            max_height: 0.0,
            bounding_sphere_center: [c.x, c.y, c.z],
            bounding_sphere_radius: 1.0e6,
            horizon_occlusion: [0.0; 3],
        },
        u,
        v,
        height: vec![0.0; n * n],
        indices,
        normals: None,
        edges: [
            (0..n).map(|row| at(0, row)).collect(),
            (0..n).map(|col| at(col, 0)).collect(),
            (0..n).map(|row| at(n - 1, row)).collect(),
            (0..n).map(|col| at(col, n - 1)).collect(),
        ],
        metadata_available: None,
        cut: 0,
    };
    let mut content = to_decoded(&quantized, &rect, skirt_height(&rect));
    for m in &mut content.meshes {
        m.material.base_color_texture = None;
    }
    content
}

/// A drape of `tile`'s own, holding no imagery: enough to be worn.
fn own_drape(tile: TileId) -> DrapeContent {
    let (z, ..) = tile.terrain_coord();
    DrapeContent {
        layer: LayerId(0),
        level: z,
        tiles: Vec::new(),
        floor_tiles: 0,
        identity: tile.0,
        withheld: false,
    }
}

/// The tiles of `drawn`, named by where they stand.
fn named(drawn: &[&PreparedTile], tiles: &[TileId]) -> Vec<TileId> {
    drawn
        .iter()
        .map(|p| {
            *tiles
                .iter()
                .find(|t| centre(**t).distance(p.origin_ecef()) < 1.0)
                .expect("a tile of the scenario")
        })
        .collect()
}

/// **The underlay is what the selection does not draw, near and in view,
/// dressed, from its level down, the finest first — and nothing by
/// default.**
///
/// The eye is 20 km above a level-10 tile at (0°, 0°), the render origin
/// with it; the view sees ±30 km east–west of it (a tile's width of slack),
/// anything north–south. Level 10 is ≈ 19.6 km wide, so its reach is
/// ≈ 235 km; level 9's ≈ 470 km.
#[test]
fn the_underlay_is_held_ground_the_selection_does_not_draw() {
    let Some(gpu) = gpu() else { return };
    // Four level-10 tiles under the eye, and two level-9 ones beside them:
    // all of the underlay, the finest first.
    let fine = [
        TileId::from_terrain(10, 1024, 512),
        TileId::from_terrain(10, 1025, 512),
        TileId::from_terrain(10, 1024, 513),
        TileId::from_terrain(10, 1025, 513),
    ];
    let coarse = [
        TileId::from_terrain(9, 513, 256),
        TileId::from_terrain(9, 513, 257),
    ];
    // Left out, each for one reason alone.
    let drawn = TileId::from_terrain(9, 511, 256); // in the draw set
    let bare = TileId::from_terrain(9, 512, 257); // no drape of its own
    let out_of_view = TileId::from_terrain(10, 1029, 512); // ≈ 108 km east
    let beyond_reach = TileId::from_terrain(10, 1024, 532); // ≈ 390 km north
    let coarser = TileId::from_terrain(8, 256, 128); // above the level
    let all: Vec<TileId> = fine
        .iter()
        .chain(&coarse)
        .copied()
        .chain([drawn, bare, out_of_view, beyond_reach, coarser])
        .collect();

    let mut messages: Vec<ServerMessage> = Vec::new();
    for (i, t) in all.iter().enumerate() {
        messages.push(mesh(*t, 1 + i as u64, flat(*t)));
        if *t != bare {
            messages.push(drape(*t, 100 + i as u64, own_drape(*t)));
        }
    }
    messages.push(select(vec![exact(drawn)], 1));
    let eye = centre(fine[0]) + centre(fine[0]).normalize() * 20_000.0;
    let mut pump = ContentPump::new(eye);
    pump.pump(&mut Scripted::new(messages), &gpu, FrameDeadline::UNBOUNDED);

    let (frame, _) = pump.resolve(&gpu);
    assert!(
        frame.underlay.is_empty(),
        "no underlay unless a host asks for one"
    );
    drop(frame);

    // At (0°, 0°), ECEF x is up and y is east: the view's x and y sides.
    let view = glam::camera::rh::proj::directx::orthographic(
        -1.0e6, 1.0e6, -30_000.0, 30_000.0, -1.0e7, 1.0e7,
    );
    pump.set_underlay_level(Some(9));
    pump.set_underlay_view(view.to_cols_array(), eye);
    let (frame, _) = pump.resolve(&gpu);
    let under = named(&frame.underlay, &all);
    assert_eq!(
        named(&frame.exact, &all),
        vec![drawn],
        "the draw set is drawn as itself"
    );
    for (left_out, why) in [
        (drawn, "drawn by the selection"),
        (bare, "wearing no drape of its own"),
        (out_of_view, "out of the view"),
        (beyond_reach, "farther than the reach of its widths"),
        (coarser, "coarser than the underlay's level"),
    ] {
        assert!(
            !under.contains(&left_out),
            "{left_out:?} is in the underlay, {why}: {under:?}"
        );
    }
    let mut expected: Vec<TileId> = fine.iter().chain(&coarse).copied().collect();
    expected.sort();
    let mut got = under.clone();
    got.sort();
    assert_eq!(got, expected, "every other held tile is in the underlay");
    let levels: Vec<u32> = under.iter().map(|t| t.terrain_coord().0).collect();
    assert!(
        levels.windows(2).all(|w| w[0] >= w[1]),
        "the finest first, each drawing where no finer one did: {levels:?}"
    );
}

/// Draws `underlay` under `tiles` through [`TileRenderer::paint_under`],
/// seen through `view_proj` (in render space); returns the frame, RGBA8.
fn draw(
    gpu: &GpuContext,
    view_proj: Mat4,
    underlay: &[&PreparedTile],
    tiles: &[&PreparedTile],
) -> Vec<u8> {
    draw_with(gpu, &UnderlayOptions::default(), view_proj, underlay, tiles)
}

/// [`draw`] through a renderer made with `options`.
fn draw_with(
    gpu: &GpuContext,
    options: &UnderlayOptions,
    view_proj: Mat4,
    underlay: &[&PreparedTile],
    tiles: &[&PreparedTile],
) -> Vec<u8> {
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
    let msaa = make(
        "underlay msaa",
        TEXTURE_FORMAT,
        wgpu::TextureUsages::RENDER_ATTACHMENT,
        SAMPLES,
    );
    let msaa_view = msaa.create_view(&wgpu::TextureViewDescriptor::default());
    let target = make(
        "underlay target",
        TEXTURE_FORMAT,
        wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        1,
    );
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let depth = make(
        "underlay depth",
        DEPTH_FORMAT,
        wgpu::TextureUsages::RENDER_ATTACHMENT,
        SAMPLES,
    );
    let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());

    let renderer = TileRenderer::new_with(gpu, TEXTURE_FORMAT, options);
    renderer.set_view(
        &gpu.queue,
        &ViewUniform {
            view_proj: view_proj.to_cols_array(),
            // Ambient only, no atmosphere: the question is what was drawn.
            params: [1.0, 0.0, 0.0, 0.0],
            ..Default::default()
        },
    );
    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("underlay"),
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
                    load: wgpu::LoadOp::Clear(0),
                    store: wgpu::StoreOp::Discard,
                }),
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        renderer.paint_under(
            &mut pass,
            std::iter::empty(),
            underlay.iter().copied(),
            std::iter::empty(),
            std::iter::empty(),
            tiles.iter().copied(),
            std::iter::empty(),
            None,
            false,
        );
    }
    let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("underlay readback"),
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

/// Pixels of the middle quarter of the frame more `channel` (0 red, 1 green)
/// than the other.
fn middle_won_by(pixels: &[u8], channel: usize) -> u64 {
    let (lo, hi) = (SIZE * 3 / 8, SIZE * 5 / 8);
    let mut count = 0;
    for row in lo..hi {
        for col in lo..hi {
            let i = ((row * SIZE + col) * 4) as usize;
            let (c, other) = (pixels[i + channel], pixels[i + 1 - channel]);
            if i32::from(c) > i32::from(other) + 40 {
                count += 1;
            }
        }
    }
    count
}

/// **An underlay tile stands where it is, wherever the render origin was
/// when it was last drawn — and under the selection, never over it.**
///
/// The tile is uploaded while the render origin is 300 km away, then the
/// eye comes to 20 km above it and the render origin with it. Looking
/// straight down, the middle of the frame is the tile's red drape. Then its
/// parent is selected, green, over the same ground: the middle is green.
#[test]
fn an_underlay_tile_is_placed_against_the_frames_render_origin() {
    let Some(gpu) = gpu() else { return };
    let child = TileId::from_terrain(10, 1024, 512);
    let parent = TileId::from_terrain(9, 512, 256);
    let look_at = centre(child);
    let up = look_at.normalize();
    let eye = look_at + up * 20_000.0;
    let east = DVec3::Z.cross(up).normalize();
    let mut pump = ContentPump::new(eye + east * 300_000.0);
    let red = painted(&gpu, 4, 4, |_, _| [230, 30, 30, 255]);
    let green = painted(&gpu, 4, 4, |_, _| [30, 230, 30, 255]);
    pump.pump(
        &mut Scripted::new([
            mesh(child, 1, flat(child)),
            drape(child, 2, whole_drape(child, 10, red)),
            mesh(parent, 3, flat(parent)),
            drape(parent, 4, whole_drape(parent, 9, green)),
            select(Vec::new(), 1),
        ]),
        &gpu,
        FrameDeadline::UNBOUNDED,
    );

    let forward = -up;
    let side = east.cross(forward).normalize();
    let view_m =
        glam::camera::rh::view::look_to_mat4(Vec3::ZERO, forward.as_vec3(), side.as_vec3());
    let proj =
        glam::camera::rh::proj::directx::perspective(40f32.to_radians(), 1.0, 100.0, 40_000_000.0);
    let view_proj = proj * view_m;
    pump.rebase(&gpu.queue, eye);
    pump.set_underlay_level(Some(10));
    pump.set_underlay_view(view_proj.to_cols_array(), eye);
    let (frame, _) = pump.resolve(&gpu);
    assert_eq!(frame.underlay.len(), 1, "the child alone is at level 10");
    let pixels = draw(&gpu, view_proj, &frame.underlay, &frame.exact);
    let area = u64::from(SIZE / 4) * u64::from(SIZE / 4);
    let red_px = middle_won_by(&pixels, 0);
    assert_eq!(
        red_px, area,
        "{red_px} of {area} middle pixels are the underlay tile: one left at \
         the render origin it was uploaded against stands 300 km off"
    );
    drop(frame);

    pump.pump(
        &mut Scripted::new([select(
            vec![wearing(
                exact(parent),
                ResourceId::drape(parent, LayerId(0), 9),
                9,
                UvTransform::IDENTITY,
            )],
            2,
        )]),
        &gpu,
        FrameDeadline::UNBOUNDED,
    );
    pump.set_underlay_level(Some(9));
    let (frame, _) = pump.resolve(&gpu);
    assert_eq!(frame.exact.len(), 1, "the parent is drawn");
    assert_eq!(frame.underlay.len(), 1, "the child is held under it");
    let pixels = draw(&gpu, view_proj, &frame.underlay, &frame.exact);
    let green_px = middle_won_by(&pixels, 1);
    assert_eq!(
        green_px, area,
        "{green_px} of {area} middle pixels are the selection's: the underlay \
         is drawn only where nothing the selection drew is"
    );
}

/// **The underlay's reach is the host's number, in the tile's own widths.**
///
/// Two level-10 tiles (≈ 19.6 km wide), one under the eye and one ≈ 390 km
/// north, ≈ 20 widths away. The default reach, 12 widths, takes the first
/// alone; 30 takes both; 0.5 takes neither (the near one's centre is
/// ≈ 1 width away).
#[test]
fn the_underlays_reach_is_the_hosts_number() {
    let Some(gpu) = gpu() else { return };
    let near = TileId::from_terrain(10, 1024, 512);
    let far = TileId::from_terrain(10, 1024, 532);
    let all = [near, far];
    let mut messages: Vec<ServerMessage> = Vec::new();
    for (i, t) in all.iter().enumerate() {
        messages.push(mesh(*t, 1 + i as u64, flat(*t)));
        messages.push(drape(*t, 100 + i as u64, own_drape(*t)));
    }
    messages.push(select(Vec::new(), 1));
    let eye = centre(near) + centre(near).normalize() * 20_000.0;
    let mut pump = ContentPump::new(eye);
    pump.pump(&mut Scripted::new(messages), &gpu, FrameDeadline::UNBOUNDED);
    let view =
        glam::camera::rh::proj::directx::orthographic(-1.0e6, 1.0e6, -1.0e6, 1.0e6, -1.0e7, 1.0e7);
    pump.set_underlay_level(Some(10));
    pump.set_underlay_view(view.to_cols_array(), eye);

    let mut under_with = |reach: f64| {
        pump.set_underlay_options(&UnderlayOptions {
            reach,
            ..UnderlayOptions::default()
        });
        let (frame, _) = pump.resolve(&gpu);
        let mut under = named(&frame.underlay, &all);
        under.sort();
        under
    };
    let mut both = all.to_vec();
    both.sort();
    assert_eq!(
        under_with(UnderlayOptions::DEFAULT_REACH),
        vec![near],
        "the default reach takes the near tile alone"
    );
    assert_eq!(under_with(30.0), both, "a longer reach takes both");
    assert_eq!(
        under_with(0.5),
        Vec::<TileId>::new(),
        "a shorter one takes neither"
    );
}

/// **A renderer made with another push still draws the underlay where
/// nothing else is.**
///
/// The push is a number the shader is assembled with; whatever it is, the
/// underlay tile stands in the middle of the frame, in its own drape, and
/// the pipeline compiles (0 and a half of the distance are as good as 1 %).
#[test]
fn an_underlay_pushed_by_the_hosts_number_still_draws() {
    let Some(gpu) = gpu() else { return };
    let child = TileId::from_terrain(10, 1024, 512);
    let look_at = centre(child);
    let up = look_at.normalize();
    let eye = look_at + up * 20_000.0;
    let east = DVec3::Z.cross(up).normalize();
    let mut pump = ContentPump::new(eye);
    let red = painted(&gpu, 4, 4, |_, _| [230, 30, 30, 255]);
    pump.pump(
        &mut Scripted::new([
            mesh(child, 1, flat(child)),
            drape(child, 2, whole_drape(child, 10, red)),
            select(Vec::new(), 1),
        ]),
        &gpu,
        FrameDeadline::UNBOUNDED,
    );
    let forward = -up;
    let side = east.cross(forward).normalize();
    let view_m =
        glam::camera::rh::view::look_to_mat4(Vec3::ZERO, forward.as_vec3(), side.as_vec3());
    let proj =
        glam::camera::rh::proj::directx::perspective(40f32.to_radians(), 1.0, 100.0, 40_000_000.0);
    let view_proj = proj * view_m;
    pump.set_underlay_level(Some(10));
    pump.set_underlay_view(view_proj.to_cols_array(), eye);
    let (frame, _) = pump.resolve(&gpu);
    assert_eq!(frame.underlay.len(), 1);
    let area = u64::from(SIZE / 4) * u64::from(SIZE / 4);
    for push in [0.0, 0.5] {
        let options = UnderlayOptions {
            push,
            ..UnderlayOptions::default()
        };
        let pixels = draw_with(&gpu, &options, view_proj, &frame.underlay, &[]);
        let red_px = middle_won_by(&pixels, 0);
        assert_eq!(
            red_px, area,
            "push {push}: {red_px} of {area} middle pixels are the underlay tile"
        );
    }
}
