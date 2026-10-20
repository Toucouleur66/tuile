// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Foreground, background, and what each one allows.
//!
//! A phone tells an application four things, in this order and its reverse:
//!
//! ```text
//! active ──resign──▶ inactive ──background──▶ background
//!   ▲                   │  ▲                       │
//!   └──────activate─────┘  └──────foreground───────┘
//! ```
//!
//! and it holds the application to rules a desktop does not: **no GPU work
//! outside the foreground** — the system ends an application that submits any
//! — and as little memory as possible in the background, where the largest
//! applications are the first ended. So:
//!
//! - **resigning** (a call, the control centre pulled down) stops drawing and
//!   holds new loads at the [`crate::gate::Gate`];
//! - **the background** also lets go of the surface and its render targets
//!   (tens of megabytes at full resolution and four samples) and tells the
//!   server's budget it is in the background (`Pressure::Background`): its
//!   targets shrink to what the view draws and the floor, never past them,
//!   so the globe comes back covered;
//! - **activating** brings the surface back first, then opens the gate and
//!   asks for a frame: the surface exists before anything draws into it; the
//!   budget is told the pressure is over.
//!
//! A desktop only ever sees its window covered and uncovered, and does what it
//! always did: stops drawing while covered.
//!
//! A browser tab sits between the two. Hidden, it holds its loads and gives
//! memory back like a phone — a tab in the background is still the user's memory — but
//! keeps its surface, which a page cannot rebuild cheaply (a WebGL2 device is
//! bound to its canvas), and comes straight back to the front when shown
//! again: a page has no separate "activate".
//!
//! Pure: events in, [`Step`]s out. The window carries the steps out.

use crate::profile::Pressure;

/// Where the application stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase {
    Active,
    Inactive,
    Background,
}

/// One thing to do about a change of phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Step {
    /// Hold every load not yet started.
    PauseLoads,
    /// Let them go.
    ResumeLoads,
    /// Hand the server's budget the platform's memory signal: its targets
    /// shrink by it, never past what the view draws and the floor.
    Pressure(Pressure),
    /// Write the tile store down and close it: the system ends background
    /// applications without a word, and an unclosed store loses its memory
    /// tier.
    SuspendStore,
    /// Open it again.
    ResumeStore,
    /// Drop the surface and the render targets.
    ReleaseSurface,
    /// Build them again, before any frame.
    RestoreSurface,
    /// Draw a frame.
    Redraw,
}

#[derive(Debug)]
pub(super) struct Lifecycle {
    phase: Phase,
    /// Whether loads are held and residency trimmed while away: a phone and a
    /// browser tab. See the module.
    pause_when_away: bool,
    /// Whether the surface is let go of too, and the way back passes through
    /// an activation: a phone.
    release_surface: bool,
    surface_released: bool,
    store_suspended: bool,
    /// The memory signal last handed to the budget.
    pressure: Pressure,
}

/// Acts once on each memory signal a system counts — Android's
/// `onTrimMemory`, read as a count and a level — however many times the
/// count is read.
#[derive(Debug, Default)]
pub(super) struct PressureWatch {
    seen: u64,
}

impl PressureWatch {
    /// The pressure to act on, if `count` is a signal not acted on yet.
    pub(super) fn fresh(&mut self, (count, pressure): (u64, Pressure)) -> Option<Pressure> {
        if count == std::mem::replace(&mut self.seen, count) {
            return None;
        }
        (pressure != Pressure::Normal).then_some(pressure)
    }
}

impl Lifecycle {
    pub(super) fn new(profile: &crate::profile::Profile) -> Self {
        Self {
            phase: Phase::Active,
            pause_when_away: profile.pause_loads_when_hidden,
            release_surface: profile.release_surface_in_background,
            surface_released: false,
            store_suspended: false,
            pressure: Pressure::Normal,
        }
    }

    /// The step that hands `pressure` to the budget, when it is news.
    fn press(&mut self, pressure: Pressure) -> Option<Step> {
        (std::mem::replace(&mut self.pressure, pressure) != pressure)
            .then_some(Step::Pressure(pressure))
    }

