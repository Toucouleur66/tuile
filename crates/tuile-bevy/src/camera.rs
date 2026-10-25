// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The camera the host drives, and the floating origin its world sits on.
//!
//! # Two precisions, one frame
//!
//! A globe is millions of metres across and a Bevy transform is `f32`: a
//! position on the Earth's surface held in `f32` is good to half a metre,
//! and a camera or a unit placed there jitters by that much every frame. So
//! the globe's positions stay `f64` — Earth-centred, Earth-fixed metres
//! (ECEF) — and Bevy's world is the same frame **translated** to a
//! [`FloatingOrigin`] near the camera: the axes are ECEF's, the origin moves.
//! Every `f32` a Bevy transform holds is then a few kilometres at most.
//!
//! - [`TuileCamera`] is the host's camera, as a [`GlobeCamera`] pose in
//!   `f64`. The plugin writes the camera entity's `Transform` and its lens
//!   from it every frame; the host writes the pose, never the transform.
//! - [`GeoAnchor`] places any entity of the host's at an ECEF position: its
//!   `Transform::translation` is written from it, relative to the origin.
//! - The origin follows the camera, moving when it is more than
//!   [`FloatingOrigin::rebase_distance`] away. Anchored entities move with
//!   it; an entity placed by its translation alone does not, so a host that
//!   rebases keeps its world's positions in anchors.
//!
//! The axes being ECEF's, "up" is not `+Y` but the ellipsoid's normal:
//! [`local_frame`] turns a model's `+Y` up there.

use bevy::prelude::*;
use glam::{DMat3, DQuat, DVec3};
use tuile_camera::GlobeCamera;
use tuile_core::geo::{ecef_to_geodetic, enu_frame, geodetic_to_ecef, Geodetic};

/// The host's camera, in `f64` ECEF. Put it on the `Camera3d` the globe is
/// drawn for — one per application.
#[derive(Component, Debug, Clone, Copy)]
pub struct TuileCamera {
    pub pose: GlobeCamera,
}

impl TuileCamera {
    /// A camera over `lon`/`lat` (degrees) at `altitude` metres, looking
    /// `heading` degrees clockwise from north and `pitch` degrees below the
    /// horizon (90 is straight down), with a vertical field of view of
    /// `fovy_deg`.
    pub fn looking(
        lon: f64,
        lat: f64,
        altitude: f64,
        heading: f64,
        pitch: f64,
        fovy_deg: f64,
    ) -> Self {
        Self {
            pose: GlobeCamera::from_geodetic(
                lat.to_radians(),
                lon.to_radians(),
                altitude,
                heading.to_radians(),
                pitch.to_radians(),
                fovy_deg.to_radians(),
            ),
        }
    }
}

/// Where Bevy's world origin is, in ECEF metres.
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct FloatingOrigin {
    pub ecef: DVec3,
    /// How far the camera may get from the origin before it moves, metres.
    /// A kilometre keeps every `f32` near the camera good to a tenth of a
    /// millimetre.
    pub rebase_distance: f64,
}

impl Default for FloatingOrigin {
    fn default() -> Self {
        Self {
            ecef: DVec3::ZERO,
            rebase_distance: 1_000.0,
        }
    }
}

impl FloatingOrigin {
    /// `ecef` relative to the origin, in Bevy's world.
    pub fn to_world(&self, ecef: DVec3) -> Vec3 {
        (ecef - self.ecef).as_vec3()
    }

    /// A point of Bevy's world, back in ECEF.
    pub fn to_ecef(&self, world: Vec3) -> DVec3 {
        self.ecef + world.as_dvec3()
    }
}

/// An entity of the host's at an ECEF position: its translation is written
/// from it every frame, relative to the [`FloatingOrigin`].
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct GeoAnchor(pub DVec3);

impl GeoAnchor {
    /// At `lon`/`lat` (degrees), `height` metres above the ellipsoid.
    pub fn geodetic(lon: f64, lat: f64, height: f64) -> Self {
        Self(geodetic_to_ecef(Geodetic {
            lon: lon.to_radians(),
            lat: lat.to_radians(),
            height,
        }))
    }
}

/// The rotation that stands a model up at `ecef`: its `+X` east, `+Y` up
/// the ellipsoid's normal, `+Z` south (so `-Z`, Bevy's forward, is north).
pub fn local_frame(ecef: DVec3) -> Quat {
    let enu = enu_frame(ecef_to_geodetic(ecef));
    let (east, north, up) = (enu.col(0), enu.col(1), enu.col(2));
    DQuat::from_mat3(&DMat3::from_cols(east, up, -north)).as_quat()
}

/// How soon a preload view's tiles are wanted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// As much as what is on screen: the camera is about to be there.
    Now,
    /// Behind the drawn view.
    Later,
}

/// A pose traversed but never drawn, so its tiles are on hand before the
/// camera arrives — the same contract as the viewer's.
#[derive(Debug, Clone, Copy)]
pub struct PreloadView {
    pub pose: GlobeCamera,
    /// Width over height of the view it will be drawn at.
    pub aspect: f64,
    pub priority: Priority,
}

impl PreloadView {
    pub fn now(pose: GlobeCamera, aspect: f64) -> Self {
        Self {
            pose,
            aspect,
            priority: Priority::Now,
        }
    }
}

/// This frame's preload views: pushed by the host during a frame, traversed
/// with that frame, and emptied at the start of the next.
#[derive(Resource, Debug, Clone, Default)]
pub struct PreloadViews(pub Vec<PreloadView>);

