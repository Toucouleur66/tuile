// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Archive indexes: what an archive holds, without its bytes.
//!
//! A consumer that only **names** tiles — a bake by reference, which records
//! which tiles a frame drapes and a digest of each, and composes nothing —
//! needs three things of a zone: which tiles exist, which archive's copy a
//! reader would get, and a digest of that copy's bytes. Reading the archives
//! to learn that costs the archives: measured on one chunk of a film, fifteen
//! gigabytes of imagery downloaded to name three thousand tiles.
//!
//! So beside each archive of a layer that asks for it
//! ([`Layer::index`](crate::Layer::index)) lies one small immutable object,
//! `<archive key>.index` ([`index_key`]): the archive's tile ids, sorted, and
//! for each the [`digest`] of its bytes exactly as the store returns them,
//! and its image size when it is an image. It is a FlatBuffer
//! (`schema/tile_index.fbs`), about twenty bytes a tile, uncompressed like the
//! archives themselves.
//!
//! # Its life is the archive's
//!
//! - **Written** by whatever writes the archive — a delta, a compaction, a
//!   scrub — after the archive and **before the manifest that publishes it**:
//!   a reader that sees the archive in a manifest finds its index, or the
//!   archive was published before indexes existed. An index uploaded for an
//!   archive that is then never published is an orphan, removed by the
//!   cleanup like an orphaned archive.
//! - **Deleted** with the archive, by every path that deletes one.
//! - **Never rewritten**: an archive's key is unique, and so is its index's.
//!
//! An archive without an index is not an error: a reader falls back to the
//! archive itself. [`TileStore::backfill_indexes`](crate::TileStore::backfill_indexes)
//! writes the indexes of the archives published before.
//!
//! # A zone, from its indexes
//!
//! [`ZoneIndex`] folds a zone's live archives the way a reader of the store
//! does: newest first, the first archive that holds a tile wins it.

use std::collections::HashMap;
use std::path::Path as FsPath;
use std::sync::Arc;

use bytes::Bytes;
use futures_util::TryStreamExt;
use object_store::path::Path;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};

use crate::archive;
use crate::layer::Layer;
use crate::store::TileStore;
use crate::StoreError;

#[allow(
    clippy::all,
    clippy::pedantic,
    clippy::unwrap_used,
    clippy::panic,
    clippy::needless_lifetimes,
    dead_code,
    unused_imports,
    unsafe_code,
    non_snake_case,
    non_camel_case_types,
    reason = "flatc output, committed verbatim"
)]
mod generated {
    include!("tile_index_generated.rs");
}

use generated::tuile::tiles as fb;

/// The index layout written. Bumped when an old reader would misread a new
/// index; a reader refuses any other.
pub const INDEX_VERSION: u32 = 1;
/// What an index's key adds to its archive's.
pub const INDEX_SUFFIX: &str = ".index";

/// Where the index of the archive at `archive_key` lives.
pub fn index_key(archive_key: &str) -> String {
    format!("{archive_key}{INDEX_SUFFIX}")
}

/// The archive an index key belongs to, if it is one.
pub fn archive_of_index(key: &str) -> Option<&str> {
    key.strip_suffix(INDEX_SUFFIX)
}

/// FNV-1a, 64 bits, over a tile's bytes as the store returns them.
///
/// The digest a pack's reference to a source tile carries, and the same
/// function: two implementations of one hash agree until the day one is
/// edited, so the bake's own tests compare this with its own on the same
/// bytes.
pub fn digest(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// What an index says of one tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Stamp {
    /// [`digest`] of the tile's bytes.
    pub digest: u64,
    /// The image's size in pixels as its header states; 0 × 0 when the tile
    /// is not an image this could read.
    pub width: u16,
    pub height: u16,
}

impl Stamp {
    /// The stamp of a tile's bytes; `images` says whether to read an image
    /// size from them.
    pub fn of(bytes: &[u8], images: bool) -> Self {
        let (width, height) = if images {
            image_size(bytes).unwrap_or((0, 0))
        } else {
            (0, 0)
        };
        Self {
            digest: digest(bytes),
            width,
            height,
        }
    }

    /// Whether the image size is known.
    pub fn has_size(&self) -> bool {
        self.width > 0 && self.height > 0
    }
}

/// An image's size from its header alone, nothing decoded.
fn image_size(bytes: &[u8]) -> Option<(u16, u16)> {
    let (w, h) = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    Some((u16::try_from(w).ok()?, u16::try_from(h).ok()?))
}

