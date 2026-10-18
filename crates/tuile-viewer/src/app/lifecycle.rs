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
//!   (tens of megabytes at full resolution and four samples) and asks the
//!   server to trim its residency to a quarter of the budget — never the
//!   tiles the view draws or their ancestors, so the globe comes back covered;
//! - **activating** brings the surface back first, then opens the gate and
//!   asks for a frame: the surface exists before anything draws into it.
//!
//! A desktop only ever sees its window covered and uncovered, and does what it
//! always did: stops drawing while covered.
//!
//! A browser tab sits between the two. Hidden, it holds its loads and trims
//! like a phone — a tab in the background is still the user's memory — but
//! keeps its surface, which a page cannot rebuild cheaply (a WebGL2 device is
//! bound to its canvas), and comes straight back to the front when shown
//! again: a page has no separate "activate".
//!
//! Pure: events in, [`Step`]s out. The window carries the steps out.

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
    /// Ask the server to let go of residency beyond this many bytes.
    Trim(usize),
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
    /// Bytes to trim to on a memory warning, and in the background.
    trim_on_warning: usize,
    trim_in_background: usize,
}

impl Lifecycle {
    pub(super) fn new(profile: &crate::profile::Profile) -> Self {
        Self {
            phase: Phase::Active,
            pause_when_away: profile.pause_loads_when_hidden,
            release_surface: profile.release_surface_in_background,
            surface_released: false,
            store_suspended: false,
            trim_on_warning: profile.trim_on_warning(),
            trim_in_background: profile.trim_in_background(),
        }
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
        steps.push(Step::Trim(self.trim_in_background));
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
        steps.push(Step::Redraw);
        steps
    }

    /// The system is short of memory.
    pub(super) fn memory_warning(&self) -> Vec<Step> {
        vec![Step::Trim(self.trim_on_warning)]
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
                Step::Trim(p.trim_in_background()),
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
                Step::Redraw
            ]
        );
        assert!(l.may_draw());
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
        let mut l = Lifecycle::new(&Profile::desktop());
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
                Step::Trim(p.trim_in_background()),
                Step::SuspendStore
            ]
        );
        assert!(!l.may_draw(), "no frame while hidden");
        assert!(l.background().is_empty(), "said twice, done once");
        assert_eq!(
            l.foreground(),
            [Step::ResumeStore, Step::ResumeLoads, Step::Redraw]
        );
        assert!(l.may_draw());
        assert_eq!(l.phase(), Phase::Active);
    }

    #[test]
    fn a_memory_warning_trims_to_half_the_budget() {
        let l = phone();
        let p = Profile::handheld(
            DeviceMemory {
                physical: 6 << 30,
                available: Some(3 << 30),
            },
            6,
        );
        assert_eq!(
            l.memory_warning(),
            [Step::Trim(p.resident_budget_bytes / 2)]
        );
    }
}
