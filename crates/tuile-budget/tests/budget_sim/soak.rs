// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The soak (I6, I7): a long scripted tour; the slopes must be flat.

use tuile_budget::{Bytes, Micros};

use super::detmath;
use super::server::{Profile, Sim};
use super::world::{Camera, SIDE};

/// The memory-pressure scenario (O1's baseline for the eviction index,
/// O1.2): the desktop on 384 MiB of its 2 GiB, the LOD relaxation capped at
/// none (`relax.max_scale` 1000), so the view keeps its tiles instead of
/// shedding them, and the tour's resident set stays over its target: the
/// sweep builds and sorts the eviction order on nearly every step, and
/// deliveries make room. Retargeting alone, the relaxation shrinks the view
/// to a few dozen tiles and the scenario measures nothing.
pub fn pressure_profile() -> Profile {
    let mut p = Profile::desktop();
    p.name = "desktop-pressure";
    p.budget.relax.max_scale = 1_000;
    p
}

/// [`pressure_profile`]'s whole budget, in MiB.
pub const PRESSURE_MIB: u64 = 384;

/// Puts a simulation on [`pressure_profile`] under its memory pressure: its
/// budget retargeted to [`PRESSURE_MIB`].
pub fn under_pressure(sim: &mut Sim) {
    sim.budget.retarget(Bytes::mib(PRESSURE_MIB));
}

/// The pressure scenario's camera: the tour's orbit (its first minute, a
/// full turn, so it loops without a jump), forever. The whole tour under
/// pressure is [`the_tour_under_pressure_keeps_its_tracked_state_bounded`].
pub fn pressure_tour(t: f64) -> Camera {
    tour(t % 60.0)
}

/// I7 under memory pressure: the whole tour, not looped — its pan and its
/// zoom-out shed what the orbit loaded — on [`pressure_profile`] retargeted
/// to [`PRESSURE_MIB`], every invariant checked. The decoded tier follows the
/// retarget as the GPU limits do: the copies an eviction demotes to are
/// bounded by a tier sized to the device the budget runs on (a quarter of
/// it, as `BudgetConfig::interactive` configures the tier), not to the one
/// configured. Before that, a 384 MiB budget kept the configured 512 MiB of
/// demoted copies, every one a tracked resource, and the tracked state broke
/// I7 at t ≈ 105 s (1 968 tracked, bound 1 284).
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn the_tour_under_pressure_keeps_its_tracked_state_bounded() {
    let mut sim = Sim::new(pressure_profile(), 11);
    under_pressure(&mut sim);
    let decoded_target = Bytes::mib(PRESSURE_MIB / 4);
    let end = Micros::from_secs(180);
    let mut report = Micros::from_secs(15);
    let (mut tracked_max, mut decoded_max) = (0u32, Bytes::ZERO);
    // Evictions, decoded releases, local re-reads within 10 s of an
    // eviction, network re-fetches within 10 s of one: what the tier costs.
    let (mut evicted, mut released, mut rereads, mut thrash) = (0u64, 0u64, 0u64, 0u64);
    while sim.now < end {
        let t = sim.now.0 as f64 / 1e6;
        let (_, d) = sim.step(tour(t));
        let decoded = sim.budget.ledger().decoded;
        // Copies in transit to the GPU are not released; everything else
        // over the tier's target is, within the step.
        assert!(
            decoded <= decoded_target.plus(d.stats.in_transit),
            "I7: {decoded:?} decoded at t = {t:.1} s, the tier's target {decoded_target:?} \
             ({:?} in transit)",
            d.stats.in_transit
        );
        tracked_max = tracked_max.max(d.stats.tracked);
        decoded_max = decoded_max.max(decoded);
        evicted += u64::from(d.stats.evicted);
        released += u64::from(d.stats.released);
        rereads += u64::from(d.stats.rereads);
        thrash += u64::from(d.stats.thrash);
        if sim.now >= report {
            eprintln!(
                "pressure tour ({PRESSURE_MIB} MiB) t = {t:.0} s: tracked {}, decoded {:.1} MB, \
                 evicted {evicted}, released {released}, re-read {rereads}, thrash {thrash}",
                d.stats.tracked,
                decoded.0 as f64 / 1e6
            );
            report = report.plus(Micros::from_secs(15));
        }
    }
    eprintln!(
        "pressure tour ({PRESSURE_MIB} MiB): tracked at most {tracked_max}, decoded at most \
         {:.1} MB, evicted {evicted}, released {released}, re-read {rereads}, thrash {thrash}, \
         {} steps",
        decoded_max.0 as f64 / 1e6,
        sim.log.len()
    );
}

