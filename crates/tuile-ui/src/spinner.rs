// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The loading indicator: a ring of spokes turning, and under it, when there
//! is a number to show, how far the load has got.
//!
//! A widget like the compass — triangles in pixels, no renderer — so it is
//! drawn by the same overlay pass and appears on the same frame as the globe
//! it stands over. It holds no clock: the host says where the turn is, which
//! keeps the geometry a pure function of its inputs and the tests free of
//! time.

use std::f64::consts::TAU;

use crate::{Mesh, Vertex};

/// Spokes round the ring. Twelve is the count every platform's own activity
/// indicator settled on: enough to read as turning, few enough to read as
/// spokes.
pub const SPOKES: usize = 12;

/// The spokes' colour at full strength: near-white, so it reads over sea,
/// desert and snow alike, and is never mistaken for part of the ground.
const SPOKE: [f32; 3] = [0.94, 0.95, 0.97];
/// The dimmest a spoke gets, as a share of the brightest: the tail of the
/// turn never quite disappears, so the ring keeps its shape.
const TAIL: f32 = 0.18;
/// The disc behind the ring, so the spokes stand out from bright ground.
const BACKING: [f32; 4] = [0.05, 0.07, 0.10, 0.45];
/// The progress track and its fill.
const TRACK: [f32; 3] = [0.30, 0.33, 0.38];
const FILL: [f32; 3] = [0.55, 0.78, 0.98];

/// Segments in a full circle for the backing disc and the progress ring.
const SEGMENTS: usize = 64;

/// A loading indicator, as it should look on one frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spinner {
    /// Its centre, in pixels.
    pub centre: (f64, f64),
    /// The outer radius of the spokes, in pixels.
    pub radius: f64,
    /// Where the turn is, in turns: 0.25 is a quarter of the way round. Only
    /// the fractional part matters.
    pub turn: f64,
    /// How much of the load is done, from 0 to 1, when that is known.
    pub progress: Option<f32>,
    /// How visible the whole indicator is, from 0 (not drawn at all) to 1.
    pub opacity: f32,
}

impl Spinner {
    /// The triangles to draw. Empty when the indicator is fully transparent,
    /// so a host can append it unconditionally.
    pub fn mesh(&self) -> Mesh {
        let mut mesh = Mesh::new();
        let opacity = self.opacity.clamp(0.0, 1.0);
        if opacity <= 0.0 || self.radius <= 0.0 {
            return mesh;
        }
        let r = self.radius;
        let backing = [BACKING[0], BACKING[1], BACKING[2], BACKING[3] * opacity];
        disc(&mut mesh, self.centre, r * 1.45, backing);

        // The head is the spoke the turn points at; each one behind it is
        // dimmer, down to the tail.
        let head = (self.turn.rem_euclid(1.0) * SPOKES as f64).floor() as usize % SPOKES;
        let (inner, half_width) = (r * 0.52, r * 0.085);
        for i in 0..SPOKES {
            let behind = (head + SPOKES - i) % SPOKES;
            let strength = 1.0 - (1.0 - TAIL) * behind as f32 / (SPOKES - 1) as f32;
            let angle = TAU * i as f64 / SPOKES as f64;
            let color = [SPOKE[0], SPOKE[1], SPOKE[2], strength * opacity];
            spoke(&mut mesh, self.centre, inner, r, half_width, angle, color);
        }

        if let Some(done) = self.progress {
            let done = f64::from(done.clamp(0.0, 1.0));
            let (inner, outer) = (r * 1.22, r * 1.36);
            let track = [TRACK[0], TRACK[1], TRACK[2], 0.8 * opacity];
            let fill = [FILL[0], FILL[1], FILL[2], opacity];
            arc(&mut mesh, self.centre, inner, outer, 0.0, TAU, track);
            if done > 0.0 {
                arc(&mut mesh, self.centre, inner, outer, 0.0, TAU * done, fill);
            }
        }
        mesh
    }
}

fn on_circle(centre: (f64, f64), radius: f64, angle: f64) -> (f64, f64) {
    // Clockwise from straight up, as a dial reads.
    (
        centre.0 + radius * angle.sin(),
        centre.1 - radius * angle.cos(),
    )
}

fn push_tri(mesh: &mut Mesh, a: (f64, f64), b: (f64, f64), c: (f64, f64), color: [f32; 4]) {
    mesh.push(Vertex::new(a.0, a.1, color));
    mesh.push(Vertex::new(b.0, b.1, color));
    mesh.push(Vertex::new(c.0, c.1, color));
}

