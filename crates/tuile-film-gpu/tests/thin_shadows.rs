// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The shadows of thin shapes, on a real GPU, under a sun that moves.
//!
//! The bench: flat ground at null island, a ribbon a few metres wide 300 m
//! over it, as a host draws a track — about two texels of the sun's map,
//! which on this view are 6.7 m. The map is fitted to an oblique camera, as
//! a film's is, and the pictures are taken by an observer that looks
//! straight down on the shadow and never moves: whatever changes in them
//! from one frame to the next is the shadow's doing.
//!
//! Between frames the sun moves by what a time-lapse moves it, 0.0035°.
//! The shadow moves by a hundredth of a texel: it should stand still.
//!
//! Skipped when the machine has no adapter.

use glam::DVec3;
use tuile_film::{
    BakedView, FrameCamera, FrameLight, Imagery, Look, OverlayBlend, OverlayDepth, OverlayEyes,
    OverlayMesh, OverlayShadow, TileKey,
};
use tuile_film_gpu::{FilmGpu, Settings, Shot, TileMesh};

const A: f64 = 6_378_137.0;
const UP: DVec3 = DVec3::X;
const EAST: DVec3 = DVec3::Y;
const NORTH: DVec3 = DVec3::Z;
const GROUND_SRGB: [u8; 4] = [90, 120, 70, 255];
const YELLOW: [f32; 4] = [1.0, 0.8, 0.0, 1.0];
/// A time-lapse's sun: a flight of two hours in five minutes of film.
const SUN_STEP_DEG: f64 = 0.0035;
const SUN_ELEVATION_DEG: f64 = 28.0;
const SUN_AZIMUTH_DEG: f64 = 235.0;
const TRAIL_HEIGHT: f64 = 300.0;

fn device() -> Option<(wgpu::Device, wgpu::Queue)> {
    pollster::block_on(async {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .ok()?;
        adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("thin shadows"),
                required_limits: adapter.limits(),
                ..Default::default()
            })
            .await
            .ok()
    })
}

fn le<T: Copy + bytemuck::Pod>(v: &[T]) -> Vec<u8> {
    bytemuck::cast_slice(v).to_vec()
}

/// Towards a sun at `azimuth` (clockwise from north) and `elevation`, in
/// degrees, at null island.
fn to_sun(azimuth: f64, elevation: f64) -> DVec3 {
    let (az, el) = (azimuth.to_radians(), elevation.to_radians());
    (UP * el.sin() + (EAST * az.sin() + NORTH * az.cos()) * el.cos()).normalize()
}

fn look() -> Look {
    Look {
        to_sun: to_sun(SUN_AZIMUTH_DEG, SUN_ELEVATION_DEG).as_vec3(),
        imagery: Imagery::Decoded,
        shadow: 1.0,
        contrast: 1.0,
        saturation: 1.0,
        ..Look::default()
    }
}

/// The look with the penumbrae of projected shadows cut to `scale` times
/// the sun's disc.
fn look_with_penumbra(scale: f32) -> Look {
    Look {
        penumbra_scale: scale,
        ..look()
    }
}

fn view(eye: DVec3, target: DVec3, fovy: f64, size: (u32, u32)) -> BakedView {
    let d = (target - eye).normalize();
    BakedView {
        position: eye.to_array(),
        direction: d.to_array(),
        up: if d.dot(UP).abs() > 0.99 {
            NORTH.to_array()
        } else {
            UP.to_array()
        },
        viewport_px: [f64::from(size.0), f64::from(size.1)],
        fovy_rad: fovy,
    }
}

/// A renderer with the ground in it: 80 km of flat ground, one colour.
fn film(size: (u32, u32)) -> Option<(FilmGpu, TileKey)> {
    film_with(size, look())
}

/// The same, under `look`.
fn film_with(size: (u32, u32), look: Look) -> Option<(FilmGpu, TileKey)> {
    let (device, queue) = device()?;
    let mut film = FilmGpu::new(
        device,
        queue,
        Settings {
            width: size.0,
            height: size.1,
            supersample: 2,
            look,
        },
    );
    let half = 40_000.0f32;
    let p = [
        0.0f32, -half, -half, 0.0, half, -half, 0.0, half, half, 0.0, -half, half,
    ];
    let texture = film.create_albedo(4, 4);
    film.write_rgba(&texture, &GROUND_SRGB.repeat(16));
    let key = TileKey { id: 1, drape: 0 };
    film.enter(
        key,
        &TileMesh {
            origin_ecef: [A, 0.0, 0.0],
            positions: &le(&p),
            normals: &le(&[1.0f32, 0.0, 0.0].repeat(4)),
            uvs: &le(&[0.0f32, 1.0, 1.0, 1.0, 1.0, 0.0, 0.0, 0.0]),
            indices: &le(&[0u32, 1, 2, 0, 2, 3]),
            index_count: 6,
            base_color_factor: [1.0; 4],
        },
        Some(texture),
    )
    .expect("enter");
    Some((film, key))
}

