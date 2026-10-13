// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What a store tells a register of its archives, as it writes, publishes and
//! deletes them.
//!
//! A host may keep, beside the bucket, a register of every zone and every
//! archive: what is where, and when each zone next falls due (see
//! [`crate::due`](fn@crate::due)). The bucket and the register share no transaction, so the
//! order in which the register is told is what keeps it right:
//!
//! 1. [`Ledger::writing`], **before** an archive is uploaded. If the register
//!    cannot take it, the archive is not written: nothing is ever in the
//!    bucket that the register did not hear of first. An archive told and
//!    never published — a lost race, a process killed — is exactly what the
//!    register holds as written and never named by a manifest: an orphan,
//!    found by asking it, not by listing the bucket.
//! 2. [`Ledger::published`], once a manifest is published: the zone's live
//!    and retired archives are exactly the manifest's, and what falls due is
//!    the given [`Due`]. A register that missed one publication is put right
//!    by the next, which carries the whole manifest; it ignores one older
//!    than what it has seen (by `generation`).
//! 3. [`Ledger::deleted`], once objects are gone.
//!
//! The store holds no database: a register is the host's.

use async_trait::async_trait;

use crate::due::Due;
use crate::manifest::Manifest;
use crate::Zone;

#[async_trait]
pub trait Ledger: Send + Sync {
    /// The archive `key` of `layer`'s `zone` is about to be uploaded. An
    /// error, and it is not.
    async fn writing(&self, layer: &str, zone: Zone, key: &str) -> Result<(), String>;
    /// `manifest` was published for `layer`'s `zone`, and `due` is when the
    /// zone next has something to be done.
    async fn published(
        &self,
        layer: &str,
        zone: Zone,
        manifest: &Manifest,
        due: &Due,
    ) -> Result<(), String>;
    /// `keys`, archives of `layer`'s `zone`, were deleted.
    async fn deleted(&self, layer: &str, zone: Zone, keys: &[String]) -> Result<(), String>;
}