    pub(super) fn phase(&self) -> Phase {
        self.phase
    }

    /// Whether a frame may be drawn — and so whether the GPU may be used.
    pub(super) fn may_draw(&self) -> bool {
        self.phase == Phase::Active && !self.surface_released
    }

    /// The application is about to stop being the one in front.
    pub(super) fn resign(&mut self) -> Vec<Step> {
        if self.phase != Phase::Active {
            return Vec::new();
        }
        self.phase = Phase::Inactive;
        if self.pause_when_away {
            vec![Step::PauseLoads]
        } else {
            Vec::new()
        }
    }

    /// The application, or its window, is no longer on screen.
    pub(super) fn background(&mut self) -> Vec<Step> {
        let was = std::mem::replace(&mut self.phase, Phase::Background);
        if !self.pause_when_away || was == Phase::Background {
            return Vec::new();
        }
        let mut steps = Vec::new();
        // Normally already held on the way through inactive; said again so
        // that a platform which skips that step still holds them.
        if was == Phase::Active {
            steps.push(Step::PauseLoads);
        }
        steps.extend(self.press(Pressure::Background));
        if !std::mem::replace(&mut self.store_suspended, true) {
            steps.push(Step::SuspendStore);
        }
        if self.release_surface && !self.surface_released {
            self.surface_released = true;
            steps.push(Step::ReleaseSurface);
        }
        steps
    }

    /// The application is on its way back, not yet in front.
    pub(super) fn foreground(&mut self) -> Vec<Step> {
        // A tab shown again is in front again, and resumes everything it held.
        if self.phase == Phase::Background && self.pause_when_away && !self.release_surface {
            return self.activate();
        }
        if self.phase == Phase::Background {
            self.phase = if self.release_surface {
                Phase::Inactive
            } else {
                // A desktop's window uncovered is in front again.
                Phase::Active
            };
        }
        if self.phase == Phase::Active {
            vec![Step::Redraw]
        } else {
            Vec::new()
        }
    }

    /// The application is in front and may draw.
    pub(super) fn activate(&mut self) -> Vec<Step> {
        let was = std::mem::replace(&mut self.phase, Phase::Active);
        let mut steps = Vec::new();
        if std::mem::take(&mut self.surface_released) {
            steps.push(Step::RestoreSurface);
        }
        if std::mem::take(&mut self.store_suspended) {
            steps.push(Step::ResumeStore);
        }
        if self.pause_when_away && was != Phase::Active {
            steps.push(Step::ResumeLoads);
        }
        // Back in front: whatever squeezed the budget while away is over.
        steps.extend(self.press(Pressure::Normal));
        steps.push(Step::Redraw);
        steps
    }

    /// The system is short of memory: a warning, unless something heavier
    /// is in force — the background squeezes harder, and a warning received
    /// there is not news.
    pub(super) fn memory_warning(&mut self) -> Vec<Step> {
        self.memory_pressure(Pressure::Warning)
    }

    /// The memory signal has gone quiet for long enough: the budget's
    /// targets come back, in front only — the background keeps its own.
    pub(super) fn calm(&mut self) -> Vec<Step> {
        if self.phase != Phase::Active {
            return Vec::new();
        }
        self.press(Pressure::Normal).into_iter().collect()
    }

    /// The system took the window's surface away: Android does, whenever the
    /// application leaves the front. Everything the background asks for, and
    /// the surface let go of whatever the profile says — it no longer exists.
    pub(super) fn surface_lost(&mut self) -> Vec<Step> {
        let mut steps = self.background();
        if !std::mem::replace(&mut self.surface_released, true) {
            steps.push(Step::ReleaseSurface);
        }
        steps
    }

