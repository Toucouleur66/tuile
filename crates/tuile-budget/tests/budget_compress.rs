// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Display at once, compress what lasts (CB9): the budget's decision to
//! compress a resident RGBA8 drape in place, and its accounting
//! (`docs/explorations/compress-what-lasts.md`).
//!
//! Each test was seen red with the compression pass taken out of the step.

#![allow(clippy::unwrap_used, clippy::panic)]

mod budget_common;

use budget_common::*;
use tuile_budget::cost::{chain_blocks, drape_cost, Cost, TextureFormat};
use tuile_budget::{
    BudgetConfig, Bytes, Class, Decisions, Generation, Micros, NeedSet, ResourceId,
    Specification,
};

const RED: &str = "red until the compression pass decides";

/// A 256² drape the GPU may compress in place: RGBA8 until then.
fn compressible() -> Cost {
    drape_cost(256, 256, TextureFormat::Rgba8).compressible(
        drape_cost(256, 256, TextureFormat::Block16).gpu,
        chain_blocks(256, 256),
    )
}

const RGBA8: Bytes = Bytes(349_524);
const BLOCKS: Bytes = Bytes(87_408);

/// The consumer: every load lands at its cost (drapes compressible), every
/// planned upload is acknowledged at what it holds.
struct Host {
    r: Rig,
    /// What each drape's compression was asked at, not yet acknowledged.
    asked: Vec<(ResourceId, Generation)>,
}

impl Host {
    fn new(cfg: BudgetConfig) -> Self {
        Host {
            r: Rig::new(cfg),
            asked: Vec::new(),
        }
    }

    fn cost(r: ResourceId) -> Cost {
        match r {
            ResourceId::Drape { .. } => compressible(),
            _ => est(r),
        }
    }

    /// One step, everything it asks for done — but the compressions, kept
    /// in `asked` for the test to answer.
    fn step(&mut self, need: &NeedSet) -> Decisions {
        let d = self.r.step(need);
        for (r, g) in std::mem::take(&mut self.r.pending) {
            self.r.b.on_loaded(r, g, Self::cost(r));
        }
        for r in &d.upload_plan.items {
            if let Some(g) = self.r.delivered.get(r) {
                self.r.b.on_drawable(*r, *g, Self::cost(*r).gpu);
            }
        }
        self.asked.extend(d.compress.iter().copied());
        d
    }

    /// Steps on `need` for `t`, answering every compression at once with
    /// its blocks; returns the decisions.
    fn run(&mut self, need: &NeedSet, t: Micros) -> Vec<Decisions> {
        let end = self.r.now.plus(t);
        let mut out = Vec::new();
        while self.r.now < end {
            out.push(self.step(need));
            self.answer(BLOCKS);
        }
        out
    }

    fn answer(&mut self, actual: Bytes) {
        for (r, g) in std::mem::take(&mut self.asked) {
            self.r.b.on_compressed(r, g, actual);
        }
    }
}

fn level4() -> Vec<tuile_budget::TileId> {
    (0..4).map(|x| t(4, x, 0)).collect()
}

/// **Display at once:** nothing waits for an encode — a compressible drape
/// is delivered, uploaded and drawn as RGBA8, charged RGBA8; **compress
/// what lasts:** a drape drawn for the specification's time (2 s) is
/// compressed, not before, and charged its blocks once acknowledged.
#[test]
fn a_drawn_drape_is_compressed_once_it_has_lasted_and_not_before() {
    let mut h = Host::new(Rig::interactive_config());
    let need = drawn_draped(&level4(), 4);
    let early = h.run(&need, Micros::from_millis(1_500));
    let drawn = early.last().unwrap();
    assert!(
        drawn.draw.items.iter().all(|i| !i.drapes.is_empty()),
        "drawn dressed at once"
    );
    assert!(
        early.iter().all(|d| d.compress.is_empty()),
        "a hot, fresh copy stays RGBA8"
    );
    assert_eq!(h.r.b.ledger().gpu[Class::Drape], Bytes(4 * RGBA8.0));
    let late = h.run(&need, Micros::from_secs(2));
    let asked: Vec<ResourceId> = late
        .iter()
        .flat_map(|d| d.compress.iter().map(|(r, _)| *r))
        .collect();
    assert_eq!(asked.len(), 4, "every drape compressed once: {RED}");
    assert!(asked.iter().all(|r| matches!(r, ResourceId::Drape { .. })));
    assert_eq!(
        h.r.b.ledger().gpu[Class::Drape],
        Bytes(4 * BLOCKS.0),
        "charged its blocks once acknowledged"
    );
    h.r.b.audit().unwrap();
}

