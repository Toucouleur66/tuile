// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Where the camera will be: the views ahead an interactive host sends the
//! geometry server (`ClientMessage::Ahead`), so the tiles of where the view
//! is going load before it gets there.
//!
//! # Three kinds of future
//!
//! - **Known** — a glide, a replay, a coast after a fling: the camera is a
//!   function of time the host can evaluate ahead ([`Future::Known`]). Exact
//!   until a hand interrupts it: its probability decays as `e^(−τ/T_i)`,
//!   `T_i` = [`PredictConfig::interrupt_s`].
//! - **Interactive** — a hand is moving the camera: a motion model
//!   ([`MotionModel`]) extrapolates its state.
//! - **Still** — nothing moves: nothing ahead to send.
//!
//! # The motion model
//!
//! The camera's state is five channels: the eye's ground position east and
//! north (metres, accumulated from frame to frame so they never wrap), the
//! logarithm of its height, its heading (unwrapped) and its pitch, radians.
//! Each channel is a constant-velocity Kalman filter (state `[x, ẋ]`,
//! white-noise acceleration of spectral density `q`, measurement noise `r`),
//! in [`nalgebra`]'s 2×2 matrices:
//!
//! ```text
//! F(Δt) = [1 Δt; 0 1]     Q(Δt) = q·[Δt³/3 Δt²/2; Δt²/2 Δt]     H = [1 0]
//! predict:  x ← F x,  P ← F P Fᵀ + Q
//! update:   K = P Hᵀ (H P Hᵀ + r)⁻¹,  x ← x + K (z − H x),  P ← (I − K H) P
//! ```
//!
//! A view `τ` ahead is the filter's mean `x + ẋ·τ` on every channel, and its
//! spread `σ(τ)² = [1 τ] P [1 τ]ᵀ + q τ³/3` grows with the horizon. Its
//! probability is Laplace-like, `p(τ) = e^(−τ/λ)`, `λ` =
//! [`PredictConfig::laplace_s`]: with `λ = 1.9 s`, 90 % at 0.2 s and 59 % at
//! 1 s — the head-motion prediction accuracy measured for viewport-adaptive
//! streaming (90 % at 0.2 s, 58 % at 1 s).
//!
//! The ground channels are in metres, the height's in its logarithm: a zoom
//! is a constant rate of ratio, and a pan's speed scales with the height, so
//! `q` for the ground is given per metre of height
//! ([`PredictConfig::q_ground_per_height`]).
//!
//! # Determinism
//!
//! The filter is arithmetic over the poses it is given, in the order given:
//! the same poses at the same times give the same predictions, bit for bit,
//! on one target. What the server decides from them is reproducible on every
//! target because the predictions are an input of the step, recorded with
//! it (the ring tiles' `AheadUse`).
//!
//! # The reachable envelope
//!
//! However the hand moves, the camera cannot leave the envelope the fastest
//! gestures reach ([`Reach`]): a pan of [`PredictConfig::reach_heights_per_s`]
//! heights a second, a zoom out of [`PredictConfig::reach_zoom_out_per_s`].
//!
//! # Measured afterwards
//!
//! Every prediction the model makes while the camera moves is kept until
//! its time comes, and compared with the pose then ([`PredictionErrors`]):
//! the ground distance, the heading and pitch differences, the height's
//! ratio, by horizon.

use std::collections::VecDeque;

use nalgebra::{Matrix2, RowVector2, Vector2};
use tuile_core::geo::{ecef_to_geodetic, enu_frame, geodetic_to_ecef, Geodetic};

use crate::GlobeCamera;

/// The Earth's mean radius, metres: what turns an angle into a ground
/// distance for the errors.
const EARTH_RADIUS: f64 = 6_371_000.0;

