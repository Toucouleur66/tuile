// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! GPU in the loop: the globe drawn into a picture the host already started.
//!
//! An engine that hosts tuile — a game engine with its own render graph — owns
//! the device, the colour target and the depth buffer, and has drawn into them
//! before tuile is asked to. Three things must hold there that a frame of our
//! own never tests:
//!
//! - **the host's content survives**: nothing is cleared that was not asked
//!   to be, and what the host drew in front of the ground stays in front;
//! - **the depth is shared**: what the host drew behind the ground is hidden
//!   by it, and the host's depth buffer afterwards holds the ground, so that
//!   whatever the host draws next is hidden by it too;
//! - **no black ground**, with the fallback pass intact — on a depth buffer
//!   that has no stencil for its ownership mark, as a game engine's has not.
//!
//! Both conventions are run: forward depth on a depth-stencil buffer (what our
//! own frame is), and reverse-Z on a stencil-less `Depth32Float` (what a game
//! engine with an infinite far plane uses).
//!
//! The quads are the host's: drawn by a pipeline of this test's own, at a
//! fixed clip-space depth, before tuile is.

mod support;

use glam::DVec3;

use support::{exact, fallback, gpu, mesh, select, Scripted};
use tuile_core::budget::FrameDeadline;
use tuile_core::content::DecodedTileContent;
use tuile_core::geo::{geodetic_to_ecef, Geodetic};
use tuile_core::source::TileId;
use tuile_terrain::{fill_content, GeographicTilingScheme, TileCoord};
use tuile_wgpu::{
    view_proj_from_world, ContentPump, DepthDirection, GpuContext, HostFrame, HostPass,
    TargetLayout, TileRenderer, ViewUniform, DEPTH_FORMAT,
};
use wgpu::util::DeviceExt;

const SIZE: u32 = 256;
/// The colour formats a host draws in: an 8-bit sRGB target, and the
/// half-float target of an engine that renders in high dynamic range.
const COLOURS: [wgpu::TextureFormat; 2] = [
    wgpu::TextureFormat::Rgba8UnormSrgb,
    wgpu::TextureFormat::Rgba16Float,
];

/// The host's near quad: red, over the left quarter of the frame.
const NEAR_COLOUR: [f32; 3] = [1.0, 0.0, 0.0];
/// The host's far quad: blue, over the right quarter, behind the planet.
const FAR_COLOUR: [f32; 3] = [0.0, 0.0, 1.0];

