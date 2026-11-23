// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The upload and release path, measured: what each frame asks of the device
//! while the real engine streams under a scripted gesture.
//!
//! The real `GeometryServer`, the real traversal and budget, the real pump,
//! real GPU uploads and a real draw each frame; the loader answers at once
//! from a generated surface wearing a 256² imagery tile of its own, so what
//! is measured is the executor, not a network.
//!
//! The gesture is the aggressive sequence of the budget's catalogue —
//! zoom-in, pan, tilt, rotate, zoom-out — and then a still view. Per phase:
//! uploads and their bytes, submits, encoders, staging allocations, writes,
//! buffers, textures and bind groups created, encode passes, readbacks and
//! polls, frame time (CPU, and CPU + waiting for the GPU) at p50 and p99, the
//! time from delivered to drawable, releases; then a release probe: frames
//! and time from an eviction applied to its memory back with the device.
//!
//! Ignored: a bench, run on purpose, in release —
//!
//! ```sh
//! TUILE_REQUIRE_GPU=1 cargo test --release -p tuile-wgpu --test upload_bench \
//!     -- --ignored --nocapture
//! ```
//!
//! `TUILE_BENCH_PLANS=off,bc7` picks the plans (all three by default).

mod support;

use std::sync::Arc;
use std::time::Instant;

use glam::{DVec2, DVec3};

use support::{gpu_with, painted, poll, Server, PLANS};
use tuile_core::budget::{BudgetConfig, Bytes};
use tuile_core::content::DecodedTexture;
use tuile_core::geo::{geodetic_to_ecef, Geodetic};
use tuile_core::raster::{ImageryCoord, ImageryLayer};
use tuile_core::runtime::in_process_with;
use tuile_core::source::{LoadError, Loaded, TileId, TileLoader, TileTree};
use tuile_core::traversal::{Config, ViewState};
use tuile_terrain::{fill_content, GeographicTilingScheme, LayerJson, TileCoord};
use tuile_wgpu::{
    ContentPump, GpuContext, PumpTotals, TileRenderer, UploadOps, ViewUniform, DEPTH_FORMAT,
    SAMPLES, TEXTURE_FORMAT,
};

/// Deepest level served: a kilometre up needs about level 14.
const DEEPEST: u32 = 15;
const PINNED: u32 = 3;
const SIZE: u32 = 512;

fn everywhere() -> LayerJson {
    let ranges: Vec<String> = (0..=DEEPEST)
        .map(|level| {
            let (x, y) = (2u64 << level, 1u64 << level);
            format!(
                r#"[{{"startX":0,"startY":0,"endX":{},"endY":{}}}]"#,
                x - 1,
                y - 1
            )
        })
        .collect();
    let doc = format!(
        r#"{{"tilejson":"2.1.0","format":"quantized-mesh-1.0","scheme":"tms",
            "projection":"EPSG:4326","tiles":["{{z}}/{{x}}/{{y}}.terrain"],
            "bounds":[-180,-90,180,90],"available":[{}]}}"#,
        ranges.join(",")
    );
    LayerJson::from_slice(doc.as_bytes()).expect("layer.json")
}

/// Every tile at once, a flat surface wearing an imagery tile of its own.
struct Instant256(Arc<DecodedTexture>);

#[async_trait::async_trait]
impl TileLoader for Instant256 {
    async fn load(&self, id: TileId) -> Result<Loaded, LoadError> {
        let (z, x, y) = id.terrain_coord();
        let rect = GeographicTilingScheme::default().tile_rect(TileCoord::new(z, x, y));
        let mut content = fill_content(&rect, [Some(0.0); 4], 0.0);
        // A texture of its own, as a decoder hands each tile: nothing the
        // upload path computes from it can be shared between tiles.
        content.imagery.push(ImageryLayer {
            coord: ImageryCoord { level: z, x, y },
            source: 0,
            texture: Arc::new(DecodedTexture::clone(&self.0)),
            coverage: [0.0, 0.0, 1.0, 1.0],
            translation: [0.0, 0.0],
            scale: [1.0, 1.0],
        });
        Ok(Loaded::Content(content))
    }
}

