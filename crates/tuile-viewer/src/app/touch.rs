// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Fingers, turned into the same camera gestures the mouse makes.
//!
//! | fingers | motion | camera |
//! |---|---|---|
//! | one | drag | the grabbed ground follows the finger ([`CameraController::drag`]) |
//! | one | double tap | zoom in toward the tap |
//! | one | hold still | centre on the current location |
//! | two | pinch | zoom toward the midpoint |
//! | two | twist | turn the heading about the centre of the view |
//! | two | slide up or down together | tilt |
//!
//! Two fingers do one thing at a time. Whether they are tilting or
//! pinching-and-twisting is decided once, from the first few pixels they
//! travel, and held until they lift: a pinch whose fingers drift a little
//! upward must not start tipping the horizon, and a tilt whose fingers spread
//! a little must not zoom.
//!
//! The recognizer is pure: touches in, [`Gesture`]s out, a clock passed in.
//! The window turns the gestures into calls on the controller and the
//! on-screen control — see `App::on_touch` — and the tests here drive it with
//! fingers that are a list of numbers.
//!
//! [`CameraController::drag`]: tuile_camera::CameraController::drag

use std::collections::BTreeMap;
use std::time::Duration;
use web_time::Instant;

/// Pixels two fingers travel before what they are doing is decided.
const DECIDE_AFTER_PX: f64 = 12.0;
/// Pixels a finger may wander and still be tapping or holding.
const STILL_PX: f64 = 10.0;
/// Two taps closer than this in time and space are a double tap.
const DOUBLE_TAP: Duration = Duration::from_millis(300);
const DOUBLE_TAP_PX: f64 = 40.0;
/// A finger held still this long asks for the current location.
const HOLD: Duration = Duration::from_millis(700);
/// Radians of tilt per pixel the two fingers slide together, at one pixel per
/// point: the window divides by the display's scale, so the same slide tilts
/// as far on every screen.
const TILT_PER_PX: f64 = 0.004;

/// What the fingers asked for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Gesture {
    /// One finger came down here: the on-screen control gets first refusal.
    Press((f64, f64)),
    /// One finger moved.
    Drag { from: (f64, f64), to: (f64, f64) },
    /// The one finger lifted, or a second one arrived and the one-finger
    /// gesture is over.
    Release,
    /// Zoom by this ratio of finger spread, toward `centre`: 2.0 is the
    /// fingers twice as far apart, which brings the ground twice as close.
    Pinch { centre: (f64, f64), ratio: f64 },
    /// Turn by this many radians, clockwise on screen.
    Twist { radians: f64 },
    /// Tilt by this many radians: positive tips toward the horizon.
    Tilt { radians: f64 },
    /// Two quick taps here.
    DoubleTap((f64, f64)),
    /// A finger held still.
    Hold((f64, f64)),
}

/// How the window reports a finger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    Started,
    Moved,
    Ended,
    Cancelled,
}

/// What two fingers have been decided to be doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pair {
    Undecided,
    PinchTwist,
    Tilt,
}

/// The fingers on the glass and what they are doing.
#[derive(Debug)]
pub(super) struct Touches {
    /// Where each finger is now, by the window's id for it.
    at: BTreeMap<u64, (f64, f64)>,
    /// Where each finger came down.
    down: BTreeMap<u64, (f64, f64)>,
    pair: Pair,
    /// Whether one finger is driving a drag (and so is owed a release).
    one_finger: bool,
    /// The one finger: when it came down, and how far it has gone.
    pressed_at: Option<Instant>,
    travelled: f64,
    /// Whether this touch has already been something other than a tap.
    spent: bool,
    /// The last tap, for the double tap.
    last_tap: Option<(Instant, (f64, f64))>,
}

impl Default for Touches {
    fn default() -> Self {
        Self {
            at: BTreeMap::new(),
            down: BTreeMap::new(),
            pair: Pair::Undecided,
            one_finger: false,
            pressed_at: None,
            travelled: 0.0,
            spent: false,
            last_tap: None,
        }
    }
}

