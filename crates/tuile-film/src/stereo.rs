// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A film for two eyes: two cameras a frame, and one picture that holds both.
//!
//! # The rig
//!
//! Each eye is the frame's camera moved sideways by half the **baseline**,
//! along its own right axis, its axis kept parallel to the frame's. Two
//! parallel cameras alone would put everything in front of the screen; each
//! eye's frustum is therefore shifted (off-axis, a lens shift — never a
//! toe-in, which makes vertical disparity at the corners) so that both frame
//! the same window at the **convergence** distance. What lies there is on the
//! screen; what is nearer comes out of it; what is further goes into it.
//!
//! # Scaled for the scene
//!
//! Eyes 65 mm apart over a valley seen from three kilometres would see no
//! relief at all. The baseline is not given in millimetres: it follows from
//! how far behind the screen the most distant ground may go — the
//! **divergence**, the parallax of a point at infinity as a fraction of the
//! picture's width. With `d` that fraction, `t` the tangent of the half
//! horizontal field and `C` the convergence, the baseline is
//! `b = 2·u·C·t`, `u = d / (1 + d)`: nothing beyond the screen ever exceeds
//! `d`, whatever the scene, and the relief scales with the distance looked
//! at — which is what hyperstereo over terrain asks for.
//!
//! # Never a tile less than the frame selected
//!
//! The pack selected each frame's tiles for the frame's camera, not for the
//! eyes. An eye moved sideways sees past that camera's frustum on one side,
//! and what it sees there was never selected: a sliver of black ground at
//! the picture's edge. So each eye's window is narrowed — zoomed in by
//! `1 / (1 − u)` — just enough that its frustum lies inside the frame's from
//! half the convergence distance outwards, at every depth: what an eye draws
//! was selected. The picture loses `u` of its field (2 % at the default
//! divergence); it never gains an unselected tile.

use glam::{DVec3, Mat4, Vec3};

use crate::FrameCamera;

/// One of the two eyes.
///
/// `tuile-overlay`'s, re-exported: an overlay says which eye it is drawn into
/// ([`tuile_overlay::OverlayEyes`]), and two enums that look alike would be
/// two things to convert between for no reason.
pub use tuile_overlay::Eye;

/// What a film does with an eye, which an overlay has no opinion about.
pub(crate) trait EyeSide {
    /// −1 for the left eye, +1 for the right: which way it is moved.
    fn side(self) -> f64;
}

impl EyeSide for Eye {
    fn side(self) -> f64 {
        match self {
            Eye::Left => -1.0,
            Eye::Right => 1.0,
        }
    }
}

/// How the two eyes share one picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Packing {
    /// Left eye on the left half, right eye on the right.
    SideBySide,
    /// Left eye on the top half, right eye on the bottom.
    TopBottom,
}

/// Where the screen is, as a distance from the eyes along the view axis.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Convergence {
    /// This many metres, every frame.
    Distance(f64),
    /// Where the frame's axis meets the ground, the ground taken as the
    /// ellipsoid raised by `height_m`: the subject the camera aims at sits
    /// on the screen. An axis that misses it — above the horizon — converges
    /// at the horizon.
    Ground { height_m: f64 },
}

/// A film for two eyes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stereo {
    pub packing: Packing,
    /// Each eye squeezed into half the picture — the frame-compatible
    /// packing a 3D television expects — rather than the picture doubled.
    pub half: bool,
    /// The parallax of a point at infinity, as a fraction of an eye's
    /// picture width: how far behind the screen the furthest ground goes.
    pub divergence: f64,
    pub convergence: Convergence,
    /// The eyes this many metres apart whatever the distance looked at —
    /// hyperstereo by the metre — in place of the baseline the divergence
    /// gives; the divergence is then what follows from it.
    pub baseline_m: Option<f64>,
}

/// The widest baseline a spec may ask for, metres.
pub const MAX_BASELINE_M: f64 = 10_000.0;

/// The most of its field an eye gives up to a baseline that is given: past
/// it the baseline is shortened rather than the picture narrowed further.
const MOST_NARROWED: f64 = 0.25;

/// The most behind-the-screen parallax a spec may ask for: past a few
/// percent of the width, eyes on a large screen must diverge.
pub const MAX_DIVERGENCE: f64 = 0.05;

/// The divergence a film is made with unless it says otherwise: two percent
/// of the width, comfortable on a television and on a cinema screen alike.
pub const DEFAULT_DIVERGENCE: f64 = 0.02;

