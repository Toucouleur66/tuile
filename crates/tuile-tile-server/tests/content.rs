// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The store as the engine's cache: the catalog in the bucket, the engine's
//! keys, absences remembered in a sibling layer.

mod common;

use std::sync::Arc;

use bytes::Bytes;
use common::*;
use tuile_core::storage::{ContentStore, ABSENT};
use tuile_tile_server::{catalog, StoreContent, TileStore};

#[tokio::test]
async fn a_store_opens_on_the_catalog_its_bucket_carries() {
    let objects = memory();
    assert!(
        TileStore::open(objects.clone(), eager()).await.is_err(),
        "no catalog, no store"
    );
    catalog::write(objects.as_ref(), &common::catalog())
        .await
        .expect("write catalog");
    let opened = TileStore::open(objects.clone(), eager())
        .await
        .expect("open");
    let imagery = opened.layer(IMAGERY).expect("listed");
    assert_eq!(imagery.zone_level, common::imagery().zone_level);
    assert_eq!(imagery.expiry, common::imagery().expiry);
    assert!(
        opened.layer("imagery.absent").is_ok(),
        "each layer has its absence sibling"
    );
    assert!(opened.layer("nope").is_err());
}

#[tokio::test]
async fn engine_keys_land_in_the_layers_and_come_back_from_another_process() {
    let objects = memory();
    catalog::write(objects.as_ref(), &common::catalog())
        .await
        .expect("catalog");
    let writer = Arc::new(
        TileStore::open(objects.clone(), eager())
            .await
            .expect("open"),
    );
    let cache = StoreContent::new(writer.clone());
    let key = format!("img/{IMAGERY}/{LEVEL}/{X0}/{Y0}");
    cache.put(&key, body(LEVEL, X0, Y0, 1), None).await;
    cache
        .put(
            &format!("mesh/{TERRAIN}/0/1/0"),
            Bytes::from_static(b"mesh"),
            None,
        )
        .await;
    assert_eq!(
        cache.get(&key).await,
        Some(body(LEVEL, X0, Y0, 1)),
        "readable before any flush"
    );
    writer.flush_all().await.expect("flush");

    let other = StoreContent::new(Arc::new(
        TileStore::open(objects, eager()).await.expect("open"),
    ));
    assert_eq!(other.get(&key).await, Some(body(LEVEL, X0, Y0, 1)));
    assert_eq!(
        other.get(&format!("mesh/{TERRAIN}/0/1/0")).await,
        Some(Bytes::from_static(b"mesh"))
    );
    assert_eq!(
        other
            .get(&format!("img/{IMAGERY}/{LEVEL}/{X0}/{}", Y0 + 1))
            .await,
        None
    );
}

#[tokio::test]
async fn an_absence_is_remembered_without_touching_the_layer_s_archives() {
    let objects = memory();
    catalog::write(objects.as_ref(), &common::catalog())
        .await
        .expect("catalog");
    let store = Arc::new(
        TileStore::open(objects.clone(), eager())
            .await
            .expect("open"),
    );
    let cache = StoreContent::new(store.clone());
    let key = format!("img/{IMAGERY}/2/1/1");
    cache.put(&key, ABSENT, None).await;
    store.flush_all().await.expect("flush");

    let fresh = StoreContent::new(Arc::new(
        TileStore::open(objects.clone(), eager())
            .await
            .expect("open"),
    ));
    let got = fresh.get(&key).await.expect("remembered");
    assert!(
        tuile_core::storage::is_absent(&got),
        "read back as the engine's absence"
    );
    // The image layer itself holds nothing for it.
    assert!(archives(objects.as_ref(), &format!("{IMAGERY}/"))
        .await
        .is_empty());
    assert_eq!(archives(objects.as_ref(), "imagery.absent/").await.len(), 1);
}

#[tokio::test]
async fn keys_the_store_does_not_know_are_misses_not_errors() {
    let objects = memory();
    catalog::write(objects.as_ref(), &common::catalog())
        .await
        .expect("catalog");
    let cache = StoreContent::new(Arc::new(
        TileStore::open(objects, eager()).await.expect("open"),
    ));
    assert_eq!(cache.get("img/unknown-layer/3/1/1").await, None);
    assert_eq!(cache.get("not-a-tile-key").await, None);
    assert_eq!(
        cache.get(&format!("img/{IMAGERY}/3/99/0")).await,
        None,
        "out of the grid"
    );
    cache
        .put("img/unknown-layer/3/1/1", Bytes::from_static(b"x"), None)
        .await; // declined, silently
}