impl Touches {
    /// Whether a finger is on the glass.
    pub(super) fn any(&self) -> bool {
        !self.at.is_empty()
    }

    /// One finger event in, the gestures it makes out.
    pub(super) fn on(
        &mut self,
        now: Instant,
        id: u64,
        phase: Phase,
        p: (f64, f64),
    ) -> Vec<Gesture> {
        match phase {
            Phase::Started => self.started(now, id, p),
            Phase::Moved => self.moved(id, p),
            Phase::Ended => self.ended(now, id, p, true),
            Phase::Cancelled => self.ended(now, id, p, false),
        }
    }

    /// The held finger, if one has been still long enough. Asked once a frame:
    /// holding is the absence of events, so no event can report it.
    pub(super) fn tick(&mut self, now: Instant) -> Option<Gesture> {
        if self.at.len() != 1 || self.spent || self.travelled > STILL_PX {
            return None;
        }
        let since = self.pressed_at?;
        if now.duration_since(since) < HOLD {
            return None;
        }
        self.spent = true;
        self.at.values().next().map(|&p| Gesture::Hold(p))
    }

    fn started(&mut self, now: Instant, id: u64, p: (f64, f64)) -> Vec<Gesture> {
        self.at.insert(id, p);
        self.down.insert(id, p);
        match self.at.len() {
            1 => {
                self.one_finger = true;
                self.pressed_at = Some(now);
                self.travelled = 0.0;
                self.spent = false;
                vec![Gesture::Press(p)]
            }
            2 => {
                // A second finger ends whatever the first was doing.
                self.pair = Pair::Undecided;
                self.spent = true;
                self.last_tap = None;
                for (k, v) in &self.at {
                    self.down.insert(*k, *v);
                }
                if std::mem::take(&mut self.one_finger) {
                    vec![Gesture::Release]
                } else {
                    Vec::new()
                }
            }
            // A third finger is not a gesture this knows; it is ignored, and
            // the first two carry on.
            _ => Vec::new(),
        }
    }

    fn moved(&mut self, id: u64, p: (f64, f64)) -> Vec<Gesture> {
        let Some(&before) = self.at.get(&id) else {
            return Vec::new();
        };
        match self.at.len() {
            1 => {
                self.at.insert(id, p);
                self.travelled += distance(before, p);
                if self.travelled > STILL_PX {
                    self.spent = true;
                }
                if self.one_finger {
                    vec![Gesture::Drag {
                        from: before,
                        to: p,
                    }]
                } else {
                    // The finger left over from a two-finger gesture: it
                    // picks the drag up from where it now is, not from where
                    // it was when the pair began.
                    self.one_finger = true;
                    vec![Gesture::Press(p)]
                }
            }
            2 => {
                let other = self
                    .at
                    .iter()
                    .find(|(k, _)| **k != id)
                    .map(|(_, v)| *v)
                    .unwrap_or(before);
                self.at.insert(id, p);
                self.pair_moved((before, other), (p, other))
            }
            _ => {
                self.at.insert(id, p);
                Vec::new()
            }
        }
    }

