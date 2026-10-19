// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! **The motion patterns, held to the budget specification's obligations on
//! today's viewer.**
//!
//! Each pattern of `control::patterns` is run headless on the real
//! application and the fixture globe, and every frame drawn is held to what
//! the specification asks of every frame:
//!
//! - **no ground left black**, in any eye — the scan of the picture itself;
//! - **no selected ground drawn by nothing** (`frame_lost`): what the
//!   backstop shell then hides is a hole all the same;
//! - **the server within the profile's budget** (`resident_bytes`), the one
//!   class limit today's viewer has;
//!
//! and the picture must then **settle within the convergence bound**: two
//! seconds on a desktop, four on a phone, at the run's refresh.
//!
//! What the network is in these runs, stated: the server is run to
//! quiescence between two frames, and the fixture answers at once — so a
//! frame bound there measures the frame's own budget: uploads, the
//! traversal, the residency — except in the runs over a slow network, where
//! every tile takes `patterns::SLOW_NETWORK` of the run's time to arrive.
//!
//! A pattern today's viewer does not meet is kept, red and ignored, with what
//! fails and the numbers measured: the budget's cut-over is what turns it
//! green. Every run prints its report either way.

mod support;

use support::{report, Report};
use tuile_viewer::control::{patterns, patterns::Pattern, Settled, State};

/// Runs `pattern`: up, warmed, moved, then given its bound to settle. The
/// states of the motion and the settling, and the settling's outcome.
fn run(pattern: Pattern) -> Option<(Vec<State>, Settled, Report)> {
    let name = pattern.name;
    let mut run = support::start(pattern.start)?;
    if pattern.warm > 0 {
        let warmed = run.settle(pattern.warm);
        assert!(warmed.settled, "{name}: never came up: {:?}", run.state());
    }
    let first = run.history().len();
    run.play(&pattern.motion);
    let settled = run.settle(pattern.settle);
    let states = run.history()[first..].to_vec();
    let r = report(&states, run.limits().resident_bytes);
    eprintln!("{name}: {settled:?} {r:?}");
    Some((states, settled, r))
}

/// What every pattern is held to: see the module.
fn holds(name: &str, settled: Settled, r: &Report) {
    assert!(r.drawn > 0, "{name}: nothing drawn");
    assert_eq!(r.black_frames, 0, "{name}: ground left black: {r:?}");
    assert_eq!(r.lost_frames, 0, "{name}: holes in the selection: {r:?}");
    assert_eq!(r.over_budget_frames, 0, "{name}: over the budget: {r:?}");
    assert!(
        settled.settled,
        "{name}: not settled within {} frames: {r:?}",
        settled.frames
    );
}

fn check(pattern: Pattern) {
    let name = pattern.name;
    if let Some((_, settled, r)) = run(pattern) {
        holds(name, settled, &r);
    }
}

// --- P1: progressive loading ---------------------------------------------

#[test]
fn p1_a_cold_start_covers_the_ground_and_settles_in_time() {
    check(patterns::cold_start());
}

/// Started again over the store the first run filled: nothing it holds is
/// asked of the network again, and the picture settles in time.
#[test]
fn p1_a_warm_start_is_served_by_the_store_and_settles_in_time() {
    let pattern = patterns::cold_start();
    let Some(mut cold) = support::start(pattern.start.clone()) else {
        return;
    };
    assert!(cold.settle(10 * pattern.settle).settled);
    assert!(!cold.store().is_empty(), "the cold run stored nothing");
    let mut warm = support::relaunch(cold);
    let settled = warm.settle(pattern.settle);
    let r = report(warm.history(), warm.limits().resident_bytes);
    let state = warm.state();
    eprintln!(
        "{}: warm {settled:?} {r:?} network {:?} store hits {} misses {}",
        pattern.name, state.network, state.metrics.store_hits, state.metrics.store_misses
    );
    assert_eq!(
        state.network,
        (0, 0),
        "the network was asked for what the store holds"
    );
    assert!(state.metrics.store_hits > 0, "the store served nothing");
    holds("P1 warm start", settled, &r);
}