/// Three kilometres of winding track, a point every 15 m, `height` over
/// the ground; the head last.
fn track(height: f64) -> Vec<DVec3> {
    let dir = (EAST * 20f64.to_radians().cos() + NORTH * 20f64.to_radians().sin()).normalize();
    let across = UP.cross(dir);
    (0..=200)
        .map(|i| {
            let u = f64::from(i) * 15.0 - 3000.0;
            let w = 150.0 * (u * std::f64::consts::TAU / 700.0).sin();
            DVec3::new(A + height, 0.0, 0.0) + dir * u + across * w
        })
        .collect()
}

/// The track as a host draws it: a band flat to the eye, `half` metres
/// either side of the line.
fn ribbon(points: &[DVec3], eye: DVec3, half: f64, shadow: OverlayShadow) -> OverlayMesh {
    let origin = *points.last().expect("a track");
    let n = points.len();
    let mut positions = Vec::new();
    for (i, &p) in points.iter().enumerate() {
        let along = points[(i + 1).min(n - 1)] - points[i.saturating_sub(1)];
        let side = along.cross(p - eye).normalize();
        for s in [1.0, -1.0] {
            positions.push((p + side * s * half - origin).as_vec3().to_array());
        }
    }
    let mut indices = Vec::new();
    for i in 0..n as u32 - 1 {
        let a = 2 * i;
        indices.extend_from_slice(&[a, a + 1, a + 2, a + 1, a + 3, a + 2]);
    }
    OverlayMesh {
        origin_ecef: origin.to_array(),
        colors: vec![YELLOW; positions.len()],
        positions,
        indices,
        depth: OverlayDepth::Terrain,
        blend: OverlayBlend::Opaque,
        eyes: OverlayEyes::Both,
        shadow,
        ..OverlayMesh::default()
    }
}

/// The picture the last frame submitted left, tightly packed RGBA.
fn picture(film: &FilmGpu) -> Vec<u8> {
    let (width, height) = (film.settings().width, film.settings().height);
    let padded = (width * 4).div_ceil(256) * 256;
    let buffer = film.device().create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: u64::from(padded * height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut e = film.device().create_command_encoder(&Default::default());
    e.copy_texture_to_buffer(
        film.output().as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(height),
            },
        },
        film.output().size(),
    );
    film.queue().submit([e.finish()]);
    let slice = buffer.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("map"));
    film.device()
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    let data = slice.get_mapped_range().to_vec();
    data.chunks(padded as usize)
        .flat_map(|r| r[..(width * 4) as usize].to_vec())
        .collect()
}

/// How deep in shadow each pixel is: 0 lit, 1 as dark as the darkest the
/// sequence shows; the shape's own pixels (and a pixel round them) are
/// `None`.
fn shade(pictures: &[Vec<u8>], width: usize) -> Vec<Vec<Option<f32>>> {
    let n = pictures[0].len() / 4;
    let shape = |p: &[u8], i: usize| p[4 * i] > 200 && p[4 * i + 2] < 100;
    let mut masked = vec![false; n];
    for p in pictures {
        for i in 0..n {
            if shape(p, i) {
                for dy in -2i64..=2 {
                    for dx in -2i64..=2 {
                        let j = i as i64 + dy * width as i64 + dx;
                        if (0..n as i64).contains(&j) {
                            masked[j as usize] = true;
                        }
                    }
                }
            }
        }
    }
    // Lit ground is the commonest green; the deepest shade any frame shows
    // is full shadow.
    let green = |p: &[u8], i: usize| f32::from(p[4 * i + 1]);
    let mut all: Vec<f32> = pictures[0]
        .chunks(4)
        .enumerate()
        .filter(|(i, _)| !masked[*i])
        .map(|(_, c)| f32::from(c[1]))
        .collect();
    all.sort_by(f32::total_cmp);
    let lit = all[all.len() / 2];
    let darkest = pictures
        .iter()
        .flat_map(|p| (0..n).filter(|i| !masked[*i]).map(move |i| green(p, i)))
        .fold(lit, f32::min);
    let depth = (lit - darkest).max(1.0);
    pictures
        .iter()
        .map(|p| {
            (0..n)
                .map(|i| (!masked[i]).then(|| ((lit - green(p, i)) / depth).clamp(0.0, 1.0)))
                .collect()
        })
        .collect()
}

