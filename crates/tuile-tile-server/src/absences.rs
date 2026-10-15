// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Where a store keeps what its sources said they do not have.
//!
//! An absence is a tile id and nothing else: no bytes, no image. By default
//! a store keeps them as tiles of the layer's absence sibling
//! ([`crate::catalog::ABSENT_SUFFIX`]) — archives, zones and manifests of
//! one-byte markers. A host may keep them elsewhere instead
//! ([`TileStore::with_absences`](crate::TileStore::with_absences)): every
//! write to an absence sibling is then [`Absences::record`]ed, every read of
//! one is [`Absences::absent`], and the bucket holds none.
//!
//! **Between a copy and an absence, the newer wins**, by when each was
//! fetched: a tile said absent and later delivered is present, and a tile
//! present and later said absent is absent. An absence store answers
//! [`Absences::absent`] true only for an absence newer than any copy the
//! store holds; a store with one asks it **first**.
//!
//! An absence expires with its layer's expiry, counted from when it was
//! fetched; an expired one is not absent.

use std::time::SystemTime;

use async_trait::async_trait;

/// One absence: a tile id of a layer, when its source said it has no such
/// tile, and when that stops being believed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Absence {
    pub id: u64,
    /// Milliseconds since the Unix epoch.
    pub fetched_ms: u64,
    /// `None`: never.
    pub expires_at: Option<SystemTime>,
}

#[async_trait]
pub trait Absences: Send + Sync {
    /// The source said it has none of `absences`, tiles of `layer` (the
    /// base layer, never its absence sibling). An absence older than a copy
    /// already held changes nothing.
    async fn record(&self, layer: &str, absences: &[Absence]) -> Result<(), String>;
    /// Whether tile `id` of `layer` is absent at `now`: said absent after any
    /// copy held was fetched, and not expired.
    async fn absent(&self, layer: &str, id: u64, now: SystemTime) -> Result<bool, String>;
}
