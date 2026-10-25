// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! GPU in the loop: the physical atmosphere of a stated instant, through the
//! viewer's own frame (`draw_to_texture`, `paint`).
//!
//! The sun and the Moon are one direction each at an instant, and every
//! ground point and every line of sight is lit by them where it is: these
//! scenes hold that to pictures — noon, sunset and night over the Pyrenees,
//! France lit while Australia is dark in one view from far away, a midnight
//! sun over Svalbard, the camera's own sky by day and by night — with the
//! one rule that is never relaxed: no black ground, and no black above a
//! low horizon.
//!
//! The ground is plain terrain tiles of one grey (`fill_content`), so what a
//! picture shows is light and air and nothing of any imagery. Every picture
//! is written to `target/test-renders/sky-*.png`.

use glam::{DMat4, DVec3, DVec4};
use tuile_core::content::DecodedTileContent;
use tuile_core::geo::{ecef_to_geodetic, enu_frame, geodetic_to_ecef, Geodetic};
use tuile_terrain::{fill_content, GeographicTilingScheme, TileCoord};
use tuile_wgpu::{
    draw_to_texture, prepare, view_proj_from_world, FrameOnTexture, FrameTargets, GpuContext,
    PreparedTile, Readback, SkyFrame, TileRenderer, ViewUniform,
};

const WIDTH: u32 = 384;
const HEIGHT: u32 = 256;
const FOV_Y: f64 = 50.0 * std::f64::consts::PI / 180.0;

/// The ground's grey: about what decoded satellite imagery of fields and
/// woods is, linear.
const GROUND: f32 = 0.08;

/// The Pic du Midi de Bigorre's summit.
const PIC_LON: f64 = 0.1418;
const PIC_LAT: f64 = 42.9365;

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

/// Unix seconds of a UTC date and time.
fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> f64 {
    // Days from the civil date (H. Hinnant's algorithm).
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe as u32 * 365 + yoe as u32 / 4 - yoe as u32 / 100 + doy;
    let days = era as i64 * 146_097 + i64::from(doe) - 719_468;
    (days * 86_400 + i64::from(h) * 3600 + i64::from(mi) * 60) as f64
}

fn grey(mut content: DecodedTileContent) -> DecodedTileContent {
    for mesh in &mut content.meshes {
        mesh.material.base_color_factor = [GROUND, GROUND, GROUND, 1.0];
        mesh.material.base_color_texture = None;
    }
    content.textures.clear();
    content
}

/// Every tile of `level` over the whole Earth: the globe a view from far
/// away sees.
fn globe(gpu: &GpuContext, origin: DVec3, level: u32) -> Vec<PreparedTile> {
    let scheme = GeographicTilingScheme::default();
    let mut out = Vec::new();
    for y in 0..(1u32 << level) {
        for x in 0..(2u32 << level) {
            let rect = scheme.tile_rect(TileCoord::new(level, u64::from(x), u64::from(y)));
            out.push(prepare(
                gpu,
                &grey(fill_content(&rect, [Some(0.0); 4], 0.0)),
                origin,
            ));
        }
    }
    out
}

/// The tiles of `level` within `reach` tiles of the place, at `height`.
fn ground_around(
    gpu: &GpuContext,
    origin: DVec3,
    lon_deg: f64,
    lat_deg: f64,
    level: u32,
    reach: i64,
    height: f64,
) -> Vec<PreparedTile> {
    let scheme = GeographicTilingScheme::default();
    let columns = 2i64 << level;
    let rows = 1i64 << level;
    let x0 = ((lon_deg + 180.0) / 360.0 * columns as f64).floor() as i64;
    let y0 = ((90.0 - lat_deg) / 180.0 * rows as f64).floor() as i64;
    let mut out = Vec::new();
    for dy in -reach..=reach {
        for dx in -reach..=reach {
            let (x, y) = ((x0 + dx).rem_euclid(columns), y0 + dy);
            if !(0..rows).contains(&y) {
                continue;
            }
            let rect = scheme.tile_rect(TileCoord::new(level, x as u64, y as u64));
            out.push(prepare(
                gpu,
                &grey(fill_content(&rect, [Some(height); 4], height)),
                origin,
            ));
        }
    }
    out
}

/// An eye: where, and which way — a heading from north and a pitch from
/// the horizon, degrees.
#[derive(Clone, Copy)]
struct Eye {
    at: DVec3,
    forward: DVec3,
    up: DVec3,
    /// The vertical field of view, radians.
    fov_y: f64,
}

