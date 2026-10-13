// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The store on one writer: reading through buffer, deltas and base; misses
//! that stay misses; compaction that changes nothing a reader sees.

mod common;

use std::time::Duration;

use common::*;
use object_store::ObjectStoreExt;
use bytes::Bytes;
use tuile_tile_server::manifest::Manifest;
use tuile_tile_server::{Compaction, Due, Ledger, Scrub, Zone};
use std::sync::Arc;

#[tokio::test]
async fn a_tile_is_readable_the_moment_it_is_put() {
    let clock = TestClock::new();
    let s = store_on(memory(), &clock, eager());
    s.put(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 1)).await.expect("put");
    assert_eq!(s.get(IMAGERY, LEVEL, X0, Y0).await.expect("get"), Some(body(LEVEL, X0, Y0, 1)));
    assert_eq!(s.get(IMAGERY, LEVEL, X0, 5969).await.expect("get"), None);
}

#[tokio::test]
async fn the_newest_archive_wins_over_older_ones() {
    let clock = TestClock::new();
    let objects = memory();
    let s = store_on(objects.clone(), &clock, eager());
    s.put(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 1)).await.expect("put");
    s.put(IMAGERY, LEVEL, 8353, Y0, body(LEVEL, 8353, Y0, 1)).await.expect("put");
    s.flush_all().await.expect("flush");
    s.put(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 2)).await.expect("put");
    s.flush_all().await.expect("flush");

    let fresh = store_on(objects, &clock, eager());
    assert_eq!(fresh.get(IMAGERY, LEVEL, X0, Y0).await.expect("get"), Some(body(LEVEL, X0, Y0, 2)));
    assert_eq!(fresh.get(IMAGERY, LEVEL, 8353, Y0).await.expect("get"), Some(body(LEVEL, 8353, Y0, 1)));
}

#[tokio::test]
async fn a_vanished_archive_is_a_miss_never_an_error() {
    let clock = TestClock::new();
    let objects = memory();
    let s = store_on(objects.clone(), &clock, eager());
    s.put(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 1)).await.expect("put");
    s.flush_all().await.expect("flush");
    for key in archives(objects.as_ref(), IMAGERY).await {
        objects.delete(&key.as_str().into()).await.expect("delete");
    }
    let fresh = store_on(objects, &clock, eager());
    assert_eq!(fresh.get(IMAGERY, LEVEL, X0, Y0).await.expect("a miss, not an error"), None);
}

#[tokio::test]
async fn a_full_buffer_freezes_into_a_delta_by_itself() {
    let clock = TestClock::new();
    let objects = memory();
    let mut cfg = eager();
    cfg.flush_bytes = 4096;
    let s = store_on(objects.clone(), &clock, cfg);
    // All in one zone, so one buffer fills up.
    let mut i = 0u32;
    while archives(objects.as_ref(), IMAGERY).await.is_empty() {
        let (x, y) = in_zone(i);
        s.put(IMAGERY, LEVEL, x, y, body(LEVEL, x, y, 1)).await.expect("put");
        i += 1;
        assert!(i < ZONE_SIDE * ZONE_SIDE, "the buffer never froze");
    }
    let fresh = store_on(objects, &clock, eager());
    assert_eq!(fresh.get(IMAGERY, LEVEL, X0, Y0).await.expect("get"), Some(body(LEVEL, X0, Y0, 1)));
}

#[tokio::test]
async fn many_small_zones_past_the_global_cap_flush_the_largest() {
    let clock = TestClock::new();
    let objects = memory();
    let mut cfg = eager();
    cfg.max_buffered_bytes = 2048;
    let s = store_on(objects.clone(), &clock, cfg);
    // One tile per zone, across a row of zones: no zone ever fills up.
    let mut zone_x = ZONE_X;
    while archives(objects.as_ref(), IMAGERY).await.is_empty() {
        let x = zone_x * ZONE_SIDE;
        s.put(IMAGERY, LEVEL, x, Y0, body(LEVEL, x, Y0, 1)).await.expect("put");
        zone_x += 1;
        assert!(zone_x < ZONE_X + 200, "the global cap never flushed anything");
    }
}

