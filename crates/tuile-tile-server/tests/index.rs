// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Archive indexes: written with their archive, before its publication;
//! deleted with it; read by a projection that then downloads no archive.

mod common;

use std::sync::Arc;

use bytes::Bytes;
use common::*;
use futures_util::TryStreamExt;
use object_store::ObjectStore;
use tuile_core::storage::{ContentStore, Stamp, ABSENT};
use tuile_tile_server::index::{self, Backfill, Lookup};
use tuile_tile_server::projection::{bounds, DEFAULT_TILE_FACTOR};
use tuile_tile_server::{
    catalog, manifest, Compaction, Eye, Footprint, Grid, Layer, Scrub, StoreContent, TileStore,
    TileType,
};

/// The imagery layer, its archives indexed, its tiles PNGs.
fn indexed() -> Layer {
    Layer {
        index: true,
        tile_type: TileType::Png,
        ..imagery()
    }
}

fn store_with(objects: Arc<dyn ObjectStore>, clock: &TestClock, layer: Layer) -> TileStore {
    let absent = catalog::absence_of(&layer);
    TileStore::with_clock(objects, vec![layer, absent], eager(), clock.clock())
}

/// A small PNG, different for every `(seed, version)`, `side` pixels wide.
fn png(seed: u32, version: u32, side: u32) -> Bytes {
    let mut img = image::RgbaImage::new(side, side / 2);
    for (i, p) in img.pixels_mut().enumerate() {
        let v = seed
            .wrapping_mul(2_654_435_761)
            .wrapping_add(version * 7919 + i as u32);
        *p = image::Rgba([v as u8, (v >> 8) as u8, (v >> 16) as u8, 255]);
    }
    let mut out = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .expect("png");
    Bytes::from(out)
}

async fn keys(objects: &dyn ObjectStore, suffix: &str) -> Vec<String> {
    let listed: Vec<object_store::ObjectMeta> =
        objects.list(None).try_collect().await.expect("list");
    let mut keys: Vec<String> = listed
        .into_iter()
        .map(|m| m.location.to_string())
        .filter(|k| k.ends_with(suffix))
        .collect();
    keys.sort();
    keys
}

fn eye_over_zone() -> Eye {
    let [w, s, e, n] = bounds(Grid::WebMercator, LEVEL, X0, Y0);
    Eye {
        lon: (w + e) / 2.0,
        lat: (s + n) / 2.0,
        height: 5_800.0,
    }
}

fn by_index() -> Footprint {
    Footprint::from_eyes([eye_over_zone()], DEFAULT_TILE_FACTOR).with_layer_by_index(IMAGERY)
}

/// Every archive the zone's manifest lists has an index, and every index
/// names an archive the manifest lists — or one retired, not yet deleted.
async fn indexes_match_archives(objects: &dyn ObjectStore) {
    let m = manifest::read(objects, &indexed().zone_prefix(zone()))
        .await
        .expect("manifest")
        .manifest;
    let named: Vec<String> = m
        .archives
        .iter()
        .map(|a| a.key.clone())
        .chain(m.retired.iter().map(|r| r.key.clone()))
        .collect();
    for a in &m.archives {
        assert!(
            index::exists(objects, &a.key).await.expect("head"),
            "{} has no index",
            a.key
        );
    }
    let under = format!("{}/", indexed().zone_prefix(zone()));
    for k in keys(objects, ".index").await {
        if !k.starts_with(&under) {
            continue;
        }
        let archive = index::archive_of_index(&k).expect("an index key");
        assert!(
            named.iter().any(|n| n == archive),
            "{k} is the index of no archive"
        );
    }
}

#[tokio::test]
async fn an_archive_s_index_is_written_before_its_publication() {
    let clock = TestClock::new();
    let objects = memory();
    let s = store_with(objects.clone(), &clock, indexed());
    let mut tiles = Vec::new();
    for i in 0..5 {
        let (x, y) = in_zone(i);
        let bytes = png(i, 0, 256);
        s.put(IMAGERY, LEVEL, x, y, bytes.clone())
            .await
            .expect("put");
        tiles.push((x, y, bytes));
    }
    let gate = s.pause_next_publication();
    let prefix = indexed().zone_prefix(zone());
    let (flushed, ()) = tokio::join!(s.flush_all(), async {
        gate.reached.notified().await;
        // Uploaded, not published: the manifest does not name it yet, and its
        // index is already there.
        let m = manifest::read(objects.as_ref(), &prefix)
            .await
            .expect("manifest")
            .manifest;
        assert!(m.archives.is_empty(), "not published yet");
        let uploaded = archives(objects.as_ref(), &prefix).await;
        assert_eq!(uploaded.len(), 1);
        let (got, _) = index::read(objects.as_ref(), &uploaded[0])
            .await
            .expect("read")
            .expect("the index exists before the publication");
        assert_eq!(got.archive, uploaded[0]);
        assert_eq!(got.layer, IMAGERY);
        assert_eq!(got.len(), tiles.len());
        for (x, y, bytes) in &tiles {
            let id = Grid::WebMercator.to_id(LEVEL, *x, *y).expect("id");
            let stamp = got.get(id).expect("indexed");
            assert_eq!(stamp.digest, index::digest(bytes));
            assert_eq!((stamp.width, stamp.height), (256, 128));
        }
        gate.go.notify_one();
    });
    flushed.expect("flush");
    indexes_match_archives(objects.as_ref()).await;
}

