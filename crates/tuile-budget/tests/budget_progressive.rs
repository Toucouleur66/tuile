// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The progressive coarse and neighbour preload
//! (`docs/explorations/budget-progressive-coarse-preload.md`): the floor
//! through level 3, the levels above the need and the neighbours around the
//! view preloaded by one score, released by the same score.
//!
//! The need is built here as the geometry server's traversal builds it
//! (`tuile_core::neighbourhood`): on the unit-square quadtree of
//! `budget_common`, a view at a point `F` of the ground needing level `n`
//! draws the level-`n` tiles around `F`, and lists as preloads, per level
//! from the floor to `n + 1`, the `⌊π(ρ+1)²⌋` tiles nearest `F` with their
//! distance in tiles of their own level.

#![allow(clippy::unwrap_used, clippy::panic)]

mod budget_common;

use std::collections::BTreeSet;

use budget_common::*;
use proptest::prelude::*;
use tuile_budget::score::Side;
use tuile_budget::{
    BudgetConfig, Bytes, Class, Decisions, Need, NeedSet, ResourceId, TileId, CERTAIN,
};

/// The floor of these tests: levels `0..=3`, as the viewer's.
const FLOOR: u32 = 3;

/// The tiles listed per level at radius `rho`.
fn per_level(rho: u32) -> usize {
    let r = f64::from(rho) + 1.0;
    (std::f64::consts::PI * r * r).floor() as usize
}

/// The distance from `(fx, fy)` (unit square) to tile `(l, x, y)`, in
/// tiles of level `l`.
fn tiles_away(l: u32, x: u64, y: u64, fx: f64, fy: f64) -> f64 {
    let n = f64::from(1u32 << l);
    let (px, py) = (fx * n, fy * n);
    let dx = (x as f64 - px).max(px - (x as f64 + 1.0)).max(0.0);
    let dy = (y as f64 - py).max(py - (y as f64 + 1.0)).max(0.0);
    (dx * dx + dy * dy).sqrt()
}

/// The tiles of level `l` within `r` tiles of `(fx, fy)`, nearest first,
/// ties by id.
fn around(l: u32, fx: f64, fy: f64, r: f64) -> Vec<(f64, TileId)> {
    let n = 1u64 << l;
    let (cx, cy) = ((fx * n as f64) as i64, (fy * n as f64) as i64);
    let span = r.ceil() as i64 + 1;
    let mut out = Vec::new();
    for x in (cx - span).max(0)..=(cx + span).min(n as i64 - 1) {
        for y in (cy - span).max(0)..=(cy + span).min(n as i64 - 1) {
            let d = tiles_away(l, x as u64, y as u64, fx, fy);
            if d <= r {
                out.push((d, t(l, x as u64, y as u64)));
            }
        }
    }
    out.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    out
}

/// The view: at `(fx, fy)` needing level `n`, drawing the level-`n` tiles
/// within `reach` of it whose centre lies in the half plane `look` points
/// to (`None`: all around).
#[derive(Clone, Copy, Debug)]
struct View {
    fx: f64,
    fy: f64,
    n: u32,
    reach: f64,
    look: Option<(f64, f64)>,
}

impl View {
    fn at(fx: f64, fy: f64, n: u32) -> Self {
        View {
            fx,
            fy,
            n,
            reach: 1.5,
            look: None,
        }
    }

    fn drawn(&self) -> Vec<TileId> {
        around(self.n, self.fx, self.fy, self.reach)
            .into_iter()
            .map(|(_, t)| t)
            .filter(|t| {
                let Some((lx, ly)) = self.look else {
                    return true;
                };
                let (l, x, y) = t.terrain_coord();
                let n = f64::from(1u32 << l);
                let (cx, cy) = ((x as f64 + 0.5) / n - self.fx, (y as f64 + 0.5) / n - self.fy);
                cx * lx + cy * ly >= 0.0
            })
            .collect()
    }