#[tokio::test]
async fn an_old_buffer_is_flushed_by_flush_due() {
    let clock = TestClock::new();
    let objects = memory();
    let mut cfg = eager();
    cfg.flush_age = Duration::from_millis(20);
    let s = store_on(objects.clone(), &clock, cfg);
    s.put(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 1)).await.expect("put");
    assert_eq!(s.flush_due().await.expect("flush_due"), 0, "not old yet");
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(s.flush_due().await.expect("flush_due"), 1);
    assert_eq!(archives(objects.as_ref(), IMAGERY).await.len(), 1);
}

#[tokio::test]
async fn compaction_changes_no_byte_a_reader_sees() {
    let clock = TestClock::new();
    let objects = memory();
    let s = store_on(objects.clone(), &clock, eager());
    let mut expected = std::collections::BTreeMap::new();
    for round in 0..5u32 {
        for i in 0..200u32 {
            // A different subset of the zone every round, overlapping.
            let (x, y) = in_zone(i * 7 + round);
            let b = body(LEVEL, x, y, round);
            s.put(IMAGERY, LEVEL, x, y, b.clone()).await.expect("put");
            expected.insert((x, y), b);
        }
        s.flush_all().await.expect("flush");
    }
    let zone = zone();
    let before = archives(objects.as_ref(), IMAGERY).await.len();
    assert!(before >= 5);
    let outcome = s.compact(IMAGERY, zone).await.expect("compact");
    assert!(matches!(outcome, Compaction::Merged { merged: 5, .. }), "{outcome:?}");

    let fresh = store_on(objects.clone(), &clock, eager());
    for ((x, y), b) in &expected {
        assert_eq!(fresh.get(IMAGERY, LEVEL, *x, *y).await.expect("get").as_ref(), Some(b), "{x}/{y}");
    }
    let m = tuile_tile_server::manifest::read(objects.as_ref(), &common::imagery().zone_prefix(zone))
        .await
        .expect("manifest")
        .manifest;
    assert_eq!(m.archives.len(), 1, "five deltas became one archive");
    assert_eq!(m.retired.len(), 5, "the inputs wait out their grace");
}

#[tokio::test]
async fn compaction_never_merges_two_epochs() {
    let clock = TestClock::new();
    let objects = memory();
    let s = store_on(objects.clone(), &clock, eager());
    for v in 0..2 {
        s.put(IMAGERY, LEVEL, X0 + v, Y0, body(LEVEL, X0 + v, Y0, v)).await.expect("put");
        s.flush_all().await.expect("flush");
    }
    clock.advance(days(31)); // next month
    for v in 2..4 {
        s.put(IMAGERY, LEVEL, X0 + v, Y0, body(LEVEL, X0 + v, Y0, v)).await.expect("put");
        s.flush_all().await.expect("flush");
    }
    let zone = zone();
    assert!(matches!(s.compact(IMAGERY, zone).await.expect("compact"), Compaction::Merged { merged: 2, .. }));
    let m = tuile_tile_server::manifest::read(objects.as_ref(), &common::imagery().zone_prefix(zone))
        .await
        .expect("manifest")
        .manifest;
    let epochs: Vec<&str> = m.archives.iter().map(|a| a.epoch.as_str()).collect();
    assert_eq!(epochs, ["202609", "202609", "202610"], "last month's two archives are untouched");
}