fn content_for(tile: TileId) -> DecodedTileContent {
    let (z, x, y) = tile.terrain_coord();
    let rect = GeographicTilingScheme::default().tile_rect(TileCoord::new(z, x, y));
    // Untextured: anything drawn samples the white 1×1 and is bright.
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

/// One host setup: how its depth runs, its depth format, its sample count,
/// and where its two quads sit in clip space.
#[derive(Clone, Copy)]
struct Host {
    colour: wgpu::TextureFormat,
    direction: DepthDirection,
    depth: wgpu::TextureFormat,
    samples: u32,
    /// Clip-space depth of the near quad (in front of the ground) and of the
    /// far one (behind it), in this host's convention.
    near_z: f32,
    far_z: f32,
}

/// What the frame's ground is made of.
#[derive(Clone, Copy)]
enum Ground {
    /// One tile, and an ancestor standing in for its neighbour.
    WithFallback,
    /// The grandparent as itself: every pixel of ground owned, so written to
    /// depth. A fallback never writes depth — in a host's frame as in our
    /// own — so it is not what the depth test looks at.
    Owned,
}

/// What came back: the resolved colour, as 8-bit RGB per pixel, and — single-sampled
/// stencil-less hosts only — the host's depth after tuile drew.
struct Picture {
    colour: Vec<u8>,
    depth: Option<Vec<f32>>,
}

impl Picture {
    fn pixel(&self, col: u32, row: u32) -> [u8; 3] {
        let i = ((row * SIZE + col) * 3) as usize;
        [self.colour[i], self.colour[i + 1], self.colour[i + 2]]
    }
}

fn texture(
    gpu: &GpuContext,
    label: &str,
    format: wgpu::TextureFormat,
    samples: u32,
    usage: wgpu::TextureUsages,
) -> wgpu::Texture {
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
}

/// The host's own frame before tuile: cleared to black and far, then the two
/// quads drawn at their fixed depths.
fn host_draws_its_quads(
    gpu: &GpuContext,
    encoder: &mut wgpu::CommandEncoder,
    host: Host,
    colour: &wgpu::TextureView,
    resolve: Option<&wgpu::TextureView>,
    depth: &wgpu::TextureView,
) {
    let shader = gpu
        .device
        .create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("host quads"),
            source: wgpu::ShaderSource::Wgsl(
                "struct V { @builtin(position) p: vec4f, @location(0) c: vec3f };
                 @vertex fn vs(@location(0) p: vec4f, @location(1) c: vec3f) -> V {
                     return V(p, c);
                 }
                 @fragment fn fs(v: V) -> @location(0) vec4f { return vec4f(v.c, 1.0); }"
                    .into(),
            ),
        });
    let has_stencil = host.depth.has_stencil_aspect();
    let pipeline = gpu
        .device
        .create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("host quads"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: 28,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x3],
                })],
            },
            primitive: Default::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: host.depth,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Always),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: wgpu::MultisampleState {
                count: host.samples,
                ..Default::default()
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(host.colour.into())],
            }),
            multiview_mask: None,
            cache: None,
        });
    let quad = |x0: f32, x1: f32, z: f32, c: [f32; 3]| -> Vec<f32> {
        let corner = |x: f32, y: f32| [x, y, z, 1.0, c[0], c[1], c[2]];
        [
            corner(x0, -1.0),
            corner(x1, -1.0),
            corner(x1, 1.0),
            corner(x0, -1.0),
            corner(x1, 1.0),
            corner(x0, 1.0),
        ]
        .concat()
    };
    let mut vertices = quad(-1.0, -0.5, host.near_z, NEAR_COLOUR);
    vertices.extend(quad(0.5, 1.0, host.far_z, FAR_COLOUR));
    let buffer = gpu
        .device
        .create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("host quads"),
            contents: bytemuck::cast_slice(&vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("host frame"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: colour,
            resolve_target: resolve,
            depth_slice: None,
            ops: wgpu::Operations {
                // Black: the colour "nothing was drawn" comes out as.
                load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: depth,
            depth_ops: Some(wgpu::Operations {
                load: wgpu::LoadOp::Clear(host.direction.far()),
                store: wgpu::StoreOp::Store,
            }),
            stencil_ops: has_stencil.then_some(wgpu::Operations {
                load: wgpu::LoadOp::Clear(0),
                store: wgpu::StoreOp::Store,
            }),
        }),
        ..Default::default()
    });
    pass.set_pipeline(&pipeline);
    pass.set_vertex_buffer(0, buffer.slice(..));
    pass.draw(0..12, 0..1);
}

fn read_back(
    gpu: &GpuContext,
    encoder: &mut wgpu::CommandEncoder,
    texture: &wgpu::Texture,
    aspect: wgpu::TextureAspect,
    texel: u32,
) -> wgpu::Buffer {
    let buffer = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("readback"),
        size: (SIZE * SIZE * texel) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(SIZE * texel),
                rows_per_image: Some(SIZE),
            },
        },
        wgpu::Extent3d {
            width: SIZE,
            height: SIZE,
            depth_or_array_layers: 1,
        },
    );
    buffer
}

fn mapped(gpu: &GpuContext, buffer: &wgpu::Buffer) -> Vec<u8> {
    let slice = buffer.slice(..);
    slice.map_async(wgpu::MapMode::Read, |_| {});
    gpu.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    let data = slice.get_mapped_range().expect("map range").to_vec();
    buffer.unmap();
    data
}

/// The host draws its quads, then tuile draws the globe into the same colour
/// and depth — loading both, clearing neither.
fn render_into_the_host(gpu: &GpuContext, host: Host, ground: Ground) -> Picture {
    render_into_the_host_under(gpu, host, ground, false)
}

