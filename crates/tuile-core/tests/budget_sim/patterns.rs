// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The motion patterns of spec §12, scenario by scenario: each test is a
//! camera script and a profile, and asserts its pattern's obligations
//! beyond the invariants `Sim::check` asserts at every step.

use std::collections::{BTreeMap, BTreeSet};

use tuile_core::budget::{Class, Decisions, Micros, NeedSet, ResourceId, TileId};

use super::scenarios::{approach, home, settle};
use super::server::{Profile, Sim};
use super::world::{seen, Camera, Lens, CULL_MARGIN};

const RED: &str = "the budget core regressed";

/// No host stalls: convergence is measurable (stalls are the lossy
/// scenario's); failures are still retried.
pub fn quiet(mut p: Profile) -> Profile {
    for s in [&mut p.net.meshes, &mut p.net.imagery] {
        s.stall_permille = 0;
    }
    p
}

/// Every tile a need lists.
pub fn listed(need: &NeedSet) -> BTreeSet<TileId> {
    need.entries().map(|(_, t, _)| t).collect()
}

/// Steps `secs` seconds with the camera at `cam`, recording each step.
pub fn record(sim: &mut Sim, secs: f64, cam: Camera) -> Vec<(NeedSet, Decisions)> {
    let end = sim.now.plus(Micros((secs * 1e6) as u64));
    let mut out = Vec::new();
    while sim.now < end {
        out.push(sim.step(cam));
    }
    out
}

/// How long after `since` the view first converged.
pub fn converged_within(sim: &Sim, since: Micros) -> Micros {
    sim.converged_after(since)
        .unwrap_or_else(|| panic!("{}: never converged after {since:?}", sim.profile.name))
}

/// P5.1: across a lens change, nothing the last step drew whose tile the
/// new views still see is evicted at the change (the old draw set stays
/// until the new frustum is covered). I1 against the new lens is
/// `Sim::check`'s, every step.
fn keeps_what_is_still_seen(sim: &Sim, before: &Decisions, at: &Decisions) {
    let drawn: BTreeSet<ResourceId> = before
        .draw
        .items
        .iter()
        .flat_map(|i| std::iter::once(i.mesh).chain(i.drape.map(|x| x.resource)))
        .collect();
    for e in &at.evict {
        assert!(
            !(drawn.contains(e) && seen(&sim.last_views, e.tile(), CULL_MARGIN)),
            "P5.1: {e:?}, drawn and still in view, evicted at the change"
        );
    }
}

fn rotation(profile: Profile, seed: u64) {
    let within = profile.converge_within;
    let mut sim = Sim::new(quiet(profile), seed);
    settle(&mut sim, 8.0);
    for lens in [Lens::landscape(), Lens::portrait()] {
        let before = sim.step(home()).1;
        sim.set_lens(lens);
        let change = sim.now;
        let (_, at) = sim.step(home());
        keeps_what_is_still_seen(&sim, &before, &at);
        sim.run(8.0, &|_| home());
        let took = converged_within(&sim, change);
        assert!(
            took <= within,
            "P5.6, {}: converged {took:?} after the rotation, bound {within:?}",
            sim.profile.name
        );
    }
}

/// P5.1, P5.6: a phone turned to landscape and back never shows a hole,
/// keeps what it still sees, and converges within its bound.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-core --test budget_sim"
)]
fn a_portrait_landscape_rotation_never_shows_a_hole() {
    rotation(Profile::handheld(), 31);
}

/// P5.1, P5.6, at the handheld's low clamp.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-core --test budget_sim"
)]
fn a_portrait_landscape_rotation_never_shows_a_hole_at_the_low_clamp() {
    rotation(Profile::handheld_low(), 32);
}