    /// The system says how short of memory it is: handed to the budget as
    /// it is, a lighter signal never overriding a heavier one still in force
    /// (the background's own is the heaviest).
    pub(super) fn memory_pressure(&mut self, pressure: Pressure) -> Vec<Step> {
        if pressure == Pressure::Normal || pressure <= self.pressure {
            return Vec::new();
        }
        self.press(pressure).into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{DeviceMemory, Profile};

    fn phone() -> Lifecycle {
        Lifecycle::new(&Profile::handheld(
            DeviceMemory {
                physical: 6 << 30,
                available: Some(3 << 30),
            },
            6,
        ))
    }

    /// The round trip a person makes by switching away and back: loads held,
    /// memory trimmed and the surface gone while away; surface first, then
    /// loads, then a frame on the way back. No step may draw in between.
    #[test]
    fn away_and_back_on_a_phone() {
        let mut l = phone();
        let p = Profile::handheld(
            DeviceMemory {
                physical: 6 << 30,
                available: Some(3 << 30),
            },
            6,
        );
        assert!(l.may_draw());

        assert_eq!(l.resign(), [Step::PauseLoads]);
        assert!(!l.may_draw(), "no frame while inactive");

        assert_eq!(
            l.background(),
            [
                Step::Pressure(Pressure::Background),
                Step::SuspendStore,
                Step::ReleaseSurface
            ]
        );
        assert!(!l.may_draw());
        // Said twice, done once.
        assert!(l.background().is_empty());

        assert!(l.foreground().is_empty(), "back but not in front yet");
        assert!(!l.may_draw());
        assert_eq!(l.phase(), Phase::Inactive);

        assert_eq!(
            l.activate(),
            [
                Step::RestoreSurface,
                Step::ResumeStore,
                Step::ResumeLoads,
                Step::Pressure(Pressure::Normal),
                Step::Redraw
            ]
        );
        assert!(l.may_draw());
        let _ = p;
    }

    /// Pulling the control centre down and letting it go: inactive and back,
    /// the surface never released.
    #[test]
    fn a_brief_interruption_keeps_the_surface() {
        let mut l = phone();
        assert_eq!(l.resign(), [Step::PauseLoads]);
        assert_eq!(l.activate(), [Step::ResumeLoads, Step::Redraw]);
        // The first activation of a session is not a resumption.
        assert_eq!(phone().activate(), [Step::Redraw]);
    }

    /// Straight to the background, as a platform may report it.
    #[test]
    fn the_background_without_resigning_first_still_holds_loads() {
        let mut l = phone();
        let steps = l.background();
        assert_eq!(steps[0], Step::PauseLoads);
        assert!(steps.contains(&Step::ReleaseSurface));
    }

    /// A desktop's covered window stops drawing and keeps everything else.
    #[test]
    fn a_desktop_window_covered_and_uncovered() {
        let mut l = Lifecycle::new(&Profile::desktop_with(16 << 30));
        assert!(l.background().is_empty());
        assert!(!l.may_draw());
        assert_eq!(l.foreground(), [Step::Redraw]);
        assert!(l.may_draw());
    }

    /// A browser tab hidden and shown: loads held and residency trimmed while
    /// hidden, the surface kept, and everything resumed the moment it is
    /// shown — no activation to wait for.
    #[test]
    fn a_browser_tab_hidden_and_shown() {
        let p = Profile::browser(false, None);
        let mut l = Lifecycle::new(&p);
        assert_eq!(
            l.background(),
            [
                Step::PauseLoads,
                Step::Pressure(Pressure::Background),
                Step::SuspendStore
            ]
        );
        assert!(!l.may_draw(), "no frame while hidden");
        assert!(l.background().is_empty(), "said twice, done once");
        assert_eq!(
            l.foreground(),
            [
                Step::ResumeStore,
                Step::ResumeLoads,
                Step::Pressure(Pressure::Normal),
                Step::Redraw
            ]
        );
        assert!(l.may_draw());
        assert_eq!(l.phase(), Phase::Active);
    }

    fn android() -> (Lifecycle, Profile) {
        let p = Profile::android(
            DeviceMemory {
                physical: 3 << 30,
                available: Some(1 << 30),
            },
            8,
        );
        (Lifecycle::new(&p), p)
    }

    /// Android takes the surface away on the way out (winit's `suspended`):
    /// loads held, memory trimmed, the store written down and the surface
    /// gone, before the call returns. Back in front (`resumed`), the surface
    /// first, then the store, then loads, then a frame.
    #[test]
    fn android_takes_the_surface_away_and_gives_it_back() {
        let (mut l, _) = android();
        assert!(l.may_draw());
        assert_eq!(
            l.surface_lost(),
            [
                Step::PauseLoads,
                Step::Pressure(Pressure::Background),
                Step::SuspendStore,
                Step::ReleaseSurface
            ]
        );
        assert!(!l.may_draw(), "no frame without a surface");
        assert!(l.surface_lost().is_empty(), "said twice, done once");
        assert_eq!(
            l.activate(),
            [
                Step::RestoreSurface,
                Step::ResumeStore,
                Step::ResumeLoads,
                Step::Pressure(Pressure::Normal),
                Step::Redraw
            ]
        );
        assert!(l.may_draw());
        // And again: a person switches away more than once.
        assert!(l.surface_lost().contains(&Step::ReleaseSurface));
        assert!(l.activate().contains(&Step::RestoreSurface));
    }

    /// A surface the system destroyed is released whatever the profile says
    /// about keeping it: there is nothing left to keep.
    #[test]
    fn a_lost_surface_is_released_even_where_it_would_be_kept() {
        let mut l = Lifecycle::new(&Profile::browser(true, None));
        assert!(l.surface_lost().contains(&Step::ReleaseSurface));
        assert!(!l.may_draw());
        assert_eq!(l.activate()[0], Step::RestoreSurface);
    }

    /// Pressure, as Android grades it, handed to the budget: nothing at
    /// normal, the grade otherwise — and a lighter grade never relaxes a
    /// heavier one still in force; calm, in front, brings the budget back.
    #[test]
    fn memory_pressure_is_handed_to_the_budget_by_its_grade() {
        let (mut l, _) = android();
        assert!(l.memory_pressure(Pressure::Normal).is_empty());
        assert_eq!(
            l.memory_pressure(Pressure::Warning),
            [Step::Pressure(Pressure::Warning)]
        );
        assert_eq!(
            l.memory_pressure(Pressure::Critical),
            [Step::Pressure(Pressure::Critical)]
        );
        assert!(
            l.memory_pressure(Pressure::Warning).is_empty(),
            "a warning relaxed a critical signal"
        );
        assert_eq!(l.calm(), [Step::Pressure(Pressure::Normal)]);
        assert!(l.calm().is_empty(), "said twice, done once");
        assert_eq!(l.memory_warning(), [Step::Pressure(Pressure::Warning)]);
    }

    /// Each signal is acted on once, however often the count is read; the
    /// first read of a quiet system is not a signal.
    #[test]
    fn each_pressure_signal_is_acted_on_once() {
        use crate::profile::Pressure;
        let mut w = PressureWatch::default();
        assert_eq!(w.fresh((0, Pressure::Normal)), None);
        assert_eq!(w.fresh((1, Pressure::Critical)), Some(Pressure::Critical));
        assert_eq!(w.fresh((1, Pressure::Critical)), None, "read again");
        assert_eq!(w.fresh((2, Pressure::Critical)), Some(Pressure::Critical));
        assert_eq!(w.fresh((3, Pressure::Normal)), None, "the window hidden");
        assert_eq!(w.fresh((4, Pressure::Warning)), Some(Pressure::Warning));
    }

    /// The background squeezes the budget harder than a warning, and a
    /// warning received there is not news.
    #[test]
    fn the_background_outranks_a_warning() {
        let mut l = phone();
        assert!(l
            .background()
            .contains(&Step::Pressure(Pressure::Background)));
        assert!(l.memory_warning().is_empty(), "a warning is lighter");
        assert!(l.memory_pressure(Pressure::Critical).is_empty());
        assert!(l.calm().is_empty(), "calm while away");
        // In front, a warning is news.
        let mut l = phone();
        assert!(l
            .memory_warning()
            .contains(&Step::Pressure(Pressure::Warning)));
        assert!(l.memory_warning().is_empty(), "the same warning again");
    }
}