    /// The need as the traversal lists it at radius `rho`.
    fn need(&self, rho: u32) -> NeedSet {
        let drawn = self.drawn();
        let drawn_set: BTreeSet<TileId> = drawn.iter().copied().collect();
        let mut ns = NeedSet {
            drawn: drawn.iter().map(|&t| (t, need(t))).collect(),
            ..NeedSet::default()
        };
        let mut listed: BTreeSet<TileId> = BTreeSet::new();
        // The floor.
        for l in 0..=FLOOR {
            for x in 0..(1u64 << l) {
                for y in 0..(1u64 << l) {
                    let id = t(l, x, y);
                    if !drawn_set.contains(&id) && listed.insert(id) {
                        ns.ancestors.push((id, need(id)));
                    }
                }
            }
        }
        // The chain of the drawn tiles: their fallbacks.
        let mut chain: BTreeSet<TileId> = BTreeSet::new();
        for &d in &drawn {
            let mut p = parent(d);
            while let Some(a) = p {
                if !drawn_set.contains(&a) {
                    chain.insert(a);
                }
                p = parent(a);
            }
        }
        // The neighbourhood, from the floor down to one under the need:
        // the ring, never protected.
        if rho > 0 {
            for l in FLOOR + 1..=self.n + 1 {
                for (d, id) in around(l, self.fx, self.fy, f64::from(rho))
                    .into_iter()
                    .take(per_level(rho))
                {
                    if !drawn_set.contains(&id) && !chain.contains(&id) && listed.insert(id) {
                        let n: Need = need(id).with_proximity(self.n, d as f32);
                        ns.ring.push((id, n));
                    }
                }
            }
        }
        // The chain of every drawn tile.
        for &d in &drawn {
            let mut p = parent(d);
            while let Some(a) = p {
                if !drawn_set.contains(&a) && listed.insert(a) {
                    let (l, x, y) = a.terrain_coord();
                    let away = tiles_away(l, x, y, self.fx, self.fy) as f32;
                    ns.ancestors.push((a, need(a).with_proximity(self.n, away)));
                }
                p = parent(a);
            }
        }
        ns
    }
}

fn config(total: Bytes, rho: u32) -> BudgetConfig {
    let mut c = BudgetConfig::interactive(total);
    c.pinned_level = Some(FLOOR);
    c.reach_tiles = rho;
    c
}

fn gpu(r: &Rig, id: TileId) -> bool {
    r.b.resource(ResourceId::Mesh(id))
        .is_some_and(|v| v.gpu > Bytes::ZERO)
}

/// Steps on `need`, completing every load and upload, until a step asks for
/// nothing; at most `max` steps.
fn settle(r: &mut Rig, need: &NeedSet, max: usize) -> Decisions {
    r.settle(need, max)
}

/// 1. Levels 1 to 3 stay resident under pressure: the floor is loaded
/// first, and the pressure that evicts the neighbourhood never takes it.
#[test]
fn the_floor_through_level_three_stays_under_pressure() {
    let mut r = Rig::new(config(Bytes::mib(256), 4));
    let v = View::at(0.37, 0.61, 13);
    let need = v.need(4);
    settle(&mut r, &need, 400);
    // A memory signal, then a budget a twentieth as large: the sweep runs.
    r.b.retarget(Bytes::mib(12));
    let mut evicted = 0;
    for _ in 0..30 {
        let d = r.step(&need);
        evicted += d.evict.len();
        for e in &d.evict {
            if let ResourceId::Mesh(id) = e {
                assert!(id.terrain_coord().0 > FLOOR, "the floor's {id:?} evicted");
            }
        }
        r.complete(&d);
    }
    assert!(evicted > 0, "the pressure evicted nothing: the test proves nothing");
    for l in 0..=FLOOR {
        for x in 0..(1u64 << l) {
            for y in 0..(1u64 << l) {
                assert!(gpu(&r, t(l, x, y)), "floor tile {l}/{x}/{y} not resident");
            }
        }
    }
}

