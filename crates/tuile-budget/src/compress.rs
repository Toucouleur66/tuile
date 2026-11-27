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
//! 2. **It survives the decisions with a very high score.** A GPU copy is
//!    compressed once its probability of use — the progressive score's `P`
//!    — has stayed at least [`Compress::probability_permille`] (90 % by
//!    default) over [`Compress::decisions`] decisions (10), as the pass
//!    samples it at its visits; a dip starts the count again. A decision
//!    is a camera pose — one camera handed to the decider — never time
//!    (Laurent, 2026-11-24): it follows how fast the view changes, and in
//!    a film a decision is a frame. At rest, a copy whose score is that
//!    high is also compressed once the need has been unchanged for
//!    [`Compress::still_poses`] consecutive poses (60). A fresh copy, or
//!    one whose score dipped, stays RGBA8 and leaves as such.
//! 3. **Highest value first.** Among those, the copies the release order
//!    keeps longest go first (the eviction key, highest first): what is due
//!    for release soonest is never encoded before it.
//! 4. **Compression before eviction.** When a class is over its low
//!    watermark, both waits are waived — never the score — and the bytes
//!    the compressions in
//!    flight will give back count as given back: a sweep evicts only what
//!    compression cannot free.
//! 5. **Charged as held.** A copy is charged RGBA8 until compressed; from
//!    the decision until the consumer acknowledges the compressed copy it
//!    is charged both (the new copy is made before the old one goes); then
//!    the compressed bytes the consumer reports.
//! 6. **Spare time only.** The compressions in flight are capped
//!    ([`Compress::flight_tiles`], scaled with the thermal state as the
//!    encode lane is) and by the hard limit; the consumer runs them in its
//!    frames' measured headroom, never before an upload or a draw.
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
use super::types::{Bytes, Class, Generation, PerClass, ResourceId};
use super::value::CERTAIN;

/// Whether a probability of use `probability` ([`CERTAIN`] units) is as
/// high as law 2 asks.
pub fn high(p: &Compress, probability: u32) -> bool {
    let p_min = u64::from(p.probability_permille) * u64::from(CERTAIN / 1_000);
    u64::from(probability) >= p_min
}

/// Law 2's count, sampled: the decision from which the copy's score has
/// stayed high, given what it was (`high_from`) and its probability at
/// decision `step` — kept while high, started at `step` when it rises,
/// cleared when it dips.
pub fn track(p: &Compress, probability: u32, high_from: Option<u64>, step: u64) -> Option<u64> {
    if high(p, probability) {
        Some(high_from.unwrap_or(step))
    } else {
        None
    }
}

/// What law 2 reads of a copy and of the step: its probability of use, the
/// decision its score has stayed high from, the step, and for how many
/// consecutive poses the need has been unchanged.
#[derive(Clone, Copy, Debug)]
pub struct Survival {
    /// Its probability of use now, [`CERTAIN`] units.
    pub probability: u32,
    /// The decision its score has stayed high from ([`track`]).
    pub high_from: Option<u64>,
    /// This decision.
    pub step: u64,
    /// The consecutive camera poses, this one included, at which the need
    /// has not changed: 0 when it changed at this one.
    pub still_for: u64,
}

/// Law 2: whether a copy is compressed — `pressed`, when its class is over
/// its low watermark (law 4): then both waits are waived, never the score.
pub fn eligible(p: &Compress, s: Survival, pressed: bool) -> bool {
    let survived = s
        .high_from
        .is_some_and(|from| s.step.saturating_sub(from) >= u64::from(p.decisions));
    high(p, s.probability) && (pressed || survived || s.still_for >= u64::from(p.still_poses))
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
    /// The decision from which its score has stayed high, as sampled (law
    /// 2): `None` fresh, or after a dip.
    pub high_from: Option<u64>,
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
    /// The first of the consecutive camera poses at which the need has not
    /// changed: law 2 at rest.
    pub still_since: Option<u64>,
}

impl Compressions {
    /// A GPU copy of `gen` that may be compressed into `to` by writing
    /// `blocks` blocks: waiting from now on, its count not begun, whatever
    /// it was (a copy delivered again starts over).
    pub fn admit(&mut self, r: ResourceId, gen: Generation, to: Bytes, blocks: u64) {
        self.flights.remove(&r);
        self.waiting.insert(
            r,
            Waiting {
                gen,
                high_from: None,
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
            out.extend(self.waiting.range(..=c).map(|(r, _)| *r).take(left));
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

    fn at(probability: u32, high_from: Option<u64>, step: u64, still_for: u64) -> Survival {
        Survival {
            probability,
            high_from,
            step,
            still_for,
        }
    }

    #[test]
    fn a_score_under_the_threshold_stays_rgba8_whatever_it_survives() {
        let low = u32::from(COMPRESS.probability_permille) * (CERTAIN / 1_000) - 1;
        let long = u64::MAX;
        assert!(!eligible(
            &COMPRESS,
            at(low, Some(0), 1_000_000, long),
            false
        ));
        assert!(!eligible(
            &COMPRESS,
            at(low, Some(0), 1_000_000, long),
            true
        ));
        assert_eq!(track(&COMPRESS, low, Some(0), 99), None);
    }

    #[test]
    fn a_very_high_score_is_compressed_once_it_survives_its_decisions() {
        let n = u64::from(COMPRESS.decisions);
        let from = track(&COMPRESS, CERTAIN, None, 100);
        assert_eq!(from, Some(100));
        assert!(!eligible(
            &COMPRESS,
            at(CERTAIN, from, 100 + n - 1, 0),
            false
        ));
        assert!(eligible(&COMPRESS, at(CERTAIN, from, 100 + n, 0), false));
        // A dip starts the count again.
        let dipped = track(&COMPRESS, CERTAIN / 2, from, 105);
        assert_eq!(dipped, None);
        let again = track(&COMPRESS, CERTAIN, dipped, 106);
        assert!(!eligible(&COMPRESS, at(CERTAIN, again, 100 + n, 0), false));
        assert!(eligible(&COMPRESS, at(CERTAIN, again, 106 + n, 0), false));
    }

    /// N − 1 still poses do not compress; N do (Laurent, 2026-11-24).
    #[test]
    fn at_rest_a_still_need_stands_for_the_decisions() {
        let still = u64::from(COMPRESS.still_poses);
        assert!(!eligible(
            &COMPRESS,
            at(CERTAIN, Some(0), 1, still - 1),
            false
        ));
        assert!(eligible(&COMPRESS, at(CERTAIN, Some(0), 1, still), false));
        assert!(!eligible(
            &COMPRESS,
            at(CERTAIN / 2, Some(0), 1, still),
            false
        ));
    }

    #[test]
    fn under_pressure_the_waits_are_waived_not_the_score() {
        assert!(eligible(&COMPRESS, at(CERTAIN, None, 0, 0), true));
        assert!(!eligible(&COMPRESS, at(CERTAIN / 2, None, 0, 0), true));
    }

    #[test]
    fn the_copy_kept_longest_goes_first() {
        let mut v = [
            (1u64, drape(1)),
            (9, drape(2)),
            (9, drape(0)),
            (5, drape(3)),
        ];
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
            c.admit(drape(n), Generation(1), Bytes(1), 1);
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
        c.admit(drape(1), Generation(1), Bytes(1), 1);
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
        c.admit(drape(1), Generation(2), Bytes(1), 1);
        assert!(c.flights.is_empty());
        assert_eq!(c.waiting[&drape(1)].high_from, None);
        assert_eq!(c.saving()[Class::Drape], Bytes::ZERO);
    }
}