/// The prediction's numbers. Empty `horizons_s` is no prediction: the host
/// sends no views ahead.
#[derive(Debug, Clone, PartialEq)]
pub struct PredictConfig {
    /// The horizons the views ahead are sampled at, seconds.
    pub horizons_s: Vec<f64>,
    /// The Laplace decay of an interactive prediction's probability, `λ`.
    pub laplace_s: f64,
    /// How long a hand's rate lasts, `τ_v`: the rate decays as `e^(−t/τ_v)`
    /// (a Singer model — a hand slows and stops), so a view `τ` ahead
    /// travels `ẋ·τ_v·(1 − e^(−τ/τ_v))`. Infinite: a constant rate.
    pub rate_decay_s: f64,
    /// The decay of a known future's probability, `T_i`: how long a glide or
    /// a coast goes, on average, before a hand interrupts it.
    pub interrupt_s: f64,
    /// White-noise acceleration of the ground channels, (m/s²)²·s per metre
    /// of height squared.
    pub q_ground_per_height: f64,
    /// White-noise acceleration of the log-height channel.
    pub q_log_height: f64,
    /// White-noise acceleration of the heading and pitch channels.
    pub q_angle: f64,
    /// Measurement noise of a pose, as a fraction of the height for the
    /// ground, in the logarithm for the height, radians for the angles
    /// (variances below are their squares).
    pub r_ground_per_height: f64,
    pub r_log_height: f64,
    pub r_angle: f64,
    /// The least motion that counts as moving: a pan in heights per second,
    /// a zoom in log-height per second, a turn or tilt in radians per
    /// second.
    pub moving_heights_per_s: f64,
    pub moving_log_height_per_s: f64,
    pub moving_angle_per_s: f64,
    /// The fastest pan, heights per second, and the fastest zoom out, ratio
    /// per second: the reachable envelope.
    pub reach_heights_per_s: f64,
    pub reach_zoom_out_per_s: f64,
}

impl Default for PredictConfig {
    fn default() -> Self {
        PredictConfig {
            horizons_s: vec![0.25, 0.5, 1.0, 2.0],
            laplace_s: 1.9,
            rate_decay_s: f64::INFINITY,
            interrupt_s: 6.0,
            q_ground_per_height: 0.5,
            q_log_height: 0.5,
            q_angle: 0.5,
            r_ground_per_height: 1e-3,
            r_log_height: 1e-3,
            r_angle: 1e-3,
            moving_heights_per_s: 0.01,
            moving_log_height_per_s: 0.01,
            moving_angle_per_s: 0.005,
            reach_heights_per_s: 3.0,
            reach_zoom_out_per_s: 4.0,
        }
    }
}

/// A camera's pose in the model's terms: radians, metres.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pose {
    pub lat: f64,
    pub lon: f64,
    /// Height above the ellipsoid, metres.
    pub height: f64,
    /// Clockwise from north.
    pub heading: f64,
    /// Below the horizon.
    pub pitch: f64,
}

impl Pose {
    /// The pose of `camera`.
    pub fn of(camera: &GlobeCamera) -> Pose {
        let g = ecef_to_geodetic(camera.position);
        Pose {
            lat: g.lat,
            lon: g.lon,
            height: g.height,
            heading: camera.heading(),
            pitch: camera.pitch(),
        }
    }

    /// The camera at this pose, with `fovy`.
    pub fn camera(&self, fovy: f64) -> GlobeCamera {
        GlobeCamera::from_geodetic(
            self.lat,
            self.lon,
            self.height,
            self.heading,
            self.pitch,
            fovy,
        )
    }

    /// How far this pose is from `other`: ground km, heading and pitch in
    /// degrees, the heights' ratio (≥ 1).
    pub fn error_to(&self, other: &Pose) -> PoseError {
        let (dlat, dlon) = (other.lat - self.lat, wrap_pi(other.lon - self.lon));
        let a = (dlat / 2.0).sin().powi(2)
            + self.lat.cos() * other.lat.cos() * (dlon / 2.0).sin().powi(2);
        let ground_km = 2.0 * EARTH_RADIUS * a.sqrt().min(1.0).asin() / 1000.0;
        let ratio = (self.height.max(1.0) / other.height.max(1.0))
            .ln()
            .abs()
            .exp();
        PoseError {
            ground_km,
            heading_deg: wrap_pi(other.heading - self.heading).abs().to_degrees(),
            pitch_deg: (other.pitch - self.pitch).abs().to_degrees(),
            height_ratio: ratio,
        }
    }
}