/// Over the pixels any frame shows in shadow: the mean change from one
/// frame to the next, the mean second difference — what is left once a
/// steady drift is taken out: flicker — both in shares of full shadow, and
/// how many pixels are in shadow, on average.
struct Flicker {
    d1: f32,
    d2: f32,
    shaded: f32,
}

fn flicker(shades: &[Vec<Option<f32>>]) -> Flicker {
    let n = shades[0].len();
    let band: Vec<usize> = (0..n)
        .filter(|&i| shades.iter().any(|s| s[i].is_some_and(|v| v > 0.1)))
        .collect();
    let at = |t: usize, i: usize| shades[t][i].unwrap_or(0.0);
    let (mut d1, mut d2, mut c1, mut c2) = (0.0, 0.0, 0usize, 0usize);
    for t in 1..shades.len() {
        for &i in &band {
            d1 += (at(t, i) - at(t - 1, i)).abs();
            c1 += 1;
            if t + 1 < shades.len() {
                d2 += (at(t + 1, i) - 2.0 * at(t, i) + at(t - 1, i)).abs();
                c2 += 1;
            }
        }
    }
    if std::env::var("THIN_SHADOWS_DEBUG").is_ok() {
        for t in 1..shades.len() {
            let changed = band
                .iter()
                .filter(|&&i| (at(t, i) - at(t - 1, i)).abs() > 0.05)
                .count();
            let sum: f32 = band.iter().map(|&i| (at(t, i) - at(t - 1, i)).abs()).sum();
            let level: f32 = band.iter().map(|&i| at(t, i)).sum();
            eprintln!(
                "frame {t}: {changed} px changed > 5 %, sum |d| {sum:.2}, total shade {level:.1}"
            );
        }
    }
    let shaded = shades
        .iter()
        .map(|s| s.iter().filter(|v| v.is_some_and(|v| v > 0.3)).count() as f32)
        .sum::<f32>()
        / shades.len() as f32;
    Flicker {
        d1: d1 / c1.max(1) as f32,
        d2: d2 / c2.max(1) as f32,
        shaded,
    }
}

const SIZE: (u32, u32) = (480, 270);

/// `frames` pictures of the ribbon's shadow by the still observer, the sun
/// moving `SUN_STEP_DEG` a frame, the map fitted to a film's oblique camera
/// 2.5 km from the track's head.
fn sequence(shadow: OverlayShadow, frames: u32) -> Option<Vec<Vec<u8>>> {
    let (mut film, key) = film(SIZE)?;
    let points = track(TRAIL_HEIGHT);
    let head = *points.last().expect("a track");
    let (yaw, tilt) = (200f64.to_radians(), 30f64.to_radians());
    let eye =
        head + ((EAST * yaw.sin() + NORTH * yaw.cos()) * tilt.cos() + UP * tilt.sin()) * 2500.0;
    let aspect = SIZE.0 as f32 / SIZE.1 as f32;
    let filmed = FrameCamera::of(&view(eye, head, std::f64::consts::FRAC_PI_4, SIZE), aspect);
    // 8 m wide: the product's band at 2.5 km is 16 m, seen from the side
    // the sun sees it half that.
    let mesh = ribbon(&points, eye, 4.0, shadow);
    let sun = to_sun(SUN_AZIMUTH_DEG, SUN_ELEVATION_DEG);
    let middle = points[points.len() / 2];
    let under = middle - sun * (TRAIL_HEIGHT / sun.dot(UP));
    let observer = FrameCamera::of(&view(under + UP * 3000.0, under, 0.6, SIZE), aspect);
    let mut out = Vec::new();
    for k in 0..frames {
        let s = to_sun(
            SUN_AZIMUTH_DEG + f64::from(k) * SUN_STEP_DEG,
            SUN_ELEVATION_DEG,
        );
        film.set_light(FrameLight::of(&Look {
            to_sun: s.as_vec3(),
            ..look()
        }));
        let shot = Shot {
            camera: observer,
            frame: filmed,
            eye: None,
        };
        let encoder = film
            .render_shot(&shot, &[key], std::slice::from_ref(&mesh))
            .expect("render");
        film.queue().submit([encoder.finish()]);
        out.push(picture(&film));
        if let Ok(dir) = std::env::var("THIN_SHADOWS_DEBUG") {
            image::save_buffer(
                format!("{dir}/{shadow:?}-{k:02}.png"),
                out.last().expect("a picture"),
                SIZE.0,
                SIZE.1,
                image::ColorType::Rgba8,
            )
            .expect("saved");
        }
    }
    Some(out)
}