fn disc(mesh: &mut Mesh, centre: (f64, f64), radius: f64, color: [f32; 4]) {
    for i in 0..SEGMENTS {
        let a = TAU * i as f64 / SEGMENTS as f64;
        let b = TAU * (i + 1) as f64 / SEGMENTS as f64;
        push_tri(
            mesh,
            centre,
            on_circle(centre, radius, a),
            on_circle(centre, radius, b),
            color,
        );
    }
}

/// A band between two radii, from `from` to `to` radians clockwise from up.
fn arc(
    mesh: &mut Mesh,
    centre: (f64, f64),
    inner: f64,
    outer: f64,
    from: f64,
    to: f64,
    color: [f32; 4],
) {
    let steps = ((SEGMENTS as f64 * (to - from) / TAU).ceil() as usize).max(1);
    for i in 0..steps {
        let a = from + (to - from) * i as f64 / steps as f64;
        let b = from + (to - from) * (i + 1) as f64 / steps as f64;
        let (ia, ib) = (on_circle(centre, inner, a), on_circle(centre, inner, b));
        let (oa, ob) = (on_circle(centre, outer, a), on_circle(centre, outer, b));
        push_tri(mesh, ia, oa, ob, color);
        push_tri(mesh, ia, ob, ib, color);
    }
}

/// One spoke: a quad along the radius at `angle`.
fn spoke(
    mesh: &mut Mesh,
    centre: (f64, f64),
    inner: f64,
    outer: f64,
    half_width: f64,
    angle: f64,
    color: [f32; 4],
) {
    let (s, c) = angle.sin_cos();
    // The direction across the spoke, perpendicular to the radius.
    let (nx, ny) = (c * half_width, s * half_width);
    let (a, b) = (
        on_circle(centre, inner, angle),
        on_circle(centre, outer, angle),
    );
    let p0 = (a.0 - nx, a.1 - ny);
    let p1 = (a.0 + nx, a.1 + ny);
    let p2 = (b.0 + nx, b.1 + ny);
    let p3 = (b.0 - nx, b.1 - ny);
    push_tri(mesh, p0, p1, p2, color);
    push_tri(mesh, p0, p2, p3, color);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spinner() -> Spinner {
        Spinner {
            centre: (200.0, 300.0),
            radius: 40.0,
            turn: 0.0,
            progress: None,
            opacity: 1.0,
        }
    }

    #[test]
    fn a_transparent_spinner_draws_nothing() {
        let hidden = Spinner {
            opacity: 0.0,
            ..spinner()
        };
        assert!(hidden.mesh().is_empty());
        assert!(!spinner().mesh().is_empty());
    }

    /// Every vertex stays inside the disc the indicator claims, so a host can
    /// keep it clear of the screen's edges by its radius alone.
    #[test]
    fn the_spinner_stays_within_its_footprint() {
        let s = Spinner {
            progress: Some(0.6),
            ..spinner()
        };
        let reach = s.radius * 1.45 + 1e-3;
        for v in s.mesh() {
            let d = ((f64::from(v.position[0]) - s.centre.0).powi(2)
                + (f64::from(v.position[1]) - s.centre.1).powi(2))
            .sqrt();
            assert!(d <= reach, "a vertex {d} px from the centre");
        }
    }

    /// The brightest spoke is the one the turn points at, and it moves with
    /// the turn: that is what makes the ring read as turning.
    #[test]
    fn the_brightest_spoke_follows_the_turn() {
        let brightest = |turn: f64| {
            let mesh = Spinner { turn, ..spinner() }.mesh();
            // The backing disc first, then six vertices per spoke.
            let spokes = &mesh[SEGMENTS * 3..SEGMENTS * 3 + SPOKES * 6];
            (0..SPOKES)
                .max_by(|&a, &b| spokes[a * 6].color[3].total_cmp(&spokes[b * 6].color[3]))
                .expect("spokes")
        };
        assert_eq!(brightest(0.0), 0);
        assert_eq!(brightest(0.25), 3);
        assert_eq!(brightest(1.5), 6);
    }

    /// Progress is drawn as a filled arc whose length follows the fraction.
    #[test]
    fn the_progress_arc_grows_with_the_load() {
        let count = |progress| {
            Spinner {
                progress,
                ..spinner()
            }
            .mesh()
            .len()
        };
        let none = count(None);
        let empty = count(Some(0.0));
        let half = count(Some(0.5));
        let full = count(Some(1.0));
        assert!(empty > none, "the track is drawn even at zero");
        assert!(half > empty && full > half);
        assert_eq!(full - empty, 2 * (half - empty));
    }
}