/// How far a predicted pose was from the pose that came.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PoseError {
    pub ground_km: f64,
    pub heading_deg: f64,
    pub pitch_deg: f64,
    /// `max(a/b, b/a)` of the heights: 1 is exact.
    pub height_ratio: f64,
}

/// An angle folded into `(−π, π]`.
fn wrap_pi(a: f64) -> f64 {
    let t = std::f64::consts::TAU;
    let r = (a + std::f64::consts::PI).rem_euclid(t) - std::f64::consts::PI;
    if r <= -std::f64::consts::PI {
        r + t
    } else {
        r
    }
}

/// One channel's constant-velocity Kalman filter. See the module.
#[derive(Debug, Clone, Copy)]
struct Channel {
    x: Vector2<f64>,
    p: Matrix2<f64>,
}

impl Channel {
    fn new(z: f64, r: f64) -> Self {
        Channel {
            x: Vector2::new(z, 0.0),
            // Known to the measurement; the rate unknown, of the order of a
            // fast gesture.
            p: Matrix2::new(r, 0.0, 0.0, 1.0),
        }
    }

    fn q(dt: f64, q: f64) -> Matrix2<f64> {
        let (d2, d3) = (dt * dt, dt * dt * dt);
        Matrix2::new(d3 / 3.0, d2 / 2.0, d2 / 2.0, dt) * q
    }

    /// How far a unit rate carries the channel in `dt`, and what is left of
    /// the rate then, under a rate that decays with time constant `tau_v`
    /// (∞: a constant rate).
    fn carry(dt: f64, tau_v: f64) -> (f64, f64) {
        if tau_v.is_finite() && tau_v > 0.0 {
            let e = (-dt / tau_v).exp();
            (tau_v * (1.0 - e), e)
        } else {
            (dt, 1.0)
        }
    }

    fn predict(&mut self, dt: f64, q: f64, tau_v: f64) {
        let (a, e) = Self::carry(dt, tau_v);
        let f = Matrix2::new(1.0, a, 0.0, e);
        self.x = f * self.x;
        self.p = f * self.p * f.transpose() + Self::q(dt, q);
    }

    fn update(&mut self, z: f64, r: f64) {
        let h = RowVector2::new(1.0, 0.0);
        let s = (h * self.p * h.transpose())[(0, 0)] + r;
        if s <= 0.0 || !s.is_finite() {
            return;
        }
        let k = self.p * h.transpose() / s;
        let innovation = z - self.x[0];
        self.x += k * innovation;
        self.p = (Matrix2::identity() - k * h) * self.p;
    }

    /// The mean and the variance `τ` ahead.
    fn ahead(&self, tau: f64, q: f64, tau_v: f64) -> (f64, f64) {
        let (a, _) = Self::carry(tau, tau_v);
        let g = RowVector2::new(1.0, a);
        let var = (g * self.p * g.transpose())[(0, 0)] + q * tau * tau * tau / 3.0;
        (self.x[0] + self.x[1] * a, var.max(0.0))
    }

    fn rate(&self) -> f64 {
        self.x[1]
    }
}

/// The channels, in order.
const EAST: usize = 0;
const NORTH: usize = 1;
const LOG_HEIGHT: usize = 2;
const HEADING: usize = 3;
const PITCH: usize = 4;

/// The camera's motion model. See the module.
#[derive(Debug, Clone)]
pub struct MotionModel {
    config: PredictConfig,
    channels: Option<[Channel; 5]>,
    /// The last pose taken in, its time, and its channels' values (the
    /// accumulated ground position and the unwrapped heading).
    last: Option<(f64, Pose, [f64; 5])>,
}

impl MotionModel {
    pub fn new(config: PredictConfig) -> Self {
        MotionModel {
            config,
            channels: None,
            last: None,
        }
    }

    pub fn config(&self) -> &PredictConfig {
        &self.config
    }

    /// The process noise of each channel at `height`.
    fn qs(&self, height: f64) -> [f64; 5] {
        let c = &self.config;
        let g = c.q_ground_per_height * height.max(1.0) * height.max(1.0);
        [g, g, c.q_log_height, c.q_angle, c.q_angle]
    }

