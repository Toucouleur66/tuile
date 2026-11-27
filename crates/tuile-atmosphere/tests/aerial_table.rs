// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The ground through the aerial table is the ground through the march.
//!
//! A viewer reads the air between the eye and each ground fragment from the
//! frame's aerial table (`air_seen`), made once a frame at the table's
//! nodes by the very march a film makes for each of its pixels
//! (`air_between`). Here both are drawn for the same ground points —
//! every line of sight of a picture that meets the ground, from views a
//! viewer is flown through: low over the ground by day, grazing the
//! horizon into a low sun, from orbit over the terminator, by moonlight —
//! and the two pictures, as the display shows them (8-bit sRGB), are held
//! to within a code value or two of each other.

#![cfg(feature = "wgpu")]

use glam::DVec3;
use tuile_atmosphere::gpu::{layout, AtmosphereGpu, AtmosphereUniform, SkyViewPass};
use tuile_atmosphere::hillaire::Moment;
use tuile_atmosphere::{AtmosphereParams, LutAtmosphere, Moon, Moonlight, SkyLook, Sun};
use tuile_core::geo::{enu_frame, geodetic_to_ecef, Geodetic};

/// The picture probed: one line of sight a probe, over a 60° field.
const WIDTH: u32 = 192;
const HEIGHT: u32 = 108;
const FOV_Y: f64 = 60.0;

/// The differences allowed between the two pictures, in 8-bit code values
/// over every channel of every ground pixel: the largest, the one a
/// thousandth of the ground's channels exceed, and the mean. The largest
/// is taken inside the ground's silhouette, and is a lone pixel where a
/// line of sight grazes a hill from a valley; on the silhouette itself, a
/// pixel grazing the planet's limb from orbit spans the air's steepest
/// change of all and is held by the thousandth and the mean only.
const MOST: f64 = 5.0;
const THOUSANDTH: f64 = 2.5;
const MEAN: f64 = 0.15;

const PROBE: &str = r"
@group(1) @binding(0) var<storage, read> probes: array<vec4f>;
@group(1) @binding(1) var<storage, read_write> results: array<vec4f>;

// Where the line of sight `d` (unit, from the eye) meets ground `hill`
// metres over the air's sphere, from the eye: 0 for a line that misses it.
fn ground_along(d: vec3f, hill: f32) -> f32 {
    let c = atmosphere.centre.xyz;
    let b = dot(d, c);
    let h = atmosphere.sun.w - hill;
    let q = h * (2.0 * (atmosphere.centre.w + hill) + h);
    let disc = b * b - q;
    if (disc < 0.0 || b <= 0.0) { return 0.0; }
    return q / (b + sqrt(disc));
}

@compute @workgroup_size(64)
fn probe(@builtin(global_invocation_id) id: vec3u) {
    if (id.x >= arrayLength(&probes)) { return; }
    let p = probes[id.x];
    let d = normalize(p.xyz);
    // Hills up to three kilometres high on every other band of the
    // picture, sea level between them: ground short of a line of sight's
    // end, and ground on it.
    var hill = 0.0;
    if (((id.x / PICTURE_WIDTH) / 12u) % 2u == 1u) {
        hill = 1500.0 + 1500.0 * sin(f32(id.x % PICTURE_WIDTH) * 0.11) * cos(f32(id.x / PICTURE_WIDTH) * 0.17);
    }
    let t = ground_along(d, hill);
    if (t <= 0.0) {
        results[4u * id.x] = vec4f(0.0, 0.0, 0.0, -1.0);
        results[4u * id.x + 1u] = vec4f(0.0, 0.0, 0.0, -1.0);
        return;
    }
    let ground = d * t;
    let up = normalize(ground - atmosphere.centre.xyz);
    let ambient = max(
        atmosphere.ambient.xyz + atmosphere.sh_x.xyz * up.x + atmosphere.sh_y.xyz * up.y
            + atmosphere.sh_z.xyz * up.z,
        vec3f(0.0),
    );
    let base = vec3f(p.w);
    let marched = graded(ground_seen(base, ground, ground, up, ambient, 1.0));
    let air = air_seen(ground);
    let tabled = graded(ground_seen_through(base, ground, ground, up, ambient, 1.0, air.veil, air.through));
    results[4u * id.x] = vec4f(marched, t);
    results[4u * id.x + 1u] = vec4f(tabled, t);
    let m = air_between(ground);
    let luma = vec3f(0.2126, 0.7152, 0.0722);
    results[4u * id.x + 2u] = vec4f(dot(m.inscattered * atmosphere.illuminance.xyz, luma), dot(m.transmittance, luma), dot(air.veil, luma), dot(air.through, luma));
    results[4u * id.x + 3u] = vec4f(hill, 0.0, 0.0, 0.0);
}
";