#[tokio::test]
async fn a_read_only_cache_reads_the_store_and_declines_what_it_is_offered() {
    use tuile_core::storage::ContentStore;
    let clock = TestClock::new();
    let objects = memory();
    let writer = store_on(objects.clone(), &clock, eager());
    writer
        .put(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 1))
        .await
        .expect("put");
    writer.flush_all().await.expect("flush");

    let store = std::sync::Arc::new(store_on(objects, &clock, eager()));
    let cache = tuile_tile_server::StoreContent::read_only(store.clone());
    let key = |x: u32| format!("img/{IMAGERY}/{LEVEL}/{x}/{Y0}");
    assert_eq!(cache.get(&key(X0)).await, Some(body(LEVEL, X0, Y0, 1)));

    cache
        .put(&key(X0 + 1), body(LEVEL, X0 + 1, Y0, 1), None)
        .await;
    assert_eq!(
        cache.get(&key(X0 + 1)).await,
        None,
        "declined: not even in a buffer"
    );
    assert_eq!(
        store.flush_all().await.expect("flush"),
        0,
        "and nothing to publish"
    );
}

/// An absence store that counts what it is asked.
#[derive(Default)]
struct Counted {
    inner: tuile_tile_server::MemoryRegister,
    asked: std::sync::atomic::AtomicUsize,
}

