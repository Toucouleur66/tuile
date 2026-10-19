// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! One decision step, in order:
//!
//! 1. apply the recorded arrivals, in a canonical order;
//! 2. read the need: quantise, merge duplicates, choose each tile's imagery
//!    level (with hysteresis, Interactive only);
//! 3. build the draw set from acknowledged resources only: exact, held finer
//!    descendants on a zoom-out, the nearest drawable ancestor otherwise, and
//!    each mesh's best drawable drape;
//! 4. the protected set: drawn, in transit, the floor;
//! 5. cancel what left the need past its grace, and loads past their timeout;
//! 6. rank the work by an integer key; start loads within per-class quotas,
//!    local reads under their own;
//! 7. deliver decoded copies within the credit, the target, the hard limit
//!    and its reserve;
//! 8. plan the consumer's uploads, byte-capped, swap groups together;
//! 9. evict outside the protected set down to the low watermark; release
//!    decoded copies over their budget;
//! 10. relax the LOD with time, never with frames.
//!
//! Every container is ordered (`BTreeMap`, `BTreeSet`, sorted `Vec`): no
//! decision depends on a hash order.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};

use super::config::FailurePolicy;
use super::decisions::{
    Decisions, DrapeChoice, DrawItem, DrawSet, Role, Start, SwapGroup, UploadPlan,
};
use super::error::{AuditError, SessionError};
use super::imagery::{best_drape, needed_level, DrapeCandidate, LevelClamp, UvTransform};
use super::need::{ImageryNeed, LayerReach, Membership, Need};
use super::priority::{score, PriorityKey, Quantised, Urgency};
use super::state::{CpuCopy, Entry, Gpu, Load, Transit};
use super::stats::{BudgetStats, GAP_BUCKETS, LATENCY_BUCKETS};
use super::tier::{Presence, PresenceOracle, Source, Tier};
use super::types::{
    Bytes, Class, Failure, Generation, LayerId, Micros, PerClass, Policy, Pressure, ResourceId,
    Thermal, TileId,
};
use super::view::{Protection, ResourceState, ResourceView};
use super::{Budget, Event, FrameInput, Ledger, TileInfo, RECENT_CAP, THRASH_WINDOW};

/// The step's effective numbers, after pressure, thermal and retarget.
struct Eff {
    target: PerClass<Bytes>,
    overflow: PerClass<Bytes>,
    hard: PerClass<Bytes>,
    reserve: PerClass<Bytes>,
    decoded: Bytes,
    quota: PerClass<u32>,
    local: u32,
    upload: Bytes,
    batch: bool,
}

/// One wanted resource this step.
#[derive(Clone, Copy, Debug)]
struct Want {
    urgency: Urgency,
    q: Quantised,
    class: Class,
    level: u32,
}

/// One wanted tile's need, merged.
#[derive(Clone, Debug)]
struct TileNeed {
    membership: Membership,
    need: Need,
    q: Quantised,
}

/// Work on a wanted resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Job {
    Load,
    Deliver,
}

fn sort_key(n: &TileNeed) -> impl Ord + '_ {
    let img = n
        .need
        .imagery
        .as_ref()
        .map(|i| (i.footprint.ratio().to_bits(), &i.layers));
    (
        Reverse(n.membership),
        n.q,
        img,
        n.need.level,
        n.need.parent,
        n.need.class,
    )
}

/// The layers a need asks for, in stacking order.
fn layers(img: &ImageryNeed) -> &[LayerReach] {
    &img.layers
}

/// Every layer of `tile`, as a range of a map keyed by `(tile, layer)`.
fn tile_layers(tile: TileId) -> std::ops::RangeInclusive<(TileId, LayerId)> {
    (tile, LayerId(0))..=(tile, LayerId(u16::MAX))
}

fn backoff(policy: FailurePolicy, failures: u32, why: Failure) -> Micros {
    match policy {
        FailurePolicy::RetryBackoff { first, max } => match why {
            Failure::Permanent => max,
            Failure::Transient => {
                let shift = failures.saturating_sub(1).min(30);
                Micros(first.0.saturating_mul(1 << shift).min(max.0))
            }
        },
        _ => Micros::ZERO,
    }
}

fn latency_bucket(d: Micros) -> usize {
    let ms = d.0 / 1_000;
    let i = (64 - ms.leading_zeros()) as usize;
    i.min(LATENCY_BUCKETS - 1)
}

impl Budget {
    fn class_of(&self, r: ResourceId, mesh_class: Class) -> Class {
        match r {
            ResourceId::Mesh(_) => mesh_class,
            ResourceId::Drape { .. } => Class::Drape,
        }
    }

    /// The content epoch `r` must be of: its class's, plus its layer's for
    /// a drape (D-A4: a layer refreshes on its own).
    fn epoch(&self, r: ResourceId, class: Class) -> u64 {
        self.epochs[class]
            + r.layer()
                .map_or(0, |l| self.layer_epochs.get(&l).copied().unwrap_or(0))
    }

    fn is_floor(&self, level: u32) -> bool {
        self.cfg.pinned_level.is_some_and(|p| level <= p)
    }

    fn eff(&self) -> Eff {
        let batch = self.cfg.policy == Policy::Batch;
        let (num, den) = self.factor;
        let pct = if batch {
            100
        } else {
            match self.pressure {
                Pressure::Normal => 100,
                Pressure::Warning => u64::from(self.cfg.pressure.warning_percent),
                Pressure::Critical => u64::from(self.cfg.pressure.critical_percent),
                Pressure::Background => u64::from(self.cfg.pressure.background_percent),
            }
        };
        let scale = |b: Bytes, p: u64| {
            if b == Bytes::MAX {
                b
            } else {
                b.scaled(num, den).scaled(p, 100)
            }
        };
        let thermal_div = if batch {
            1
        } else {
            match self.thermal {
                Thermal::Nominal | Thermal::Fair => 1,
                Thermal::Serious => 2,
                Thermal::Critical => 4,
            }
        };
        let hard = PerClass::from_fn(|c| scale(self.cfg.hard_limit[c], 100));
        Eff {
            target: PerClass::from_fn(|c| scale(self.cfg.target[c], pct)),
            overflow: PerClass::from_fn(|c| scale(self.cfg.overflow[c], pct)),
            reserve: PerClass::from_fn(|c| {
                if hard[c] == Bytes::MAX {
                    Bytes::ZERO
                } else {
                    hard[c].scaled(u64::from(self.cfg.hard_reserve_percent), 100)
                }
            }),
            hard,
            decoded: if self.cfg.decoded_target == Bytes::MAX {
                Bytes::MAX
            } else {
                self.cfg.decoded_target.scaled(pct, 100)
            },
            quota: PerClass::from_fn(|c| (self.cfg.max_in_flight[c] / thermal_div).max(1)),
            local: self.cfg.max_local_reads,
            upload: if self.cfg.upload_bytes_per_frame == Bytes::MAX {
                Bytes::MAX
            } else {
                Bytes(self.cfg.upload_bytes_per_frame.0 / u64::from(thermal_div))
            },
            batch,
        }
    }

