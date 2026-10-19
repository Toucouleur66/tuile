// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! How much of a picture is bare **where ground was supposed to be**.
//!
//! Counting black pixels alone is a trap, and it caught the first version of
//! `tuile-scan-black`: from geostationary orbit the Earth subtends 17° in a
//! 45° field, most of the frame is legitimately sky, and a naive count
//! reported 68 % black on a frame that was perfectly drawn.
//!
//! So each pixel is cast as a ray from the camera that drew it and tested
//! against the globe. A pixel counts as ground only when its ray — and the
//! rays of its four neighbours — meet an ellipsoid a little inside the real
//! one: the limb, where a pixel is part sky and the multisampled resolve
//! blends the two, is never counted, and neither is the thin band of horizon
//! the backstop shell sits below. What is left is the ground a person sees
//! for certain, and the answer is how many of those pixels came out the clear
//! colour — the only quantity a hole moves.

use crate::Frame;

/// A pixel this dark on every channel is the clear colour rather than dark
/// ground. Ground lit by ambient alone still lands well above it.
pub const BLACK: u8 = 8;

/// WGS84's semi-axes, metres.
const EQUATORIAL: f64 = 6_378_137.0;
const POLAR: f64 = 6_356_752.314_245;

/// How far inside the ellipsoid a ray must reach to count as ground, metres:
/// past the backstop shell, which is drawn five hundred metres below.
const INSIDE: f64 = 1_000.0;

/// What a scan found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Bare {
    /// Pixels whose ray meets the globe for certain.
    pub ground: u64,
    /// Those of them left the clear colour.
    pub black: u64,
}

impl Bare {
    /// The share of the ground that is bare; zero when there is no ground.
    pub fn fraction(&self) -> f64 {
        self.black as f64 / self.ground.max(1) as f64
    }
}

/// Scans tightly packed RGBA8 of `width × height`, drawn by `camera`, every
/// `step`-th pixel in each direction. `step` 1 is every pixel.
pub fn bare_ground(rgba: &[u8], width: u32, height: u32, camera: &Frame, step: usize) -> Bare {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 || rgba.len() < w * h * 4 {
        return Bare::default();
    }
    let step = step.max(1);
    let eye = camera.position;
    // The camera basis the renderer used: forward, right, up.
    let f = normalise(camera.direction);
    let r = normalise(cross(f, camera.up));
    let u = cross(r, f);
    let aspect = w as f64 / h as f64;
    let half_v = (camera.fovy * 0.5).tan();
    let half_h = half_v * aspect;
    let ray = |x: f64, y: f64| {
        let ndc_x = (x + 0.5) / w as f64 * 2.0 - 1.0;
        let ndc_y = 1.0 - (y + 0.5) / h as f64 * 2.0;
        normalise([
            f[0] + r[0] * ndc_x * half_h + u[0] * ndc_y * half_v,
            f[1] + r[1] * ndc_x * half_h + u[1] * ndc_y * half_v,
            f[2] + r[2] * ndc_x * half_h + u[2] * ndc_y * half_v,
        ])
    };
    let mut bare = Bare::default();
    for y in (0..h).step_by(step) {
        for x in (0..w).step_by(step) {
            let (fx, fy) = (x as f64, y as f64);
            let certain = [(0.0, 0.0), (-1.0, 0.0), (1.0, 0.0), (0.0, -1.0), (0.0, 1.0)]
                .iter()
                .all(|(dx, dy)| hits_globe(eye, ray(fx + dx, fy + dy)));
            if !certain {
                continue;
            }
            bare.ground += 1;
            let i = (y * w + x) * 4;
            if rgba[i] < BLACK && rgba[i + 1] < BLACK && rgba[i + 2] < BLACK {
                bare.black += 1;
            }
        }
    }
    bare
}

/// Whether a ray from `eye` along `dir` meets the ellipsoid shrunk by
/// [`INSIDE`], in front of the camera.
fn hits_globe(eye: [f64; 3], dir: [f64; 3]) -> bool {
    // Scaled into a unit sphere: x and y by the equatorial axis, z by the
    // polar one.
    let (a, b) = (EQUATORIAL - INSIDE, POLAR - INSIDE);
    let o = [eye[0] / a, eye[1] / a, eye[2] / b];
    let d = [dir[0] / a, dir[1] / a, dir[2] / b];
    let qa = dot(d, d);
    let qb = 2.0 * dot(o, d);
    let qc = dot(o, o) - 1.0;
    let discriminant = qb * qb - 4.0 * qa * qc;
    // Behind the camera does not count: at low altitude half the rays would
    // otherwise "hit" the planet through the ground beneath the eye.
    discriminant >= 0.0 && (-qb - discriminant.sqrt()) > 0.0
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn normalise(a: [f64; 3]) -> [f64; 3] {
    let n = dot(a, a).sqrt().max(1e-12);
    [a[0] / n, a[1] / n, a[2] / n]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Straight down from 2000 km over the equator.
    fn overhead() -> Frame {
        Frame {
            position: [EQUATORIAL + 2.0e6, 0.0, 0.0],
            direction: [-1.0, 0.0, 0.0],
            up: [0.0, 0.0, 1.0],
            fovy: 45f64.to_radians(),
        }
    }

    fn filled(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
        (0..w * h)
            .flat_map(|_| [rgb[0], rgb[1], rgb[2], 255])
            .collect()
    }

    /// A frame of drawn ground has no bare pixel; the same frame left the
    /// clear colour is bare wherever there is ground, and only there.
    #[test]
    fn bare_ground_is_counted_where_ground_is_and_nowhere_else() {
        let camera = overhead();
        let drawn = bare_ground(&filled(64, 64, [90, 120, 60]), 64, 64, &camera, 1);
        assert!(drawn.ground > 3000, "{drawn:?}");
        assert_eq!(drawn.black, 0);
        let cleared = bare_ground(&filled(64, 64, [0, 0, 0]), 64, 64, &camera, 1);
        assert_eq!(cleared.ground, drawn.ground);
        assert_eq!(cleared.black, cleared.ground);
        assert!((cleared.fraction() - 1.0).abs() < 1e-12);
    }

    /// Looking away from the planet there is no ground at all, so a black
    /// sky is not bare ground.
    #[test]
    fn the_sky_is_never_ground() {
        let camera = Frame {
            direction: [1.0, 0.0, 0.0],
            ..overhead()
        };
        let sky = bare_ground(&filled(32, 32, [0, 0, 0]), 32, 32, &camera, 1);
        assert_eq!(sky, Bare::default());
    }

    /// From geostationary orbit the Earth is a small disc: the pixels around
    /// it are sky, and the limb itself — part sky, part ground — is left out.
    #[test]
    fn the_limb_is_left_out() {
        let camera = Frame {
            position: [EQUATORIAL + 35_786_000.0, 0.0, 0.0],
            ..overhead()
        };
        let disc = bare_ground(&filled(128, 128, [0, 0, 0]), 128, 128, &camera, 1);
        let all = 128 * 128;
        assert!(disc.ground > 0 && disc.ground < all / 4, "{disc:?}");
    }
}
