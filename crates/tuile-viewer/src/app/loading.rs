// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! When the loading indicator is shown, and how strongly.
//!
//! On a phone the window opens at once — the system ends an application that
//! shows nothing for too long — so the first frames are the whole-planet shell
//! with nothing on it yet. The indicator is what says that this is loading and
//! not broken:
//!
//! - **At launch** it is large, in the middle of the view, with the coarse
//!   pyramid's progress around it, and it stays until the first view is
//!   *covered*: the session is up, the pyramid every fallback lands on is on
//!   the GPU, and every selected tile draws something. Then it fades.
//! - **Later** it comes back, small and in a corner, when a jump leaves most
//!   of the view drawn by coarser ancestors for a moment — and goes once the
//!   view has caught up. A few soft tiles at the edge of a drag are normal
//!   streaming and do not bring it back.
//!
//! Pure: observations in, an [`Indicator`] out, the clock passed in. The
//! window turns the indicator into a [`tuile_ui::Spinner`].

use std::time::{Duration, Instant};

/// How long a fade takes, in or out.
const FADE: Duration = Duration::from_millis(350);
/// How long the view must stay under-resolved before the small indicator
/// appears: a brief soft patch during a drag is not worth a spinner.
const UNDER_RESOLVED_FOR: Duration = Duration::from_millis(400);
/// The small indicator's strength: present, not shouting.
const SUBTLE: f32 = 0.75;
/// Fewer selected tiles than this is not a view to judge.
const ENOUGH_TILES: usize = 8;
/// Loads in flight, against tiles on screen, that say the view is still far
/// from what was asked: after a jump the selection is a handful of coarse
/// tiles standing in for hundreds on their way, and those stand-ins count as
/// drawn.
const BACKLOG_PER_TILE: usize = 4;
const BACKLOG_FLOOR: usize = 64;

/// What the frame saw.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(super) struct Observed {
    /// The session is connected and its geometry server is running.
    pub ready: bool,
    /// The pinned coarse pyramid is on the GPU.
    pub warm: bool,
    /// Tiles the traversal selected for this view.
    pub selected: usize,
    /// Selected tiles with nothing at all to draw them — the shell shows.
    pub lost: usize,
    /// Selected tiles drawn by a coarser ancestor, or by a stand-in.
    pub coarser: usize,
    /// Tile loads the server has in flight.
    pub loading: usize,
    /// How much of the pyramid has arrived, when the server has said.
    pub progress: Option<f32>,
    /// The session will not start; the reason is on screen.
    pub failed: bool,
}

/// Which indicator to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Style {
    /// Large, centred, with progress.
    Launch,
    /// Small, in a corner.
    Refining,
}

/// What to draw this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Indicator {
    pub style: Style,
    pub opacity: f32,
    pub progress: Option<f32>,
}

/// The indicator's state between frames.
#[derive(Debug)]
pub(super) struct Loading {
    /// Whether the first view has been covered. Once, for the session.
    launched: bool,
    opacity: f32,
    style: Style,
    /// Since when the view has been under-resolved, if it is.
    under_since: Option<Instant>,
    /// Whether the small indicator has been called for and not yet let go.
    refining: bool,
    last: Instant,
}

impl Loading {
    /// Full strength from the first frame: the launch indicator is up before
    /// anything is known.
    pub(super) fn new(now: Instant) -> Self {
        Self {
            launched: false,
            opacity: 1.0,
            style: Style::Launch,
            under_since: None,
            refining: false,
            last: now,
        }
    }

    /// Whether the first view has been covered.
    pub(super) fn launched(&self) -> bool {
        self.launched
    }

    pub(super) fn step(&mut self, now: Instant, o: Observed) -> Indicator {
        let dt = now.saturating_duration_since(self.last);
        self.last = now;

        let target = if !self.launched {
            if o.ready && o.warm && o.selected > 0 && o.lost == 0 {
                self.launched = true;
                0.0
            } else if o.failed {
                0.0
            } else {
                1.0
            }
        } else {
            self.refine(now, o)
        };
        if self.launched && self.opacity <= 0.0 {
            self.style = Style::Refining;
        }
        let step = dt.as_secs_f32() / FADE.as_secs_f32();
        self.opacity = if target > self.opacity {
            (self.opacity + step).min(target)
        } else {
            (self.opacity - step).max(target)
        };
        Indicator {
            style: self.style,
            opacity: self.opacity,
            progress: if self.style == Style::Launch {
                o.progress.or(if o.ready { None } else { Some(0.0) })
            } else {
                None
            },
        }
    }

