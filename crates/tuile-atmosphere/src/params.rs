// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What the air is made of, and where the sphere it is drawn on stands.
//!
//! [`AtmosphereParams`] is the canonical description of one atmosphere —
//! `docs/19-sky-atmosphere.md` §4.1 — and [`AtmosphereFrame`] the sphere it
//! is laid on for one place (§4.2): the Earth is not a sphere, the air is
//! drawn as one, and a sphere that osculates the ellipsoid where the camera
//! is keeps every height the model reads a *geodetic* height.

use glam::DVec3;
use tuile_core::geo::{geodetic_to_ecef, Geodetic, WGS84_A, WGS84_F};

/// Rayleigh scattering at sea level, per metre, red, green, blue (Hillaire
/// 2020, after Bruneton 2017): the reason the sky is blue.
pub const RAYLEIGH_SCATTERING: [f64; 3] = [5.802e-6, 13.558e-6, 33.1e-6];
/// The height over which air thins by a factor of e, in metres.
pub const RAYLEIGH_SCALE_HEIGHT: f64 = 8_000.0;
/// The height over which aerosols thin by a factor of e, in metres: haze
/// lies low.
pub const MIE_SCALE_HEIGHT: f64 = 1_200.0;
/// How far forward aerosols scatter (Cornette-Shanks g).
pub const MIE_ANISOTROPY: f64 = 0.8;
/// The share of the light aerosols take that they scatter rather than
/// absorb (Hillaire 2020: 3.996 of 4.440).
pub const MIE_SINGLE_SCATTERING_ALBEDO: f64 = 3.996 / 4.440;
/// Ozone's absorption at its densest, per metre: what keeps the zenith blue
/// at twilight (the Chappuis band, absorbing in the green and red).
pub const OZONE_ABSORPTION: [f64; 3] = [0.650e-6, 1.881e-6, 0.085e-6];
/// Ozone is a tent: densest at this height, gone this far either side.
pub const OZONE_CENTRE: f64 = 25_000.0;
pub const OZONE_HALF_WIDTH: f64 = 15_000.0;
/// Where the air is taken to end, over the ground.
pub const ATMOSPHERE_THICKNESS: f64 = 100_000.0;
/// The ground's mean albedo, what light it sends back into the air.
pub const GROUND_ALBEDO: f64 = 0.3;
/// The turbidity of a clear day (Linke-like: the air's whole optical depth
/// over the depth of its molecules alone): aerosols as deep as the air.
pub const CLEAR_TURBIDITY: f64 = 2.0;

/// One atmosphere: its molecules, its aerosols, its ozone and the ground
/// under it. Canonical and plain: two equal values give the same light to
/// the bit, which is what a render's digest wants.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AtmosphereParams {
    /// Rayleigh scattering at sea level, per metre, RGB.
    pub rayleigh_scattering: [f64; 3],
    pub rayleigh_scale_height: f64,
    /// Aerosol scattering and absorption at sea level, per metre.
    pub mie_scattering: f64,
    pub mie_absorption: f64,
    pub mie_scale_height: f64,
    pub mie_anisotropy: f64,
    /// The share of the light aerosols take that they scatter rather than
    /// absorb: what [`Self::with_mie_extinction`] splits an extinction by.
    pub mie_albedo: f64,
    /// Ozone absorption at its densest, per metre, RGB, and its tent.
    pub ozone_absorption: [f64; 3],
    pub ozone_centre: f64,
    pub ozone_half_width: f64,
    /// Where the air ends, over the ground, in metres.
    pub thickness: f64,
    pub ground_albedo: f64,
}

impl Default for AtmosphereParams {
    /// A clear day: [`CLEAR_TURBIDITY`].
    fn default() -> Self {
        Self {
            rayleigh_scattering: RAYLEIGH_SCATTERING,
            rayleigh_scale_height: RAYLEIGH_SCALE_HEIGHT,
            mie_scattering: 0.0,
            mie_absorption: 0.0,
            mie_scale_height: MIE_SCALE_HEIGHT,
            mie_anisotropy: MIE_ANISOTROPY,
            mie_albedo: MIE_SINGLE_SCATTERING_ALBEDO,
            ozone_absorption: OZONE_ABSORPTION,
            ozone_centre: OZONE_CENTRE,
            ozone_half_width: OZONE_HALF_WIDTH,
            thickness: ATMOSPHERE_THICKNESS,
            ground_albedo: GROUND_ALBEDO,
        }
        .with_turbidity(CLEAR_TURBIDITY)
    }
}