    fn rs(&self, height: f64) -> [f64; 5] {
        let c = &self.config;
        let g = (c.r_ground_per_height * height.max(1.0)).powi(2);
        [
            g,
            g,
            c.r_log_height.powi(2),
            c.r_angle.powi(2),
            c.r_angle.powi(2),
        ]
    }

    /// Takes in the pose at time `t`, seconds. A time not after the last is
    /// ignored; a gap of over a second starts the filter again (the rates
    /// before it say nothing about the motion after).
    pub fn observe(&mut self, t: f64, pose: Pose) {
        let z = match self.last {
            None => [
                0.0,
                0.0,
                pose.height.max(1.0).ln(),
                pose.heading,
                pose.pitch,
            ],
            Some((t0, p0, z0)) => {
                if t <= t0 {
                    return;
                }
                if t - t0 > 1.0 {
                    self.channels = None;
                }
                let mid = 0.5 * (p0.lat + pose.lat);
                [
                    z0[EAST] + wrap_pi(pose.lon - p0.lon) * EARTH_RADIUS * mid.cos(),
                    z0[NORTH] + (pose.lat - p0.lat) * EARTH_RADIUS,
                    pose.height.max(1.0).ln(),
                    z0[HEADING] + wrap_pi(pose.heading - p0.heading),
                    pose.pitch,
                ]
            }
        };
        self.take(t, z, pose);
    }

    /// Takes in the channels' values `z` at time `t` directly — ground east
    /// and north in metres (continuous, never wrapped), the height's
    /// logarithm, heading (unwrapped) and pitch in radians: for a host whose
    /// camera is not on the globe (a simulation's flat world).
    pub fn observe_state(&mut self, t: f64, z: [f64; 5]) {
        if let Some((t0, _, _)) = self.last {
            if t <= t0 {
                return;
            }
            if t - t0 > 1.0 {
                self.channels = None;
            }
        }
        let pose = Pose {
            lat: 0.0,
            lon: 0.0,
            height: z[LOG_HEIGHT].exp(),
            heading: z[HEADING],
            pitch: z[PITCH],
        };
        self.take(t, z, pose);
    }

    /// The channels' means and standard deviations `tau` after the last
    /// values taken in ([`Self::observe_state`]'s terms).
    pub fn predict_state(&self, tau: f64) -> Option<([f64; 5], [f64; 5])> {
        let (ch, (_, pose, _)) = (self.channels.as_ref()?, self.last?);
        let qs = self.qs(pose.height);
        let mut mean = [0.0; 5];
        let mut sigma = [0.0; 5];
        for i in 0..5 {
            let (m, v) = ch[i].ahead(tau, qs[i], self.config.rate_decay_s);
            mean[i] = m;
            sigma[i] = v.sqrt();
        }
        Some((mean, sigma))
    }

    fn take(&mut self, t: f64, z: [f64; 5], pose: Pose) {
        let (qs, rs) = (self.qs(pose.height), self.rs(pose.height));
        match (&mut self.channels, self.last) {
            (Some(ch), Some((t0, _, _))) => {
                let dt = t - t0;
                for i in 0..5 {
                    ch[i].predict(dt, qs[i], self.config.rate_decay_s);
                    ch[i].update(z[i], rs[i]);
                }
            }
            _ => {
                self.channels = Some(std::array::from_fn(|i| Channel::new(z[i], rs[i])));
            }
        }
        self.last = Some((t, pose, z));
    }

    /// Forgets the motion: the next pose starts the filter again (a jump, a
    /// host taking the camera).
    pub fn reset(&mut self) {
        self.channels = None;
        self.last = None;
    }

    /// Whether the camera is moving, by the filter's rates.
    pub fn moving(&self) -> bool {
        let (Some(ch), Some((_, pose, _))) = (&self.channels, self.last) else {
            return false;
        };
        let c = &self.config;
        let h = pose.height.max(1.0);
        let ground = ch[EAST].rate().hypot(ch[NORTH].rate()) / h;
        ground > c.moving_heights_per_s
            || ch[LOG_HEIGHT].rate().abs() > c.moving_log_height_per_s
            || ch[HEADING].rate().abs() > c.moving_angle_per_s
            || ch[PITCH].rate().abs() > c.moving_angle_per_s
    }