/// The closest the screen may be put, in units of the camera's near plane.
const NEAREST_CONVERGENCE_IN_NEARS: f64 = 10.0;

impl Default for Stereo {
    fn default() -> Self {
        Self {
            packing: Packing::SideBySide,
            half: false,
            divergence: DEFAULT_DIVERGENCE,
            convergence: Convergence::Ground { height_m: 0.0 },
            baseline_m: None,
        }
    }
}

/// Why a stereo spec cannot be filmed.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum StereoError {
    #[error("stereo divergence is {0}, and must be above 0 and at most {MAX_DIVERGENCE}")]
    Divergence(f64),
    #[error("stereo convergence is {0} m, and must be a positive distance")]
    Convergence(f64),
    #[error("stereo ground height is {0} m, and must be finite")]
    GroundHeight(f64),
    #[error("stereo baseline is {0} m, and must be above 0 and at most {MAX_BASELINE_M} m")]
    Baseline(f64),
}

/// One frame's pair of eyes, as the frame's camera gives them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EyePair {
    /// From one eye to the other, metres.
    pub baseline_m: f64,
    /// The distance on the screen, metres along the view axis.
    pub convergence_m: f64,
    /// How much each eye's window is narrowed: `1 − u`.
    pub window: f64,
    /// The lens shift of the right eye, in NDC; the left eye's is its
    /// opposite. Equal to the divergence.
    pub shift_ndc: f64,
}

impl Stereo {
    pub fn check(&self) -> Result<(), StereoError> {
        let d = self.divergence;
        if !(d.is_finite() && d > 0.0 && d <= MAX_DIVERGENCE) {
            return Err(StereoError::Divergence(d));
        }
        if let Some(b) = self.baseline_m {
            if !(b.is_finite() && b > 0.0 && b <= MAX_BASELINE_M) {
                return Err(StereoError::Baseline(b));
            }
        }
        match self.convergence {
            Convergence::Distance(m) if !(m.is_finite() && m > 0.0) => {
                Err(StereoError::Convergence(m))
            }
            Convergence::Ground { height_m } if !height_m.is_finite() => {
                Err(StereoError::GroundHeight(height_m))
            }
            _ => Ok(()),
        }
    }

    /// One eye's picture, from the size the film would have for one eye
    /// alone, `width × height`: the same, or squeezed into half of it.
    /// Kept to multiples of eight, as every picture of a film is.
    pub fn eye_size(&self, width: u32, height: u32) -> (u32, u32) {
        let half = |side: u32| ((side / 2) / 8).max(1) * 8;
        match (self.half, self.packing) {
            (false, _) => (width, height),
            (true, Packing::SideBySide) => (half(width), height),
            (true, Packing::TopBottom) => (width, half(height)),
        }
    }

    /// The whole picture, both eyes in it, for eyes of `eye_size`.
    pub fn picture_size(&self, eye: (u32, u32)) -> (u32, u32) {
        match self.packing {
            Packing::SideBySide => (eye.0 * 2, eye.1),
            Packing::TopBottom => (eye.0, eye.1 * 2),
        }
    }

    /// The pair of eyes for the frame whose camera is `centre`.
    pub fn eyes(&self, centre: &FrameCamera) -> EyePair {
        // The projection's x scale is 1 / tan(half the horizontal field).
        let t = 1.0 / f64::from(centre.projection.x_axis.x);
        let convergence_m = self
            .convergence_of(centre)
            .max(NEAREST_CONVERGENCE_IN_NEARS * f64::from(centre.near));
        // `u` is half the baseline against the window's half-width at the
        // convergence; the divergence is `u / (1 − u)` either way.
        let (u, d) = match self.baseline_m {
            None => (self.divergence / (1.0 + self.divergence), self.divergence),
            Some(b) => {
                let u = (b / (2.0 * convergence_m * t)).min(MOST_NARROWED);
                (u, u / (1.0 - u))
            }
        };
        EyePair {
            baseline_m: 2.0 * u * convergence_m * t,
            convergence_m,
            window: 1.0 - u,
            shift_ndc: d,
        }
    }

    fn convergence_of(&self, centre: &FrameCamera) -> f64 {
        match self.convergence {
            Convergence::Distance(m) => m,
            Convergence::Ground { height_m } => {
                let axis = forward(centre);
                ground_along(centre.eye, axis, height_m)
                    .unwrap_or_else(|| horizon_distance(centre.eye, height_m))
            }
        }
    }
}