/// **Charged as held (law 5):** from the decision until its
/// acknowledgement, both copies; then the blocks. A copy evicted
/// meanwhile frees both; an acknowledgement that it could not be done
/// keeps it RGBA8 and it is never asked again.
#[test]
fn a_compression_in_flight_is_charged_both_copies() {
    let mut h = Host::new(Rig::interactive_config());
    let need = drawn_draped(&level4(), 4);
    h.run(&need, Micros::from_millis(1_900));
    let mut d = h.step(&need);
    while d.compress.is_empty() && h.r.now < Micros::from_secs(5) {
        d = h.step(&need);
    }
    let n = d.compress.len() as u64;
    assert!(n > 0, "{RED}");
    assert_eq!(
        h.r.b.ledger().gpu[Class::Drape],
        Bytes(4 * RGBA8.0 + n * BLOCKS.0),
        "both copies while it compresses"
    );
    assert_eq!(d.stats.compress.in_flight, n as u32);
    assert_eq!(d.stats.compress.saving[Class::Drape], Bytes(n * RGBA8.0));
    // One refused, the rest done.
    let mut asked = std::mem::take(&mut h.asked);
    let (refused, g) = asked.remove(0);
    h.r.b.on_compressed(refused, g, RGBA8);
    for (r, g) in asked {
        h.r.b.on_compressed(r, g, BLOCKS);
    }
    let d = h.step(&need);
    assert_eq!(d.stats.compress.refused, 1);
    assert_eq!(d.stats.compress.done, n as u32 - 1);
    let after = h.run(&need, Micros::from_secs(4));
    assert!(
        after
            .iter()
            .all(|d| !d.compress.iter().any(|(r, _)| *r == refused)),
        "a refused compression is not asked again"
    );
    assert_eq!(
        h.r.b.ledger().gpu[Class::Drape],
        Bytes(RGBA8.0 + 3 * BLOCKS.0)
    );
    h.r.b.audit().unwrap();
}

/// **A low score stays RGBA8:** drapes the view has left keep a 64th of
/// their probability of use — under the specification's threshold — and
/// are never compressed however long they stay, while the drapes the view
/// now draws are, once they have lasted. A threshold re-specified at run
/// time to certainty alone keeps compressing the drawn ones.
#[test]
fn a_copy_the_score_gives_up_stays_rgba8_whatever_its_age() {
    let mut h = Host::new(Rig::interactive_config());
    let here = drawn_draped(&level4(), 4);
    h.run(&here, Micros::from_millis(500));
    let there: Vec<_> = (0..4).map(|x| t(4, x, 8)).collect();
    let there_need = drawn_draped(&there, 4);
    let d = h.run(&there_need, Micros::from_secs(6));
    let asked: Vec<ResourceId> = d
        .iter()
        .flat_map(|d| d.compress.iter().map(|(r, _)| *r))
        .collect();
    let left = |r: &ResourceId| level4().contains(&r.tile());
    assert!(!asked.is_empty(), "the drawn drapes are compressed: {RED}");
    assert!(
        !asked.iter().any(left),
        "a copy the view left is not compressed: {asked:?}"
    );
    // The left copies are still resident: kept as cache, as RGBA8.
    assert!(level4().iter().all(|t| h
        .r
        .b
        .resource(dr(*t, 4))
        .is_some_and(|v| v.gpu == RGBA8)));
    let mut s = Specification::interactive();
    s.params.compress.probability_permille = 1_000;
    h.r.b.specify(s).unwrap();
    let elsewhere: Vec<_> = (0..4).map(|x| t(4, x, 4)).collect();
    let d = h.run(&drawn_draped(&elsewhere, 4), Micros::from_secs(4));
    assert!(
        d.iter()
            .flat_map(|d| d.compress.iter())
            .any(|(r, _)| elsewhere.contains(&r.tile())),
        "drawn copies are certain: still compressed at a threshold of 1"
    );
}

/// The scene of the pressure tests: rows of four tiles at level 4, each
/// drawn for `dwell`, the drapes' class targeted at `target` drapes.
fn rows(compress: bool, target: u64, dwell: Micros, rows: u64) -> (usize, Option<Micros>) {
    let mut cfg = Rig::interactive_config();
    cfg.target[Class::Drape] = Bytes(target * RGBA8.0);
    cfg.overflow[Class::Drape] = Bytes(4 * RGBA8.0);
    cfg.hard_limit[Class::Drape] = Bytes(40 * RGBA8.0);
    cfg.pool = tuile_budget::Pool::NONE;
    let mut h = Host::new(cfg);
    let mut evicted = 0;
    let mut first_compress = None;
    for row in 0..rows {
        let tiles: Vec<_> = (0..4).map(|x| t(4, x, row * 2)).collect();
        let need = drawn_draped(&tiles, 4);
        let end = h.r.now.plus(dwell);
        while h.r.now < end {
            let d = if compress {
                h.step(&need)
            } else {
                let d = h.r.step(&need);
                for (r, g) in std::mem::take(&mut h.r.pending) {
                    h.r.b.on_loaded(r, g, est(r));
                }
                h.r.upload(&d);
                d
            };
            evicted += d.evict.len();
            if !d.compress.is_empty() {
                first_compress.get_or_insert(h.r.now);
            }
            h.answer(BLOCKS);
        }
    }
    h.r.b.audit().unwrap();
    (evicted, first_compress)
}