struct Gpu {
    device: wgpu::Device,
    queue: wgpu::Queue,
    atmosphere: AtmosphereGpu,
    pipeline: wgpu::ComputePipeline,
    probe_layout: wgpu::BindGroupLayout,
}

fn gpu() -> Option<Gpu> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = pollster::block_on(instance.request_adapter(&Default::default())).ok()?;
    let (device, queue) = pollster::block_on(adapter.request_device(&Default::default())).ok()?;
    if device.limits().max_compute_workgroups_per_dimension == 0 {
        return None;
    }
    let bgl = layout(&device, wgpu::ShaderStages::COMPUTE, true);
    let mut atmosphere = AtmosphereGpu::new(&device, &bgl, SkyViewPass::Compute);
    atmosphere.hold_with(&queue, AtmosphereParams::default(), LutAtmosphere::of);
    let storage = |binding, read_only| wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    };
    let probe_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("probe"),
        entries: &[storage(0, true), storage(1, false)],
    });
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("aerial probe"),
        source: wgpu::ShaderSource::Wgsl(
            format!(
                "{}{}\n{}\n{}\nconst PICTURE_WIDTH: u32 = {WIDTH}u;\n{PROBE}",
                tuile_atmosphere::hillaire::wgsl_bindings(0),
                tuile_atmosphere::hillaire::WGSL,
                tuile_atmosphere::gpu::SEEN_WGSL,
                tuile_atmosphere::gpu::aerial_wgsl(),
            )
            .into(),
        ),
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("aerial probe"),
        bind_group_layouts: &[Some(&bgl), Some(&probe_layout)],
        immediate_size: 0,
    });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("aerial probe"),
        layout: Some(&pipeline_layout),
        module: &module,
        entry_point: Some("probe"),
        compilation_options: Default::default(),
        cache: None,
    });
    Some(Gpu {
        device,
        queue,
        atmosphere,
        pipeline,
        probe_layout,
    })
}

/// A view a viewer is flown through: where the eye is, where it looks
/// (heading from north, pitch, degrees) and the instant.
struct View {
    name: &'static str,
    at: Geodetic,
    heading: f64,
    pitch: f64,
    instant: f64,
}

fn place(lon: f64, lat: f64, height: f64) -> Geodetic {
    Geodetic {
        lon: lon.to_radians(),
        lat: lat.to_radians(),
        height,
    }
}

/// 2026-06-21, at `hour` UTC.
fn june(hour: f64) -> f64 {
    1_782_043_200.0 + (hour - 12.0) * 3_600.0
}