#[tokio::test]
async fn tiles_past_their_expiry_are_no_longer_served() {
    let clock = TestClock::new();
    let objects = memory();
    let s = store_on(objects.clone(), &clock, eager());
    s.put(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 1)).await.expect("put");
    s.put(TERRAIN, 12, 4100, 2900, body(12, 4100, 2900, 1)).await.expect("put");
    s.flush_all().await.expect("flush");
    clock.advance(days(89));
    let fresh = store_on(objects.clone(), &clock, eager());
    assert!(fresh.get(IMAGERY, LEVEL, X0, Y0).await.expect("get").is_some(), "still within 90 days of the month's end");
    clock.advance(days(40));
    let fresh = store_on(objects, &clock, eager());
    assert_eq!(fresh.get(IMAGERY, LEVEL, X0, Y0).await.expect("get"), None, "expired");
    assert!(fresh.get(TERRAIN, 12, 4100, 2900).await.expect("get").is_some(), "a durable layer keeps its tiles");
}

#[tokio::test]
async fn coarse_tiles_share_the_top_zone_and_fine_ones_their_cell() {
    let l = common::imagery();
    // One level above the zone level: coarser than any zone.
    assert_eq!(l.zone_of(ZONE_LEVEL - 1, ZONE_X / 2, ZONE_Y / 2).expect("zone"), Zone::Top);
    assert_eq!(l.zone_of(ZONE_LEVEL, ZONE_X, ZONE_Y).expect("zone"), zone());
    // Seven levels down, the last row of the zone is still the zone.
    let side = 1u32 << 7;
    assert_eq!(l.zone_of(ZONE_LEVEL + 7, ZONE_X * side + 5, ZONE_Y * side + side - 1).expect("zone"), zone());
    assert!(l.zone_of(3, 1 << 3, 0).is_err(), "x one past the edge of z3");

    let clock = TestClock::new();
    let objects = memory();
    let s = store_on(objects.clone(), &clock, eager());
    s.put(IMAGERY, 2, 1, 1, body(2, 1, 1, 1)).await.expect("put");
    s.flush_all().await.expect("flush");
    assert_eq!(archives(objects.as_ref(), "imagery/top").await.len(), 1);
}

#[tokio::test]
async fn tiered_compaction_leaves_a_large_base_alone_while_small_deltas_pile_up() {
    let clock = TestClock::new();
    let objects = memory();
    let s = store_on(objects.clone(), &clock, eager());
    let prefix = common::imagery().zone_prefix(zone());
    let mut expected = std::collections::BTreeMap::new();

    // A base holding the whole zone.
    for i in 0..ZONE_SIDE * ZONE_SIDE {
        let (x, y) = in_zone(i);
        let b = body(LEVEL, x, y, 0);
        s.put(IMAGERY, LEVEL, x, y, b.clone()).await.expect("put");
        expected.insert((x, y), b);
    }
    s.flush_all().await.expect("flush");
    let base = tuile_tile_server::manifest::read(objects.as_ref(), &prefix).await.expect("manifest").manifest.archives[0]
        .key
        .clone();

    // Then many small deltas, each followed by a maintenance pass.
    const DELTAS: u32 = 24;
    const DELTA_TILES: u32 = 3;
    for d in 0..DELTAS {
        for t in 0..DELTA_TILES {
            let (x, y) = in_zone(d * DELTA_TILES + t);
            let b = body(LEVEL, x, y, d + 1);
            s.put(IMAGERY, LEVEL, x, y, b.clone()).await.expect("put");
            expected.insert((x, y), b);
        }
        s.flush_all().await.expect("flush");
        s.compact_due().await.expect("compact_due");
        let m = tuile_tile_server::manifest::read(objects.as_ref(), &prefix).await.expect("manifest").manifest;
        let sizes: Vec<u64> = m.archives.iter().map(|a| a.bytes).collect();
        assert_eq!(m.archives[0].key, base, "the base was rewritten after delta {d}: sizes {sizes:?}");
        assert!(m.archives.len() <= s.object_store_config().tiering.max_archives, "{} archives", m.archives.len());
    }
    let fresh = store_on(objects, &clock, eager());
    for ((x, y), b) in &expected {
        assert_eq!(fresh.get(IMAGERY, LEVEL, *x, *y).await.expect("get").as_ref(), Some(b), "{x}/{y}");
    }
}

