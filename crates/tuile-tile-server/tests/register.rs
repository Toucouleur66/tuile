// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A register kept by the ledger's calls alone answers a projection by index
//! as the archives do: the same tiles, the same copies a reader gets,
//! nothing listed and no archive read.

mod common;

use std::ops::RangeInclusive;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use common::*;
use object_store::{ObjectStore, ObjectStoreExt};
use tuile_tile_server::index::{self, Lookup, Stamp};
use tuile_tile_server::projection::{bounds, DEFAULT_TILE_FACTOR};
use tuile_tile_server::{
    catalog, Compaction, Eye, Footprint, Grid, IndexReader, KnownZone, Layer, Ledger,
    MemoryRegister, Publication, Scrub, TileBatches, TileStore, TileType, Zone,
};

fn indexed() -> Layer {
    Layer {
        index: true,
        tile_type: TileType::Png,
        ..imagery()
    }
}

fn layers() -> Vec<Layer> {
    let l = indexed();
    let absent = catalog::absence_of(&l);
    vec![l, absent]
}

fn png(seed: u32, version: u32) -> Bytes {
    let mut img = image::RgbaImage::new(64, 32);
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

fn by_index() -> Footprint {
    let [w, s, e, n] = bounds(Grid::WebMercator, LEVEL, X0, Y0);
    Footprint::from_eyes(
        [Eye {
            lon: (w + e) / 2.0,
            lat: (s + n) / 2.0,
            height: 5_800.0,
        }],
        DEFAULT_TILE_FACTOR,
    )
    .with_layer_by_index(IMAGERY)
}

/// The reference register, which can be told to refuse, to fail, and
/// records what it was told.
#[derive(Default)]
struct Mem {
    inner: MemoryRegister,
    said: Mutex<Vec<String>>,
    refuse_deleting: std::sync::atomic::AtomicBool,
    fail_reads: std::sync::atomic::AtomicBool,
}

impl Mem {
    fn said(&self) -> Vec<String> {
        self.said.lock().expect("said").clone()
    }
    fn say(&self, what: String) {
        self.said.lock().expect("said").push(what);
    }
}

#[async_trait::async_trait]
impl Ledger for Mem {
    async fn writing(&self, layer: &str, zone: Zone, key: &str) -> Result<(), String> {
        self.say(format!("writing {key}"));
        self.inner.writing(layer, zone, key).await
    }
    async fn published(&self, p: &Publication<'_>) -> Result<(), String> {
        self.say(format!(
            "published g{} added={} replaced={} uncovering={}",
            p.manifest.generation,
            p.added.map_or(0, |a| a.ids.len()),
            p.replaced.len(),
            p.uncovering
        ));
        self.inner.published(p).await
    }
    async fn deleting(&self, layer: &str, zone: Zone, keys: &[String]) -> Result<(), String> {
        self.say(format!("deleting {}", keys.len()));
        if self
            .refuse_deleting
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err("still pointed at".into());
        }
        self.inner.deleting(layer, zone, keys).await
    }
    async fn deleted(&self, layer: &str, zone: Zone, keys: &[String]) -> Result<(), String> {
        self.say(format!("deleted {}", keys.len()));
        self.inner.deleted(layer, zone, keys).await
    }
}

#[async_trait::async_trait]
impl tuile_tile_server::Absences for Mem {
    async fn record(
        &self,
        layer: &str,
        absences: &[tuile_tile_server::Absence],
    ) -> Result<(), String> {
        self.say(format!("absent {layer} {}", absences.len()));
        self.inner.record(layer, absences).await
    }
    async fn absent(
        &self,
        layer: &str,
        id: u64,
        now: std::time::SystemTime,
    ) -> Result<bool, String> {
        self.inner.absent(layer, id, now).await
    }
}

#[async_trait::async_trait]
impl IndexReader for Mem {
    async fn zones(&self, layer: &str) -> Result<Option<Vec<KnownZone>>, String> {
        if self.fail_reads.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("the register is down".into());
        }
        self.inner.zones(layer).await
    }
    fn tiles<'a>(&'a self, layer: &'a str, ranges: &'a [RangeInclusive<u64>]) -> TileBatches<'a> {
        self.inner.tiles(layer, ranges)
    }
}

