// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! GPU in the loop: the globe in a Bevy application with no window, drawn
//! into an image and read back.
//!
//! What must hold in a host's frame that no frame of tuile's own tests:
//!
//! - **no black ground**: the clear colour is black here, so any pixel the
//!   ground leaves uncovered is black — from the first frame, before the
//!   session is up, and once it has settled;
//! - **one depth**: a cube of the host's above the ground is seen, a cube
//!   buried under it is not — the host's meshes, in Bevy's own pipelines,
//!   depth-tested against the ground the globe's pass wrote;
//! - in every target a Bevy camera renders into: with and without MSAA, in
//!   8-bit sRGB and in HDR.
//!
//! What it does not cover: a host that draws *before* the globe (a depth
//! prepass), WebGL2, and a window's surface.

mod support;

use bevy::prelude::*;
use bevy::render::view::Msaa;
use support::{Headless, Target, LAT, LON};
use tuile_bevy::{local_frame, GeoAnchor, SessionState, TuileCamera};

const ALTITUDE: f64 = 3_000.0;
const FOVY_DEG: f64 = 35.0;

/// Straight down over the map, north up the frame.
fn camera() -> TuileCamera {
    TuileCamera::looking(LON, LAT, ALTITUDE, 0.0, 90.0, FOVY_DEG)
}

/// Where a point `east`, `north` metres from the camera's nadir and `height`
/// metres up lands in a `size`-pixel frame.
fn pixel(east: f64, north: f64, height: f64, size: u32) -> (u32, u32) {
    let half = f64::from(size) / 2.0;
    let reach = (ALTITUDE - height) * (FOVY_DEG.to_radians() / 2.0).tan();
    (
        (half + east / reach * half) as u32,
        (half - north / reach * half) as u32,
    )
}

const METRES_PER_DEG_LON: f64 = 111_319.5;
const METRES_PER_DEG_LAT: f64 = 110_574.3;

/// A cube of the host's, `side` metres, stood up at `east`/`north` of the
/// nadir with its centre `height` metres up, in an unlit colour.
fn cube(app: &mut Headless, east: f64, north: f64, height: f64, side: f32, colour: Color) {
    let at = GeoAnchor::geodetic(
        LON + east / METRES_PER_DEG_LON,
        LAT + north / METRES_PER_DEG_LAT,
        height,
    );
    let world = app.world();
    let mesh = world
        .resource_mut::<Assets<Mesh>>()
        .add(Cuboid::new(side, side, side));
    let material = world
        .resource_mut::<Assets<StandardMaterial>>()
        .add(StandardMaterial {
            base_color: colour,
            unlit: true,
            ..default()
        });
    world.spawn((
        Mesh3d(mesh),
        MeshMaterial3d(material),
        at,
        Transform::from_rotation(local_frame(at.0)),
    ));
}

fn is_red(p: [u8; 3]) -> bool {
    p[0] > 200 && p[1] < 60 && p[2] < 60
}

fn is_ground(p: [u8; 3]) -> bool {
    p[1] > p[0] && p[1] > p[2] && p[1] > 60
}

fn targets() -> [Target; 4] {
    let t = |msaa, hdr| Target {
        msaa,
        hdr,
        size: 256,
    };
    [
        t(Msaa::Sample4, false),
        t(Msaa::Off, false),
        t(Msaa::Sample4, true),
        t(Msaa::Off, true),
    ]
}

fn out_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
}

/// The ground is there from the first frame: the backdrop covers the planet
/// before the session has answered anything.
#[test]
fn the_first_frame_is_not_black() {
    if !support::has_gpu() {
        eprintln!("no GPU: skipped");
        return;
    }
    for target in targets() {
        let mut app = Headless::new(support::plugin(), camera(), target);
        app.update();
        let picture = app.picture();
        let black = picture.black_pixels(8);
        picture.save(&out_dir().join(format!(
            "first-frame-{:?}-hdr{}.png",
            target.msaa, target.hdr
        )));
        assert_eq!(
            black, 0,
            "{target:?}: {black} black pixels in the first frames"
        );
    }
}

/// Settled, the frame is all ground, the host's cube above it is seen and
/// the one buried under it is not.
#[test]
fn the_host_shares_the_grounds_depth_and_no_ground_is_black() {
    if !support::has_gpu() {
        eprintln!("no GPU: skipped");
        return;
    }
    for target in targets() {
        let mut app = Headless::new(support::plugin(), camera(), target);
        // Above the ground, west of the nadir: in front of it.
        cube(
            &mut app,
            -445.0,
            0.0,
            400.0,
            300.0,
            Color::srgb(1.0, 0.0, 0.0),
        );
        // Under the hill's flank, east of the nadir: its top 150 m below
        // the ground, hidden by it.
        cube(
            &mut app,
            445.0,
            -220.0,
            -300.0,
            300.0,
            Color::srgb(0.0, 0.0, 1.0),
        );
        let status = app.settle(60.0);
        assert_eq!(status.session, SessionState::Running, "{status:?}");
        assert!(status.is_settled(), "{target:?}: never settled: {status:?}");
        let picture = app.picture();
        picture.save(&out_dir().join(format!("settled-{:?}-hdr{}.png", target.msaa, target.hdr)));

        let black = picture.black_pixels(8);
        assert_eq!(black, 0, "{target:?}: {black} black pixels");
        let (rx, ry) = pixel(-445.0, 0.0, 550.0, picture.width);
        assert!(
            is_red(picture.at(rx, ry)),
            "{target:?}: the cube in front of the ground is not seen at ({rx}, {ry}): {:?}",
            picture.at(rx, ry)
        );
        let (bx, by) = pixel(445.0, -220.0, 0.0, picture.width);
        assert!(
            is_ground(picture.at(bx, by)),
            "{target:?}: the buried cube shows through the ground at ({bx}, {by}): {:?}",
            picture.at(bx, by)
        );
        // And the ground it is buried under is ground everywhere around it.
        let blue = picture
            .rgba
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|p| p[2] > 150 && p[0] < 80 && p[1] < 80)
            .count();
        assert_eq!(blue, 0, "{target:?}: {blue} pixels of the buried cube");
    }
}
