// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What the headless tests share: the GPU gate, one run at a time, and the
//! obligations a frame is held to.
//!
//! # The gate
//!
//! A machine without a usable adapter skips, with a printed reason — a
//! laptop, a container and a CI runner do not all have a GPU. Set
//! `TUILE_REQUIRE_GPU=1` to turn that skip into a failure, which is what a
//! machine with a GPU should run with: a test that silently skips for a year
//! is a test that does not exist.

#![allow(dead_code, reason = "each test binary uses its own part of this")]

use std::sync::{Mutex, MutexGuard};

use tuile_viewer::control::{Headless, Refused, Start, State};

/// The run's numbers are the process's: one run at a time.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

/// A run, and the turn it holds.
pub struct Run {
    pub run: Headless,
    pub turn: MutexGuard<'static, ()>,
}

impl std::ops::Deref for Run {
    type Target = Headless;
    fn deref(&self) -> &Headless {
        &self.run
    }
}

impl std::ops::DerefMut for Run {
    fn deref_mut(&mut self) -> &mut Headless {
        &mut self.run
    }
}

/// Waits for the turn, and starts a run from `start` — or says why not and
/// returns `None` where there is no GPU and none is required.
pub fn start(start: Start) -> Option<Run> {
    let turn = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    match Headless::start(start) {
        Ok(run) => Some(Run { run, turn }),
        Err(Refused::NoGpu(why)) => {
            assert!(
                std::env::var("TUILE_REQUIRE_GPU").is_err(),
                "TUILE_REQUIRE_GPU is set and there is no usable adapter: {why}"
            );
            eprintln!("skipping GPU test: no usable adapter ({why})");
            None
        }
        #[allow(clippy::panic, reason = "a run that cannot start fails its test")]
        Err(why) => panic!("the run did not start: {why}"),
    }
}

/// Starts the run again over its store: the application killed and opened
/// again. The turn is kept.
pub fn relaunch(run: Run) -> Run {
    let Run { run, turn } = run;
    Run {
        run: run.relaunch().expect("the run starts again"),
        turn,
    }
}

/// What a stretch of frames came to, against the specification's per-frame
/// obligations — the numbers a red test reports, so that its baseline is in
/// its failure.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Report {
    pub frames: usize,
    pub drawn: usize,
    /// Frames with ground left the clear colour, in any eye; the worst count
    /// of such pixels; the first frame it happened on.
    pub black_frames: usize,
    pub worst_black: u64,
    pub first_black: Option<u64>,
    /// Frames with selected ground drawn by nothing (`frame_lost`); the most
    /// tiles; the first frame.
    pub lost_frames: usize,
    pub worst_lost: u64,
    pub first_lost: Option<u64>,
    /// Frames drawn with coarser ancestors standing in; the most tiles.
    pub coarser_frames: usize,
    pub worst_coarser: u64,
    /// The most decoded bytes the server held, and the frames over the
    /// profile's budget.
    pub worst_resident_bytes: u64,
    pub over_budget_frames: usize,
    /// The most GPU bytes held, meshes and imagery.
    pub worst_gpu_bytes: u64,
}

/// Reads `states` against the obligations, with `budget` the profile's
/// resident bytes.
pub fn report(states: &[State], budget: u64) -> Report {
    let mut r = Report {
        frames: states.len(),
        ..Report::default()
    };
    for s in states {
        let m = &s.metrics;
        r.worst_resident_bytes = r.worst_resident_bytes.max(m.resident_bytes);
        r.worst_gpu_bytes = r.worst_gpu_bytes.max(m.mesh_bytes + m.texture_bytes);
        if m.resident_bytes > budget {
            r.over_budget_frames += 1;
        }
        if !s.drew {
            continue;
        }
        r.drawn += 1;
        let black = s.black();
        if black > 0 {
            r.black_frames += 1;
            r.worst_black = r.worst_black.max(black);
            r.first_black.get_or_insert(s.frame);
        }
        if m.frame_lost > 0 {
            r.lost_frames += 1;
            r.worst_lost = r.worst_lost.max(m.frame_lost);
            r.first_lost.get_or_insert(s.frame);
        }
        if m.frame_coarser > 0 {
            r.coarser_frames += 1;
            r.worst_coarser = r.worst_coarser.max(m.frame_coarser);
        }
    }
    r
}