/// A ribbon whose shadow is projected: its shadow is all there, and holds
/// still while the sun moves — under a hundredth of full shadow changes
/// from a frame to the next, against the map's several hundredths.
#[test]
fn a_projected_shadow_holds_still_as_the_sun_moves() {
    let Some(pictures) = sequence(OverlayShadow::Project, 12) else {
        eprintln!("no GPU adapter: skipped");
        return;
    };
    let f = flicker(&shade(&pictures, SIZE.0 as usize));
    eprintln!(
        "projected: d1 {:.4} d2 {:.4} shaded {:.0} px",
        f.d1, f.d2, f.shaded
    );
    // The whole band: 3 km of track 8 m wide, at 6.9 m a pixel, is some
    // 500 pixels of full shadow, and its edges.
    assert!(f.shaded > 400.0, "only {:.0} pixels in shadow", f.shaded);
    // What is left is the picture's own sampling of an edge that moves by
    // a two-hundredth of a pixel a frame: now and then a sample of an edge
    // pixel crosses it — a quarter of that pixel, at two by two samples.
    // The map's shadow changes ten times as much (the reference below).
    assert!(f.d2 < 0.02, "flicker {:.4} of full shadow a frame", f.d2);
    assert!(f.d1 < 0.01, "change {:.4} of full shadow a frame", f.d1);
}

/// The same ribbon drawn into the sun's map, as any thin caster that does
/// not project its shadow is — a mascot's strings, say. Its shadow is
/// coarser and it flickers, but each sample of the picture reads a map of
/// its own, its grid moved by a fraction of a texel, so that the picture's
/// pixel, which averages them, sees a shadow four grids deep: its flicker
/// is held to a bound. On one grid, d2 was 0.087; on four it is 0.055 —
/// a safety net, not a cure: a shadow that must hold still is projected
/// (0.008, above).
#[test]
fn a_thin_caster_in_the_map_flickers_within_a_bound() {
    let Some(pictures) = sequence(OverlayShadow::Cast, 12) else {
        eprintln!("no GPU adapter: skipped");
        return;
    };
    let f = flicker(&shade(&pictures, SIZE.0 as usize));
    eprintln!(
        "in the map: d1 {:.4} d2 {:.4} shaded {:.0} px",
        f.d1, f.d2, f.shaded
    );
    assert!(f.shaded > 300.0, "only {:.0} pixels in shadow", f.shaded);
    assert!(f.d2 < 0.065, "flicker {:.4} of full shadow a frame", f.d2);
}