/// A tour that orbits, pans, zooms and turns, looping every four minutes.
pub fn tour(t: f64) -> Camera {
    let u = t % 240.0;
    let base = Camera {
        x: 0.62 * SIDE,
        y: 0.5 * SIDE,
        h: 2_500.0,
        heading: 0.0,
        pitch: 30f64.to_radians(),
    };
    match u {
        u if u < 60.0 => Camera {
            heading: u / 60.0 * std::f64::consts::TAU,
            ..base
        },
        u if u < 120.0 => Camera {
            x: base.x + (u - 60.0) * 400.0,
            ..base
        },
        u if u < 180.0 => Camera {
            x: base.x + 24_000.0,
            h: 2_500.0
                * detmath::exp2(detmath::log2(40.0) * (1.0 - ((u - 120.0) / 30.0 - 1.0).abs())),
            ..base
        },
        u => Camera {
            x: base.x + 24_000.0 * (1.0 - (u - 180.0) / 60.0),
            heading: (u - 180.0) / 60.0 * std::f64::consts::PI,
            ..base
        },
    }
}

struct Window {
    p95_ms: f64,
    decide_us: f64,
    tracked: f64,
    heap: f64,
}

fn window(sim: &Sim, from: Micros, to: Micros, decide: &[(Micros, f64)]) -> Window {
    let mut lat: Vec<f64> = sim
        .waits
        .iter()
        .filter(|(t, _)| *t >= from && *t < to)
        .map(|(_, l)| *l)
        .collect();
    lat.sort_by(f64::total_cmp);
    let p95_ms = lat.get(lat.len() * 95 / 100).copied().unwrap_or(0.0);
    let logs: Vec<_> = sim
        .log
        .iter()
        .filter(|l| l.now >= from && l.now < to)
        .collect();
    let n = logs.len().max(1) as f64;
    let ds: Vec<f64> = decide
        .iter()
        .filter(|(t, _)| *t >= from && *t < to)
        .map(|(_, d)| *d)
        .collect();
    Window {
        p95_ms,
        decide_us: ds.iter().sum::<f64>() / ds.len().max(1) as f64,
        // Entries beyond what the byte budgets hold: wanted, loading,
        // failed. Residency itself is bounded by bytes (checked every step);
        // this overhead is what would leak.
        tracked: logs
            .iter()
            .map(|l| f64::from(l.stats.tracked.saturating_sub(l.held)))
            .sum::<f64>()
            / n,
        heap: logs
            .iter()
            .map(|l| f64::from(l.stats.heap_len))
            .sum::<f64>()
            / n,
    }
}