/// Red on today's viewer, measured 2026-10-19 on an M-series Mac (Metal):
/// a phone opens on the shell and draws from its first frame, before its
/// floor (the pinned level 3) is on the GPU — 114 of 122 frames drew
/// selected ground with nothing on its ancestor chain (worst 10 tiles, first
/// at frame 2), 99 with coarser ground (worst 4); no black pixel (the shell
/// covers them); settled in 122 frames, under the 240 allowed; 66 MB
/// resident against 160 MiB.
#[test]
#[ignore = "red until budget cut-over: a phone's first frames draw selected ground before its floor is resident (frame_lost on 114 of 122 frames, worst 10)"]
fn p1_a_cold_start_at_low_memory_covers_the_ground_and_settles_in_time() {
    check(patterns::cold_start_at_low_memory());
}

/// Red on today's viewer, measured 2026-10-19 (Metal): a phone over a
/// 150 ms network — 3 of 232 frames drew selected ground with nothing on its
/// ancestor chain (one tile each, from frame 209); no black pixel; settled
/// in 232 frames, under the 240 allowed.
#[test]
#[ignore = "red until budget cut-over: a phone over a 150 ms network draws selected ground with nothing under it (frame_lost on 3 of 232 frames, from frame 209)"]
fn p1_a_cold_start_over_a_slow_network_covers_the_ground_and_settles_in_time() {
    check(patterns::cold_start_over_a_slow_network());
}

// --- P2: zoom in ----------------------------------------------------------

#[test]
fn p2_a_slow_zoom_in_never_uncovers_the_ground() {
    check(patterns::zoom_in_slow());
}

#[test]
fn p2_a_fast_zoom_in_never_uncovers_the_ground() {
    check(patterns::zoom_in_fast());
}

#[test]
fn p2_a_fast_zoom_in_over_a_slow_network_never_uncovers_the_ground() {
    check(patterns::zoom_in_fast_over_a_slow_network());
}

/// Down to where the deepest level is chosen: the selection reaches it.
#[test]
fn p2_a_zoom_to_the_deepest_level_reaches_it_covered() {
    let pattern = patterns::zoom_in_to_the_deepest_level();
    let name = pattern.name;
    let Some((states, settled, r)) = run(pattern) else {
        return;
    };
    holds(name, settled, &r);
    let last = states.last().expect("states");
    assert!(last.metrics.frame_exact > 0, "{last:?}");
}

// --- P3: zoom out -----------------------------------------------------------

#[test]
fn p3_a_zoom_out_to_200_km_and_back_never_uncovers_the_ground() {
    check(patterns::zoom_out_and_back());
}

// --- P4: rotation -----------------------------------------------------------

#[test]
fn p4_a_yaw_in_place_never_uncovers_the_ground() {
    check(patterns::yaw_in_place());
}

#[test]
fn p4_a_yaw_at_head_rate_never_uncovers_the_ground() {
    check(patterns::yaw_at_head_rate());
}

#[test]
fn p4_a_yaw_at_head_rate_over_a_slow_network_never_uncovers_the_ground() {
    check(patterns::yaw_at_head_rate_over_a_slow_network());
}

#[test]
fn p4_an_orbit_about_a_target_never_uncovers_the_ground() {
    check(patterns::orbit());
}

#[test]
fn p4_a_tilt_toward_the_horizon_never_uncovers_the_ground() {
    check(patterns::tilt());
}

// --- P5: orientation --------------------------------------------------------

#[test]
fn p5_portrait_and_landscape_never_uncover_the_ground() {
    check(patterns::portrait_and_landscape());
}

#[test]
fn p5_a_resize_and_a_change_of_density_never_uncover_the_ground() {
    check(patterns::resize_and_rescale());
}

// --- P6: stereo -------------------------------------------------------------

/// Both eyes, every frame: neither is left a hole the other lacks.
#[test]
fn p6_two_eyes_at_90_hz_never_uncover_the_ground_in_either() {
    let pattern = patterns::stereo_head_motion();
    let name = pattern.name;
    let Some((states, settled, r)) = run(pattern) else {
        return;
    };
    assert!(states.iter().all(|s| s.eyes.len() == 2));
    holds(name, settled, &r);
}