    /// The pose `tau` seconds after the last one taken in, and the standard
    /// deviation of each channel there (east and north metres, log-height,
    /// heading and pitch radians). `None` before any pose.
    pub fn predict(&self, tau: f64) -> Option<(Pose, [f64; 5])> {
        let (_, pose, z) = self.last?;
        let (mean, sigma) = self.predict_state(tau)?;
        // The ground offset from the last pose, back to an angle there.
        let (de, dn) = (mean[EAST] - z[EAST], mean[NORTH] - z[NORTH]);
        let lat = (pose.lat + dn / EARTH_RADIUS).clamp(
            -std::f64::consts::FRAC_PI_2 + 1e-9,
            std::f64::consts::FRAC_PI_2 - 1e-9,
        );
        let lon = wrap_pi(pose.lon + de / (EARTH_RADIUS * lat.cos().max(1e-9)));
        Some((
            Pose {
                lat,
                lon,
                height: mean[LOG_HEIGHT].exp(),
                heading: mean[HEADING].rem_euclid(std::f64::consts::TAU),
                pitch: mean[PITCH],
            },
            sigma,
        ))
    }

    /// The envelope the fastest gestures reach from the last pose.
    pub fn reach(&self) -> Reach {
        let h = self.last.map_or(0.0, |(_, p, _)| p.height.max(1.0));
        Reach {
            ground_mps: self.config.reach_heights_per_s * h,
            zoom_out_per_s: self.config.reach_zoom_out_per_s,
        }
    }
}

/// How far the fastest gestures carry the camera.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Reach {
    /// Metres per second over the ground.
    pub ground_mps: f64,
    /// Height ratio per second.
    pub zoom_out_per_s: f64,
}

/// What the host knows of the camera's future this frame.
pub enum Future<'a> {
    /// A function of the time ahead, seconds, interruptible by a hand: a
    /// glide, a coast. `None` past its end.
    Known(&'a dyn Fn(f64) -> Option<GlobeCamera>),
    /// A function of the time ahead nothing interrupts: a replay.
    Certain(&'a dyn Fn(f64) -> Option<GlobeCamera>),
    /// A hand, or nothing: the motion model's.
    Interactive,
}

/// One view ahead.
#[derive(Debug, Clone, Copy)]
pub struct Ahead {
    /// When, seconds from now.
    pub in_s: f64,
    pub camera: GlobeCamera,
    /// How likely, thousandths.
    pub p_permille: u16,
}

/// What kind of future the views ahead came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Source {
    /// Nothing moves: no view ahead.
    #[default]
    Still,
    Known,
    Certain,
    Interactive,
}

/// How many buckets an error histogram has.
pub const BUCKETS: usize = 12;

/// An error histogram's bucket bounds: the upper bound of each but the last
/// (open), doubling from `first`.
fn bucket(v: f64, first: f64) -> usize {
    let mut bound = first;
    for b in 0..BUCKETS - 1 {
        if v <= bound {
            return b;
        }
        bound *= 2.0;
    }
    BUCKETS - 1
}

/// One measure's errors at one horizon, as a histogram of doubling buckets.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Histogram {
    /// The first bucket's upper bound.
    pub first: f64,
    pub counts: [u64; BUCKETS],
}

impl Histogram {
    fn new(first: f64) -> Self {
        Histogram {
            first,
            counts: [0; BUCKETS],
        }
    }

    fn take(&mut self, v: f64) {
        if v.is_finite() {
            self.counts[bucket(v, self.first)] += 1;
        }
    }

    /// How many errors it holds.
    pub fn count(&self) -> u64 {
        self.counts.iter().sum()
    }

    /// The upper bound of the bucket the `q` quantile falls in (`∞` in the
    /// open one; `None` when empty).
    pub fn quantile(&self, q: f64) -> Option<f64> {
        let n = self.count();
        if n == 0 {
            return None;
        }
        let want = ((n as f64) * q).ceil().max(1.0) as u64;
        let mut seen = 0;
        let mut bound = self.first;
        for (b, c) in self.counts.iter().enumerate() {
            seen += c;
            if seen >= want {
                return Some(if b == BUCKETS - 1 {
                    f64::INFINITY
                } else {
                    bound
                });
            }
            bound *= 2.0;
        }
        Some(f64::INFINITY)
    }
}