/// The same, with the physical atmosphere drawn — its sky on the far plane —
/// when `sky` says.
fn render_into_the_host_under(gpu: &GpuContext, host: Host, ground: Ground, sky: bool) -> Picture {
    let here = TileId::from_terrain(3, 4, 4);
    let neighbour = TileId::from_terrain(3, 5, 4);
    let grandparent = TileId::from_terrain(1, 1, 1);
    let mut stream = Scripted::new([
        mesh(here, 1, content_for(here)),
        mesh(grandparent, 2, content_for(grandparent)),
        match ground {
            // One tile as itself, its neighbour drawn by the grandparent
            // under the fallback pass: the ownership mark is exercised, and
            // on a stencil-less host it is a mark tuile has to bring along.
            Ground::WithFallback => {
                select(vec![exact(here), fallback(grandparent, &[neighbour])], 1)
            }
            Ground::Owned => select(vec![exact(grandparent)], 1),
        },
    ]);
    let eye = centre_of(grandparent) * 1.25;
    let mut pump = ContentPump::new(eye);
    pump.pump(&mut stream, gpu, FrameDeadline::UNBOUNDED);

    let multisampled = host.samples > 1;
    let resolved = texture(
        gpu,
        "host resolved",
        host.colour,
        1,
        wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
    );
    let msaa = multisampled.then(|| {
        texture(
            gpu,
            "host msaa",
            host.colour,
            host.samples,
            wgpu::TextureUsages::RENDER_ATTACHMENT,
        )
    });
    let depth = texture(
        gpu,
        "host depth",
        host.depth,
        host.samples,
        wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC,
    );
    let resolved_view = resolved.create_view(&Default::default());
    let msaa_view = msaa.as_ref().map(|t| t.create_view(&Default::default()));
    let depth_view = depth.create_view(&Default::default());
    let (colour_view, resolve_view) = match &msaa_view {
        Some(msaa) => (msaa, Some(&resolved_view)),
        None => (&resolved_view, None),
    };

    let layout = TargetLayout {
        colour: host.colour,
        depth: host.depth,
        samples: host.samples,
        direction: host.direction,
    };
    let mut renderer = TileRenderer::for_layout(gpu, layout);
    let mut tuile = HostPass::new(gpu, layout);

    // The host's camera, f64 and in the world tuile's tiles are in, with the
    // projection of its own convention.
    let down = -eye.normalize();
    let up = DVec3::Z.cross(down).cross(down).normalize_or(DVec3::Y);
    let view = glam::dcamera::rh::view::look_to_mat4(eye, down, up);
    let fov = 60f64.to_radians();
    let proj = match host.direction {
        DepthDirection::Reverse => {
            glam::dcamera::rh::proj::directx::perspective_infinite_reverse(fov, 1.0, 1_000.0)
        }
        DepthDirection::Forward => {
            glam::dcamera::rh::proj::directx::perspective(fov, 1.0, 1_000.0, 40_000_000.0)
        }
    };
    pump.rebase(&gpu.queue, eye);
    renderer.set_view(
        &gpu.queue,
        &ViewUniform {
            view_proj: view_proj_from_world(proj * view, pump.render_origin),
            params: [1.0, 0.0, 0.0, 0.0],
            ..Default::default()
        },
    );
    if sky {
        // Midsummer noon over Europe, the eye over the tiles.
        renderer.set_sky(
            gpu,
            Some(
                &tuile_wgpu::SkyView {
                    eye,
                    render_origin: pump.render_origin,
                    forward: down,
                    up,
                    fov_y: fov,
                    aspect: 1.0,
                    height_px: SIZE,
                    ground_height: 0.0,
                }
                .at(1_782_043_200.0, 0.0),
            ),
        );
    }
    let (drawn, _) = pump.resolve(gpu);

    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    host_draws_its_quads(
        gpu,
        &mut encoder,
        host,
        colour_view,
        resolve_view,
        &depth_view,
    );
    tuile.encode(
        gpu,
        &mut encoder,
        HostFrame {
            colour: colour_view,
            resolve: resolve_view,
            depth: &depth_view,
            size: (SIZE, SIZE),
            clear: None,
        },
        |pass| {
            renderer.paint(
                pass,
                std::iter::empty(),
                drawn.exact.iter().copied(),
                drawn.fallback.iter().copied(),
                None,
                false,
            )
        },
    );
    let half = host.colour == wgpu::TextureFormat::Rgba16Float;
    let texel = if half { 8 } else { 4 };
    let colour_buf = read_back(
        gpu,
        &mut encoder,
        &resolved,
        wgpu::TextureAspect::All,
        texel,
    );
    let depth_buf = (!multisampled && host.depth == wgpu::TextureFormat::Depth32Float)
        .then(|| read_back(gpu, &mut encoder, &depth, wgpu::TextureAspect::DepthOnly, 4));
    gpu.queue.submit([encoder.finish()]);
    let raw = mapped(gpu, &colour_buf);
    let colour = if half {
        // Linear half floats, brought to bytes: 0 stays 0 and 1 is 255, which
        // is all the thresholds below ask of them.
        raw.as_chunks::<8>()
            .0
            .iter()
            .flat_map(|px| {
                (0..3).map(move |c| {
                    let v = f16_to_f32(u16::from_le_bytes([px[2 * c], px[2 * c + 1]]));
                    (v.clamp(0.0, 1.0) * 255.0).round() as u8
                })
            })
            .collect()
    } else {
        raw.as_chunks::<4>()
            .0
            .iter()
            .flat_map(|px| [px[0], px[1], px[2]])
            .collect()
    };
    Picture {
        colour,
        depth: depth_buf.map(|b| bytemuck::cast_slice(&mapped(gpu, &b)).to_vec()),
    }
}

