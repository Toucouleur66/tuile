// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The planets the eye sees, Mercury to Uranus: where each is, how much
//! light it sends, its colour. Drawn as the stars are — points of light,
//! through the air before them, hidden by what stands before them.
//!
//! # Where
//!
//! The heliocentric places of the planet and of the Earth are the `astro`
//! crate's evaluation of VSOP87D (Bretagnon & Francou 1988), in the
//! ecliptic and equinox of the date, the planet taken where it was when
//! the light now arriving left it (one light-time iteration). Their
//! difference is the geocentric place; the nutation in longitude and the
//! true obliquity of the date (as [`crate::moon`] uses) turn it to the
//! equator of the date, and [`crate::celestial_to_ecef`] to the Earth's
//! frame. Aberration (20″) and the planets' parallax (under 35″, Venus at
//! its closest) are left out: a pixel of a picture is a few arcminutes.
//! Against JPL Horizons the places are good to well under an arcminute.
//!
//! # Light
//!
//! Each planet's visual magnitude is Mallama & Hilton's (2018, *Astronomy
//! and Computing* 25, 10 — the formulas of the *Astronomical Almanac*):
//! `5 log₁₀(r Δ)` and the planet's phase curve in the phase angle; Saturn
//! with its rings' tilt towards the Earth. The small terms they add for
//! Mars's and Uranus's orientation (under 0.1 magnitude) are left out.
//! The light is then the stars' rule: `10^(−0.4 (V − V☉))` of the sun's,
//! in the colour of a black body of the planet's B−V (Mallama 2017's
//! colour indices of the planets), luminance one.

use std::sync::Mutex;

use glam::DVec3;

use crate::stars::{colour_of, irradiance_of, temperature_of};

/// A planet the eye can see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Planet {
    Mercury,
    Venus,
    Mars,
    Jupiter,
    Saturn,
    Uranus,
}

impl Planet {
    pub const ALL: [Planet; 6] = [
        Planet::Mercury,
        Planet::Venus,
        Planet::Mars,
        Planet::Jupiter,
        Planet::Saturn,
        Planet::Uranus,
    ];

    fn astro(self) -> astro::planet::Planet {
        match self {
            Planet::Mercury => astro::planet::Planet::Mercury,
            Planet::Venus => astro::planet::Planet::Venus,
            Planet::Mars => astro::planet::Planet::Mars,
            Planet::Jupiter => astro::planet::Planet::Jupiter,
            Planet::Saturn => astro::planet::Planet::Saturn,
            Planet::Uranus => astro::planet::Planet::Uranus,
        }
    }

    /// Its B−V colour index (Mallama 2017).
    pub fn b_v(self) -> f64 {
        match self {
            Planet::Mercury => 0.97,
            Planet::Venus => 0.81,
            Planet::Mars => 1.43,
            Planet::Jupiter => 0.87,
            Planet::Saturn => 1.09,
            Planet::Uranus => 0.56,
        }
    }

    /// Its visual magnitude at `r` AU from the sun and `delta` AU from the
    /// eye, seen at a phase angle of `phase` degrees; Saturn's rings tilted
    /// by `rings` radians, their lit face towards the eye (Mallama &
    /// Hilton 2018).
    pub fn magnitude(self, r: f64, delta: f64, phase: f64, rings: f64) -> f64 {
        let a = phase;
        let distance = 5.0 * (r * delta).log10();
        distance
            + match self {
                Planet::Mercury => {
                    -0.613 + 6.3280e-2 * a - 1.6336e-3 * a.powi(2) + 3.3644e-5 * a.powi(3)
                        - 3.4265e-7 * a.powi(4)
                        + 1.6893e-9 * a.powi(5)
                        - 3.0334e-12 * a.powi(6)
                }
                Planet::Venus if a <= 163.7 => {
                    -4.384 - 1.044e-3 * a + 3.687e-4 * a.powi(2) - 2.814e-6 * a.powi(3)
                        + 8.938e-9 * a.powi(4)
                }
                Planet::Venus => 236.05828 - 2.81914 * a + 8.39034e-3 * a.powi(2),
                Planet::Mars if a <= 50.0 => -1.601 + 2.267e-2 * a - 1.302e-4 * a.powi(2),
                Planet::Mars => -0.367 - 0.02573 * a + 3.445e-4 * a.powi(2),
                Planet::Jupiter if a <= 12.0 => -9.395 - 3.7e-4 * a + 6.16e-4 * a.powi(2),
                Planet::Jupiter => {
                    let x = a / 180.0;
                    -9.428
                        - 2.5
                            * (1.0 - 1.507 * x - 0.363 * x.powi(2) - 0.062 * x.powi(3)
                                + 2.809 * x.powi(4)
                                - 1.876 * x.powi(5))
                            .log10()
                }
                Planet::Saturn => {
                    let s = rings.abs().sin();
                    -8.914 - 1.825 * s + 0.026 * a - 0.378 * s * (-2.25 * a).exp()
                }
                Planet::Uranus => -7.110 + 6.587e-3 * a + 1.045e-4 * a.powi(2),
            }
    }
}