/// 2. An aggressive zoom-out from level 13, one level every two steps with
/// no load completing on the way: every step's view is drawn exactly, with
/// its own tiles — the neighbourhood held them already — and levels 11 and
/// 12 were resident before the zoom-out began.
#[test]
fn an_aggressive_zoom_out_from_level_13_finds_its_ground_resident() {
    let mut r = Rig::new(config(Bytes::mib(512), 4));
    let (fx, fy) = (0.4137, 0.5871);
    let start = View::at(fx, fy, 13);
    settle(&mut r, &start.need(4), 600);
    for l in [11, 12] {
        for (_, id) in around(l, fx, fy, 2.0) {
            assert!(gpu(&r, id), "level {l}: {id:?} not resident before the zoom-out");
        }
    }
    for n in (5..13).rev() {
        let v = View::at(fx, fy, n);
        let need = v.need(4);
        for _ in 0..2 {
            let d = r.step(&need);
            assert_eq!(d.stats.lost, 0, "level {n}: a hole");
            assert_eq!(
                d.stats.fallback, 0,
                "level {n}: {} tiles drawn by a coarser ancestor",
                d.stats.fallback
            );
            // Nothing completes: the network stalled for the whole zoom-out.
        }
    }
}

/// 3. A rotation near the ground: the view turns from east to west; the
/// tiles the turn brings in were neighbours, resident, and draw at once.
#[test]
fn a_rotation_near_the_ground_finds_its_neighbours_resident() {
    let mut r = Rig::new(config(Bytes::mib(512), 4));
    let (fx, fy) = (0.6213, 0.3149);
    let east = View {
        reach: 3.0,
        look: Some((1.0, 0.0)),
        ..View::at(fx, fy, 13)
    };
    settle(&mut r, &east.need(4), 600);
    let west = View {
        look: Some((-1.0, 0.0)),
        ..east
    };
    let d = r.step(&west.need(4));
    assert!(west.drawn().iter().any(|t| !east.drawn().contains(t)));
    assert_eq!(d.stats.lost, 0);
    assert_eq!(d.stats.fallback, 0, "the turn found coarser ground only");
}

/// 4. The release order follows the score: under a shrinking budget, every
/// preload evicted scores no more than any preload still held (the
/// present need and the floor apart).
#[test]
fn the_release_order_follows_the_score() {
    let mut runner = proptest::test_runner::TestRunner::new(ProptestConfig {
        cases: 12,
        ..ProptestConfig::default()
    });
    let strategy = (0.05f64..0.95, 0.05f64..0.95, 9u32..14, 2u32..5, 1_800u64..2_800);
    runner
        .run(&strategy, |(fx, fy, n, rho, permille)| {
            let mut r = Rig::new(config(Bytes::mib(512), rho));
            let v = View::at(fx, fy, n);
            let need = v.need(rho);
            let held = settle(&mut r, &need, 600).stats.gpu[Class::TerrainMesh];
            // A device total under which terrain's share (about a third)
            // holds a third to four fifths of what is held now.
            r.b.retarget(Bytes(held.0 * permille / 1_000));
            let d = r.step(&need);
            let evicted: Vec<u64> = d
                .scores
                .iter()
                .filter(|e| e.side == Side::Evict && e.class == Class::TerrainMesh)
                .map(|e| e.terms.score)
                .collect();
            let held = r.b.held_report();
            let kept_least = held
                .lowest
                .iter()
                .filter(|h| h.state == tuile_budget::score::HeldState::Wanted)
                .filter(|h| h.class == Class::TerrainMesh)
                .map(|h| h.terms.score)
                .min();
            // Not vacuous: the pressure evicted preloads and kept some.
            prop_assert!(!evicted.is_empty(), "nothing evicted");
            let (Some(max_evicted), Some(least)) = (evicted.iter().max(), kept_least) else {
                return Err(TestCaseError::fail("no preload kept"));
            };
            prop_assert!(
                *max_evicted <= least,
                "evicted a score of {max_evicted} while keeping {least}"
            );
            Ok(())
        })
        .unwrap();
}