/// Where the camera is: over a ground point, `range` metres from it, its
/// view `pitch` degrees below the horizon, heading `heading` degrees.
#[derive(Clone, Copy)]
struct Pose {
    lon: f64,
    lat: f64,
    range: f64,
    pitch: f64,
    heading: f64,
}

impl Pose {
    fn view(self) -> (ViewState, DVec3, DVec3) {
        let target = geodetic_to_ecef(Geodetic {
            lon: self.lon.to_radians(),
            lat: self.lat.to_radians(),
            height: 0.0,
        });
        let up = target.normalize();
        let east = DVec3::Z.cross(up).normalize();
        let north = up.cross(east);
        let h = self.heading.to_radians();
        let forward_flat = north * h.cos() + east * h.sin();
        let p = self.pitch.to_radians();
        let dir = (forward_flat * p.cos() - up * p.sin()).normalize();
        let eye = target - dir * self.range;
        let right = dir.cross(up).normalize_or(east);
        let screen_up = right.cross(dir).normalize();
        let view = ViewState::perspective(
            eye,
            dir,
            screen_up,
            DVec2::new(SIZE as f64, SIZE as f64),
            45f64.to_radians(),
        );
        (view, eye, screen_up)
    }
}

/// The offscreen target the frames are drawn into.
struct Target {
    colour: wgpu::TextureView,
    msaa: wgpu::TextureView,
    depth: wgpu::TextureView,
    renderer: TileRenderer,
}