/// Ten rotations two seconds apart, after a first cycle that converged in
/// both orientations. Returns the simulation, what the first cycle started
/// and when the ten rotations began.
fn ten_rotations(profile: Profile, seed: u64, store: bool) -> (Sim, BTreeSet<ResourceId>, Micros) {
    let mut sim = Sim::new(quiet(profile), seed);
    sim.net.store_fills = store;
    settle(&mut sim, 8.0);
    let mut first = BTreeSet::new();
    for lens in [Lens::landscape(), Lens::portrait()] {
        sim.set_lens(lens);
        for (_, d) in record(&mut sim, 8.0, home()) {
            first.extend(d.start.iter().map(|s| s.resource));
        }
    }
    let from = sim.now;
    for k in 0..10 {
        sim.set_lens(if k % 2 == 0 {
            Lens::landscape()
        } else {
            Lens::portrait()
        });
        sim.run(2.0, &|_| home());
    }
    (sim, first, from)
}

/// P5.4: once a first cycle has converged in both orientations, ten
/// rotations start no network load: a local store holds what either
/// orientation needed.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-core --test budget_sim"
)]
fn ten_rotations_fetch_nothing_after_the_first() {
    for (profile, seed) in [(Profile::handheld(), 33), (Profile::handheld_low(), 34)] {
        let name = profile.name;
        let (sim, first, from) = ten_rotations(profile, seed, true);
        let after = sim.log.iter().filter(|l| l.now >= from);
        let network: u32 = after.map(|l| l.stats.started_network).sum();
        assert!(!first.is_empty(), "{RED}");
        assert_eq!(network, 0, "P5.4, {name}: {network} network loads");
    }
}

/// P5.4 with no local store: the handheld holds both orientations, so the
/// rotations thrash nothing (no network fetch of what it evicted less than
/// ten seconds before).
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-core --test budget_sim"
)]
fn ten_rotations_without_a_store_thrash_nothing() {
    let (sim, first, from) = ten_rotations(Profile::handheld(), 35, false);
    let thrash: u32 = sim
        .log
        .iter()
        .filter(|l| l.now >= from)
        .map(|l| l.stats.thrash)
        .sum();
    assert!(!first.is_empty(), "{RED}");
    assert_eq!(
        thrash, 0,
        "P5.4: {thrash} refetched within 10 s of their eviction"
    );
}

/// P5.2: a resize decides on the new viewport from the step it lands: the
/// need of that step is the new lens's — narrower, nothing outside the new
/// frustum; larger at the same angles, finer.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-core --test budget_sim"
)]
fn a_resize_decides_on_the_new_viewport_the_same_step() {
    let mut sim = Sim::new(quiet(Profile::desktop()), 36);
    settle(&mut sim, 6.0);
    let (wide, _) = sim.step(home());
    sim.set_lens(Lens::desktop().narrowed(1.0, 2.0));
    let (narrow, _) = sim.step(home());
    for (t, _) in &narrow.drawn {
        assert!(
            seen(&sim.last_views, *t, CULL_MARGIN),
            "P5.2: {t:?} wanted outside the new frustum"
        );
    }
    let gone = listed(&wide).difference(&listed(&narrow)).count();
    assert!(
        gone > 0,
        "P5.2: the narrower view wants all the wider one did"
    );
    sim.run(6.0, &|_| home());
    // 3840 × 2160 at scale 1: the same angles, twice the focal.
    let (before, _) = sim.step(home());
    sim.set_lens(Lens::horizontal(3840.0, 2160.0, 1.0, 60.0));
    let (after, _) = sim.step(home());
    // Finer: more of the ground refined, each texel wanted twice as sharp.
    let sharpest = |n: &NeedSet| {
        n.drawn
            .iter()
            .filter_map(|(_, x)| x.imagery.map(|i| i.footprint.ratio()))
            .fold(0.0, f64::max)
    };
    assert!(
        after.drawn.len() > before.drawn.len(),
        "P5.2: twice the focal, no more tiles wanted ({} → {})",
        before.drawn.len(),
        after.drawn.len()
    );
    assert!(
        sharpest(&after) > 1.9 * sharpest(&before),
        "P5.2: the footprint"
    );
}