fn views() -> Vec<View> {
    vec![
        // Laurent's measured view: 2348 m over the Pyrenees, tilted down.
        View {
            name: "2 km, noon, tilted",
            at: place(0.1418, 42.9365, 2_348.0),
            heading: 200.0,
            pitch: -25.0,
            instant: june(12.0),
        },
        // Into a low sun, the horizon in the picture: the air's longest
        // paths and its sharpest change from one line of sight to the next.
        View {
            name: "2 km, low sun ahead, horizon",
            at: place(0.1418, 42.9365, 2_348.0),
            heading: 300.0,
            pitch: -6.0,
            instant: june(19.0),
        },
        View {
            name: "2 km, low sun behind, horizon",
            at: place(0.1418, 42.9365, 2_348.0),
            heading: 120.0,
            pitch: -6.0,
            instant: june(19.0),
        },
        // From orbit, the terminator under the eye.
        View {
            name: "500 km, terminator",
            at: place(30.0, 40.0, 500_000.0),
            heading: 270.0,
            pitch: -60.0,
            instant: june(18.0),
        },
        View {
            name: "500 km, noon, limb",
            at: place(0.0, 30.0, 500_000.0),
            heading: 0.0,
            pitch: -20.0,
            instant: june(12.0),
        },
        // By moonlight, the Moon up over the Pyrenees.
        View {
            name: "2 km, night",
            at: place(0.1418, 42.9365, 2_348.0),
            heading: 180.0,
            pitch: -15.0,
            instant: june(23.5),
        },
    ]
}

/// The view's uniform: its light, exposed as the look exposes it, the
/// aerial table laid over its frustum.
fn uniform(view: &View) -> (AtmosphereUniform, DVec3, DVec3, DVec3) {
    let air = LutAtmosphere::today();
    let look = SkyLook::default();
    let moon = Moon::seen_from(view.instant, view.at).map(|m| Moonlight::of(&m));
    let light = air.light(
        &air,
        Geodetic {
            height: 0.0,
            ..view.at
        },
        Moment {
            to_sun_ecef: Sun::at_unix_seconds(view.instant).direction_ecef,
            moon,
            compensation_ev: 0.0,
        },
        &[],
        look.calibration,
    );
    let eye = geodetic_to_ecef(view.at);
    let enu = enu_frame(view.at);
    let (h, p) = (view.heading.to_radians(), view.pitch.to_radians());
    let forward =
        (enu.x_axis * h.sin() * p.cos() + enu.y_axis * h.cos() * p.cos() + enu.z_axis * p.sin())
            .normalize();
    let right = forward.cross(enu.z_axis).normalize();
    let up = right.cross(forward);
    let u = AtmosphereUniform::new(
        &AtmosphereParams::default(),
        &light,
        eye,
        look.exposure_scale(&light),
        glam::DMat3::IDENTITY,
        &look,
    )
    .seen_from(&light, eye);
    (u, forward, right, up)
}

/// A linear, pre-exposed and graded colour as the display shows it,
/// 0–255 a channel, not rounded: the sRGB curve.
fn display(c: [f32; 3]) -> [f64; 3] {
    c.map(|x| {
        let x = f64::from(x).clamp(0.0, 1.0);
        255.0
            * if x <= 0.003_130_8 {
                12.92 * x
            } else {
                1.055 * x.powf(1.0 / 2.4) - 0.055
            }
    })
}