/// Every tile `0..n` of the test zone, as the store's index says it.
fn lookups(s: &TileStore, n: u32) -> Vec<(Lookup, Lookup)> {
    (0..n)
        .map(|i| {
            let (x, y) = in_zone(i);
            (
                s.indexed(IMAGERY, LEVEL, x, y).expect("lookup"),
                s.indexed(&format!("{IMAGERY}.absent"), LEVEL, x, y)
                    .expect("lookup"),
            )
        })
        .collect()
}

/// The same, as the archives say it: what a reader of the store gets.
async fn truth(
    objects: Arc<dyn ObjectStore>,
    clock: &TestClock,
    mem: &Arc<Mem>,
    n: u32,
) -> Vec<(Lookup, Lookup)> {
    let s =
        TileStore::with_clock(objects, layers(), eager(), clock.clock()).with_absences(mem.clone());
    got(&s, n).await
}

/// What a reader of `s` gets of tiles `0..n`, stamped.
async fn got(s: &TileStore, n: u32) -> Vec<(Lookup, Lookup)> {
    let mut out = Vec::new();
    for i in 0..n {
        let (x, y) = in_zone(i);
        let mut pair = [Lookup::NotHeld, Lookup::NotHeld];
        for (k, layer) in layers().iter().enumerate() {
            if let Some(bytes) = s.get(&layer.name, LEVEL, x, y).await.expect("get") {
                // An absence carries no stamp.
                pair[k] = Lookup::Held(if k == 1 {
                    Stamp::default()
                } else {
                    Stamp::of(&bytes, index::holds_images(layer))
                });
            }
        }
        // With an absence store, an absence hides an older copy, as `get`
        // reads it.
        if s.absences_first() && matches!(pair[1], Lookup::Held(_)) {
            pair[0] = Lookup::NotHeld;
        }
        out.push((pair[0], pair[1]));
    }
    out
}

/// A history with every kind of publication: deltas over each other, a
/// merge, a delta after it, absences.
async fn history(s: &TileStore) {
    for (round, tiles) in [(0u32, 0..6u32), (1, 3..9), (2, 5..7)] {
        for i in tiles {
            let (x, y) = in_zone(i);
            s.put(IMAGERY, LEVEL, x, y, png(i, round))
                .await
                .expect("put");
        }
        s.flush_all().await.expect("flush");
    }
    assert!(matches!(
        s.compact(IMAGERY, zone()).await.expect("compact"),
        Compaction::Merged { merged: 3, .. }
    ));
    for i in [1u32, 8, 11] {
        let (x, y) = in_zone(i);
        s.put(IMAGERY, LEVEL, x, y, png(i, 7)).await.expect("put");
    }
    let (x, y) = in_zone(20);
    s.put(
        &format!("{IMAGERY}.absent"),
        LEVEL,
        x,
        y,
        Bytes::from_static(b"\0"),
    )
    .await
    .expect("put");
    s.flush_all().await.expect("flush");
    // A merge published after a later delta: the delta's copy of tile 1
    // still wins over the merge's.
    let (x, y) = in_zone(1);
    s.put(IMAGERY, LEVEL, x, y, png(1, 8)).await.expect("put");
    s.flush_all().await.expect("flush");
    let m = tuile_tile_server::manifest::read(
        s.object_store().as_ref(),
        &indexed().zone_prefix(zone()),
    )
    .await
    .expect("manifest")
    .manifest;
    let run = &m.archives[..m.archives.len() - 1];
    assert_eq!(run.len(), 2);
    assert!(matches!(
        s.merge_run_planned(IMAGERY, zone(), run)
            .await
            .expect("merge"),
        Compaction::Merged { merged: 2, .. }
    ));
}

