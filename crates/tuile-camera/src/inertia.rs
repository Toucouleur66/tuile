// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A gesture's momentum: what the camera does after a hand lets go.
//!
//! While a hand holds the globe, every gesture the controller answers is
//! summed per frame and turned into a rate — a spin about the geocentre for
//! a drag, radians per second of heading and tilt, wheel units per second of
//! zoom — smoothed over [`RATE_SMOOTHING_S`]. When the hand lets go while
//! still moving, that rate becomes the momentum, and the camera coasts on
//! it, the momentum decaying as `e^(−t/τ)` with `τ` = [`Inertia::coast_s`]:
//! a fling travels `v·τ` in all, and stops by itself.
//!
//! The coast is a function of the controller's state and the time alone, so
//! where the camera will be is known while it coasts
//! ([`crate::CameraController::coast_ahead`]): the views ahead a prediction
//! sends are the frames the controller will draw.
//!
//! `τ = 0` is no momentum: a number, not a switch.

use glam::DVec3;

/// How long a hand's rate is smoothed over, seconds: a few frames, so one
/// uneven frame does not decide a fling.
pub const RATE_SMOOTHING_S: f64 = 0.05;

/// A hand that stood still this long before letting go flings nothing,
/// seconds.
pub const STILL_BEFORE_RELEASE_S: f64 = 0.1;

/// The default coast time constant, seconds.
pub const DEFAULT_COAST_S: f64 = 0.35;

/// Rates of the four gestures, or their amounts over one frame.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Rates {
    /// A drag: the rotation of the globe about its centre, as an axis
    /// scaled by the angle (radians, or radians per second).
    pub spin: DVec3,
    /// A turn of the heading about the centre of interest.
    pub heading: f64,
    /// A tilt about the centre of interest.
    pub tilt: f64,
    /// A zoom, in wheel units (positive in).
    pub zoom: f64,
}

impl Rates {
    fn scaled(self, k: f64) -> Rates {
        Rates {
            spin: self.spin * k,
            heading: self.heading * k,
            tilt: self.tilt * k,
            zoom: self.zoom * k,
        }
    }

    fn plus(self, o: Rates) -> Rates {
        Rates {
            spin: self.spin + o.spin,
            heading: self.heading + o.heading,
            tilt: self.tilt + o.tilt,
            zoom: self.zoom + o.zoom,
        }
    }

    /// Whether every rate is under the smallest that still moves the view.
    pub fn is_still(&self) -> bool {
        /// Radians per second (a thousandth of a radian a second).
        const ANGLE: f64 = 1e-3;
        /// Wheel units per second.
        const ZOOM: f64 = 1e-2;
        self.spin.length() < ANGLE * 1e-3
            && self.heading.abs() < ANGLE
            && self.tilt.abs() < ANGLE
            && self.zoom.abs() < ZOOM
    }
}

/// The momentum state of one controller. See the module.
#[derive(Debug, Clone, Copy)]
pub struct Inertia {
    /// The coast's time constant, seconds; 0 is no momentum.
    pub coast_s: f64,
    held: bool,
    /// The gestures answered since the last frame.
    stroke: Rates,
    /// The hand's smoothed rate.
    velocity: Rates,
    /// How long the hand has not moved.
    quiet_s: f64,
    /// What the camera coasts on.
    momentum: Rates,
    /// The viewport of the last gesture: where the coast's turns pivot.
    pub(crate) viewport: (f64, f64),
}

impl Default for Inertia {
    fn default() -> Self {
        Inertia {
            coast_s: DEFAULT_COAST_S,
            held: false,
            stroke: Rates::default(),
            velocity: Rates::default(),
            quiet_s: 0.0,
            momentum: Rates::default(),
            viewport: (1.0, 1.0),
        }
    }
}

impl Inertia {
    /// A hand is on the globe: any coast stops.
    pub fn press(&mut self) {
        self.held = true;
        self.stroke = Rates::default();
        self.velocity = Rates::default();
        self.momentum = Rates::default();
        self.quiet_s = 0.0;
    }