    /// Two fingers moved from `was` to `now` (one of them, in fact; the other
    /// is the same in both).
    fn pair_moved(
        &mut self,
        was: ((f64, f64), (f64, f64)),
        now: ((f64, f64), (f64, f64)),
    ) -> Vec<Gesture> {
        if self.pair == Pair::Undecided {
            self.pair = self.decide();
            if self.pair == Pair::Undecided {
                return Vec::new();
            }
        }
        let (centre0, centre1) = (midpoint(was.0, was.1), midpoint(now.0, now.1));
        match self.pair {
            Pair::Tilt => {
                // Sliding up tips the view toward the horizon, as on every map.
                let dy = centre1.1 - centre0.1;
                vec![Gesture::Tilt {
                    radians: -dy * TILT_PER_PX,
                }]
            }
            Pair::PinchTwist => {
                let (d0, d1) = (distance(was.0, was.1), distance(now.0, now.1));
                let mut out = Vec::new();
                if d0 > 1.0 && d1 > 1.0 && (d1 - d0).abs() > f64::EPSILON {
                    out.push(Gesture::Pinch {
                        centre: centre1,
                        ratio: d1 / d0,
                    });
                }
                let turn = wrap(angle(now.0, now.1) - angle(was.0, was.1));
                if turn.abs() > 1e-6 {
                    out.push(Gesture::Twist { radians: turn });
                }
                out
            }
            Pair::Undecided => Vec::new(),
        }
    }

    /// From where both fingers came down to where they are: moving the same
    /// way, mostly up or down, is a tilt; anything else is a pinch and twist.
    fn decide(&self) -> Pair {
        let mut fingers = self.at.iter();
        let (Some((ia, a)), Some((ib, b))) = (fingers.next(), fingers.next()) else {
            return Pair::Undecided;
        };
        let (Some(a0), Some(b0)) = (self.down.get(ia), self.down.get(ib)) else {
            return Pair::Undecided;
        };
        let (da, db) = ((a.0 - a0.0, a.1 - a0.1), (b.0 - b0.0, b.1 - b0.1));
        let (la, lb) = (da.0.hypot(da.1), db.0.hypot(db.1));
        if la.max(lb) < DECIDE_AFTER_PX {
            return Pair::Undecided;
        }
        let vertical = |d: (f64, f64)| d.1.abs() > 2.0 * d.0.abs();
        // The two fingers' events arrive one after the other. A finger going
        // up or down may be the first half of a tilt, so wait for the other
        // to move too — unless the first has clearly gone on without it, which
        // is a twist about a still finger.
        let leading = if la >= lb { da } else { db };
        if vertical(leading)
            && la.min(lb) < DECIDE_AFTER_PX * 0.5
            && la.max(lb) < DECIDE_AFTER_PX * 3.0
        {
            return Pair::Undecided;
        }
        let together = da.0 * db.0 + da.1 * db.1 > 0.0;
        if together && vertical(da) && vertical(db) && la.min(lb) >= 0.35 * la.max(lb) {
            Pair::Tilt
        } else {
            Pair::PinchTwist
        }
    }

    fn ended(&mut self, now: Instant, id: u64, p: (f64, f64), lifted: bool) -> Vec<Gesture> {
        if self.at.remove(&id).is_none() {
            return Vec::new();
        }
        self.down.remove(&id);
        let mut out = Vec::new();
        if self.at.is_empty() {
            if std::mem::take(&mut self.one_finger) {
                out.push(Gesture::Release);
            }
            let tapped = lifted && !self.spent && self.travelled <= STILL_PX;
            if tapped {
                match self.last_tap {
                    Some((when, at))
                        if now.duration_since(when) <= DOUBLE_TAP
                            && distance(at, p) <= DOUBLE_TAP_PX =>
                    {
                        self.last_tap = None;
                        out.push(Gesture::DoubleTap(p));
                    }
                    _ => self.last_tap = Some((now, p)),
                }
            }
            self.pair = Pair::Undecided;
            self.pressed_at = None;
        } else if self.at.len() == 1 {
            // Back to one finger: it does nothing until it moves, and then
            // drags from where it is.
            self.pair = Pair::Undecided;
        }
        out
    }
}

fn distance(a: (f64, f64), b: (f64, f64)) -> f64 {
    (a.0 - b.0).hypot(a.1 - b.1)
}

fn midpoint(a: (f64, f64), b: (f64, f64)) -> (f64, f64) {
    ((a.0 + b.0) * 0.5, (a.1 + b.1) * 0.5)
}

