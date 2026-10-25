// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Where the sun is, for a given instant.
//!
//! Everything else in this crate is downstream of this. The colour of the air,
//! where the terminator falls, whether a limb glows orange or not at all — all
//! of it is a function of the sun's direction, so a globe that wants to be lit
//! at a *stated* time needs the sun placed from that time rather than from a
//! direction someone picked by eye.
//!
//! # Where the numbers come from
//!
//! NREL's Solar Position Algorithm (Reda & Andreas, 2003), as the
//! `solar-positioning` crate implements it: ±0.0003° from the year −2000 to
//! 6000, with ΔT (TT − UT) estimated from Espenak & Meeus's fits. That is
//! far more than a terminator or a shadow needs; what it buys is that the
//! sun here is *the* sun of the stated instant — the one a reference
//! calculator gives — and never a second opinion to argue with.
//!
//! The crate is built with `libm` and without `std`: the same arithmetic,
//! bit for bit, on every target, the browser's included.
//!
//! Not modelled: refraction. Every position here is geometric — the sun
//! where it is, not where the air shows it, which near the horizon is about
//! half a degree higher. Light is computed from where the sun is.

use glam::DVec3;
use solar_positioning::{time::JulianDate, Location, SolarPositions};
use tuile_core::geo::{enu_frame, Geodetic};

/// Julian date of the Unix epoch, 1970-01-01 00:00 UTC.
const UNIX_EPOCH_JD: f64 = 2_440_587.5;
const SECONDS_PER_DAY: f64 = 86_400.0;
/// ΔT where its fit cannot say: about this century's.
const FALLBACK_DELTA_T: f64 = 69.0;
/// The equatorial radius SPA's parallax is written against, metres.
const SPA_EARTH_RADIUS: f64 = 6_378_140.0;

/// The sun, seen from Earth at one instant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sun {
    /// Unit vector from the planet's centre **toward** the sun, ECEF.
    ///
    /// Toward, not along — the direction light *travels* is its negation, and
    /// mixing the two is the classic way to light a globe inside out. Renderers
    /// that want a travel direction should say so and negate here.
    pub direction_ecef: DVec3,
    /// Latitude of the point directly under the sun, radians. Between roughly
    /// ±23.44° over a year; this is what the seasons are.
    pub subsolar_lat: f64,
    /// Longitude of the same point, radians, east positive. Sweeps a full turn
    /// westward each day.
    pub subsolar_lon: f64,
}

impl Sun {
    /// The sun at a UTC instant given as seconds since the Unix epoch.
    ///
    /// Seconds rather than a date type on purpose: this crate has no business
    /// pulling in a calendar, and every caller already has one instant in some
    /// form it can turn into a number.
    pub fn at_unix_seconds(utc: f64) -> Self {
        Self::at_julian_date(UNIX_EPOCH_JD + utc / SECONDS_PER_DAY)
    }

    /// The sun at a Julian date (UTC), as seen from the Earth's centre.
    ///
    /// SPA answers for an observer; one placed at the Earth's centre — on
    /// the equator, at the prime meridian, its height minus SPA's radius —
    /// has no parallax at all, and its azimuth and elevation are the
    /// geocentric direction itself, written in the axes of that meridian.
    /// An instant SPA refuses (not finite) gives a direction of NaN.
    pub fn at_julian_date(jd: f64) -> Self {
        let Some(seen) = spa(jd, 0.0, 0.0, -SPA_EARTH_RADIUS) else {
            return Self::from_direction_unchecked(DVec3::NAN);
        };
        // East, north and up at (0°, 0°) are +Y, +Z and +X.
        let (azimuth, elevation) = (seen.0.to_radians(), seen.1.to_radians());
        let d = DVec3::new(
            elevation.sin(),
            elevation.cos() * azimuth.sin(),
            elevation.cos() * azimuth.cos(),
        );
        Self::from_direction_unchecked(d)
    }

    /// The sun at a UTC instant (Unix seconds) as it stands over the place
    /// `at` — its azimuth and elevation over that place's horizon, and the
    /// direction towards it from there, ECEF. `None` for an instant or a
    /// place SPA refuses.
    pub fn seen_from(utc: f64, at: Geodetic) -> Option<SunSeen> {
        let (lon, lat) = (at.lon.to_degrees(), at.lat.to_degrees());
        let (azimuth_deg, elevation_deg) =
            spa(UNIX_EPOCH_JD + utc / SECONDS_PER_DAY, lon, lat, at.height)?;
        let local = enu_frame(at);
        let (azimuth, elevation) = (azimuth_deg.to_radians(), elevation_deg.to_radians());
        let level = local.x_axis * azimuth.sin() + local.y_axis * azimuth.cos();
        Some(SunSeen {
            azimuth_deg,
            elevation_deg,
            direction_ecef: (level * elevation.cos() + local.z_axis * elevation.sin()).normalize(),
        })
    }

