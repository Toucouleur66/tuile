// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Drawing only while something on screen changes.
//!
//! A desktop draws every frame the display offers. A phone pays for each one
//! in battery and in heat, and a still globe with nothing loading looks the
//! same at four frames a second as at sixty — so a phone draws at full rate
//! while the view moves, a finger is down, the indicator turns, tiles arrive
//! or slide into place, and drops to a slow wake-up once all of that has been
//! quiet for a moment. The wake-up is not idle time thrown away: each one
//! drains the stream, so a tile that lands on a still camera is drawn within
//! a quarter of a second, and its upload makes the next frames busy again.
//!
//! Pure: what the frame saw in, a [`Pace`] out, the clock passed in.

use std::time::Duration;
use web_time::Instant;

/// Quiet for this long before the rate drops: an ease coming to rest, a
/// finger lifting between two gestures.
const GRACE: Duration = Duration::from_millis(500);
/// How often a quiet session wakes to look for news.
pub(super) const IDLE_WAKE: Duration = Duration::from_millis(250);

/// What a frame saw that needs further frames.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Busy {
    /// The eased camera has not reached where the gestures put it.
    pub moving: bool,
    /// A finger is on the glass.
    pub touching: bool,
    /// The loading indicator is on screen, and turning.
    pub indicator: bool,
    /// Tiles reached the GPU this frame.
    pub uploaded: usize,
    /// Tiles decoded and waiting for a frame's upload budget.
    pub pending_uploads: usize,
    /// Tiles still sliding to a new edge where they meet a neighbour.
    pub sliding: usize,
}

impl Busy {
    fn any(&self) -> bool {
        self.moving
            || self.touching
            || self.indicator
            || self.uploaded > 0
            || self.pending_uploads > 0
            || self.sliding > 0
    }
}

/// How often to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Pace {
    /// As often as the device allows.
    Continuous,
    /// Once every [`IDLE_WAKE`].
    Idle,
}

/// A start-up taking longer than this is waiting on a slow or absent network.
const PATIENT_AFTER: Duration = Duration::from_secs(3);
/// The frame interval while it waits: the indicator still turns, at a rate
/// that costs little.
const WHILE_WAITING: Duration = Duration::from_nanos(1_000_000_000 / 15);

/// The frame interval for `interval`, given how long the session has been
/// waiting to start (`None` once it is up). A start-up that hangs on a dead
/// network must not draw sixty frames a second of a spinner.
pub(super) fn while_waiting(interval: Duration, waited: Option<Duration>) -> Duration {
    match waited {
        Some(waited) if waited >= PATIENT_AFTER => interval.max(WHILE_WAITING),
        _ => interval,
    }
}

#[derive(Debug, Default)]
pub(super) struct Demand {
    /// Since when nothing has needed a frame; `None` while something does.
    quiet_since: Option<Instant>,
}

impl Demand {
    /// Notes what a frame saw.
    pub(super) fn saw(&mut self, now: Instant, busy: Busy) {
        if busy.any() {
            self.quiet_since = None;
        } else {
            self.quiet_since.get_or_insert(now);
        }
    }

    /// Something happened between frames — a touch, a command, the
    /// application coming back — and wants to be seen at once.
    pub(super) fn poke(&mut self) {
        self.quiet_since = None;
    }

    pub(super) fn pace(&self, now: Instant) -> Pace {
        match self.quiet_since {
            Some(since) if now.duration_since(since) >= GRACE => Pace::Idle,
            _ => Pace::Continuous,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A launch gets full rate for its first seconds, then fifteen frames a
    /// second for as long as it hangs; a running session is never held back.
    #[test]
    fn a_long_wait_to_start_is_drawn_slowly() {
        let full = Duration::from_secs(1) / 60;
        assert_eq!(while_waiting(full, None), full);
        assert_eq!(while_waiting(full, Some(Duration::from_secs(1))), full);
        assert_eq!(
            while_waiting(full, Some(Duration::from_secs(4))),
            WHILE_WAITING
        );
        // Already slower (idle, or hot): not made faster.
        let idle = IDLE_WAKE;
        assert_eq!(while_waiting(idle, Some(Duration::from_secs(9))), idle);
    }

    #[test]
    fn a_quiet_session_slows_down_after_a_grace_and_wakes_on_news() {
        let t0 = Instant::now();
        let mut d = Demand::default();
        assert_eq!(d.pace(t0), Pace::Continuous, "a new session draws");

        d.saw(t0, Busy::default());
        assert_eq!(d.pace(t0 + Duration::from_millis(100)), Pace::Continuous);
        let t1 = t0 + GRACE;
        assert_eq!(d.pace(t1), Pace::Idle);
        // Staying quiet does not restart the grace.
        d.saw(t1, Busy::default());
        assert_eq!(d.pace(t1), Pace::Idle);

        // Each reason alone is enough to draw at full rate again.
        for busy in [
            Busy {
                moving: true,
                ..Busy::default()
            },
            Busy {
                touching: true,
                ..Busy::default()
            },
            Busy {
                indicator: true,
                ..Busy::default()
            },
            Busy {
                uploaded: 1,
                ..Busy::default()
            },
            Busy {
                pending_uploads: 3,
                ..Busy::default()
            },
            Busy {
                sliding: 2,
                ..Busy::default()
            },
        ] {
            let mut d = Demand::default();
            d.saw(t0, Busy::default());
            assert_eq!(d.pace(t1), Pace::Idle);
            d.saw(t1, busy);
            assert_eq!(d.pace(t1), Pace::Continuous, "{busy:?}");
        }

        // A poke between frames does the same.
        d.poke();
        assert_eq!(d.pace(t1), Pace::Continuous);
    }
}