/// The two pictures of `view` over the ground, as the display shows them:
/// marched and read from the table, pixel for pixel; `made` whether the
/// table is made before they are drawn.
fn pictures(gpu: &mut Gpu, view: &View, made: bool) -> Vec<Pixel> {
    use wgpu::util::DeviceExt;
    let aspect = f64::from(WIDTH) / f64::from(HEIGHT);
    let (u, forward, right, up) = uniform(view);
    gpu.queue
        .write_buffer(&gpu.atmosphere.uniform, 0, bytemuck::bytes_of(&u));
    let half = (FOV_Y.to_radians() * 0.5).tan();
    let albedos = [0.03f32, 0.12, 0.35, 0.7];
    let mut probes = Vec::new();
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let a = (2.0 * (f64::from(x) + 0.5) / f64::from(WIDTH) - 1.0) * half * aspect;
            let b = (1.0 - 2.0 * (f64::from(y) + 0.5) / f64::from(HEIGHT)) * half;
            let d = (forward + right * a + up * b).normalize().as_vec3();
            let albedo = albedos[((x / 8 + y / 8) % 4) as usize];
            probes.push([d.x, d.y, d.z, albedo]);
        }
    }
    let device = &gpu.device;
    let input = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: None,
        contents: bytemuck::cast_slice(&probes),
        usage: wgpu::BufferUsages::STORAGE,
    });
    let size = (probes.len() * 4 * 16) as u64;
    let output = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let read = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: None,
        layout: &gpu.probe_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: input.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: output.as_entire_binding(),
            },
        ],
    });
    let mut encoder = device.create_command_encoder(&Default::default());
    if made {
        gpu.atmosphere.aerial_stale = true;
        gpu.atmosphere.record_aerial(&mut encoder);
    } else {
        // A table of another frame: the eye somewhere else entirely.
        let elsewhere = View {
            name: "elsewhere",
            at: place(120.0, -10.0, 30_000.0),
            heading: 0.0,
            pitch: -40.0,
            instant: june(6.0),
        };
        let (other, ..) = uniform(&elsewhere);
        gpu.queue
            .write_buffer(&gpu.atmosphere.uniform, 0, bytemuck::bytes_of(&other));
        gpu.atmosphere.aerial_stale = true;
        gpu.atmosphere.record_aerial(&mut encoder);
        gpu.queue.submit([encoder.finish()]);
        gpu.queue
            .write_buffer(&gpu.atmosphere.uniform, 0, bytemuck::bytes_of(&u));
        encoder = device.create_command_encoder(&Default::default());
    }
    {
        let mut pass = encoder.begin_compute_pass(&Default::default());
        pass.set_pipeline(&gpu.pipeline);
        pass.set_bind_group(0, &gpu.atmosphere.bind, &[]);
        pass.set_bind_group(1, &bind, &[]);
        pass.dispatch_workgroups((probes.len() as u32).div_ceil(64), 1, 1);
    }
    encoder.copy_buffer_to_buffer(&output, 0, &read, 0, size);
    gpu.queue.submit([encoder.finish()]);
    read.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    let data = read.slice(..).get_mapped_range().expect("mapped").to_vec();
    let f: &[f32] = bytemuck::cast_slice(&data);
    (0..probes.len())
        .filter(|&i| f[16 * i + 3] > 0.0)
        .map(|i| {
            let v = |k: usize| {
                [
                    f[16 * i + 4 * k],
                    f[16 * i + 4 * k + 1],
                    f[16 * i + 4 * k + 2],
                ]
            };
            Pixel {
                at: (i as u32 % WIDTH, i as u32 / WIDTH),
                distance: f[16 * i + 3],
                air: [
                    f[16 * i + 8],
                    f[16 * i + 9],
                    f[16 * i + 10],
                    f[16 * i + 11],
                    f[16 * i + 12],
                ],
                marched: display(v(0)),
                tabled: display(v(1)),
            }
        })
        .collect()
}

/// A ground pixel of both pictures: where it is in the picture, how far
/// its ground is, and its colour marched and read from the table.
#[derive(Clone, Copy)]
struct Pixel {
    at: (u32, u32),
    distance: f32,
    /// The veil's luminance and the transmittance's, marched then read
    /// from the table, and the ground's height over the sphere.
    air: [f32; 5],
    marched: [f64; 3],
    tabled: [f64; 3],
}

impl Pixel {
    fn difference(&self) -> f64 {
        (0..3)
            .map(|c| (self.marched[c] - self.tabled[c]).abs())
            .fold(0.0, f64::max)
    }
}

/// The largest and the mean difference, in code values, over every
/// channel of every ground pixel.
fn differences(pixels: &[Pixel]) -> (f64, f64) {
    let mut most = 0.0f64;
    let mut sum = 0.0;
    for Pixel {
        marched: a,
        tabled: b,
        ..
    } in pixels
    {
        for c in 0..3 {
            let d = (a[c] - b[c]).abs();
            most = most.max(d);
            sum += d;
        }
    }
    (most, sum / (3 * pixels.len().max(1)) as f64)
}