/// Whether a layer's tiles are images an index reads the size of.
pub fn holds_images(layer: &Layer) -> bool {
    matches!(
        layer.tile_type,
        pmtiles::TileType::Jpeg | pmtiles::TileType::Png | pmtiles::TileType::Webp
    )
}

/// One archive's index, read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArchiveIndex {
    pub layer: String,
    /// The archive's key.
    pub archive: String,
    /// Strictly increasing.
    pub ids: Vec<u64>,
    /// `stamps[i]` is `ids[i]`'s.
    pub stamps: Vec<Stamp>,
}

impl ArchiveIndex {
    /// The stamp of tile `id`, if the archive holds it.
    pub fn get(&self, id: u64) -> Option<Stamp> {
        self.ids
            .binary_search(&id)
            .ok()
            .and_then(|i| self.stamps.get(i).copied())
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// The FlatBuffer.
    pub fn encode(&self) -> Vec<u8> {
        let mut b = flatbuffers::FlatBufferBuilder::with_capacity(64 + self.ids.len() * 20);
        let layer = b.create_string(&self.layer);
        let archive = b.create_string(&self.archive);
        let ids = b.create_vector(&self.ids);
        let digests: Vec<u64> = self.stamps.iter().map(|s| s.digest).collect();
        let digests = b.create_vector(&digests);
        let widths: Vec<u16> = self.stamps.iter().map(|s| s.width).collect();
        let widths = b.create_vector(&widths);
        let heights: Vec<u16> = self.stamps.iter().map(|s| s.height).collect();
        let heights = b.create_vector(&heights);
        let root = fb::TileIndex::create(
            &mut b,
            &fb::TileIndexArgs {
                version: INDEX_VERSION,
                layer: Some(layer),
                archive: Some(archive),
                tiles: self.ids.len() as u64,
                ids: Some(ids),
                digests: Some(digests),
                widths: Some(widths),
                heights: Some(heights),
            },
        );
        fb::finish_tile_index_buffer(&mut b, root);
        b.finished_data().to_vec()
    }

    /// Reads a FlatBuffer, checking everything a reader relies on: the
    /// identifier, the version, the lengths, the order of the ids.
    pub fn decode(bytes: &[u8]) -> Result<Self, StoreError> {
        let corrupt = |why: String| StoreError::Corrupt(format!("archive index: {why}"));
        // The identifier check asserts on a buffer too short to hold one.
        const ROOT_AND_IDENTIFIER: usize = 8;
        if bytes.len() < ROOT_AND_IDENTIFIER || !fb::tile_index_buffer_has_identifier(bytes) {
            return Err(corrupt("not an index".into()));
        }
        // An index of a few hundred thousand tiles is a few megabytes of
        // scalar vectors: within the verifier's default bounds.
        let root = fb::root_as_tile_index(bytes).map_err(|e| corrupt(e.to_string()))?;
        if root.version() != INDEX_VERSION {
            return Err(corrupt(format!(
                "version {} (this reader knows {INDEX_VERSION})",
                root.version()
            )));
        }
        let ids: Vec<u64> = root.ids().map(|v| v.iter().collect()).unwrap_or_default();
        let digests: Vec<u64> = root
            .digests()
            .map(|v| v.iter().collect())
            .unwrap_or_default();
        let widths: Vec<u16> = root
            .widths()
            .map(|v| v.iter().collect())
            .unwrap_or_default();
        let heights: Vec<u16> = root
            .heights()
            .map(|v| v.iter().collect())
            .unwrap_or_default();
        let n = ids.len();
        if root.tiles() != n as u64 || digests.len() != n || widths.len() != n || heights.len() != n
        {
            return Err(corrupt(format!(
                "{} tiles said, vectors of {n}, {}, {}, {}",
                root.tiles(),
                digests.len(),
                widths.len(),
                heights.len()
            )));
        }
        if ids.windows(2).any(|w| w[0] >= w[1]) {
            return Err(corrupt("tile ids out of order".into()));
        }
        let stamps = digests
            .into_iter()
            .zip(widths)
            .zip(heights)
            .map(|((digest, width), height)| Stamp {
                digest,
                width,
                height,
            })
            .collect();
        Ok(Self {
            layer: root.layer().unwrap_or_default().to_string(),
            archive: root.archive().unwrap_or_default().to_string(),
            ids,
            stamps,
        })
    }
}

/// The index of the archive file at `path`, to be stored as `archive_key`'s:
/// every tile read back as a reader of the store would get it.
pub async fn of_local_archive(
    path: &FsPath,
    layer: &Layer,
    archive_key: &str,
) -> Result<ArchiveIndex, StoreError> {
    let reader = Arc::new(archive::open_local(path).await?);
    of_reader(reader, layer, archive_key).await
}

/// The same, of an open archive.
pub async fn of_reader<B, C>(
    reader: Arc<pmtiles::AsyncPmTilesReader<B, C>>,
    layer: &Layer,
    archive_key: &str,
) -> Result<ArchiveIndex, StoreError>
where
    B: pmtiles::AsyncBackend + Sync + Send + 'static,
    C: pmtiles::DirectoryCache + Sync + Send + 'static,
{
    let images = holds_images(layer);
    let mut out = ArchiveIndex {
        layer: layer.name.clone(),
        archive: archive_key.to_string(),
        ..ArchiveIndex::default()
    };
    let mut entries = reader.clone().entries();
    while let Some(entry) = entries.try_next().await? {
        for tid in entry.iter_coords() {
            let bytes = reader.get_tile(tid).await?.ok_or_else(|| {
                StoreError::Corrupt(format!(
                    "{archive_key}: tile {} listed but absent",
                    tid.value()
                ))
            })?;
            out.ids.push(tid.value());
            out.stamps.push(Stamp::of(&bytes, images));
        }
    }
    // The directory lists ids in order; an index says so rather than trusts it.
    if out.ids.windows(2).any(|w| w[0] >= w[1]) {
        let mut pairs: Vec<(u64, Stamp)> = out.ids.into_iter().zip(out.stamps).collect();
        pairs.sort_by_key(|p| p.0);
        pairs.dedup_by_key(|p| p.0);
        (out.ids, out.stamps) = pairs.into_iter().unzip();
    }
    Ok(out)
}

/// Writes an index beside its archive. Says how many bytes it is.
pub async fn put(store: &dyn ObjectStore, index: &ArchiveIndex) -> Result<u64, StoreError> {
    let body = index.encode();
    let len = body.len() as u64;
    store
        .put(
            &Path::from(index_key(&index.archive)),
            PutPayload::from(Bytes::from(body)),
        )
        .await?;
    Ok(len)
}

/// The index of the archive at `archive_key`, and its size in bytes; `None`
/// when it has none.
pub async fn read(
    store: &dyn ObjectStore,
    archive_key: &str,
) -> Result<Option<(ArchiveIndex, u64)>, StoreError> {
    match store.get(&Path::from(index_key(archive_key))).await {
        Ok(got) => {
            let bytes = got.bytes().await?;
            let index = ArchiveIndex::decode(&bytes)?;
            if index.archive != archive_key {
                return Err(StoreError::Corrupt(format!(
                    "{}: the index of {}",
                    index_key(archive_key),
                    index.archive
                )));
            }
            Ok(Some((index, bytes.len() as u64)))
        }
        Err(object_store::Error::NotFound { .. }) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Whether the archive at `archive_key` has an index, without reading it.
pub async fn exists(store: &dyn ObjectStore, archive_key: &str) -> Result<bool, StoreError> {
    match store.head(&Path::from(index_key(archive_key))).await {
        Ok(_) => Ok(true),
        Err(object_store::Error::NotFound { .. }) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Deletes the index of the archive at `archive_key`; one that is not there
/// is already deleted.
pub async fn delete(store: &dyn ObjectStore, archive_key: &str) -> Result<(), StoreError> {
    match store.delete(&Path::from(index_key(archive_key))).await {
        Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// What a zone's index says of one tile: see [`TileStore::indexed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup {
    /// The zone holds it; the stamp of the copy a reader would get.
    Held(Stamp),
    /// The zone, as its index was read, does not hold it.
    NotHeld,
    /// Nothing to say: the zone is not known by its index.
    Unknown,
}

/// Every tile of a zone, and the stamp of the copy a reader would get.
#[derive(Debug, Clone, Default)]
pub struct ZoneIndex {
    tiles: HashMap<u64, Stamp>,
}

impl ZoneIndex {
    /// From the indexes of a zone's live archives, **oldest first**, as a
    /// manifest lists them: on a tile several hold, the last one wins — the
    /// store's own rule.
    pub fn from_archives<'a>(oldest_first: impl IntoIterator<Item = &'a ArchiveIndex>) -> Self {
        let mut tiles = HashMap::new();
        for index in oldest_first {
            for (id, stamp) in index.ids.iter().zip(&index.stamps) {
                tiles.insert(*id, *stamp);
            }
        }
        Self { tiles }
    }

    /// The stamp of the copy of tile `id` a reader would get, if the zone
    /// holds it.
    pub fn get(&self, id: u64) -> Option<Stamp> {
        self.tiles.get(&id).copied()
    }

    pub fn len(&self) -> usize {
        self.tiles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tiles.is_empty()
    }
}

/// How [`TileStore::backfill_indexes`] goes about it.
#[derive(Debug, Clone, Copy)]
pub struct Backfill {
    /// Zones worked on at once.
    pub zones_in_flight: usize,
    /// Count what would be written, write nothing.
    pub dry_run: bool,
}

impl Default for Backfill {
    fn default() -> Self {
        Self {
            zones_in_flight: 8,
            dry_run: false,
        }
    }
}

/// What a backfill did, or would do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackfillReport {
    pub zones: u64,
    /// Archives the zones' manifests list.
    pub archives: u64,
    /// …of which already had an index, and were left alone.
    pub already: u64,
    /// …of which got one (or, dry, would have).
    pub written: u64,
    /// …of which vanished before they could be read (compacted, expired).
    pub vanished: u64,
    /// Archive bytes downloaded to index them (dry: that would be).
    pub archive_bytes: u64,
    /// Index bytes written.
    pub index_bytes: u64,
    /// Zones that failed, with why; the rest went on.
    pub failed: Vec<String>,
}

impl BackfillReport {
    fn add(&mut self, other: BackfillReport) {
        self.zones += other.zones;
        self.archives += other.archives;
        self.already += other.already;
        self.written += other.written;
        self.vanished += other.vanished;
        self.archive_bytes += other.archive_bytes;
        self.index_bytes += other.index_bytes;
        self.failed.extend(other.failed);
    }
}

impl TileStore {
    /// Writes the index of every archive of `layer` that has none: the
    /// archives published before the layer kept indexes. Idempotent — an
    /// archive with an index is left alone — so an interrupted backfill is
    /// resumed by running it again. Retired archives are not indexed: they
    /// are on their way out.
    ///
    /// Refused for a layer whose catalog entry does not keep indexes: its
    /// next archives would come without one, and the work would never be
    /// done.
    pub async fn backfill_indexes(
        &self,
        layer: &str,
        how: Backfill,
    ) -> Result<BackfillReport, StoreError> {
        use futures_util::StreamExt;
        let l = self.layer(layer)?.clone();
        if !l.index {
            return Err(StoreError::Corrupt(format!(
                "layer {layer} keeps no index: set `index` in its catalog entry first"
            )));
        }
        let zones = self.zones_of(&l).await?;
        let reports: Vec<BackfillReport> = futures_util::stream::iter(zones)
            .map(|zone| {
                let l = &l;
                async move {
                    match self.backfill_zone(l, zone, how.dry_run).await {
                        Ok(r) => r,
                        Err(e) => {
                            let why = format!("{}: {e}", l.zone_prefix(zone));
                            tracing::warn!(%why, "INDEX-BACKFILL-FAILED");
                            BackfillReport {
                                zones: 1,
                                failed: vec![why],
                                ..BackfillReport::default()
                            }
                        }
                    }
                }
            })
            .buffer_unordered(how.zones_in_flight.max(1))
            .collect()
            .await;
        let mut total = BackfillReport::default();
        for r in reports {
            total.add(r);
        }
        Ok(total)
    }

    async fn backfill_zone(
        &self,
        layer: &Layer,
        zone: crate::layer::Zone,
        dry_run: bool,
    ) -> Result<BackfillReport, StoreError> {
        let m = crate::manifest::read(self.object_store().as_ref(), &layer.zone_prefix(zone))
            .await?
            .manifest;
        let mut report = BackfillReport {
            zones: 1,
            ..BackfillReport::default()
        };
        for a in &m.archives {
            report.archives += 1;
            if exists(self.object_store().as_ref(), &a.key).await? {
                report.already += 1;
                continue;
            }
            if dry_run {
                report.written += 1;
                report.archive_bytes += a.bytes;
                continue;
            }
            let dir = tempfile::tempdir()?;
            let local = dir.path().join("archive.pmtiles");
            match crate::store::download(self.object_store().as_ref(), &a.key, &local).await {
                Ok(()) => {}
                Err(StoreError::ObjectStore(object_store::Error::NotFound { .. })) => {
                    report.vanished += 1;
                    continue;
                }
                Err(e) => return Err(e),
            }
            report.archive_bytes += std::fs::metadata(&local)?.len();
            let built = of_local_archive(&local, layer, &a.key).await?;
            report.index_bytes += put(self.object_store().as_ref(), &built).await?;
            report.written += 1;
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ArchiveIndex {
        ArchiveIndex {
            layer: "imagery".into(),
            archive: "imagery/zones/z10/1/2/d-000-abc.pmtiles".into(),
            ids: vec![3, 7, 1_000_000_000_000],
            stamps: vec![
                Stamp {
                    digest: 1,
                    width: 256,
                    height: 256,
                },
                Stamp {
                    digest: u64::MAX,
                    width: 512,
                    height: 256,
                },
                Stamp {
                    digest: 42,
                    width: 0,
                    height: 0,
                },
            ],
        }
    }

    #[test]
    fn an_index_round_trips() {
        let index = sample();
        let bytes = index.encode();
        assert_eq!(&bytes[4..8], b"TIDX", "the file identifier");
        let back = ArchiveIndex::decode(&bytes).expect("decode");
        assert_eq!(back, index);
        assert_eq!(back.get(7).map(|s| s.digest), Some(u64::MAX));
        assert_eq!(back.get(8), None);
        assert_eq!(back.get(1_000_000_000_000).map(|s| s.digest), Some(42));
        // Twenty bytes a tile, give or take the header.
        let many = ArchiveIndex {
            ids: (0..10_000).collect(),
            stamps: vec![
                Stamp {
                    digest: 9,
                    width: 256,
                    height: 256
                };
                10_000
            ],
            ..sample()
        };
        let len = many.encode().len();
        assert!((200_000..200_400).contains(&len), "{len} bytes");
    }

    #[test]
    fn an_index_that_is_not_one_is_refused() {
        assert!(ArchiveIndex::decode(b"").is_err());
        assert!(ArchiveIndex::decode(&[0u8; 64]).is_err());
        let mut out_of_order = sample();
        out_of_order.ids.swap(0, 1);
        assert!(ArchiveIndex::decode(&out_of_order.encode()).is_err());
        let mut short = sample();
        short.stamps.pop();
        // `encode` writes what it is given; `decode` refuses vectors that
        // disagree.
        let bytes = {
            let mut b = flatbuffers::FlatBufferBuilder::new();
            let ids = b.create_vector(&short.ids);
            let digests = b.create_vector(&[1u64, 2]);
            let root = fb::TileIndex::create(
                &mut b,
                &fb::TileIndexArgs {
                    version: INDEX_VERSION,
                    tiles: 3,
                    ids: Some(ids),
                    digests: Some(digests),
                    ..Default::default()
                },
            );
            fb::finish_tile_index_buffer(&mut b, root);
            b.finished_data().to_vec()
        };
        assert!(ArchiveIndex::decode(&bytes).is_err());
    }

    #[test]
    fn the_digest_is_fnv_1a() {
        assert_eq!(digest(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(digest(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(digest(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn the_newest_archive_wins_a_tile() {
        let stamp = |digest| Stamp {
            digest,
            width: 1,
            height: 1,
        };
        let old = ArchiveIndex {
            ids: vec![1, 2],
            stamps: vec![stamp(10), stamp(20)],
            ..ArchiveIndex::default()
        };
        let new = ArchiveIndex {
            ids: vec![2, 3],
            stamps: vec![stamp(21), stamp(30)],
            ..ArchiveIndex::default()
        };
        let zone = ZoneIndex::from_archives([&old, &new]);
        assert_eq!(zone.len(), 3);
        assert_eq!(zone.get(1).map(|s| s.digest), Some(10));
        assert_eq!(zone.get(2).map(|s| s.digest), Some(21), "the newest copy");
        assert_eq!(zone.get(3).map(|s| s.digest), Some(30));
        assert_eq!(zone.get(4), None);
    }

    #[test]
    fn an_image_s_size_is_read_from_its_header() {
        let mut png = Vec::new();
        image::RgbaImage::new(256, 128)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("png");
        let s = Stamp::of(&png, true);
        assert_eq!((s.width, s.height), (256, 128));
        assert_eq!(s.digest, digest(&png));
        assert!(!Stamp::of(&png, false).has_size());
        assert!(!Stamp::of(b"\0", true).has_size());
    }
}