impl Counted {
    fn asked(&self) -> usize {
        self.asked.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl tuile_tile_server::Absences for Counted {
    async fn record(
        &self,
        layer: &str,
        absences: &[tuile_tile_server::Absence],
    ) -> Result<(), String> {
        self.inner.record(layer, absences).await
    }
    async fn absent(
        &self,
        layer: &str,
        id: u64,
        now: std::time::SystemTime,
    ) -> Result<bool, String> {
        self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.absent(layer, id, now).await
    }
}

/// A tile the process holds — buffered, or in a projection — is read
/// without asking the absence store; one it does not hold asks it, before
/// the archives, as before.
#[tokio::test]
async fn a_tile_held_is_read_without_asking_the_absence_store() {
    let clock = TestClock::new();
    let objects = memory();
    // The layers and their absence siblings, as a catalog makes them.
    let on = |objects: &Arc<dyn object_store::ObjectStore>| {
        let layers = catalog::layers(&common::catalog()).expect("layers");
        TileStore::with_clock(objects.clone(), layers, eager(), clock.clock())
    };
    let writer = on(&objects);
    writer
        .put(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 1))
        .await
        .expect("put");
    writer.flush_all().await.expect("flush");

    let absences = Arc::new(Counted::default());
    let store = Arc::new(on(&objects).with_absences(absences.clone()));
    let cache = StoreContent::new(store.clone());
    let key = |x: u32| format!("img/{IMAGERY}/{LEVEL}/{x}/{Y0}");

    // Not held: the absence store first, then the bucket.
    assert_eq!(cache.get(&key(X0)).await, Some(body(LEVEL, X0, Y0, 1)));
    assert_eq!(absences.asked(), 1, "a tile not held asks");

    // Projected: held, read locally, nobody asked.
    let [w, s, e, n] =
        tuile_tile_server::projection::bounds(tuile_tile_server::Grid::WebMercator, LEVEL, X0, Y0);
    let eye = tuile_tile_server::Eye {
        lon: (w + e) / 2.0,
        lat: (s + n) / 2.0,
        height: 5_800.0,
    };
    let fp = tuile_tile_server::Footprint::from_eyes(
        [eye],
        tuile_tile_server::projection::DEFAULT_TILE_FACTOR,
    );
    let dir = tempfile::tempdir().expect("dir");
    store.project(&fp, dir.path()).await.expect("project");
    let before = store.stats();
    assert_eq!(cache.get(&key(X0)).await, Some(body(LEVEL, X0, Y0, 1)));
    assert_eq!(
        store.stats().projection,
        before.projection + 1,
        "read from the projection"
    );
    assert_eq!(absences.asked(), 1, "a projected tile asks nothing");

    // Buffered: held, nobody asked.
    cache
        .put(&key(X0 + 1), body(LEVEL, X0 + 1, Y0, 1), None)
        .await;
    assert_eq!(
        cache.get(&key(X0 + 1)).await,
        Some(body(LEVEL, X0 + 1, Y0, 1))
    );
    assert_eq!(absences.asked(), 1, "a buffered tile asks nothing");

    // An absence of a tile not held is still answered by the store.
    cache.put(&key(X0 + 2), ABSENT, None).await;
    assert_eq!(cache.get(&key(X0 + 2)).await, Some(ABSENT));
    assert_eq!(absences.asked(), 2);
}

/// Each place a tile is read from records what it answered, under its own
/// name and the tile's kind: the absence store and the bucket for a tile not
/// held, the projection for one projected — and its miss for one it left
/// out — the buffer for one just put; the bytes of what each had.
#[tokio::test]
async fn each_place_a_tile_is_read_from_is_recorded_under_its_name() {
    let recorded = Recorded::start();
    let clock = TestClock::new();
    let objects = memory();
    let on = |objects: &Arc<dyn object_store::ObjectStore>| {
        let layers = catalog::layers(&common::catalog()).expect("layers");
        TileStore::with_clock(objects.clone(), layers, eager(), clock.clock())
    };
    let writer = on(&objects);
    let tile = body(LEVEL, X0, Y0, 1);
    writer
        .put(IMAGERY, LEVEL, X0, Y0, tile.clone())
        .await
        .expect("put");
    let mesh = Bytes::from_static(b"a terrain tile");
    writer
        .put(TERRAIN, 0, 1, 0, mesh.clone())
        .await
        .expect("put");
    writer.flush_all().await.expect("flush");

    let store = Arc::new(on(&objects).with_absences(Arc::new(Counted::default())));
    let cache = StoreContent::new(store.clone());
    let key = |x: u32| format!("img/{IMAGERY}/{LEVEL}/{x}/{Y0}");
    let fetches = |source: &str, kind: &str, outcome: &str| {
        recorded.counter(
            "tuile_tiles_fetch_total",
            &[("source", source), ("kind", kind), ("outcome", outcome)],
        )
    };
    let timed = |source: &str, kind: &str| {
        recorded.histogram(
            "tuile_tiles_fetch_seconds",
            &[("source", source), ("kind", kind)],
        )
    };
    let bytes = |source: &str, kind: &str| {
        recorded.counter(
            "tuile_tiles_fetch_bytes_total",
            &[("source", source), ("kind", kind)],
        )
    };

    // Not held: the absence store does not know it, the bucket has it.
    assert_eq!(cache.get(&key(X0)).await, Some(tile.clone()));
    assert_eq!(fetches("absences", "texture", "miss"), 1);
    assert_eq!(fetches("bucket", "texture", "hit"), 1);
    assert_eq!(bytes("bucket", "texture"), tile.len() as u64);
    assert_eq!(
        (timed("absences", "texture"), timed("bucket", "texture")),
        (1, 1)
    );

    // A terrain tile is a mesh.
    assert_eq!(
        cache.get(&format!("mesh/{TERRAIN}/0/1/0")).await,
        Some(mesh.clone())
    );
    assert_eq!(fetches("bucket", "mesh", "hit"), 1);
    assert_eq!(bytes("bucket", "mesh"), mesh.len() as u64);

    // Projected: read from the projection, nobody else asked.
    let [w, s, e, n] =
        tuile_tile_server::projection::bounds(tuile_tile_server::Grid::WebMercator, LEVEL, X0, Y0);
    let eye = tuile_tile_server::Eye {
        lon: (w + e) / 2.0,
        lat: (s + n) / 2.0,
        height: 5_800.0,
    };
    let fp = tuile_tile_server::Footprint::from_eyes(
        [eye],
        tuile_tile_server::projection::DEFAULT_TILE_FACTOR,
    );
    let dir = tempfile::tempdir().expect("dir");
    store.project(&fp, dir.path()).await.expect("project");
    assert_eq!(cache.get(&key(X0)).await, Some(tile.clone()));
    assert_eq!(fetches("projection", "texture", "hit"), 1);
    assert_eq!(bytes("projection", "texture"), tile.len() as u64);
    assert_eq!(timed("projection", "texture"), 1);
    assert_eq!(
        fetches("bucket", "texture", "hit"),
        1,
        "the bucket not asked again"
    );

    // In the projected zone but not in the projection: its miss, then the
    // absence store and the bucket, which has nothing either.
    assert_eq!(cache.get(&key(X0 + 3)).await, None);
    assert_eq!(fetches("projection", "texture", "miss"), 1);
    assert_eq!(fetches("absences", "texture", "miss"), 2);
    assert_eq!(fetches("bucket", "texture", "miss"), 1);

    // Buffered: the buffer answers.
    let put = body(LEVEL, X0 + 1, Y0, 1);
    cache.put(&key(X0 + 1), put.clone(), None).await;
    assert_eq!(cache.get(&key(X0 + 1)).await, Some(put.clone()));
    assert_eq!(fetches("buffer", "texture", "hit"), 1);
    assert_eq!(bytes("buffer", "texture"), put.len() as u64);

    // An absence: the absence store says so, and the archives are not read.
    cache.put(&key(X0 + 2), ABSENT, None).await;
    assert_eq!(cache.get(&key(X0 + 2)).await, Some(ABSENT));
    assert_eq!(fetches("absences", "texture", "absent"), 1);
    assert_eq!(fetches("bucket", "texture", "miss"), 1);
}