#[tokio::test]
async fn a_publication_tells_the_tiles_it_adds_and_the_archives_it_replaces() {
    let clock = TestClock::new();
    let objects = memory();
    let mem = Arc::new(Mem {
        inner: MemoryRegister::new().with_batch(3),
        ..Mem::default()
    });
    let s = TileStore::with_clock(objects, layers(), eager(), clock.clock())
        .with_ledger(mem.clone())
        .with_absences(mem.clone());
    history(&s).await;
    let said = mem.said();
    // Zones flush concurrently: the order across zones is not the point.
    let mut published: Vec<&str> = said
        .iter()
        .filter(|l| l.starts_with("published"))
        .map(String::as_str)
        .collect();
    published.sort_unstable();
    assert_eq!(
        published,
        [
            "published g1 added=6 replaced=0 uncovering=false",
            "published g2 added=6 replaced=0 uncovering=false",
            "published g3 added=2 replaced=0 uncovering=false",
            // The merge: one archive of every tile, three replaced.
            "published g4 added=9 replaced=3 uncovering=false",
            "published g5 added=3 replaced=0 uncovering=false",
            "published g6 added=1 replaced=0 uncovering=false",
            // The planned merge, the delta after it.
            "published g7 added=10 replaced=2 uncovering=false",
        ],
        "{said:#?}"
    );
}

/// What the archives say of tiles `0..n`, and what a store that projects
/// `objects` by the register says; the register's projection must read no
/// archive. And the counts of what it did read.
async fn both_ways(
    objects: Arc<dyn ObjectStore>,
    clock: &TestClock,
    mem: Arc<Mem>,
    n: u32,
) -> (Vec<(Lookup, Lookup)>, Vec<(Lookup, Lookup)>, Arc<Counting>) {
    let (truth, registered, counting, _) = both_ways_store(objects, clock, mem, n).await;
    (truth, registered, counting)
}

/// The same, and the store that projected.
async fn both_ways_store(
    objects: Arc<dyn ObjectStore>,
    clock: &TestClock,
    mem: Arc<Mem>,
    n: u32,
) -> (
    Vec<(Lookup, Lookup)>,
    Vec<(Lookup, Lookup)>,
    Arc<Counting>,
    TileStore,
) {
    let truth = truth(objects.clone(), clock, &mem, n).await;
    let counting = Counting::over(objects);
    let registered = TileStore::with_clock(counting.clone(), layers(), eager(), clock.clock())
        .with_index_reader(mem.clone())
        .with_absences(mem);
    let dir = tempfile::tempdir().expect("dir");
    registered
        .project(&by_index(), dir.path())
        .await
        .expect("project");
    (truth, lookups(&registered, n), counting, registered)
}

#[tokio::test]
async fn a_register_kept_by_publications_answers_as_the_indexes_do() {
    let clock = TestClock::new();
    let objects = memory();
    let mem = Arc::new(Mem {
        inner: MemoryRegister::new().with_batch(3),
        ..Mem::default()
    });
    let s = TileStore::with_clock(objects.clone(), layers(), eager(), clock.clock())
        .with_ledger(mem.clone())
        .with_absences(mem.clone());
    history(&s).await;

    let (plain, registered, counting) = both_ways(objects, &clock, mem.clone(), 24).await;
    assert_eq!(registered, plain);
    assert!(
        plain
            .iter()
            .filter(|(l, _)| matches!(l, Lookup::Held(_)))
            .count()
            >= 10,
        "the history holds tiles: {plain:?}"
    );
    assert_eq!(
        registered[20].1, plain[20].1,
        "absences, by the absent layer's register"
    );
    assert!(matches!(registered[20].1, Lookup::Held(_)));
    assert_eq!(counting.lists(), 0, "no bucket listed");
    assert_eq!(counting.archive_gets(), 0, "no archive read");
    // The newest copy, as a reader gets it.
    let (x, y) = in_zone(5);
    assert_eq!(
        registered[5].0,
        Lookup::Held(Stamp::of(&png(5, 2), true)),
        "{x}/{y}"
    );
}