fn eye_at(lon: f64, lat: f64, height: f64, heading: f64, pitch: f64) -> Eye {
    let g = Geodetic {
        lon: lon.to_radians(),
        lat: lat.to_radians(),
        height,
    };
    let enu = enu_frame(g);
    let (h, p) = (heading.to_radians(), pitch.to_radians());
    let level = enu.x_axis * h.sin() + enu.y_axis * h.cos();
    let forward = level * p.cos() + enu.z_axis * p.sin();
    let up = (enu.z_axis - forward * enu.z_axis.dot(forward)).normalize();
    Eye {
        at: geodetic_to_ecef(g),
        forward,
        up,
        fov_y: FOV_Y,
    }
}

/// An eye `distance` metres from the Earth's centre over a place, looking
/// at the centre.
fn eye_far(lon: f64, lat: f64, distance: f64) -> Eye {
    let over = geodetic_to_ecef(Geodetic {
        lon: lon.to_radians(),
        lat: lat.to_radians(),
        height: 0.0,
    })
    .normalize();
    // The Earth filling most of the picture's height.
    let fov_y = 2.4 * (tuile_core::geo::WGS84_A / distance).atan();
    Eye {
        at: over * distance,
        forward: -over,
        up: (DVec3::Z - over * over.z).normalize(),
        fov_y,
    }
}

fn clip_from_world(eye: &Eye) -> DMat4 {
    let height = ecef_to_geodetic(eye.at).height.max(1.0);
    let near = (height * 0.25).clamp(0.5, 1.0e6);
    let far = eye.at.length() + 2.0e7;
    glam::dcamera::rh::proj::directx::perspective(
        eye.fov_y,
        f64::from(WIDTH) / f64::from(HEIGHT),
        near,
        far,
    ) * glam::dcamera::rh::view::look_at_mat4(eye.at, eye.at + eye.forward, eye.up)
}

/// Where the direction `d` from the eye falls in the picture, pixels, if
/// in front of it.
fn on_picture(eye: &Eye, d: DVec3) -> Option<(f64, f64)> {
    let c = clip_from_world(eye) * (eye.at + d * 1.0e5).extend(1.0);
    if c.w <= 0.0 {
        return None;
    }
    let (x, y) = (c.x / c.w, c.y / c.w);
    Some((
        (x * 0.5 + 0.5) * f64::from(WIDTH),
        (0.5 - y * 0.5) * f64::from(HEIGHT),
    ))
}

/// Where the ECEF point `p` falls in the picture, pixels.
fn point_on_picture(eye: &Eye, p: DVec3) -> (u32, u32) {
    let c: DVec4 = clip_from_world(eye) * p.extend(1.0);
    let (x, y) = (c.x / c.w, c.y / c.w);
    (
        ((x * 0.5 + 0.5) * f64::from(WIDTH)) as u32,
        ((0.5 - y * 0.5) * f64::from(HEIGHT)) as u32,
    )
}

struct Picture {
    rgba: image::RgbaImage,
    uniform: tuile_atmosphere::gpu::AtmosphereUniform,
}

impl Picture {
    fn px(&self, x: u32, y: u32) -> [u8; 3] {
        let p = self.rgba.get_pixel(x.min(WIDTH - 1), y.min(HEIGHT - 1));
        [p[0], p[1], p[2]]
    }

    fn luma(&self, x: u32, y: u32) -> f64 {
        let [r, g, b] = self.px(x, y).map(f64::from);
        0.2126 * r + 0.7152 * g + 0.0722 * b
    }

    /// The mean luma over a box of `half` pixels about a point.
    fn luma_about(&self, x: u32, y: u32, half: u32) -> f64 {
        let mut sum = 0.0;
        let mut n = 0.0;
        for j in y.saturating_sub(half)..=(y + half).min(HEIGHT - 1) {
            for i in x.saturating_sub(half)..=(x + half).min(WIDTH - 1) {
                sum += self.luma(i, j);
                n += 1.0;
            }
        }
        sum / n
    }

    /// Pixels no channel of which reaches `floor`: black, to the eye.
    fn black(&self, rows: std::ops::Range<u32>, floor: u8) -> usize {
        let mut n = 0;
        for y in rows {
            for x in 0..WIDTH {
                if self.px(x, y).iter().all(|&c| c < floor) {
                    n += 1;
                }
            }
        }
        n
    }
}