/// Saturn's north pole, J2000 equatorial (IAU WGCCRE 2015: α 40.589°,
/// δ 83.537°), what its rings lie about.
fn saturn_pole() -> DVec3 {
    let (a, d) = (40.589f64.to_radians(), 83.537f64.to_radians());
    DVec3::new(d.cos() * a.cos(), d.cos() * a.sin(), d.sin())
}

/// A planet as the eye sees it at an instant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlanetSeen {
    pub planet: Planet,
    /// Towards it from the Earth's centre, the true equator and equinox of
    /// the date.
    pub of_date: DVec3,
    /// Its visual magnitude.
    pub v: f64,
    /// The angle at the planet between the sun and the Earth, degrees.
    pub phase: f64,
    /// Its distances from the sun and from the Earth, AU.
    pub r: f64,
    pub delta: f64,
}

impl PlanetSeen {
    /// Its irradiance outside the air, against the sun's.
    pub fn irradiance(&self) -> f64 {
        irradiance_of(self.v)
    }

    /// Its colour in units of the sun's, luminance one.
    pub fn colour(&self) -> [f64; 3] {
        colour_of(temperature_of(self.planet.b_v()))
    }
}

/// Heliocentric ecliptic rectangular coordinates of date, AU.
fn heliocentric(planet: &astro::planet::Planet, jde: f64) -> DVec3 {
    let (l, b, r) = astro::planet::heliocent_coords(planet, jde);
    DVec3::new(r * b.cos() * l.cos(), r * b.cos() * l.sin(), r * b.sin())
}

/// Every planet at the UTC instant `utc`.
pub fn seen_at(utc: f64) -> [PlanetSeen; 6] {
    const UNIX_EPOCH_JD: f64 = 2_440_587.5;
    // Light from the planet travels 1 AU in 0.005 775 518 3 days.
    const LIGHT_DAYS_PER_AU: f64 = 0.005_775_518_3;
    let jd = UNIX_EPOCH_JD + utc / 86_400.0;
    let jde = jd + crate::moon::delta_t(jd) / 86_400.0;
    let earth = heliocentric(&astro::planet::Planet::Earth, jde);
    let (nutation_longitude, nutation_obliquity) = astro::nutation::nutation(jde);
    let obliquity = astro::ecliptic::mn_oblq_laskar(jde) + nutation_obliquity;
    Planet::ALL.map(|planet| {
        let p = planet.astro();
        let mut at = heliocentric(&p, jde);
        let tau = (at - earth).length() * LIGHT_DAYS_PER_AU;
        at = heliocentric(&p, jde - tau);
        let geo = at - earth;
        let longitude = geo.y.atan2(geo.x) + nutation_longitude;
        let latitude = (geo.z / geo.length()).asin();
        let ra = astro::coords::asc_frm_ecl(longitude, latitude, obliquity);
        let dec = astro::coords::dec_frm_ecl(longitude, latitude, obliquity);
        let of_date = DVec3::new(dec.cos() * ra.cos(), dec.cos() * ra.sin(), dec.sin());
        let (r, delta, sun_earth) = (at.length(), geo.length(), earth.length());
        let phase = ((r * r + delta * delta - sun_earth * sun_earth) / (2.0 * r * delta))
            .clamp(-1.0, 1.0)
            .acos()
            .to_degrees();
        // The rings' tilt: the latitude of the Earth over Saturn's
        // equator. The pole is J2000's, the direction the date's: the
        // quarter of a degree between them is nothing to a magnitude.
        // The rings' tilt: the Earth's and the sun's latitudes over
        // Saturn's equator, their sines' geometric mean — the lit face as
        // the Earth sees it, as JPL Horizons applies Mallama & Hilton; none
        // when the Earth sees the dark face. The pole is J2000's, the
        // directions the date's: the quarter of a degree between them is
        // nothing to a magnitude.
        let rings = if planet == Planet::Saturn {
            let from_sun = {
                let s = -at;
                let lon = s.y.atan2(s.x);
                let lat = (s.z / s.length()).asin();
                let ra = astro::coords::asc_frm_ecl(lon, lat, obliquity);
                let dec = astro::coords::dec_frm_ecl(lon, lat, obliquity);
                DVec3::new(dec.cos() * ra.cos(), dec.cos() * ra.sin(), dec.sin())
            };
            let earth_side = (-of_date).dot(saturn_pole());
            let sun_side = from_sun.dot(saturn_pole());
            if earth_side * sun_side > 0.0 {
                (earth_side * sun_side).sqrt().asin()
            } else {
                0.0
            }
        } else {
            0.0
        };
        PlanetSeen {
            planet,
            of_date,
            v: planet.magnitude(r, delta, phase, rings),
            phase,
            r,
            delta,
        }
    })
}

