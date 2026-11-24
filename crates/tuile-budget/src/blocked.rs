// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Why a tile under its need is not drawn at its need yet: one reason per
//! tile, read off the state of the resource it waits on — its mesh, for a
//! wanted tile drawn by an ancestor; the drape of its needed level, for a
//! tile drawn wearing a coarser drape. The same tiles the per-level
//! `under_need` counts, each given exactly one [`Blocked`], so the reasons
//! of a level sum back to its `under_need`.
//!
//! Read after the step has decided, as its statistics are: the reason is
//! what the step left the resource in (a load started this step is in
//! flight, not queued). The step's own time dates a block, never a clock:
//! a tile blocked since a step keeps that step's time while it stays under
//! its need, whatever its reason becomes, and the oldest of each reason is
//! published as an age.

/// Why a tile under its need waits, in [`REASONS`]' order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Blocked {
    /// Nothing asks for it: not among the step's wants.
    NotRequested,
    /// A drape worn one level under its need: its own level is not wanted
    /// until the quality it brings pays for its restore cost (the deficit's
    /// dwell, spec §5.3).
    Deferred,
    /// Wanted and ranked, not started: no load slot, the decoded memory
    /// full, or behind more urgent work.
    Queued,
    /// Wanted, its origin down: it waits for the origin's probe.
    OriginDown,
    /// Its last load failed: it waits out its backoff.
    Backoff,
    /// Its load reads the local store.
    FetchingStore,
    /// Its load is on the network.
    FetchingNetwork,
    /// Decoded, not handed to the consumer yet: it waits for a delivery
    /// (the upload plan's room, a discard's backoff).
    Decoded,
    /// Handed to the consumer, not acknowledged: its upload, or its GPU
    /// encode, deferred by the frame's lanes or under way.
    Uploading,
    /// Drawable, still not drawn as itself: its swap waits (the siblings
    /// that replace their parent together, the drapes it must wear).
    Swap,
}

/// The number of [`Blocked`] reasons.
pub const BLOCKED: usize = 10;

/// Every reason, in its index's order.
pub const REASONS: [Blocked; BLOCKED] = [
    Blocked::NotRequested,
    Blocked::Deferred,
    Blocked::Queued,
    Blocked::OriginDown,
    Blocked::Backoff,
    Blocked::FetchingStore,
    Blocked::FetchingNetwork,
    Blocked::Decoded,
    Blocked::Uploading,
    Blocked::Swap,
];

/// The `reason` label of each, in [`REASONS`]' order.
pub const BLOCKED_LABELS: [&str; BLOCKED] = [
    "not_requested",
    "deferred",
    "queued",
    "origin_down",
    "backoff",
    "fetching_store",
    "fetching_network",
    "decoded",
    "uploading",
    "swap",
];

impl Blocked {
    /// Its index in [`REASONS`].
    pub const fn index(self) -> usize {
        self as usize
    }

    /// Its `reason` label.
    pub const fn label(self) -> &'static str {
        BLOCKED_LABELS[self as usize]
    }

    /// What it means, in a few words a person reads.
    pub const fn words(self) -> &'static str {
        match self {
            Blocked::NotRequested => "not requested",
            Blocked::Deferred => "one level coarse, deferred until worth its cost",
            Blocked::Queued => "queued, not started",
            Blocked::OriginDown => "origin down, waiting for its probe",
            Blocked::Backoff => "failed, waiting out its backoff",
            Blocked::FetchingStore => "reading the local store",
            Blocked::FetchingNetwork => "fetching from the network",
            Blocked::Decoded => "decoded, waiting for delivery",
            Blocked::Uploading => "uploading or encoding on the GPU",
            Blocked::Swap => "drawable, its swap waits",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reasons_are_in_their_index_order() {
        for (i, r) in REASONS.iter().enumerate() {
            assert_eq!(r.index(), i);
            assert_eq!(r.label(), BLOCKED_LABELS[i]);
        }
    }
}