fn resize_starts(profile: Profile, seed: u64, lens: Lens) {
    let within = profile.converge_within;
    let mut sim = Sim::new(quiet(profile), seed);
    settle(&mut sim, 8.0);
    sim.set_lens(lens);
    let mut started: BTreeMap<ResourceId, u32> = BTreeMap::new();
    let steps = (within.0 / sim.profile.dt.0) as usize;
    for _ in 0..steps {
        let (need, d) = sim.step(home());
        let wanted = listed(&need);
        for s in &d.start {
            assert!(
                wanted.contains(&s.resource.tile()),
                "P5.3: {:?} started outside the new need",
                s.resource
            );
            *started.entry(s.resource).or_default() += 1;
        }
    }
    let failed: u32 = sim
        .log
        .iter()
        .rev()
        .take(steps)
        .map(|l| l.stats.failed + l.stats.cancelled)
        .sum();
    let twice = started.values().filter(|n| **n > 1).count() as u32;
    assert!(
        twice <= failed,
        "P5.3: {twice} resources started twice, {failed} failures or cancels"
    );
    assert!(!started.is_empty(), "{RED}: the change asked for nothing");
}

/// P5.3: after a resize, every start is in the new need and none is
/// repeated without a failure in between; I14, nothing held fetched again,
/// is `Sim::check`'s.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-core --test budget_sim"
)]
fn a_resize_starts_nothing_outside_the_new_need() {
    resize_starts(
        Profile::desktop(),
        37,
        Lens::horizontal(3840.0, 2160.0, 1.0, 60.0),
    );
    resize_starts(Profile::handheld(), 38, Lens::landscape());
}

/// P5.6, P3.2: a split screen (full width, then a half, then a third)
/// brings every class under its target within the bound, and under
/// `max(target + overflow, protected)` before, without a hole.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-core --test budget_sim"
)]
fn a_split_screen_shrink_comes_back_under_target_without_a_hole() {
    let profile = quiet(Profile::handheld());
    let within = profile.converge_within;
    let mut sim = Sim::new(profile, 39);
    settle(&mut sim, 8.0);
    for den in [2.0, 3.0] {
        sim.set_lens(Lens::portrait().narrowed(1.0, den));
        let change = sim.now;
        sim.run(8.0, &|_| home());
        for l in sim.log.iter().filter(|l| l.now >= change) {
            let s = &l.stats;
            for c in Class::ALL {
                let bound = if l.now >= change.plus(within) {
                    s.target[c].max(s.protected[c])
                } else {
                    s.target[c].plus(s.overflow[c]).max(s.protected[c])
                };
                assert!(
                    s.gpu[c] <= bound,
                    "P3.2 after a split to 1/{den}: {c:?} at {} MiB, bound {} MiB",
                    s.gpu[c].0 >> 20,
                    bound.0 >> 20
                );
            }
        }
        converged_within(&sim, change);
    }
}

fn scale_digests(change: bool, seed: u64) -> Vec<u64> {
    let mut sim = Sim::new(quiet(Profile::desktop()), seed);
    sim.run(6.0, &approach);
    if change {
        sim.set_lens(Lens::desktop().at_scale(2.0));
    }
    sim.run(3.0, &|_| home());
    if change {
        sim.set_lens(Lens::desktop());
    }
    sim.run(3.0, &|_| home());
    sim.log.iter().map(|l| l.digest).collect()
}

/// P5.5, D-L1: a scale change at constant logical size — a window moved
/// to a 2× display and back — decides nothing new: the decisions are the
/// same, step for step, bit for bit.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-core --test budget_sim"
)]
fn a_scale_change_alone_decides_nothing_new() {
    let still = scale_digests(false, 40);
    let moved = scale_digests(true, 40);
    assert_eq!(still.len(), moved.len());
    let first = still.iter().zip(&moved).position(|(a, b)| a != b);
    assert_eq!(first, None, "P5.5: the decisions moved at step {first:?}");
    assert!(Lens::desktop().at_scale(2.0) != Lens::desktop(), "{RED}");
}