/// **Compress what lasts, and keep it (zswap's bargain):** drapes that have
/// lasted are compressed, and the room they give back keeps the copies a
/// view left as cache where, held RGBA8, they were evicted.
#[test]
fn what_lasts_compressed_keeps_more_resident() {
    let dwell = Micros::from_millis(2_500);
    let (rgba8, none) = rows(false, 7, dwell, 4);
    let (compressed, first) = rows(true, 7, dwell, 4);
    assert_eq!(none, None);
    assert!(rgba8 > 0, "the scene evicts when held RGBA8");
    assert!(first.is_some(), "{RED}");
    assert!(
        compressed < rgba8,
        "compressed {compressed} evictions against RGBA8's {rgba8}"
    );
}

/// **Compression before eviction (law 4):** over its low watermark — where
/// evictions start — a class compresses its drawn drapes at once, the age
/// waived; under it, a fresh copy waits for its age.
#[test]
fn under_pressure_the_age_is_waived() {
    let half = Micros::from_millis(500);
    let (_, pressed) = rows(true, 4, half, 3);
    assert!(
        pressed.is_some_and(|t| t < Micros::from_secs(2)),
        "over the low watermark, compressed at once: {pressed:?} — {RED}"
    );
    let (_, roomy) = rows(true, 40, half, 3);
    assert_eq!(roomy, None, "with room, a fresh copy stays RGBA8");
}

/// **What compressions in flight give back counts as given back (law 4):**
/// while a slow GPU has not yet acknowledged them, the transient of both
/// copies does not make the sweep evict the cache the compressions will
/// make room for.
#[test]
fn a_sweep_counts_what_the_compressions_in_flight_give_back() {
    let mut cfg = Rig::interactive_config();
    cfg.target[Class::Drape] = Bytes(11 * RGBA8.0 / 2);
    cfg.overflow[Class::Drape] = Bytes(4 * RGBA8.0);
    cfg.hard_limit[Class::Drape] = Bytes(40 * RGBA8.0);
    cfg.pool = tuile_budget::Pool::NONE;
    let mut h = Host::new(cfg);
    let a: Vec<_> = (0..4).map(|x| t(4, x, 0)).collect();
    h.run(&drawn_draped(&a, 4), Micros::from_millis(2_500));
    assert_eq!(h.r.b.ledger().gpu[Class::Drape], Bytes(4 * BLOCKS.0), "{RED}");
    // The view moves; its new drapes are compressed, and the GPU is slow
    // to say it is done.
    let b: Vec<_> = (0..4).map(|x| t(4, x, 2)).collect();
    let need = drawn_draped(&b, 4);
    let mut evicted = Vec::new();
    let mut flying = 0;
    for _ in 0..60 {
        let d = h.step(&need);
        flying = flying.max(d.stats.compress.in_flight);
        evicted.extend(d.evict.iter().copied());
    }
    assert!(flying > 0, "{RED}");
    assert!(
        evicted.is_empty(),
        "the cache stays while its room is on its way: {evicted:?}"
    );
    h.answer(BLOCKS);
    h.step(&need);
    h.r.b.audit().unwrap();
}

/// **Spare time only (law 6):** the compressions in flight never write
/// more blocks than the step's encode lane, one always admitted.
#[test]
fn the_compressions_in_flight_fit_the_lane() {
    let mut cfg = Rig::interactive_config();
    cfg.encode_blocks_per_step = 2 * chain_blocks(256, 256);
    let mut h = Host::new(cfg);
    let tiles: Vec<_> = (0..8).map(|x| t(4, x, 0)).collect();
    let need = drawn_draped(&tiles, 4);
    h.run(&need, Micros::from_millis(2_000));
    // Nothing answered: the flights stay.
    let mut most = 0;
    for _ in 0..20 {
        let d = h.step(&need);
        most = most.max(d.stats.compress.in_flight);
    }
    assert_eq!(most, 2, "{RED}");
    h.answer(BLOCKS);
    let d = h.run(&need, Micros::from_millis(500));
    assert!(d.iter().any(|d| !d.compress.is_empty()), "the lane frees");
}

/// **Off:** a cost that cannot be compressed asks for nothing, ever.
#[test]
fn nothing_compressible_nothing_compressed() {
    let mut r = Rig::interactive();
    let need = drawn_draped(&level4(), 4);
    r.settle(&need, 32);
    let d = r.idle_for(&need, Micros::from_secs(5));
    assert!(d.iter().all(|d| d.compress.is_empty()));
    assert!(d
        .iter()
        .all(|d| d.stats.compress == tuile_budget::compress::CompressStats::default()));
}
