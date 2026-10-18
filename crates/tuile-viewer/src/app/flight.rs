// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A glide from one view to another: up, across and down.
//!
//! A `goto` is a jump: the eye is put where it was told, at once, because
//! easing a jump across a country would drag it through the planet in a
//! straight line. A glide is the other answer to the same problem: it travels
//! the great circle between the two points rather than the chord, and climbs
//! as it goes — far enough to see where it is going, never more than the
//! distance warrants — and comes down at the far end. A page's `flyTo` and a
//! steering URL's `goto?…&seconds=` both end here.
//!
//! Pure: two views and a clock in, a view out.

use glam::DVec3;
use web_time::Instant;

use crate::start::StartView;

/// The mean radius of the Earth, metres: what turns an arc into a distance.
const EARTH_RADIUS: f64 = 6_371_000.0;
/// How high the climb goes for the distance covered: a quarter of it.
const CLIMB_PER_DISTANCE: f64 = 0.25;
/// No glide climbs higher than this above where it starts or ends.
const HIGHEST_CLIMB: f64 = 4_000_000.0;

/// A glide under way.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Flight {
    from: StartView,
    to: StartView,
    began: Instant,
    seconds: f64,
    /// How much higher than the straight line between the two altitudes the
    /// middle of the glide goes.
    climb: f64,
}

impl Flight {
    /// A glide from `from` to `to`, starting at `now` and lasting `seconds`.
    pub(crate) fn new(from: StartView, to: StartView, now: Instant, seconds: f64) -> Self {
        let distance = arc(unit(&from), unit(&to)) * EARTH_RADIUS;
        let climb = (distance * CLIMB_PER_DISTANCE - from.altitude.max(to.altitude))
            .clamp(0.0, HIGHEST_CLIMB);
        Self {
            from,
            to,
            began: now,
            seconds: seconds.max(f64::EPSILON),
            climb,
        }
    }

    /// Where the eye is at `now`, and whether the glide is over.
    pub(crate) fn at(&self, now: Instant) -> (StartView, bool) {
        let t = (now.saturating_duration_since(self.began).as_secs_f64() / self.seconds).min(1.0);
        if t >= 1.0 {
            return (self.to, true);
        }
        // Slow out, slow in.
        let s = t * t * (3.0 - 2.0 * t);
        let (a, b) = (unit(&self.from), unit(&self.to));
        let point = slerp(a, b, s);
        let altitude = self.from.altitude
            + (self.to.altitude - self.from.altitude) * s
            + self.climb * 4.0 * s * (1.0 - s);
        // The short way round: from 350° to 10° is twenty degrees, not 340.
        let turn = (self.to.heading - self.from.heading + 540.0).rem_euclid(360.0) - 180.0;
        let view = StartView {
            lon: point.y.atan2(point.x).to_degrees(),
            lat: point.z.clamp(-1.0, 1.0).asin().to_degrees(),
            altitude,
            heading: (self.from.heading + turn * s).rem_euclid(360.0),
            pitch: self.from.pitch + (self.to.pitch - self.from.pitch) * s,
        };
        (view, false)
    }
}

/// The view's ground point on the unit sphere.
fn unit(view: &StartView) -> DVec3 {
    let (lat, lon) = (view.lat.to_radians(), view.lon.to_radians());
    DVec3::new(lat.cos() * lon.cos(), lat.cos() * lon.sin(), lat.sin())
}

/// The angle between two unit vectors, radians.
fn arc(a: DVec3, b: DVec3) -> f64 {
    a.dot(b).clamp(-1.0, 1.0).acos()
}

/// Along the great circle from `a` to `b`, a fraction `s` of the way.
fn slerp(a: DVec3, b: DVec3, s: f64) -> DVec3 {
    let angle = arc(a, b);
    if angle < 1e-12 {
        return a;
    }
    // Antipodes have no single great circle; any one through both will do.
    let b = if (std::f64::consts::PI - angle) < 1e-9 {
        let side = if a.z.abs() < 0.9 { DVec3::Z } else { DVec3::X };
        a.cross(side).normalize()
    } else {
        b
    };
    let angle = arc(a, b);
    let sin = angle.sin();
    (a * ((1.0 - s) * angle).sin() + b * (s * angle).sin()) / sin
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn view(lon: f64, lat: f64, altitude: f64, heading: f64) -> StartView {
        StartView {
            lon,
            lat,
            altitude,
            heading,
            pitch: 60.0,
        }
    }

    /// It starts where it was and ends exactly where it was told, then says
    /// it is over.
    #[test]
    fn a_glide_ends_exactly_where_it_was_told() {
        let t0 = Instant::now();
        let (from, to) = (
            view(2.0, 46.0, 2_000_000.0, 0.0),
            view(6.86, 45.83, 6000.0, 120.0),
        );
        let f = Flight::new(from, to, t0, 3.0);
        let (start, done) = f.at(t0);
        assert!(!done);
        assert!((start.lon - 2.0).abs() < 1e-9 && (start.lat - 46.0).abs() < 1e-9);
        assert!((start.altitude - from.altitude).abs() < 1e-6);
        let (end, done) = f.at(t0 + Duration::from_secs(3));
        assert!(done);
        assert_eq!(end, to);
    }

    /// Across an ocean it climbs above both ends, and no higher than the cap.
    #[test]
    fn a_long_glide_climbs_and_a_short_one_does_not() {
        let t0 = Instant::now();
        let (paris, new_york) = (
            view(2.35, 48.85, 5000.0, 0.0),
            view(-74.0, 40.7, 5000.0, 0.0),
        );
        let f = Flight::new(paris, new_york, t0, 4.0);
        let (middle, _) = f.at(t0 + Duration::from_secs(2));
        assert!(middle.altitude > 1_000_000.0, "{}", middle.altitude);
        assert!(middle.altitude <= 5000.0 + HIGHEST_CLIMB + 1.0);
        // It travels the great circle, north of the straight line on a map.
        assert!(middle.lat > 48.85, "{}", middle.lat);

        let near = Flight::new(paris, view(2.36, 48.86, 5000.0, 0.0), t0, 1.0);
        let (middle, _) = near.at(t0 + Duration::from_millis(500));
        assert!(
            (middle.altitude - 5000.0).abs() < 1.0,
            "{}",
            middle.altitude
        );
    }

    /// The heading turns the short way round.
    #[test]
    fn the_heading_turns_the_short_way() {
        let t0 = Instant::now();
        let f = Flight::new(
            view(0.0, 0.0, 1e4, 350.0),
            view(0.0, 0.0, 1e4, 10.0),
            t0,
            2.0,
        );
        let (middle, _) = f.at(t0 + Duration::from_secs(1));
        assert!(
            middle.heading < 1e-6 || middle.heading > 359.999_999,
            "{}",
            middle.heading
        );
    }

    /// From one side of the planet to the other there is still a way.
    #[test]
    fn antipodes_have_a_way_through() {
        let t0 = Instant::now();
        let f = Flight::new(
            view(0.0, 0.0, 1e5, 0.0),
            view(180.0, 0.0, 1e5, 0.0),
            t0,
            2.0,
        );
        let (middle, _) = f.at(t0 + Duration::from_secs(1));
        assert!(middle.lat.is_finite() && middle.lon.is_finite());
        assert!(middle.altitude > 1e6);
    }
}
