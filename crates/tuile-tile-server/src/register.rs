// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A register in memory: the reference of what a register does with what a
//! [`Ledger`] is told, and an [`IndexReader`] for a host that keeps no
//! database (tests, one process that writes and reads its own store).
//!
//! A register keeps, for each tile, **only the copy a reader gets**: its
//! stamp, the archive that holds it, the epoch of that archive and the
//! generation of the publication that put it there. A publication decides it
//! from what it says, and nothing else:
//!
//! - every archive its manifest names is live — unless a newer publication
//!   retired it: an archive's state only moves forward (written, live,
//!   retired, deleted);
//! - each tile of the archive it added, if that archive is still live, wins
//!   unless the copy kept was put there by a **newer** publication, or is in
//!   an archive that comes **after** the added one in the manifest (a merge
//!   puts its archive where its run was; a delta after the run still wins);
//! - then no tile points at an archive it replaced any more;
//! - a publication that is [`uncovering`](Publication::uncovering) and took
//!   a tile out leaves its zone not known whole, until it is rebuilt.
//!
//! Replayed, or told late, a publication changes nothing a newer one
//! decided. Told nothing of an archive's tiles (a publication missed, a
//! layer whose tiles are not indexed), a zone is not known whole either, and
//! a projection reads it from its archives.

use std::collections::{BTreeMap, HashMap};
use std::ops::RangeInclusive;
use std::sync::{Mutex, MutexGuard};

use async_trait::async_trait;

use crate::index::{ArchiveIndex, Stamp};
use crate::ledger::{IndexReader, KnownZone, Ledger, Publication, RegisteredTile, TileBatches};
use crate::manifest::Manifest;
use crate::Zone;

/// Where an archive is in its life. Only ever moves forward.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ArchiveState {
    Writing,
    Live,
    Retired,
    Deleted,
}

/// What a register holds of an archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveEntry {
    pub layer: String,
    pub zone: Zone,
    pub state: ArchiveState,
    /// Whether its tiles were told.
    pub tiles_known: bool,
    pub epoch: String,
}

/// The copy of a tile a reader gets, as a register holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TileEntry {
    pub stamp: Stamp,
    pub archive: String,
    pub generation: u64,
    pub epoch: String,
}

/// What a register holds of a zone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ZoneEntry {
    /// The newest generation of its manifest told.
    pub generation: u64,
    /// A tile was taken out that an older archive may hold.
    pub stale: bool,
}

/// Everything a [`MemoryRegister`] holds.
#[derive(Debug, Default, Clone)]
pub struct Book {
    pub zones: BTreeMap<(String, Zone), ZoneEntry>,
    pub archives: HashMap<String, ArchiveEntry>,
    pub tiles: BTreeMap<(String, u64), TileEntry>,
}

/// A register in memory: see the module.
#[derive(Debug, Default)]
pub struct MemoryRegister {
    book: Mutex<Book>,
    /// Tiles given back per batch.
    batch: usize,
}

impl MemoryRegister {
    pub fn new() -> Self {
        Self {
            book: Mutex::default(),
            batch: 1024,
        }
    }

    /// Gives tiles back `n` at a time.
    pub fn with_batch(mut self, n: usize) -> Self {
        self.batch = n.max(1);
        self
    }

    /// What it holds, now.
    pub fn book(&self) -> Book {
        self.lock().clone()
    }

    fn lock(&self) -> MutexGuard<'_, Book> {
        match self.book.lock() {
            Ok(b) => b,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Puts a zone right from its manifest and the index of every one of its
    /// live archives, oldest first, as a rebuild does: what the publications
    /// would have left, and nothing of what they no longer name.
    pub fn rebuild_zone(
        &self,
        layer: &str,
        zone: Zone,
        manifest: &Manifest,
        live: &[ArchiveIndex],
    ) {
        let mut b = self.lock();
        let g = manifest.generation;
        let named: Vec<&str> = manifest.archives.iter().map(|a| a.key.as_str()).collect();
        for (i, a) in manifest.archives.iter().enumerate() {
            let entry = b.archives.entry(a.key.clone()).or_insert(ArchiveEntry {
                layer: layer.into(),
                zone,
                state: ArchiveState::Live,
                tiles_known: false,
                epoch: a.epoch.clone(),
            });
            entry.state = entry.state.max(ArchiveState::Live);
            if let Some(index) = live.iter().find(|x| x.archive == a.key) {
                entry.tiles_known = true;
                apply_tiles(&mut b, layer, g, index, &a.epoch, &named[i + 1..]);
            }
        }
        for r in &manifest.retired {
            if let Some(a) = b.archives.get_mut(&r.key) {
                a.state = a.state.max(ArchiveState::Retired);
            }
        }
        // Nothing of the zone points at an archive the manifest does not name.
        let zone_archives: Vec<String> = b
            .archives
            .iter()
            .filter(|(_, a)| a.layer == layer && a.zone == zone)
            .map(|(k, _)| k.clone())
            .filter(|k| !named.contains(&k.as_str()))
            .collect();
        b.tiles.retain(|(l, _), t| {
            l != layer || t.generation > g || !zone_archives.contains(&t.archive)
        });
        let z = b.zones.entry((layer.into(), zone)).or_default();
        if g >= z.generation {
            z.generation = g;
            z.stale = false;
        }
    }
}

/// The tiles of `index` win where no newer publication, nor an archive in
/// `later`, holds them.
fn apply_tiles(
    b: &mut Book,
    layer: &str,
    g: u64,
    index: &ArchiveIndex,
    epoch: &str,
    later: &[&str],
) {
    for (id, stamp) in index.ids.iter().zip(&index.stamps) {
        let key = (layer.to_string(), *id);
        let kept = b
            .tiles
            .get(&key)
            .is_some_and(|t| t.generation > g || later.contains(&t.archive.as_str()));
        if !kept {
            b.tiles.insert(
                key,
                TileEntry {
                    stamp: *stamp,
                    archive: index.archive.clone(),
                    generation: g,
                    epoch: epoch.to_string(),
                },
            );
        }
    }
}

#[async_trait]
impl Ledger for MemoryRegister {
    async fn writing(&self, layer: &str, zone: Zone, key: &str) -> Result<(), String> {
        let mut b = self.lock();
        b.zones.entry((layer.into(), zone)).or_default();
        b.archives.entry(key.into()).or_insert(ArchiveEntry {
            layer: layer.into(),
            zone,
            state: ArchiveState::Writing,
            tiles_known: false,
            epoch: String::new(),
        });
        Ok(())
    }

