// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The views one frame is drawn for: one for a window, two for a headset.
//!
//! A rig is a row of eyes across the camera's right-hand axis, each the
//! camera moved sideways and looking the same way — parallel axes, as a
//! headset's are. Every eye is a view the traversal is given (its selection is
//! the union, the detail the finest any eye needs), and every eye is drawn
//! from the one resolved set of tiles: what one eye sees, the other is not
//! left without.

use tuile_camera::GlobeCamera;

/// The eyes a frame is drawn for, and how far apart they are.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Rig {
    /// How many: one for a window.
    pub eyes: usize,
    /// Between two neighbours, metres.
    pub separation: f64,
}

impl Rig {
    /// A window's: one eye, where the camera is.
    pub(crate) const MONO: Self = Self {
        eyes: 1,
        separation: 0.0,
    };

    /// Each eye's camera, left to right. One eye is the camera itself, bit
    /// for bit.
    pub(crate) fn cameras(&self, camera: &GlobeCamera) -> Vec<GlobeCamera> {
        let count = self.eyes.max(1);
        if count == 1 {
            return vec![*camera];
        }
        let right = camera.right();
        (0..count)
            .map(|eye| {
                let across = (eye as f64 - (count - 1) as f64 * 0.5) * self.separation;
                GlobeCamera {
                    position: camera.position + right * across,
                    ..*camera
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::DVec3;

    fn camera() -> GlobeCamera {
        GlobeCamera::look_at(
            DVec3::new(7.0e6, 0.0, 0.0),
            DVec3::ZERO,
            DVec3::Z,
            45f64.to_radians(),
        )
    }

    /// One eye is the camera, untouched; two are the camera moved half the
    /// separation each way along its right, looking the same way.
    #[test]
    fn a_pair_of_eyes_straddles_the_camera_along_its_right() {
        let c = camera();
        let mono = Rig::MONO.cameras(&c);
        assert_eq!(mono.len(), 1);
        assert_eq!(mono[0].position, c.position);

        let pair = Rig {
            eyes: 2,
            separation: 0.063,
        }
        .cameras(&c);
        assert_eq!(pair.len(), 2);
        assert!((pair[0].position.distance(pair[1].position) - 0.063).abs() < 1e-6);
        assert!(((pair[0].position + pair[1].position) * 0.5).distance(c.position) < 1e-6);
        assert!(
            (pair[1].position - pair[0].position).dot(c.right()) > 0.0,
            "left to right"
        );
        assert_eq!(pair[0].direction, c.direction);
    }
}
