// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Request priority: an integer lexicographic key, total and deterministic.
//!
//! 1. the urgency tier;
//! 2. the score (higher first): SSE excess weighted by centrality, plus an
//!    age bonus against starvation and a cover bonus for a coarse tile that
//!    many waiting tiles fall back on;
//! 3. the residency tier (cheaper first: it closes a hole sooner);
//! 4. the resource id.
//!
//! No float is compared: [`Need`](super::Need)'s floats are quantised once.

use std::cmp::Reverse;

use super::need::Need;
use super::tier::Tier;
use super::types::{Micros, ResourceId};

/// The urgency tiers, most urgent first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Urgency {
    /// The pinned coarse floor: global coverage.
    Floor,
    /// A tile wanted on screen that is not drawable: it shows a fallback
    /// (coarser ancestor, or finer descendants kept on a zoom-out). Its
    /// absence forces either blur or over-detail.
    Hole,
    /// A child a held parent waits on before it can refine.
    Blocker,
    /// The drape a drawn tile needs, when the one it wears is not it.
    Drape,
    /// The prefetch ring.
    Ring,
    /// A preloaded ancestor.
    Ancestor,
    /// Resident content whose upstream epoch changed: refreshed with free
    /// slots and budget only, never preempting, new before old.
    Refresh,
}

/// A need's floats, quantised once at the boundary.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Quantised {
    /// SSE excess in thousandths, clamped to ±16 000.
    pub sse_excess: i32,
    /// Centrality in thousandths, `0..=1000`.
    pub centrality: i32,
    /// Distance in whole metres.
    pub distance: u32,
    /// Whether any input float was not finite (it then counts as zero).
    pub rejected: bool,
}

impl Quantised {
    /// Quantises a need's floats.
    pub fn of(need: &Need) -> Self {
        let _ = need;
        Quantised::default()
    }
}

/// The score within an urgency tier: higher is more urgent.
///
/// `sse_excess × (0.5 + 0.5 × centrality) + age_bonus + cover_bonus`, in
/// integers: the age bonus grows linearly to 1000 over `age_full`, the cover
/// bonus is 250 per waiting descendant, capped at 4000.
pub fn score(q: Quantised, waited: Micros, age_full: Micros, waiting_descendants: u32) -> i64 {
    let _ = (q, waited, age_full, waiting_descendants);
    0
}

/// The full priority key; smaller sorts first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PriorityKey {
    /// The urgency tier.
    pub urgency: Urgency,
    /// The score, reversed so that higher sorts first.
    pub score: Reverse<i64>,
    /// The residency tier, cheaper first.
    pub tier: Tier,
    /// The final tie-break.
    pub resource: ResourceId,
}