/// The errors at one horizon.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HorizonErrors {
    pub horizon_s: f64,
    /// Ground distance, km (buckets from 10 m).
    pub ground_km: Histogram,
    /// Heading, degrees (from 0.25°).
    pub heading_deg: Histogram,
    /// Pitch, degrees (from 0.25°).
    pub pitch_deg: Histogram,
    /// The heights' ratio minus one (from 0.5 %).
    pub height_ratio: Histogram,
}

impl HorizonErrors {
    fn new(horizon_s: f64) -> Self {
        HorizonErrors {
            horizon_s,
            ground_km: Histogram::new(0.01),
            heading_deg: Histogram::new(0.25),
            pitch_deg: Histogram::new(0.25),
            height_ratio: Histogram::new(0.005),
        }
    }

    fn take(&mut self, e: &PoseError) {
        self.ground_km.take(e.ground_km);
        self.heading_deg.take(e.heading_deg);
        self.pitch_deg.take(e.pitch_deg);
        self.height_ratio.take(e.height_ratio - 1.0);
    }
}

/// Every horizon's errors, measured once their time came.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PredictionErrors {
    pub horizons: Vec<HorizonErrors>,
}

/// The predictor a host runs once a frame: the motion model fed with the
/// camera, the views ahead of the frame's future, and the errors measured.
#[derive(Debug, Clone)]
pub struct Predictor {
    model: MotionModel,
    /// The model's predictions waiting for their time: when, which horizon,
    /// the pose predicted.
    pending: VecDeque<(f64, usize, Pose)>,
    errors: PredictionErrors,
    source: Source,
}

impl Predictor {
    pub fn new(config: PredictConfig) -> Self {
        let errors = PredictionErrors {
            horizons: config
                .horizons_s
                .iter()
                .map(|h| HorizonErrors::new(*h))
                .collect(),
        };
        Predictor {
            model: MotionModel::new(config),
            pending: VecDeque::new(),
            errors,
            source: Source::Still,
        }
    }

    pub fn config(&self) -> &PredictConfig {
        self.model.config()
    }

    pub fn model(&self) -> &MotionModel {
        &self.model
    }

    /// The errors measured so far.
    pub fn errors(&self) -> &PredictionErrors {
        &self.errors
    }

    /// Where the last views ahead came from.
    pub fn source(&self) -> Source {
        self.source
    }

    /// Forgets the motion (a jump): the errors measured stay.
    pub fn reset(&mut self) {
        self.model.reset();
        self.pending.clear();
    }

    /// The frame at `t` seconds, its camera drawn as `camera`, its future
    /// `future`: the model fed and its due predictions scored, and the
    /// views ahead returned — empty when nothing moves or no horizon is
    /// configured.
    pub fn frame(&mut self, t: f64, camera: &GlobeCamera, future: Future<'_>) -> Vec<Ahead> {
        let pose = Pose::of(camera);
        self.model.observe(t, pose);
        while let Some(&(due, h, predicted)) = self.pending.front() {
            if due > t {
                break;
            }
            self.pending.pop_front();
            if let Some(e) = self.errors.horizons.get_mut(h) {
                e.take(&predicted.error_to(&pose));
            }
        }
        let moving = self.model.moving();
        // The model's own predictions are scored whatever the frame sends,
        // while the camera moves: what it would have said.
        if moving {
            for (h, tau) in self.model.config.horizons_s.iter().enumerate() {
                if let Some((p, _)) = self.model.predict(*tau) {
                    self.pending.push_back((t + tau, h, p));
                }
            }
            // Kept in time order for the scoring above.
            self.pending
                .make_contiguous()
                .sort_by(|a, b| a.0.total_cmp(&b.0));
        }
        let horizons = self.model.config.horizons_s.clone();
        let (interrupt, laplace) = (self.model.config.interrupt_s, self.model.config.laplace_s);
        let decay = |tau: f64, scale: f64| -> u16 {
            if scale <= 0.0 {
                return 0;
            }
            ((-tau / scale).exp() * 1000.0).round().clamp(0.0, 1000.0) as u16
        };
        let mut out = Vec::new();
        match future {
            Future::Known(f) | Future::Certain(f) => {
                let certain = matches!(future, Future::Certain(_));
                self.source = if certain {
                    Source::Certain
                } else {
                    Source::Known
                };
                for tau in horizons {
                    if let Some(c) = f(tau) {
                        out.push(Ahead {
                            in_s: tau,
                            camera: c,
                            p_permille: if certain { 1000 } else { decay(tau, interrupt) },
                        });
                    }
                }
            }
            Future::Interactive if moving => {
                self.source = Source::Interactive;
                for tau in horizons {
                    if let Some((p, _)) = self.model.predict(tau) {
                        out.push(Ahead {
                            in_s: tau,
                            camera: p.camera(camera.fovy),
                            p_permille: decay(tau, laplace),
                        });
                    }
                }
            }
            Future::Interactive => self.source = Source::Still,
        }
        out
    }

