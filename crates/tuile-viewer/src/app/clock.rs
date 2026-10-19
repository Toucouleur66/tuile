// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The clock the application reads its time from.
//!
//! A window's is the wall clock, as it always was. A headless run's is
//! stepped: time moves only when the run says so, by exactly what it says, so
//! the frame's interval — the one number the camera's easing depends on —
//! and every instant a glide, a finger or the loading indicator reads are the
//! same on every run of the same script. What the clock is used for does not
//! change between the two; only who turns it.

use std::cell::Cell;
use std::rc::Rc;
use web_time::Instant;

/// The application's time: the wall's, or a stepped one held by whoever runs
/// the session.
#[derive(Debug, Clone, Default)]
pub(crate) struct Clock(Option<Rc<Cell<Instant>>>);

impl Clock {
    /// Now, as this clock has it.
    pub(crate) fn now(&self) -> Instant {
        self.0.as_ref().map_or_else(Instant::now, |at| at.get())
    }

    /// A clock that stands at `start` until its [`Stepper`] moves it.
    #[cfg(feature = "control")]
    pub(crate) fn stepped(start: Instant) -> (Self, Stepper) {
        let at = Rc::new(Cell::new(start));
        (Self(Some(Rc::clone(&at))), Stepper(at))
    }
}

/// The hand that moves a stepped [`Clock`].
#[cfg(feature = "control")]
#[derive(Debug)]
pub(crate) struct Stepper(Rc<Cell<Instant>>);

#[cfg(feature = "control")]
impl Stepper {
    /// Moves the clock on by `by`.
    pub(crate) fn advance(&self, by: std::time::Duration) {
        self.0.set(self.0.get() + by);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A stepped clock stands still between steps and moves by exactly the
    /// step; every copy of it reads the same instant.
    #[test]
    fn a_stepped_clock_moves_only_when_stepped_and_by_the_step() {
        let start = Instant::now();
        let (clock, hand) = Clock::stepped(start);
        let copy = clock.clone();
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(clock.now(), start, "the wall moved, the clock did not");
        hand.advance(Duration::from_millis(16));
        assert_eq!(copy.now(), start + Duration::from_millis(16));
        // The wall clock is the wall's.
        assert!(Clock::default().now() >= start);
    }
}
