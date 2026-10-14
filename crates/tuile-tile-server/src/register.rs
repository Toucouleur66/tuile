// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A register in memory: the reference of what a register does with what a
//! [`Ledger`] and an [`Absences`] store are told, and an [`IndexReader`] for
//! a host that keeps no database (tests, one process that writes and reads
//! its own store).
//!
//! A register keeps **one row per tile of a layer**: the copy a reader gets
//! — its stamp, the archive that holds it, the epoch of that archive, the
//! generation of the publication that put it there — or that the tile is
//! absent. A publication decides from what it says, and nothing else:
//!
//! - every archive its manifest names is live — unless a newer publication
//!   retired it: an archive's state only moves forward (written, live,
//!   retired, deleted);
//! - each tile of the archive it added, if that archive is still live, wins
//!   unless the copy kept was put there by a **newer** publication, or is in
//!   an archive that comes **after** the added one in the manifest (a merge
//!   puts its archive where its run was; a delta after the run still wins),
//!   or the tile was said absent after the archive's newest tile was fetched;
//! - then no tile points at an archive it replaced any more;
//! - a publication that is [`uncovering`](Publication::uncovering) and took
//!   a tile out leaves its zone not known whole, until it is rebuilt.
//!
//! An absence wins over a copy fetched before it, and over an older absence
//! (its expiry with it); it is never written over a newer copy.
//!
//! Replayed, or told late, a publication changes nothing a newer one
//! decided. Told nothing of an archive's tiles (a publication missed, a
//! layer whose tiles are not indexed), a zone is not known whole either, and
//! a projection reads it from its archives.

use std::collections::{BTreeMap, HashMap};
use std::ops::RangeInclusive;
use std::sync::{Mutex, MutexGuard};
use std::time::SystemTime;

use async_trait::async_trait;

use crate::absences::{Absence, Absences};
use crate::index::{ArchiveIndex, Stamp};
use crate::ledger::{IndexReader, KnownZone, Ledger, Publication, RegisteredTile, TileBatches};
use crate::manifest::{ArchiveRef, Manifest};
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

/// What a register holds of a tile: the copy a reader gets, or that it is
/// absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TileEntry {
    pub absent: bool,
    /// None for an absence.
    pub stamp: Stamp,
    /// The archive that holds the copy; empty for an absence.
    pub archive: String,
    /// The publication's; 0 for an absence.
    pub generation: u64,
    pub epoch: String,
    /// The copy's archive's newest fetch, or when the absence was said
    /// (milliseconds since the Unix epoch): what decides between the two.
    pub fetched_ms: u64,
    /// An absence's expiry.
    pub expires_at: Option<SystemTime>,
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
    /// `(layer, tile id)`.
    pub tiles: BTreeMap<(String, u64), TileEntry>,
}

/// The layer a layer's tiles are kept under, and whether it is an absence
/// sibling.
pub fn base_of(layer: &str) -> (&str, bool) {
    match layer.strip_suffix(crate::catalog::ABSENT_SUFFIX) {
        Some(base) => (base, true),
        None => (layer, false),
    }
}

