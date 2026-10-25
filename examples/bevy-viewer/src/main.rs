// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! tuile's globe in a Bevy application: the application drives the camera,
//! and draws meshes of its own in the same depth as the ground.
//!
//! An orbit camera of the kind a strategy game has: it looks at a point on
//! the ground from a distance, tilted. Left-drag pans the point, right-drag
//! turns and tilts, the wheel zooms from 30 km down to 50 m. Pillars of the
//! application's own stand on the map, their feet hidden by the ground.
//!
//! ```text
//! cargo run --release -p tuile-heightfield-serve        # terminal 1
//! cargo run --release -p tuile-bevy-viewer              # terminal 2
//! ```
//!
//! Flags (every one optional):
//!
//! ```text
//! --terrain-url URL          a layer.json        (default: heightfield-serve's)
//! --imagery-template T       {z}/{x}/{y}         (default: heightfield-serve's)
//! --imagery-scheme S         geographic | webmercator   (default: geographic)
//! --imagery-max-level N      the template's deepest level (default: 15)
//! --lon D --lat D            what the camera looks at    (default: 10, 0)
//! --range M                  how far from it, metres     (default: 12000)
//! --heading D --pitch D      degrees; pitch below the horizon (default: 20, 40)
//! --msaa N                   1, 2, 4 or 8                (default: 4)
//! --hdr                      render in HDR
//! --compression C            off | auto | bc7 | astc | uastc (default: off)
//! --gpu-mib N                the globe's GPU budget
//! --screenshot PATH          once the globe has settled, save the window and quit
//! ```

use bevy::camera::Hdr;
use bevy::input::mouse::{AccumulatedMouseMotion, AccumulatedMouseScroll, MouseScrollUnit};
use bevy::prelude::*;
use bevy::render::view::screenshot::{save_to_disk, Screenshot};
use bevy::render::view::Msaa;
use tuile_bevy::{
    local_frame, GeoAnchor, GlobeCamera, GlobeSources, ImageryCompression, TuileBudget,
    TuileCamera, TuilePlugin, TuileStatus, XyzTemplate,
};
use tuile_core::geo::{enu_frame, geodetic_to_ecef, Geodetic};

const DEFAULT_TERRAIN: &str = "http://127.0.0.1:8092/terrain/layer.json";
const DEFAULT_IMAGERY: &str = "http://127.0.0.1:8092/imagery/{z}/{x}/{y}.png";
const FOVY_DEG: f64 = 35.0;
const MIN_RANGE: f64 = 50.0;
const MAX_RANGE: f64 = 30_000.0;

/// What the command line said.
#[derive(Resource, Debug, Clone)]
struct Options {
    terrain: url::Url,
    imagery: XyzTemplate,
    view: Orbit,
    msaa: Msaa,
    hdr: bool,
    compression: ImageryCompression,
    gpu_mib: Option<u64>,
    screenshot: Option<std::path::PathBuf>,
}

fn number(flag: &str, value: Option<String>) -> Result<f64, String> {
    value
        .ok_or(format!("{flag} needs a value"))?
        .parse()
        .map_err(|e| format!("{flag}: {e}"))
}