/// A half float's value, as far as a colour channel needs: finite numbers
/// only.
fn f16_to_f32(bits: u16) -> f32 {
    let sign = if bits & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exponent = i32::from((bits >> 10) & 0x1f);
    let mantissa = f32::from(bits & 0x3ff);
    sign * match exponent {
        0 => mantissa * 2f32.powi(-24),
        _ => (1.0 + mantissa / 1024.0) * 2f32.powi(exponent - 15),
    }
}

/// What every host must see: its near quad intact, its far quad hidden by
/// the ground, and no black in the middle of the frame.
fn assert_composed(picture: &Picture, what: &str) {
    // Away from the quads' edges and the frame's.
    let rows = SIZE / 8..SIZE * 7 / 8;
    let (mut near_kept, mut far_shown, mut black, mut total) = (0, 0, 0, 0);
    for row in rows.clone() {
        for col in SIZE / 32..SIZE / 4 - SIZE / 32 {
            if picture.pixel(col, row) == [255, 0, 0] {
                near_kept += 1;
            }
        }
        for col in SIZE * 3 / 4 + SIZE / 32..SIZE - SIZE / 32 {
            let [r, g, b] = picture.pixel(col, row);
            if b > 200 && r < 50 && g < 50 {
                far_shown += 1;
            }
        }
        for col in SIZE / 4..SIZE * 3 / 4 {
            total += 1;
            let [r, g, b] = picture.pixel(col, row);
            if r < 8 && g < 8 && b < 8 {
                black += 1;
            }
        }
    }
    let strip = rows.len() as u32 * (SIZE / 4 - SIZE / 16);
    assert_eq!(
        near_kept, strip,
        "{what}: the host's quad in front of the ground was painted over"
    );
    assert_eq!(
        far_shown, 0,
        "{what}: the host's quad behind the ground shows through it"
    );
    assert!(
        black * 100 < total,
        "{what}: {black} of {total} middle pixels are black — ground not drawn"
    );
}