    /// The hand let go: the camera coasts on its rate, unless it stood still.
    pub fn let_go(&mut self) {
        if !self.held {
            return;
        }
        self.held = false;
        self.momentum = if self.quiet_s < STILL_BEFORE_RELEASE_S && self.coast_s > 0.0 {
            self.velocity
        } else {
            Rates::default()
        };
        self.velocity = Rates::default();
    }

    /// Stops any coast, as a jump or a host's drive does.
    pub fn halt(&mut self) {
        self.momentum = Rates::default();
        self.velocity = Rates::default();
        self.stroke = Rates::default();
    }

    /// Whether a hand is on the globe.
    pub fn is_held(&self) -> bool {
        self.held
    }

    /// Whether the camera is coasting.
    pub fn is_coasting(&self) -> bool {
        !self.held && !self.momentum.is_still()
    }

    /// The momentum the camera coasts on now.
    pub fn momentum(&self) -> Rates {
        self.momentum
    }

    /// The hand's smoothed rate now.
    pub fn velocity(&self) -> Rates {
        self.velocity
    }

    /// A gesture answered: summed into this frame's stroke.
    pub(crate) fn stroke(&mut self, r: Rates) {
        self.stroke = self.stroke.plus(r);
    }

    /// One frame of `dt` seconds: the hand's rate taken in, or the step the
    /// coast makes — returned for the controller to apply — and the momentum
    /// decayed.
    pub(crate) fn frame(&mut self, dt: f64) -> Option<Rates> {
        let stroke = std::mem::take(&mut self.stroke);
        if dt <= 0.0 {
            return None;
        }
        if self.held {
            let k = 1.0 - (-dt / RATE_SMOOTHING_S).exp();
            let now = stroke.scaled(1.0 / dt);
            self.velocity = self
                .velocity
                .plus(now.plus(self.velocity.scaled(-1.0)).scaled(k));
            if stroke.is_still() {
                self.quiet_s += dt;
            } else {
                self.quiet_s = 0.0;
            }
            return None;
        }
        if self.momentum.is_still() || self.coast_s <= 0.0 {
            self.momentum = Rates::default();
            return None;
        }
        // The exact integral of the decaying rate over the frame, so the
        // coast travels the same whatever the frame rate.
        let decay = (-dt / self.coast_s).exp();
        let step = self.momentum.scaled(self.coast_s * (1.0 - decay));
        self.momentum = self.momentum.scaled(decay);
        Some(step)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spin(rad_per_frame: f64) -> Rates {
        Rates {
            spin: DVec3::Z * rad_per_frame,
            ..Rates::default()
        }
    }

    /// A fling coasts `v·τ` in all, whatever the frame rate, and stops.
    #[test]
    fn a_fling_travels_its_rate_times_the_coast_and_stops() {
        for hz in [30.0, 60.0, 144.0] {
            let dt = 1.0 / hz;
            let mut i = Inertia::default();
            i.press();
            for _ in 0..30 {
                i.stroke(spin(0.5 * dt));
                assert!(i.frame(dt).is_none());
            }
            i.let_go();
            assert!(i.is_coasting());
            let mut travelled = 0.0;
            for _ in 0..(hz as usize * 5) {
                if let Some(s) = i.frame(dt) {
                    travelled += s.spin.z;
                }
            }
            assert!(!i.is_coasting());
            assert!(
                (travelled - 0.5 * DEFAULT_COAST_S).abs() < 0.01,
                "{hz} Hz: {travelled}"
            );
        }
    }

    /// A hand that stopped before letting go flings nothing.
    #[test]
    fn a_hand_held_still_flings_nothing() {
        let mut i = Inertia::default();
        i.press();
        i.stroke(spin(0.01));
        i.frame(1.0 / 60.0);
        for _ in 0..12 {
            i.frame(1.0 / 60.0);
        }
        i.let_go();
        assert!(!i.is_coasting());
    }

    /// No coast time, no momentum.
    #[test]
    fn a_coast_of_zero_is_no_momentum() {
        let mut i = Inertia {
            coast_s: 0.0,
            ..Inertia::default()
        };
        i.press();
        i.stroke(spin(0.01));
        i.frame(1.0 / 60.0);
        i.let_go();
        assert!(!i.is_coasting());
    }
}
