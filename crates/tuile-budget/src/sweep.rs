// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What the proactive sweep may release (Laurent, 2026-11-24).
//!
//! The sweep runs when a class (or the pool) is over its high watermark —
//! its target — and takes it down to its low one. Two parts, in the one
//! eviction order:
//!
//! - **back within the target**: what is over the target goes, whatever it
//!   is (but the kept set, the floor, transit) — reason `fit_target`, as
//!   I2's pass;
//! - **under the target, down to the low watermark: proactive.** Its law:
//!   **a proactive release takes only from what the view will not need
//!   soon** — never a copy the need wants this step, at any urgency (the
//!   ring is the need's own prediction of what comes next), and never one
//!   it wanted within the specification's horizon, its last camera poses
//!   (`Params::sweep_keep_poses`), the likeliest to be wanted again within
//!   it. A pose is a step of the decider, whatever its time: the law counts
//!   steps and reads no clock (Laurent, 2026-11-24 — the horizon was five
//!   seconds before; it is 300 poses, five seconds at 60 steps a second).
//!   What the law keeps stays; the room a delivery needs is made by
//!   admission under its own law (worth less than the work), and the class
//!   may sit between its watermarks meanwhile. Reason `headroom`.
//!
//! The proactive part runs first, so the target is reached by what the law
//! releases before anything it keeps goes.
//!
//! Before it (#289 at a7a4f720), the sweep walked the eviction order with no
//! regard for the need: a ring preload or an ancestor wanted this step and
//! not drawn went first when its key was the lowest, and was loaded again
//! the steps after — on a view swinging between two headings under a tight
//! budget, every copy the sweep took was a tile the need listed, and nearly
//! every one was loaded again (`budget_progressive`, the pendulum test).
//!
//! The walk it bounds: the copies needed at the step's time or the step
//! before (the grouped ones) are within any horizon of two poses or more,
//! so the sweep walks the filed copies alone; and it passes over at most the
//! specification's `release_visits` copies the law keeps before it leaves
//! the rest to the next step — a still view's step does not grow with
//! what it holds. A kept copy at the head of the order leaves it once its
//! horizon has run.
//!
//! Not bound by it: a memory signal (the sweep is then the answer to the
//! system's demand, reason `pressure`), the way back within the target, and
//! I2's pass (the unprotected bytes fit in the target), laws of their own.

/// The sweep's verdict on a copy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Verdict {
    /// It goes.
    Release,
    /// It stays; the walk goes on.
    Keep,
    /// It stays, and the walk has passed over as many as it may this step.
    Stop,
}

/// The law, at one step, with what its walk has passed over.
#[derive(Clone, Copy, Debug)]
pub(super) struct SweepLaw {
    /// How many camera poses a wanted copy is kept after the last that
    /// wanted it.
    keep: u64,
    /// Whether the law binds: no memory signal.
    binds: bool,
    /// The kept copies the walk may still pass over.
    passes: u32,
}

impl SweepLaw {
    /// The law keeping for `keep` poses, passing over at most `passes`
    /// kept copies; it binds when `normal` (no memory signal).
    pub(super) fn new(keep: u32, passes: u16, normal: bool) -> Self {
        SweepLaw {
            keep: u64::from(keep),
            binds: normal,
            passes: u32::from(passes),
        }
    }

    /// Whether the walk visits the copies needed at the step's time or the
    /// step before: only when the law does not bind, or keeps nothing past
    /// the pose before.
    pub(super) fn walks_grouped(&self) -> bool {
        !self.binds || self.keep <= 1
    }

    /// Whether the sweep may release a copy the need wants this step
    /// (`wanted_now`) or not, last wanted `since` poses ago (`None`: longer
    /// ago than the horizon).
    pub(super) fn releases(&self, wanted_now: bool, since: Option<u64>) -> bool {
        !self.binds || (!wanted_now && since.is_none_or(|n| n >= self.keep))
    }

    /// The verdict on a copy, counting what the walk passes over.
    pub(super) fn judge(&mut self, wanted_now: bool, since: Option<u64>) -> Verdict {
        if self.releases(wanted_now, since) {
            return Verdict::Release;
        }
        self.passes = self.passes.saturating_sub(1);
        if self.passes == 0 {
            Verdict::Stop
        } else {
            Verdict::Keep
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEEP: u32 = 10;

    #[test]
    fn a_copy_wanted_now_is_never_released() {
        let law = SweepLaw::new(KEEP, 16, true);
        assert!(!law.releases(true, None));
    }

    /// Laurent, 2026-11-24: wanted N − 1 poses ago it stays, N + 1 it goes;
    /// N is the first pose past the horizon.
    #[test]
    fn a_copy_wanted_within_the_horizon_poses_is_kept_and_released_after_it() {
        let law = SweepLaw::new(KEEP, 16, true);
        let n = u64::from(KEEP);
        assert!(!law.releases(false, Some(0)));
        assert!(!law.releases(false, Some(n - 1)));
        assert!(law.releases(false, Some(n)));
        assert!(law.releases(false, Some(n + 1)));
        assert!(law.releases(false, None), "forgotten: past the horizon");
    }

    #[test]
    fn a_memory_signal_is_not_bound() {
        let law = SweepLaw::new(KEEP, 16, false);
        assert!(law.releases(true, Some(0)));
    }

    #[test]
    fn a_zero_horizon_keeps_only_what_is_wanted_now() {
        let law = SweepLaw::new(0, 16, true);
        assert!(law.releases(false, Some(0)));
        assert!(!law.releases(true, Some(0)));
        assert!(law.walks_grouped());
    }

    #[test]
    fn a_one_pose_horizon_walks_the_copies_of_the_pose_before() {
        let law = SweepLaw::new(1, 16, true);
        assert!(!law.releases(false, Some(0)));
        assert!(law.releases(false, Some(1)));
        assert!(law.walks_grouped());
        assert!(!SweepLaw::new(2, 16, true).walks_grouped());
    }

    #[test]
    fn the_walk_passes_over_a_bounded_number_of_kept_copies() {
        let mut law = SweepLaw::new(KEEP, 3, true);
        assert!(!law.walks_grouped());
        assert_eq!(law.judge(true, None), Verdict::Keep);
        assert_eq!(law.judge(false, None), Verdict::Release);
        assert_eq!(law.judge(false, Some(1)), Verdict::Keep);
        assert_eq!(law.judge(true, None), Verdict::Stop);
    }
}
