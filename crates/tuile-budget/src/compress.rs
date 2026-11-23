// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Display at once, compress what lasts (CB9).
//!
//! A texture the consumer's GPU can compress (BC7, ASTC 4×4) is uploaded
//! and drawn as RGBA8 at once; whether it is ever compressed is this
//! module's decision, taken later, on what is already resident. It is the
//! behaviour of a compressed-memory pool (zswap, zram, the macOS memory
//! compressor), with one difference: a GPU samples BC7 and ASTC directly,
//! so a compressed texture costs nothing to read where a compressed page
//! must be decompressed first.
//!
//! The laws (`docs/explorations/compress-what-lasts.md`):
//!
//! 1. **Upload first.** A delivery never waits for an encode: it crosses,
//!    is held and is charged as RGBA8 until compressed.
//! 2. **Compress what lasts.** A GPU copy is compressed when its
//!    probability of use (the progressive score's `P`) is at least
//!    [`Compress::probability_permille`] **and** it has been drawable for
//!    [`Compress::after`] — its expected residency, read off its age: a copy
//!    that has lasted is likely to last. A hot, freshly needed copy, or one
//!    the score already gives up, stays RGBA8 and leaves as such.
//! 3. **Highest value first.** Among those, the copies the release order
//!    keeps longest go first (the eviction key, highest first): what is due
//!    for release soonest is never encoded before it.
//! 4. **Compression before eviction.** When a class is over its high
//!    watermark, the age is waived, and the bytes the compressions in
//!    flight will give back count as given back: a sweep evicts only what
//!    compression cannot free.
//! 5. **Charged as held.** A copy is charged RGBA8 until compressed; from
//!    the decision until the consumer acknowledges the compressed copy it
//!    is charged both (the new copy is made before the old one goes); then
//!    the compressed bytes the consumer reports.
//! 6. **Spare time only.** The compressions in flight are capped by the
//!    step's encode lane (`BudgetConfig::encode_blocks_per_step`) and by
//!    the hard limit; the consumer runs them in its frames' measured
//!    headroom, never before an upload or a draw.
//!
//! The numbers are the specification's ([`Compress`]): re-specifiable at
//! run time, recorded, never a switch. Without a compressible copy nothing
//! here runs, and every decision is the one it was.
//!
//! The policy ([`eligible`], [`ranks_before`]) proposes; the engine's
//! mechanism (`Budget::compress_pass`) checks what it proposes against the
//! ledger — still on the GPU, of the same generation, not in transit, the
//! transient under the hard limit, the lane not spent — and decides.

use std::collections::BTreeMap;

use super::spec::Compress;
use super::types::{Bytes, Class, Generation, Micros, PerClass, ResourceId};
use super::value::CERTAIN;

/// Law 2: whether a copy of probability of use `probability` ([`CERTAIN`]
/// units), drawable for `age`, is compressed — `pressed`, when its class
/// is over its high watermark (law 4).
pub fn eligible(p: &Compress, probability: u32, age: Micros, pressed: bool) -> bool {
    let p_min = u64::from(p.probability_permille) * u64::from(CERTAIN / 1_000);
    u64::from(probability) >= p_min && (pressed || age >= p.after)
}

/// Law 3: `a` goes before `b`, given their eviction keys: the copy the
/// release order keeps longer first, ties by resource.
pub fn ranks_before<K: Ord>(a: (&K, ResourceId), b: (&K, ResourceId)) -> std::cmp::Ordering {
    b.0.cmp(a.0).then(a.1.cmp(&b.1))
}

/// A GPU copy that may be compressed, not yet asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Waiting {
    /// The generation of the GPU copy it is of.
    pub gen: Generation,
    /// When it became drawable.
    pub since: Micros,
    /// What it holds once compressed, and the blocks that writes.
    pub to: Bytes,
    pub blocks: u64,
}

/// A compression asked of the consumer, not yet acknowledged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Flight {
    pub gen: Generation,
    pub class: Class,
    /// The RGBA8 bytes it held: what its acknowledgement gives back.
    pub from: Bytes,
    /// What it is expected to hold once compressed.
    pub to: Bytes,
    pub blocks: u64,
}

/// The copies that may be compressed, and the compressions in flight. Empty
/// without compression: then nothing reads it.
#[derive(Clone, Debug, Default)]
pub(crate) struct Compressions {
    /// Every GPU copy that may be compressed and was not asked to be, by
    /// resource. An entry whose copy left the GPU is dropped when the pass
    /// reaches it.
    pub waiting: BTreeMap<ResourceId, Waiting>,
    /// Where the last pass stopped: the next one starts after it.
    pub cursor: Option<ResourceId>,
    /// The compressions in flight.
    pub flights: BTreeMap<ResourceId, Flight>,
}

impl Compressions {
    /// A GPU copy of `gen`, drawable from `now`, that may be compressed
    /// into `to` by writing `blocks` blocks: waiting from now on, whatever
    /// it was (a copy delivered again starts over).
    pub fn admit(&mut self, r: ResourceId, gen: Generation, now: Micros, to: Bytes, blocks: u64) {
        self.flights.remove(&r);
        self.waiting.insert(
            r,
            Waiting {
                gen,
                since: now,
                to,
                blocks,
            },
        );
    }