/// The camera's view axis, in world (ECEF) axes.
fn forward(camera: &FrameCamera) -> DVec3 {
    -camera.view.row(2).truncate().as_dvec3().normalize()
}

/// Where a ray from `eye` along `dir` (unit) first meets the ellipsoid
/// raised by `height_m`, as a distance; `None` if it does not.
fn ground_along(eye: DVec3, dir: DVec3, height_m: f64) -> Option<f64> {
    use tuile_core::geo::{WGS84_A, WGS84_B};
    let (a, b) = (WGS84_A + height_m, WGS84_B + height_m);
    // Scaled to the unit sphere, in f64: the eye is millions of metres from
    // the centre, the answer is wanted to the metre.
    let scale = DVec3::new(1.0 / a, 1.0 / a, 1.0 / b);
    let (o, v) = (eye * scale, dir * scale);
    let (qa, qb, qc) = (v.dot(v), 2.0 * o.dot(v), o.dot(o) - 1.0);
    let disc = qb * qb - 4.0 * qa * qc;
    if disc < 0.0 {
        return None;
    }
    let root = disc.sqrt();
    // The nearer of the two in front of the eye; the stable form of the
    // quadratic, so a ray almost tangent keeps its digits.
    let q = -0.5 * (qb + qb.signum() * root);
    let (t0, t1) = (q / qa, qc / q);
    [t0.min(t1), t0.max(t1)].into_iter().find(|t| *t > 0.0)
}

/// How far the horizon is from `eye`, over a sphere of the ellipsoid's
/// equatorial radius raised by `height_m`.
fn horizon_distance(eye: DVec3, height_m: f64) -> f64 {
    let r = tuile_core::geo::WGS84_A + height_m;
    (eye.length_squared() - r * r).max(0.0).sqrt()
}

impl FrameCamera {
    /// This frame's camera as `eye` of `pair` sees it: moved sideways by
    /// half the baseline, its window narrowed and shifted so that both eyes
    /// frame the same window at the convergence distance.
    pub fn eye(&self, pair: &EyePair, eye: Eye) -> FrameCamera {
        let right = self.view.row(0).truncate().as_dvec3().normalize();
        let side = eye.side();
        let zoom = (1.0 / pair.window) as f32;
        // A shift of the picture, in NDC: `x + shift·w` after the
        // projection. The scale narrows the window about its centre first.
        let shift = Mat4::from_translation(Vec3::new((side * pair.shift_ndc) as f32, 0.0, 0.0));
        let narrow = Mat4::from_scale(Vec3::new(zoom, zoom, 1.0));
        FrameCamera {
            eye: self.eye + right * (side * pair.baseline_m / 2.0),
            view: self.view,
            projection: shift * narrow * self.projection,
            near: self.near,
        }
    }
}

/// Two eyes' RGBA pictures, `eye_w × eye_h` each, packed into one.
pub fn pack_rgba(packing: Packing, left: &[u8], right: &[u8], eye_w: u32, eye_h: u32) -> Vec<u8> {
    pack_plane(packing, left, right, (eye_w * 4) as usize, eye_h as usize)
}

/// Two eyes' I420 pictures, `eye_w × eye_h` each, packed into one I420
/// picture: each plane packed as the picture is, the luma, then the two
/// chromas at half the size each way.
pub fn pack_i420(packing: Packing, left: &[u8], right: &[u8], eye_w: u32, eye_h: u32) -> Vec<u8> {
    let (w, h) = (eye_w as usize, eye_h as usize);
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let planes = [(0, w, h), (w * h, cw, ch), (w * h + cw * ch, cw, ch)];
    let mut out = Vec::with_capacity(left.len() * 2);
    for (start, row, rows) in planes {
        let end = start + row * rows;
        out.extend(pack_plane(
            packing,
            &left[start..end],
            &right[start..end],
            row,
            rows,
        ));
    }
    out
}