#[tokio::test]
async fn a_replayed_or_older_publication_changes_nothing() {
    let clock = TestClock::new();
    let objects = memory();
    let mem = Arc::new(Mem {
        inner: MemoryRegister::new().with_batch(3),
        ..Mem::default()
    });
    let s = TileStore::with_clock(objects.clone(), layers(), eager(), clock.clock())
        .with_ledger(mem.clone())
        .with_absences(mem.clone());
    history(&s).await;
    let before: Vec<(u64, u64, String)> = mem
        .inner
        .book()
        .tiles
        .iter()
        .map(|((_, id), r)| (*id, r.stamp.digest, r.archive.clone()))
        .collect();
    // Every publication told again, oldest first, as a ledger replaying a
    // log would: each is older than what the register has seen.
    let m = tuile_tile_server::manifest::read(objects.as_ref(), &indexed().zone_prefix(zone()))
        .await
        .expect("manifest")
        .manifest;
    for a in &m.archives {
        let idx = archive_index(objects.clone(), &a.key).await;
        let mut older = m.clone();
        older.generation = 1;
        let due = tuile_tile_server::Due::default();
        mem.published(&Publication {
            layer: IMAGERY,
            zone: zone(),
            manifest: &older,
            due: &due,
            added: Some(&idx),
            replaced: &[],
            uncovering: false,
        })
        .await
        .expect("told");
        // And the same, as new as the last.
        mem.published(&Publication {
            layer: IMAGERY,
            zone: zone(),
            manifest: &m,
            due: &due,
            added: Some(&idx),
            replaced: &[],
            uncovering: false,
        })
        .await
        .expect("told");
    }
    let after: Vec<(u64, u64, String)> = mem
        .inner
        .book()
        .tiles
        .iter()
        .map(|((_, id), r)| (*id, r.stamp.digest, r.archive.clone()))
        .collect();
    assert_eq!(after, before);
}

#[tokio::test]
async fn a_scrub_that_uncovers_an_older_copy_leaves_the_zone_to_its_archives() {
    let clock = TestClock::new();
    let objects = memory();
    let mem = Arc::new(Mem {
        inner: MemoryRegister::new().with_batch(3),
        ..Mem::default()
    });
    let s = TileStore::with_clock(objects.clone(), layers(), eager(), clock.clock())
        .with_ledger(mem.clone())
        .with_absences(mem.clone());
    // Tile 0 twice: an old copy, then the unwanted one over it.
    let (x, y) = in_zone(0);
    s.put(IMAGERY, LEVEL, x, y, png(0, 0)).await.expect("put");
    s.flush_all().await.expect("flush");
    let unwanted = png(0, 1);
    s.put(IMAGERY, LEVEL, x, y, unwanted.clone())
        .await
        .expect("put");
    let (x1, y1) = in_zone(1);
    s.put(IMAGERY, LEVEL, x1, y1, png(1, 0)).await.expect("put");
    s.flush_all().await.expect("flush");
    assert!(matches!(
        s.scrub(IMAGERY, zone(), |b| b == unwanted)
            .await
            .expect("scrub"),
        Scrub::Removed { tiles: 1, .. }
    ));
    let said = mem.said();
    assert!(
        said.iter().any(|l| l.ends_with("uncovering=true")),
        "{said:#?}"
    );
    let known = mem.zones(IMAGERY).await.expect("zones").expect("known");
    assert_eq!(
        known,
        vec![KnownZone {
            zone: zone(),
            indexed: false
        }]
    );
    // Projected from its archives then, and right: the old copy uncovered.
    let (truth, looked, counting, store) = both_ways_store(objects, &clock, mem, 2).await;
    assert!(
        looked
            .iter()
            .all(|l| *l == (Lookup::Unknown, Lookup::Unknown)),
        "not by index: {looked:?}"
    );
    let before = counting.archive_gets();
    assert_eq!(got(&store, 2).await, truth);
    assert_eq!(counting.archive_gets(), before, "read from its projection");
    assert_eq!(truth[0].0, Lookup::Held(Stamp::of(&png(0, 0), true)));
    assert!(
        counting.archive_gets() > 0,
        "the zone read from its archives"
    );
    assert_eq!(counting.lists(), 0, "its zones still from the register");
}

