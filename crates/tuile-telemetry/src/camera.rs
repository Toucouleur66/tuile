// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The camera's pose, sent with every push: where the view is drawn from,
//! how high, which way it looks and how wide.
//!
//! The host hands the pose over as it draws ([`set_camera`]); each push
//! takes the last one as eight gauges, one series each per launch:
//!
//! | gauge | what |
//! |---|---|
//! | `tuile_camera_altitude_meters` | above the ground under the camera, as the engine knows the ground |
//! | `tuile_camera_height_ellipsoid_meters` | above the WGS84 ellipsoid |
//! | `tuile_camera_heading_degrees` | clockwise from north, `[0, 360)` |
//! | `tuile_camera_pitch_degrees` | below the horizontal, positive looking down |
//! | `tuile_camera_roll_degrees` | about the view direction, against the local vertical |
//! | `tuile_camera_fov_degrees` | the vertical field of view |
//! | `tuile_camera_latitude_degrees` | the camera's geodetic latitude |
//! | `tuile_camera_longitude_degrees` | the camera's geodetic longitude |
//!
//! **The camera's, never the device's.** The position is the point the
//! view is drawn from, which a person flies anywhere on the globe; the
//! device's own location (a phone's or a browser's geolocation) is not part
//! of a pose, has no way into this module, and is never sent.
//!
//! [`CAMERA_METRICS`] is the closed list of these names: a series named
//! `tuile_camera_*` that is not on it is withheld where the data leaves
//! ([`crate::otlp::sendable`]).

use std::sync::Mutex;

use crate::registry::{Point, Value};

/// The camera's series, every one the push may send: exactly these.
pub const CAMERA_METRICS: [&str; 8] = [
    "tuile_camera_altitude_meters",
    "tuile_camera_height_ellipsoid_meters",
    "tuile_camera_heading_degrees",
    "tuile_camera_pitch_degrees",
    "tuile_camera_roll_degrees",
    "tuile_camera_fov_degrees",
    "tuile_camera_latitude_degrees",
    "tuile_camera_longitude_degrees",
];

/// The prefix every camera series carries, and that the guard watches.
pub const CAMERA_PREFIX: &str = "tuile_camera_";

/// Whether a series named `name` may leave: anything outside the camera's
/// prefix is not this module's to judge; inside it, only [`CAMERA_METRICS`].
pub fn allowed_name(name: &str) -> bool {
    !name.starts_with(CAMERA_PREFIX) || CAMERA_METRICS.contains(&name)
}

/// One camera pose, in the units the gauges carry. All `f64`: what the
/// engine computes, unrounded.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CameraPose {
    /// Metres above the ground under the camera.
    pub altitude_m: f64,
    /// Metres above the WGS84 ellipsoid.
    pub height_ellipsoid_m: f64,
    /// Degrees clockwise from north.
    pub heading_deg: f64,
    /// Degrees below the horizontal.
    pub pitch_deg: f64,
    /// Degrees about the view direction.
    pub roll_deg: f64,
    /// The vertical field of view, degrees.
    pub fov_deg: f64,
    /// Geodetic latitude, degrees.
    pub latitude_deg: f64,
    /// Geodetic longitude, degrees.
    pub longitude_deg: f64,
}

impl CameraPose {
    /// The pose of `camera`, `altitude_m` metres above the ground under it —
    /// the height the host already measures against the terrain it draws
    /// (`CameraController::height_above_ground`), not a second query here.
    /// For a pair of eyes, the camera between them.
    pub fn of(camera: &tuile_camera::GlobeCamera, altitude_m: f64) -> Self {
        let g = camera.geodetic();
        Self {
            altitude_m,
            height_ellipsoid_m: g.height,
            heading_deg: camera.heading().to_degrees(),
            pitch_deg: camera.pitch().to_degrees(),
            roll_deg: camera.roll().to_degrees(),
            fov_deg: camera.fovy.to_degrees(),
            latitude_deg: g.lat.to_degrees(),
            longitude_deg: g.lon.to_degrees(),
        }
    }

    /// The pose as the push's points, in [`CAMERA_METRICS`]' order; a value
    /// that is not a number is left out.
    pub fn points(&self) -> Vec<Point> {
        let values = [
            self.altitude_m,
            self.height_ellipsoid_m,
            self.heading_deg,
            self.pitch_deg,
            self.roll_deg,
            self.fov_deg,
            self.latitude_deg,
            self.longitude_deg,
        ];
        CAMERA_METRICS
            .iter()
            .zip(values)
            .filter(|(_, v)| v.is_finite())
            .map(|(name, v)| Point {
                name: (*name).to_owned(),
                labels: Vec::new(),
                value: Value::Gauge(v),
            })
            .collect()
    }
}

