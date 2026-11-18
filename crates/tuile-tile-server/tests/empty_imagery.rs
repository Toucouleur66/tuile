// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Imagery that stands for "no imagery" — a provider's placeholder picture, a
//! uniform black tile — is an absence: never written as a tile, and read as an
//! absence where an older writer stored it, so the ground stands on the
//! coarser tile above.

mod common;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use common::*;
use object_store::ObjectStore;
use tuile_core::storage::{is_absent, ContentStore};
use tuile_tile_server::catalog::{absence_of, layers};
use tuile_tile_server::{
    Fresh, Layer, ServiceConfig, StoreContent, TileService, TileStore, TileType, Upstream,
    UpstreamError,
};

/// Bing's "no imagery" picture as it was found stored in a JPEG layer.
const PLACEHOLDER: &[u8] =
    include_bytes!("../../tuile-bing/tests/fixtures/no-imagery-placeholder.png");
/// Bing's black tile as it was found stored, and is still being answered.
const BLACK: &[u8] = include_bytes!("../../tuile-bing/tests/fixtures/black-tile.jpg");

fn jpeg(picture: image::RgbImage) -> Bytes {
    let mut out = std::io::Cursor::new(Vec::new());
    picture
        .write_to(&mut out, image::ImageFormat::Jpeg)
        .expect("a jpeg");
    Bytes::from(out.into_inner())
}

/// A real tile, and a dark one: deep water at dusk, but water.
fn real() -> Bytes {
    jpeg(image::RgbImage::from_fn(256, 256, |x, y| {
        image::Rgb([(x ^ y) as u8, (x * 3) as u8, (y * 5) as u8])
    }))
}

fn dark() -> Bytes {
    jpeg(image::RgbImage::from_fn(256, 256, |x, y| {
        let n = ((x * 7 + y * 13) % 9) as u8;
        image::Rgb([n, n + 2, n + 5])
    }))
}

/// A black tile of other bytes than Bing's: recognised by its pixels.
fn other_black() -> Bytes {
    jpeg(image::RgbImage::from_pixel(256, 256, image::Rgb([0, 0, 0])))
}

/// The store's layers as the catalog has them, absence siblings included.
fn cataloged(objects: Arc<dyn ObjectStore>, clock: &TestClock) -> TileStore {
    TileStore::with_clock(
        objects,
        layers(&catalog()).expect("layers"),
        eager(),
        clock.clock(),
    )
}

/// A writer from before: the imagery layer's tiles not known to be images,
/// so it stores whatever it is given — as the archives in a bucket were.
fn legacy(objects: Arc<dyn ObjectStore>, clock: &TestClock) -> TileStore {
    let imagery = Layer {
        tile_type: TileType::Unknown,
        ..imagery()
    };
    let absent = absence_of(&imagery);
    TileStore::with_clock(objects, vec![imagery, absent], eager(), clock.clock())
}

#[tokio::test]
async fn a_put_of_what_stands_for_no_imagery_is_recorded_as_an_absence() {
    let clock = TestClock::new();
    let objects = memory();
    let store = cataloged(objects.clone(), &clock);
    let tiles = [
        (in_zone(0), Bytes::from_static(PLACEHOLDER)),
        (in_zone(1), Bytes::from_static(BLACK)),
        (in_zone(2), other_black()),
    ];
    for ((x, y), bytes) in &tiles {
        store
            .put(IMAGERY, LEVEL, *x, *y, bytes.clone())
            .await
            .expect("put");
    }
    let (dx, dy) = in_zone(3);
    store
        .put(IMAGERY, LEVEL, dx, dy, dark())
        .await
        .expect("put");
    store.flush_all().await.expect("flush");

    let reader = cataloged(objects.clone(), &clock);
    for ((x, y), _) in &tiles {
        assert_eq!(
            reader.get(IMAGERY, LEVEL, *x, *y).await.expect("get"),
            None,
            "{x}/{y}: never stored as a tile"
        );
        assert!(
            reader
                .get("imagery.absent", LEVEL, *x, *y)
                .await
                .expect("get")
                .is_some(),
            "{x}/{y}: stored as an absence"
        );
    }
    assert_eq!(
        reader.get(IMAGERY, LEVEL, dx, dy).await.expect("get"),
        Some(dark()),
        "a dark picture is a picture"
    );
}

#[tokio::test]
async fn a_batch_of_a_writer_keeps_what_stands_for_no_imagery_as_absences() {
    let clock = TestClock::new();
    let objects = memory();
    let store = cataloged(objects.clone(), &clock);
    let fresh = |(x, y): (u32, u32), bytes: Bytes| Fresh {
        level: LEVEL,
        x,
        y,
        bytes,
        fetched_ms: 1,
    };
    let written = store
        .publish_batch(
            IMAGERY,
            zone(),
            vec![
                fresh(in_zone(0), Bytes::from_static(PLACEHOLDER)),
                fresh(in_zone(1), Bytes::from_static(BLACK)),
                fresh(in_zone(2), real()),
            ],
        )
        .await
        .expect("published");
    assert_eq!(written, 3, "the tile, and two absences");

    let reader = cataloged(objects.clone(), &clock);
    for i in [0, 1] {
        let (x, y) = in_zone(i);
        assert_eq!(reader.get(IMAGERY, LEVEL, x, y).await.expect("get"), None);
        assert!(reader
            .get("imagery.absent", LEVEL, x, y)
            .await
            .expect("get")
            .is_some());
    }
    let (x, y) = in_zone(2);
    assert_eq!(
        reader.get(IMAGERY, LEVEL, x, y).await.expect("get"),
        Some(real())
    );
}