/// The angle of the line from `a` to `b`, clockwise on screen (y grows down).
fn angle(a: (f64, f64), b: (f64, f64)) -> f64 {
    (b.1 - a.1).atan2(b.0 - a.0)
}

fn wrap(radians: f64) -> f64 {
    use std::f64::consts::{PI, TAU};
    let r = radians.rem_euclid(TAU);
    if r > PI {
        r - TAU
    } else {
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Hand {
        touches: Touches,
        t0: Instant,
        now: Duration,
    }

    impl Hand {
        fn new() -> Self {
            Self {
                touches: Touches::default(),
                t0: Instant::now(),
                now: Duration::ZERO,
            }
        }
        fn at(&mut self, ms: u64) -> &mut Self {
            self.now = Duration::from_millis(ms);
            self
        }
        fn go(&mut self, id: u64, phase: Phase, p: (f64, f64)) -> Vec<Gesture> {
            let now = self.t0 + self.now;
            self.touches.on(now, id, phase, p)
        }
    }

    #[test]
    fn one_finger_presses_drags_and_releases() {
        let mut h = Hand::new();
        assert_eq!(
            h.go(1, Phase::Started, (100.0, 100.0)),
            [Gesture::Press((100.0, 100.0))]
        );
        assert_eq!(
            h.go(1, Phase::Moved, (130.0, 90.0)),
            [Gesture::Drag {
                from: (100.0, 100.0),
                to: (130.0, 90.0)
            }]
        );
        assert_eq!(h.go(1, Phase::Ended, (130.0, 90.0)), [Gesture::Release]);
        assert!(!h.touches.any());
    }

    /// Fingers spreading apart along a line zoom in, by the ratio of their
    /// spread, toward their midpoint — and do not tilt.
    #[test]
    fn two_fingers_spreading_pinch_toward_their_midpoint() {
        let mut h = Hand::new();
        h.go(1, Phase::Started, (100.0, 200.0));
        assert_eq!(h.go(2, Phase::Started, (200.0, 200.0)), [Gesture::Release]);
        let mut ratio = 1.0;
        for step in 1..=10 {
            let x = 200.0 + 10.0 * f64::from(step);
            for g in h.go(2, Phase::Moved, (x, 200.0)) {
                match g {
                    Gesture::Pinch { ratio: r, centre } => {
                        ratio *= r;
                        assert!((centre.1 - 200.0).abs() < 1e-9);
                    }
                    Gesture::Twist { radians } => assert!(radians.abs() < 1e-9),
                    other => panic!("a spread made {other:?}"),
                }
            }
        }
        // Decided after the first 12 px, so the first step is not counted:
        // from 110 px apart to 200 px apart.
        assert!((ratio - 200.0 / 110.0).abs() < 1e-9, "{ratio}");
    }

    /// Two fingers turning about their midpoint twist the heading by the
    /// angle they turned.
    #[test]
    fn two_fingers_turning_twist() {
        let mut h = Hand::new();
        let r = 80.0;
        let at = |a: f64| {
            (
                (200.0 + r * a.cos(), 300.0 + r * a.sin()),
                (200.0 - r * a.cos(), 300.0 - r * a.sin()),
            )
        };
        let (a, b) = at(0.0);
        h.go(1, Phase::Started, a);
        h.go(2, Phase::Started, b);
        let mut turned = 0.0;
        for step in 1..=20 {
            let (a, b) = at(0.02 * f64::from(step));
            for g in h
                .go(1, Phase::Moved, a)
                .into_iter()
                .chain(h.go(2, Phase::Moved, b))
            {
                if let Gesture::Twist { radians } = g {
                    turned += radians;
                }
                assert!(!matches!(g, Gesture::Tilt { .. }), "a twist tilted");
            }
        }
        // Everything from the decision on, which took the first 12 px — the
        // eighth step of twenty — so thirteen steps of 0.02 rad.
        assert!((turned - 0.26).abs() < 0.01, "{turned}");
    }

    /// Two fingers sliding up together tilt toward the horizon, and keep
    /// tilting even if they then spread a little: the gesture is held.
    #[test]
    fn two_fingers_sliding_up_together_tilt_and_stay_a_tilt() {
        let mut h = Hand::new();
        h.go(1, Phase::Started, (150.0, 400.0));
        h.go(2, Phase::Started, (250.0, 400.0));
        let mut tilt = 0.0;
        for step in 1..=10 {
            let y = 400.0 - 8.0 * f64::from(step);
            let spread = f64::from(step);
            for g in h
                .go(1, Phase::Moved, (150.0 - spread, y))
                .into_iter()
                .chain(h.go(2, Phase::Moved, (250.0 + spread, y)))
            {
                match g {
                    Gesture::Tilt { radians } => tilt += radians,
                    other => panic!("a tilt made {other:?}"),
                }
            }
        }
        assert!(tilt > 0.0, "sliding up tips toward the horizon: {tilt}");
    }

    /// A tap is not a drag; two quick ones close together are a double tap;
    /// two slow ones are not.
    #[test]
    fn two_quick_taps_are_a_double_tap() {
        let mut h = Hand::new();
        h.at(0).go(1, Phase::Started, (50.0, 50.0));
        assert_eq!(
            h.at(80).go(1, Phase::Ended, (51.0, 50.0)),
            [Gesture::Release]
        );
        h.at(200).go(2, Phase::Started, (55.0, 52.0));
        assert_eq!(
            h.at(260).go(2, Phase::Ended, (55.0, 52.0)),
            [Gesture::Release, Gesture::DoubleTap((55.0, 52.0))]
        );

        let mut slow = Hand::new();
        slow.at(0).go(1, Phase::Started, (50.0, 50.0));
        slow.at(80).go(1, Phase::Ended, (50.0, 50.0));
        slow.at(900).go(2, Phase::Started, (50.0, 50.0));
        assert_eq!(
            slow.at(960).go(2, Phase::Ended, (50.0, 50.0)),
            [Gesture::Release]
        );
    }

    /// A finger held still asks once; one that moved, or lifted, never does.
    #[test]
    fn a_finger_held_still_asks_once() {
        let mut h = Hand::new();
        h.at(0).go(1, Phase::Started, (70.0, 70.0));
        assert_eq!(h.touches.tick(h.t0 + Duration::from_millis(300)), None);
        assert_eq!(
            h.touches.tick(h.t0 + Duration::from_millis(800)),
            Some(Gesture::Hold((70.0, 70.0)))
        );
        assert_eq!(h.touches.tick(h.t0 + Duration::from_millis(1600)), None);

        let mut moved = Hand::new();
        moved.at(0).go(1, Phase::Started, (70.0, 70.0));
        moved.at(100).go(1, Phase::Moved, (100.0, 70.0));
        assert_eq!(moved.touches.tick(moved.t0 + Duration::from_secs(2)), None);
    }

    /// The finger left when the other lifts drags from where it is, not from
    /// where it came down — no jump.
    #[test]
    fn the_finger_left_over_drags_from_where_it_is() {
        let mut h = Hand::new();
        h.go(1, Phase::Started, (100.0, 100.0));
        h.go(2, Phase::Started, (200.0, 100.0));
        h.go(1, Phase::Moved, (60.0, 100.0));
        assert!(h.go(2, Phase::Ended, (200.0, 100.0)).is_empty());
        assert_eq!(
            h.go(1, Phase::Moved, (65.0, 100.0)),
            [Gesture::Press((65.0, 100.0))]
        );
        assert_eq!(
            h.go(1, Phase::Moved, (70.0, 100.0)),
            [Gesture::Drag {
                from: (65.0, 100.0),
                to: (70.0, 100.0)
            }]
        );
        // A cancelled touch is never a tap.
        assert_eq!(h.go(1, Phase::Cancelled, (70.0, 100.0)), [Gesture::Release]);
    }
}