impl AtmosphereParams {
    /// The optical depth of the molecules alone, straight up, in the green.
    pub fn rayleigh_depth(&self) -> f64 {
        self.rayleigh_scattering[1] * self.rayleigh_scale_height
    }

    /// With aerosols that make the air `turbidity` times as deep, in the
    /// green, as its molecules alone (1: no aerosol at all; 2 a clear day,
    /// 4 a hazy one, 6 a very hazy one). Below 1 is taken for 1.
    pub fn with_turbidity(self, turbidity: f64) -> Self {
        let depth = (turbidity.max(1.0) - 1.0) * self.rayleigh_depth();
        self.with_mie_extinction(depth / self.mie_scale_height)
    }

    /// With aerosols of optical depth `depth` straight up from sea level, in
    /// the green — the model's 550 nm (Bruneton 2017's wavelengths are 680,
    /// 550 and 440 nm): what a sun photometer such as AERONET's reports as
    /// the aerosol optical depth at 550 nm. Spread over the aerosols' scale
    /// height, it is an extinction of `depth / mie_scale_height` per metre
    /// at sea level; against turbidity, `depth = (T − 1) · τ_R`, `τ_R` the
    /// molecules' own depth ([`Self::rayleigh_depth`], 0.108 by default).
    /// The model's aerosols are grey: the same depth in every channel.
    pub fn with_aerosol_depth(self, depth: f64) -> Self {
        self.with_mie_extinction(depth.max(0.0) / self.mie_scale_height)
    }

    /// The aerosols' optical depth straight up from sea level, in the green:
    /// [`Self::with_aerosol_depth`]'s inverse.
    pub fn aerosol_depth(&self) -> f64 {
        (self.mie_scattering + self.mie_absorption) * self.mie_scale_height
    }

    /// With aerosols that take `extinction` per metre at sea level, of which
    /// they scatter [`Self::mie_albedo`].
    pub fn with_mie_extinction(self, extinction: f64) -> Self {
        let extinction = extinction.max(0.0);
        Self {
            mie_scattering: extinction * self.mie_albedo,
            mie_absorption: extinction * (1.0 - self.mie_albedo),
            ..self
        }
    }

    /// The turbidity these aerosols make: [`Self::with_turbidity`]'s inverse.
    pub fn turbidity(&self) -> f64 {
        1.0 + (self.mie_scattering + self.mie_absorption) * self.mie_scale_height
            / self.rayleigh_depth()
    }

    /// Scattering (Rayleigh RGB, Mie) and extinction RGB at `height` over
    /// the ground, per metre.
    pub fn medium(&self, height: f64) -> Medium {
        let h = height.max(0.0);
        let rayleigh_density = (-h / self.rayleigh_scale_height).exp();
        let mie_density = (-h / self.mie_scale_height).exp();
        let ozone_density = (1.0 - (h - self.ozone_centre).abs() / self.ozone_half_width).max(0.0);
        let rayleigh = self.rayleigh_scattering.map(|s| s * rayleigh_density);
        let mie = self.mie_scattering * mie_density;
        let mie_extinction = (self.mie_scattering + self.mie_absorption) * mie_density;
        let mut extinction = [0.0; 3];
        for c in 0..3 {
            extinction[c] = rayleigh[c] + mie_extinction + self.ozone_absorption[c] * ozone_density;
        }
        Medium {
            rayleigh,
            mie,
            extinction,
        }
    }
}

/// The air at one height, per metre.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Medium {
    pub rayleigh: [f64; 3],
    pub mie: f64,
    pub extinction: [f64; 3],
}

/// The sphere the air is laid on for one place: the one that osculates the
/// ellipsoid there, of the Gaussian radius `√(M·N)` at its latitude, its
/// centre under the place along its geodetic normal. Over a few hundred
/// kilometres around it, the distance to that centre less the radius is the
/// geodetic height to within metres — never `|ecef| − a`, which puts sea
/// level ten kilometres under the ground at 43°N.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AtmosphereFrame {
    /// The sphere's centre, ECEF.
    pub centre: DVec3,
    pub radius: f64,
}