/// The engine's key of a tile.
fn key(level: u8, x: u32, y: u32) -> String {
    format!("img/{IMAGERY}/{level}/{x}/{y}")
}

/// What a store written before holds: the placeholder and the black tile at
/// z14, a real tile at their parent z13, and a dark one beside them.
async fn written_before(objects: &Arc<dyn ObjectStore>, clock: &TestClock) {
    let old = legacy(objects.clone(), clock);
    for (i, bytes) in [
        (0, Bytes::from_static(PLACEHOLDER)),
        (1, Bytes::from_static(BLACK)),
        (2, dark()),
    ] {
        let (x, y) = in_zone(i);
        old.put(IMAGERY, LEVEL, x, y, bytes).await.expect("put");
    }
    let (x, y) = in_zone(0);
    old.put(IMAGERY, LEVEL - 1, x / 2, y / 2, real())
        .await
        .expect("put");
    old.flush_all().await.expect("flush");
}

#[tokio::test]
async fn a_held_tile_that_stands_for_no_imagery_is_read_as_an_absence_and_its_parent_kept() {
    let clock = TestClock::new();
    let objects = memory();
    written_before(&objects, &clock).await;
    // The bytes are in the archives: nothing was purged.
    let raw = legacy(objects.clone(), &clock);
    let (x0, y0) = in_zone(0);
    assert_eq!(
        raw.get(IMAGERY, LEVEL, x0, y0).await.expect("get"),
        Some(Bytes::from_static(PLACEHOLDER))
    );

    let content = StoreContent::read_only(Arc::new(cataloged(objects.clone(), &clock)));
    for i in [0, 1] {
        let (x, y) = in_zone(i);
        let got = content.get(&key(LEVEL, x, y)).await.expect("answered");
        assert!(is_absent(&got), "{x}/{y}: read as an absence");
    }
    let (x, y) = in_zone(2);
    assert_eq!(content.get(&key(LEVEL, x, y)).await, Some(dark()));
    // Never black ground: the absence's parent is there, and is the picture.
    assert_eq!(
        content.get(&key(LEVEL - 1, x0 / 2, y0 / 2)).await,
        Some(real()),
        "the coarser tile the ground stands on"
    );
}

/// A source that counts being asked and has every tile.
struct Counted(AtomicUsize, Bytes);

#[async_trait]
impl Upstream for Counted {
    async fn fetch(&self, _: u8, _: u32, _: u32) -> Result<Option<Bytes>, UpstreamError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Some(self.1.clone()))
    }
}

fn service(store: Arc<TileStore>, up: Arc<Counted>) -> TileService {
    let mut ups: HashMap<String, Arc<dyn Upstream>> = HashMap::new();
    ups.insert(IMAGERY.into(), up);
    TileService::with_config(store, ups, ServiceConfig::default())
}

#[tokio::test]
async fn a_served_store_answers_no_tile_for_what_stands_for_no_imagery_and_asks_no_source() {
    let clock = TestClock::new();
    let objects = memory();
    written_before(&objects, &clock).await;
    let up = Arc::new(Counted(AtomicUsize::new(0), real()));
    let svc = service(Arc::new(cataloged(objects, &clock)), up.clone());
    let (x0, y0) = in_zone(0);
    let (x1, y1) = in_zone(1);
    assert!(svc
        .tile(IMAGERY, LEVEL, x0, y0)
        .await
        .expect("tile")
        .is_none());
    assert!(svc
        .tile(IMAGERY, LEVEL, x1, y1)
        .await
        .expect("tile")
        .is_none());
    assert_eq!(
        up.0.load(Ordering::SeqCst),
        0,
        "an absence held, not a miss"
    );
    let parent = svc
        .tile(IMAGERY, LEVEL - 1, x0 / 2, y0 / 2)
        .await
        .expect("tile")
        .expect("the parent is served");
    assert_eq!(parent.bytes, real());
}

#[tokio::test]
async fn a_source_s_placeholder_is_served_and_stored_as_no_tile() {
    let clock = TestClock::new();
    let objects = memory();
    let store = Arc::new(cataloged(objects.clone(), &clock));
    let up = Arc::new(Counted(
        AtomicUsize::new(0),
        Bytes::from_static(PLACEHOLDER),
    ));
    let svc = service(store.clone(), up.clone());
    let (x, y) = in_zone(4);
    assert!(svc
        .tile(IMAGERY, LEVEL, x, y)
        .await
        .expect("tile")
        .is_none());
    store.flush_all().await.expect("flush");
    let reader = cataloged(objects, &clock);
    assert_eq!(reader.get(IMAGERY, LEVEL, x, y).await.expect("get"), None);
    assert!(reader
        .get("imagery.absent", LEVEL, x, y)
        .await
        .expect("get")
        .is_some());
}