    async fn published(&self, p: &Publication<'_>) -> Result<(), String> {
        let mut b = self.lock();
        let g = p.manifest.generation;
        for a in &p.manifest.archives {
            let entry = b.archives.entry(a.key.clone()).or_insert(ArchiveEntry {
                layer: p.layer.into(),
                zone: p.zone,
                state: ArchiveState::Writing,
                tiles_known: false,
                epoch: a.epoch.clone(),
            });
            entry.epoch = a.epoch.clone();
            entry.state = entry.state.max(ArchiveState::Live);
        }
        if let Some(added) = p.added {
            let live = b
                .archives
                .get(&added.archive)
                .is_some_and(|a| a.state == ArchiveState::Live);
            let at = p
                .manifest
                .archives
                .iter()
                .position(|a| a.key == added.archive);
            if let (true, Some(at)) = (live, at) {
                let later: Vec<&str> = p.manifest.archives[at + 1..]
                    .iter()
                    .map(|a| a.key.as_str())
                    .collect();
                let epoch = p.manifest.archives[at].epoch.clone();
                apply_tiles(&mut b, p.layer, g, added, &epoch, &later);
                if let Some(a) = b.archives.get_mut(&added.archive) {
                    a.tiles_known = true;
                }
            }
        }
        let mut dropped = 0;
        for r in p.replaced {
            if let Some(a) = b.archives.get_mut(r) {
                a.state = a.state.max(ArchiveState::Retired);
            }
            let before = b.tiles.len();
            b.tiles.retain(|_, t| &t.archive != r);
            dropped += before - b.tiles.len();
        }
        let z = b.zones.entry((p.layer.into(), p.zone)).or_default();
        z.generation = z.generation.max(g);
        if p.uncovering && dropped > 0 {
            z.stale = true;
        }
        Ok(())
    }

    async fn deleting(&self, _layer: &str, _zone: Zone, keys: &[String]) -> Result<(), String> {
        let mut b = self.lock();
        b.tiles.retain(|_, t| !keys.contains(&t.archive));
        Ok(())
    }

    async fn deleted(&self, _layer: &str, _zone: Zone, keys: &[String]) -> Result<(), String> {
        let mut b = self.lock();
        for k in keys {
            if let Some(a) = b.archives.get_mut(k) {
                a.state = ArchiveState::Deleted;
            }
        }
        Ok(())
    }
}

#[async_trait]
impl IndexReader for MemoryRegister {
    async fn zones(&self, layer: &str) -> Result<Option<Vec<KnownZone>>, String> {
        let b = self.lock();
        Ok(Some(
            b.zones
                .iter()
                .filter(|((l, _), _)| l == layer)
                .map(|((_, zone), z)| {
                    let complete = b.archives.values().all(|a| {
                        a.layer != layer
                            || a.zone != *zone
                            || a.state != ArchiveState::Live
                            || a.tiles_known
                    });
                    KnownZone {
                        zone: *zone,
                        indexed: !z.stale && complete,
                    }
                })
                .collect(),
        ))
    }

    fn tiles<'a>(&'a self, layer: &'a str, ranges: &'a [RangeInclusive<u64>]) -> TileBatches<'a> {
        let rows: Vec<RegisteredTile> = {
            let b = self.lock();
            ranges
                .iter()
                .flat_map(|r| {
                    b.tiles
                        .range((layer.to_string(), *r.start())..=(layer.to_string(), *r.end()))
                        .map(|((_, id), t)| RegisteredTile {
                            id: *id,
                            stamp: t.stamp,
                            epoch: t.epoch.clone(),
                        })
                        .collect::<Vec<_>>()
                })
                .collect()
        };
        let batches: Vec<Result<Vec<RegisteredTile>, String>> = rows
            .chunks(self.batch.max(1))
            .map(|c| Ok(c.to_vec()))
            .collect();
        Box::pin(futures_util::stream::iter(batches))
    }
}
