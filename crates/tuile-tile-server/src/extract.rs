// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Extracts: the tiles a reader asks for, as one archive.
//!
//! A renderer that knows ahead every tile it will draw asks for them once,
//! and reads them from a local file for as long as it renders. The store
//! gathers them: every zone they lie in, each of its live archives fetched
//! whole — newest first, the first to hold a tile wins it, as a reader of the
//! zone would see — and keeps the tiles asked for. They go out as **one
//! PMTiles archive** of the layer, the tile ids the layer's own, whatever
//! zones they came from.
//!
//! What is asked for is tile addresses, and/or levels: an extract of
//! `tiles` holds those of them the store has; `levels` keeps only those
//! levels; `whole_zones` takes the zones of the given addresses whole.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt};
use pmtiles::TileCoord;

use crate::archive;
use crate::layer::{Layer, Zone};
use crate::manifest;
use crate::store::{download, TileStore};
use crate::StoreError;

/// Zones read at once while extracting.
const ZONES_AT_ONCE: usize = 16;

/// What an extract holds.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractAsk {
    pub layer: String,
    /// Tile addresses, `(level, x, y)` on the layer's grid.
    #[serde(default)]
    pub tiles: Vec<(u8, u32, u32)>,
    /// Addresses whose zones are taken whole.
    #[serde(default)]
    pub whole_zones: Vec<(u8, u32, u32)>,
    /// The lowest and highest level kept; every level when absent.
    #[serde(default)]
    pub levels: Option<(u8, u8)>,
}

/// What an extract took and made.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExtractReport {
    pub zones: u64,
    /// Archives fetched whole, and their bytes.
    pub archives: u64,
    pub archive_bytes: u64,
    /// Tiles kept, and the archive's bytes.
    pub tiles: u64,
    pub bytes: u64,
    pub millis: u64,
}

/// Which tiles of a zone are kept.
struct Keep {
    ids: HashSet<u64>,
    whole: HashSet<Zone>,
    levels: Option<(u8, u8)>,
}

impl Keep {
    fn of(layer: &Layer, ask: &ExtractAsk) -> Result<(Self, BTreeSet<Zone>), StoreError> {
        let mut zones = BTreeSet::new();
        let mut ids = HashSet::with_capacity(ask.tiles.len());
        for &(level, x, y) in &ask.tiles {
            zones.insert(layer.zone_of(level, x, y)?);
            ids.insert(layer.grid.to_id(level, x, y)?);
        }
        let mut whole = HashSet::new();
        for &(level, x, y) in &ask.whole_zones {
            let zone = layer.zone_of(level, x, y)?;
            zones.insert(zone);
            whole.insert(zone);
        }
        Ok((
            Self {
                ids,
                whole,
                levels: ask.levels,
            },
            zones,
        ))
    }

    fn keeps(&self, layer: &Layer, zone: Zone, id: u64) -> bool {
        let level = pmtiles::TileId::new(id)
            .ok()
            .and_then(|t| layer.grid.from_archive(TileCoord::from(t)))
            .map(|(level, _, _)| level);
        let Some(level) = level else { return false };
        if let Some((low, high)) = self.levels {
            if level < low || level > high {
                return false;
            }
        }
        self.whole.contains(&zone) || self.ids.contains(&id)
    }
}

impl TileStore {
    /// The tiles `ask` names, as one archive of its layer: a temporary file,
    /// gone when dropped.
    pub async fn extract(
        &self,
        ask: &ExtractAsk,
    ) -> Result<(tempfile::NamedTempFile, ExtractReport), StoreError> {
        let began = Instant::now();
        let layer = self.layer(&ask.layer)?.clone();
        let (keep, zones) = Keep::of(&layer, ask)?;
        let (layer, keep) = (Arc::new(layer), Arc::new(keep));
        let mut report = ExtractReport {
            zones: zones.len() as u64,
            ..Default::default()
        };
        let read: Vec<(BTreeMap<u64, Bytes>, ExtractReport)> = futures_util::stream::iter(zones)
            .map(|zone| {
                let (layer, keep) = (layer.clone(), keep.clone());
                async move { self.extract_zone(&layer, zone, &keep).await }
            })
            .buffer_unordered(ZONES_AT_ONCE)
            .try_collect()
            .await?;
        // Zones do not share tiles: their maps join without a choice to make.
        let mut tiles = BTreeMap::new();
        for (zone_tiles, zone) in read {
            report.archives += zone.archives;
            report.archive_bytes += zone.archive_bytes;
            tiles.extend(zone_tiles);
        }
        report.tiles = tiles.len() as u64;
        let zooms = archive::zoom_range(tiles.keys());
        let epoch = layer.epoch(self.now());
        let (file, _) = tokio::task::spawn_blocking(move || {
            archive::write(&layer, &epoch, zooms, tiles.into_iter().map(Ok))
        })
        .await
        .map_err(|e| StoreError::Corrupt(format!("extract writer: {e}")))??;
        report.bytes = std::fs::metadata(file.path())?.len();
        report.millis = began.elapsed().as_millis() as u64;
        Ok((file, report))
    }