#[tokio::test]
async fn a_register_that_keeps_an_archive_keeps_it_from_being_deleted() {
    let clock = TestClock::new();
    let objects = memory();
    let mem = Arc::new(Mem {
        inner: MemoryRegister::new().with_batch(3),
        ..Mem::default()
    });
    let s = TileStore::with_clock(objects.clone(), layers(), eager(), clock.clock())
        .with_ledger(mem.clone())
        .with_absences(mem.clone());
    history(&s).await;
    let prefix = indexed().zone_prefix(zone());
    let before = archives(objects.as_ref(), &prefix).await.len();
    mem.refuse_deleting
        .store(true, std::sync::atomic::Ordering::SeqCst);
    clock.advance(eager().retire_grace + std::time::Duration::from_secs(1));
    s.maintain(IMAGERY, zone()).await.expect("maintain");
    assert_eq!(
        archives(objects.as_ref(), &prefix).await.len(),
        before,
        "nothing deleted the register did not let go of"
    );
    let m = tuile_tile_server::manifest::read(objects.as_ref(), &prefix)
        .await
        .expect("manifest")
        .manifest;
    assert_eq!(m.retired.len(), 5, "still retired, for the next upkeep");
    mem.refuse_deleting
        .store(false, std::sync::atomic::Ordering::SeqCst);
    s.maintain(IMAGERY, zone()).await.expect("maintain");
    assert_eq!(archives(objects.as_ref(), &prefix).await.len(), before - 5);
    let said = mem.said();
    let deleting = said
        .iter()
        .rposition(|l| l == "deleting 5")
        .expect("deleting");
    let deleted = said
        .iter()
        .rposition(|l| l == "deleted 5")
        .expect("deleted");
    assert!(deleting < deleted, "{said:#?}");
}

#[tokio::test]
async fn a_register_that_fails_is_read_around() {
    let clock = TestClock::new();
    let objects = memory();
    let mem = Arc::new(Mem {
        inner: MemoryRegister::new().with_batch(3),
        ..Mem::default()
    });
    let s = TileStore::with_clock(objects.clone(), layers(), eager(), clock.clock())
        .with_ledger(mem.clone())
        .with_absences(mem.clone());
    history(&s).await;
    mem.fail_reads
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (truth, _, counting, store) = both_ways_store(objects, &clock, mem, 24).await;
    assert!(counting.lists() > 0, "listed, as without a register");
    assert_eq!(got(&store, 24).await, truth, "projected from the archives");
}

/// The index of the archive at `key`, read from the archive itself.
async fn archive_index(objects: Arc<dyn ObjectStore>, key: &str) -> index::ArchiveIndex {
    let bytes = objects
        .get(&object_store::path::Path::from(key))
        .await
        .expect("get")
        .bytes()
        .await
        .expect("bytes");
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("a.pmtiles");
    std::fs::write(&path, &bytes).expect("write");
    index::of_local_archive(&path, &indexed(), key)
        .await
        .expect("index")
}

/// A register rebuilt from the archives alone holds what the publications
/// left: rebuilt twice, the same.
#[tokio::test]
async fn a_rebuild_gives_what_the_publications_gave_and_again_the_same() {
    let clock = TestClock::new();
    let objects = memory();
    let mem = Arc::new(Mem {
        inner: MemoryRegister::new().with_batch(3),
        ..Mem::default()
    });
    let s = TileStore::with_clock(objects.clone(), layers(), eager(), clock.clock())
        .with_ledger(mem.clone())
        .with_absences(mem.clone());
    history(&s).await;
    let rebuilt = MemoryRegister::new();
    for _ in 0..2 {
        // The copies: the absences are in the absence store, not the bucket.
        let layer = indexed();
        let read = s.zone_tiles(&layer.name, zone(), 2).await.expect("read");
        assert!(read.vanished.is_empty());
        assert!(read.bytes > 0);
        rebuilt.rebuild_zone(&layer, zone(), &read.manifest, &read.archives);
        let tiles = |b: tuile_tile_server::register::Book| {
            b.tiles
                .into_iter()
                .filter(|(_, t)| !t.absent)
                .map(|(k, t)| (k, t.stamp.digest, t.archive))
                .collect::<Vec<_>>()
        };
        assert_eq!(tiles(rebuilt.book()), tiles(mem.inner.book()));
    }
}