fn options() -> Result<Options, String> {
    let mut terrain = DEFAULT_TERRAIN.to_owned();
    let mut template = DEFAULT_IMAGERY.to_owned();
    let mut geographic = true;
    let mut max_level = Some(15);
    let mut view = Orbit::default();
    let mut msaa = Msaa::Sample4;
    let mut hdr = false;
    let mut compression = ImageryCompression::Off;
    let mut gpu_mib = None;
    let mut screenshot = None;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--terrain-url" => terrain = args.next().ok_or("--terrain-url needs a URL")?,
            "--imagery-template" => {
                template = args.next().ok_or("--imagery-template needs a template")?;
            }
            "--imagery-scheme" => {
                geographic = match args.next().as_deref() {
                    Some("geographic") => true,
                    Some("webmercator") => false,
                    other => return Err(format!("--imagery-scheme: {other:?}")),
                };
            }
            "--imagery-max-level" => max_level = Some(number(&flag, args.next())? as u32),
            "--lon" => view.lon = number(&flag, args.next())?,
            "--lat" => view.lat = number(&flag, args.next())?,
            "--range" => view.range = number(&flag, args.next())?.clamp(MIN_RANGE, MAX_RANGE),
            "--heading" => view.heading = number(&flag, args.next())?,
            "--pitch" => view.pitch = number(&flag, args.next())?.clamp(5.0, 90.0),
            "--msaa" => {
                msaa = match number(&flag, args.next())? as u32 {
                    1 => Msaa::Off,
                    2 => Msaa::Sample2,
                    4 => Msaa::Sample4,
                    8 => Msaa::Sample8,
                    n => return Err(format!("--msaa {n}: 1, 2, 4 or 8")),
                };
            }
            "--hdr" => hdr = true,
            "--compression" => {
                compression = tuile_bevy::compression_named(
                    &args.next().ok_or("--compression needs a value")?,
                )?;
            }
            "--gpu-mib" => gpu_mib = Some(number(&flag, args.next())? as u64),
            "--screenshot" => {
                screenshot = Some(args.next().ok_or("--screenshot needs a path")?.into());
            }
            "--help" | "-h" => {
                return Err(
                    "the flags are listed at the head of examples/bevy-viewer/src/main.rs".into(),
                )
            }
            other => return Err(format!("unknown flag {other:?}")),
        }
    }
    let terrain = url::Url::parse(&terrain).map_err(|e| format!("--terrain-url: {e}"))?;
    let mut imagery = if geographic {
        XyzTemplate::geographic(template)
    } else {
        XyzTemplate::web_mercator(template)
    };
    imagery.max_level = max_level;
    imagery.check()?;
    Ok(Options {
        terrain,
        imagery,
        view,
        msaa,
        hdr,
        compression,
        gpu_mib,
        screenshot,
    })
}

fn main() {
    let options = match options() {
        Ok(options) => options,
        Err(why) => {
            eprintln!("tuile-bevy-viewer: {why}");
            std::process::exit(2);
        }
    };
    let mut budget = TuileBudget::default();
    if let Some(mib) = options.gpu_mib {
        budget = budget.with_gpu_bytes(mib * 1024 * 1024);
    }
    let globe = TuilePlugin::new(GlobeSources::addresses(
        options.terrain.clone(),
        options.imagery.clone(),
    ))
    .with_budget(budget)
    .with_compression(options.compression);
    App::new()
        .add_plugins(DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "tuile-bevy-viewer".into(),
                resolution: (1280, 720).into(),
                ..default()
            }),
            ..default()
        }))
        .add_plugins(globe)
        .insert_resource(options.view)
        .insert_resource(options)
        .add_systems(Startup, setup)
        .add_systems(
            Update,
            (steer, aim, report, screenshot_when_settled).chain(),
        )
        .run();
}

/// The orbit camera's state: what it looks at, from how far, from where.
#[derive(Resource, Debug, Clone, Copy)]
struct Orbit {
    lon: f64,
    lat: f64,
    range: f64,
    heading: f64,
    pitch: f64,
}

impl Default for Orbit {
    fn default() -> Self {
        Self {
            lon: 10.0,
            lat: 0.0,
            range: 12_000.0,
            heading: 20.0,
            pitch: 40.0,
        }
    }
}

impl Orbit {
    /// The eye, looking at the point from `range` metres, `heading` clockwise
    /// from north and `pitch` below the horizon.
    fn pose(&self) -> GlobeCamera {
        let target = Geodetic {
            lon: self.lon.to_radians(),
            lat: self.lat.to_radians(),
            height: 0.0,
        };
        let at = geodetic_to_ecef(target);
        let enu = enu_frame(target);
        let (east, north, up) = (enu.col(0), enu.col(1), enu.col(2));
        let (h, p) = (self.heading.to_radians(), self.pitch.to_radians());
        let horizontal = north * h.cos() + east * h.sin();
        let direction = horizontal * p.cos() - up * p.sin();
        GlobeCamera::look_at(at - direction * self.range, at, up, FOVY_DEG.to_radians())
    }
}

