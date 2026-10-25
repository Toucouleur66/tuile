// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The ground as it is drawn, for the host: heights under its units and
//! where a screen ray meets the ground.
//!
//! The render world hands the main world a fresh `tuile_core::surface`
//! snapshot every frame — the meshes resident on the GPU, each marked
//! whether it was drawn — and [`TuileGround`] answers from it:
//!
//! - [`TuileGround::ground_heights`]: a batch of places, the height of the
//!   **drawn** ground under each, at the level of detail drawn, from its
//!   triangles. Off screen, the finest resident tile answers and says so
//!   ([`GroundHeight::drawn`], [`GroundHeight::level`]); with nothing
//!   resident, the ellipsoid (`level: None`). It never fails.
//! - [`GroundHeight::generation`]: the generation of the tile that answered.
//!   A host caching a unit's height keeps it while the generation it gets is
//!   the one it got.
//! - [`TuileGround::pick`] / [`TuileGround::pick_viewport`]: where a ray, or
//!   the ray under a pixel of a camera, first meets that ground.
//!
//! The snapshot is a frame or two behind the main world — the render world
//! draws what the main world extracted — which is the frame the host sees.

use std::sync::{Arc, RwLock};

use bevy::prelude::*;
use glam::DVec3;
use tuile_core::surface::Surface;

use crate::camera::FloatingOrigin;

/// A place, degrees east and north.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LonLat {
    pub lon: f64,
    pub lat: f64,
}

impl LonLat {
    pub fn new(lon: f64, lat: f64) -> Self {
        Self { lon, lat }
    }
}

/// The drawn ground's height at a place.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GroundHeight {
    /// Metres above the ellipsoid.
    pub height: f64,
    /// The level of the tile that answered, `None` for the bare ellipsoid.
    pub level: Option<u32>,
    /// The tile's generation: the same as long as the same tile answers.
    pub generation: u64,
    /// Whether that tile is drawn — `false` is a resident coarser stand-in,
    /// for a place off screen.
    pub drawn: bool,
}

/// Where a ray met the drawn ground.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GroundHit {
    /// ECEF metres (`FloatingOrigin::to_world` places it in Bevy's world).
    pub ecef: DVec3,
    /// Degrees, and metres above the ellipsoid.
    pub lon: f64,
    pub lat: f64,
    pub height: f64,
    /// Metres along the ray.
    pub distance: f64,
    pub level: Option<u32>,
    pub generation: u64,
}

/// The snapshot, as the render world swaps it in.
#[derive(Clone, Default)]
pub(crate) struct SharedSurface(pub Arc<RwLock<Arc<Surface>>>);

impl SharedSurface {
    pub(crate) fn current(&self) -> Arc<Surface> {
        self.0.read().map(|s| Arc::clone(&s)).unwrap_or_default()
    }

    pub(crate) fn publish(&self, next: Surface) {
        if let Ok(mut s) = self.0.write() {
            *s = Arc::new(next);
        }
    }
}

/// The drawn ground, for the main world: see the module.
#[derive(Resource, Clone)]
pub struct TuileGround {
    pub(crate) surface: SharedSurface,
}

/// How far a pick looks along its ray, metres.
pub const PICK_DISTANCE: f64 = 200_000.0;

impl TuileGround {
    /// The drawn ground's height under each of `places`, in order.
    pub fn ground_heights(&self, places: &[LonLat]) -> Vec<GroundHeight> {
        let surface = self.surface.current();
        places
            .iter()
            .map(|p| {
                let h = surface.height_at(p.lon.to_radians(), p.lat.to_radians());
                GroundHeight {
                    height: h.height,
                    level: h.level,
                    generation: h.generation,
                    drawn: h.drawn,
                }
            })
            .collect()
    }

    /// One place's.
    pub fn height_at(&self, place: LonLat) -> GroundHeight {
        self.ground_heights(&[place])[0]
    }

    /// Where the ray from `from` along `dir` (ECEF) first meets the drawn
    /// ground, within [`PICK_DISTANCE`].
    pub fn pick(&self, from: DVec3, dir: DVec3) -> Option<GroundHit> {
        let hit = self.surface.current().pick(from, dir, PICK_DISTANCE)?;
        Some(GroundHit {
            ecef: hit.position,
            lon: hit.lon.to_degrees(),
            lat: hit.lat.to_degrees(),
            height: hit.height,
            distance: hit.distance,
            level: hit.level,
            generation: hit.generation,
        })
    }

    /// Where the ray under `pixel` (logical, from the viewport's top left)
    /// of `camera` first meets the drawn ground.
    pub fn pick_viewport(
        &self,
        camera: &Camera,
        transform: &GlobalTransform,
        origin: &FloatingOrigin,
        pixel: Vec2,
    ) -> Option<GroundHit> {
        let ray = camera.viewport_to_world(transform, pixel).ok()?;
        self.pick(
            origin.to_ecef(ray.origin),
            ray.direction.as_vec3().as_dvec3(),
        )
    }

    /// Bumped whenever an answer may have changed: a host with nothing else
    /// to go on asks again when it moves.
    pub fn version(&self) -> u64 {
        self.surface.current().version()
    }

    /// How many tiles the ground is answered from.
    pub fn tiles(&self) -> usize {
        self.surface.current().len()
    }
}