/// When a copy in `archive` counts as fetched, against an absence: its
/// newest tile's fetch, or its publication when that is not recorded.
pub fn copy_fetched_ms(archive: &ArchiveRef) -> u64 {
    archive
        .newest_fetch_ms
        .unwrap_or(archive.created.saturating_mul(1000))
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

    /// Forgets the absences expired at `now`; says how many.
    pub fn expire(&self, now: SystemTime) -> usize {
        let mut b = self.lock();
        let before = b.tiles.len();
        b.tiles
            .retain(|_, t| !(t.absent && t.expires_at.is_some_and(|at| at <= now)));
        before - b.tiles.len()
    }

    /// Puts a zone right from its manifest and the index of every one of its
    /// live archives, as a rebuild does: what the publications would have
    /// left, and nothing of what they no longer name. A zone of an absence
    /// sibling becomes absences, each expiring with its archive's epoch.
    pub fn rebuild_zone(
        &self,
        layer: &crate::Layer,
        zone: Zone,
        manifest: &Manifest,
        live: &[ArchiveIndex],
    ) {
        let (base, absent) = base_of(&layer.name);
        if absent {
            let mut b = self.lock();
            for a in &manifest.archives {
                let Some(index) = live.iter().find(|x| x.archive == a.key) else {
                    continue;
                };
                for id in &index.ids {
                    record(
                        &mut b,
                        base,
                        Absence {
                            id: *id,
                            fetched_ms: copy_fetched_ms(a),
                            expires_at: layer.expires_at(&a.epoch),
                        },
                    );
                }
            }
            return;
        }
        let mut b = self.lock();
        let g = manifest.generation;
        let named: Vec<&str> = manifest.archives.iter().map(|a| a.key.as_str()).collect();
        for (i, a) in manifest.archives.iter().enumerate() {
            let entry = b.archives.entry(a.key.clone()).or_insert(ArchiveEntry {
                layer: base.into(),
                zone,
                state: ArchiveState::Live,
                tiles_known: false,
                epoch: a.epoch.clone(),
            });
            entry.state = entry.state.max(ArchiveState::Live);
            if let Some(index) = live.iter().find(|x| x.archive == a.key) {
                entry.tiles_known = true;
                apply_tiles(&mut b, base, g, index, a, &named[i + 1..]);
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
            .filter(|(_, a)| a.layer == base && a.zone == zone)
            .map(|(k, _)| k.clone())
            .filter(|k| !named.contains(&k.as_str()))
            .collect();
        b.tiles
            .retain(|_, t| t.absent || t.generation > g || !zone_archives.contains(&t.archive));
        let z = b.zones.entry((base.into(), zone)).or_default();
        if g >= z.generation {
            z.generation = g;
            z.stale = false;
        }
    }
}

/// The tiles of `index`, in `archive`, win where no newer publication, nor
/// an archive in `later`, holds them, and no absence said after them.
fn apply_tiles(
    b: &mut Book,
    layer: &str,
    g: u64,
    index: &ArchiveIndex,
    archive: &ArchiveRef,
    later: &[&str],
) {
    let fetched_ms = copy_fetched_ms(archive);
    for (id, stamp) in index.ids.iter().zip(&index.stamps) {
        let key = (layer.to_string(), *id);
        let kept = b.tiles.get(&key).is_some_and(|t| {
            if t.absent {
                t.fetched_ms > fetched_ms
            } else {
                t.generation > g || later.contains(&t.archive.as_str())
            }
        });
        if !kept {
            b.tiles.insert(
                key,
                TileEntry {
                    absent: false,
                    stamp: *stamp,
                    archive: index.archive.clone(),
                    generation: g,
                    epoch: archive.epoch.clone(),
                    fetched_ms,
                    expires_at: None,
                },
            );
        }
    }
}

/// An absence wins over a copy or an absence fetched before it.
fn record(b: &mut Book, layer: &str, a: Absence) {
    let key = (layer.to_string(), a.id);
    if b.tiles
        .get(&key)
        .is_some_and(|t| t.fetched_ms >= a.fetched_ms)
    {
        return;
    }
    b.tiles.insert(
        key,
        TileEntry {
            absent: true,
            stamp: Stamp::default(),
            archive: String::new(),
            generation: 0,
            epoch: String::new(),
            fetched_ms: a.fetched_ms,
            expires_at: a.expires_at,
        },
    );
}

#[async_trait]
impl Ledger for MemoryRegister {
    async fn writing(&self, layer: &str, zone: Zone, key: &str) -> Result<(), String> {
        let (base, _) = base_of(layer);
        let mut b = self.lock();
        b.zones.entry((base.into(), zone)).or_default();
        b.archives.entry(key.into()).or_insert(ArchiveEntry {
            layer: base.into(),
            zone,
            state: ArchiveState::Writing,
            tiles_known: false,
            epoch: String::new(),
        });
        Ok(())
    }

    async fn published(&self, p: &Publication<'_>) -> Result<(), String> {
        // An absence sibling's archives are a store without an absence
        // store: its absences reach a register through `Absences`, or a
        // rebuild.
        if base_of(p.layer).1 {
            return Ok(());
        }
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
                apply_tiles(&mut b, p.layer, g, added, &p.manifest.archives[at], &later);
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
            b.tiles.retain(|_, t| t.absent || &t.archive != r);
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
        b.tiles
            .retain(|_, t| t.absent || !keys.contains(&t.archive));
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
impl Absences for MemoryRegister {
    async fn record(&self, layer: &str, absences: &[Absence]) -> Result<(), String> {
        let mut b = self.lock();
        for a in absences {
            record(&mut b, layer, *a);
        }
        Ok(())
    }

    async fn absent(&self, layer: &str, id: u64, now: SystemTime) -> Result<bool, String> {
        let b = self.lock();
        Ok(b.tiles
            .get(&(layer.to_string(), id))
            .is_some_and(|t| t.absent && t.expires_at.is_none_or(|at| at > now)))
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
                            absent: t.absent,
                            stamp: t.stamp,
                            epoch: t.epoch.clone(),
                            expires_at: t.expires_at,
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