/// 5. The bound: what the need lists as preloads is at most `⌊π(ρ+1)²⌋` a
/// level, and so at most the floor plus that many per level; the
/// probabilities the budget gives them are those of the formula.
#[test]
fn the_preloads_are_bounded_per_profile() {
    for (rho, k) in [(2u32, 28usize), (4, 78)] {
        assert_eq!(per_level(rho), k);
        for n in 5..=16u32 {
            let v = View::at(0.3711, 0.6283, n);
            let need = v.need(rho);
            let floor = (0..=FLOOR).map(|l| 1usize << (2 * l)).sum::<usize>();
            let placed: Vec<_> = need
                .ancestors
                .iter()
                .chain(need.ring.iter())
            .filter(|(_, n)| n.proximity.is_some())
                .collect();
            for l in FLOOR + 1..=n + 1 {
                let at = placed.iter().filter(|(t, _)| t.terrain_coord().0 == l).count();
                // The chain of the drawn tiles adds at most the drawn ones'.
                assert!(at <= k + v.drawn().len(), "ρ {rho}, level {l}: {at}");
            }
            let bound = floor + (n + 1 - FLOOR) as usize * k + v.drawn().len() * n as usize;
            assert!(need.ancestors.len() + need.ring.len() <= bound, "ρ {rho}, n {n}");
        }
    }
    // A(ℓ, n) and D(d) as the document writes them.
    let p = tuile_budget::spec::PROGRESSIVE;
    let (a, d) = tuile_budget::progressive_terms(&p, 12, 13, 0, 4);
    assert_eq!((a, d), (CERTAIN * 8 / 10, CERTAIN));
    let (a, _) = tuile_budget::progressive_terms(&p, 11, 13, 0, 4);
    assert_eq!(a, CERTAIN * 4 / 10);
    let (_, d) = tuile_budget::progressive_terms(&p, 13, 13, 64, 4);
    assert_eq!(d, CERTAIN / 4, "¼ at d = ρ");
    let (a, _) = tuile_budget::progressive_terms(&p, 14, 13, 0, 4);
    assert_eq!(a, CERTAIN * 2 / 10);
}

/// 6. Preloads go on in the background while the view is still, and never
/// delay the present need: a hole that opens while preloads hold the slots
/// starts on the step it opens.
#[test]
fn preloads_continue_when_idle_and_never_delay_the_present_need() {
    let mut r = Rig::new(config(Bytes::mib(512), 4));
    let v = View::at(0.5531, 0.4412, 13);
    let need = v.need(4);
    // Loads take three steps; the view does not move.
    let mut steps_starting_preloads = 0;
    for i in 0..120 {
        let d = r.step(&need);
        if d.scores.iter().any(|e| e.side == Side::Preload) {
            steps_starting_preloads += 1;
        }
        if i % 3 == 2 {
            r.load_where(|_| true);
        }
        r.upload(&d);
    }
    assert!(
        steps_starting_preloads > 20,
        "the network idled: preloads started on {steps_starting_preloads} steps"
    );
    // The neighbourhood came in: the level under the need, around the view.
    for (_, id) in around(12, v.fx, v.fy, 2.0) {
        assert!(gpu(&r, id), "{id:?} not preloaded");
    }
    // Preloads in flight now, and a hole opens beside the view.
    let d = r.step(&need);
    r.upload(&d);
    // A hole opens beside the view: it starts at once.
    let (l, x, y) = v.drawn()[0].terrain_coord();
    let hole = t(l, x + 9, y);
    let mut urgent = need.clone();
    urgent.drawn.push((hole, budget_common::need(hole)));
    let d = r.step(&urgent);
    assert!(
        d.start.iter().any(|s| s.resource == ResourceId::Mesh(hole)),
        "the hole waited for the preloads"
    );
}

