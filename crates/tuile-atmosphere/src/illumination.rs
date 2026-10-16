// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What one moment's sun and sky give a scene, and how a camera meets it:
//! the `ReferenceIllumination` of `docs/18-color-harmonization.md` §6, as
//! amended by `docs/19-sky-atmosphere.md` §5.2 — the numbers a renderer is
//! lit by, frame by frame, whatever model made them.
//!
//! # Units
//!
//! Linear Rec.709 RGB on the renderer's own scale, the one its look's lights
//! are written in: today, a white Lambert surface facing the sun comes out
//! at `sun_rgb`. Exposure is in EV100, and the renderer's look is exposed
//! for [`DAYLIGHT_EV100`]: an illumination at that EV100 with no
//! compensation leaves the look's exposure as it is.
//!
//! # Physical or provisional
//!
//! [`crate::LutAtmosphere::light`] works these numbers out from the air
//! (Hillaire 2020). [`ReferenceIllumination::provisional`] stands in where
//! no atmosphere is drawn: the colour and strength of the light are then the
//! look's own constants, and only the sun's direction follows the hour.
//!
//! # How the camera meets the light
//!
//! Like a camera on automatic, but not all the way: its exposure makes up
//! [`EXPOSURE_FOLLOWS`] of a light's difference from daylight, and never
//! raises the picture by more than [`MAX_ADAPTATION_EV`] — dusk is darker
//! than noon, and night is dim, not black. Its white balance goes
//! [`ReferenceIllumination::WHITE_ADAPTATION`] of the way to the light's
//! white: dawn and dusk stay warm (`docs/19-sky-atmosphere.md` §4.6).

use glam::{DVec3, Vec3};

/// The EV100 a renderer's look is exposed for: full daylight, the "sunny
/// 16" rule's 15. An illumination brighter than that is exposed down by the
/// difference, a dimmer one up.
pub const DAYLIGHT_EV100: f32 = 15.0;

/// How much of a light's difference from daylight, in stops, the camera's
/// exposure makes up.
pub const EXPOSURE_FOLLOWS: f32 = 0.75;

/// The most, in stops, a dim light's exposure is raised over daylight's.
pub const MAX_ADAPTATION_EV: f32 = 4.0;

/// What a moment's sun and sky give a scene, and how the camera meets it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReferenceIllumination {
    /// Unit vector towards the sun, ECEF.
    pub to_sun_ecef: DVec3,
    /// The sun's irradiance on a surface facing it, through the air.
    pub sun_rgb: [f32; 3],
    /// The sky's irradiance as an L1 spherical harmonic, its constants
    /// folded in: a surface of normal `n` (ECEF axes) receives
    /// `sky_sh[0] + sky_sh[1]·n.x + sky_sh[2]·n.y + sky_sh[3]·n.z`.
    /// `sky_sh[0]` alone is the sky averaged over the facings of a wall all
    /// round — what a renderer with one ambient term uses; level ground
    /// receives `sky_sh[0] + sky_sh[·]·up`.
    pub sky_sh: [[f32; 3]; 4],
    /// The white the camera balances towards: the colour a white surface
    /// takes under this light. Equal channels: no cast.
    pub white: [f32; 3],
    /// How far the camera adapts to `white`: 0 not at all, 1 fully. Kept
    /// below 1 so that dawn and dusk stay warmer than noon.
    pub adaptation: f32,
    /// The exposure the light calls for, EV100, physical.
    pub ev100: f32,
    /// The exposure chosen on top, in stops: a shot's choice, not the
    /// light's.
    pub compensation_ev: f32,
    /// The least share of daylight's irradiance the ground is let fall to,
    /// already applied to `sun_rgb` and `sky_sh` by whatever made them:
    /// night is dim, never black.
    pub night_floor: f32,
}

impl ReferenceIllumination {
    /// The night floor: a fiftieth of the reference daylight on level
    /// ground — under the exposure's cap, a third of noon's brightness on
    /// the picture.
    pub const NIGHT_FLOOR: f32 = 0.02;

    /// How far the camera's white balance goes towards a light's white.
    pub const WHITE_ADAPTATION: f32 = 0.6;

