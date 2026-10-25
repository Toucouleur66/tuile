// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! How a renderer draws the physical atmosphere: what it calibrates the
//! light against, how its camera is exposed and graded, how large the
//! discs are drawn and how finely the air is marched — every value the
//! GPU uniform ([`crate::gpu::AtmosphereUniform`]) reads of a look, apart
//! from the light itself.
//!
//! A film's look (`tuile_film::Look`) carries these among its own; an
//! interactive viewer has no film look and takes [`SkyLook::default`], which
//! is the film's default to the bit, tuned by the same [`AirTuning`] — so
//! that the sky a viewer shows and the sky a film renders of the same
//! moment are one sky.

use crate::hillaire::{Calibration, SkyLight};
use crate::illumination::CameraResponse;
use crate::tuning::{AirTuning, DEFAULT_HIGHLIGHT_KNEE, DEFAULT_MARCH_STEPS, DEFAULT_VEIL_BOUND};

/// What a renderer's look says of the physical atmosphere.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SkyLook {
    /// The lights the imagery was calibrated under, its reflectance and the
    /// camera ([`crate::LutAtmosphere::light`]).
    pub calibration: Calibration,
    /// The exposure in stops before the camera's own (its auto-exposure).
    pub exposure_ev: f32,
    /// The look's white balance, under the light's.
    pub white_balance: [f32; 3],
    /// The linear luminance where the roll-off of highlights begins.
    pub highlight_knee: f32,
    /// How many times its true size the sun's disc is drawn, and the
    /// Moon's.
    pub sun_scale: f32,
    pub moon_scale: f32,
    /// The camera's glare about the discs, a clean lens's at 1; 0: none.
    pub glare: f32,
    /// [`crate::tuning::VEIL_BOUND`].
    pub veil_bound: f32,
    /// [`crate::tuning::MARCH_STEPS`] and its high twin.
    pub march_steps: [u32; 2],
}

impl Default for SkyLook {
    /// A film's default look (`tuile_film::Look::default`), as its
    /// atmosphere reads it.
    fn default() -> Self {
        Self {
            calibration: Calibration {
                // The film's `Vec3 × f32`, channel by channel: the same
                // floats.
                sun: [1.0f32 * 2.5, 0.98 * 2.5, 0.92 * 2.5],
                sky: [0.9f32 * 0.8, 0.95 * 0.8, 1.0 * 0.8],
                reflectance: 6.5,
                camera: CameraResponse::TODAY,
            },
            exposure_ev: 1.0,
            white_balance: [1.0; 3],
            highlight_knee: DEFAULT_HIGHLIGHT_KNEE,
            sun_scale: 1.0,
            moon_scale: 1.0,
            glare: 1.0,
            veil_bound: DEFAULT_VEIL_BOUND,
            march_steps: DEFAULT_MARCH_STEPS,
        }
    }
}

impl SkyLook {
    /// This look under the tuning values a host read, as a film's look is
    /// tuned (`tuile_film::Look::tuned`): the camera, the veil's bound, the
    /// march and the knee are the tuning's; the reflectance, the exposure
    /// and the calibration's lights those set.
    pub fn tuned(self, air: &AirTuning) -> Self {
        let t = &air.look;
        Self {
            calibration: Calibration {
                sun: t.sun_light.unwrap_or(self.calibration.sun),
                sky: t.sky_light.unwrap_or(self.calibration.sky),
                reflectance: t.reflectance.unwrap_or(self.calibration.reflectance),
                camera: air.camera,
            },
            exposure_ev: t.exposure_ev.unwrap_or(self.exposure_ev),
            highlight_knee: air.highlight_knee,
            veil_bound: air.veil_bound,
            march_steps: air.march_steps,
            ..self
        }
    }

    /// The linear multiplier a picture under `light` is exposed by: the
    /// look's stops and the camera's — what a film's
    /// `Look::lit_by(..).exposure_scale()` is.
    pub fn exposure_scale(&self, light: &SkyLight) -> f32 {
        (self.exposure_ev
            + light
                .illumination
                .exposure_offset_ev_under(&self.calibration.camera))
        .exp2()
    }

    /// The white balance under `light`: the look's times the camera's.
    pub fn white_balance_under(&self, light: &SkyLight) -> [f32; 3] {
        let w = light.illumination.white_balance();
        [0, 1, 2].map(|c| self.white_balance[c] * w[c])
    }
}
