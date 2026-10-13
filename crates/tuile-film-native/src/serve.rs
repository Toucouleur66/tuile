// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The render as a service: a request a segment, from the first word of its
//! bake to the segment in the store.
//!
//! The bake runs elsewhere. As it goes it announces the tiles of the store its
//! frames name (`tuile_bake::announce`), as JSON objects under a prefix of the
//! run store. A request to render a segment arrives as the bake begins: the
//! instance reads every tile announced, as it is announced, into the blocks it
//! keeps in memory; and when the pack is there, renders it, puts the segment
//! and its receipt in the store, and answers. The instance that read the
//! tiles is the one that renders them: no affinity to ask of anyone, and an
//! instance kept warm keeps its blocks for the next segment.
//!
//! What it keeps, it keeps in a generation of caches: one directory, the
//! sources opened over it. Where the disk is memory, as on a serverless
//! container, a cache that only grows ends the instance; so once the
//! directory is past its budget, or the generation past its age, the next
//! segment opens a new one, and the old goes with the last segment that
//! reads it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{StreamExt, TryStreamExt};
use tuile_bake::announce::Announced;
use tuile_farm::{ObjectRunStore, RunStore};
use tuile_repository::TileRepository;

use crate::engine::RenderSpec;
use crate::render::render;
use crate::setup::{film_sink, sun_at};
use crate::source::{Film, Sources};
use crate::Error;

/// A segment to render.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SegmentOrder {
    /// The pack's key in the run store; it may not be there yet.
    pub pack: String,
    /// Where its bake announces the tiles it names, if it does.
    #[serde(default)]
    pub announced: Option<String>,
    /// First and last frame.
    pub frames: (u32, u32),
    /// The segment's key in the run store; its receipt beside it, `.json`.
    pub out: String,
    #[serde(default)]
    pub rendering: RenderSpec,
    /// The sun's direction, earth-centred, for every segment of a film
    /// alike; else `rendering.sun`, placed at this segment's first frame.
    #[serde(default)]
    pub to_sun: Option<[f32; 3]>,
    /// How long to wait for the pack. Default: an hour.
    #[serde(default)]
    pub pack_within_seconds: Option<u64>,
}

/// What a segment came to.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SegmentDone {
    pub out: String,
    pub frames: u64,
    pub bytes: u64,
    /// Store tiles read ahead, from the announcements.
    pub announced_tiles: u64,
    /// Seconds waiting for the pack, then rendering.
    pub waited_seconds: f64,
    pub render_seconds: f64,
}

/// Store tiles read at once while reading ahead.
const AHEAD_AT_ONCE: usize = 48;
/// Announcements read at once.
const ANNOUNCEMENTS_AT_ONCE: usize = 4;
/// How often the announcements and the pack are looked for.
const LOOK_EVERY: Duration = Duration::from_secs(2);

/// What a render service holds for as long as it lives.
pub struct Service {
    pub runs: Arc<ObjectRunStore>,
    /// The tile store, as the bucket or the routes give it.
    pub tiles: Arc<dyn tuile_repository::Objects>,
    /// Where generations of caches are opened.
    pub cache: PathBuf,
    pub limits: Limits,
    kept: tokio::sync::Mutex<Option<Arc<Generation>>>,
    /// Renders at once on this instance; reading ahead is not counted.
    renders: tokio::sync::Semaphore,
}

/// When a generation of caches is let go, and how many renders share the
/// card.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    pub cache_bytes: u64,
    pub generation_age: Duration,
    pub renders_at_once: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            cache_bytes: 6 << 30,
            // Sources believe the store as it was when they opened.
            generation_age: Duration::from_secs(3600),
            renders_at_once: 1,
        }
    }
}

/// Sources and the directory their caches live in, gone together.
struct Generation {
    sources: Sources,
    dir: tempfile::TempDir,
    opened: Instant,
}

impl Generation {
    fn spent(&self, limits: &Limits) -> bool {
        self.opened.elapsed() > limits.generation_age
            || bytes_under(self.dir.path()) > limits.cache_bytes
    }
}

/// Bytes the files under `dir` hold. Held, not said: a cache preallocates
/// its files sparse, and a disk in memory pays only for what is written.
fn bytes_under(dir: &Path) -> u64 {
    let Ok(read) = std::fs::read_dir(dir) else {
        return 0;
    };
    read.flatten()
        .map(|entry| match entry.file_type() {
            Ok(t) if t.is_dir() => bytes_under(&entry.path()),
            Ok(_) => entry.metadata().map(|m| held(&m)).unwrap_or(0),
            Err(_) => 0,
        })
        .sum()
}

#[cfg(unix)]
fn held(file: &std::fs::Metadata) -> u64 {
    std::os::unix::fs::MetadataExt::blocks(file) * 512
}

#[cfg(not(unix))]
fn held(file: &std::fs::Metadata) -> u64 {
    file.len()
}