impl Target {
    fn new(gpu: &GpuContext) -> Self {
        let make = |format, usage, samples| {
            gpu.device
                .create_texture(&wgpu::TextureDescriptor {
                    label: None,
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
                .create_view(&wgpu::TextureViewDescriptor::default())
        };
        Self {
            colour: make(TEXTURE_FORMAT, wgpu::TextureUsages::RENDER_ATTACHMENT, 1),
            msaa: make(
                TEXTURE_FORMAT,
                wgpu::TextureUsages::RENDER_ATTACHMENT,
                SAMPLES,
            ),
            depth: make(DEPTH_FORMAT, wgpu::TextureUsages::RENDER_ATTACHMENT, SAMPLES),
            renderer: TileRenderer::new(gpu, TEXTURE_FORMAT),
        }
    }

    fn draw(&self, gpu: &GpuContext, drawn: &tuile_wgpu::Drawn<'_>, view: &ViewState, up: DVec3) {
        let view_m = glam::camera::rh::view::look_to_mat4(
            glam::Vec3::ZERO,
            view.direction().as_vec3(),
            up.as_vec3(),
        );
        let proj = glam::camera::rh::proj::directx::perspective(
            45f32.to_radians(),
            1.0,
            10.0,
            2.0e7,
        );
        self.renderer.set_view(
            &gpu.queue,
            &ViewUniform {
                view_proj: (proj * view_m).to_cols_array(),
                params: [1.0, 0.0, 0.0, 0.0],
                ..Default::default()
            },
        );
        let mut encoder = gpu
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.msaa,
                    depth_slice: None,
                    resolve_target: Some(&self.colour),
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth,
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
            self.renderer
                .render(&mut pass, drawn.exact.iter().copied(), false);
            self.renderer
                .render_fallback(&mut pass, drawn.fallback.iter().copied());
        }
        gpu.queue.submit([encoder.finish()]);
    }
}

/// One frame's numbers.
#[derive(Default, Clone, Copy)]
struct FrameNumbers {
    ops: UploadOps,
    pump: PumpTotals,
    cpu_ms: f64,
    gpu_ms: f64,
    lost: usize,
}

/// The device's own count of what it holds (wgpu's `counters`).
fn device_memory(gpu: &GpuContext) -> (isize, isize, isize, isize) {
    let c = gpu.device.get_internal_counters();
    (
        c.hal.buffers.read(),
        c.hal.textures.read(),
        c.hal.bind_groups.read(),
        c.hal.buffer_memory.read() + c.hal.texture_memory.read(),
    )
}

struct Session {
    server: Server,
    stream: tuile_core::protocol::InProcessStream,
    pump: ContentPump,
    target: Target,
}

impl Session {
    fn frame(&mut self, gpu: &GpuContext, pose: Pose) -> FrameNumbers {
        let (view, eye, up) = pose.view();
        let ops0 = gpu.upload_ops();
        let pump0 = self.pump.totals();
        let began = Instant::now();
        poll(&mut self.server);
        poll(&mut self.server);
        let deadline = self.pump.deadline(60);
        self.pump
            .advance(&mut self.stream, gpu, &[view], eye, deadline);
        let (drawn, resolution) = self.pump.resolve(gpu);
        self.target.draw(gpu, &drawn, &view, up);
        drop(drawn);
        let cpu = began.elapsed();
        gpu.device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        let all = began.elapsed();
        FrameNumbers {
            ops: gpu.upload_ops() - ops0,
            pump: self.pump.totals() - pump0,
            cpu_ms: cpu.as_secs_f64() * 1e3,
            gpu_ms: all.as_secs_f64() * 1e3,
            lost: resolution.lost,
        }
    }
}

fn percentile(values: &mut [f64], p: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    let i = ((values.len() - 1) as f64 * p).round() as usize;
    values[i]
}

fn report(plan: &str, phase: &str, frames: &[FrameNumbers]) {
    let n = frames.len().max(1) as f64;
    let ops = frames
        .iter()
        .fold(UploadOps::default(), |a, f| a + f.ops);
    let uploads: u64 = frames.iter().map(|f| f.pump.uploads).sum();
    let bytes: u64 = frames.iter().map(|f| f.pump.upload_bytes).sum();
    let releases: u64 = frames.iter().map(|f| f.pump.releases).sum();
    let acks: u64 = frames.iter().map(|f| f.pump.ack_messages).sum();
    let wait_us: u64 = frames.iter().map(|f| f.pump.wait_micros).sum();
    let wait_frames: u64 = frames.iter().map(|f| f.pump.wait_frames).sum();
    let wait_max = frames
        .iter()
        .map(|f| f.pump.wait_micros_max)
        .max()
        .unwrap_or(0);
    let upload_us: u64 = frames.iter().map(|f| f.pump.upload_micros).sum();
    let lost: usize = frames.iter().map(|f| f.lost).sum();
    let mut cpu: Vec<f64> = frames.iter().map(|f| f.cpu_ms).collect();
    let mut gpu: Vec<f64> = frames.iter().map(|f| f.gpu_ms).collect();
    let per = |v: u64| v as f64 / n;
    let mut line = format!(
        "BENCH plan={plan} phase={phase} frames={} uploads/f={:.2} MB/f={:.3} releases/f={:.2} acks/f={:.2} \
         upload_ms/f={:.3} wait_ms_mean={:.2} wait_frames_mean={:.2} wait_ms_max={:.1} \
         cpu_p50={:.2} cpu_p99={:.2} gpu_p50={:.2} gpu_p99={:.2} lost={lost}",
        frames.len(),
        per(uploads),
        per(bytes) / 1e6,
        per(releases),
        per(acks),
        per(upload_us) / 1e3,
        if uploads > 0 { wait_us as f64 / uploads as f64 / 1e3 } else { 0.0 },
        if uploads > 0 { wait_frames as f64 / uploads as f64 } else { 0.0 },
        wait_max as f64 / 1e3,
        percentile(&mut cpu, 0.5),
        percentile(&mut cpu, 0.99),
        percentile(&mut gpu, 0.5),
        percentile(&mut gpu, 0.99),
    );
    for (name, value) in ops.named() {
        if name.ends_with("bytes") {
            line += &format!(" {name}_MB/f={:.3}", per(value) / 1e6);
        } else {
            line += &format!(" {name}/f={:.2}", per(value));
        }
    }
    eprintln!("{line}");
}

fn plans() -> Vec<(&'static str, tuile_wgpu::ImageryCompression)> {
    let wanted = std::env::var("TUILE_BENCH_PLANS").unwrap_or_default();
    PLANS
        .into_iter()
        .filter(|(name, _)| wanted.is_empty() || wanted.split(',').any(|w| w == *name))
        .collect()
}

/// The aggressive sequence and a still view, per plan.
#[test]
#[ignore = "a bench: run with --ignored --nocapture, in release"]
fn upload_path_under_the_aggressive_sequence() {
    for (plan, compression) in plans() {
        let Some(gpu) = gpu_with(compression) else {
            continue;
        };
        run(plan, &gpu);
        release_probe(plan, &gpu);
    }
}

fn run(plan: &str, gpu: &GpuContext) {
    let imagery = Arc::new(painted(gpu, 256, 256, |x, y| {
        [60 + (x % 160) as u8, 140, 60 + (y % 160) as u8, 255]
    }));
    let tree: Box<dyn TileTree> = tuile_planetary::terrain_tree(everywhere());
    let config = Config {
        maximum_screen_space_error: 2.0,
        ..Config::default()
    };
    let mut budget = BudgetConfig::interactive(Bytes::mib(256));
    budget.pinned_level = Some(PINNED);
    let (stream, server) = in_process_with(tree, Arc::new(Instant256(imagery)), config, budget)
        .expect("a valid budget");
    let mut s = Session {
        server: Box::pin(server.run()),
        stream,
        pump: ContentPump::new(DVec3::ZERO),
        target: Target::new(gpu),
    };
    let start = Pose {
        lon: 6.86,
        lat: 45.83,
        range: 300_000.0,
        pitch: 80.0,
        heading: 0.0,
    };
    // Settled first, at the gesture's start: a session in use, not a cold
    // start.
    for _ in 0..400 {
        s.frame(gpu, start);
        if s.pump.pending_uploads() == 0 && s.pump.converged {
            break;
        }
    }
    let lerp = |a: f64, b: f64, t: f64| a + (b - a) * t;
    let geo = |a: f64, b: f64, t: f64| a * (b / a).powf(t);
    let mut pose = start;
    let mut all = Vec::new();
    let mut phase = |name: &str, frames: usize, s: &mut Session, f: &mut dyn FnMut(f64) -> Pose| {
        let numbers: Vec<FrameNumbers> = (1..=frames)
            .map(|i| s.frame(gpu, f(i as f64 / frames as f64)))
            .collect();
        report(plan, name, &numbers);
        all.extend(numbers);
    };
    // Zoom-in: 300 km to 1 km in 3 s.
    phase("zoom-in", 180, &mut s, &mut |t| {
        pose.range = geo(300_000.0, 1_000.0, t);
        pose
    });
    // Pan: 20 km east in 3 s.
    let lon0 = pose.lon;
    phase("pan", 180, &mut s, &mut |t| {
        pose.lon = lon0 + lerp(0.0, 20_000.0 / (111_320.0 * 45.83f64.to_radians().cos()), t);
        pose
    });
    // Tilt: 80° to 10° below the horizon in 2 s.
    phase("tilt", 120, &mut s, &mut |t| {
        pose.pitch = lerp(80.0, 10.0, t);
        pose
    });
    // Rotate: a full turn in 3 s.
    phase("rotate", 180, &mut s, &mut |t| {
        pose.heading = lerp(0.0, 360.0, t);
        pose
    });
    // Zoom-out: 1 km to 300 km in 3 s.
    phase("zoom-out", 180, &mut s, &mut |t| {
        pose.range = geo(1_000.0, 300_000.0, t);
        pose
    });
    report(plan, "gesture", &all);
    // Still: the camera put down, as a person reading the map.
    for _ in 0..200 {
        s.frame(gpu, pose);
        if s.pump.pending_uploads() == 0 && s.pump.converged {
            break;
        }
    }
    let still: Vec<FrameNumbers> = (0..120).map(|_| s.frame(gpu, pose)).collect();
    report(plan, "still", &still);
    let (buffers, textures, bind_groups, memory) = device_memory(gpu);
    eprintln!(
        "BENCH plan={plan} phase=end device_buffers={buffers} device_textures={textures} \
         device_bind_groups={bind_groups} device_MB={:.1} pump_held_MB={:.1}",
        memory as f64 / 1e6,
        s.pump.held_bytes().iter().map(|(_, b)| b).sum::<u64>() as f64 / 1e6
    );
}

/// From an eviction applied to its memory given back: N drapes and meshes
/// uploaded and drawn, then all evicted at once; frames are run — each a
/// submit and a non-blocking poll, as a host's — until the device holds no
/// more than it did before they came.
fn release_probe(plan: &str, gpu: &GpuContext) {
    use support::{drape, exact, mesh, select, wearing, whole_drape, Scripted};
    use tuile_core::budget::{FrameDeadline, ResourceId};
    use tuile_core::protocol::ServerMessage;
    const N: u32 = 64;
    let tiles: Vec<TileId> = (0..N).map(|i| TileId::from_terrain(8, u64::from(i), 3)).collect();
    let mut stream = Scripted::default();
    let mut pump = ContentPump::new(DVec3::ZERO);
    let target = Target::new(gpu);
    let (view, _, up) = Pose {
        lon: 0.0,
        lat: 0.0,
        range: 1e6,
        pitch: 90.0,
        heading: 0.0,
    }
    .view();
    let frame = |pump: &mut ContentPump, stream: &mut Scripted| {
        pump.pump(stream, gpu, FrameDeadline::UNBOUNDED);
        let (drawn, _) = pump.resolve(gpu);
        target.draw(gpu, &drawn, &view, up);
        let _ = gpu.device.poll(wgpu::PollType::Poll);
    };
    // Whatever earlier runs left in flight is settled first.
    let _ = gpu.device.poll(wgpu::PollType::wait_indefinitely());
    let held = |gpu: &GpuContext| {
        let (buffers, textures, bind_groups, _) = device_memory(gpu);
        (buffers, textures, bind_groups)
    };
    let before = held(gpu);
    for (i, tile) in tiles.iter().enumerate() {
        let (z, x, y) = tile.terrain_coord();
        let rect = GeographicTilingScheme::default().tile_rect(TileCoord::new(z, x, y));
        stream
            .inbox
            .push_back(mesh(*tile, i as u64 + 1, fill_content(&rect, [Some(0.0); 4], 0.0)));
        let texture = painted(gpu, 256, 256, |x, y| [(x + i as u32) as u8, y as u8, 90, 255]);
        stream
            .inbox
            .push_back(drape(*tile, 1000 + i as u64, whole_drape(*tile, z, texture)));
    }
    let items: Vec<_> = tiles
        .iter()
        .map(|t| {
            let (z, _, _) = t.terrain_coord();
            wearing(
                exact(*t),
                ResourceId::drape(*t, tuile_core::budget::LayerId(0), z),
                z,
                tuile_core::budget::UvTransform::IDENTITY,
            )
        })
        .collect();
    stream.inbox.push_back(select(items, 1));
    frame(&mut pump, &mut stream);
    frame(&mut pump, &mut stream);
    let _ = gpu.device.poll(wgpu::PollType::wait_indefinitely());
    let loaded = held(gpu);
    let mut evicted: Vec<ResourceId> = tiles.iter().map(|t| ResourceId::Mesh(*t)).collect();
    evicted.extend(
        tiles
            .iter()
            .map(|t| ResourceId::drape(*t, tuile_core::budget::LayerId(0), t.terrain_coord().0)),
    );
    stream.inbox.push_back(select(Vec::new(), 2));
    stream.inbox.push_back(ServerMessage::Evict { resources: evicted });
    let began = Instant::now();
    let mut frames = 0;
    let mut freed_at = None;
    while frames < 120 {
        frame(&mut pump, &mut stream);
        frames += 1;
        let now = held(gpu);
        if now.0 <= before.0 && now.1 <= before.1 {
            freed_at = Some((frames, began.elapsed()));
            break;
        }
    }
    match freed_at {
        Some((frames, time)) => eprintln!(
            "BENCH plan={plan} phase=release-probe resources={} loaded_buffers={} loaded_textures={} \
             freed_after_frames={frames} freed_after_ms={:.2}",
            2 * N,
            loaded.0 - before.0,
            loaded.1 - before.1,
            time.as_secs_f64() * 1e3
        ),
        None => {
            let now = held(gpu);
            eprintln!(
                "BENCH plan={plan} phase=release-probe resources={} loaded_buffers={} loaded_textures={} \
                 NOT FREED after 120 frames ({} buffers, {} textures over the start)",
                2 * N,
                loaded.0 - before.0,
                loaded.1 - before.1,
                now.0 - before.0,
                now.1 - before.1
            )
        }
    }
}