    // 1. Arrivals.
    fn apply_events(&mut self, now: Micros, st: &mut BudgetStats) {
        let mut events = std::mem::take(&mut self.events);
        events.sort();
        for ev in events {
            match ev {
                Event::Loaded(r, g, cost) => {
                    let Some(e) = self.entries.get_mut(&r) else {
                        st.stale += 1;
                        continue;
                    };
                    match e.load {
                        Load::Loading {
                            gen,
                            source,
                            started,
                            epoch,
                        } if gen == g => {
                            e.load = Load::Idle;
                            match source {
                                Source::Network => self.in_flight[e.class] -= 1,
                                Source::Local => {
                                    self.local_reads -= 1;
                                    st.network_bytes_avoided =
                                        st.network_bytes_avoided.plus(cost.gpu);
                                }
                            }
                            e.put_cpu(
                                &mut self.ledger,
                                CpuCopy {
                                    gen,
                                    cost,
                                    epoch,
                                    delivered_before: false,
                                },
                            );
                            e.failures = 0;
                            e.store_missed = false;
                            st.completed += 1;
                            st.latency_ms[latency_bucket(now.since(started))] += 1;
                        }
                        _ => st.stale += 1,
                    }
                }
                Event::Failed(r, g, why) => {
                    let Some(e) = self.entries.get_mut(&r) else {
                        continue;
                    };
                    if let Load::Loading { gen, source, .. } = e.load {
                        if gen == g {
                            e.load = Load::Idle;
                            match source {
                                Source::Network => self.in_flight[e.class] -= 1,
                                Source::Local => self.local_reads -= 1,
                            }
                            st.failed += 1;
                            if self.cfg.policy == Policy::Batch {
                                self.fatal.get_or_insert(SessionError::LoadFailed(r));
                            } else {
                                e.failures += 1;
                                e.retry_at = now.plus(backoff(self.cfg.failure, e.failures, why));
                            }
                        }
                    }
                }
                Event::StoreMiss(r, g) => {
                    let Some(e) = self.entries.get_mut(&r) else {
                        continue;
                    };
                    if let Load::Loading {
                        gen,
                        source: Source::Local,
                        ..
                    } = e.load
                    {
                        if gen == g {
                            e.load = Load::Idle;
                            self.local_reads -= 1;
                            e.store_missed = true;
                            e.presence = Some(Presence::Absent);
                            st.store_misses += 1;
                        }
                    }
                }
                Event::Drawable(r, g, actual) => {
                    let Some(e) = self.entries.get_mut(&r) else {
                        st.stale += 1;
                        continue;
                    };
                    if e.transit.is_some_and(|t| t.gen == g) {
                        if let Some(t) = e.take_transit(&mut self.ledger) {
                            e.put_gpu(
                                &mut self.ledger,
                                Gpu {
                                    gen: g,
                                    bytes: actual,
                                    epoch: t.epoch,
                                },
                            );
                        }
                        if let Some(c) = e.cpu.as_mut() {
                            if c.gen == g {
                                c.delivered_before = true;
                            }
                        }
                    } else if e.transit.is_none()
                        && e.gpu.is_none()
                        && e.cpu.is_some_and(|c| c.gen == g)
                    {
                        // Uploaded although its delivery was withdrawn: the
                        // GPU holds it, so the ledger does.
                        let epoch = e.cpu.map(|c| c.epoch).unwrap_or(0);
                        e.put_gpu(
                            &mut self.ledger,
                            Gpu {
                                gen: g,
                                bytes: actual,
                                epoch,
                            },
                        );
                    } else {
                        st.stale += 1;
                    }
                }
                Event::Discarded(r, g) => {
                    let Some(e) = self.entries.get_mut(&r) else {
                        continue;
                    };
                    if e.transit.is_some_and(|t| t.gen == g) {
                        e.take_transit(&mut self.ledger);
                        e.discards += 1;
                        e.deliver_after =
                            now.plus(backoff(self.cfg.failure, e.discards, Failure::Transient));
                    }
                }
            }
        }
    }

    // 2. The need.
    fn read_need(
        &mut self,
        input: &FrameInput<'_>,
        st: &mut BudgetStats,
    ) -> BTreeMap<TileId, TileNeed> {
        let mut tiles: BTreeMap<TileId, TileNeed> = BTreeMap::new();
        for (m, t, n) in input.need.entries() {
            let q = Quantised::of(n);
            if q.rejected {
                st.rejected_inputs += 1;
            }
            let cand = TileNeed {
                membership: m,
                need: n.clone(),
                q,
            };
            match tiles.get(&t) {
                Some(cur) if sort_key(cur) >= sort_key(&cand) => {}
                _ => {
                    tiles.insert(t, cand);
                }
            }
            self.tiles.insert(
                t,
                TileInfo {
                    parent: n.parent,
                    level: n.level,
                    rect: n.rect,
                },
            );
        }
        tiles
    }

    /// How cheaply a drape could be drawn: on the GPU, decoded, in a local
    /// store, or only remotely.
    fn drape_tier(&self, r: ResourceId, presence: &dyn PresenceOracle) -> Tier {
        let e = self.entries.get(&r);
        let epoch = self.epoch(r, Class::Drape);
        match e {
            Some(e) if e.gpu.is_some_and(|g| g.epoch == epoch) => Tier::Gpu,
            Some(e) if e.fresh_cpu(epoch).is_some() => Tier::Decoded,
            Some(e) if e.store_missed => Tier::Remote,
            _ => match presence.holds(r) {
                Presence::Stored => Tier::Stored,
                Presence::Absent => Tier::Remote,
            },
        }
    }

    /// Each wanted tile's imagery level this step.
    ///
    /// The level follows the texel footprint (relaxed with the LOD, with
    /// hysteresis). When that level is held nowhere locally, a level already
    /// held (GPU, decoded, store) is taken instead if it lies within one
    /// level of the window between the relaxed and the unrelaxed need: a
    /// relaxation never sends to the network what a local tier can show
    /// (I14, I15). Batch takes the exact level, a function of the frame.
    fn drape_needs(
        &mut self,
        tiles: &BTreeMap<TileId, TileNeed>,
        presence: &dyn PresenceOracle,
    ) -> BTreeMap<(TileId, LayerId), u32> {
        let mut out = BTreeMap::new();
        let floor = self.cfg.pinned_level.unwrap_or(0);
        let batch = self.cfg.policy == Policy::Batch;
        let cap = self.cfg.imagery_cap();
        for (t, tn) in tiles {
            let draped = matches!(tn.membership, Membership::Drawn | Membership::Blocker);
            let Some(img) = tn.need.imagery.as_ref().filter(|_| draped) else {
                continue;
            };
            for reach in layers(img) {
                let key = (*t, reach.layer);
                let drape = |l| ResourceId::drape(*t, reach.layer, l);
                let clamp = LevelClamp {
                    floor,
                    terrain_level: tn.need.level,
                    over_terrain_cap: cap,
                    source_max: reach.max_level,
                };
                if batch {
                    out.insert(key, needed_level(None, img.footprint, clamp));
                    continue;
                }
                let prev = self.imagery_prev.get(&key).copied();
                let relaxed = needed_level(prev, img.footprint.relaxed(self.scale), clamp);
                let mut level = relaxed;
                if self.drape_tier(drape(relaxed), presence) == Tier::Remote {
                    let full = needed_level(prev, img.footprint, clamp);
                    let ceiling = tn.need.level.saturating_add(cap).min(reach.max_level);
                    let lo = relaxed.min(full).saturating_sub(1).max(floor);
                    let hi = relaxed.max(full).saturating_add(1).min(ceiling);
                    let best = (lo..=hi)
                        .map(|l| {
                            let tier = self.drape_tier(drape(l), presence);
                            ((tier, l.abs_diff(relaxed), Reverse(l)), l)
                        })
                        .min();
                    if let Some(((tier, ..), l)) = best {
                        if tier != Tier::Remote {
                            level = l;
                        }
                    }
                }
                out.insert(key, level);
            }
        }
        for (k, l) in &out {
            self.imagery_prev.insert(*k, *l);
        }
        out
    }