impl AtmosphereFrame {
    /// The sphere osculating the ellipsoid under `at` (its height is not
    /// read).
    pub fn at(at: Geodetic) -> Self {
        let e2 = WGS84_F * (2.0 - WGS84_F);
        let s = at.lat.sin();
        let w = (1.0 - e2 * s * s).sqrt();
        let n = WGS84_A / w;
        let m = WGS84_A * (1.0 - e2) / (w * w * w);
        let radius = (m * n).sqrt();
        let normal = DVec3::new(
            at.lat.cos() * at.lon.cos(),
            at.lat.cos() * at.lon.sin(),
            at.lat.sin(),
        );
        let ground = geodetic_to_ecef(Geodetic { height: 0.0, ..at });
        Self {
            centre: ground - normal * radius,
            radius,
        }
    }

    /// `p`'s height over the sphere, in metres: geodetic, near the place.
    pub fn height(&self, p: DVec3) -> f64 {
        (p - self.centre).length() - self.radius
    }

    /// Up at `p`: away from the sphere's centre.
    pub fn up(&self, p: DVec3) -> DVec3 {
        (p - self.centre).normalize()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuile_core::geo::{ecef_to_geodetic, geodetic_to_ecef};

    #[test]
    fn heights_on_the_osculating_sphere_are_geodetic_near_the_place() {
        let at = Geodetic {
            lon: 0.5f64.to_radians(),
            lat: 43.0f64.to_radians(),
            height: 0.0,
        };
        let frame = AtmosphereFrame::at(at);
        for (dlat, dlon, h) in [(0.0, 0.0, 0.0), (0.0, 0.0, 5000.0), (1.5, -2.0, 1200.0)] {
            let p = geodetic_to_ecef(Geodetic {
                lon: at.lon + f64::to_radians(dlon),
                lat: at.lat + f64::to_radians(dlat),
                height: h,
            });
            let geodetic = ecef_to_geodetic(p).height;
            assert!(
                (frame.height(p) - geodetic).abs() < 120.0,
                "{dlat},{dlon},{h}: {} against {geodetic}",
                frame.height(p)
            );
        }
        // What `|ecef| − a` makes of sea level there: ten kilometres under.
        let sea = geodetic_to_ecef(at);
        assert!(sea.length() - WGS84_A < -9_000.0);
    }

    #[test]
    fn turbidity_is_the_air_over_its_molecules() {
        for t in [1.0, 2.0, 3.5, 6.0] {
            let p = AtmosphereParams::default().with_turbidity(t);
            assert!((p.turbidity() - t).abs() < 1e-12, "{t}: {}", p.turbidity());
        }
        assert_eq!(AtmosphereParams::default().turbidity(), CLEAR_TURBIDITY);
        assert_eq!(
            AtmosphereParams::default()
                .with_turbidity(0.0)
                .mie_scattering,
            0.0
        );
    }

    #[test]
    fn an_aerosol_depth_is_turbidity_in_other_units() {
        let p = AtmosphereParams::default();
        // Today's clear day: turbidity 2, aerosols as deep as the molecules.
        assert!((p.aerosol_depth() - p.rayleigh_depth()).abs() < 1e-15);
        assert!((p.rayleigh_depth() - 0.108_464).abs() < 1e-9);
        let clear = p.with_aerosol_depth(0.06);
        assert!((clear.aerosol_depth() - 0.06).abs() < 1e-15);
        assert!((clear.turbidity() - (1.0 + 0.06 / p.rayleigh_depth())).abs() < 1e-12);
        // Spread over the scale height: the extinction at sea level.
        let e = clear.mie_scattering + clear.mie_absorption;
        assert!((e - 0.06 / MIE_SCALE_HEIGHT).abs() < 1e-18);
    }

    #[test]
    fn the_aerosols_albedo_splits_their_extinction() {
        let p = AtmosphereParams {
            mie_albedo: 0.95,
            ..AtmosphereParams::default()
        }
        .with_mie_extinction(1e-4);
        assert!((p.mie_scattering - 0.95e-4).abs() < 1e-18);
        assert!((p.mie_absorption - 0.05e-4).abs() < 1e-18);
    }
}
