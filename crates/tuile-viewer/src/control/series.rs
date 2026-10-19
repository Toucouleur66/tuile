// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What a run can be told to do, and when, in seconds of its own time.
//!
//! Every action enters where the same thing enters a window: a move of the
//! eye is a steering command on the queue a URL arrives on, a finger goes
//! through the touch recognizer, the wheel through the wheel's handler, a
//! resize and a change of density through the window's, pressure and the
//! background through the lifecycle. Nothing here moves the camera itself.

use super::{Headless, View};
use crate::steer::Request;

/// How a finger touches the glass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finger {
    Down,
    Moved,
    Up,
    Cancelled,
}

impl Finger {
    fn phase(self) -> crate::app::Finger {
        use crate::app::Finger as Phase;
        match self {
            Self::Down => Phase::Started,
            Self::Moved => Phase::Moved,
            Self::Up => Phase::Ended,
            Self::Cancelled => Phase::Cancelled,
        }
    }
}

/// One thing to do to a run.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Puts the eye there at once.
    Go(View),
    /// Glides there over this many seconds.
    Glide(View, f64),
    /// A steering URL, read as any other program's is.
    Url(String),
    /// A finger `id` at `at`, as fractions of the frame.
    Touch {
        id: u64,
        finger: Finger,
        at: (f64, f64),
    },
    /// The wheel turned by `amount` lines with the pointer at `at`, as
    /// fractions of the frame.
    Wheel { amount: f64, at: (f64, f64) },
    /// The drawable is now this size, device pixels: a resize, or a phone
    /// turned between portrait and landscape.
    Resize { width: u32, height: u32 },
    /// The display's density is now this many device pixels a point.
    Rescale(f64),
    /// The system says it is short of memory.
    MemoryWarning,
    /// The system grades how short: Android's levels.
    Pressure(Pressure),
    /// The application leaves the front.
    Background,
    /// It comes back.
    Foreground,
}

/// How short of memory the system says it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pressure {
    Warning,
    Critical,
}

/// Actions timed in seconds from the start of the series, and how long it
/// lasts.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Series {
    actions: Vec<(f64, Action)>,
    seconds: f64,
}

impl Series {
    pub fn new() -> Self {
        Self::default()
    }

    /// Nothing done for `seconds`: the time a still camera is given.
    pub fn still(seconds: f64) -> Self {
        Self {
            actions: Vec::new(),
            seconds,
        }
    }

    /// `action` at `seconds`, the series lasting at least until then.
    #[must_use]
    pub fn at(mut self, seconds: f64, action: Action) -> Self {
        let i = self.actions.partition_point(|(t, _)| *t <= seconds);
        self.actions.insert(i, (seconds, action));
        self.seconds = self.seconds.max(seconds);
        self
    }

    /// The eye along a path over `seconds`: `at(t)` for `t` from 0 to 1,
    /// sampled `per_second` times a second. A frame does what is due by the
    /// refresh it waits for, so a run whose frames are slower than the
    /// samples moves the eye as far, in fewer and longer steps.
    pub fn path(seconds: f64, per_second: f64, at: impl Fn(f64) -> View) -> Self {
        let n = (seconds * per_second).ceil().max(1.0) as usize;
        let mut series = Self::still(seconds);
        for i in 1..=n {
            let t = i as f64 / n as f64;
            series = series.at(t * seconds, Action::Go(at(t)));
        }
        series
    }

    /// This series, then `next`.
    #[must_use]
    pub fn then(mut self, next: Series) -> Self {
        let offset = self.seconds;
        self.actions
            .extend(next.actions.into_iter().map(|(t, a)| (t + offset, a)));
        self.seconds += next.seconds;
        self
    }

    /// How long it lasts.
    pub fn seconds(&self) -> f64 {
        self.seconds
    }

    pub(super) fn actions(&self) -> &[(f64, Action)] {
        &self.actions
    }
}

/// Does `action` to the run, where the same thing enters a window.
pub(super) fn carry_out(run: &mut Headless, action: &Action) {
    let size = run
        .app
        .screen_size()
        .map_or((1.0, 1.0), |((w, h), _)| (f64::from(w), f64::from(h)));
    let px = |at: (f64, f64)| (at.0 * size.0, at.1 * size.1);
    match action {
        Action::Go(view) => run.queue(Request::Do(view.command(None))),
        Action::Glide(view, seconds) => run.queue(Request::Do(view.command(Some(*seconds)))),
        Action::Url(url) => run.queue(Request::Url(url.clone())),
        Action::Touch { id, finger, at } => run.app_mut().finger(*id, finger.phase(), px(*at)),
        Action::Wheel { amount, at } => {
            let app = run.app_mut();
            app.point_at(px(*at));
            app.wheel(*amount);
        }
        Action::Resize { width, height } => run.app_mut().resized((*width, *height)),
        Action::Rescale(scale) => run.app_mut().rescaled(*scale),
        Action::MemoryWarning => run.app_mut().short_of_memory(),
        Action::Pressure(level) => run.app_mut().under_pressure(match level {
            Pressure::Warning => crate::profile::Pressure::Warning,
            Pressure::Critical => crate::profile::Pressure::Critical,
        }),
        // As a phone says it: no longer in front, then out of sight — and
        // back the other way. A desktop hears only the second half, and acts
        // on it as a window covered and uncovered.
        Action::Background => {
            let app = run.app_mut();
            app.step_away();
            app.hidden(true);
        }
        Action::Foreground => {
            let app = run.app_mut();
            app.hidden(false);
            app.come_back();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Actions keep their order in time, a series that follows another starts
    /// where it ended, and a path ends where it says.
    #[test]
    fn a_series_is_timed_in_seconds_and_follows_on() {
        let view = |altitude| View {
            altitude,
            ..View::default()
        };
        let s = Series::new()
            .at(0.5, Action::Rescale(2.0))
            .at(0.1, Action::Rescale(1.0))
            .then(Series::still(1.0))
            .then(Series::path(2.0, 10.0, |t| view(1000.0 * (1.0 + t))));
        assert!((s.seconds() - 3.5).abs() < 1e-9);
        let times: Vec<f64> = s.actions().iter().map(|(t, _)| *t).collect();
        assert!(times.windows(2).all(|w| w[0] <= w[1]), "{times:?}");
        assert_eq!(s.actions()[0].1, Action::Rescale(1.0));
        assert_eq!(s.actions().len(), 2 + 20);
        assert!(
            (times[2] - 1.6).abs() < 1e-9,
            "the path starts after the still"
        );
        assert_eq!(
            s.actions().last().map(|(_, a)| a),
            Some(&Action::Go(view(2000.0)))
        );
    }
}