#[tokio::test]
async fn a_maintenance_pass_finds_every_zone_and_drops_expired_archives() {
    let clock = TestClock::new();
    let objects = memory();
    let s = store_on(objects.clone(), &clock, eager());
    // Three zones: the test zone, its neighbour, and the top zone.
    s.put(IMAGERY, LEVEL, X0, Y0, body(LEVEL, X0, Y0, 0)).await.expect("put");
    s.put(IMAGERY, LEVEL, X0 + ZONE_SIDE, Y0, body(LEVEL, X0 + ZONE_SIDE, Y0, 0)).await.expect("put");
    s.put(IMAGERY, 2, 1, 1, body(2, 1, 1, 0)).await.expect("put");
    s.put(TERRAIN, 12, 4100, 2900, body(12, 4100, 2900, 0)).await.expect("put");
    s.flush_all().await.expect("flush");
    let mut zones = s.zones().await.expect("zones");
    zones.sort();
    assert_eq!(zones.len(), 4, "{zones:?}");

    clock.advance(days(130));
    s.compact_due().await.expect("compact_due");
    let m = tuile_tile_server::manifest::read(objects.as_ref(), &common::imagery().zone_prefix(zone()))
        .await
        .expect("manifest")
        .manifest;
    assert!(m.archives.is_empty(), "expired imagery retired");
    assert_eq!(m.retired.len(), 1);
    let terrain = tuile_tile_server::manifest::read(objects.as_ref(), &common::terrain().zone_prefix(tuile_tile_server::Zone::Cell { x: 4100 >> 3, y: 2900 >> 3 }))
        .await
        .expect("manifest")
        .manifest;
    assert_eq!(terrain.archives.len(), 1, "durable terrain kept");
}

/// The epochs of a zone's archives, oldest archive first.
async fn epochs(objects: &dyn object_store::ObjectStore) -> Vec<String> {
    let m = tuile_tile_server::manifest::read(objects, &common::imagery().zone_prefix(zone())).await.expect("manifest").manifest;
    m.archives.iter().map(|a| a.epoch.clone()).collect()
}

#[tokio::test]
async fn a_past_month_s_deltas_are_merged_among_themselves_and_never_with_another_month_s() {
    let clock = TestClock::new();
    let objects = memory();
    let s = store_on(objects.clone(), &clock, eager());
    let fanout = eager().tiering.fanout as u32;
    // What a month's last hours leave: enough small deltas for a run.
    for v in 0..fanout {
        s.put(IMAGERY, LEVEL, X0 + v, Y0, body(LEVEL, X0 + v, Y0, v)).await.expect("put");
        s.flush_all().await.expect("flush");
    }
    clock.advance(days(31)); // next month
    // And too few of the new month for a run of their own.
    for v in fanout..fanout + 2 {
        s.put(IMAGERY, LEVEL, X0 + v, Y0, body(LEVEL, X0 + v, Y0, v)).await.expect("put");
        s.flush_all().await.expect("flush");
    }
    assert_eq!(epochs(objects.as_ref()).await.len(), fanout as usize + 2);

    s.compact_due().await.expect("compact_due");
    assert_eq!(epochs(objects.as_ref()).await, ["202609", "202610", "202610"], "last month's are one, this month's untouched");
    let fresh = store_on(objects, &clock, eager());
    for v in 0..fanout + 2 {
        assert_eq!(fresh.get(IMAGERY, LEVEL, X0 + v, Y0).await.expect("get"), Some(body(LEVEL, X0 + v, Y0, v)));
    }
}

/// A picture a source served in place of tiles it does not have: the same
/// bytes wherever it was served.
const PLACEHOLDER: &[u8] = b"no imagery here";