/// As the engine's cache: a stamp answers what a `get` would have, unread.
#[tokio::test]
async fn a_stamp_from_the_register_is_the_digest_of_what_get_returns() {
    use tuile_core::storage::{ContentStore, Stamp as ValueStamp};
    let clock = TestClock::new();
    let objects = memory();
    let mem = Arc::new(Mem::default());
    let s = TileStore::with_clock(objects.clone(), layers(), eager(), clock.clock())
        .with_ledger(mem.clone())
        .with_absences(mem.clone());
    history(&s).await;
    let counting = Counting::over(objects);
    let registered = Arc::new(
        TileStore::with_clock(counting.clone(), layers(), eager(), clock.clock())
            .with_index_reader(mem.clone())
            .with_absences(mem),
    );
    let dir = tempfile::tempdir().expect("dir");
    registered
        .project(&by_index(), dir.path())
        .await
        .expect("project");
    let content = tuile_tile_server::StoreContent::new(registered.clone());
    let key = |i| {
        let (x, y) = in_zone(i);
        format!("img/{IMAGERY}/{LEVEL}/{x}/{y}")
    };
    match content.stamp(&key(5)).await {
        ValueStamp::Present(v) => {
            assert_eq!(counting.archive_gets(), 0, "unread");
            let got = content.get(&key(5)).await.expect("get");
            assert_eq!(v.digest, index::digest(&got));
            assert_eq!((v.width, v.height), (64, 32));
        }
        other => unreachable!("{other:?}"),
    }
    assert_eq!(content.stamp(&key(20)).await, ValueStamp::Absent);
    assert_eq!(
        content.stamp(&key(23)).await,
        ValueStamp::Unknown,
        "neither holds it"
    );
    // A tile in this store's own buffer is the buffer's: unknown to the index.
    let (x, y) = in_zone(5);
    registered
        .put(IMAGERY, LEVEL, x, y, png(5, 9))
        .await
        .expect("put");
    assert_eq!(content.stamp(&key(5)).await, ValueStamp::Unknown);
}

/// An archive whose epoch has expired is not read by the store, even before
/// the upkeep retires it: nor is it by the register's projection.
#[tokio::test]
async fn a_copy_in_an_expired_archive_is_not_held() {
    let clock = TestClock::new();
    let objects = memory();
    let mem = Arc::new(Mem::default());
    let s = TileStore::with_clock(objects.clone(), layers(), eager(), clock.clock())
        .with_ledger(mem.clone())
        .with_absences(mem.clone());
    let (x, y) = in_zone(0);
    s.put(IMAGERY, LEVEL, x, y, png(0, 0)).await.expect("put");
    s.flush_all().await.expect("flush");
    // An epoch later, another tile; then the first epoch's expiry passes.
    clock.advance(days(31));
    let (x1, y1) = in_zone(1);
    s.put(IMAGERY, LEVEL, x1, y1, png(1, 0)).await.expect("put");
    s.flush_all().await.expect("flush");
    clock.advance(IMAGERY_EXPIRY);
    let (truth, registered, counting) = both_ways(objects, &clock, mem, 2).await;
    assert_eq!(truth[0].0, Lookup::NotHeld, "expired, not read");
    assert!(matches!(truth[1].0, Lookup::Held(_)));
    // The absences have no zone at all: nothing to say of them.
    let layer = |v: &[(Lookup, Lookup)]| v.iter().map(|p| p.0).collect::<Vec<_>>();
    assert_eq!(layer(&registered), layer(&truth));
    assert_eq!(counting.archive_gets(), 0);
}