/// The margin's swap: room full of preloads around a level-9 view, the
/// view zooms in to level 12; the levels just above the new need score more
/// than the coarse ones held by more than the margin, and take their place
/// — evicted as swaps, lowest score first — while nothing the present need
/// wants is displaced.
#[test]
fn a_preload_worth_more_by_the_margin_takes_a_weaker_ones_place() {
    let (fx, fy) = (0.3141, 0.2718);
    let mut r = Rig::new(config(Bytes::mib(64), 4));
    let far = View::at(fx, fy, 9);
    settle(&mut r, &far.need(4), 600);
    let near = View::at(fx, fy, 12);
    let need = near.need(4);
    let mut swaps = 0;
    for _ in 0..400 {
        let d = r.step(&need);
        swaps += d
            .scores
            .iter()
            .filter(|e| e.reason == tuile_budget::score::Reason::Swap)
            .count();
        assert_eq!(d.stats.lost, 0);
        r.complete(&d);
    }
    assert!(swaps > 0, "no preload took a weaker one's place");
    for (_, id) in around(11, fx, fy, 1.0) {
        assert!(gpu(&r, id), "level 11: {id:?} not resident after the zoom-in");
    }
}

/// A grazing view: tiles near the eye at level 13, the far ground towards
/// the horizon drawn at level 7. The traversal places each ancestor against
/// the need *at its place* — the finest level drawn under it — so the far
/// ground's coarse ancestors (level 6, `A` = a0) load before a refinement
/// under the eye (level 14, `A` = a↓).
#[test]
fn a_grazing_view_loads_the_far_coarse_ground_before_a_refinement_under_the_eye() {
    let mut c = config(Bytes::mib(512), 4);
    c.max_in_flight = tuile_budget::PerClass::splat(2);
    let mut r = Rig::new(c);
    let near: Vec<TileId> = (0..3).map(|x| t(13, 4000 + x, 3000)).collect();
    let far: Vec<TileId> = (0..3).map(|x| t(7, 70 + x, 40)).collect();
    let mut ns = NeedSet {
        drawn: near.iter().chain(&far).map(|&t| (t, need(t))).collect(),
        ..NeedSet::default()
    };
    // The far ground's parents, on the footprint, needing level 7 there.
    let far_parents: BTreeSet<TileId> = far.iter().filter_map(|t| parent(*t)).collect();
    for &p in &far_parents {
        ns.ancestors.push((p, need(p).with_proximity(7, 0.0)));
    }
    // A refinement under the eye: level 14, on the footprint, needing 13.
    let fine: Vec<TileId> = near.iter().flat_map(|t| children(*t)).take(4).collect();
    for &f in &fine {
        ns.ring.push((f, need(f).with_proximity(13, 0.0)));
    }
    // The drawn tiles in first.
    let drawn: BTreeSet<TileId> = ns.drawn.iter().map(|(t, _)| *t).collect();
    let mut order: Vec<TileId> = Vec::new();
    for _ in 0..40 {
        let d = r.step(&ns);
        for s in &d.start {
            if let ResourceId::Mesh(t) = s.resource {
                if !drawn.contains(&t) {
                    order.push(t);
                }
            }
        }
        r.load_where(|_| true);
        r.upload(&d);
    }
    let first_fine = order.iter().position(|t| fine.contains(t)).unwrap();
    let last_far = order.iter().rposition(|t| far_parents.contains(t)).unwrap();
    assert!(last_far < first_fine, "the refinement came first: {order:?}");
}