    /// The tiles `keep` wants of one zone, from its live archives, newest
    /// first.
    async fn extract_zone(
        &self,
        layer: &Layer,
        zone: Zone,
        keep: &Keep,
    ) -> Result<(BTreeMap<u64, Bytes>, ExtractReport), StoreError> {
        let mut report = ExtractReport::default();
        let now = self.now();
        let m = manifest::read(self.object_store().as_ref(), &layer.zone_prefix(zone))
            .await?
            .manifest;
        let archives: Vec<_> = m
            .archives
            .iter()
            .filter(|a| !layer.is_expired(&a.epoch, now))
            .cloned()
            .collect();
        let scratch = tempfile::tempdir()?;
        // Fetched together, read newest first.
        let local: Vec<Option<std::path::PathBuf>> =
            futures_util::stream::iter(archives.iter().enumerate())
                .map(|(i, a)| {
                    let to = scratch.path().join(format!("{i}.pmtiles"));
                    async move {
                        match download(self.object_store().as_ref(), &a.key, &to).await {
                            Ok(()) => Ok(Some(to)),
                            // Gone under us (merged, expired): nothing of it to take.
                            Err(StoreError::ObjectStore(object_store::Error::NotFound {
                                ..
                            })) => Ok(None),
                            Err(e) => Err(e),
                        }
                    }
                })
                .buffered(ZONES_AT_ONCE)
                .try_collect()
                .await?;
        let mut tiles = BTreeMap::new();
        for path in local.into_iter().rev().flatten() {
            report.archives += 1;
            report.archive_bytes += std::fs::metadata(&path)?.len();
            let reader = Arc::new(archive::open_local(&path).await?);
            for id in archive::ids(reader.clone()).await? {
                if tiles.contains_key(&id) || !keep.keeps(layer, zone, id) {
                    continue;
                }
                if let Some(bytes) = reader.get_tile(pmtiles::TileId::new(id)?).await? {
                    tiles.insert(id, bytes);
                }
            }
        }
        Ok((tiles, report))
    }
}

/// Extracts built and not yet all read, held in memory: a reader takes one
/// in parts, each a request of its own, as many at once as it likes.
///
/// Held until read whole or too old, within a budget of bytes: an extract
/// that would not fit is refused rather than another one dropped, and the
/// asker asks again later.
pub struct Extracts {
    held: std::sync::Mutex<std::collections::HashMap<String, Held>>,
    budget: u64,
    ttl: std::time::Duration,
}

struct Held {
    bytes: Bytes,
    at: Instant,
    /// Parts not read yet, by part size: the extract goes once all of one
    /// division have been.
    unread: std::collections::HashMap<u64, BTreeSet<u64>>,
}

/// What a reader is told of an extract it asked for.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExtractHeld {
    pub id: String,
    pub bytes: u64,
    pub report: ExtractReport,
}

/// The number of parts of `bytes`, each of `part` bytes but the last.
pub fn parts(bytes: u64, part: u64) -> u64 {
    bytes.div_ceil(part.max(1)).max(1)
}

impl Extracts {
    pub fn new(budget: u64, ttl: std::time::Duration) -> Self {
        Self {
            held: Default::default(),
            budget,
            ttl,
        }
    }

    /// Holds `bytes`; `None` when they do not fit in what is left of the
    /// budget.
    pub fn hold(&self, bytes: Bytes) -> Option<String> {
        let mut held = self.held.lock().ok()?;
        let ttl = self.ttl;
        held.retain(|_, h| h.at.elapsed() < ttl);
        let used: u64 = held.values().map(|h| h.bytes.len() as u64).sum();
        if used + bytes.len() as u64 > self.budget {
            return None;
        }
        let id = format!(
            "{:016x}{:016x}",
            rand::random::<u64>(),
            rand::random::<u64>()
        );
        held.insert(
            id.clone(),
            Held {
                bytes,
                at: Instant::now(),
                unread: Default::default(),
            },
        );
        Some(id)
    }

    /// Part `n` of extract `id` cut in parts of `part` bytes; `None` when
    /// there is no such extract or part. Once every part of that cut has
    /// been read, the extract is let go.
    pub fn part(&self, id: &str, part: u64, n: u64) -> Option<Bytes> {
        let mut held = self.held.lock().ok()?;
        let h = held.get_mut(id)?;
        let len = h.bytes.len() as u64;
        let count = parts(len, part);
        if n >= count {
            return None;
        }
        let out = h
            .bytes
            .slice((n * part) as usize..((n + 1) * part).min(len) as usize);
        let unread = h.unread.entry(part).or_insert_with(|| (0..count).collect());
        unread.remove(&n);
        if unread.is_empty() {
            held.remove(id);
        }
        Some(out)
    }