    /// **Provisional**, until an atmosphere model computes the light: the
    /// sun at `to_sun_ecef`, and the light the renderer's look already
    /// has — `sun_rgb` and `sky_rgb` as the look states them, the camera
    /// exposed for daylight and not balanced. Night is never black under
    /// it: the sky's constant stays whatever the sun's elevation, and the
    /// ground lit by it alone is the floor.
    pub fn provisional(to_sun_ecef: DVec3, sun_rgb: [f32; 3], sky_rgb: [f32; 3]) -> Self {
        Self {
            to_sun_ecef,
            sun_rgb,
            sky_sh: [sky_rgb, [0.0; 3], [0.0; 3], [0.0; 3]],
            white: [1.0; 3],
            adaptation: 0.0,
            ev100: DAYLIGHT_EV100,
            compensation_ev: 0.0,
            night_floor: Self::NIGHT_FLOOR,
        }
    }

    /// The sky averaged over every facing: [`Self::sky_sh`]'s first term.
    pub fn sky_ambient(&self) -> [f32; 3] {
        self.sky_sh[0]
    }

    /// Stops to add to a look exposed for daylight: the compensation, less
    /// [`EXPOSURE_FOLLOWS`] of how much brighter than daylight the light is,
    /// a dim light raised by [`MAX_ADAPTATION_EV`] at the most.
    pub fn exposure_offset_ev(&self) -> f32 {
        let metered = (self.ev100 - DAYLIGHT_EV100) * EXPOSURE_FOLLOWS;
        self.compensation_ev - metered.max(-MAX_ADAPTATION_EV)
    }

    /// The camera's white balance, as gains on the picture's light: none
    /// at no adaptation; at full, what makes `white` grey at its own
    /// luminance (a von Kries scaling in RGB).
    pub fn white_balance(&self) -> [f32; 3] {
        let white = Vec3::from_array(self.white).max(Vec3::splat(1e-6));
        let luma = white.dot(Vec3::new(0.2126, 0.7152, 0.0722));
        let full = Vec3::splat(luma) / white;
        let a = self.adaptation.clamp(0.0, 1.0);
        (Vec3::ONE + (full - Vec3::ONE) * a).to_array()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_provisional_light_is_the_looks_and_changes_no_exposure_or_colour() {
        let light =
            ReferenceIllumination::provisional(DVec3::Z, [2.5, 2.45, 2.3], [0.7, 0.76, 0.8]);
        assert_eq!(light.exposure_offset_ev(), 0.0);
        assert_eq!(light.white_balance(), [1.0; 3]);
        assert_eq!(light.sky_ambient(), [0.7, 0.76, 0.8]);
        assert!(light.night_floor > 0.0);
    }

    #[test]
    fn a_brighter_light_is_exposed_down_and_a_compensation_added() {
        let mut light = ReferenceIllumination::provisional(DVec3::Z, [1.0; 3], [1.0; 3]);
        light.ev100 = DAYLIGHT_EV100 + 2.0;
        light.compensation_ev = 0.5;
        assert!((light.exposure_offset_ev() - (0.5 - 2.0 * EXPOSURE_FOLLOWS)).abs() < 1e-6);
    }

    #[test]
    fn a_dim_light_is_exposed_up_to_the_cap_and_no_further() {
        let mut light = ReferenceIllumination::provisional(DVec3::Z, [1.0; 3], [1.0; 3]);
        light.ev100 = DAYLIGHT_EV100 - 2.0;
        assert!((light.exposure_offset_ev() - 2.0 * EXPOSURE_FOLLOWS).abs() < 1e-6);
        light.ev100 = DAYLIGHT_EV100 - 12.0;
        assert_eq!(light.exposure_offset_ev(), MAX_ADAPTATION_EV);
        // Darker than daylight on the picture, however dim.
        assert!(light.exposure_offset_ev() < DAYLIGHT_EV100 - light.ev100);
    }

    #[test]
    fn a_warm_white_is_balanced_as_far_as_the_camera_adapts() {
        let mut light = ReferenceIllumination::provisional(DVec3::Z, [1.0; 3], [1.0; 3]);
        light.white = [1.2, 1.0, 0.6];
        light.adaptation = 1.0;
        let full = Vec3::from_array(light.white_balance()) * Vec3::from_array(light.white);
        assert!(
            (full.x - full.y).abs() < 1e-5 && (full.y - full.z).abs() < 1e-5,
            "{full}"
        );
        light.adaptation = 0.5;
        let half = light.white_balance();
        assert!(half[2] > 1.0 && half[2] < 1.0 / 0.6 * 0.95, "{half:?}");
        assert!(half[0] < 1.0);
    }
}