impl Service {
    pub fn new(
        runs: Arc<ObjectRunStore>,
        tiles: Arc<dyn tuile_repository::Objects>,
        cache: PathBuf,
        limits: Limits,
    ) -> Self {
        Self {
            runs,
            tiles,
            cache,
            limits,
            kept: tokio::sync::Mutex::new(None),
            renders: tokio::sync::Semaphore::new(limits.renders_at_once.max(1)),
        }
    }

    /// The generation a segment begins in: the one kept, unless it is
    /// spent.
    async fn generation(&self) -> Result<Arc<Generation>, Error> {
        let mut kept = self.kept.lock().await;
        if let Some(generation) = kept.as_ref().filter(|g| !g.spent(&self.limits)) {
            return Ok(generation.clone());
        }
        std::fs::create_dir_all(&self.cache)?;
        let dir = tempfile::tempdir_in(&self.cache)?;
        let sources = Sources::open(self.runs.clone(), self.tiles.clone(), dir.path()).await?;
        let generation = Arc::new(Generation {
            sources,
            dir,
            opened: Instant::now(),
        });
        *kept = Some(generation.clone());
        Ok(generation)
    }
}

impl Service {
    /// Reads every announcement under `prefix` not read yet, and the store
    /// tiles each names. Returns how many tiles were read.
    async fn read_announced(
        &self,
        sources: &Sources,
        prefix: &str,
        read: &mut HashSet<String>,
    ) -> Result<u64, Error> {
        let listed = self.runs.list(prefix).await?;
        let new: Vec<_> = listed
            .into_iter()
            .filter(|e| e.key.ends_with(".json") && !read.contains(&e.key))
            .collect();
        read.extend(new.iter().map(|e| e.key.clone()));
        // The announcements, a few at a time; the tiles of each, many.
        let counts: Vec<u64> = futures_util::stream::iter(new)
            .map(|entry| async move {
                let bytes = self.runs.get_range(&entry.key, 0..entry.size).await?;
                let announced: Announced = serde_json::from_slice(&bytes)?;
                let store = &sources.store.tiles;
                let layers = [
                    (announced.terrain_layer.as_str(), &announced.terrain),
                    (announced.imagery_layer.as_str(), &announced.imagery),
                ];
                futures_util::future::try_join_all(layers.map(|(layer, wanted)| async move {
                    store.open_zones(layer, wanted.iter().copied()).await?;
                    futures_util::stream::iter(wanted.iter().copied())
                        .map(|at| store.tile(layer, at.0, at.1, at.2))
                        .buffer_unordered(AHEAD_AT_ONCE)
                        .try_fold(0u64, |n, _| async move { Ok(n + 1) })
                        .await
                        .map_err(Error::from)
                }))
                .await
                .map(|both| both.into_iter().sum::<u64>())
            })
            .buffer_unordered(ANNOUNCEMENTS_AT_ONCE)
            .try_collect()
            .await?;
        Ok(counts.into_iter().sum())
    }