/// Where a plate's hard projected shadow (no penumbra) falls: `height`
/// over the tangent of the sun's elevation from under it, down the sun's
/// azimuth — at low suns as at high ones. Under the sun's own disc a 30 m
/// plate 200 m up has no umbra at all at 6° (its 3 m across the rays is
/// under the disc's 18 m there), so the shadow's place and size are pinned
/// hard here, and its penumbra below.
#[test]
fn a_projected_shadow_falls_height_over_tan_elevation_away() {
    let size = (320, 240);
    let Some((mut film, key)) = film_with(size, look_with_penumbra(0.0)) else {
        eprintln!("no GPU adapter: skipped");
        return;
    };
    let aspect = size.0 as f32 / size.1 as f32;
    // A level plate 30 m a side, 200 m up, over null island.
    let height = 200.0;
    let half = 15.0f32;
    let plate = OverlayMesh {
        origin_ecef: [A + height, 0.0, 0.0],
        positions: vec![
            [0.0, -half, -half],
            [0.0, half, -half],
            [0.0, half, half],
            [0.0, -half, half],
        ],
        colors: vec![YELLOW; 4],
        indices: vec![0, 1, 2, 0, 2, 3],
        depth: OverlayDepth::Terrain,
        blend: OverlayBlend::Opaque,
        eyes: OverlayEyes::Both,
        shadow: OverlayShadow::Project,
        ..OverlayMesh::default()
    };
    // The sun in the south: the shadow falls north.
    for elevation in [6.0, 15.0, 30.0, 60.0] {
        let away = height / f64::to_radians(elevation).tan();
        let s = to_sun(180.0, elevation);
        film.set_light(FrameLight::of(&Look {
            to_sun: s.as_vec3(),
            ..look()
        }));
        // Straight down on where it should be: 0.25 m a pixel.
        let under = DVec3::new(A, 0.0, 0.0) + NORTH * away;
        let fovy = 2.0 * (30.0f64 / 1000.0).atan();
        let seen = FrameCamera::of(&view(under + UP * 1000.0, under, fovy, size), aspect);
        let encoder = film
            .render_shot(&Shot::mono(seen), &[key], std::slice::from_ref(&plate))
            .expect("render");
        film.queue().submit([encoder.finish()]);
        let shades = shade(&[picture(&film)], size.0 as usize);
        // The shadow's centre, in pixels, then in metres from the picture's.
        let (mut sx, mut sy, mut w) = (0.0, 0.0, 0.0);
        for (i, v) in shades[0].iter().enumerate() {
            if let Some(v) = v.filter(|v| *v > 0.5) {
                sx += v * (i % size.0 as usize) as f32;
                sy += v * (i / size.0 as usize) as f32;
                w += v;
            }
        }
        assert!(w > 100.0, "{elevation}°: no shadow");
        let metres_per_pixel = 60.0 / f64::from(size.1);
        let (cx, cy) = (f64::from(sx / w) + 0.5, f64::from(sy / w) + 0.5);
        let east = (cx - f64::from(size.0) / 2.0) * metres_per_pixel;
        let north = (f64::from(size.1) / 2.0 - cy) * metres_per_pixel;
        eprintln!(
            "{elevation}°: shadow centre {east:.2} m east, {north:.2} m north of {away:.1} m"
        );
        assert!(
            east.abs() < 0.5 && north.abs() < 0.5,
            "{elevation}°: the shadow is {east:.2} m east and {north:.2} m north of where it falls"
        );
        // And it is the plate's size: 30 m by 30 m on level ground.
        let area = f64::from(w) * metres_per_pixel * metres_per_pixel;
        assert!((area - 900.0).abs() < 60.0, "{elevation}°: {area:.0} m²");
    }
}

/// The sun's disc, seen from the ground, as the `penumbra_scale` of 1 cuts
/// it: its angular radius, in radians.
const SUN_RADIUS: f64 = 4.65e-3;
/// The sun of the penumbra's bench: in the south, 30° up.
const PENUMBRA_ELEVATION_DEG: f64 = 30.0;
const PENUMBRA_SIZE: (u32, u32) = (320, 240);

/// Of a disc, the share a straight edge `x` radii past its centre leaves
/// covered: a half-plane's share of a uniform sun.
fn edge_share(x: f64) -> f64 {
    let x = x.clamp(-1.0, 1.0);
    (x.acos() - x * (1.0 - x * x).sqrt()) / std::f64::consts::PI
}

/// The ground's penumbra across an edge along the sun's azimuth, for a
/// caster `height` up and the disc cut to `scale`: the disc's radius at the
/// caster, `height / sin(elevation)` up the ray.
fn radius(height: f64, scale: f64) -> f64 {
    height / PENUMBRA_ELEVATION_DEG.to_radians().sin() * (SUN_RADIUS * scale).tan()
}

/// A level plate 400 m a side `height` up, unseen (wholly transparent),
/// that projects its shadow: its east edge at null island's meridian, so
/// that under a sun in the south its shadow's east edge runs north–south
/// through it, `height / tan(elevation)` north.
fn plate(height: f64) -> OverlayMesh {
    OverlayMesh {
        origin_ecef: [A + height, 0.0, 0.0],
        positions: vec![
            [0.0, -400.0, -200.0],
            [0.0, 0.0, -200.0],
            [0.0, 0.0, 200.0],
            [0.0, -400.0, 200.0],
        ],
        colors: vec![[0.0; 4]; 4],
        indices: vec![0, 1, 2, 0, 2, 3],
        depth: OverlayDepth::Terrain,
        blend: OverlayBlend::Translucent,
        eyes: OverlayEyes::Both,
        shadow: OverlayShadow::Project,
        ..OverlayMesh::default()
    }
}