    /// Bytes held now.
    pub fn held_bytes(&self) -> u64 {
        self.held
            .lock()
            .map(|h| h.values().map(|h| h.bytes.len() as u64).sum())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{self, Catalog, LayerDef};
    use crate::grid::Grid;
    use crate::store::StoreConfig;
    use object_store::memory::InMemory;
    use object_store::ObjectStore;

    async fn store() -> TileStore {
        let memory: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let def = LayerDef {
            name: "imagery".into(),
            grid: Grid::WebMercator,
            tile_type: "png".into(),
            tile_compression: "none".into(),
            zone_level: 4,
            expiry_days: None,
            content_type: "image/png".into(),
        };
        catalog::write(memory.as_ref(), &Catalog { layers: vec![def] })
            .await
            .expect("catalog");
        TileStore::open(memory, StoreConfig::default())
            .await
            .expect("open")
    }

    async fn put(store: &TileStore, (level, x, y): (u8, u32, u32), body: &str) {
        store
            .put("imagery", level, x, y, Bytes::from(body.to_string()))
            .await
            .expect("put");
    }

    async fn held(file: &tempfile::NamedTempFile) -> BTreeMap<(u8, u32, u32), String> {
        let reader = Arc::new(archive::open_local(file.path()).await.expect("open"));
        let mut out = BTreeMap::new();
        for id in archive::ids(reader.clone()).await.expect("ids") {
            let c = TileCoord::from(pmtiles::TileId::new(id).expect("id"));
            let bytes = reader.get_tile(c).await.expect("get").expect("tile");
            out.insert(
                (c.z(), c.x(), c.y()),
                String::from_utf8_lossy(&bytes).into(),
            );
        }
        out
    }

    #[tokio::test]
    async fn an_extract_holds_the_tiles_asked_for_as_the_zone_s_newest_archive_has_them() {
        let store = store().await;
        // Two zones; in the first, a tile written again in a newer archive.
        for (at, body) in [
            ((7, 77, 50), "old"),
            ((7, 78, 50), "b"),
            ((8, 154, 100), "c"),
            ((7, 5, 120), "far"),
        ] {
            put(&store, at, body).await;
        }
        store.flush_all().await.expect("flush");
        put(&store, (7, 77, 50), "new").await;
        store.flush_all().await.expect("flush");

        let ask = ExtractAsk {
            layer: "imagery".into(),
            tiles: vec![(7, 77, 50), (8, 154, 100), (7, 5, 120), (7, 76, 50)],
            ..Default::default()
        };
        let (file, report) = store.extract(&ask).await.expect("extract");
        assert_eq!(
            held(&file).await,
            BTreeMap::from([
                ((7, 5, 120), "far".to_string()),
                ((7, 77, 50), "new".to_string()),
                ((8, 154, 100), "c".to_string()),
            ]),
            "the newest wins, the tile not asked for and the one never written are not there"
        );
        assert_eq!((report.zones, report.archives, report.tiles), (2, 3, 3));
        assert_eq!(
            report.bytes,
            std::fs::metadata(file.path()).expect("meta").len()
        );

        // Levels cut what the tiles asked for; a zone taken whole brings
        // what was not named.
        let ask = ExtractAsk {
            layer: "imagery".into(),
            whole_zones: vec![(7, 77, 50)],
            levels: Some((7, 7)),
            ..Default::default()
        };
        let (file, _) = store.extract(&ask).await.expect("extract");
        assert_eq!(
            held(&file).await.into_keys().collect::<Vec<_>>(),
            [(7, 77, 50), (7, 78, 50)]
        );
    }

    #[test]
    fn an_extract_is_read_in_parts_and_let_go_once_every_part_is_read() {
        let extracts = Extracts::new(100, std::time::Duration::from_secs(60));
        let id = extracts
            .hold(Bytes::from_static(b"0123456789"))
            .expect("fits");
        assert_eq!(parts(10, 4), 3);
        assert_eq!(extracts.part(&id, 4, 2).as_deref(), Some(&b"89"[..]));
        assert_eq!(extracts.part(&id, 4, 3), None, "no fourth part");
        assert_eq!(extracts.part(&id, 4, 0).as_deref(), Some(&b"0123"[..]));
        assert_eq!(extracts.held_bytes(), 10, "a part still unread");
        assert_eq!(extracts.part(&id, 4, 1).as_deref(), Some(&b"4567"[..]));
        assert_eq!(extracts.held_bytes(), 0, "all read: gone");
        assert_eq!(extracts.part(&id, 4, 0), None);

        // Over the budget: refused, nothing dropped for it.
        let kept = extracts.hold(Bytes::from(vec![0u8; 60])).expect("fits");
        assert!(extracts.hold(Bytes::from(vec![0u8; 60])).is_none());
        assert_eq!(extracts.part(&kept, 32, 1).map(|b| b.len()), Some(28));

        // Too old: gone at the next hold.
        let brief = Extracts::new(100, std::time::Duration::ZERO);
        let old = brief.hold(Bytes::from_static(b"x")).expect("fits");
        brief.hold(Bytes::from_static(b"y")).expect("fits");
        assert_eq!(brief.part(&old, 1, 0), None);
    }
}