    fn drawable(&self, r: ResourceId) -> bool {
        self.entries.get(&r).is_some_and(|e| e.gpu.is_some())
    }

    fn is_descendant(&self, mut t: TileId, of: TileId) -> bool {
        let target = self.tiles.get(&of).map(|i| i.level);
        for _ in 0..64 {
            let Some(info) = self.tiles.get(&t) else {
                return false;
            };
            let Some(p) = info.parent else {
                return false;
            };
            if p == of {
                return true;
            }
            if target.is_some_and(|l| info.level <= l) {
                return false;
            }
            t = p;
        }
        false
    }

    /// The tile's ancestors, nearest first, as far as the budget knows them.
    fn ancestors(&self, t: TileId) -> Vec<TileId> {
        let mut out = Vec::new();
        let mut cur = t;
        while let Some(p) = self.tiles.get(&cur).and_then(|i| i.parent) {
            if out.len() >= 64 {
                break;
            }
            out.push(p);
            cur = p;
        }
        out
    }

    fn best_drape_for(&self, tile: TileId, layer: LayerId, need: u32) -> Option<DrapeChoice> {
        // The tile's own drape at exactly its need is the best there is:
        // the coarsest at or above the need, and the closest owner.
        let own = ResourceId::drape(tile, layer, need);
        if self.drawable(own) {
            return Some(DrapeChoice {
                resource: own,
                layer,
                level: need,
                uv: UvTransform::IDENTITY,
            });
        }
        let rect = self.tiles.get(&tile).and_then(|i| i.rect);
        let mut chain = vec![tile];
        chain.extend(self.ancestors(tile));
        let mut cands: Vec<(DrapeCandidate, ResourceId, UvTransform)> = Vec::new();
        for (up, x) in chain.iter().enumerate() {
            let uv = if up == 0 {
                Some(UvTransform::IDENTITY)
            } else {
                match (rect, self.tiles.get(x).and_then(|i| i.rect)) {
                    (Some(a), Some(b)) => UvTransform::between(a, b),
                    _ => None,
                }
            };
            let Some(uv) = uv else { continue };
            // Only this layer's drapes: a layer never falls back on another's.
            let lo = ResourceId::drape(*x, layer, 0);
            let hi = ResourceId::drape(*x, layer, u32::MAX);
            for (r, e) in self.entries.range(lo..=hi) {
                if let (ResourceId::Drape { level, .. }, Some(_)) = (r, e.gpu) {
                    cands.push((
                        DrapeCandidate {
                            level: *level,
                            up: up as u32,
                        },
                        *r,
                        uv,
                    ));
                }
            }
        }
        let best = best_drape(need, cands.iter().map(|c| c.0))?;
        cands
            .into_iter()
            .find(|c| c.0 == best)
            .map(|(c, resource, uv)| DrapeChoice {
                resource,
                layer,
                level: c.level,
                uv,
            })
    }

    // 3. The draw set.
    fn build_draw(
        &self,
        tiles: &BTreeMap<TileId, TileNeed>,
        drapes: &BTreeMap<(TileId, LayerId), u32>,
        st: &mut BudgetStats,
    ) -> (DrawSet, BTreeSet<TileId>) {
        let mut items: BTreeMap<ResourceId, (Role, BTreeSet<TileId>)> = BTreeMap::new();
        let mut coarsening = BTreeSet::new();
        let add = |items: &mut BTreeMap<ResourceId, (Role, BTreeSet<TileId>)>,
                   t: TileId,
                   role: Role,
                   serves: TileId| {
            let e = items
                .entry(ResourceId::Mesh(t))
                .or_insert((role, BTreeSet::new()));
            e.0 = e.0.min(role);
            e.1.insert(serves);
        };
        for (t, tn) in tiles
            .iter()
            .filter(|(_, n)| n.membership == Membership::Drawn)
        {
            if self.drawable(ResourceId::Mesh(*t)) {
                add(&mut items, *t, Role::Exact, *t);
                st.exact += 1;
                continue;
            }
            let _ = tn;
            let held: Vec<TileId> = self
                .held_last
                .iter()
                .copied()
                .filter(|h| self.drawable(ResourceId::Mesh(*h)) && self.is_descendant(*h, *t))
                .collect();
            for h in &held {
                add(&mut items, *h, Role::Held, *t);
            }
            if !held.is_empty() {
                coarsening.insert(*t);
            }
            let anc = self
                .ancestors(*t)
                .into_iter()
                .find(|a| self.drawable(ResourceId::Mesh(*a)));
            if let Some(a) = anc {
                add(&mut items, a, Role::Fallback, *t);
                if held.is_empty() {
                    st.fallback += 1;
                }
            } else if held.is_empty() {
                st.lost += 1;
            }
        }
        let mut out = Vec::new();
        for (mesh, (role, serves)) in items {
            let tile = mesh.tile();
            // The level each layer is needed at: the tile's own need when
            // exact; a held tile keeps its own last level; a fallback
            // dresses for the finest of the tiles it serves.
            let mut needs: BTreeMap<LayerId, u32> = BTreeMap::new();
            for s in &serves {
                for ((_, layer), n) in drapes.range(tile_layers(*s)) {
                    let e = needs.entry(*layer).or_insert(*n);
                    *e = (*e).max(*n);
                }
            }
            if role == Role::Held {
                for (layer, n) in needs.iter_mut() {
                    if let Some(p) = self.imagery_prev.get(&(tile, *layer)) {
                        *n = *p;
                    }
                }
            }
            let mut chosen: Vec<DrapeChoice> = needs
                .iter()
                .filter_map(|(layer, n)| self.best_drape_for(tile, *layer, *n))
                .collect();
            if chosen.len() < needs.len() {
                // A layer switch in progress: a layer the tiles now ask for
                // is drawable nowhere yet, so the layers this mesh wore last
                // step and no longer needs stay worn until it is.
                for old in self.worn_last.get(&mesh).into_iter().flatten() {
                    if !needs.contains_key(&old.layer) && self.drawable(old.resource) {
                        chosen.push(*old);
                    }
                }
                chosen.sort_by_key(|d| d.layer);
            }
            if role == Role::Exact {
                let off = needs.iter().any(|(layer, n)| {
                    !chosen
                        .iter()
                        .any(|d| d.layer == *layer && d.level.abs_diff(*n) <= 1)
                });
                if off {
                    st.underdraped += 1;
                }
                for d in &chosen {
                    let gap = i64::from(d.level)
                        - i64::from(self.tiles.get(&tile).map(|i| i.level).unwrap_or(0));
                    let i = (gap + 8).clamp(0, GAP_BUCKETS as i64 - 1) as usize;
                    st.imagery_over_terrain[i] += 1;
                }
            }
            if role == Role::Held {
                st.held += 1;
            }
            out.push(DrawItem {
                mesh,
                role,
                drapes: chosen,
                serves: serves.into_iter().collect(),
            });
        }
        (DrawSet { items: out }, coarsening)
    }