    fn from_direction_unchecked(d: DVec3) -> Self {
        Self {
            direction_ecef: d,
            subsolar_lat: d.z.clamp(-1.0, 1.0).asin(),
            subsolar_lon: d.y.atan2(d.x),
        }
    }

    /// A sun placed by hand, from a direction that already points at it.
    ///
    /// For scenes that are not meant to be a real moment — a look someone chose
    /// rather than a time someone stated. Kept distinct from the ephemeris so
    /// that a render's lighting is never ambiguous about which it was.
    pub fn from_direction(direction_ecef: DVec3) -> Self {
        let d = direction_ecef.normalize_or(DVec3::X);
        Self {
            direction_ecef: d,
            subsolar_lat: d.z.clamp(-1.0, 1.0).asin(),
            subsolar_lon: d.y.atan2(d.x),
        }
    }

    /// How high the sun stands above the horizon at a point on the ground,
    /// radians. Negative is night; near zero is the terminator, where every
    /// interesting atmospheric effect lives.
    pub fn elevation_at(&self, up_ecef: DVec3) -> f64 {
        self.direction_ecef
            .dot(up_ecef.normalize_or(DVec3::Z))
            .clamp(-1.0, 1.0)
            .asin()
    }

    /// The direction sunlight **travels** — what a renderer's light vector
    /// usually wants.
    pub fn light_travel_direction(&self) -> DVec3 {
        -self.direction_ecef
    }
    /// The first sunset at `at` after `after_utc` (Unix seconds), to the
    /// second: the instant the sun's centre goes down through
    /// [`SUNSET_ELEVATION_DEG`], the almanac's sunset. `None` when it does
    /// not set within two days — a midnight sun, a polar night.
    pub fn next_sunset(after_utc: f64, at: Geodetic) -> Option<f64> {
        Self::next_crossing(after_utc, at, true)
    }

    /// The first sunrise at `at` after `after_utc`, as [`Self::next_sunset`].
    pub fn next_sunrise(after_utc: f64, at: Geodetic) -> Option<f64> {
        Self::next_crossing(after_utc, at, false)
    }

