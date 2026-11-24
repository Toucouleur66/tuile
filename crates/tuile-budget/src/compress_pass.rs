// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The mechanism of [`crate::compress`]: what the policy proposes, checked
//! against the ledger, decided, and its acknowledgement applied.

use super::compress::Waiting;
use super::compress::{eligible, ranks_before, track, CompressStats, Flight, Survival};
use super::cost::{within_drift, Cost};
use super::decisions::Decisions;
use super::state::Gpu;
use super::state::{EvictKey, Slot};
use super::stats::BudgetStats;
use super::types::{Bytes, Class, Generation, Micros, PerClass, ResourceId};
use super::{Budget, Ledger};

/// An eligible copy, as the pass found it: its eviction key, resource,
/// slot, what waits of it, its GPU copy and its class.
pub(super) type Found = (EvictKey, ResourceId, Slot, Waiting, Gpu, Class);

impl Budget {
    /// A GPU copy of `r` at `gen` became drawable, delivered at `cost` and
    /// acknowledged at `actual`: waiting to be compressed when its cost says
    /// it can be into less than it holds, forgotten otherwise.
    pub(super) fn compress_admit(
        &mut self,
        r: ResourceId,
        gen: Generation,
        cost: Cost,
        actual: Bytes,
    ) {
        match cost.compresses_to() {
            Some(to) if to < actual => self.compress.admit(r, gen, to, cost.compress_blocks),
            _ => {
                if !self.compress.waiting.is_empty() || !self.compress.flights.is_empty() {
                    self.compress.waiting.remove(&r);
                    self.compress.flights.remove(&r);
                }
            }
        }
    }

    /// The consumer compressed `r` at `gen`, which now holds `actual`.
    pub(super) fn compress_done(
        &mut self,
        r: ResourceId,
        gen: Generation,
        actual: Bytes,
        st: &mut BudgetStats,
    ) {
        let Some(f) = self.compress.flights.get(&r).copied() else {
            st.stale += 1;
            return;
        };
        if f.gen != gen {
            st.stale += 1;
            return;
        }
        self.compress.flights.remove(&r);
        let Some(s) = self.arena.find(r) else {
            return;
        };
        let Some(g) = self.arena.gpu(s).filter(|g| g.gen == gen) else {
            return;
        };
        let c = &mut st.compress;
        if actual >= f.from {
            // It could not be compressed after all: held as it is, and
            // not asked again.
            c.refused += 1;
        } else {
            c.done += 1;
            c.freed = c.freed.plus(g.bytes.minus(actual));
            if !within_drift(f.to, actual) {
                c.drift += 1;
            }
        }
        self.arena
            .put_gpu(s, &mut self.ledger, Gpu { bytes: actual, ..g });
    }

    /// The ledger as it will be once the compressions in flight are
    /// acknowledged (law 4): what an eviction sweep weighs.
    pub(super) fn settled(&self) -> Ledger {
        if self.compress.flights.is_empty() {
            return self.ledger;
        }
        let saving = self.compress.saving();
        let mut l = self.ledger;
        for c in Class::ALL {
            l.gpu[c] = l.gpu[c].minus(saving[c]);
        }
        l
    }

    /// One step's compressions (laws 2–6): the waiting copies the visit
    /// reaches, their score sampled into their window, those the policy
    /// finds eligible — `pressed` per class over its low watermark —
    /// highest value first, while the compressions in flight write at most
    /// `lane` blocks (one always admitted when none is in flight) and the
    /// transient stays under `hard`.
    pub(super) fn compress_pass(
        &mut self,
        now: Micros,
        still: bool,
        lane: u64,
        hard: &PerClass<Bytes>,
        pressed: &PerClass<bool>,
        d: &mut Decisions,
        st: &mut BudgetStats,
    ) {
        if self.compress.waiting.is_empty() && self.compress.flights.is_empty() {
            return;
        }
        // Law 2 at rest: since when the need has not changed.
        let still_since = if still {
            *self.compress.still_since.get_or_insert(now)
        } else {
            self.compress.still_since = None;
            now
        };
        let still_for = now.since(still_since);
        let step = self.steps;
        // The flights whose copy left the GPU, or was delivered again, are
        // over: nothing of them is acknowledged.
        let a = &self.arena;
        self.compress.flights.retain(|r, f| {
            a.find(*r)
                .and_then(|s| a.gpu(s))
                .is_some_and(|g| g.gen == f.gen)
        });
        let p = self.costed.params.compress;
        let mut visit = std::mem::take(&mut self.scratch.compress_visit);
        self.compress.visit(usize::from(p.visits), &mut visit);
        let mut c = CompressStats::default();
        let mut found = std::mem::take(&mut self.scratch.compress_found);
        found.clear();
        for &r in &visit {
            c.visited += 1;
            let Some(w) = self.compress.waiting.get(&r).copied() else {
                continue;
            };
            let a = &self.arena;
            let Some((s, g)) = a
                .find(r)
                .and_then(|s| a.gpu(s).map(|g| (s, g)))
                .filter(|(_, g)| g.gen == w.gen && w.to < g.bytes)
            else {
                // Its copy left the GPU, or changed: forgotten.
                self.compress.waiting.remove(&r);
                continue;
            };
            if a.has_transit(s) {
                // Delivered again, on its way: it starts over when it lands.
                continue;
            }
            let (_, probability, _) = a.place(s);
            let class = a.class(s);
            let high_from = track(&p, probability, w.high_from, step);
            if high_from != w.high_from {
                if let Some(e) = self.compress.waiting.get_mut(&r) {
                    e.high_from = high_from;
                }
            }
            let survival = Survival {
                probability,
                high_from,
                step,
                still_for,
            };
            if !eligible(&p, survival, pressed[class]) {
                continue;
            }
            c.eligible += 1;
            found.push((a.evict_key(s), r, s, w, g, class));
        }
        visit.clear();
        self.scratch.compress_visit = visit;
        found.sort_by(|x, y| ranks_before((&x.0, x.1), (&y.0, y.1)));
        let mut blocks = self.compress.blocks_in_flight();
        for &(_, r, s, w, g, class) in &found {
            if blocks > 0 && blocks.saturating_add(w.blocks) > lane {
                c.lane_full += 1;
                continue;
            }
            if self.ledger.gpu[class].plus(w.to) > hard[class] {
                c.over_hard += 1;
                continue;
            }
            // Charged both from now on: the compressed copy is made before
            // the RGBA8 one goes (law 5).
            self.arena.put_gpu(
                s,
                &mut self.ledger,
                Gpu {
                    bytes: g.bytes.plus(w.to),
                    ..g
                },
            );
            self.compress.waiting.remove(&r);
            self.compress.flights.insert(
                r,
                Flight {
                    gen: g.gen,
                    class,
                    from: g.bytes,
                    to: w.to,
                    blocks: w.blocks,
                },
            );
            blocks = blocks.saturating_add(w.blocks);
            d.compress.push((r, g.gen));
            c.started += 1;
        }
        found.clear();
        self.scratch.compress_found = found;
        st.work = st.work.saturating_add(c.visited);
        let done = std::mem::take(&mut st.compress);
        c.done = done.done;
        c.refused = done.refused;
        c.drift = done.drift;
        c.freed = done.freed;
        c.waiting = u32::try_from(self.compress.waiting.len()).unwrap_or(u32::MAX);
        c.in_flight = u32::try_from(self.compress.flights.len()).unwrap_or(u32::MAX);
        c.saving = self.compress.saving();
        st.compress = c;
    }
}