    /// The reachable envelope from the last pose.
    pub fn reach(&self) -> Reach {
        self.model.reach()
    }
}

/// The ground point `metres` east and north of `g`, as a geodetic: for a
/// test's path.
pub fn offset(g: Geodetic, east: f64, north: f64) -> Geodetic {
    let f = enu_frame(g);
    let p = geodetic_to_ecef(g) + f.col(0) * east + f.col(1) * north;
    let mut out = ecef_to_geodetic(p);
    out.height = g.height;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pose_at(t: f64, rate: [f64; 5]) -> Pose {
        // A constant-rate motion: east and north in metres per second, the
        // height's log-rate, heading and pitch rates.
        let start = Geodetic {
            lon: 6.86_f64.to_radians(),
            lat: 45.83_f64.to_radians(),
            height: 20_000.0,
        };
        let g = offset(start, rate[0] * t, rate[1] * t);
        Pose {
            lat: g.lat,
            lon: g.lon,
            height: 20_000.0 * (rate[2] * t).exp(),
            heading: (0.3 + rate[3] * t).rem_euclid(std::f64::consts::TAU),
            pitch: 0.8 + rate[4] * t,
        }
    }

    fn fed(rate: [f64; 5], until: f64) -> MotionModel {
        let mut m = MotionModel::new(PredictConfig::default());
        let mut t = 0.0;
        while t <= until + 1e-9 {
            m.observe(t, pose_at(t, rate));
            t += 1.0 / 60.0;
        }
        m
    }

    /// A rotation at a constant rate is predicted a second ahead within a
    /// degree; a zoom out at a constant rate within 2 % of the height.
    #[test]
    fn a_constant_rotation_and_zoom_out_are_predicted_at_one_second() {
        let rate = [0.0, 0.0, 0.7, 0.5, 0.0];
        let m = fed(rate, 1.0);
        let (p, _) = m.predict(1.0).expect("a prediction");
        let truth = pose_at(2.0, rate);
        let e = p.error_to(&truth);
        assert!(e.heading_deg < 1.0, "{e:?}");
        assert!(e.height_ratio < 1.02, "{e:?}");
        assert!(e.ground_km < 0.1, "{e:?}");
        assert!(m.moving());
    }

    /// A pan at a constant speed is predicted a second ahead within a
    /// hundredth of its travel.
    #[test]
    fn a_constant_pan_is_predicted_at_one_second() {
        let rate = [3_000.0, -1_500.0, 0.0, 0.0, 0.0];
        let m = fed(rate, 1.0);
        let (p, _) = m.predict(1.0).expect("a prediction");
        let e = p.error_to(&pose_at(2.0, rate));
        assert!(e.ground_km < 0.034, "{e:?}");
    }

    /// The spread grows with the horizon on every channel.
    #[test]
    fn the_uncertainty_grows_with_the_horizon() {
        let m = fed([1000.0, 0.0, 0.2, 0.1, 0.05], 0.5);
        let mut before = [0.0; 5];
        for tau in [0.25, 0.5, 1.0, 2.0] {
            let (_, s) = m.predict(tau).expect("a prediction");
            for i in 0..5 {
                assert!(
                    s[i] > before[i],
                    "channel {i} at {tau}: {} ≤ {}",
                    s[i],
                    before[i]
                );
            }
            before = s;
        }
    }

    /// A hand that changes its motion: the measured error grows with the
    /// horizon.
    #[test]
    fn the_measured_error_grows_with_the_horizon() {
        let mut p = Predictor::new(PredictConfig::default());
        let cam = |t: f64| {
            // A pan that turns and speeds up and slows down, and a zoom that
            // reverses: what a hand does.
            let rate = [
                2_000.0 * (0.9 * t).cos(),
                2_000.0 * (0.7 * t).sin(),
                0.4 * (0.5 * t).sin(),
                0.3 * (1.1 * t).cos(),
                0.0,
            ];
            pose_at(t, rate).camera(0.6)
        };
        let mut t = 0.0;
        while t < 20.0 {
            p.frame(t, &cam(t), Future::Interactive);
            t += 1.0 / 60.0;
        }
        let medians: Vec<f64> = p
            .errors()
            .horizons
            .iter()
            .map(|h| h.ground_km.quantile(0.5).expect("measured"))
            .collect();
        for w in medians.windows(2) {
            assert!(w[1] >= w[0], "{medians:?}");
        }
        assert!(medians[3] > medians[0], "{medians:?}");
    }

    /// The same poses give the same predictions, bit for bit.
    #[test]
    fn the_same_poses_give_the_same_predictions() {
        let run = || {
            let mut p = Predictor::new(PredictConfig::default());
            let mut out = Vec::new();
            let mut t = 0.0;
            while t < 3.0 {
                let c = pose_at(t, [800.0, 200.0, 0.3, 0.2, 0.01]).camera(0.6);
                for a in p.frame(t, &c, Future::Interactive) {
                    out.push((
                        a.in_s.to_bits(),
                        a.p_permille,
                        a.camera.position.to_array().map(f64::to_bits),
                    ));
                }
                t += 1.0 / 60.0;
            }
            out
        };
        let (a, b) = (run(), run());
        assert!(!a.is_empty());
        assert_eq!(a, b);
    }

    /// Nothing moves, nothing ahead; no horizons, nothing ahead.
    #[test]
    fn a_still_camera_and_an_empty_configuration_send_nothing() {
        let mut p = Predictor::new(PredictConfig::default());
        let c = pose_at(0.0, [0.0; 5]).camera(0.6);
        for i in 0..60 {
            assert!(p
                .frame(f64::from(i) / 60.0, &c, Future::Interactive)
                .is_empty());
        }
        let mut none = Predictor::new(PredictConfig {
            horizons_s: Vec::new(),
            ..PredictConfig::default()
        });
        let mut t = 0.0;
        while t < 1.0 {
            let c = pose_at(t, [800.0, 0.0, 0.0, 0.0, 0.0]).camera(0.6);
            assert!(none.frame(t, &c, Future::Interactive).is_empty());
            t += 1.0 / 60.0;
        }
    }

    /// The probabilities: Laplace for a hand, the interruption's decay for
    /// a known future, certain for a replay.
    #[test]
    fn the_probabilities_decay_with_the_horizon() {
        let mut p = Predictor::new(PredictConfig::default());
        let mut t = 0.0;
        let mut last = Vec::new();
        while t < 1.0 {
            let c = pose_at(t, [800.0, 0.0, 0.0, 0.0, 0.0]).camera(0.6);
            last = p.frame(t, &c, Future::Interactive);
            t += 1.0 / 60.0;
        }
        let ps: Vec<u16> = last.iter().map(|a| a.p_permille).collect();
        // e^(−τ/1.9): 877, 769, 591, 349.
        assert_eq!(ps, vec![877, 769, 591, 349]);
        let still = pose_at(0.0, [0.0; 5]).camera(0.6);
        let f = |_: f64| Some(still);
        let known: Vec<u16> = p
            .frame(t, &still, Future::Known(&f))
            .iter()
            .map(|a| a.p_permille)
            .collect();
        assert_eq!(known, vec![959, 920, 846, 717]);
        let certain: Vec<u16> = p
            .frame(t + 0.1, &still, Future::Certain(&f))
            .iter()
            .map(|a| a.p_permille)
            .collect();
        assert_eq!(certain, vec![1000; 4]);
    }
}