    fn next_crossing(after_utc: f64, at: Geodetic, setting: bool) -> Option<f64> {
        // Over the horizon by how much, the sign the way the crossing goes.
        let over = |t: f64| -> Option<f64> {
            let e = Self::seen_from(t, at)?.elevation_deg - SUNSET_ELEVATION_DEG;
            Some(if setting { e } else { -e })
        };
        const STEP: f64 = 600.0;
        let mut t = after_utc;
        let mut before = over(t)?;
        while t < after_utc + 2.0 * 86_400.0 {
            let next = t + STEP;
            let now = over(next)?;
            if before > 0.0 && now <= 0.0 {
                // Bisected to a second: the sun's height is monotonic over
                // ten minutes about a crossing.
                let (mut lo, mut hi) = (t, next);
                while hi - lo > 0.5 {
                    let mid = 0.5 * (lo + hi);
                    if over(mid)? > 0.0 {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                return Some(0.5 * (lo + hi));
            }
            t = next;
            before = now;
        }
        None
    }
}

/// The almanac's sunset and sunrise: the sun's centre this far under the
/// geometric horizon, its half-diameter and the refraction at the horizon
/// (16′ + 34′).
pub const SUNSET_ELEVATION_DEG: f64 = -0.833;

/// The sun from one place: where it stands over that place's horizon.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SunSeen {
    /// Degrees from north, clockwise through east.
    pub azimuth_deg: f64,
    /// Degrees above the horizon, geometric; negative below it.
    pub elevation_deg: f64,
    /// Unit vector towards the sun from the place, ECEF.
    pub direction_ecef: DVec3,
}

/// SPA's azimuth and elevation, degrees, geometric, at Julian date `jd`
/// (UTC) for an observer at `lon`, `lat` (degrees) and `height` metres.
fn spa(jd: f64, lon: f64, lat: f64, height: f64) -> Option<(f64, f64)> {
    let year = 2000.0 + (jd - 2_451_545.0) / 365.25;
    let delta_t = solar_positioning::delta_t::estimate(year).unwrap_or(FALLBACK_DELTA_T);
    let time = JulianDate::new(jd, delta_t).ok()?;
    let at = SolarPositions::new()
        .at_from_julian(
            time,
            Location {
                latitude: lat,
                longitude: lon,
            },
            height,
            None,
        )
        .ok()?;
    Some((at.azimuth(), at.elevation_angle()))
}

#[cfg(test)]
fn wrap_to_pi(radians: f64) -> f64 {
    use std::f64::consts::{PI, TAU};
    let wrapped = radians.rem_euclid(TAU);
    if wrapped > PI {
        wrapped - TAU
    } else {
        wrapped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Seconds since the Unix epoch for a UTC calendar instant. Written out
    /// rather than pulled from a date crate so the test states its own inputs.
    fn utc(year: i64, month: i64, day: i64, hour: i64, minute: i64) -> f64 {
        // Days from 1970-01-01 to the given date, by the civil-from-days
        // algorithm (Howard Hinnant's), which is exact and has no leap-year
        // special cases to get wrong.
        let y = if month <= 2 { year - 1 } else { year };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = y - era * 400;
        let mp = (month + 9) % 12;
        let doy = (153 * mp + 2) / 5 + day - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146_097 + doe - 719_468;
        (days * 86_400 + hour * 3_600 + minute * 60) as f64
    }

    /// The June solstice is *defined* by the subsolar point reaching its
    /// northern limit, one axial tilt above the equator. If this drifts, the
    /// seasons are wrong and so is every terminator in June.
    #[test]
    fn the_june_solstice_puts_the_sun_over_the_tropic_of_cancer() {
        // 2024-06-20 20:51 UTC.
        let sun = Sun::at_unix_seconds(utc(2024, 6, 20, 20, 51));
        let lat = sun.subsolar_lat.to_degrees();
        assert!(
            (lat - 23.44).abs() < 0.05,
            "subsolar latitude {lat}°, expected the tropic of Cancer"
        );
    }

    /// And the December solstice mirrors it. Testing both catches a sign error
    /// in the obliquity that a single solstice would let through.
    #[test]
    fn the_december_solstice_mirrors_it() {
        // 2024-12-21 09:20 UTC.
        let sun = Sun::at_unix_seconds(utc(2024, 12, 21, 9, 20));
        let lat = sun.subsolar_lat.to_degrees();
        assert!(
            (lat + 23.44).abs() < 0.05,
            "subsolar latitude {lat}°, expected the tropic of Capricorn"
        );
    }

    /// At an equinox the sun stands over the equator, whatever the time of day.
    #[test]
    fn at_an_equinox_the_sun_is_over_the_equator_all_day() {
        // 2024-03-20 03:06 UTC is the March equinox.
        for hour in 0..24 {
            let sun = Sun::at_unix_seconds(utc(2024, 3, 20, 3, 6) + f64::from(hour) * 3600.0);
            let lat = sun.subsolar_lat.to_degrees();
            assert!(
                lat.abs() < 0.45,
                "{hour} h after the equinox the subsolar latitude is {lat}°"
            );
        }
    }

    /// Local noon at Greenwich puts the sun near the prime meridian. Not exactly
    /// on it — the equation of time swings solar noon by up to a quarter of an
    /// hour across the year, which is about 4° of longitude, and reproducing
    /// that swing is most of what the equation of centre above is for.
    #[test]
    fn the_sun_is_near_the_prime_meridian_at_noon_utc() {
        for (month, day) in [(1, 15), (4, 15), (7, 15), (10, 15)] {
            let sun = Sun::at_unix_seconds(utc(2024, month, day, 12, 0));
            let lon = sun.subsolar_lon.to_degrees();
            assert!(
                lon.abs() < 5.0,
                "{month}/{day} noon UTC puts the sun at {lon}° instead of near 0°"
            );
        }
    }

    /// One solar day returns the sun to the same meridian. The point of using
    /// sidereal time is that this stays true; mean solar time would drift the
    /// subsolar longitude by about a degree a day.
    #[test]
    fn a_solar_day_brings_the_sun_back_to_the_same_meridian() {
        let start = utc(2024, 5, 1, 12, 0);
        let a = Sun::at_unix_seconds(start);
        let b = Sun::at_unix_seconds(start + 86_400.0);
        let drift = wrap_to_pi(b.subsolar_lon - a.subsolar_lon).to_degrees();
        assert!(drift.abs() < 0.5, "a day moved the meridian by {drift}°");
    }

    /// The almanac's sunset at Bagnères-de-Bigorre, under the Pic du Midi,
    /// on midsummer's day 2026: 21:42 CEST, 19:42 UTC (calendriergratuit.fr,
    /// for 43.0667° N 0.15° E, to the minute). Ours, within a minute of it;
    /// and the next sunrise, 06:20 CEST, the same.
    #[test]
    fn the_next_sunset_is_the_almanacs() {
        let at = Geodetic {
            lon: 0.15f64.to_radians(),
            lat: 43.0667f64.to_radians(),
            height: 0.0,
        };
        let sunset = Sun::next_sunset(utc(2026, 6, 21, 12, 0), at).expect("it sets");
        let almanac = utc(2026, 6, 21, 19, 42) + 30.0;
        assert!(
            (sunset - almanac).abs() <= 60.0,
            "{} s from the almanac",
            sunset - almanac
        );
        let sunrise = Sun::next_sunrise(sunset, at).expect("it rises");
        let almanac = utc(2026, 6, 22, 4, 20) + 30.0;
        assert!(
            (sunrise - almanac).abs() <= 60.0,
            "{} s from the almanac",
            sunrise - almanac
        );
        // Over the pole in June, it does not set.
        let pole = Geodetic {
            lat: 80f64.to_radians(),
            ..at
        };
        assert_eq!(Sun::next_sunset(utc(2026, 6, 21, 0, 0), pole), None);
    }

    /// NREL's own worked example (Reda & Andreas 2003, table A4.1):
    /// 2003-10-17 12:30:30 at UTC−7, 39.742476° N 105.1786° W, 1830.14 m.
    /// The paper's zenith, 50.11162°, includes 0.0163° of refraction at its
    /// 820 mbar and 11 °C; the geometric sun is that much lower. Its ΔT is
    /// 67 s where ours is estimated, a few seconds off: hundredths of a
    /// degree at most.
    #[test]
    fn the_sun_seen_from_a_place_is_nrels_worked_example() {
        let at = Geodetic {
            lon: (-105.1786f64).to_radians(),
            lat: 39.742476f64.to_radians(),
            height: 1830.14,
        };
        let seen = Sun::seen_from(utc(2003, 10, 17, 19, 30) + 30.0, at).expect("seen");
        let elevation = 90.0 - (50.11162 + 0.0163);
        assert!(
            (seen.elevation_deg - elevation).abs() < 0.02,
            "{} against {elevation}",
            seen.elevation_deg
        );
        assert!(
            (seen.azimuth_deg - 194.34024).abs() < 0.02,
            "{}",
            seen.azimuth_deg
        );
        // The direction says what the angles say, over the place's own
        // horizon.
        let up = enu_frame(at).z_axis;
        let sine = seen.direction_ecef.dot(up);
        assert!((sine - seen.elevation_deg.to_radians().sin()).abs() < 1e-9);
        // And the sun from the Earth's centre is the same sun, but for the
        // parallax of a planet's radius at an astronomical unit: 0.0025°.
        let apart = seen
            .direction_ecef
            .angle_between(Sun::at_unix_seconds(utc(2003, 10, 17, 19, 30) + 30.0).direction_ecef)
            .to_degrees();
        assert!(apart < 0.003, "{apart}°");
    }

    /// From the Earth's centre there is no parallax: the subsolar latitude
    /// is SPA's geocentric declination itself, to rounding.
    #[test]
    fn the_sun_from_the_earths_centre_is_spas_geocentric_sun() {
        for t in [
            utc(2024, 6, 20, 20, 51),
            utc(2003, 10, 17, 19, 30),
            utc(2026, 2, 1, 6, 0),
        ] {
            let jd = UNIX_EPOCH_JD + t / SECONDS_PER_DAY;
            let year = 2000.0 + (jd - 2_451_545.0) / 365.25;
            let delta_t = solar_positioning::delta_t::estimate(year).expect("ΔT");
            let time = JulianDate::new(jd, delta_t).expect("time");
            let declination = SolarPositions::new()
                .for_time_from_julian(time)
                .expect("prepared")
                .declination();
            let lat = Sun::at_unix_seconds(t).subsolar_lat.to_degrees();
            assert!(
                (lat - declination).abs() < 1e-7,
                "{lat} against {declination}"
            );
        }
    }

    /// The direction light travels is the negation of the direction to the sun.
    /// Getting this backwards lights a globe on its night side, and the render
    /// looks plausible until you notice the terminator runs the wrong way.
    #[test]
    fn the_travel_direction_is_the_negation_of_the_direction_to_the_sun() {
        let sun = Sun::at_unix_seconds(utc(2024, 7, 1, 15, 0));
        assert!((sun.light_travel_direction() + sun.direction_ecef).length() < 1e-12);
        // The subsolar point has the sun overhead by construction.
        let overhead = sun.elevation_at(sun.direction_ecef).to_degrees();
        assert!(
            (overhead - 90.0).abs() < 1e-6,
            "{overhead}° at the subsolar point"
        );
        // And its antipode has it exactly underfoot.
        let midnight = sun.elevation_at(-sun.direction_ecef).to_degrees();
        assert!(
            (midnight + 90.0).abs() < 1e-6,
            "{midnight}° at the antipode"
        );
    }
}