#[tokio::test]
async fn a_layer_that_keeps_no_index_gets_none() {
    let clock = TestClock::new();
    let objects = memory();
    let s = store_on(objects.clone(), &clock, eager());
    s.put(IMAGERY, LEVEL, X0, Y0, png(1, 0, 8))
        .await
        .expect("put");
    s.flush_all().await.expect("flush");
    assert_eq!(archives(objects.as_ref(), IMAGERY).await.len(), 1);
    assert!(keys(objects.as_ref(), ".index").await.is_empty());
}

#[tokio::test]
async fn compaction_and_scrub_write_the_index_of_what_they_write() {
    let clock = TestClock::new();
    let objects = memory();
    let s = store_with(objects.clone(), &clock, indexed());
    // Two deltas; tile 0 in both, the second copy newer.
    for version in 0..2 {
        for i in 0..3 + version {
            let (x, y) = in_zone(i);
            s.put(IMAGERY, LEVEL, x, y, png(i, version, 16))
                .await
                .expect("put");
        }
        s.flush_all().await.expect("flush");
    }
    assert!(matches!(
        s.compact(IMAGERY, zone()).await.expect("compact"),
        Compaction::Merged { merged: 2, .. }
    ));
    indexes_match_archives(objects.as_ref()).await;
    let m = manifest::read(objects.as_ref(), &indexed().zone_prefix(zone()))
        .await
        .expect("manifest")
        .manifest;
    let (merged, _) = index::read(objects.as_ref(), &m.archives[0].key)
        .await
        .expect("read")
        .expect("the merged archive's index");
    let (x, y) = in_zone(0);
    let id = Grid::WebMercator.to_id(LEVEL, x, y).expect("id");
    assert_eq!(
        merged.get(id).map(|s| s.digest),
        Some(index::digest(&png(0, 1, 16))),
        "the newest copy"
    );
    assert_eq!(merged.len(), 4);

    // Scrub one tile out: the rewritten archive's index does not name it.
    let unwanted = png(2, 1, 16);
    let scrubbed = s
        .scrub(IMAGERY, zone(), |b| b == unwanted.as_ref())
        .await
        .expect("scrub");
    assert_eq!(
        scrubbed,
        Scrub::Removed {
            tiles: 1,
            archives: 1
        }
    );
    indexes_match_archives(objects.as_ref()).await;
    let m = manifest::read(objects.as_ref(), &indexed().zone_prefix(zone()))
        .await
        .expect("manifest")
        .manifest;
    let (rewritten, _) = index::read(objects.as_ref(), &m.archives[0].key)
        .await
        .expect("read")
        .expect("the rewritten archive's index");
    assert_eq!(rewritten.len(), 3);
    let (x, y) = in_zone(2);
    let gone = Grid::WebMercator.to_id(LEVEL, x, y).expect("id");
    assert_eq!(rewritten.get(gone), None);
}

#[tokio::test]
async fn the_garbage_collection_deletes_an_archive_with_its_index() {
    let clock = TestClock::new();
    let objects = memory();
    let s = store_with(objects.clone(), &clock, indexed());
    for version in 0..3 {
        s.put(IMAGERY, LEVEL, X0, Y0, png(0, version, 16))
            .await
            .expect("put");
        s.flush_all().await.expect("flush");
    }
    assert_eq!(keys(objects.as_ref(), ".index").await.len(), 3);
    s.compact(IMAGERY, zone()).await.expect("compact");
    // Retired, within their grace: still there, both halves.
    assert_eq!(keys(objects.as_ref(), ".index").await.len(), 4);

    // An index whose archive never made it: an upload that failed after it.
    let orphan = format!(
        "{}/{}-0-dead.pmtiles",
        indexed().zone_prefix(zone()),
        indexed().epoch(clock.now())
    );
    index::put(
        objects.as_ref(),
        &index::ArchiveIndex {
            layer: IMAGERY.into(),
            archive: orphan.clone(),
            ..Default::default()
        },
    )
    .await
    .expect("put orphan");

    // Past every grace — and past the wall clock the in-memory bucket dates
    // its objects by, which the orphan grace is measured against.
    clock.advance(days(60));
    s.cleanup(&indexed(), zone()).await.expect("cleanup");
    let left = keys(objects.as_ref(), ".index").await;
    let pmtiles = archives(objects.as_ref(), IMAGERY).await;
    assert_eq!(pmtiles.len(), 1, "the merged archive alone");
    assert_eq!(left, vec![index::index_key(&pmtiles[0])], "and its index");
    indexes_match_archives(objects.as_ref()).await;
}

