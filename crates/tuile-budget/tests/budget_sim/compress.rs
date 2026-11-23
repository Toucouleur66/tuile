// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! CB9 in the simulation: display at once, compress what lasts
//! (`docs/explorations/compress-what-lasts.md`). Every step is held to
//! `Sim::check`'s compression invariants besides these.

use tuile_budget::{Class, Micros, ResourceId};

use super::motion::fast_dive;
use super::scenarios::home;
use super::server::{Profile, Sim};
use super::world::{cost_for, SimPlan};

const RED: &str = "the compression in place regressed";

fn clean(mut p: Profile) -> Profile {
    p.net = p.net.clean();
    p
}

/// Whether the step's drapes were over their low watermark: where the age
/// is waived (law 4).
fn pressed(l: &super::server::StepLog, low_percent: u64) -> bool {
    let target = l.stats.target[Class::Drape];
    l.stats.gpu[Class::Drape].0 > target.0 / 100 * low_percent
}

/// **A still view compresses what lasts, and nothing before it lasted:**
/// on a desktop holding BC7, no compression is decided in the first two
/// seconds a drape is drawable unless the drapes are over their low
/// watermark; by the end, the drapes drawn are held compressed — but the
/// few the encoder refused.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn a_still_view_compresses_what_lasts_and_nothing_fresh() {
    let profile = clean(Profile::desktop_bc7());
    let low = u64::from(profile.budget.watermarks.low_percent);
    let mut sim = Sim::new(profile, 241);
    sim.run(8.0, &|_| home());
    let first = sim
        .log
        .iter()
        .find(|l| l.stats.compress.started > 0)
        .map(|l| (l.now, pressed(l, low)));
    let Some((at, under_pressure)) = first else {
        panic!("nothing compressed in eight still seconds: {RED}");
    };
    assert!(
        at >= Micros::from_secs(2) || under_pressure,
        "compressed at {at:?}, before anything lasted, with room"
    );
    let last = sim.log.last().unwrap();
    assert_eq!(last.stats.compress.in_flight, 0, "all done once still");
    // What the drapes drawn hold, against them held RGBA8 and compressed.
    let drawn: Vec<ResourceId> = sim.drawn_drapes();
    let (held, rgba8, blocks) = drawn.iter().fold((0u64, 0u64, 0u64), |a, r| {
        let c = cost_for(*r, SimPlan::Gpu);
        (
            a.0 + sim.gpu.drawable.get(r).map_or(0, |b| b.0),
            a.1 + c.gpu.0,
            a.2 + c.compressed.0,
        )
    });
    assert!(!drawn.is_empty());
    assert!(
        held * 10 <= blocks * 11 + rgba8 / 20,
        "drawn drapes hold {held}: {blocks} compressed, {rgba8} as RGBA8 — {RED}"
    );
}

/// **Under pressure, compressing comes before evicting:** a phone short of
/// memory diving fast holds more of what it loaded when its drapes can be
/// compressed — fewer evictions than the same dive held RGBA8, its picture
/// as whole (no hole, checked every step).
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn under_pressure_compression_spares_evictions() {
    let run = |plan: SimPlan| {
        let p = clean(Profile::handheld_low()).compressed("handheld-low-cb9", plan);
        let mut sim = Sim::new(p, 242);
        sim.run(6.0, &fast_dive);
        sim.run(4.0, &|_| fast_dive(6.0));
        let evicted: u64 = sim.log.iter().map(|l| u64::from(l.stats.evicted)).sum();
        let compressed: u64 = sim
            .log
            .iter()
            .map(|l| u64::from(l.stats.compress.started))
            .sum();
        (evicted, compressed)
    };
    let (rgba8, none) = run(SimPlan::Rgba8);
    let (gpu, compressed) = run(SimPlan::Gpu);
    assert_eq!(none, 0);
    assert!(compressed > 0, "{RED}");
    assert!(
        gpu < rgba8,
        "evictions: {gpu} compressing, {rgba8} held RGBA8"
    );
}