/// Between a copy and an absence, the newer wins — either way — and an
/// expired absence is no absence, then forgotten.
#[tokio::test]
async fn the_newer_of_a_copy_and_an_absence_wins_and_an_absence_expires() {
    use tuile_core::storage::{ContentStore, ABSENT};
    let clock = TestClock::new();
    let objects = memory();
    let mem = Arc::new(Mem::default());
    let s = Arc::new(
        TileStore::with_clock(objects.clone(), layers(), eager(), clock.clock())
            .with_ledger(mem.clone())
            .with_absences(mem.clone()),
    );
    let content = tuile_tile_server::StoreContent::new(s.clone());
    let key = |i| {
        let (x, y) = in_zone(i);
        format!("img/{IMAGERY}/{LEVEL}/{x}/{y}")
    };
    // Absent, then delivered: present.
    content.put(&key(0), ABSENT, None).await;
    clock.advance(std::time::Duration::from_secs(60));
    content.put(&key(0), png(0, 0), None).await;
    // Present, then said absent: absent.
    content.put(&key(1), png(1, 0), None).await;
    s.flush_all().await.expect("flush");
    clock.advance(std::time::Duration::from_secs(60));
    content.put(&key(1), ABSENT, None).await;
    s.flush_all().await.expect("flush");
    assert_eq!(content.get(&key(0)).await, Some(png(0, 0)));
    assert_eq!(content.get(&key(1)).await, Some(ABSENT));
    // No absence in the bucket.
    let absent_prefix = format!("{IMAGERY}.absent");
    assert!(archives(objects.as_ref(), &absent_prefix).await.is_empty());
    // The register says the same, by index.
    let (truth, registered, counting) = both_ways(objects.clone(), &clock, mem.clone(), 2).await;
    assert_eq!(registered, truth);
    assert!(matches!(registered[0], (Lookup::Held(_), Lookup::NotHeld)));
    assert!(matches!(registered[1], (Lookup::NotHeld, Lookup::Held(_))));
    assert_eq!(counting.archive_gets(), 0);
    // Told late, the older loses: an absence fetched before a copy the
    // register holds, and a copy fetched before an absence it holds.
    let t = clock.now();
    let (x2, y2) = in_zone(2);
    s.put_fetched(IMAGERY, LEVEL, x2, y2, png(2, 0), t)
        .await
        .expect("put");
    s.flush_all().await.expect("flush");
    s.put_fetched(
        &format!("{IMAGERY}.absent"),
        LEVEL,
        x2,
        y2,
        Bytes::from_static(b"\0"),
        t - std::time::Duration::from_secs(30),
    )
    .await
    .expect("late absence");
    let (x3, y3) = in_zone(3);
    s.put_fetched(
        IMAGERY,
        LEVEL,
        x3,
        y3,
        png(3, 0),
        t - std::time::Duration::from_secs(30),
    )
    .await
    .expect("put");
    s.put_fetched(
        &format!("{IMAGERY}.absent"),
        LEVEL,
        x3,
        y3,
        Bytes::from_static(b"\0"),
        t,
    )
    .await
    .expect("absence");
    s.flush_all().await.expect("late copy");
    let book = mem.inner.book();
    let row = |i| {
        let (x, y) = in_zone(i);
        let id = Grid::WebMercator.to_id(LEVEL, x, y).expect("id");
        book.tiles.get(&(IMAGERY.to_string(), id)).map(|t| t.absent)
    };
    assert_eq!(
        row(2),
        Some(false),
        "the copy, newer than the absence told after it"
    );
    assert_eq!(
        row(3),
        Some(true),
        "the absence, newer than the copy published after it"
    );
    // An absence past its layer's expiry is no absence, and is forgotten.
    clock.advance(IMAGERY_EXPIRY + std::time::Duration::from_secs(60));
    assert_eq!(
        content.get(&key(1)).await,
        Some(png(1, 0)),
        "expired: the older copy, still in the bucket, is read again"
    );
    assert_eq!(mem.inner.expire(clock.now()), 2, "tiles 1 and 3");
}
