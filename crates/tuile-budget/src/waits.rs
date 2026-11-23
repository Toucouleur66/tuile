// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Why a tile waits (`docs/explorations/budget-progressive-coarse-preload.md`
//! §7): every wait of the present need — a tile it wants drawn that is not
//! drawn as itself, from the step it is wanted to the step it is —
//! classified as a first load or a reload after an eviction, and for a
//! reload, how long after the eviction and why it was evicted. What the
//! viewer, the board and the metrics read to explain a latency, live. The
//! meter follows the waits ([`crate::BudgetMeter`]); `decide` does not.

use std::collections::{BTreeMap, VecDeque};

use crate::decisions::{Decisions, Role};
use crate::need::NeedSet;
use crate::score::{Reason, Side};
use crate::stats::{level_index, LATENCY_BUCKETS, LEVELS};
use crate::types::{Class, Micros, PerClass, ResourceId, TileId};

/// The kinds of a wait, in [`WaitTotals`]' order: a first load; a reload
/// whose copy was evicted less than 5 s before, less than 30 s before; a
/// reload of a copy evicted longer ago (or never seen evicted).
pub const WAIT_KINDS: [&str; 3] = ["first", "reload_5s", "reload_30s"];

/// How long an eviction is remembered for a reload to be told from a first
/// load.
pub const RELOAD_MEMORY: Micros = Micros::from_secs(30);

/// The kind of a wait for a resource evicted `since` ago (`None`: never,
/// or longer than [`RELOAD_MEMORY`]).
pub fn kind(since: Option<Micros>) -> usize {
    match since {
        Some(t) if t < Micros::from_secs(5) => 1,
        Some(t) if t < RELOAD_MEMORY => 2,
        _ => 0,
    }
}

/// The bucket of a wait in [`WaitTotals::millis`]: bucket `i` counts waits
/// of `[2^(i-1), 2^i)` ms (bucket 0: under 1 ms), the load latencies' own.
pub fn bucket(wait: Micros) -> usize {
    let ms = wait.0 / 1_000;
    let b = 64 - ms.leading_zeros() as usize;
    b.min(LATENCY_BUCKETS - 1)
}

/// A wait the meter follows until its tile is drawn as itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OpenWait {
    /// When the need first wanted the tile drawn.
    since: Micros,
    class: Class,
    level: u32,
    /// [`kind`].
    kind: u8,
    /// For a reload: the eviction's reason, when the step named one.
    reason: Option<Reason>,
}

/// The waits, totalled since the meter's start: what
/// [`crate::BudgetSignals`] publishes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaitTotals {
    /// Waits ended, by class, level and kind.
    pub count: PerClass<[[u64; 3]; LEVELS]>,
    /// Their durations, ms, by kind ([`bucket`]).
    pub millis: [[u64; LATENCY_BUCKETS]; 3],
    /// Reloads by the reason of the eviction they undo, in
    /// [`crate::score::RELEASE_REASONS`]' order, then the rest (an epoch's
    /// recycling, or an eviction the step scored no reason for).
    pub reload_reasons: [u64; 4],
}

impl Default for WaitTotals {
    fn default() -> Self {
        WaitTotals {
            count: PerClass::splat([[0; 3]; LEVELS]),
            millis: [[0; LATENCY_BUCKETS]; 3],
            reload_reasons: [0; 4],
        }
    }
}

/// The meter's follower of waits. A wait is a tile the need wants drawn
/// that is not drawn as itself (`Role::Exact`) — shown by an ancestor or
/// not at all — from the step it is first wanted so to the step it is; a
/// tile the need stops wanting first ends no wait. Each is classified when
/// it opens, by the time since its mesh's last eviction
/// (`Decisions::evict`, the reason from the step's `Decisions::scores`).
/// Observes the decisions; decides nothing, and costs `decide` nothing.
/// Its maps change only when a wait opens or ends or a mesh is evicted: a
/// still step allocates nothing.
#[derive(Clone, Debug, Default)]
pub(crate) struct WaitTracker {
    evicted: BTreeMap<TileId, (Micros, Option<Reason>)>,
    evicted_order: VecDeque<(Micros, TileId)>,
    open: BTreeMap<TileId, OpenWait>,
    /// The tiles wanted this step, sorted: a buffer kept across steps.
    wanted: Vec<TileId>,
}