    /// Renders `order`: reads ahead what is announced until the pack is
    /// there, then renders it to the store.
    pub async fn segment(&self, order: &SegmentOrder) -> Result<SegmentDone, Error> {
        let began = Instant::now();
        let within = Duration::from_secs(order.pack_within_seconds.unwrap_or(3600));
        let generation = self.generation().await?;
        let sources = &generation.sources;
        let mut read = HashSet::new();
        let mut announced_tiles = 0;
        loop {
            if let Some(prefix) = &order.announced {
                announced_tiles += self.read_announced(sources, prefix, &mut read).await?;
            }
            if self.runs.exists(&order.pack).await? {
                break;
            }
            if began.elapsed() > within {
                return Err(
                    format!("{} did not come within {} s", order.pack, within.as_secs()).into(),
                );
            }
            tokio::time::sleep(LOOK_EVERY).await;
        }
        // What was announced between the last look and the pack.
        if let Some(prefix) = &order.announced {
            announced_tiles += self.read_announced(sources, prefix, &mut read).await?;
        }
        let rendering = order.rendering.rendering()?;
        let film = Film::of(sources.packs.objects.as_ref(), &[order.pack.as_str()]).await?;
        let mut render_order = rendering.order.clone();
        render_order.frames = Some(order.frames);
        match (order.to_sun, rendering.sun) {
            (Some(to_sun), _) => {
                render_order.look.to_sun = glam::Vec3::from_array(to_sun).normalize()
            }
            (None, Some((azimuth, elevation))) => {
                let head = &film.packs.first().ok_or("a film of no pack")?.head;
                let eye = tuile_film::Pack::open_table(head)?
                    .view_of(order.frames.0)?
                    .position;
                render_order.look.to_sun = sun_at(eye, azimuth, elevation);
            }
            (None, None) => {}
        }
        let scratch = tempfile::tempdir()?;
        let local = scratch.path().join("segment.mp4");
        let _card = self.renders.acquire().await.map_err(|e| e.to_string())?;
        let waited_seconds = began.elapsed().as_secs_f64();

        // The render holds its thread: on one of its own, its reads waited
        // for on this runtime.
        let rendered = Instant::now();
        let runtime = tokio::runtime::Handle::current();
        let (to, kept) = (local.clone(), generation.clone());
        let frames = tokio::task::spawn_blocking(move || -> Result<u32, Error> {
            let mut sink = film_sink(
                to.to_string_lossy().into_owned(),
                rendering.bitrate,
                rendering.codec.as_deref(),
            )?;
            let done = runtime.block_on(render(
                &kept.sources,
                &film,
                &render_order,
                &mut (),
                sink.as_mut(),
                &mut (),
            ))?;
            drop(sink);
            Ok(done.frames)
        })
        .await
        .map_err(|e| e.to_string())??;
        let render_seconds = rendered.elapsed().as_secs_f64();
        drop(_card);

        let bytes = self.runs.put(&local, &order.out).await?;
        let receipt = SegmentDone {
            out: order.out.clone(),
            frames: u64::from(frames),
            bytes,
            announced_tiles,
            waited_seconds,
            render_seconds,
        };
        let receipt_path = scratch.path().join("segment.json");
        std::fs::write(&receipt_path, serde_json::to_vec_pretty(&receipt)?)?;
        self.runs
            .put(
                &receipt_path,
                &format!("{}.json", order.out.trim_end_matches(".mp4")),
            )
            .await?;
        Ok(receipt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_order_reads_from_json_with_only_what_it_must_say() {
        let order: SegmentOrder = serde_json::from_str(
            r#"{"pack": "packs/a/1-450.tuilepack", "frames": [1, 450], "out": "films/a/x/parts/1-450.mp4"}"#,
        )
        .expect("an order");
        assert_eq!(order.frames, (1, 450));
        assert_eq!(order.rendering, RenderSpec::default());
        assert!(order.announced.is_none() && order.to_sun.is_none());
        // A field nobody knows is refused, not ignored: a typo would render
        // another film.
        assert!(serde_json::from_str::<SegmentOrder>(
            r#"{"pack": "p", "frames": [1, 2], "out": "o", "exposur": 1.5}"#
        )
        .is_err());
        let spec: RenderSpec =
            serde_json::from_str(r#"{"exposure": 1.6, "sun_light": "3.2,1.7,0.8", "mbps": 45}"#)
                .expect("a spec");
        let rendering = spec.rendering().expect("a rendering");
        assert_eq!(rendering.bitrate, 45_000_000);
        assert_eq!(rendering.order.look.exposure_ev, 1.6);
        assert!(
            serde_json::from_str::<RenderSpec>(r#"{"sky_light": "1,2"}"#)
                .expect("parsed")
                .rendering()
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_segment_whose_pack_never_comes_is_refused_once_its_time_is_over() {
        let dir = tempfile::tempdir().expect("dir");
        // A tile store with no layer: its catalog is all it needs.
        std::fs::write(dir.path().join("catalog.json"), br#"{"layers": []}"#).expect("catalog");
        let runs = Arc::new(
            ObjectRunStore::local(dir.path(), tuile_farm::Tuning::default()).expect("store"),
        );
        let tiles: Arc<dyn tuile_repository::Objects> = runs.clone();
        let service = Service::new(runs, tiles, dir.path().join("cache"), Limits::default());
        let order = SegmentOrder {
            pack: "packs/never.tuilepack".into(),
            announced: None,
            frames: (1, 2),
            out: "out.mp4".into(),
            rendering: RenderSpec::default(),
            to_sun: None,
            pack_within_seconds: Some(0),
        };
        let refused = service.segment(&order).await.expect_err("no pack");
        assert!(refused.to_string().contains("did not come"), "{refused}");
    }

    #[tokio::test]
    async fn a_generation_is_kept_until_its_caches_pass_their_budget_or_their_age() {
        let dir = tempfile::tempdir().expect("dir");
        std::fs::write(dir.path().join("catalog.json"), br#"{"layers": []}"#).expect("catalog");
        let runs = Arc::new(
            ObjectRunStore::local(dir.path(), tuile_farm::Tuning::default()).expect("store"),
        );
        let tiles: Arc<dyn tuile_repository::Objects> = runs.clone();
        let limits = Limits {
            cache_bytes: 1 << 20,
            ..Limits::default()
        };
        let service = Service::new(runs, tiles, dir.path().join("cache"), limits);
        let first = service.generation().await.expect("first");
        let again = service.generation().await.expect("again");
        assert!(
            Arc::ptr_eq(&first, &again),
            "a generation under its budget is kept"
        );

        // Past its budget: the next segment begins in another, and the old
        // directory goes with the last segment that holds it.
        std::fs::write(first.dir.path().join("big"), vec![0u8; 2 << 20]).expect("fill");
        let other = service.generation().await.expect("other");
        assert!(
            !Arc::ptr_eq(&first, &other),
            "a generation past its budget is let go"
        );
        let old = first.dir.path().to_path_buf();
        drop((first, again));
        assert!(!old.exists(), "its caches go with it");

        assert!(!other.spent(&limits));
        assert!(other.spent(&Limits {
            generation_age: Duration::ZERO,
            ..limits
        }));
    }
}