pub(crate) fn clear_preload_views(mut views: ResMut<PreloadViews>) {
    views.0.clear();
}

/// Moves the origin to the camera when it has strayed, then writes the
/// camera's transform and lens and every anchor's translation.
pub(crate) fn place_in_world(
    mut origin: ResMut<FloatingOrigin>,
    mut cameras: Query<(
        &TuileCamera,
        &mut Transform,
        Option<&mut Projection>,
        Option<&mut Camera3d>,
    )>,
    mut anchors: Query<(&GeoAnchor, &mut Transform), Without<TuileCamera>>,
) {
    let Some(eye) = cameras.iter().next().map(|(c, ..)| c.pose.position) else {
        return;
    };
    if eye.distance(origin.ecef) > origin.rebase_distance {
        origin.ecef = eye;
    }
    let origin = *origin;
    for (camera, mut transform, projection, camera3d) in &mut cameras {
        let pose = camera.pose;
        let placed = Transform {
            translation: origin.to_world(pose.position),
            rotation: pose.orientation().as_quat(),
            scale: Vec3::ONE,
        };
        if *transform != placed {
            *transform = placed;
        }
        if let Some(mut projection) = projection {
            if let Projection::Perspective(lens) = projection.as_mut() {
                let fov = pose.fovy as f32;
                if lens.fov != fov {
                    lens.fov = fov;
                }
            }
        }
        // The globe's pass reads the host's depth to share it: the texture
        // must be bindable as well as drawn into.
        if let Some(mut camera3d) = camera3d {
            let usages = wgpu::TextureUsages::from_bits_truncate(camera3d.depth_texture_usages.0);
            if !usages.contains(wgpu::TextureUsages::TEXTURE_BINDING) {
                camera3d.depth_texture_usages =
                    (usages | wgpu::TextureUsages::TEXTURE_BINDING).into();
            }
        }
    }
    for (anchor, mut transform) in &mut anchors {
        let at = origin.to_world(anchor.0);
        if transform.translation != at {
            transform.translation = at;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Near the origin, an `f32` holds a position to well under a
    /// millimetre however far from the Earth's centre it is, and the trip
    /// back to ECEF loses no more.
    #[test]
    fn positions_near_the_origin_survive_the_trip_through_f32() {
        let origin = FloatingOrigin {
            ecef: GeoAnchor::geodetic(10.0, 0.0, 0.0).0,
            ..Default::default()
        };
        let there = GeoAnchor::geodetic(10.005, 0.003, 412.25).0;
        let back = origin.to_ecef(origin.to_world(there));
        assert!(back.distance(there) < 1e-3, "{}", back.distance(there));
    }

    /// A model stood up at a place has its `+Y` along the ellipsoid's normal
    /// there and `-Z` toward the north.
    #[test]
    fn the_local_frame_stands_a_model_up() {
        let at = GeoAnchor::geodetic(10.0, 45.0, 0.0).0;
        let q = local_frame(at).as_dquat();
        let enu = enu_frame(ecef_to_geodetic(at));
        assert!((q * DVec3::Y).distance(enu.col(2)) < 1e-6);
        assert!((q * DVec3::NEG_Z).distance(enu.col(1)) < 1e-6);
        assert!((q * DVec3::X).distance(enu.col(0)) < 1e-6);
    }

    /// The origin follows the camera only once it has strayed, and anchors
    /// and the camera are written relative to it.
    #[test]
    fn the_origin_follows_the_camera_and_anchors_follow_the_origin() {
        let mut app = App::new();
        app.init_resource::<FloatingOrigin>()
            .add_systems(Update, place_in_world);
        let start = TuileCamera::looking(10.0, 0.0, 3_000.0, 0.0, 45.0, 35.0);
        let camera = app
            .world_mut()
            .spawn((start, Transform::default(), Projection::default()))
            .id();
        let anchored = GeoAnchor::geodetic(10.01, 0.0, 50.0);
        let unit = app.world_mut().spawn((anchored, Transform::default())).id();
        app.update();
        let origin = *app.world().resource::<FloatingOrigin>();
        assert_eq!(origin.ecef, start.pose.position, "first frame: rebased");
        let t = app.world().get::<Transform>(unit).expect("transform");
        assert!(origin.to_ecef(t.translation).distance(anchored.0) < 1e-3);
        let Some(Projection::Perspective(lens)) = app.world().get::<Projection>(camera) else {
            panic!("a perspective lens");
        };
        assert!((f64::from(lens.fov) - 35f64.to_radians()).abs() < 1e-6);

        // 300 m further: the origin stays, the camera moves in the world.
        let mut moved = start;
        moved.pose.position += start.pose.direction * 300.0;
        *app.world_mut()
            .get_mut::<TuileCamera>(camera)
            .expect("camera") = moved;
        app.update();
        assert_eq!(app.world().resource::<FloatingOrigin>().ecef, origin.ecef);
        let t = app.world().get::<Transform>(camera).expect("transform");
        assert!((t.translation.length() - 300.0).abs() < 1e-3);

        // 3 km further: the origin moves to the camera, the unit with it.
        moved.pose.position += start.pose.direction * 3_000.0;
        *app.world_mut()
            .get_mut::<TuileCamera>(camera)
            .expect("camera") = moved;
        app.update();
        let origin = *app.world().resource::<FloatingOrigin>();
        assert_eq!(origin.ecef, moved.pose.position);
        let t = app.world().get::<Transform>(unit).expect("transform");
        assert!(origin.to_ecef(t.translation).distance(anchored.0) < 1e-3);
    }
}