#[tokio::test]
async fn a_scrub_takes_out_the_tiles_it_is_told_to_and_no_other() {
    let clock = TestClock::new();
    let objects = memory();
    let s = store_on(objects.clone(), &clock, eager());
    let placeholder = Bytes::from_static(PLACEHOLDER);
    let mut expected = std::collections::BTreeMap::new();

    // First archive: forty real tiles, and the placeholder at five places.
    for i in 0..40u32 {
        let (x, y) = in_zone(i);
        s.put(IMAGERY, LEVEL, x, y, body(LEVEL, x, y, 1)).await.expect("put");
        expected.insert((x, y), Some(body(LEVEL, x, y, 1)));
    }
    for i in 100..105u32 {
        let (x, y) = in_zone(i);
        s.put(IMAGERY, LEVEL, x, y, placeholder.clone()).await.expect("put");
        expected.insert((x, y), None);
    }
    s.flush_all().await.expect("flush");
    // Second archive: real tiles only — it is not to be touched.
    for i in 200..220u32 {
        let (x, y) = in_zone(i);
        s.put(IMAGERY, LEVEL, x, y, body(LEVEL, x, y, 2)).await.expect("put");
        expected.insert((x, y), Some(body(LEVEL, x, y, 2)));
    }
    s.flush_all().await.expect("flush");
    // Third archive: nothing but the placeholder — over a real tile of the
    // first archive, which is seen again once it is gone, and at new places.
    let (ox, oy) = in_zone(7);
    s.put(IMAGERY, LEVEL, ox, oy, placeholder.clone()).await.expect("put");
    for i in 300..303u32 {
        let (x, y) = in_zone(i);
        s.put(IMAGERY, LEVEL, x, y, placeholder.clone()).await.expect("put");
        expected.insert((x, y), None);
    }
    s.flush_all().await.expect("flush");

    let zone = zone();
    let prefix = common::imagery().zone_prefix(zone);
    let before = tuile_tile_server::manifest::read(objects.as_ref(), &prefix).await.expect("manifest").manifest;
    assert_eq!(before.archives.len(), 3);

    let outcome = s.scrub(IMAGERY, zone, |bytes| bytes == PLACEHOLDER).await.expect("scrub");
    assert_eq!(outcome, Scrub::Removed { tiles: 9, archives: 2 });

    // Read by a store that knows nothing of what was done: every real tile
    // as it was, no placeholder anywhere.
    let fresh = store_on(objects.clone(), &clock, eager());
    for ((x, y), wanted) in &expected {
        assert_eq!(fresh.get(IMAGERY, LEVEL, *x, *y).await.expect("get"), *wanted, "{x}/{y}");
    }
    // The same store that scrubbed sees it too, without being told.
    assert_eq!(s.get(IMAGERY, LEVEL, ox, oy).await.expect("get"), Some(body(LEVEL, ox, oy, 1)));

    let after = tuile_tile_server::manifest::read(objects.as_ref(), &prefix).await.expect("manifest").manifest;
    // The archive of placeholders alone is gone, the clean one is the very
    // same object, the mixed one was written again without them.
    assert_eq!(after.archives.len(), 2);
    assert_eq!(after.archives[1].key, before.archives[1].key);
    assert_ne!(after.archives[0].key, before.archives[0].key);
    assert_eq!(after.archives[0].tiles, 40);
    assert_eq!(after.archives[0].epoch, before.archives[0].epoch);
    assert_eq!(after.archives[0].oldest_fetch_ms, before.archives[0].oldest_fetch_ms);
    assert_eq!(after.retired.len(), 2, "what was replaced waits out its grace");

    // Nothing left to take out: nothing is written.
    assert_eq!(s.scrub(IMAGERY, zone, |bytes| bytes == PLACEHOLDER).await.expect("scrub"), Scrub::Clean);
    let again = tuile_tile_server::manifest::read(objects.as_ref(), &prefix).await.expect("manifest").manifest;
    assert_eq!(again.generation, after.generation);
}