/// A strip `width` metres wide, 400 m long north–south, `height` up, its
/// east edge on null island's meridian: a ribbon as a host draws one, two
/// triangles every 15 m, unseen, projecting its shadow.
fn strip(height: f64, width: f32) -> OverlayMesh {
    let mut positions = Vec::new();
    let mut indices = Vec::new();
    for i in 0..=26u32 {
        let north = i as f32 * 15.0 - 200.0;
        positions.push([0.0, -width, north]);
        positions.push([0.0, 0.0, north]);
        if i > 0 {
            let v = 2 * i;
            indices.extend_from_slice(&[v - 2, v - 1, v, v, v - 1, v + 1]);
        }
    }
    OverlayMesh {
        origin_ecef: [A + height, 0.0, 0.0],
        colors: vec![[0.0; 4]; positions.len()],
        positions,
        indices,
        ..plate(height)
    }
}

/// A frame of the ground straight down from `eye` (over null island,
/// north up the picture), 40 m of it top to bottom, with `caster`
/// projecting its shadow under a look of `scale`: its picture, and how
/// much of the sun projected shadows take from each of its samples.
struct Seen {
    picture: Vec<u8>,
    taken: Vec<f32>,
}

fn seen(caster: OverlayMesh, scale: f32, eye: DVec3) -> Option<Seen> {
    let size = PENUMBRA_SIZE;
    // Two stops under the bench's look: lit ground well under the knee
    // where highlights roll off, so that a picture's light is the ground's.
    let dim = Look {
        exposure_ev: -1.0,
        ..look_with_penumbra(scale)
    };
    let (mut film, key) = film_with(size, dim)?;
    film.set_light(FrameLight::of(&Look {
        to_sun: to_sun(180.0, PENUMBRA_ELEVATION_DEG).as_vec3(),
        ..dim
    }));
    let below = eye.length() - A;
    let under = eye * (A / eye.length());
    let fovy = 2.0 * (20.0 / below).atan();
    let aspect = size.0 as f32 / size.1 as f32;
    let camera = FrameCamera::of(&view(eye, under, fovy, size), aspect);
    let encoder = film
        .render_shot(&Shot::mono(camera), &[key], &[caster])
        .expect("render");
    film.queue().submit([encoder.finish()]);
    Some(Seen {
        picture: picture(&film),
        taken: taken(&film),
    })
}

/// The projected shadows' target of the last frame, a value a sample.
fn taken(film: &FilmGpu) -> Vec<f32> {
    let texture = film.projected_shadow_texture();
    let (width, height) = (texture.width(), texture.height());
    let padded = (width * 2).div_ceil(256) * 256;
    let buffer = film.device().create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: u64::from(padded * height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut e = film.device().create_command_encoder(&Default::default());
    e.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(height),
            },
        },
        texture.size(),
    );
    film.queue().submit([e.finish()]);
    let slice = buffer.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("map"));
    film.device()
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    let data = slice.get_mapped_range().to_vec();
    data.chunks(padded as usize)
        .flat_map(|r| {
            r[..(width * 2) as usize]
                .chunks(2)
                .map(|h| half(u16::from_le_bytes([h[0], h[1]])))
                .collect::<Vec<_>>()
        })
        .collect()
}

/// An IEEE half, widened.
fn half(h: u16) -> f32 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exponent = i32::from((h >> 10) & 0x1f);
    let mantissa = f32::from(h & 0x3ff);
    sign * match exponent {
        0 => mantissa * 2f32.powi(-24),
        31 => f32::INFINITY,
        e => (1.0 + mantissa / 1024.0) * 2f32.powi(e - 15),
    }
}

/// Across the frame, the share of the sun its middle rows of samples lose,
/// column by column — at most the whole sun, as the resolve takes it — and
/// the metres east of the frame's centre each column is at.
fn shares(seen: &Seen) -> Vec<(f64, f64)> {
    let (w, h) = (2 * PENUMBRA_SIZE.0 as usize, 2 * PENUMBRA_SIZE.1 as usize);
    let metres = 40.0 / h as f64;
    (0..w)
        .map(|x| {
            let rows = h / 2 - 40..h / 2 + 40;
            let n = rows.len() as f64;
            let v: f64 = rows
                .map(|y| f64::from(seen.taken[y * w + x].min(1.0)))
                .sum();
            ((x as f64 + 0.5 - w as f64 / 2.0) * metres, v / n)
        })
        .collect()
}

/// The share at the column nearest `x` metres east of the centre.
fn share_at(shares: &[(f64, f64)], x: f64) -> f64 {
    shares
        .iter()
        .min_by(|a, b| (a.0 - x).abs().total_cmp(&(b.0 - x).abs()))
        .expect("a column")
        .1
}