fn slot() -> &'static Mutex<Option<CameraPose>> {
    static SLOT: Mutex<Option<CameraPose>> = Mutex::new(None);
    &SLOT
}

/// The camera's pose as of now; the next push sends it. Cheap — a lock and
/// a copy — and nothing at all when no metrics are offered, so a host may
/// call it every frame.
pub fn set_camera(pose: CameraPose) {
    if !crate::offered().metrics {
        return;
    }
    if let Ok(mut s) = slot().lock() {
        *s = Some(pose);
    }
}

/// The last pose handed over, if any.
pub fn camera() -> Option<CameraPose> {
    slot().lock().ok().and_then(|s| *s)
}

/// The points a push adds to the registry's: the last pose's, or none.
pub(crate) fn points() -> Vec<Point> {
    camera().map(|p| p.points()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A known camera's eight gauges, with its values exactly.
    #[test]
    fn a_pose_is_eight_gauges() {
        let pose = CameraPose {
            altitude_m: 1234.5,
            height_ellipsoid_m: 4321.25,
            heading_deg: 120.0,
            pitch_deg: 35.5,
            roll_deg: -0.25,
            fov_deg: 60.0,
            latitude_deg: 42.9365,
            longitude_deg: 0.1418,
        };
        let points = pose.points();
        let got: Vec<(&str, f64)> = points
            .iter()
            .map(|p| match p.value {
                Value::Gauge(v) => (p.name.as_str(), v),
                _ => panic!("not a gauge: {p:?}"),
            })
            .collect();
        assert_eq!(
            got,
            [
                ("tuile_camera_altitude_meters", 1234.5),
                ("tuile_camera_height_ellipsoid_meters", 4321.25),
                ("tuile_camera_heading_degrees", 120.0),
                ("tuile_camera_pitch_degrees", 35.5),
                ("tuile_camera_roll_degrees", -0.25),
                ("tuile_camera_fov_degrees", 60.0),
                ("tuile_camera_latitude_degrees", 42.9365),
                ("tuile_camera_longitude_degrees", 0.1418),
            ]
        );
        assert!(points.iter().all(|p| p.labels.is_empty()), "no labels");
        let partial = CameraPose {
            altitude_m: f64::NAN,
            ..pose
        };
        assert_eq!(partial.points().len(), 7);
    }

    /// From the engine's camera: its geodetic place, its angles, its field
    /// of view, and the altitude the host measured.
    #[test]
    fn a_pose_is_read_off_a_camera() {
        let camera = tuile_camera::GlobeCamera::from_geodetic(
            42.9365f64.to_radians(),
            0.1418f64.to_radians(),
            3100.0,
            120f64.to_radians(),
            25f64.to_radians(),
            50f64.to_radians(),
        );
        let p = CameraPose::of(&camera, 1100.0);
        let near = |a: f64, b: f64, tol: f64| assert!((a - b).abs() < tol, "{a} vs {b}");
        near(p.latitude_deg, 42.9365, 1e-9);
        near(p.longitude_deg, 0.1418, 1e-9);
        near(p.height_ellipsoid_m, 3100.0, 1e-6);
        near(p.heading_deg, 120.0, 1e-6);
        near(p.pitch_deg, 25.0, 1e-6);
        near(p.roll_deg, 0.0, 1e-6);
        near(p.fov_deg, 50.0, 1e-9);
        assert_eq!(p.altitude_m, 1100.0);
    }

    /// The camera's list is exactly these names; anything else under its
    /// prefix is refused, anything outside it is not its business.
    #[test]
    fn the_list_accepts_exactly_the_camera_s_names() {
        for name in CAMERA_METRICS {
            assert!(allowed_name(name), "{name}");
        }
        for name in [
            "tuile_camera_device_latitude_degrees",
            "tuile_camera_location",
            "tuile_camera_",
            "tuile_camera_latitude",
        ] {
            assert!(!allowed_name(name), "{name}");
        }
        assert!(allowed_name("tuile_frames"));
        let mut sorted = CAMERA_METRICS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), CAMERA_METRICS.len());
    }
}
