// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What an archive holds, without its bytes: its tile ids and a stamp of each.
//!
//! A consumer that only **names** tiles — a bake by reference, which records
//! which tiles a frame drapes and a digest of each, and composes nothing —
//! needs three things of a zone: which tiles exist, which archive's copy a
//! reader would get, and a digest of that copy's bytes. Reading the archives
//! to learn that costs the archives: measured on one chunk of a film, fifteen
//! gigabytes of imagery downloaded to name three thousand tiles.
//!
//! So whatever writes an archive of a layer that asks for it
//! ([`Layer::index`](crate::Layer::index)) — a delta, a compaction, a scrub —
//! reads it back once, while it is still on local disk, into an
//! [`ArchiveIndex`]: its tile ids, sorted, and for each the [`digest`] of its
//! bytes exactly as the store returns them, and its image size when it is an
//! image. That index is handed to the host's register with the publication
//! that brings the archive in ([`crate::Publication::added`]); the register
//! keeps, for every tile, the copy a reader gets, and gives it back to a
//! projection ([`crate::IndexReader`]). Nothing of it is kept in the bucket.
//!
//! [`ZoneIndex`] is what a projection attaches to a zone it knows that way.

use std::collections::HashMap;
use std::path::Path as FsPath;
use std::sync::Arc;

use futures_util::TryStreamExt;

use crate::archive;
use crate::layer::Layer;
use crate::StoreError;

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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
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

/// What a zone's index says of one tile: see [`crate::TileStore::indexed`].
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

    /// From every tile of a zone and the stamp of the copy a reader gets,
    /// as a register keeps them.
    pub fn from_tiles(tiles: HashMap<u64, Stamp>) -> Self {
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

/// A zone read whole from its archives: its manifest, and the index of
/// each archive it names, in the manifest's order (oldest first). What a
/// register is rebuilt from.
#[derive(Debug, Clone, Default)]
pub struct ZoneTiles {
    pub manifest: crate::manifest::Manifest,
    /// One per archive of `manifest.archives` that could be read.
    pub archives: Vec<ArchiveIndex>,
    /// Archives the manifest names that were gone when read (compacted or
    /// expired since): the zone changed under the read, and is to be read
    /// again.
    pub vanished: Vec<String>,
    /// Archive bytes downloaded.
    pub bytes: u64,
}

impl crate::TileStore {
    /// Reads `layer`'s `zone` whole from its archives — each downloaded once
    /// to local disk, `in_flight` at a time, every tile digested and its
    /// image size read — into what a register keeps of it. Nothing is
    /// written anywhere.
    pub async fn zone_tiles(
        &self,
        layer: &str,
        zone: crate::Zone,
        in_flight: usize,
    ) -> Result<ZoneTiles, StoreError> {
        use futures_util::StreamExt;
        let l = self.layer(layer)?.clone();
        let manifest = crate::manifest::read(self.object_store().as_ref(), &l.zone_prefix(zone))
            .await?
            .manifest;
        let read: Vec<Result<Option<(ArchiveIndex, u64)>, StoreError>> =
            futures_util::stream::iter(manifest.archives.iter())
                .map(|a| {
                    let l = &l;
                    async move {
                        let dir = tempfile::tempdir()?;
                        let local = dir.path().join("archive.pmtiles");
                        match crate::store::download(self.object_store().as_ref(), &a.key, &local)
                            .await
                        {
                            Ok(()) => {}
                            Err(StoreError::ObjectStore(object_store::Error::NotFound {
                                ..
                            })) => return Ok(None),
                            Err(e) => return Err(e),
                        }
                        let bytes = std::fs::metadata(&local)?.len();
                        Ok(Some((of_local_archive(&local, l, &a.key).await?, bytes)))
                    }
                })
                .buffered(in_flight.max(1))
                .collect()
                .await;
        let mut out = ZoneTiles {
            manifest: manifest.clone(),
            ..ZoneTiles::default()
        };
        for (a, r) in manifest.archives.iter().zip(read) {
            match r? {
                Some((index, bytes)) => {
                    out.bytes += bytes;
                    out.archives.push(index);
                }
                None => out.vanished.push(a.key.clone()),
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