#[tokio::test]
async fn a_lost_compaction_deletes_its_archive_and_its_index() {
    let clock = TestClock::new();
    let objects = memory();
    let s = store_with(objects.clone(), &clock, indexed());
    for version in 0..2 {
        s.put(IMAGERY, LEVEL, X0, Y0, png(0, version, 16))
            .await
            .expect("put");
        s.flush_all().await.expect("flush");
    }
    let m = manifest::read(objects.as_ref(), &indexed().zone_prefix(zone()))
        .await
        .expect("manifest")
        .manifest;
    // Planned on a run that is not there any more: a compaction got in first.
    s.compact(IMAGERY, zone()).await.expect("compact");
    assert_eq!(
        s.merge_run_planned(IMAGERY, zone(), &m.archives)
            .await
            .expect("merge"),
        Compaction::Superseded
    );
    indexes_match_archives(objects.as_ref()).await;
    // Two retired and their indexes, one live and its index; nothing of the
    // lost merge.
    assert_eq!(keys(objects.as_ref(), ".index").await.len(), 3);
    assert_eq!(archives(objects.as_ref(), IMAGERY).await.len(), 3);
}

/// Publishes two deltas — tile A in both (the second newer), B in the first
/// only — and an absence for tile D.
async fn two_deltas(objects: Arc<dyn ObjectStore>, clock: &TestClock, layer: Layer) {
    let s = Arc::new(store_with(objects, clock, layer));
    let (a, b) = (in_zone(0), in_zone(1));
    s.put(IMAGERY, LEVEL, a.0, a.1, png(0, 0, 256))
        .await
        .expect("put");
    s.put(IMAGERY, LEVEL, b.0, b.1, png(1, 0, 256))
        .await
        .expect("put");
    s.flush_all().await.expect("flush");
    clock.advance(std::time::Duration::from_secs(1));
    s.put(IMAGERY, LEVEL, a.0, a.1, png(0, 1, 256))
        .await
        .expect("put");
    let d = in_zone(3);
    StoreContent::new(s.clone())
        .put(
            &format!("img/{IMAGERY}/{LEVEL}/{}/{}", d.0, d.1),
            ABSENT,
            None,
        )
        .await;
    s.flush_all().await.expect("flush");
}

#[tokio::test]
async fn a_zone_projected_by_its_indexes_downloads_no_archive_and_the_newest_wins() {
    let clock = TestClock::new();
    let objects = memory();
    two_deltas(objects.clone(), &clock, indexed()).await;

    let counting = Counting::over(objects.clone());
    let s = Arc::new(store_with(counting.clone(), &clock, indexed()));
    let dir = tempfile::tempdir().expect("dir");
    let report = s.project(&by_index(), dir.path()).await.expect("project");
    assert_eq!(counting.archive_gets(), 0, "no archive read: {report:?}");
    assert_eq!(
        report.zones_indexed, 2,
        "the layer and its absences: {report:?}"
    );
    assert_eq!(report.zones_projected, 0, "nothing projected from bytes");
    assert_eq!(report.bytes, report.index_bytes);

    let id = |i| {
        let (x, y) = in_zone(i);
        (x, y)
    };
    let held = |i| {
        let (x, y) = id(i);
        s.indexed(IMAGERY, LEVEL, x, y).expect("lookup")
    };
    let digest_of = |l: Lookup| match l {
        Lookup::Held(s) => Some(s.digest),
        _ => None,
    };
    assert_eq!(
        digest_of(held(0)),
        Some(index::digest(&png(0, 1, 256))),
        "the newest archive's copy, as a reader gets it"
    );
    assert_eq!(digest_of(held(1)), Some(index::digest(&png(1, 0, 256))));
    assert_eq!(held(2), Lookup::NotHeld);

    // As the engine's cache: what a `get` would have answered, unread.
    let content = StoreContent::new(s.clone());
    let key = |i| {
        let (x, y) = id(i);
        format!("img/{IMAGERY}/{LEVEL}/{x}/{y}")
    };
    match content.stamp(&key(0)).await {
        Stamp::Present(v) => {
            let got = content.get(&key(0)).await.expect("get");
            assert_eq!(
                v.digest,
                index::digest(&got),
                "the digest of what get returns"
            );
            assert_eq!((v.width, v.height), (256, 128));
        }
        other => unreachable!("{other:?}"),
    }
    assert_eq!(content.stamp(&key(3)).await, Stamp::Absent);
    assert_eq!(
        content.stamp(&key(2)).await,
        Stamp::Unknown,
        "neither holds it"
    );
    assert!(counting.archive_gets() > 0, "the get above read it");

    // A tile in this store's own buffer is the buffer's: unknown to the index.
    let (x, y) = id(1);
    s.put(IMAGERY, LEVEL, x, y, png(1, 9, 256))
        .await
        .expect("put");
    assert_eq!(content.stamp(&key(1)).await, Stamp::Unknown);
}