/// `R` is measured: a slower network, measured, raises the restore cost of
/// what comes back from it — its copies held score higher (kept longer) and
/// its preloads gain more lead (started sooner) — and the specification's
/// constant stands until a tier has samples.
#[test]
fn a_slower_measured_network_raises_its_restore_cost() {
    use tuile_budget::{Measured, TierSample};
    let run = |latency_us: Option<u32>| {
        // A small decoded tier: the copies held lose their decoded copy and
        // would come back from the network.
        let mut c = config(Bytes::mib(512), 4);
        c.decoded_target = Bytes::mib(2);
        let mut r = Rig::new(c);
        if let Some(l) = latency_us {
            r.b.measure(Measured {
                interval: 1,
                network: TierSample {
                    latency_us: l,
                    bytes_per_ms: 0,
                    failures_permille: 0,
                    samples: 20,
                },
                ..Measured::default()
            });
        }
        let v = View::at(0.4, 0.6, 11);
        let need = v.need(4);
        let mut preload = None;
        for _ in 0..400 {
            let d = r.step(&need);
            let lead = d
                .scores
                .iter()
                .filter(|e| e.side == Side::Preload)
                .map(|e| e.terms.r_us)
                .max();
            preload = preload.max(lead);
            let idle = d.start.is_empty() && d.deliver.is_empty() && d.upload_plan.items.is_empty();
            r.complete(&d);
            if idle {
                break;
            }
        }
        let held = r.b.held_report();
        let restore = r.b.restore_costs();
        // The copies held without their decoded copy come back from the
        // network: the largest R held, and the largest score.
        let score = held
            .board
            .cells()
            .fold(None, |acc: Option<(u64, u64)>, (.., c)| {
                let (r, sc) = acc.unwrap_or((0, 0));
                Some((r.max(c.r_us.max), sc.max(c.score.max)))
            });
        (restore, preload, score)
    };
    let (default, ..) = run(None);
    assert_eq!(default.remote, tuile_budget::Micros::from_millis(150), "the constant until samples");
    let (fast, fast_preload, fast_held) = run(Some(80_000));
    let (slow, slow_preload, slow_held) = run(Some(1_200_000));
    assert!(slow.remote > fast.remote, "{slow:?} {fast:?}");
    assert!(slow_preload > fast_preload, "{slow_preload:?} {fast_preload:?}");
    let (fast_r, fast_s) = fast_held.unwrap();
    let (slow_r, slow_s) = slow_held.unwrap();
    assert!(slow_r > fast_r && slow_s > fast_s, "{slow_held:?} {fast_held:?}");
}

/// The release order follows the camera without re-keying what is held:
/// a turn re-places only the wants the need named — the neighbourhood —
/// and re-keys the persistent index only for those filed whose probability
/// moved, as many with 4 000 as with 40 000 copies held.
#[test]
fn a_turn_re_keys_what_it_names_not_what_is_held() {
    let rekeys_with = |held: u64| -> (u32, u32) {
        let mut c = config(Bytes::gib(64), 4);
        c.max_in_flight = tuile_budget::PerClass::splat(8_192);
        c.request_heap = 65_536;
        c.upload_bytes_per_frame = Bytes::gib(64);
        c.delivery_credit = Bytes::gib(64);
        let mut r = Rig::new(c);
        // `held` copies at level 9 far from the view, then left.
        let side = (held as f64).sqrt().ceil() as u64;
        let bulk: Vec<TileId> = (0..held).map(|i| t(9, i % side, 300 + i / side)).collect();
        let mut first = NeedSet::default();
        first.drawn = bulk.iter().map(|&t| (t, need(t))).collect();
        settle(&mut r, &first, 50);
        let east = View {
            reach: 3.0,
            look: Some((1.0, 0.0)),
            ..View::at(0.21, 0.13, 13)
        };
        settle(&mut r, &east.need(4), 200);
        // The turn: the neighbourhood's distances move (the motion's side
        // nearer), every tile of it named.
        let mut turned = east.need(4);
        for (_, n) in turned.ring.iter_mut() {
            if let Some(p) = n.proximity.as_mut() {
                p.tiles_away = (p.tiles_away + 0.75).min(4.0);
            }
        }
        let d = r.step(&turned);
        (d.stats.score.places, d.stats.score.rekeys)
    };
    let (places_small, rekeys_small) = rekeys_with(4_000);
    let (places_large, rekeys_large) = rekeys_with(40_000);
    assert!(places_small > 0, "the turn placed nothing");
    assert_eq!(places_small, places_large, "places depend on what is held");
    assert_eq!(rekeys_small, rekeys_large, "re-keys depend on what is held");
    // Bounded by the neighbourhood: K per level over the levels listed.
    assert!(places_large as usize <= per_level(4) * 14 * 2, "{places_large}");
}