    /// What the step wants loaded or delivered.
    fn wants(
        &self,
        tiles: &BTreeMap<TileId, TileNeed>,
        drapes: &BTreeMap<(TileId, LayerId), u32>,
        draw: &DrawSet,
    ) -> BTreeMap<ResourceId, Want> {
        let mut out: BTreeMap<ResourceId, Want> = BTreeMap::new();
        let batch = self.cfg.policy == Policy::Batch;
        let put = |out: &mut BTreeMap<ResourceId, Want>, r: ResourceId, w: Want| match out.get(&r) {
            Some(cur) if (cur.urgency, Reverse(cur.q)) <= (w.urgency, Reverse(w.q)) => {}
            _ => {
                out.insert(r, w);
            }
        };
        for (t, tn) in tiles {
            let floor = self.is_floor(tn.need.level);
            let mesh_drawable = self.drawable(ResourceId::Mesh(*t));
            let urgency = if floor {
                Urgency::Floor
            } else {
                match tn.membership {
                    Membership::Drawn => Urgency::Hole,
                    Membership::Blocker => Urgency::Blocker,
                    Membership::Ring => Urgency::Ring,
                    Membership::Ancestor => Urgency::Ancestor,
                }
            };
            put(
                &mut out,
                ResourceId::Mesh(*t),
                Want {
                    urgency,
                    q: tn.q,
                    class: tn.need.class,
                    level: tn.need.level,
                },
            );
            if floor {
                for reach in tn.need.imagery.as_ref().map(layers).unwrap_or(&[]) {
                    put(
                        &mut out,
                        ResourceId::drape(*t, reach.layer, tn.need.level),
                        Want {
                            urgency: Urgency::Floor,
                            q: tn.q,
                            class: Class::Drape,
                            level: tn.need.level,
                        },
                    );
                }
            }
            for ((_, layer), &n) in drapes.range(tile_layers(*t)) {
                let drape = ResourceId::drape(*t, *layer, n);
                let du = match tn.membership {
                    Membership::Blocker => Some(Urgency::Blocker),
                    _ if !mesh_drawable => Some(urgency),
                    _ => {
                        let worn = draw
                            .get(ResourceId::Mesh(*t))
                            .and_then(|i| i.drape(*layer).copied());
                        let off = match worn {
                            None => true,
                            Some(w) if batch => w.resource != drape,
                            Some(w) => {
                                w.level.abs_diff(n) > 1
                                    || (self.pressure != Pressure::Normal && w.level > n)
                            }
                        };
                        off.then_some(if floor {
                            Urgency::Floor
                        } else {
                            Urgency::Drape
                        })
                    }
                };
                if let Some(du) = du {
                    put(
                        &mut out,
                        drape,
                        Want {
                            urgency: du,
                            q: tn.q,
                            class: Class::Drape,
                            level: tn.need.level,
                        },
                    );
                }
            }
        }
        out
    }

    fn protection(&self, r: ResourceId) -> Option<Protection> {
        let e = self.entries.get(&r)?;
        if self.is_floor(e.level) && (e.gpu.is_some() || e.transit.is_some()) {
            return Some(Protection::Floor);
        }
        if self.drawn_last.contains(&r) {
            return Some(Protection::Drawn);
        }
        if e.transit.is_some() {
            return Some(Protection::InTransit);
        }
        if self.swap_last.contains(&r) {
            return Some(match r {
                ResourceId::Mesh(_) if self.chain_last.contains(&r) => Protection::Chain,
                _ => Protection::Swap,
            });
        }
        None
    }

    fn tier(
        &self,
        r: ResourceId,
        e: Option<&Entry>,
        class: Class,
        presence: &dyn PresenceOracle,
    ) -> Tier {
        let epoch = self.epoch(r, class);
        match e {
            Some(e) if e.gpu.is_some_and(|g| g.epoch == epoch) => Tier::Gpu,
            Some(e) if e.fresh_cpu(epoch).is_some() => Tier::Decoded,
            // A wrong hint is believed once; otherwise the index is asked
            // afresh, since a store fills as the session runs.
            Some(e) if e.store_missed => Tier::Remote,
            _ => match presence.holds(r) {
                Presence::Stored => Tier::Stored,
                Presence::Absent => Tier::Remote,
            },
        }
    }