/// Over the edge of the shadow of a caster `height` up: the eye 200 m
/// over the ground where the edge falls.
fn edge_eye(height: f64) -> DVec3 {
    let away = height / PENUMBRA_ELEVATION_DEG.to_radians().tan();
    DVec3::new(A + 200.0, 0.0, 0.0) + NORTH * away
}

/// The mean green of the picture's middle rows straight down, linear (its
/// sRGB curve undone), at the column nearest `x` metres east of its centre.
fn green_at(seen: &Seen, x: f64) -> f32 {
    let (w, h) = (PENUMBRA_SIZE.0 as usize, PENUMBRA_SIZE.1 as usize);
    let column = ((x / (40.0 / h as f64) + w as f64 / 2.0) as usize).min(w - 1);
    let linear = |v: u8| {
        let v = f32::from(v) / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    let rows = h / 2 - 20..h / 2 + 20;
    let n = rows.len() as f32;
    rows.map(|y| linear(seen.picture[4 * (y * w + column) + 1]))
        .sum::<f32>()
        / n
}

/// A raised caster's projected shadow has a penumbra: as wide as the sun's
/// disc is at the caster, straddling the hard edge, the share of the sun
/// it takes the share of the disc the edge hides — and wider the higher
/// the caster is, or the wider the disc is cut. A caster on the ground, or
/// no disc at all, keeps the hard edge. And the penumbra moves no darkness
/// about: across the edge, the soft shadow takes as much of the sun as the
/// hard one — and the picture's ground is darkened as the shares say.
#[test]
fn a_raised_casters_projected_shadow_has_a_penumbra_as_wide_as_the_sun() {
    let Some(hard_seen) = seen(plate(400.0), 0.0, edge_eye(400.0)) else {
        eprintln!("no GPU adapter: skipped");
        return;
    };
    let hard = shares(&hard_seen);
    let soft = |height: f64, scale: f32| {
        let seen = seen(plate(height), scale, edge_eye(height)).expect("an adapter");
        (shares(&seen), seen)
    };

    // No disc: the edge as sharp as the frame's samples.
    eprintln!(
        "hard: {:.3} 0.2 m out, {:.3} 0.2 m in",
        share_at(&hard, 0.2),
        share_at(&hard, -0.2)
    );
    assert!(share_at(&hard, 0.2) == 0.0 && share_at(&hard, -0.2) == 1.0);

    // The sun's own disc, 400 m up: 3.7 m either side of the edge.
    let (high, high_seen) = soft(400.0, 1.0);
    let r = radius(400.0, 1.0);
    for k in [-1.05, -0.7, -0.4, 0.0, 0.4, 0.7, 1.05] {
        let (got, want) = (share_at(&high, k * r), edge_share(k));
        eprintln!(
            "400 m, the sun's disc: {k:+.2} r ({:+.2} m): {got:.4}, want {want:.4}",
            k * r
        );
        assert!(
            (got - want).abs() < 0.01,
            "{k} r: {got:.4} against {want:.4}"
        );
    }
    // As much of the sun taken across the edge as by the hard shadow.
    let mean = |s: &[(f64, f64)]| s.iter().map(|v| v.1).sum::<f64>() / s.len() as f64;
    eprintln!(
        "mean share: hard {:.4}, soft {:.4}",
        mean(&hard),
        mean(&high)
    );
    assert!((mean(&high) - mean(&hard)).abs() < 0.002);
    // And the picture says so: lit ground, full shadow, and halfway.
    let (lit, full) = (green_at(&hard_seen, 20.0), green_at(&hard_seen, -20.0));
    let half_way = green_at(&high_seen, 0.0);
    let darkened = f64::from((lit - half_way) / (lit - full));
    eprintln!(
        "picture: lit {lit:.4}, full shadow {full:.4}, at the edge {half_way:.4} ({darkened:.3})"
    );
    assert!(lit > 2.0 * full, "no shadow in the picture");
    assert!((darkened - 0.5).abs() < 0.06, "{darkened:.3} at the edge");

    // A quarter as high: a quarter as wide, nothing left 1.5 m out where
    // the higher caster still takes a quarter of the sun.
    let (low, _) = soft(100.0, 1.0);
    eprintln!(
        "1.5 m out: {:.4} at 100 m, {:.4} at 400 m",
        share_at(&low, 1.5),
        share_at(&high, 1.5)
    );
    assert!(share_at(&low, 1.5) == 0.0);
    assert!(share_at(&high, 1.5) > 0.2);

    // The disc cut eight times as wide: so is the penumbra.
    let (wide, _) = soft(100.0, 8.0);
    let r = radius(100.0, 8.0);
    for k in [-0.6, 0.2, 0.6] {
        let (got, want) = (share_at(&wide, k * r), edge_share(k));
        eprintln!(
            "100 m, 8 discs: {k:+.1} r ({:+.2} m): {got:.4}, want {want:.4}",
            k * r
        );
        assert!(
            (got - want).abs() < 0.01,
            "{k} r: {got:.4} against {want:.4}"
        );
    }

    // A caster a metre off the ground: its penumbra under a centimetre,
    // even at eight discs.
    let (ground, _) = soft(1.0, 8.0);
    eprintln!(
        "1 m up, 8 discs: {:.4} 0.2 m out, {:.4} 0.2 m in",
        share_at(&ground, 0.2),
        share_at(&ground, -0.2)
    );
    assert!(share_at(&ground, 0.2) < 0.01 && share_at(&ground, -0.2) > 0.99);
}

/// A ribbon narrower than its penumbra: its shadow is fainter and wider,
/// and takes as much of the sun in all as its hard one does — its
/// triangles' shares of the disc add up across the edges they share, so
/// the ribbon is covered as one shape, not as the last of its triangles
/// drawn.
#[test]
fn a_ribbon_narrower_than_its_penumbra_keeps_its_whole_shadow() {
    let (height, width, scale) = (400.0, 3.0, 4.0);
    let eye = edge_eye(height) - EAST * (f64::from(width) / 2.0);
    let Some(hard) = seen(strip(height, width), 0.0, eye) else {
        eprintln!("no GPU adapter: skipped");
        return;
    };
    let hard = shares(&hard);
    let soft = shares(&seen(strip(height, width), scale, eye).expect("an adapter"));
    let r = radius(height, f64::from(scale));
    let mut worst = 0.0f64;
    for (x, v) in &soft {
        let e = x - f64::from(width) / 2.0;
        let want = edge_share(e / r) - edge_share((e + f64::from(width)) / r);
        worst = worst.max((v - want).abs());
    }
    let sum = |s: &[(f64, f64)]| s.iter().map(|v| v.1).sum::<f64>();
    let peak = soft.iter().map(|v| v.1).fold(0.0, f64::max);
    eprintln!(
        "a {width} m ribbon {height} m up, {scale} discs: sun taken {:.2} hard, {:.2} soft; \
         soft at most {peak:.4}, {worst:.4} at worst from a uniform disc's",
        sum(&hard),
        sum(&soft)
    );
    // 3 m under a disc 30 m across: an eighth of the sun at most.
    assert!(peak < 0.15, "{peak}");
    assert!(worst < 0.01, "{worst}");
    assert!(
        (sum(&soft) / sum(&hard) - 1.0).abs() < 0.01,
        "{:.2} soft against {:.2} hard",
        sum(&soft),
        sum(&hard)
    );
}

/// An eye inside the volume a caster's shadow sweeps — a camera under a
/// track, in its shadow — sees the shadow as one outside it does: the
/// volume's far side covers the pixel, its near side behind the eye.
#[test]
fn a_projected_shadow_is_whole_seen_from_inside_its_volume() {
    let height = 400.0;
    let away = height / PENUMBRA_ELEVATION_DEG.to_radians().tan();
    // 8 m inside the shadow's east edge, then 60 m up the ray to the sun:
    // 30 m over the ground, under the plate, in its shadow.
    let target = DVec3::new(A, 0.0, 0.0) + NORTH * away - EAST * 8.0;
    let eye = target + to_sun(180.0, PENUMBRA_ELEVATION_DEG) * 60.0;
    let Some(inside) = seen(plate(height), 1.0, eye) else {
        eprintln!("no GPU adapter: skipped");
        return;
    };
    let lit = seen(plate(height), 1.0, eye + NORTH * 2000.0).expect("an adapter");
    let shares = shares(&inside);
    eprintln!(
        "from inside: {:.4} of the sun taken under the eye, {:.4} 8 m east of the edge; \
         green {:.4} there, {:.4} lit",
        share_at(&shares, 0.0),
        share_at(&shares, 16.0),
        green_at(&inside, 0.0),
        green_at(&lit, 0.0)
    );
    assert!(share_at(&shares, 0.0) == 1.0, "no shadow under the eye");
    assert!(share_at(&shares, 16.0) == 0.0);
    assert!(green_at(&lit, 0.0) > 2.0 * green_at(&inside, 0.0));
}
