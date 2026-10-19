// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What a run can be told to do, one frame at a time.
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

/// Actions timed in frames: frame `n`'s actions are done before the `n`-th
/// step of the series. A frame with none is a step with nothing done.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Series(Vec<Vec<Action>>);

impl Series {
    pub fn new() -> Self {
        Self::default()
    }

    /// `frames` steps with nothing done: the time a still camera is given.
    pub fn still(frames: usize) -> Self {
        Self(vec![Vec::new(); frames])
    }

    /// One action a frame, in order.
    pub fn of(actions: impl IntoIterator<Item = Action>) -> Self {
        Self(actions.into_iter().map(|a| vec![a]).collect())
    }

    /// Adds `action` to frame `frame`, the series growing to reach it.
    #[must_use]
    pub fn at(mut self, frame: usize, action: Action) -> Self {
        if self.0.len() <= frame {
            self.0.resize(frame + 1, Vec::new());
        }
        self.0[frame].push(action);
        self
    }

    /// This series, then `next`.
    #[must_use]
    pub fn then(mut self, next: Series) -> Self {
        self.0.extend(next.0);
        self
    }

    /// Steps it takes.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(super) fn frames(&self) -> impl Iterator<Item = &Vec<Action>> {
        self.0.iter()
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