    /// The step.
    pub(super) fn step(&mut self, input: FrameInput<'_>) -> Decisions {
        let now = input.now;
        let mut st = BudgetStats::default();
        let mut d = Decisions::default();
        let eff = self.eff();
        for r in std::mem::take(&mut self.ranked_last) {
            if let Some(e) = self.entries.get_mut(&r) {
                e.urgency = None;
                e.priority = None;
            }
        }
        self.steps += 1;

        self.apply_events(now, &mut st);
        let tiles = self.read_need(&input, &mut st);
        let drapes = self.drape_needs(&tiles, input.presence);
        let (draw, coarsening) = self.build_draw(&tiles, &drapes, &mut st);
        let mut wants = self.wants(&tiles, &drapes, &draw);
        // Drawn content of an older epoch is refreshed, new before old.
        for i in &draw.items {
            for r in std::iter::once(i.mesh).chain(i.drapes.iter().map(|x| x.resource)) {
                if let Some(e) = self.entries.get(&r) {
                    if e.stale_gpu(self.epoch(r, e.class)) && !wants.contains_key(&r) {
                        wants.insert(
                            r,
                            Want {
                                urgency: Urgency::Refresh,
                                q: Quantised::default(),
                                class: e.class,
                                level: e.level,
                            },
                        );
                    }
                }
            }
        }

        // 4. Mark: needed now, or unneeded since when.
        let mut drawn_res: BTreeSet<ResourceId> = BTreeSet::new();
        for i in &draw.items {
            drawn_res.insert(i.mesh);
            drawn_res.extend(i.drapes.iter().map(|x| x.resource));
        }
        self.drawn_last = drawn_res.clone();
        self.worn_last = draw
            .items
            .iter()
            .filter(|i| !i.drapes.is_empty())
            .map(|i| (i.mesh, i.drapes.clone()))
            .collect();
        self.chain_last = wants
            .iter()
            .filter(|(r, w)| w.urgency == Urgency::Ancestor && self.drawable(**r))
            .map(|(r, _)| *r)
            .collect();
        self.swap_last = wants
            .iter()
            .filter(|(r, w)| {
                matches!(
                    w.urgency,
                    Urgency::Hole | Urgency::Blocker | Urgency::Ancestor
                ) && self.drawable(**r)
            })
            .map(|(r, _)| *r)
            .collect();
        let mut keep = drawn_res.clone();
        keep.extend(self.swap_last.iter().copied());
        // Only what the view touched this step or the last: everything else
        // was already marked unneeded.
        let touched: BTreeSet<ResourceId> = wants
            .keys()
            .chain(drawn_res.iter())
            .chain(self.wanted_last.iter())
            .copied()
            .collect();
        for r in &touched {
            let Some(e) = self.entries.get_mut(r) else {
                continue;
            };
            if let Some(w) = wants.get(r) {
                e.last_needed = now;
                e.unneeded_since = None;
                e.first_wanted.get_or_insert(now);
                if let Some(tn) = tiles.get(&r.tile()) {
                    e.distance = tn.q.distance;
                }
                let _ = w;
            } else if drawn_res.contains(r) {
                e.last_needed = now;
                e.unneeded_since = None;
                e.first_wanted = None;
            } else {
                e.unneeded_since.get_or_insert(now);
                e.first_wanted = None;
            }
        }

        // 5. Cancellations: grace and timeouts.
        self.cancel(now, &eff, &mut d, &mut st);

        // 6. Rank the work.
        let fallback_cover: BTreeMap<ResourceId, u32> = draw
            .items
            .iter()
            .filter(|i| i.role == Role::Fallback)
            .map(|i| (i.mesh, i.serves.len() as u32))
            .collect();
        let mut keys: BTreeMap<ResourceId, PriorityKey> = BTreeMap::new();
        let mut jobs: Vec<(PriorityKey, ResourceId, Job, Want)> = Vec::new();
        for (r, w) in &wants {
            let class = self.class_of(*r, w.class);
            let epoch = self.epoch(*r, class);
            let e = self.entries.get(r);
            let mut urgency = w.urgency;
            let job = match e {
                Some(e) if e.gpu.is_some_and(|g| g.epoch == epoch) => None,
                Some(e) if e.transit.is_some() => None,
                Some(e) if e.fresh_cpu(epoch).is_some() => {
                    (e.deliver_after <= now).then_some(Job::Deliver)
                }
                Some(e) if matches!(e.load, Load::Loading { .. }) => None,
                Some(e) if e.backing_off(now) => None,
                _ => Some(Job::Load),
            };
            if e.is_some_and(|e| e.stale_gpu(epoch)) {
                urgency = Urgency::Refresh;
            }
            let waited = e
                .and_then(|e| e.first_wanted)
                .map(|f| now.since(f))
                .unwrap_or(Micros::ZERO);
            let cover = fallback_cover.get(r).copied().unwrap_or(0);
            let mut sc = score(w.q, waited, self.cfg.age_full, cover);
            if urgency == Urgency::Hole && coarsening.contains(&r.tile()) {
                // At the ceiling a coarser covering tile frees bytes: first.
                sc += 1_000_000;
            }
            let key = PriorityKey {
                urgency,
                score: Reverse(sc),
                tier: self.tier(*r, e, class, input.presence),
                resource: *r,
            };
            keys.insert(*r, key);
            if let Some(job) = job {
                jobs.push((key, *r, job, Want { urgency, ..*w }));
            }
        }
        jobs.sort_by_key(|j| j.0);
        self.ranked_last = jobs.iter().map(|j| j.1).collect();
        for (k, r, _, w) in &jobs {
            if let Some(e) = self.entries.get_mut(r) {
                e.urgency = Some(w.urgency);
                e.priority = Some(*k);
            }
        }

        let loads: Vec<_> = jobs
            .iter()
            .filter(|j| j.2 == Job::Load)
            .take(self.cfg.request_heap as usize)
            .copied()
            .collect();
        st.heap_len = loads.len() as u32;
        self.start(
            now,
            &eff,
            &loads,
            &keys,
            &wants,
            input.presence,
            &mut d,
            &mut st,
        );

        // 7. Deliveries.
        let delivers: Vec<_> = jobs
            .iter()
            .filter(|j| j.2 == Job::Deliver)
            .copied()
            .collect();
        let (blocked, strained) = self.deliver(
            now,
            &eff,
            &delivers,
            &coarsening,
            &keep,
            &wants,
            &mut d,
            &mut st,
        );

        // 8. The upload plan.
        d.upload_plan = self.plan(&eff, &keys, &tiles, &drapes);

        // 9. Eviction and release.
        if !eff.batch {
            self.evict(now, &eff, &keep, &mut d, &mut st);
        }

        // 10. Relaxation.
        let protected = self.protected_bytes();
        self.relax(now, &eff, &protected, blocked, strained);
        let thermal_floor = match (eff.batch, self.thermal) {
            (true, _) => 1_000,
            (_, Thermal::Serious) => 1_500,
            (_, Thermal::Critical) => 2_000,
            _ => 1_000,
        };
        d.sse_scale_milli = self.scale.max(thermal_floor);
        self.last_now = Some(now);

        // Batch completeness.
        let pending_view = jobs.iter().any(|j| j.3.urgency <= Urgency::Drape)
            || wants.iter().any(|(r, w)| {
                w.urgency <= Urgency::Drape
                    && self.entries.get(r).is_some_and(|e| {
                        e.transit.is_some() || matches!(e.load, Load::Loading { .. })
                    })
            });
        if eff.batch {
            let complete = input.need.blockers.is_empty()
                && tiles
                    .iter()
                    .filter(|(_, n)| n.membership == Membership::Drawn)
                    .all(|(t, _)| {
                        let item = draw.get(ResourceId::Mesh(*t));
                        let exact = item.is_some_and(|i| i.role == Role::Exact);
                        let drape_ok = drapes.range(tile_layers(*t)).all(|((_, l), &n)| {
                            item.and_then(|i| i.drape(*l))
                                .is_some_and(|x| x.resource == ResourceId::drape(*t, *l, n))
                        });
                        exact && drape_ok
                    });
            d.frame_ready = complete && self.fatal.is_none();
        }
        d.fatal = self.fatal.clone();
        st.converged = if eff.batch {
            d.frame_ready
        } else {
            st.lost == 0 && st.fallback == 0 && st.held == 0 && st.underdraped == 0 && !pending_view
        };

        // Bookkeeping for the next step.
        self.held_last = draw
            .items
            .iter()
            .filter(|i| i.role != Role::Fallback)
            .map(|i| i.mesh.tile())
            .collect();
        self.wanted_last = wants
            .keys()
            .copied()
            .chain(drawn_res.iter().copied())
            .collect();
        self.prune(now, &tiles, &wants, &drapes);

        // Statistics.
        st.gpu = self.ledger.gpu;
        st.target = eff.target;
        st.overflow = eff.overflow;
        st.decoded = self.ledger.decoded;
        st.in_transit = self.ledger.in_transit;
        st.sse_scale_milli = d.sse_scale_milli;
        st.in_flight = self.in_flight;
        st.local_reads = self.local_reads;
        st.tracked = self.entries.len() as u32;
        st.protected = protected;
        d.draw = draw;
        d.stats = st;
        d
    }

    fn protected_bytes(&self) -> PerClass<Bytes> {
        let mut p = PerClass::splat(Bytes::ZERO);
        let candidates: BTreeSet<ResourceId> = self
            .drawn_last
            .iter()
            .chain(&self.swap_last)
            .chain(&self.active)
            .chain(&self.floor_set)
            .copied()
            .collect();
        for r in candidates {
            let Some(e) = self.entries.get(&r) else {
                continue;
            };
            if self.protection(r).is_some() {
                let b = e
                    .gpu
                    .map(|g| g.bytes)
                    .unwrap_or(Bytes::ZERO)
                    .plus(e.transit.map(|t| t.cost.gpu).unwrap_or(Bytes::ZERO));
                p[e.class] = p[e.class].plus(b);
            }
        }
        p
    }