    /// The bytes the compressions in flight of each class give back once
    /// acknowledged: law 4's.
    pub fn saving(&self) -> PerClass<Bytes> {
        let mut out = PerClass::splat(Bytes::ZERO);
        for f in self.flights.values() {
            out[f.class] = out[f.class].plus(f.from);
        }
        out
    }

    /// The blocks the compressions in flight write.
    pub fn blocks_in_flight(&self) -> u64 {
        self.flights
            .values()
            .fold(0u64, |a, f| a.saturating_add(f.blocks))
    }

    /// Up to `n` waiting resources from the cursor on, wrapping once, and
    /// the cursor moved past them.
    pub fn visit(&mut self, n: usize, out: &mut Vec<ResourceId>) {
        use std::ops::Bound::{Excluded, Unbounded};
        out.clear();
        let after = match self.cursor {
            Some(c) => (Excluded(c), Unbounded),
            None => (Unbounded, Unbounded),
        };
        out.extend(self.waiting.range(after).map(|(r, _)| *r).take(n));
        if let Some(c) = self.cursor.filter(|_| out.len() < n) {
            let left = n - out.len();
            out.extend(
                self.waiting
                    .range(..=c)
                    .map(|(r, _)| *r)
                    .take(left),
            );
        }
        self.cursor = out.last().copied();
    }
}

/// What the compression did in one step, and what it holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CompressStats {
    /// Copies waiting to be compressed, and the compressions in flight.
    pub waiting: u32,
    pub in_flight: u32,
    /// Compressions decided this step, and acknowledged this step.
    pub started: u32,
    pub done: u32,
    /// Copies visited this step, and of them those eligible.
    pub visited: u32,
    pub eligible: u32,
    /// Eligible but not started: the lane spent, or the transient over the
    /// hard limit.
    pub lane_full: u32,
    pub over_hard: u32,
    /// Acknowledged as not compressed after all (the consumer could not),
    /// and acknowledged off the estimate by more than 5 %.
    pub refused: u32,
    pub drift: u32,
    /// Bytes the compressions in flight give back once acknowledged, per
    /// class.
    pub saving: PerClass<Bytes>,
    /// Bytes given back by the compressions acknowledged this step.
    pub freed: Bytes,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::spec::COMPRESS;
    use crate::TileId;

    fn drape(n: u64) -> ResourceId {
        ResourceId::drape(TileId(n), crate::LayerId(0), 3)
    }

    #[test]
    fn a_low_score_stays_rgba8_whatever_its_age() {
        let low = u32::from(COMPRESS.probability_permille) * (CERTAIN / 1_000) - 1;
        assert!(!eligible(&COMPRESS, low, Micros::from_secs(3_600), false));
        assert!(!eligible(&COMPRESS, low, Micros::from_secs(3_600), true));
    }

    #[test]
    fn a_high_score_is_compressed_once_it_has_lasted() {
        assert!(!eligible(&COMPRESS, CERTAIN, Micros::from_millis(1_999), false));
        assert!(eligible(&COMPRESS, CERTAIN, Micros::from_secs(2), false));
    }

    #[test]
    fn under_pressure_the_age_is_waived() {
        assert!(eligible(&COMPRESS, CERTAIN, Micros::ZERO, true));
    }

    #[test]
    fn the_copy_kept_longest_goes_first() {
        let mut v = vec![(1u64, drape(1)), (9, drape(2)), (9, drape(0)), (5, drape(3))];
        v.sort_by(|a, b| ranks_before((&a.0, a.1), (&b.0, b.1)));
        assert_eq!(
            v.iter().map(|x| x.1).collect::<Vec<_>>(),
            vec![drape(0), drape(2), drape(3), drape(1)]
        );
    }

    #[test]
    fn the_visit_wraps_round_the_waiting_copies() {
        let mut c = Compressions::default();
        for n in 0..5 {
            c.admit(drape(n), Generation(1), Micros::ZERO, Bytes(1), 1);
        }
        let mut out = Vec::new();
        c.visit(3, &mut out);
        assert_eq!(out, vec![drape(0), drape(1), drape(2)]);
        c.visit(3, &mut out);
        assert_eq!(out, vec![drape(3), drape(4), drape(0)]);
        c.visit(9, &mut out);
        assert_eq!(out.len(), 5, "each at most once a visit");
    }

    #[test]
    fn a_copy_delivered_again_starts_over() {
        let mut c = Compressions::default();
        c.admit(drape(1), Generation(1), Micros::ZERO, Bytes(1), 1);
        c.flights.insert(
            drape(1),
            Flight {
                gen: Generation(1),
                class: Class::Drape,
                from: Bytes(4),
                to: Bytes(1),
                blocks: 1,
            },
        );
        c.admit(drape(1), Generation(2), Micros(7), Bytes(1), 1);
        assert!(c.flights.is_empty());
        assert_eq!(c.waiting[&drape(1)].since, Micros(7));
        assert_eq!(c.saving()[Class::Drape], Bytes::ZERO);
    }
}