    /// The small indicator's target strength, with hysteresis: it comes on at
    /// half the view under-resolved and goes off below a fifth.
    fn refine(&mut self, now: Instant, o: Observed) -> f32 {
        let enough = o.selected >= ENOUGH_TILES;
        let backlog = o.loading >= (o.selected * BACKLOG_PER_TILE).max(BACKLOG_FLOOR);
        let bad = backlog || (enough && (o.lost > 0 || o.coarser * 2 >= o.selected));
        let good = !backlog
            && (!enough || (o.lost == 0 && o.coarser * 5 < o.selected))
            && o.loading * 2 < (o.selected * BACKLOG_PER_TILE).max(BACKLOG_FLOOR);
        if bad {
            let since = *self.under_since.get_or_insert(now);
            if now.duration_since(since) >= UNDER_RESOLVED_FOR {
                self.refining = true;
            }
        } else {
            self.under_since = None;
        }
        if good {
            self.refining = false;
        }
        if self.refining {
            SUBTLE
        } else {
            0.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: Duration = Duration::from_millis(16);

    fn covered() -> Observed {
        Observed {
            ready: true,
            warm: true,
            selected: 40,
            lost: 0,
            coarser: 0,
            loading: 0,
            progress: Some(1.0),
            failed: false,
        }
    }

    /// Runs `frames` frames of `o`, returning the last indicator.
    fn run(l: &mut Loading, t: &mut Instant, frames: usize, o: Observed) -> Indicator {
        let mut last = None;
        for _ in 0..frames {
            *t += FRAME;
            last = Some(l.step(*t, o));
        }
        last.expect("at least one frame")
    }

    /// A session whose network never answers keeps its indicator up — for a
    /// whole minute of frames, at full strength, with an empty progress ring
    /// rather than a frozen one.
    #[test]
    fn the_launch_indicator_stays_while_nothing_arrives() {
        let mut t = Instant::now();
        let mut l = Loading::new(t);
        let stalled = Observed::default();
        let i = run(&mut l, &mut t, 60 * 60, stalled);
        assert_eq!(i.style, Style::Launch);
        assert_eq!(i.opacity, 1.0);
        assert_eq!(i.progress, Some(0.0));
        assert!(l.opacity > 0.0 && !l.launched());
    }

    /// Not before every part of "covered" holds: a warm pyramid with ground
    /// still missing, or a full view before the pyramid, keep it up.
    #[test]
    fn the_launch_indicator_waits_for_the_whole_view() {
        let mut t = Instant::now();
        let mut l = Loading::new(t);
        for o in [
            Observed {
                lost: 3,
                ..covered()
            },
            Observed {
                warm: false,
                ..covered()
            },
            Observed {
                selected: 0,
                ..covered()
            },
            Observed {
                ready: false,
                ..covered()
            },
        ] {
            let i = run(&mut l, &mut t, 120, o);
            assert_eq!(i.opacity, 1.0, "{o:?}");
        }
        // Coarse ground is covered ground: it does not hold the launch.
        let i = run(
            &mut l,
            &mut t,
            1,
            Observed {
                coarser: 30,
                ..covered()
            },
        );
        assert!(i.opacity < 1.0 && l.launched());
    }

    /// Covered, it fades within the fade time and is gone, and comes back as
    /// the small one only after a jump leaves the view under-resolved for a
    /// moment — then goes again once the view catches up.
    #[test]
    fn it_fades_once_covered_and_returns_small_after_a_jump() {
        let mut t = Instant::now();
        let mut l = Loading::new(t);
        let i = run(&mut l, &mut t, 30, covered());
        assert_eq!(i.opacity, 0.0);
        assert_eq!(l.opacity, 0.0);

        // A few soft tiles: streaming, not a jump.
        let i = run(
            &mut l,
            &mut t,
            120,
            Observed {
                coarser: 6,
                ..covered()
            },
        );
        assert_eq!(i.opacity, 0.0);

        // Most of the view coarse: nothing for the first moment, then the
        // small indicator.
        let jump = Observed {
            coarser: 30,
            ..covered()
        };
        let i = run(&mut l, &mut t, 10, jump);
        assert_eq!(i.opacity, 0.0, "a brief soft patch is not worth a spinner");
        let i = run(&mut l, &mut t, 60, jump);
        assert_eq!(i.style, Style::Refining);
        assert_eq!(i.opacity, SUBTLE);
        assert_eq!(i.progress, None);

        // Partly caught up: stays (hysteresis). Caught up: fades out.
        let i = run(
            &mut l,
            &mut t,
            30,
            Observed {
                coarser: 12,
                ..covered()
            },
        );
        assert_eq!(i.opacity, SUBTLE);
        let i = run(&mut l, &mut t, 30, covered());
        assert_eq!(i.opacity, 0.0);
    }

    /// After a jump down to the ground the view is a handful of stand-ins
    /// with hundreds of loads behind them — counted as drawn, but nothing
    /// like what was asked. The backlog brings the small indicator back.
    #[test]
    fn a_backlog_of_loads_brings_the_small_indicator_back() {
        let mut t = Instant::now();
        let mut l = Loading::new(t);
        run(&mut l, &mut t, 30, covered());
        let jumped = Observed {
            selected: 10,
            loading: 477,
            ..covered()
        };
        let i = run(&mut l, &mut t, 60, jumped);
        assert_eq!((i.style, i.opacity), (Style::Refining, SUBTLE));
        // Ordinary streaming behind a pan does not.
        let mut calm = Loading::new(t);
        run(&mut calm, &mut t, 30, covered());
        let i = run(
            &mut calm,
            &mut t,
            120,
            Observed {
                loading: 40,
                ..covered()
            },
        );
        assert_eq!(i.opacity, 0.0);
        // Caught up: it goes.
        let i = run(&mut l, &mut t, 60, covered());
        assert_eq!(i.opacity, 0.0);
    }

    /// A session that cannot start says why in a message; the spinner does
    /// not keep promising a load that will never come.
    #[test]
    fn a_failed_session_lets_the_indicator_go() {
        let mut t = Instant::now();
        let mut l = Loading::new(t);
        let i = run(
            &mut l,
            &mut t,
            30,
            Observed {
                failed: true,
                ..Observed::default()
            },
        );
        assert_eq!(i.opacity, 0.0);
    }
}