fn setup(
    mut commands: Commands,
    options: Res<Options>,
    orbit: Res<Orbit>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let mut camera = commands.spawn((
        Camera3d::default(),
        Camera {
            clear_color: ClearColorConfig::Custom(Color::BLACK),
            ..default()
        },
        options.msaa,
        Projection::Perspective(PerspectiveProjection {
            far: 1.0e7,
            ..default()
        }),
        TuileCamera { pose: orbit.pose() },
    ));
    if options.hdr {
        camera.insert(Hdr);
    }
    // The application's own light, for its own meshes: the ground is lit by
    // the globe's.
    commands.spawn((
        DirectionalLight {
            illuminance: 8_000.0,
            ..default()
        },
        Transform::from_rotation(Quat::from_rotation_x(-1.0)),
    ));
    // Pillars of the application's own, 1.2 km tall from 100 m under the
    // ellipsoid: the map's relief stands 60 to 930 m, so their feet are
    // hidden by the ground they stand in.
    let pillar = meshes.add(Cuboid::new(80.0, 1200.0, 80.0));
    let colours = [
        Color::srgb(0.85, 0.25, 0.2),
        Color::srgb(0.95, 0.75, 0.2),
        Color::srgb(0.25, 0.45, 0.9),
    ];
    for i in -2i32..=2 {
        for j in -2..=2 {
            let at = GeoAnchor::geodetic(
                orbit.lon + f64::from(i) * 0.012,
                orbit.lat + f64::from(j) * 0.012,
                500.0,
            );
            let colour = colours[(i + j).rem_euclid(3) as usize];
            commands.spawn((
                Mesh3d(pillar.clone()),
                MeshMaterial3d(materials.add(StandardMaterial {
                    base_color: colour,
                    ..default()
                })),
                at,
                Transform::from_rotation(local_frame(at.0)),
            ));
        }
    }
}

/// The orbit controller: drags and the wheel move the orbit.
fn steer(
    buttons: Res<ButtonInput<MouseButton>>,
    motion: Res<AccumulatedMouseMotion>,
    scroll: Res<AccumulatedMouseScroll>,
    mut orbit: ResMut<Orbit>,
) {
    let delta = motion.delta;
    if buttons.pressed(MouseButton::Left) && delta != Vec2::ZERO {
        // Pan: the point moves under the cursor by about the ground a pixel
        // covers at this range.
        let metres_per_pixel = orbit.range * 0.0015;
        let h = orbit.heading.to_radians();
        let (right, forward) = (-f64::from(delta.x), f64::from(delta.y));
        let east = (right * h.cos() + forward * h.sin()) * metres_per_pixel;
        let north = (-right * h.sin() + forward * h.cos()) * metres_per_pixel;
        orbit.lon += east / (111_319.5 * orbit.lat.to_radians().cos().max(0.01));
        orbit.lat = (orbit.lat + north / 110_574.3).clamp(-85.0, 85.0);
    }
    if buttons.pressed(MouseButton::Right) && delta != Vec2::ZERO {
        orbit.heading = (orbit.heading + f64::from(delta.x) * 0.3).rem_euclid(360.0);
        orbit.pitch = (orbit.pitch + f64::from(delta.y) * 0.2).clamp(5.0, 90.0);
    }
    let lines = match scroll.unit {
        MouseScrollUnit::Line => f64::from(scroll.delta.y),
        MouseScrollUnit::Pixel => f64::from(scroll.delta.y) / 40.0,
    };
    if lines != 0.0 {
        orbit.range = (orbit.range * (1.0 - 0.12 * lines)).clamp(MIN_RANGE, MAX_RANGE);
    }
}

/// The orbit, handed to the globe's camera.
fn aim(orbit: Res<Orbit>, mut cameras: Query<&mut TuileCamera>) {
    if !orbit.is_changed() {
        return;
    }
    for mut camera in &mut cameras {
        camera.pose = orbit.pose();
    }
}

/// A line when the globe's state changes.
fn report(status: Res<TuileStatus>, mut last: Local<Option<(bool, usize)>>) {
    let now = (status.is_settled(), status.lost);
    if *last != Some(now) {
        info!(
            "globe: {:?}, {} exact, {} coarser, {} lost, {:.0} MiB, settled {}",
            status.session,
            status.exact,
            status.coarser,
            status.lost,
            status.gpu_bytes as f64 / 1_048_576.0,
            status.is_settled()
        );
        *last = Some(now);
    }
}

/// With `--screenshot`, the window once the globe has settled, then quit.
fn screenshot_when_settled(
    mut commands: Commands,
    options: Res<Options>,
    status: Res<TuileStatus>,
    mut state: Local<(u32, u32)>,
    mut exit: MessageWriter<AppExit>,
) {
    let Some(path) = &options.screenshot else {
        return;
    };
    let (settled, after) = &mut *state;
    if *after > 0 {
        *after += 1;
        if *after > 30 {
            exit.write(AppExit::Success);
        }
        return;
    }
    *settled = if status.is_settled() { *settled + 1 } else { 0 };
    if *settled > 30 {
        commands
            .spawn(Screenshot::primary_window())
            .observe(save_to_disk(path.clone()));
        *after = 1;
    }
}