    fn cancel(&mut self, now: Micros, eff: &Eff, d: &mut Decisions, st: &mut BudgetStats) {
        let grace = self.cfg.cancel_grace;
        let timeout = self.cfg.load_timeout;
        let batch = eff.batch;
        let failure = self.cfg.failure;
        let mut fatal = None;
        let active: Vec<ResourceId> = self.active.iter().copied().collect();
        for r in &active {
            let Some(e) = self.entries.get_mut(r) else {
                continue;
            };
            if let Load::Loading {
                gen,
                source,
                started,
                ..
            } = e.load
            {
                let unneeded = e.unneeded_since.is_some_and(|u| now.since(u) >= grace);
                let timed_out = now.since(started) >= timeout;
                if (unneeded && !batch) || timed_out {
                    e.load = Load::Idle;
                    match source {
                        Source::Network => self.in_flight[e.class] -= 1,
                        Source::Local => self.local_reads -= 1,
                    }
                    d.cancel.push((*r, gen));
                    st.cancelled += 1;
                    if timed_out && !unneeded {
                        if batch {
                            fatal.get_or_insert(SessionError::LoadFailed(*r));
                        } else {
                            e.failures += 1;
                            e.retry_at = now.plus(backoff(failure, e.failures, Failure::Transient));
                        }
                    }
                }
            }
            if !batch {
                if let Some(t) = e.transit {
                    if e.unneeded_since.is_some_and(|u| now.since(u) >= grace)
                        && !self.drawn_last.contains(r)
                    {
                        e.take_transit(&mut self.ledger);
                        d.cancel.push((*r, t.gen));
                    }
                }
            }
        }
        if let Some(f) = fatal {
            self.fatal.get_or_insert(f);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn start(
        &mut self,
        now: Micros,
        eff: &Eff,
        loads: &[(PriorityKey, ResourceId, Job, Want)],
        keys: &BTreeMap<ResourceId, PriorityKey>,
        wants: &BTreeMap<ResourceId, Want>,
        presence: &dyn PresenceOracle,
        d: &mut Decisions,
        st: &mut BudgetStats,
    ) {
        let decoded_full = self.ledger.decoded >= eff.decoded;
        for (key, r, _, w) in loads {
            let class = self.class_of(*r, w.class);
            if decoded_full && w.urgency >= Urgency::Ring {
                // Backpressure: preload that could neither be delivered nor
                // kept decoded is not fetched.
                continue;
            }
            let source = if key.tier == Tier::Stored {
                Source::Local
            } else {
                Source::Network
            };
            match source {
                Source::Local => {
                    if self.local_reads >= eff.local {
                        continue;
                    }
                }
                Source::Network => {
                    if self.in_flight[class] >= eff.quota[class] {
                        if w.urgency > Urgency::Hole {
                            continue;
                        }
                        // Preempt the least urgent in-flight preload.
                        let victim = self
                            .entries
                            .iter()
                            .filter(|(vr, ve)| {
                                ve.class == class
                                    && matches!(
                                        ve.load,
                                        Load::Loading {
                                            source: Source::Network,
                                            ..
                                        }
                                    )
                                    && wants.get(vr).is_some_and(|vw| {
                                        matches!(vw.urgency, Urgency::Ring | Urgency::Ancestor)
                                    })
                                    || ve.class == class
                                        && matches!(
                                            ve.load,
                                            Load::Loading {
                                                source: Source::Network,
                                                ..
                                            }
                                        )
                                        && ve.urgency == Some(Urgency::Refresh)
                            })
                            .filter_map(|(vr, _)| keys.get(vr).map(|k| (*k, *vr)))
                            .max();
                        let Some((_, vr)) = victim else { continue };
                        if let Some(ve) = self.entries.get_mut(&vr) {
                            if let Load::Loading { gen, .. } = ve.load {
                                ve.load = Load::Idle;
                                self.in_flight[class] -= 1;
                                d.cancel.push((vr, gen));
                                st.cancelled += 1;
                            }
                        }
                    }
                }
            }
            let gen = Generation(self.next_gen);
            self.next_gen += 1;
            let epoch = self.epoch(*r, class);
            let e = self
                .entries
                .entry(*r)
                .or_insert_with(|| Entry::new(class, w.level, now));
            if matches!(e.load, Load::Loading { .. }) {
                st.duplicates_in_flight += 1;
                continue;
            }
            if e.presence.is_none() && !e.store_missed {
                e.presence = Some(presence.holds(*r));
            }
            e.first_wanted.get_or_insert(now);
            e.last_needed = now;
            e.unneeded_since = None;
            e.urgency = Some(w.urgency);
            e.priority = Some(*key);
            e.load = Load::Loading {
                gen,
                source,
                started: now,
                epoch,
            };
            if self.cfg.pinned_level.is_some_and(|p| w.level <= p) {
                self.floor_set.insert(*r);
            }
            self.active.insert(*r);
            match source {
                Source::Network => {
                    self.in_flight[class] += 1;
                    st.started_network += 1;
                }
                Source::Local => {
                    self.local_reads += 1;
                    st.started_local += 1;
                }
            }
            if let Some(t) = self.recent.remove(r) {
                if now.since(t) <= THRASH_WINDOW {
                    // Back from a local store is a demotion paying off; back
                    // from the network is thrash.
                    match source {
                        Source::Network => st.thrash += 1,
                        Source::Local => st.rereads += 1,
                    }
                }
            }
            d.start.push(Start {
                resource: *r,
                gen,
                class,
                source,
            });
        }
    }

    /// Returns whether the hard limit held anything back.
    /// Evicts unprotected residents of `class` worth less than work of
    /// `urgency`, in eviction order, until `bytes` more fit under `limit`.
    /// Admission preempts lower-value residents rather than refusing work.
    #[allow(clippy::too_many_arguments)]
    fn make_room(
        &mut self,
        now: Micros,
        class: Class,
        bytes: Bytes,
        limit: Bytes,
        urgency: Urgency,
        keep: &BTreeSet<ResourceId>,
        wants: &BTreeMap<ResourceId, Want>,
        d: &mut Decisions,
        st: &mut BudgetStats,
        strained: &mut bool,
    ) -> bool {
        if self.ledger.gpu[class].plus(bytes) <= limit {
            return true;
        }
        let mut victims: Vec<_> = self
            .entries
            .iter()
            .filter(|(r, e)| {
                e.class == class
                    && e.gpu.is_some()
                    && e.transit.is_none()
                    && !keep.contains(r)
                    && !self.is_floor(e.level)
                    && wants
                        .get(r)
                        .is_none_or(|w| urgency < Urgency::Ring && w.urgency > urgency)
            })
            .map(|(r, e)| {
                (
                    (e.last_needed, Reverse(e.level), Reverse(e.distance), *r),
                    *r,
                    e.gpu.map(|g| g.bytes).unwrap_or(Bytes::ZERO),
                )
            })
            .collect();
        victims.sort();
        let freeable = victims.iter().fold(Bytes::ZERO, |a, v| a.plus(v.2));
        if self.ledger.gpu[class].minus(freeable).plus(bytes) > limit {
            *strained = true;
            return false;
        }
        for (_, r, _) in victims {
            if self.ledger.gpu[class].plus(bytes) <= limit {
                break;
            }
            if wants.contains_key(&r) {
                // Wanted work displaces wanted work: the view asks for more
                // than the budget holds.
                *strained = true;
            }
            if let Some(e) = self.entries.get_mut(&r) {
                e.take_gpu(&mut self.ledger);
                d.evict.push(r);
                st.evicted += 1;
                self.recent.insert(r, now);
            }
        }
        true
    }

    #[allow(clippy::too_many_arguments)]
    fn deliver(
        &mut self,
        now: Micros,
        eff: &Eff,
        jobs: &[(PriorityKey, ResourceId, Job, Want)],
        coarsening: &BTreeSet<TileId>,
        keep: &BTreeSet<ResourceId>,
        wants: &BTreeMap<ResourceId, Want>,
        d: &mut Decisions,
        st: &mut BudgetStats,
    ) -> (bool, bool) {
        let mut blocked = false;
        let mut strained = false;
        for (_, r, _, w) in jobs {
            let Some(e) = self.entries.get(r) else {
                continue;
            };
            let class = e.class;
            let Some(c) = e.fresh_cpu(self.epoch(*r, class)) else {
                continue;
            };
            let credit_ok = self.ledger.in_transit == Bytes::ZERO
                || self.ledger.in_transit.plus(c.cost.staging) <= self.cfg.delivery_credit;
            if !credit_ok {
                continue;
            }
            if !eff.batch {
                let shrinking = matches!(r, ResourceId::Mesh(t) if coarsening.contains(t));
                let urgent = matches!(w.urgency, Urgency::Floor | Urgency::Hole | Urgency::Drape);
                let preload = w.urgency >= Urgency::Ring;
                let limit = if shrinking {
                    eff.hard[class]
                } else if urgent {
                    eff.hard[class].minus(eff.reserve[class])
                } else if preload {
                    // Preload only into room the view does not use, and it
                    // never displaces anything wanted.
                    eff.target[class].scaled(u64::from(self.cfg.watermarks.low_percent), 100)
                } else {
                    eff.target[class].min(eff.hard[class].minus(eff.reserve[class]))
                };
                if preload && (strained || blocked) {
                    continue;
                }
                let mut strain = false;
                let ok = self.make_room(
                    now,
                    class,
                    c.cost.gpu,
                    limit,
                    w.urgency,
                    keep,
                    wants,
                    d,
                    st,
                    &mut strain,
                );
                if strain && w.urgency <= Urgency::Blocker {
                    strained = true;
                }
                if !ok {
                    if urgent || shrinking {
                        blocked = true;
                    }
                    continue;
                }
            }
            let reused = c.delivered_before;
            let Some(e) = self.entries.get_mut(r) else {
                continue;
            };
            self.active.insert(*r);
            e.put_transit(
                &mut self.ledger,
                Transit {
                    gen: c.gen,
                    cost: c.cost,
                    epoch: c.epoch,
                },
            );
            if reused {
                st.network_bytes_avoided = st.network_bytes_avoided.plus(c.cost.gpu);
            }
            st.delivered += 1;
            d.deliver.push((*r, c.gen));
        }
        (blocked, strained)
    }

    fn plan(
        &self,
        eff: &Eff,
        keys: &BTreeMap<ResourceId, PriorityKey>,
        tiles: &BTreeMap<TileId, TileNeed>,
        drapes: &BTreeMap<(TileId, LayerId), u32>,
    ) -> UploadPlan {
        // Group: the blockers of one parent (meshes and drapes), else a
        // tile's mesh and its drape.
        let group_of = |r: ResourceId| -> (u8, TileId) {
            let t = r.tile();
            match tiles.get(&t) {
                Some(tn) if tn.membership == Membership::Blocker => match tn.need.parent {
                    Some(p) => (0, p),
                    None => (1, t),
                },
                _ => (1, t),
            }
        };
        let mut transit: Vec<(PriorityKey, ResourceId, Bytes)> = self
            .entries
            .iter()
            .filter_map(|(r, e)| e.transit.map(|t| (*r, t.cost.gpu)))
            .map(|(r, b)| {
                let k = keys.get(&r).copied().unwrap_or(PriorityKey {
                    urgency: Urgency::Refresh,
                    score: Reverse(i64::MIN),
                    tier: Tier::Remote,
                    resource: r,
                });
                (k, r, b)
            })
            .collect();
        transit.sort();
        let _ = drapes;
        let mut taken: BTreeSet<ResourceId> = BTreeSet::new();
        let mut plan = UploadPlan::default();
        let mut groups: BTreeMap<(u8, TileId), Vec<ResourceId>> = BTreeMap::new();
        for (_, r, _) in &transit {
            groups.entry(group_of(*r)).or_default().push(*r);
        }
        for (_, r, _) in &transit {
            if taken.contains(r) {
                continue;
            }
            let g = group_of(*r);
            let members: Vec<ResourceId> = groups.get(&g).cloned().unwrap_or_else(|| vec![*r]);
            let bytes = members.iter().fold(Bytes::ZERO, |a, m| {
                a.plus(
                    transit
                        .iter()
                        .find(|x| x.1 == *m)
                        .map(|x| x.2)
                        .unwrap_or(Bytes::ZERO),
                )
            });
            if plan.items.is_empty() && plan.bytes.plus(bytes) > eff.upload {
                // The first item always goes, alone if its group is too big.
                let b = transit
                    .iter()
                    .find(|x| x.1 == *r)
                    .map(|x| x.2)
                    .unwrap_or(Bytes::ZERO);
                plan.items.push(*r);
                plan.bytes = b;
                break;
            }
            if plan.bytes.plus(bytes) > eff.upload {
                break;
            }
            for m in &members {
                taken.insert(*m);
                plan.items.push(*m);
            }
            plan.bytes = plan.bytes.plus(bytes);
            if members.len() > 1 {
                let mut sorted = members.clone();
                sorted.sort();
                plan.groups.push(SwapGroup { members: sorted });
            }
        }
        plan
    }

    fn evict(
        &mut self,
        now: Micros,
        eff: &Eff,
        drawn: &BTreeSet<ResourceId>,
        d: &mut Decisions,
        st: &mut BudgetStats,
    ) {
        let w = self.cfg.watermarks;
        for class in Class::ALL {
            let used = self.ledger.gpu[class];
            let high = eff.target[class].scaled(u64::from(w.high_percent), 100);
            if used <= high {
                continue;
            }
            let low = eff.target[class].scaled(u64::from(w.low_percent), 100);
            let mut cands: Vec<_> = self
                .entries
                .iter()
                .filter(|(r, e)| {
                    e.class == class
                        && e.gpu.is_some()
                        && !drawn.contains(r)
                        && e.transit.is_none()
                        && !self.is_floor(e.level)
                })
                .map(|(r, e)| {
                    (
                        (e.last_needed, Reverse(e.level), Reverse(e.distance), *r),
                        *r,
                    )
                })
                .collect();
            cands.sort();
            for (_, r) in cands {
                if self.ledger.gpu[class] <= low {
                    break;
                }
                if let Some(e) = self.entries.get_mut(&r) {
                    e.take_gpu(&mut self.ledger);
                    d.evict.push(r);
                    st.evicted += 1;
                    self.recent.insert(r, now);
                }
            }
        }
        if self.ledger.decoded > eff.decoded {
            let mut cands: Vec<_> = self
                .entries
                .iter()
                .filter(|(_, e)| e.cpu.is_some() && e.transit.is_none())
                // Release order: copies of protected GPU residents first
                // (they will not be evicted soon), then of evictable GPU
                // residents (their copy is what an eviction demotes to),
                // then copies already demoted (they save a re-read when the
                // view comes back).
                .map(|(r, e)| {
                    let rank = match (e.gpu.is_some(), drawn.contains(r)) {
                        (true, true) => 0u8,
                        (true, false) => 1,
                        (false, _) => 2,
                    };
                    (
                        (
                            rank,
                            e.last_needed,
                            Reverse(e.level),
                            Reverse(e.distance),
                            *r,
                        ),
                        *r,
                    )
                })
                .collect();
            cands.sort();
            for (_, r) in cands {
                if self.ledger.decoded <= eff.decoded {
                    break;
                }
                if let Some(e) = self.entries.get_mut(&r) {
                    e.take_cpu(&mut self.ledger);
                    d.release.push(r);
                    st.released += 1;
                }
            }
        }
    }

    fn relax(
        &mut self,
        now: Micros,
        eff: &Eff,
        protected: &PerClass<Bytes>,
        blocked: bool,
        strained: bool,
    ) {
        if eff.batch {
            self.scale = 1_000;
            return;
        }
        let dt = self
            .last_now
            .map(|l| now.since(l))
            .unwrap_or(Micros::ZERO)
            .0;
        let over = strained
            || Class::ALL
                .iter()
                .any(|&c| protected[c] > eff.target[c].plus(eff.overflow[c]));
        let at_hard = blocked
            || Class::ALL.iter().any(|&c| {
                eff.hard[c] != Bytes::MAX && self.ledger.gpu[c] >= eff.hard[c].minus(eff.reserve[c])
            });
        // The scale is kept in millionths of a thousandth, so no step's
        // rounding is lost: a rate is the same at 30 and at 120 fps.
        let r = self.cfg.relax;
        let one = 1_000u64 * 1_000_000;
        let max = u64::from(r.max_scale) * 1_000_000;
        if over || at_hard {
            let rate = u64::from(r.up_per_second) * if at_hard { 4 } else { 1 };
            self.scale_fine = self
                .scale_fine
                .saturating_add(rate.saturating_mul(dt))
                .min(max);
        } else {
            let calm = Class::ALL.iter().all(|&c| {
                self.ledger.gpu[c]
                    <= eff.target[c].scaled(u64::from(self.cfg.watermarks.low_percent), 100)
            });
            if calm {
                let down = u64::from(r.down_per_second).saturating_mul(dt);
                self.scale_fine = self.scale_fine.saturating_sub(down).max(one);
            }
        }
        self.scale = (self.scale_fine / 1_000_000) as u32;
    }

    fn prune(
        &mut self,
        now: Micros,
        tiles: &BTreeMap<TileId, TileNeed>,
        wants: &BTreeMap<ResourceId, Want>,
        drapes: &BTreeMap<(TileId, LayerId), u32>,
    ) {
        // Amortised: emptied entries and unreferenced tiles go every eighth
        // step, so a step does not scan all it tracks.
        if !self.steps.is_multiple_of(8) {
            let held = &self.held_last;
            self.imagery_prev
                .retain(|k, _| drapes.contains_key(k) || held.contains(&k.0));
            return;
        }
        let entries = &self.entries;
        self.active.retain(|r| {
            entries
                .get(r)
                .is_some_and(|e| e.transit.is_some() || matches!(e.load, Load::Loading { .. }))
        });
        self.entries.retain(|r, e| {
            if !e.is_empty() {
                return true;
            }
            if e.backing_off(now)
                && e.unneeded_since
                    .is_none_or(|u| now.since(u) < Micros::from_secs(30))
            {
                return true;
            }
            wants.contains_key(r) && e.unneeded_since.is_none()
        });
        let entries = &self.entries;
        self.floor_set.retain(|r| entries.contains_key(r));
        // Keep only what is wanted this step among idle, never-started entries.
        self.recent.retain(|_, t| now.since(*t) <= THRASH_WINDOW);
        while self.recent.len() > RECENT_CAP {
            let oldest = self
                .recent
                .iter()
                .min_by_key(|(r, t)| (**t, **r))
                .map(|(r, _)| *r);
            if let Some(r) = oldest {
                self.recent.remove(&r);
            }
        }
        let mut keep: BTreeSet<TileId> = tiles.keys().copied().collect();
        keep.extend(self.entries.keys().map(|r| r.tile()));
        keep.extend(self.held_last.iter().copied());
        let seeds: Vec<TileId> = keep.iter().copied().collect();
        for s in seeds {
            let mut cur = s;
            while let Some(p) = self.tiles.get(&cur).and_then(|i| i.parent) {
                if !keep.insert(p) {
                    break;
                }
                cur = p;
            }
        }
        self.tiles.retain(|t, _| keep.contains(t));
        let held = &self.held_last;
        self.imagery_prev
            .retain(|k, _| drapes.contains_key(k) || held.contains(&k.0));
    }

    pub(super) fn release_beyond_impl(&mut self, target: Bytes) -> Vec<ResourceId> {
        let total = |l: &Ledger| {
            Class::ALL
                .iter()
                .fold(Bytes::ZERO, |a, &c| a.plus(l.gpu[c]))
        };
        let mut cands: Vec<_> = self
            .entries
            .iter()
            .filter(|(r, e)| {
                e.gpu.is_some()
                    && e.transit.is_none()
                    && !self.wanted_last.contains(r)
                    && !self.is_floor(e.level)
            })
            .map(|(r, e)| {
                (
                    (e.last_needed, Reverse(e.level), Reverse(e.distance), *r),
                    *r,
                )
            })
            .collect();
        cands.sort();
        let mut out = Vec::new();
        for (_, r) in cands {
            if total(&self.ledger) <= target {
                break;
            }
            if let Some(e) = self.entries.get_mut(&r) {
                e.take_gpu(&mut self.ledger);
                out.push(r);
            }
        }
        out
    }

    pub(super) fn view(&self, r: ResourceId) -> Option<ResourceView> {
        let e = self.entries.get(&r)?;
        let now = self.last_now.unwrap_or(Micros::ZERO);
        let state = match e.load {
            Load::Loading { source, .. } => ResourceState::Loading { source },
            Load::Idle if e.transit.is_some() => ResourceState::InTransit,
            Load::Idle if e.gpu.is_some() => ResourceState::Drawable,
            Load::Idle if e.cpu.is_some() => ResourceState::Loaded,
            Load::Idle if e.backing_off(now) => ResourceState::Failed {
                retries: e.failures,
                next_at: e.retry_at,
            },
            Load::Idle => ResourceState::Wanted,
        };
        let gen = match e.load {
            Load::Loading { gen, .. } => Some(gen),
            Load::Idle => e
                .transit
                .map(|t| t.gen)
                .or(e.gpu.map(|g| g.gen))
                .or(e.cpu.map(|c| c.gen)),
        };
        let epoch = self.epoch(r, e.class);
        let tier = if e.gpu.is_some() {
            Tier::Gpu
        } else if e.fresh_cpu(epoch).is_some() {
            Tier::Decoded
        } else if e.presence == Some(Presence::Stored) && !e.store_missed {
            Tier::Stored
        } else {
            Tier::Remote
        };
        Some(ResourceView {
            state,
            tier,
            gen,
            gpu: e.gpu.map(|g| g.bytes).unwrap_or(Bytes::ZERO),
            decoded: e.cpu.map(|c| c.cost.gpu).unwrap_or(Bytes::ZERO),
            protected: self.protection(r),
            urgency: e.urgency,
            priority: e.priority,
        })
    }

    pub(super) fn audit_impl(&self) -> Result<(), AuditError> {
        let mut l = Ledger::default();
        let mut flight = PerClass::splat(0u32);
        for e in self.entries.values() {
            if let Some(g) = e.gpu {
                l.gpu[e.class] = l.gpu[e.class].plus(g.bytes);
            }
            if let Some(t) = e.transit {
                l.gpu[e.class] = l.gpu[e.class].plus(t.cost.gpu);
                l.in_transit = l.in_transit.plus(t.cost.staging);
            }
            if let Some(c) = e.cpu {
                l.decoded = l.decoded.plus(c.cost.gpu);
            }
            if let Load::Loading {
                source: Source::Network,
                ..
            } = e.load
            {
                flight[e.class] += 1;
            }
        }
        for c in Class::ALL {
            if l.gpu[c] != self.ledger.gpu[c] {
                return Err(AuditError::Gpu {
                    class: c,
                    ledger: self.ledger.gpu[c].0,
                    states: l.gpu[c].0,
                });
            }
            if flight[c] != self.in_flight[c] {
                return Err(AuditError::InFlight {
                    class: c,
                    ledger: self.in_flight[c],
                    states: flight[c],
                });
            }
        }
        if l.decoded != self.ledger.decoded {
            return Err(AuditError::Decoded {
                ledger: self.ledger.decoded.0,
                states: l.decoded.0,
            });
        }
        Ok(())
    }
}