impl WaitTracker {
    /// Follows a step: its evictions remembered, the waits its draw ended
    /// counted into `totals` (and their number by kind into `recent`), the
    /// waits its need opened. `drawn` is the step's draw, (tile, role),
    /// sorted by tile.
    pub(crate) fn step(
        &mut self,
        now: Micros,
        need: &NeedSet,
        d: &Decisions,
        drawn: &[(TileId, Role)],
        totals: &mut WaitTotals,
        recent: &mut [u64; 3],
    ) {
        for r in &d.evict {
            if let ResourceId::Mesh(t) = r {
                let why = d
                    .scores
                    .iter()
                    .find(|e| e.resource == *r && e.side == Side::Evict)
                    .map(|e| e.reason);
                self.evicted.insert(*t, (now, why));
                self.evicted_order.push_back((now, *t));
            }
        }
        while let Some(&(t, tile)) = self.evicted_order.front() {
            if now.since(t) < RELOAD_MEMORY {
                break;
            }
            self.evicted_order.pop_front();
            if self.evicted.get(&tile).is_some_and(|(at, _)| *at == t) {
                self.evicted.remove(&tile);
            }
        }
        let exact = |t: &TileId| {
            drawn
                .binary_search_by(|e| e.0.cmp(t))
                .is_ok_and(|i| drawn[i].1 == Role::Exact)
        };
        self.wanted.clear();
        for (t, n) in &need.drawn {
            self.wanted.push(*t);
            if exact(t) {
                if let Some(w) = self.open.remove(t) {
                    let k = usize::from(w.kind).min(2);
                    totals.count[w.class][level_index(w.level)][k] += 1;
                    totals.millis[k][bucket(now.since(w.since))] += 1;
                    recent[k] += 1;
                    if k > 0 {
                        let i = w.reason.and_then(crate::score::release_reason).unwrap_or(3);
                        totals.reload_reasons[i] += 1;
                    }
                }
            } else if !self.open.contains_key(t) {
                let evicted = self.evicted.get(t).copied();
                let since = evicted.map(|(at, _)| now.since(at));
                self.open.insert(
                    *t,
                    OpenWait {
                        since: now,
                        class: n.class,
                        level: n.level,
                        kind: u8::try_from(kind(since)).unwrap_or(0),
                        reason: evicted.and_then(|(_, why)| why),
                    },
                );
            }
        }
        if !self.open.is_empty() {
            self.wanted.sort_unstable();
            let wanted = &self.wanted;
            self.open.retain(|t, _| wanted.binary_search(t).is_ok());
        }
    }
}

/// The `q`th percentile, ms, of a wait histogram (each wait at the lower
/// bound of its bucket); `None` when empty.
pub fn percentile_ms(h: &[u64; LATENCY_BUCKETS], q: u64) -> Option<u64> {
    let total: u64 = h.iter().sum();
    if total == 0 {
        return None;
    }
    let rank = (total * q).div_ceil(100).max(1);
    let mut seen = 0;
    for (i, n) in h.iter().enumerate() {
        seen += n;
        if seen >= rank {
            return Some(if i == 0 { 0 } else { 1u64 << (i - 1) });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_and_buckets() {
        assert_eq!(kind(None), 0);
        assert_eq!(kind(Some(Micros::from_millis(4_999))), 1);
        assert_eq!(kind(Some(Micros::from_secs(5))), 2);
        assert_eq!(kind(Some(Micros::from_secs(30))), 0);
        assert_eq!(bucket(Micros::from_millis(0)), 0);
        assert_eq!(bucket(Micros::from_millis(150)), 8);
        let mut h = [0u64; LATENCY_BUCKETS];
        h[8] = 9;
        h[10] = 1;
        assert_eq!(percentile_ms(&h, 50), Some(128));
        assert_eq!(percentile_ms(&h, 95), Some(512));
    }
}