/// Laurent's Tuile.app, 4.3 km over a coast (main 9a5ee85e): the drapes'
/// class at its target, nearly all of it undrawn fine drapes left from a
/// zoom-in, 220 tiles under their imagery need, and nothing loading. The
/// pool full of what nobody draws must give way to the need: still, the
/// view's own drapes load and every tile wears its own level.
#[test]
fn a_pool_full_of_undrawn_fine_drapes_gives_way_to_the_need() {
    let mut r = Rig::new(config(Bytes::mib(96), 4));
    // Zoomed in: tiles of level 16 wearing level-18 imagery, as many as
    // the drapes' class holds.
    let fine: Vec<TileId> = (0..12u64).flat_map(|x| (0..12u64).map(move |y| t(16, 21_000 + x, 14_000 + y))).collect();
    let zoomed = NeedSet {
        drawn: fine.iter().map(|&id| (id, draped(id, 18))).collect(),
        ..NeedSet::default()
    };
    settle(&mut r, &zoomed, 600);
    // Out: other tiles, level 11, wanting level-13 imagery; the fine drapes
    // are drawn by nobody now.
    let wide: Vec<TileId> = (0..10u64).flat_map(|x| (0..10u64).map(move |y| t(11, 600 + x, 400 + y))).collect();
    let out = NeedSet {
        drawn: wide.iter().map(|&id| (id, draped(id, 13))).collect(),
        ..NeedSet::default()
    };
    let mut started = 0;
    let mut last = None;
    for _ in 0..600 {
        let d = r.step(&out);
        started += d.start.len();
        r.complete(&d);
        last = Some(d);
    }
    let d = last.unwrap();
    assert!(started > 0, "nothing loaded");
    assert_eq!(d.stats.lost + d.stats.bare, 0);
    assert_eq!(
        d.stats.deficit.imagery_under(),
        0,
        "tiles under their imagery need while undrawn drapes hold the pool"
    );
    for &id in &wide {
        assert!(
            r.b.resource(dr(id, 13)).is_some_and(|v| v.gpu > Bytes::ZERO),
            "{id:?}'s own drape not resident"
        );
    }
}

/// The headroom: a still view whose neighbourhood outgrows the budget
/// settles with the unprotected copies under `target − headroom` in every
/// class, and holds no copy left behind under the floor value.
#[test]
fn a_still_view_keeps_its_pools_under_the_headroom() {
    let mut r = Rig::new(config(Bytes::mib(48), 4));
    let v = View::at(0.3333, 0.6666, 14);
    let need = v.need(4);
    let mut d = settle(&mut r, &need, 800);
    for _ in 0..30 {
        d = r.step(&need);
        r.complete(&d);
    }
    let headroom = tuile_budget::spec::HEADROOM;
    for c in [Class::TerrainMesh, Class::Drape] {
        let free = d.stats.gpu[c].minus(d.stats.protected[c]);
        let under = d.stats.target[c].scaled(u64::from(1_000 - headroom), 1_000);
        assert!(free <= under, "{c:?}: {free:?} unprotected over {under:?}");
    }
    let floor = CERTAIN / 1_000 * u32::from(tuile_budget::spec::FLOOR_VALUE);
    let held = r.b.held_report();
    for h in held.lowest.iter().filter(|h| h.state == tuile_budget::score::HeldState::Left) {
        assert!(h.terms.p >= floor, "{:?} held under the floor value", h.resource);
    }
}

/// After a zoom-out, the preloads left behind — the finer level the old
/// view anticipated — are released within a few steps, room or not.
#[test]
fn the_fine_preloads_left_behind_by_a_zoom_out_are_released() {
    let mut r = Rig::new(config(Bytes::mib(512), 4));
    let (fx, fy) = (0.7071, 0.2929);
    let low = View::at(fx, fy, 13);
    settle(&mut r, &low.need(4), 600);
    let finer: Vec<TileId> = around(14, fx, fy, 4.0).into_iter().map(|(_, t)| t).collect();
    assert!(finer.iter().any(|t| gpu(&r, *t)), "level 14 never preloaded");
    let high = View::at(fx, fy, 9);
    let need = high.need(4);
    for _ in 0..20 {
        let d = r.step(&need);
        r.complete(&d);
    }
    let left = finer.iter().filter(|t| gpu(&r, **t)).count();
    assert_eq!(left, 0, "{left} level-14 preloads still held 20 steps after the zoom-out");
}
