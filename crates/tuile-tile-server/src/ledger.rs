// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What a store tells a register of its archives, as it writes, publishes and
//! deletes them — and what a register may tell a reader back.
//!
//! A host may keep, beside the bucket, a register of every zone and every
//! archive: what is where, which copy of each tile a reader gets, and when
//! each zone next falls due (see [`crate::due`](fn@crate::due)). The bucket
//! and the register share no transaction, so the order in which the register
//! is told is what keeps it right:
//!
//! 1. [`Ledger::writing`], **before** an archive is uploaded. If the register
//!    cannot take it, the archive is not written: nothing is ever in the
//!    bucket that the register did not hear of first. An archive told and
//!    never published — a lost race, a process killed — is exactly what the
//!    register holds as written and never named by a manifest: an orphan,
//!    found by asking it, not by listing the bucket.
//! 2. [`Ledger::published`], once a manifest is published: the zone's live
//!    and retired archives are exactly the manifest's, the archive the
//!    publication brought in comes with its tiles ([`Publication::added`]),
//!    and the archives it took out of the zone are named
//!    ([`Publication::replaced`]). A register ignores what a publication
//!    older than one it has seen (by `generation`) says, so a replay is
//!    harmless.
//! 3. [`Ledger::deleting`], **before** objects are deleted. If the register
//!    cannot let go of them — it would still point a reader at one — they
//!    are not deleted, and the next upkeep tries again.
//! 4. [`Ledger::deleted`], once objects are gone.
//!
//! The store holds no database: a register is the host's.
//!
//! # Reading it back
//!
//! A register that keeps, for each tile, the copy a reader would get can
//! answer a projection by index without the bucket being listed or a single
//! archive index read: see [`IndexReader`] and
//! [`TileStore::with_index_reader`](crate::TileStore::with_index_reader).

use std::ops::RangeInclusive;

use async_trait::async_trait;
use futures_util::stream::BoxStream;

use crate::due::Due;
use crate::index::{ArchiveIndex, Stamp};
use crate::manifest::Manifest;
use crate::Zone;

/// One publication of a zone's manifest, as a register is told of it.
#[derive(Debug, Clone, Copy)]
pub struct Publication<'a> {
    pub layer: &'a str,
    pub zone: Zone,
    /// The manifest as published: its `generation` orders publications.
    pub manifest: &'a Manifest,
    /// When the zone next has something to be done.
    pub due: &'a Due,
    /// The archive this publication brought into the zone, with every tile
    /// it holds and the stamp of each — when it brought one, and the layer
    /// keeps indexes ([`Layer::index`](crate::Layer::index)). Its key is
    /// `added.archive`; where it sits in `manifest.archives` says which
    /// archives win a tile over it: those after it.
    pub added: Option<&'a ArchiveIndex>,
    /// The archives this publication took out of the zone's live list:
    /// merged, scrubbed, expired. They are in `manifest.retired`.
    pub replaced: &'a [String],
    /// Whether a tile of a replaced archive that `added` does not hold may
    /// now be read from an older archive of the zone — a copy no register
    /// was told wins. Only a scrub does that: a merge's archive holds every
    /// tile of its run, and an expired archive's older ones are expired
    /// with it. A register that drops such a tile no longer knows the zone
    /// whole.
    pub uncovering: bool,
}

#[async_trait]
pub trait Ledger: Send + Sync {
    /// The archive `key` of `layer`'s `zone` is about to be uploaded. An
    /// error, and it is not.
    async fn writing(&self, layer: &str, zone: Zone, key: &str) -> Result<(), String>;
    /// A manifest was published. An error is said and counted, and is not
    /// the publication's: the bucket is right already.
    async fn published(&self, publication: &Publication<'_>) -> Result<(), String>;
    /// `keys`, archives of `layer`'s `zone`, are about to be deleted. An
    /// error, and none of them is.
    async fn deleting(&self, _layer: &str, _zone: Zone, _keys: &[String]) -> Result<(), String> {
        Ok(())
    }
    /// `keys`, archives of `layer`'s `zone`, were deleted.
    async fn deleted(&self, layer: &str, zone: Zone, keys: &[String]) -> Result<(), String>;
}

/// A zone a register knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnownZone {
    pub zone: Zone,
    /// Whether the register holds, for every tile of the zone, the copy a
    /// reader gets. A zone it does not (a publication it missed, a zone not
    /// rebuilt yet) is read by its archives' indexes, as without a register.
    pub indexed: bool,
}

/// One tile as a register gives it back: its id, the stamp of the copy a
/// reader gets, and the epoch of the archive that holds that copy — an
/// expired one is not read, as the store would not read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredTile {
    pub id: u64,
    pub stamp: Stamp,
    pub epoch: String,
}

/// Batches of tiles, as a register streams them.
pub type TileBatches<'a> = BoxStream<'a, Result<Vec<RegisteredTile>, String>>;

/// A register read back: which zones a layer has, and the copy a reader gets
/// of each tile — what a projection by index otherwise learns by listing the
/// bucket and reading every archive's index.
#[async_trait]
pub trait IndexReader: Send + Sync {
    /// Every zone of `layer` the register knows; `None` when it does not
    /// know the layer whole, and the bucket is listed instead.
    async fn zones(&self, layer: &str) -> Result<Option<Vec<KnownZone>>, String>;
    /// The tiles of `layer` whose ids fall within `ranges` (sorted,
    /// disjoint), in batches as they are read: never all at once.
    fn tiles<'a>(&'a self, layer: &'a str, ranges: &'a [RangeInclusive<u64>]) -> TileBatches<'a>;
}