#[tokio::test]
async fn a_zone_with_an_archive_without_index_is_projected_from_its_archives() {
    let clock = TestClock::new();
    let objects = memory();
    // Published before the layer kept indexes.
    two_deltas(
        objects.clone(),
        &clock,
        Layer {
            index: false,
            ..indexed()
        },
    )
    .await;
    let counting = Counting::over(objects.clone());
    let s = store_with(counting.clone(), &clock, indexed());
    let dir = tempfile::tempdir().expect("dir");
    let report = s.project(&by_index(), dir.path()).await.expect("project");
    assert_eq!(report.zones_indexed, 0, "{report:?}");
    assert_eq!(report.zones_projected, 2, "{report:?}");
    assert!(counting.archive_gets() > 0);
    let (x, y) = in_zone(0);
    assert_eq!(
        s.indexed(IMAGERY, LEVEL, x, y).expect("lookup"),
        Lookup::Unknown
    );
    assert_eq!(
        s.get(IMAGERY, LEVEL, x, y).await.expect("get"),
        Some(png(0, 1, 256)),
        "read from the projection, as before"
    );
}

#[tokio::test]
async fn a_backfill_is_idempotent_and_then_the_zone_projects_by_index() {
    let clock = TestClock::new();
    let objects = memory();
    two_deltas(
        objects.clone(),
        &clock,
        Layer {
            index: false,
            ..indexed()
        },
    )
    .await;
    let unindexed = store_on(objects.clone(), &clock, eager());
    assert!(
        unindexed
            .backfill_indexes(IMAGERY, Backfill::default())
            .await
            .is_err(),
        "a layer that keeps no index is refused"
    );

    let s = store_with(objects.clone(), &clock, indexed());
    let dry = s
        .backfill_indexes(
            IMAGERY,
            Backfill {
                dry_run: true,
                ..Backfill::default()
            },
        )
        .await
        .expect("dry");
    assert_eq!(
        (dry.zones, dry.archives, dry.written, dry.already),
        (1, 2, 2, 0)
    );
    assert!(keys(objects.as_ref(), ".index").await.is_empty(), "dry");

    let first = s
        .backfill_indexes(IMAGERY, Backfill::default())
        .await
        .expect("backfill");
    assert_eq!((first.written, first.already), (2, 0));
    assert!(first.archive_bytes > 0 && first.index_bytes > 0);
    let written = keys(objects.as_ref(), ".index").await;
    let again = s
        .backfill_indexes(IMAGERY, Backfill::default())
        .await
        .expect("again");
    assert_eq!(
        (again.written, again.already, again.archive_bytes),
        (0, 2, 0)
    );
    assert_eq!(
        keys(objects.as_ref(), ".index").await,
        written,
        "nothing rewritten"
    );
    s.backfill_indexes(&format!("{IMAGERY}.absent"), Backfill::default())
        .await
        .expect("absences");
    indexes_match_archives(objects.as_ref()).await;

    let counting = Counting::over(objects.clone());
    let fresh = store_with(counting.clone(), &clock, indexed());
    let dir = tempfile::tempdir().expect("dir");
    let report = fresh
        .project(&by_index(), dir.path())
        .await
        .expect("project");
    assert_eq!(report.zones_indexed, 2, "{report:?}");
    assert_eq!(counting.archive_gets(), 0);
    let (x, y) = in_zone(0);
    assert_eq!(
        fresh.indexed(IMAGERY, LEVEL, x, y).expect("lookup"),
        Lookup::Held(index::Stamp::of(&png(0, 1, 256), true)),
        "a backfilled index says what a written one would"
    );
}