/// One plane of `rows` rows of `row` bytes from each eye, packed.
fn pack_plane(packing: Packing, left: &[u8], right: &[u8], row: usize, rows: usize) -> Vec<u8> {
    let (left, right) = (&left[..row * rows], &right[..row * rows]);
    match packing {
        Packing::TopBottom => [left, right].concat(),
        Packing::SideBySide => left
            .chunks_exact(row)
            .zip(right.chunks_exact(row))
            .flat_map(|(l, r)| l.iter().chain(r))
            .copied()
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::Vec4Swizzles;
    use tuile_pack::BakedView;

    /// 1500 m over the ellipsoid at null island, looking north and down 35°.
    fn looking_down(height: f64) -> BakedView {
        let pitch = 35f64.to_radians();
        BakedView {
            position: [tuile_core::geo::WGS84_A + height, 0.0, 0.0],
            direction: [-pitch.sin(), 0.0, pitch.cos()],
            up: [pitch.cos(), 0.0, pitch.sin()],
            viewport_px: [1920.0, 1080.0],
            fovy_rad: 0.8,
        }
    }

    const ASPECT: f32 = 16.0 / 9.0;

    fn rig(convergence: Convergence) -> (FrameCamera, Stereo, EyePair) {
        let centre = FrameCamera::of(&looking_down(1500.0), ASPECT);
        let stereo = Stereo {
            convergence,
            ..Stereo::default()
        };
        let pair = stereo.eyes(&centre);
        (centre, stereo, pair)
    }

    /// Where a world point lands in `camera`'s picture, NDC x and y.
    fn ndc(camera: &FrameCamera, at: DVec3) -> glam::Vec2 {
        let clip = camera.view_projection() * (at - camera.eye).as_vec3().extend(1.0);
        clip.xy() / clip.w
    }

    #[test]
    fn what_lies_at_the_convergence_is_on_the_screen_for_both_eyes() {
        let (centre, _, pair) = rig(Convergence::Distance(2000.0));
        let axis = forward(&centre);
        // On the axis, and off it: the whole plane of the screen.
        for (x, y) in [(0.0, 0.0), (0.5, 0.3), (-0.8, -0.6)] {
            let on_axis = centre.eye + axis * 2000.0;
            let target = ndc(&centre, on_axis);
            assert!(target.length() < 1e-4, "{target}");
            // A point of the convergence plane seen at (x, y) by the centre.
            let right = centre.view.row(0).truncate().as_dvec3();
            let up = centre.view.row(1).truncate().as_dvec3();
            let t = 1.0 / f64::from(centre.projection.x_axis.x);
            let tv = 1.0 / f64::from(centre.projection.y_axis.y);
            let p = on_axis + right * (x * 2000.0 * t) + up * (y * 2000.0 * tv);
            let (l, r) = (
                ndc(&centre.eye(&pair, Eye::Left), p),
                ndc(&centre.eye(&pair, Eye::Right), p),
            );
            assert!(
                (l - r).length() < 1e-4,
                "no parallax on the screen: {l} {r}"
            );
            // Narrowed by the window: the same point, a little further out.
            let expected = glam::Vec2::new(x as f32, y as f32) / pair.window as f32;
            assert!((l - expected).length() < 1e-4, "{l} vs {expected}");
        }
    }

    #[test]
    fn the_furthest_ground_goes_behind_the_screen_by_the_divergence_and_no_more() {
        let (centre, stereo, pair) = rig(Convergence::Distance(2000.0));
        let axis = forward(&centre);
        let mut last = f32::NEG_INFINITY;
        for depth in [2500.0, 5000.0, 20_000.0, 1e6, 1e9] {
            let p = centre.eye + axis * depth;
            let (l, r) = (
                ndc(&centre.eye(&pair, Eye::Left), p),
                ndc(&centre.eye(&pair, Eye::Right), p),
            );
            // Of a picture two NDC wide, as a fraction of it.
            let parallax = (r.x - l.x) / 2.0;
            assert!(parallax > 0.0, "behind the screen: right of the left eye's");
            assert!(parallax > last, "further is further behind");
            assert!(parallax <= stereo.divergence as f32 + 1e-5, "{parallax}");
            assert!((r.y - l.y).abs() < 1e-5, "never a vertical disparity");
            last = parallax;
        }
        assert!((last - stereo.divergence as f32).abs() < 1e-4, "{last}");
        // Nearer than the screen comes out of it.
        let near = centre.eye + axis * 1200.0;
        let crossed = ndc(&centre.eye(&pair, Eye::Right), near).x
            - ndc(&centre.eye(&pair, Eye::Left), near).x;
        assert!(crossed < 0.0, "{crossed}");
    }

    #[test]
    fn the_baseline_scales_with_the_distance_looked_at() {
        let (_, _, near) = rig(Convergence::Distance(1000.0));
        let (_, _, far) = rig(Convergence::Distance(4000.0));
        assert!((far.baseline_m / near.baseline_m - 4.0).abs() < 1e-9);
        // At two percent, a scene a kilometre away is seen from eyes some
        // thirty metres apart: hyperstereo, not 65 mm.
        assert!(
            (20.0..40.0).contains(&near.baseline_m),
            "{}",
            near.baseline_m
        );
        // The eyes are that far apart, along the camera's right axis.
        let (centre, _, pair) = rig(Convergence::Distance(1000.0));
        let (l, r) = (centre.eye(&pair, Eye::Left), centre.eye(&pair, Eye::Right));
        assert!(((r.eye - l.eye).length() - pair.baseline_m).abs() < 1e-6);
        assert!(
            ((r.eye + l.eye) / 2.0 - centre.eye).length() < 1e-6,
            "about the frame's eye"
        );
        let right = centre.view.row(0).truncate().as_dvec3();
        assert!((r.eye - l.eye).normalize().dot(right) > 1.0 - 1e-9);
    }

    /// The pack chose the frame's tiles for the centre camera: whatever an
    /// eye can see from half the convergence outwards, the centre saw too.
    #[test]
    fn an_eye_sees_nothing_the_frame_did_not_select() {
        let (centre, _, pair) = rig(Convergence::Distance(2000.0));
        let axis = forward(&centre);
        for eye in Eye::BOTH {
            let cam = centre.eye(&pair, eye);
            let inv = cam.view_projection().inverse();
            // The eye's four corners, cast to depths from C/2 to far away.
            for (x, y) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
                let far = inv * glam::Vec4::new(x, y, 1e-7, 1.0);
                let dir = (far.xyz() / far.w).as_dvec3().normalize();
                for depth in [1000.0, 1500.0, 2000.0, 8000.0, 1e6] {
                    let p = cam.eye + dir * (depth / dir.dot(axis));
                    let seen = ndc(&centre, p);
                    assert!(
                        seen.x.abs() <= 1.0 + 1e-5 && seen.y.abs() <= 1.0 + 1e-5,
                        "{eye:?} corner ({x}, {y}) at {depth} m lands at {seen} for the centre"
                    );
                }
            }
        }
    }

    /// Hyperstereo by the metre: the eyes as far apart as asked, whatever
    /// the distance; the screen still where it converges, and still nothing
    /// unselected seen.
    #[test]
    fn a_baseline_given_in_metres_is_the_baseline() {
        let centre = FrameCamera::of(&looking_down(1500.0), ASPECT);
        for convergence in [800.0, 2000.0, 6000.0] {
            let stereo = Stereo {
                convergence: Convergence::Distance(convergence),
                baseline_m: Some(10.0),
                ..Stereo::default()
            };
            stereo.check().expect("a baseline");
            let pair = stereo.eyes(&centre);
            let (l, r) = (centre.eye(&pair, Eye::Left), centre.eye(&pair, Eye::Right));
            assert!(
                ((r.eye - l.eye).length() - 10.0).abs() < 1e-6,
                "{convergence}"
            );
            let p = centre.eye + forward(&centre) * convergence;
            assert!((ndc(&l, p) - ndc(&r, p)).length() < 1e-4, "on the screen");
            // The furthest ground: as deep as the divergence it implies.
            let far = centre.eye + forward(&centre) * 1e9;
            let parallax = (ndc(&r, far).x - ndc(&l, far).x) / 2.0;
            assert!((f64::from(parallax) - pair.shift_ndc).abs() < 1e-4);
            let inv = l.view_projection().inverse();
            let corner = inv * glam::Vec4::new(1.0, 1.0, 1e-7, 1.0);
            let dir = (corner.xyz() / corner.w).as_dvec3().normalize();
            let seen = ndc(&centre, l.eye + dir * (1e6 / dir.dot(forward(&centre))));
            assert!(seen.x.abs() <= 1.0 + 1e-5, "{seen}");
        }
        for wild in [0.0, -1.0, f64::NAN, 2e4] {
            let s = Stereo {
                baseline_m: Some(wild),
                ..Stereo::default()
            };
            assert!(matches!(s.check(), Err(StereoError::Baseline(_))), "{wild}");
        }
    }

    #[test]
    fn the_screen_is_put_where_the_axis_meets_the_ground() {
        let (_, _, pair) = rig(Convergence::Ground { height_m: 0.0 });
        // 1500 m up, 35° down: the ground is 1500 / sin 35° away, near enough
        // for the curvature to be a metre or two.
        let flat = 1500.0 / 35f64.to_radians().sin();
        assert!(
            (pair.convergence_m - flat).abs() < 5.0,
            "{}",
            pair.convergence_m
        );
        // Ground raised 500 m: nearer.
        let (_, _, raised) = rig(Convergence::Ground { height_m: 500.0 });
        let flat = 1000.0 / 35f64.to_radians().sin();
        assert!(
            (raised.convergence_m - flat).abs() < 5.0,
            "{}",
            raised.convergence_m
        );
        // Looking above the horizon: the horizon.
        let mut sky = looking_down(1500.0);
        sky.direction = [0.0, 0.0, 1.0];
        sky.up = [1.0, 0.0, 0.0];
        let cam = FrameCamera::of(&sky, ASPECT);
        let pair = Stereo::default().eyes(&cam);
        let horizon = horizon_distance(cam.eye, 0.0);
        assert!((pair.convergence_m - horizon).abs() < 1e-6);
        assert!((130_000.0..140_000.0).contains(&horizon), "{horizon}");
    }

    #[test]
    fn a_spec_out_of_its_bounds_is_refused() {
        assert!(Stereo::default().check().is_ok());
        for d in [0.0, -0.01, 0.2, f64::NAN] {
            let s = Stereo {
                divergence: d,
                ..Stereo::default()
            };
            assert!(matches!(s.check(), Err(StereoError::Divergence(_))), "{d}");
        }
        let s = Stereo {
            convergence: Convergence::Distance(0.0),
            ..Stereo::default()
        };
        assert!(matches!(s.check(), Err(StereoError::Convergence(_))));
    }

    #[test]
    fn the_picture_holds_both_eyes_doubled_or_squeezed() {
        let sbs = Stereo::default();
        assert_eq!(sbs.eye_size(1920, 1080), (1920, 1080));
        assert_eq!(sbs.picture_size(sbs.eye_size(1920, 1080)), (3840, 1080));
        let half = Stereo { half: true, ..sbs };
        assert_eq!(half.eye_size(1920, 1080), (960, 1080));
        assert_eq!(half.picture_size(half.eye_size(1920, 1080)), (1920, 1080));
        let tb = Stereo {
            packing: Packing::TopBottom,
            half: true,
            ..sbs
        };
        assert_eq!(tb.eye_size(1920, 1080), (1920, 536));
        assert_eq!(tb.picture_size(tb.eye_size(1920, 1080)), (1920, 1072));
    }

    #[test]
    fn eyes_are_packed_row_by_row_side_by_side_and_one_above_the_other() {
        // Two 2×2 pictures, one byte a pixel for the sake of reading.
        let (l, r) = ([1u8, 2, 3, 4], [5u8, 6, 7, 8]);
        assert_eq!(
            pack_plane(Packing::SideBySide, &l, &r, 2, 2),
            [1, 2, 5, 6, 3, 4, 7, 8]
        );
        assert_eq!(
            pack_plane(Packing::TopBottom, &l, &r, 2, 2),
            [1, 2, 3, 4, 5, 6, 7, 8]
        );
        // RGBA: four bytes a pixel.
        let left = vec![10u8; 4 * 4 * 2];
        let right = vec![20u8; 4 * 4 * 2];
        let sbs = pack_rgba(Packing::SideBySide, &left, &right, 4, 2);
        assert_eq!(&sbs[..16], &[10; 16]);
        assert_eq!(&sbs[16..32], &[20; 16]);
        // I420 of 4×2 eyes: 8 luma, 2 + 2 chroma each.
        let left: Vec<u8> = (0..12).collect();
        let right: Vec<u8> = (100..112).collect();
        let packed = pack_i420(Packing::SideBySide, &left, &right, 4, 2);
        assert_eq!(packed.len(), 24);
        assert_eq!(&packed[..8], &[0, 1, 2, 3, 100, 101, 102, 103]);
        assert_eq!(&packed[16..20], &[8, 9, 108, 109], "U side by side");
        assert_eq!(&packed[20..], &[10, 11, 110, 111], "V side by side");
        let packed = pack_i420(Packing::TopBottom, &left, &right, 4, 2);
        assert_eq!(
            &packed[..16],
            &[0, 1, 2, 3, 4, 5, 6, 7, 100, 101, 102, 103, 104, 105, 106, 107]
        );
        assert_eq!(&packed[16..20], &[8, 9, 108, 109], "U one above the other");
    }
}
