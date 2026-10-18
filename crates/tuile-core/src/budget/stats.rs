// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! [`BudgetStats`]: one step's numbers, the raw material of the
//! `BudgetObserver` (JSONL and `/metrics`) and of the device audits: bytes per
//! class against the target, cancel/complete ratio, latency histogram,
//! duplicates in flight, holes, underdraped tiles, convergence.

use super::types::{Bytes, PerClass};

/// Buckets of the load-latency histogram: bucket `i` counts loads that took
/// `[2^(i-1), 2^i)` milliseconds (bucket 0: under 1 ms; the last: the rest).
pub const LATENCY_BUCKETS: usize = 18;

/// Span of the imagery-over-terrain histogram: index `gap + 8`, clamped.
pub const GAP_BUCKETS: usize = 17;

/// One step's statistics. Counters count this step only.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct BudgetStats {
    /// GPU bytes per class (drawable plus in transit).
    pub gpu: PerClass<Bytes>,
    /// The protected part of `gpu`.
    pub protected: PerClass<Bytes>,
    /// The effective target per class, after pressure.
    pub target: PerClass<Bytes>,
    /// The effective overflow per class, after pressure.
    pub overflow: PerClass<Bytes>,
    /// Decoded CPU bytes held.
    pub decoded: Bytes,
    /// Bytes delivered and not yet acknowledged.
    pub in_transit: Bytes,
    /// The LOD relaxation, thousandths.
    pub sse_scale_milli: u32,
    /// Loads started over the network.
    pub started_network: u32,
    /// Loads started from a local store.
    pub started_local: u32,
    /// Loads cancelled.
    pub cancelled: u32,
    /// Loads completed (fresh generation).
    pub completed: u32,
    /// Results discarded because their generation was stale.
    pub stale: u32,
    /// Resources delivered.
    pub delivered: u32,
    /// GPU copies evicted.
    pub evicted: u32,
    /// Decoded copies released.
    pub released: u32,
    /// Loads failed.
    pub failed: u32,
    /// Local reads that missed after a positive presence hint.
    pub store_misses: u32,
    /// Network bytes avoided by local tiers this step.
    pub network_bytes_avoided: Bytes,
    /// Resources fetched again from the network within 10 s of their
    /// eviction (what I14 forbids in spirit).
    pub thrash: u32,
    /// Resources read again from a local store within 10 s of their
    /// eviction: the eviction demoted them to the Stored tier.
    pub rereads: u32,
    /// Entries of the bounded wish list.
    pub heap_len: u32,
    /// Network loads in flight per class.
    pub in_flight: PerClass<u32>,
    /// Local reads in flight.
    pub local_reads: u32,
    /// Resources with more than one load in flight (must be 0).
    pub duplicates_in_flight: u32,
    /// Resources the budget tracks (bounded state).
    pub tracked: u32,
    /// Wanted tiles drawn as themselves.
    pub exact: u32,
    /// Finer meshes kept for a wanted coarser tile.
    pub held: u32,
    /// Wanted tiles standing on an ancestor.
    pub fallback: u32,
    /// Wanted tiles with nothing to draw (must be 0 once the floor is in).
    pub lost: u32,
    /// Drawn tiles whose drape is more than one level from their need.
    pub underdraped: u32,
    /// Drawn tiles per imagery-minus-terrain level gap (`gap + 8`).
    pub imagery_over_terrain: [u32; GAP_BUCKETS],
    /// Load latencies of the loads completed this step.
    pub latency_ms: [u32; LATENCY_BUCKETS],
    /// Need entries with a non-finite float.
    pub rejected_inputs: u32,
    /// Every wanted tile drawn exactly with its needed drape, and no work
    /// left for the view.
    pub converged: bool,
}

impl BudgetStats {
    /// A digest of every field, FNV-1a, the same on every platform: a device
    /// run's per-step statistics compare to the simulation's.
    pub fn digest(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut put = |v: u64| {
            for b in v.to_le_bytes() {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
        };
        for per in [&self.gpu, &self.protected, &self.target, &self.overflow] {
            for (_, b) in per.iter() {
                put(b.0);
            }
        }
        for v in [
            self.decoded.0,
            self.in_transit.0,
            u64::from(self.sse_scale_milli),
            u64::from(self.started_network),
            u64::from(self.started_local),
            u64::from(self.cancelled),
            u64::from(self.completed),
            u64::from(self.stale),
            u64::from(self.delivered),
            u64::from(self.evicted),
            u64::from(self.released),
            u64::from(self.failed),
            u64::from(self.store_misses),
            self.network_bytes_avoided.0,
            u64::from(self.thrash),
            u64::from(self.rereads),
            u64::from(self.heap_len),
            u64::from(self.local_reads),
            u64::from(self.duplicates_in_flight),
            u64::from(self.tracked),
            u64::from(self.exact),
            u64::from(self.held),
            u64::from(self.fallback),
            u64::from(self.lost),
            u64::from(self.underdraped),
            u64::from(self.rejected_inputs),
            u64::from(self.converged),
        ] {
            put(v);
        }
        for (_, n) in self.in_flight.iter() {
            put(u64::from(n));
        }
        for n in self.imagery_over_terrain.iter().chain(&self.latency_ms) {
            put(u64::from(*n));
        }
        h
    }
}