/// A game engine's buffer: reverse-Z, `Depth32Float`, no stencil — one and
/// four samples, an sRGB and a half-float colour. The composition, and the
/// ground with its fallback.
#[test]
fn the_globe_composes_into_a_reverse_z_stencil_less_host() {
    let Some(gpu) = gpu() else { return };
    for colour in COLOURS {
        for samples in [1, 4] {
            let picture = render_into_the_host(
                &gpu,
                Host {
                    colour,
                    direction: DepthDirection::Reverse,
                    depth: wgpu::TextureFormat::Depth32Float,
                    samples,
                    near_z: 0.5,
                    far_z: 1e-7,
                },
                Ground::WithFallback,
            );
            assert_composed(&picture, &format!("reverse-Z, {colour:?}, {samples}×"));
        }
    }
}

/// Our own kind of buffer, handed over by a host: forward depth with a
/// stencil it lends — the same matrix.
#[test]
fn the_globe_composes_into_a_forward_depth_stencil_host() {
    let Some(gpu) = gpu() else { return };
    for colour in COLOURS {
        for samples in [1, 4] {
            let picture = render_into_the_host(
                &gpu,
                Host {
                    colour,
                    direction: DepthDirection::Forward,
                    depth: DEPTH_FORMAT,
                    samples,
                    near_z: 0.1,
                    far_z: 0.999_99,
                },
                Ground::WithFallback,
            );
            assert_composed(&picture, &format!("forward, {colour:?}, {samples}×"));
        }
    }
}

/// The physical atmosphere composes as the globe does: its sky on the far
/// plane, depth and stencil untouched — the host's quad in front kept, the
/// one behind the planet hidden, in either convention.
#[test]
fn the_sky_composes_into_a_host_too() {
    let Some(gpu) = gpu() else { return };
    for (direction, depth, near_z, far_z) in [
        (
            DepthDirection::Reverse,
            wgpu::TextureFormat::Depth32Float,
            0.5,
            1e-7,
        ),
        (DepthDirection::Forward, DEPTH_FORMAT, 0.1, 0.999_99),
    ] {
        for samples in [1, 4] {
            let picture = render_into_the_host_under(
                &gpu,
                Host {
                    colour: wgpu::TextureFormat::Rgba8UnormSrgb,
                    direction,
                    depth,
                    samples,
                    near_z,
                    far_z,
                },
                Ground::WithFallback,
                true,
            );
            assert_composed(&picture, &format!("sky, {direction:?}, {samples}×"));
        }
    }
}

/// The stencil-less host single-sampled, its depth read back: **the ground is
/// in the host's depth afterwards** where it is in front, and the host's own
/// depth is untouched where tuile drew nothing nearer.
#[test]
fn the_host_depth_holds_the_ground_afterwards() {
    let Some(gpu) = gpu() else { return };
    let far_z = 1e-7;
    let picture = render_into_the_host(
        &gpu,
        Host {
            colour: wgpu::TextureFormat::Rgba8UnormSrgb,
            direction: DepthDirection::Reverse,
            depth: wgpu::TextureFormat::Depth32Float,
            samples: 1,
            near_z: 0.5,
            far_z,
        },
        Ground::Owned,
    );
    assert_composed(&picture, "reverse-Z, 1 sample");
    let depth = picture
        .depth
        .expect("a single-sampled Depth32Float is read");
    let at = |col: u32, row: u32| depth[(row * SIZE + col) as usize];
    for row in SIZE / 8..SIZE * 7 / 8 {
        // The near quad's own depth — exactly where the device has a float
        // depth with a stencil, to the companion's 24 bits where it has not.
        let near = at(SIZE / 8, row);
        assert!(
            (near - 0.5).abs() < 1e-6,
            "the host's near depth was moved ({near})"
        );
        // Behind the ground: the ground's depth now, nearer than the quad's.
        let behind = at(SIZE * 7 / 8, row);
        assert!(
            behind > far_z * 10.0,
            "row {row}: the host's depth still holds its far quad ({behind}) \
             where the ground is in front of it"
        );
        // The middle is ground too.
        assert!(at(SIZE / 2, row) > 0.0, "row {row}: no ground in the depth");
    }
}