/// One picture of `tiles` from `eye` at `instant`, through the viewer's
/// frame, with the physical atmosphere on — a renderer of its own, so that
/// its light is this instant's and no other's.
fn shoot(
    gpu: &GpuContext,
    tiles: &[PreparedTile],
    origin: DVec3,
    eye: Eye,
    instant: f64,
    name: &str,
) -> Picture {
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let mut renderer = TileRenderer::new(gpu, format);
    let sun = tuile_atmosphere::Sun::at_unix_seconds(instant);
    let travel = sun.light_travel_direction().as_vec3();
    renderer.set_view(
        &gpu.queue,
        &ViewUniform {
            view_proj: view_proj_from_world(clip_from_world(&eye), origin),
            sun_dir: [travel.x, travel.y, travel.z, 0.0],
            params: [0.25, 0.0, 0.0, 0.0],
            atmosphere: tuile_atmosphere::AerialPerspective::new(eye.at, origin, &sun, 0.0),
        },
    );
    renderer.set_sky(
        gpu,
        Some(&SkyFrame {
            instant,
            eye: eye.at,
            render_origin: origin,
            forward: eye.forward,
            up: eye.up,
            fov_y: eye.fov_y,
            aspect: f64::from(WIDTH) / f64::from(HEIGHT),
            height_px: HEIGHT,
            ground_height: 0.0,
        }),
    );
    let targets = FrameTargets::new(gpu, format, (WIDTH, HEIGHT));
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("sky test"),
        size: wgpu::Extent3d {
            width: WIDTH,
            height: HEIGHT,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let mut readback = Readback::default();
    draw_to_texture(
        FrameOnTexture {
            gpu,
            texture: &texture,
            targets: &targets,
            clear: wgpu::Color::BLACK,
            capture: Some(&mut readback),
        },
        |pass| {
            renderer.paint(
                pass,
                std::iter::empty(),
                tiles.iter(),
                std::iter::empty(),
                None,
                false,
            )
        },
        |_, _| {},
    );
    let pixels = readback.take(gpu, (WIDTH, HEIGHT)).expect("read back");
    let rgba = image::RgbaImage::from_raw(WIDTH, HEIGHT, pixels).expect("image");
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-renders");
    std::fs::create_dir_all(&dir).expect("renders dir");
    let path = dir.join(format!("sky-{name}.png"));
    rgba.save(&path).expect("save");
    eprintln!("{}", path.display());
    Picture {
        rgba,
        uniform: renderer.sky_uniform().expect("the sky is on"),
    }
}

/// The Pic du Midi, 2 877 m, looking west over the Pyrenees, a few degrees
/// down: the horizon a third of the way down the picture.
fn pic_du_midi(gpu: &GpuContext) -> (Vec<PreparedTile>, DVec3, Eye) {
    let eye = eye_at(PIC_LON, PIC_LAT, 2_877.0, 270.0, -4.0);
    let origin = eye.at;
    let mut tiles = globe(gpu, origin, 2);
    tiles.extend(ground_around(gpu, origin, PIC_LON, PIC_LAT, 9, 3, 600.0));
    (tiles, origin, eye)
}

/// The sun's direction the frame was lit by is the ephemeris's at the
/// instant, to the step the light is worked out again at.
fn assert_sun_is_the_ephemeris(picture: &Picture, instant: f64) {
    let s = picture.uniform.sun;
    let drawn = DVec3::new(f64::from(s[0]), f64::from(s[1]), f64::from(s[2]));
    let ephemeris = tuile_atmosphere::Sun::at_unix_seconds(instant).direction_ecef;
    let off = drawn.angle_between(ephemeris);
    assert!(
        off <= tuile_wgpu::LIGHT_SUN_STEP,
        "the sun drawn is {:.3}° from the ephemeris's",
        off.to_degrees()
    );
}

#[test]
fn noon_sunset_and_night_over_the_pyrenees_keep_the_ground_and_the_sky() {
    let Some(gpu) = gpu() else { return };
    let (tiles, origin, eye) = pic_du_midi(&gpu);
    // Midsummer 2026: noon, the sun on the horizon, and an hour and a half
    // after it.
    let noon = utc(2026, 6, 21, 11, 50);
    let sunset = sunset_at_the_pic(utc(2026, 6, 21, 12, 0));
    let night = sunset + 5_400.0;
    let ground_row = HEIGHT * 3 / 4;
    let sky_row = HEIGHT / 8;
    let mut lumas = Vec::new();
    for (name, instant) in [("noon", noon), ("sunset", sunset), ("night", night)] {
        let p = shoot(&gpu, &tiles, origin, eye, instant, &format!("pic-{name}"));
        assert_sun_is_the_ephemeris(&p, instant);
        let ground = p.luma_about(WIDTH / 2, ground_row, 20);
        let sky = p.luma_about(WIDTH / 2, sky_row, 20);
        eprintln!("{name}: ground {ground:.1}, sky {sky:.1}");
        assert_eq!(
            p.black(HEIGHT / 2..HEIGHT, 3),
            0,
            "{name}: black ground under the horizon"
        );
        assert_eq!(
            p.black(0..HEIGHT / 4, 2),
            0,
            "{name}: black sky over the horizon"
        );
        lumas.push((ground, sky, p));
    }
    let (noon_ground, noon_sky, noon_p) = &lumas[0];
    let (_, sunset_sky, _) = &lumas[1];
    let (night_ground, night_sky, _) = &lumas[2];
    // Noon's sky is blue; the night's darker than noon's, ground and sky.
    let [r, _, b] = noon_p.px(WIDTH / 2, sky_row);
    assert!(b > r, "noon's sky is blue: {r} red, {b} blue");
    assert!(
        night_ground < noon_ground,
        "night ground {night_ground} under noon's {noon_ground}"
    );
    assert!(night_sky < sunset_sky && night_sky < noon_sky);
}

/// The sun on the horizon, seen from the Pic du Midi looking at it: its
/// disc is where the ephemeris puts it, the brightest thing in the
/// picture, and the sky about it warm.
#[test]
fn the_setting_sun_is_drawn_where_the_ephemeris_puts_it() {
    let Some(gpu) = gpu() else { return };
    let (tiles, origin, _) = pic_du_midi(&gpu);
    // A quarter of an hour before sunset: the disc two degrees up.
    let instant = sunset_at_the_pic(utc(2026, 6, 21, 12, 0)) - 900.0;
    let at = Geodetic {
        lon: PIC_LON.to_radians(),
        lat: PIC_LAT.to_radians(),
        height: 2_877.0,
    };
    let seen = tuile_atmosphere::Sun::seen_from(instant, at).expect("the sun");
    let eye = eye_at(
        PIC_LON,
        PIC_LAT,
        2_877.0,
        seen.azimuth_deg,
        seen.elevation_deg - 2.0,
    );
    let p = shoot(&gpu, &tiles, origin, eye, instant, "pic-sun-low");
    let (x, y) = on_picture(&eye, seen.direction_ecef).expect("the sun is in front");
    let (x, y) = (x as u32, y as u32);
    let [r, g, b] = p.px(x, y);
    eprintln!(
        "the sun at ({x}, {y}): {r} {g} {b}, elevation {:.2}°",
        seen.elevation_deg
    );
    assert!(
        r >= 250,
        "the sun's disc is bright where the ephemeris puts it: {r} {g} {b}"
    );
    assert!(r >= b, "a low sun is not blue: {r} {g} {b}");
    // Ten degrees to its side, the sky is far dimmer than the disc.
    let side = p.luma(x.saturating_sub(WIDTH / 5), y);
    assert!(
        side < p.luma(x, y) * 0.9,
        "the disc stands out of the sky: {side}"
    );
}

/// A sunset from the Pic du Midi on the given day, from the ephemeris: the
/// sun's centre at 0.833° under the horizon.
fn sunset_at_the_pic(after: f64) -> f64 {
    tuile_atmosphere::Sun::next_sunset(
        after,
        Geodetic {
            lon: PIC_LON.to_radians(),
            lat: PIC_LAT.to_radians(),
            height: 0.0,
        },
    )
    .expect("the sun sets over the Pyrenees in June")
}

/// One instant, one view from far away: France in the afternoon, Australia
/// at night — each lit by the sun where it is, not by the camera's — and
/// the night side never black.
#[test]
fn france_is_lit_while_australia_is_dark_in_one_frame() {
    let Some(gpu) = gpu() else { return };
    let instant = utc(2026, 6, 21, 14, 0);
    // Over the Arabian Sea, from beyond the Moon's distance: a whole
    // hemisphere at once, France on one side and Perth on the other.
    let eye = eye_far(60.0, 8.0, 6.0e8);
    let origin = DVec3::ZERO;
    let tiles = globe(&gpu, origin, 3);
    // Narrow enough to hold the Earth: the field is the picture's.
    let p = shoot(&gpu, &tiles, origin, eye, instant, "france-australia");
    let place = |lon: f64, lat: f64| {
        geodetic_to_ecef(Geodetic {
            lon: lon.to_radians(),
            lat: lat.to_radians(),
            height: 0.0,
        })
    };
    let france = point_on_picture(&eye, place(2.35, 46.0));
    let perth = point_on_picture(&eye, place(115.9, -31.9));
    let lit = p.luma(france.0, france.1);
    let dark = p.luma(perth.0, perth.1);
    eprintln!("France {france:?}: {lit:.1}; Perth {perth:?}: {dark:.1}");
    assert!(
        lit > 3.0 * dark.max(1.0),
        "France lit ({lit}) and Perth dark ({dark})"
    );
    assert!(
        p.px(perth.0, perth.1).iter().any(|&c| c >= 2),
        "the night side is dark, never black"
    );
}

#[test]
fn svalbard_has_the_midnight_sun_in_june_and_the_polar_night_in_december() {
    let Some(gpu) = gpu() else { return };
    let (lon, lat) = (15.6, 78.2);
    let eye = eye_at(lon, lat, 2_000.0, 0.0, -8.0);
    let origin = eye.at;
    let mut tiles = globe(&gpu, origin, 2);
    tiles.extend(ground_around(&gpu, origin, lon, lat, 8, 2, 0.0));
    // Local midnight: the sun at its lowest, due north.
    let june = utc(2026, 6, 21, 23, 0);
    let december = utc(2026, 12, 21, 23, 0);
    let at = Geodetic {
        lon: lon.to_radians(),
        lat: lat.to_radians(),
        height: 0.0,
    };
    let up_in_june = tuile_atmosphere::Sun::seen_from(june, at)
        .unwrap()
        .elevation_deg;
    assert!(up_in_june > 5.0, "the midnight sun: {up_in_june}°");
    let summer = shoot(&gpu, &tiles, origin, eye, june, "svalbard-june-midnight");
    let winter = shoot(
        &gpu,
        &tiles,
        origin,
        eye,
        december,
        "svalbard-december-midnight",
    );
    let row = HEIGHT * 3 / 4;
    let (s, w) = (
        summer.luma_about(WIDTH / 2, row, 20),
        winter.luma_about(WIDTH / 2, row, 20),
    );
    eprintln!("Svalbard at midnight: June {s:.1}, December {w:.1}");
    // The camera exposes for the night it is in — a polar night is shown
    // dark, not black — so the two differ by less than the light does.
    assert!(
        s > 1.4 * w,
        "lit at midnight in June ({s}), dark in December ({w})"
    );
    let sun_up = |p: &Picture| {
        let s = p.uniform.sun;
        let up = p.uniform.up;
        s[0] * up[0] + s[1] * up[1] + s[2] * up[2]
    };
    assert!(sun_up(&summer) > 0.0 && sun_up(&winter) < 0.0);
    assert_eq!(
        winter.black(HEIGHT / 2..HEIGHT, 2),
        0,
        "the polar night keeps its ground"
    );
}

/// The camera's own sky follows where it is: at one instant, a blue sky
/// over France and a dark one over Australia.
#[test]
fn the_cameras_sky_is_its_own_places() {
    let Some(gpu) = gpu() else { return };
    let instant = utc(2026, 6, 21, 12, 0);
    let mut seen = Vec::new();
    for (name, lon, lat) in [("france", 2.35, 46.0), ("australia", 133.0, -25.0)] {
        let eye = eye_at(lon, lat, 1_500.0, 90.0, 5.0);
        let origin = eye.at;
        let mut tiles = globe(&gpu, origin, 2);
        tiles.extend(ground_around(&gpu, origin, lon, lat, 9, 2, 0.0));
        let p = shoot(
            &gpu,
            &tiles,
            origin,
            eye,
            instant,
            &format!("own-sky-{name}"),
        );
        assert_eq!(p.black(0..HEIGHT, 2), 0, "{name}: black in the picture");
        seen.push((
            p.luma_about(WIDTH / 2, HEIGHT / 8, 20),
            p.px(WIDTH / 2, HEIGHT / 8),
        ));
    }
    let (day, [r, _, b]) = seen[0];
    let (night, _) = seen[1];
    eprintln!("the sky over France {day:.1}, over Australia {night:.1}");
    assert!(b > r, "a blue sky by day");
    assert!(day > 4.0 * night, "day {day}, night {night}");
}

/// From 106 m, looking at the horizon by day: sky over it, not black.
#[test]
fn a_low_view_shows_sky_over_the_horizon() {
    let Some(gpu) = gpu() else { return };
    let (lon, lat) = (1.44, 43.6);
    let eye = eye_at(lon, lat, 106.0, 200.0, 0.0);
    let origin = eye.at;
    let mut tiles = globe(&gpu, origin, 2);
    tiles.extend(ground_around(&gpu, origin, lon, lat, 11, 3, 0.0));
    let p = shoot(
        &gpu,
        &tiles,
        origin,
        eye,
        utc(2026, 6, 21, 10, 0),
        "low-106m",
    );
    assert_eq!(p.black(0..HEIGHT / 2, 8), 0, "black over the horizon");
    let [r, _, b] = p.px(WIDTH / 2, HEIGHT / 10);
    assert!(b > r, "the sky is blue: {r} red, {b} blue");
}

/// However the instant runs — a real second a frame or a thousand times
/// faster — the light is worked out again only as the sun moves, and never
/// more often than its interval.
#[test]
fn the_light_is_worked_out_as_the_sun_moves_and_no_more() {
    let Some(gpu) = gpu() else { return };
    let eye = eye_at(PIC_LON, PIC_LAT, 2_877.0, 270.0, -4.0);
    let origin = eye.at;
    let frame = |instant: f64| SkyFrame {
        instant,
        eye: eye.at,
        render_origin: origin,
        forward: eye.forward,
        up: eye.up,
        fov_y: FOV_Y,
        aspect: 1.5,
        height_px: HEIGHT,
        ground_height: 0.0,
    };
    let start = utc(2026, 6, 21, 18, 0);
    // Real time at sixty frames a second, for a simulated ten seconds: the
    // sun moves 0.04°, under the step.
    let mut renderer = TileRenderer::new(&gpu, wgpu::TextureFormat::Rgba8UnormSrgb);
    for f in 0..600 {
        renderer.set_sky(&gpu, Some(&frame(start + f64::from(f) / 60.0)));
    }
    let (lights, views) = renderer.sky_updates();
    assert_eq!((lights, views), (1, 1), "real time for ten seconds");
    // A thousand times faster, frames as fast as they come: the sun moves
    // degrees, and the light follows at most once an interval.
    let mut renderer = TileRenderer::new(&gpu, wgpu::TextureFormat::Rgba8UnormSrgb);
    let began = web_time::Instant::now();
    let mut f = 0u32;
    while began.elapsed().as_secs_f64() < 1.2 {
        renderer.set_sky(&gpu, Some(&frame(start + f64::from(f) * 1000.0 / 60.0)));
        f += 1;
    }
    let elapsed = began.elapsed().as_secs_f64();
    let (lights, _) = renderer.sky_updates();
    let allowed = 2 + (elapsed / tuile_wgpu::LIGHT_INTERVAL.as_secs_f64()) as u64;
    eprintln!("{lights} lights in {elapsed:.2} s at a thousand times");
    assert!(lights >= 2, "the light follows a moving sun");
    assert!(
        lights <= allowed,
        "{lights} lights in {elapsed:.2} s: over budget"
    );
}

/// A host setting its clock forward a quarter of an hour is answered at
/// once, not an interval later.
#[test]
fn a_jump_of_the_clock_is_lit_at_once() {
    let Some(gpu) = gpu() else { return };
    let eye = eye_at(PIC_LON, PIC_LAT, 2_877.0, 270.0, -4.0);
    let frame = |instant: f64| SkyFrame {
        instant,
        eye: eye.at,
        render_origin: eye.at,
        forward: eye.forward,
        up: eye.up,
        fov_y: FOV_Y,
        aspect: 1.5,
        height_px: HEIGHT,
        ground_height: 0.0,
    };
    let start = utc(2026, 6, 21, 18, 0);
    let mut renderer = TileRenderer::new(&gpu, wgpu::TextureFormat::Rgba8UnormSrgb);
    renderer.set_sky(&gpu, Some(&frame(start)));
    renderer.set_sky(&gpu, Some(&frame(start + 900.0)));
    assert_eq!(renderer.sky_updates().0, 2, "the jump was lit at once");
    let s = renderer.sky_uniform().expect("on").sun;
    let drawn = DVec3::new(f64::from(s[0]), f64::from(s[1]), f64::from(s[2]));
    let ephemeris = tuile_atmosphere::Sun::at_unix_seconds(start + 900.0).direction_ecef;
    assert!(drawn.angle_between(ephemeris) < 1e-4);
}