#[test]
fn the_ground_through_the_table_is_the_ground_through_the_march() {
    let Some(mut gpu) = gpu() else {
        eprintln!("skipping GPU test: no usable adapter with compute");
        return;
    };
    let mut failures = Vec::new();
    for view in views() {
        let pixels = pictures(&mut gpu, &view, true);
        assert!(
            pixels.len() > (WIDTH * HEIGHT / 8) as usize,
            "{}: the picture holds too little ground ({} pixels)",
            view.name,
            pixels.len()
        );
        let lit = pixels
            .iter()
            .map(|p| 0.2126 * p.marched[0] + 0.7152 * p.marched[1] + 0.0722 * p.marched[2])
            .sum::<f64>()
            / pixels.len() as f64;
        let (most, mean) = differences(&pixels);
        eprintln!(
            "{:<32} ground {:>6} px, mean luma {lit:>5.1}: differs by {most:.2} at most, {mean:.3} on average",
            view.name,
            pixels.len()
        );
        let mut sorted: Vec<f64> = pixels.iter().map(Pixel::difference).collect();
        sorted.sort_by(f64::total_cmp);
        let worst = pixels
            .iter()
            .max_by(|a, b| a.difference().total_cmp(&b.difference()))
            .expect("ground");
        eprintln!(
            "    99 %: {:.2}, 99.9 %: {:.2}; worst at {:?}, {:.0} m away on ground {:.0} m high: \
             veil {:.4} marched, {:.4} read; transmittance {:.4} marched, {:.4} read",
            sorted[sorted.len() * 99 / 100],
            sorted[sorted.len() * 999 / 1000],
            worst.at,
            worst.distance,
            worst.air[4],
            worst.air[0],
            worst.air[2],
            worst.air[1],
            worst.air[3],
        );
        let thousandth = sorted[sorted.len() * 999 / 1000];
        // The largest within the ground's silhouette: a pixel on its edge
        // (a neighbour not ground) where a line of sight grazes the limb
        // from orbit spans the air's steepest change — from 500 km, the
        // last ground pixel of a column reaches 2 506 km where its
        // neighbour reached 2 321 km, the march's transmittance falling
        // from 0.120 to 0.023 across the one pixel; read between nodes,
        // 0.034 (14.4 code values). Such pixels count in the thousandth
        // and the mean, held as everywhere else.
        let ground: std::collections::HashSet<(u32, u32)> = pixels.iter().map(|p| p.at).collect();
        let inside = |(x, y): (u32, u32)| {
            [(1i64, 0i64), (-1, 0), (0, 1), (0, -1)]
                .iter()
                .all(|(dx, dy)| {
                    let (nx, ny) = (i64::from(x) + dx, i64::from(y) + dy);
                    match (u32::try_from(nx), u32::try_from(ny)) {
                        (Ok(nx), Ok(ny)) if nx < WIDTH && ny < HEIGHT => ground.contains(&(nx, ny)),
                        // The picture's own edge is no silhouette.
                        _ => true,
                    }
                })
        };
        let most_inside = pixels
            .iter()
            .filter(|p| inside(p.at))
            .map(Pixel::difference)
            .fold(0.0, f64::max);
        if most_inside > MOST || thousandth > THOUSANDTH || mean > MEAN {
            failures.push(format!(
                "{}: {most_inside:.2} at most inside the silhouette ({most:.2} on it), \
                 {thousandth:.2} past a thousandth, {mean:.3} on average",
                view.name
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "the table's ground is not the march's: {failures:?}"
    );
}

/// The tolerance has teeth: a table made for another frame — the eye
/// elsewhere — is told apart from the march at once.
#[test]
fn a_table_made_for_another_frame_is_told_apart() {
    let Some(mut gpu) = gpu() else {
        eprintln!("skipping GPU test: no usable adapter with compute");
        return;
    };
    let view = &views()[0];
    let (most, mean) = differences(&pictures(&mut gpu, view, false));
    eprintln!("a stale table: {most:.2} at most, {mean:.3} on average");
    assert!(most > 4.0 * MOST, "{most:.2} at most");
}