/// [`seen_at`] at the start of the minute `utc` falls in, worked out once
/// a minute: a planet moves among the stars by a few arcseconds a minute,
/// and a frame asks for them sixty times a second. The same instant always
/// gives the same planets, whatever was asked before. The Earth's turn is
/// applied by the caller ([`crate::celestial_to_ecef`]) at the instant
/// itself.
pub fn seen_at_cached(utc: f64) -> [PlanetSeen; 6] {
    static LAST: Mutex<Option<(i64, [PlanetSeen; 6])>> = Mutex::new(None);
    let minute = (utc / 60.0).floor() as i64;
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    match *last {
        Some((m, planets)) if m == minute => planets,
        _ => {
            let planets = seen_at(minute as f64 * 60.0);
            *last = Some((minute, planets));
            planets
        }
    }
}

/// The planets of `utc` in the Earth's frame (ECEF), as the sky draws
/// them: direction and irradiance over the sun's, and colour.
pub fn lights_at(utc: f64) -> [([f32; 4], [f32; 4]); 6] {
    lights_turned(utc, crate::celestial_to_ecef(utc))
}

/// The planets of `utc`, turned from the equator of the date by `turn`.
pub fn lights_turned(utc: f64, turn: glam::DMat3) -> [([f32; 4], [f32; 4]); 6] {
    seen_at_cached(utc).map(|p| {
        let d = (turn * p.of_date).as_vec3();
        let c = p.colour();
        (
            [d.x, d.y, d.z, p.irradiance() as f32],
            [c[0] as f32, c[1] as f32, c[2] as f32, 0.0],
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// JPL Horizons, geocentric, airless apparent right ascension and
    /// declination of the date (degrees), visual magnitude (APmag), and
    /// phase angle (S-T-O, degrees), as fetched on 2026-10-25:
    /// 2026-06-21 20:00 UTC, then 2027-02-19 00:00 UTC.
    /// A planet, its right ascension and declination, magnitude and phase.
    type Row = (Planet, f64, f64, f64, f64);

    const HORIZONS: [(f64, [Row; 6]); 2] = [
        (
            1_782_072_000.0,
            [
                (Planet::Mercury, 115.746_024, 21.146_910, 0.953, 117.0847),
                (Planet::Venus, 132.716_837, 19.721_966, -4.026, 63.2121),
                (Planet::Mars, 52.748_904, 18.620_229, 1.337, 24.3158),
                (Planet::Jupiter, 120.419_033, 20.937_056, -1.826, 5.1532),
                (Planet::Saturn, 13.543_875, 3.257_025, 0.810, 5.9990),
                (Planet::Uranus, 61.230_979, 20.647_460, 5.818, 1.3655),
            ],
        ),
        (
            1_802_995_200.0,
            [
                (Planet::Mercury, 330.246_923, -8.176_586, 5.323, 169.2803),
                (Planet::Venus, 289.165_767, -20.841_868, -4.137, 66.7649),
                (Planet::Mars, 154.713_699, 15.278_681, -1.337, 2.7067),
                (Planet::Jupiter, 143.731_889, 15.508_232, -2.549, 1.6818),
                (Planet::Saturn, 11.917_844, 2.630_753, 0.795, 4.0351),
                (Planet::Uranus, 59.653_506, 20.381_417, 5.703, 2.9153),
            ],
        ),
    ];

    #[test]
    fn the_planets_are_where_jpl_horizons_puts_them() {
        for (utc, table) in HORIZONS {
            let seen = seen_at(utc);
            for (planet, ra, dec, _, phase) in table {
                let p = seen.iter().find(|s| s.planet == planet).expect("listed");
                let (ra, dec) = (ra.to_radians(), dec.to_radians());
                let want = DVec3::new(dec.cos() * ra.cos(), dec.cos() * ra.sin(), dec.sin());
                let off = p.of_date.angle_between(want).to_degrees() * 60.0;
                assert!(off < 0.75, "{planet:?} at {utc}: {off:.2}′ off");
                assert!(
                    (p.phase - phase).abs() < 0.05,
                    "{planet:?}: phase {}",
                    p.phase
                );
            }
        }
    }

    #[test]
    fn the_planets_are_as_bright_as_the_almanac_says() {
        for (utc, table) in HORIZONS {
            let seen = seen_at(utc);
            for (planet, _, _, v, _) in table {
                let p = seen.iter().find(|s| s.planet == planet).expect("listed");
                // Mars's and Uranus's orientation terms are left out.
                let within = match planet {
                    Planet::Mars | Planet::Uranus => 0.12,
                    _ => 0.03,
                };
                assert!(
                    (p.v - v).abs() < within,
                    "{planet:?} at {utc}: V {:.3} against {v}",
                    p.v
                );
            }
        }
    }

    #[test]
    fn venus_is_the_brightest_point_of_the_sky_after_the_moon() {
        let seen = seen_at(1_782_072_000.0);
        let venus = seen
            .iter()
            .find(|s| s.planet == Planet::Venus)
            .expect("Venus");
        assert!(venus.irradiance() > irradiance_of(-1.46) * 10.0);
        let c = venus.colour();
        // Sunlight off clouds: a little warmer than the sun, never blue.
        assert!(c[0] > c[2]);
    }
}