fn soak(minutes: u64) {
    let mut sim = Sim::new(Profile::desktop(), 99);
    let end = Micros::from_secs(minutes * 60);
    while sim.now < end {
        let t = sim.now.0 as f64 / 1e6;
        sim.step(tour(t));
    }
    // `decide` alone: the budget's own cost per step, not the simulation's.
    let decide = sim.decide_us.clone();
    // The drapes fetched over the tour, from the network and locally, for
    // the record (`--nocapture`): what a change of the drape wants costs.
    let drape_starts = sim.log.iter().fold([0u64; 2], |a, l| {
        let s = l.stats.starts[tuile_budget::Class::Drape];
        [a[0] + u64::from(s[0]), a[1] + u64::from(s[1])]
    });
    eprintln!(
        "soak {minutes} min: drape starts {} local, {} network",
        drape_starts[0], drape_starts[1]
    );
    // The first window warms the caches; the slope is measured from the
    // second one on.
    let span = Micros::from_secs(60).0 * minutes.min(5) / 5;
    let first = window(&sim, Micros(span), Micros(2 * span), &decide);
    let last = window(&sim, end.since(Micros(span)), end, &decide);
    // The slopes, for the record (`--nocapture`).
    eprintln!(
        "soak {minutes} min: wait p95 {:.1} -> {:.1} ms; decide {:.1} -> {:.1} us; \
         tracked beyond residency {:.0} -> {:.0}; heap {:.0} -> {:.0}; steps {}",
        first.p95_ms,
        last.p95_ms,
        first.decide_us,
        last.decide_us,
        first.tracked,
        last.tracked,
        first.heap,
        last.heap,
        sim.log.len()
    );
    // The waits split by their kind, as the meter tells them
    // (`tuile_budget::waits`): first loads, and reloads of a mesh evicted
    // under 5 s or 30 s before, by `Decisions::evict` — told from the
    // outside, so the same on any budget (`--nocapture`).
    let outside = |from: Micros, to: Micros| {
        let mut by: [Vec<f64>; 3] = Default::default();
        for ((t, ms), k) in sim.waits.iter().zip(&sim.wait_kinds) {
            if *t >= from && *t < to {
                by[usize::from(*k)].push(*ms);
            }
        }
        let parts: Vec<String> = by
            .iter_mut()
            .zip(["first", "reload_5s", "reload_30s"])
            .map(|(v, kind)| {
                v.sort_by(f64::total_cmp);
                let q = |q: usize| {
                    v.get(v.len() * q / 100)
                        .map_or("-".to_owned(), |x| format!("{x:.0}"))
                };
                format!("{kind} {} (p50 {} p95 {} ms)", v.len(), q(50), q(95))
            })
            .collect();
        parts.join(", ")
    };
    eprintln!(
        "soak {minutes} min: mesh waits by eviction, first window: {}; last window: {}",
        outside(Micros(span), Micros(2 * span)),
        outside(end.since(Micros(span)), end)
    );
    // The reloads by their eviction's reason, as its decision record says.
    let reasons = |from: Micros, to: Micros| {
        let mut by: std::collections::BTreeMap<&str, u32> = Default::default();
        for ((t, _), why) in sim.waits.iter().zip(&sim.wait_reasons) {
            if *t >= from && *t < to && !why.is_empty() {
                *by.entry(*why).or_default() += 1;
            }
        }
        by
    };
    eprintln!(
        "soak {minutes} min: reloads by eviction reason, first window: {:?}; last window: {:?}",
        reasons(Micros(span), Micros(2 * span)),
        reasons(end.since(Micros(span)), end)
    );
    assert!(first.p95_ms > 0.0, "nothing loaded");
    // The last window draws at full detail: before the progressive score,
    // its flat latency came from a picture relaxed on every one of its
    // steps (scale 1.93, 406 waits against 697) — a budget held between its
    // watermarks by copies nobody drew, the stall Laurent saw in Tuile.app
    // (`docs/explorations/budget-progressive-coarse-preload.md` §3). At full
    // detail the tour asks for more (1 753 waits) and waits a little longer:
    // the bound is over the first window's (below), never a relaxed picture.
    let relaxed_last = sim
        .log
        .iter()
        .filter(|l| l.now >= end.since(Micros(span)) && l.stats.sse_scale_milli > 1_000)
        .count();
    assert_eq!(relaxed_last, 0, "the last window is relaxed: the stall");
    // A third over until the screen-space error was priced by view depth;
    // 60 % since (Laurent, 2026-11-24: the simulation's time bounds follow
    // the new config): the finer screen edges make the last window's tour
    // ask for more, measured 150 → 233 ms.
    assert!(
        last.p95_ms <= 1.6 * first.p95_ms,
        "I6: p95 {} → {} ms",
        first.p95_ms,
        last.p95_ms
    );
    assert!(
        last.tracked <= 1.2 * first.tracked + 64.0,
        "I7: tracked beyond residency {} → {}",
        first.tracked,
        last.tracked
    );
    assert!(
        last.heap <= 1.2 * first.heap + 16.0,
        "I7: heap {} → {}",
        first.heap,
        last.heap
    );
    // Wall time is noisy; only a growth of the per-step cost fails.
    assert!(
        last.decide_us <= 2.0 * first.decide_us + 50.0,
        "I6: decide {} → {} µs",
        first.decide_us,
        last.decide_us
    );
}

/// I6, I7: five simulated minutes, flat slopes (every PR).
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "release only: cargo test --release -p tuile-budget --test budget_sim"
)]
fn five_simulated_minutes_keep_flat_latency_and_bounded_state() {
    soak(5);
}

/// I6, I7: one simulated hour, flat slopes, in release. Kept out of CI's
/// release job for its time: 12.6 min on an M-series Mac (passed
/// 2026-10-21). No scheduled workflow runs it yet; it is run by hand.
#[test]
#[ignore = "13 min in release: by hand, cargo test --release -p tuile-budget --test budget_sim -- --ignored one_simulated_hour (no nightly job yet)"]
fn one_simulated_hour_keeps_flat_latency_and_bounded_state() {
    soak(60);
}