/// What a ledger was told, in order, and what the bucket held at each step.
#[derive(Default)]
struct Told {
    said: std::sync::Mutex<Vec<String>>,
    refuse_writing: std::sync::atomic::AtomicBool,
    objects: std::sync::OnceLock<Arc<dyn object_store::ObjectStore>>,
}

#[async_trait::async_trait]
impl Ledger for Told {
    async fn writing(&self, layer: &str, _zone: Zone, key: &str) -> Result<(), String> {
        // Told before the archive exists, never after.
        let there = self.objects.get().expect("objects").head(&key.into()).await.is_ok();
        self.said.lock().expect("lock").push(format!("writing {layer} {key} there={there}"));
        if self.refuse_writing.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("the ledger is down".into());
        }
        Ok(())
    }
    async fn published(&self, layer: &str, _zone: Zone, manifest: &Manifest, due: &Due) -> Result<(), String> {
        self.said.lock().expect("lock").push(format!(
            "published {layer} g{} archives={} retired={} due={}",
            manifest.generation,
            manifest.archives.len(),
            manifest.retired.len(),
            due.at.is_some()
        ));
        Ok(())
    }
    async fn deleted(&self, layer: &str, _zone: Zone, keys: &[String]) -> Result<(), String> {
        self.said.lock().expect("lock").push(format!("deleted {layer} {}", keys.len()));
        Ok(())
    }
}

#[tokio::test]
async fn a_ledger_is_told_of_every_archive_before_it_is_written_of_every_publication_and_of_every_deletion() {
    let clock = TestClock::new();
    let objects = memory();
    let told = Arc::new(Told::default());
    let _ = told.objects.set(objects.clone());
    let s = store_on(objects.clone(), &clock, eager()).with_ledger(told.clone());

    // Two deltas, then a merge of them, then the retired pair deleted once
    // their grace is over.
    for round in 0..2u32 {
        let (x, y) = in_zone(round);
        s.put(IMAGERY, LEVEL, x, y, body(LEVEL, x, y, round)).await.expect("put");
        s.flush_all().await.expect("flush");
    }
    assert!(matches!(s.compact(IMAGERY, zone()).await.expect("compact"), Compaction::Merged { merged: 2, .. }));
    clock.advance(eager().retire_grace + Duration::from_secs(1));
    s.maintain(IMAGERY, zone()).await.expect("maintain");

    let said = told.said.lock().expect("lock").clone();
    let writing: Vec<&String> = said.iter().filter(|l| l.starts_with("writing")).collect();
    assert_eq!(writing.len(), 3, "{said:#?}");
    assert!(writing.iter().all(|l| l.ends_with("there=false")), "{said:#?}");
    assert!(said.contains(&"published imagery g1 archives=1 retired=0 due=true".to_string()), "{said:#?}");
    assert!(said.contains(&"published imagery g3 archives=1 retired=2 due=true".to_string()), "{said:#?}");
    assert!(said.iter().any(|l| l == "deleted imagery 2"), "{said:#?}");
    // Each archive was announced before the manifest that names it.
    let first_writing = said.iter().position(|l| l.starts_with("writing")).expect("writing");
    let first_published = said.iter().position(|l| l.starts_with("published")).expect("published");
    assert!(first_writing < first_published, "{said:#?}");

    // A ledger that cannot record an archive: the archive is not written.
    told.refuse_writing.store(true, std::sync::atomic::Ordering::SeqCst);
    let before = archives(objects.as_ref(), IMAGERY).await.len();
    let (x, y) = in_zone(9);
    s.put(IMAGERY, LEVEL, x, y, body(LEVEL, x, y, 9)).await.expect("put");
    assert!(s.flush_all().await.is_err(), "an unrecorded archive must not be written");
    assert_eq!(archives(objects.as_ref(), IMAGERY).await.len(), before);
}
