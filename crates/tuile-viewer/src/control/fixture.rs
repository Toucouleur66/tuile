// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A globe that needs no network: the network seam of the headless viewer.
//!
//! The viewer fetches its ground through the host's [`Sources`]; a headless
//! run hands it these. The terrain is the WGS84 ellipsoid, every tile there
//! down to [`TERRAIN_LEVELS`]; the imagery is made up, on the same
//! geographic grid, a flat colour per level in a checkerboard of two shades,
//! down to a deepest level of the run's choosing — bright everywhere, so a pixel left the clear colour is a
//! hole and never a dark picture. Both answer at once and the same every
//! time, so a run's frames depend on its script alone.
//!
//! Each counts what it is asked: the network a run would have used, beside
//! the store's own count of what it served instead
//! (`tuile_core::metrics::Metrics::store_hits`).
//!
//! **Latency**, when a run asks for it ([`Fixture::with_latency`]), is in the
//! run's own time: an answer lands once the run's stepped clock has moved
//! that far past the question, never by the wall clock — so a slow network is
//! as repeatable as an instant one.
//!
//! [`Store`] is the tile store between runs: memory shared by a run and the
//! one relaunched after it, which is what a warm start reads.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use bytes::Bytes;
use image::ImageEncoder as _;
use tuile_core::fetch::Fetched;
use tuile_core::geo::{geodetic_to_ecef, Geodetic, WGS84_A};
use tuile_core::raster::{ImageryCoord, ImageryProvider, RasterError, TilingScheme};
use tuile_core::storage::ContentStore;
use tuile_terrain::decode::{encode, Header};
use tuile_terrain::{
    AvailabilityRange, GeographicTilingScheme, LayerJson, TerrainSource, TerrainSourceError,
    TileCoord,
};

use crate::embed::{ImageryChoice, Sources, TerrainChoice};

/// The deepest terrain level served: past it the loader cuts finer tiles from
/// these, as it does from any source that stops.
pub const TERRAIN_LEVELS: u32 = 14;
/// Spacings a side of a terrain tile.
const STEPS: usize = 16;
/// The imagery's side, pixels: a web map's.
const IMAGERY_SIDE: u32 = 256;

/// The terrain and the imagery layer a fixture globe is asked for, by number.
pub(crate) const TERRAIN_ASSET: i64 = 1;
pub(crate) const IMAGERY_ASSET: i64 = 2;

/// The fixture's one imagery layer, as a host lists its layers.
pub(crate) fn layers() -> &'static [ImageryChoice] {
    static LAYERS: OnceLock<Vec<ImageryChoice>> = OnceLock::new();
    LAYERS.get_or_init(|| {
        vec![ImageryChoice::new(
            "fixture",
            "Fixture",
            IMAGERY_ASSET,
            "made up for tests",
        )]
    })
}

/// The fixture's terrain, as a host names it.
pub(crate) fn terrain() -> TerrainChoice {
    TerrainChoice {
        asset: TERRAIN_ASSET,
        cache: "terrain-fixture".into(),
    }
}

/// What the network was asked, in a run.
#[derive(Debug, Default)]
pub struct Asked {
    pub terrain: AtomicU64,
    pub imagery: AtomicU64,
}

/// The fixture globe's sources: see the module.
#[derive(Debug, Clone)]
pub struct Fixture {
    /// The deepest imagery level served.
    pub imagery_levels: u32,
    /// How long, in the run's time, every answer takes.
    pub latency: Duration,
    asked: Arc<Asked>,
    wire: Arc<Wire>,
}

/// The fixture's network: the run's time, and the answers waiting for it.
#[derive(Debug, Default)]
struct Wire {
    /// Nanoseconds since the run began.
    now: AtomicU64,
    waiting: Mutex<Vec<(u64, Waker)>>,
}

impl Wire {
    /// An answer, `latency` after now.
    fn answer(self: &Arc<Self>, latency: Duration) -> Answer {
        let due = self.now.load(Ordering::SeqCst) + latency.as_nanos() as u64;
        Answer {
            wire: Arc::clone(self),
            due,
        }
    }
}

/// An answer on its way: ready once the run's time reaches `due`.
struct Answer {
    wire: Arc<Wire>,
    due: u64,
}

impl Future for Answer {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.wire.now.load(Ordering::SeqCst) >= self.due {
            return Poll::Ready(());
        }
        if let Ok(mut waiting) = self.wire.waiting.lock() {
            waiting.push((self.due, cx.waker().clone()));
        }
        Poll::Pending
    }
}

impl Default for Fixture {
    fn default() -> Self {
        Self::new(16)
    }
}

impl Fixture {
    /// A globe whose imagery goes down to `imagery_levels`.
    pub fn new(imagery_levels: u32) -> Self {
        Self {
            imagery_levels,
            latency: Duration::ZERO,
            asked: Arc::new(Asked::default()),
            wire: Arc::new(Wire::default()),
        }
    }

    /// The same globe behind a network that takes `latency` to answer, in
    /// the run's time.
    #[must_use]
    pub fn with_latency(mut self, latency: Duration) -> Self {
        self.latency = latency;
        self
    }

    /// The run's clock has reached `since_start`: every answer due by then
    /// lands. A new run starts its time again from zero.
    pub(crate) fn advance_to(&self, since_start: Duration) {
        let now = since_start.as_nanos() as u64;
        self.wire.now.store(now, Ordering::SeqCst);
        let due: Vec<Waker> = match self.wire.waiting.lock() {
            Ok(mut waiting) => {
                let (due, later): (Vec<_>, Vec<_>) =
                    waiting.drain(..).partition(|(at, _)| *at <= now);
                *waiting = later;
                due.into_iter().map(|(_, waker)| waker).collect()
            }
            Err(_) => Vec::new(),
        };
        for waker in due {
            waker.wake();
        }
    }

    /// What has been asked of it.
    pub fn asked(&self) -> (u64, u64) {
        (
            self.asked.terrain.load(Ordering::Relaxed),
            self.asked.imagery.load(Ordering::Relaxed),
        )
    }
}

#[async_trait::async_trait]
impl Sources for Fixture {
    async fn terrain(&self, asset: i64) -> Result<(LayerJson, Arc<dyn TerrainSource>), String> {
        if asset != TERRAIN_ASSET {
            return Err(format!("the fixture has no terrain {asset}"));
        }
        Ok((
            ellipsoid_layer(),
            Arc::new(Ellipsoid {
                asked: Arc::clone(&self.asked),
                wire: Arc::clone(&self.wire),
                latency: self.latency,
            }),
        ))
    }

    async fn imagery(&self, asset: i64) -> Result<Arc<dyn ImageryProvider>, String> {
        if asset != IMAGERY_ASSET {
            return Err(format!("the fixture has no imagery {asset}"));
        }
        Ok(Arc::new(Colours {
            deepest: self.imagery_levels,
            asked: Arc::clone(&self.asked),
            wire: Arc::clone(&self.wire),
            latency: self.latency,
            pictures: Mutex::new(BTreeMap::new()),
        }))
    }
}

/// The ellipsoid's `layer.json`: quantized mesh, TMS, geographic, every tile
/// there.
fn ellipsoid_layer() -> LayerJson {
    let available = (0..=TERRAIN_LEVELS)
        .map(|level| {
            vec![AvailabilityRange {
                start_x: 0,
                start_y: 0,
                end_x: (2u64 << level) - 1,
                end_y: (1u64 << level) - 1,
            }]
        })
        .collect();
    LayerJson {
        format: "quantized-mesh-1.0".into(),
        scheme: Some("tms".into()),
        projection: Some("EPSG:4326".into()),
        tiles: vec!["{z}/{x}/{y}".into()],
        version: Some("1".into()),
        minzoom: 0,
        maxzoom: TERRAIN_LEVELS,
        metadata_availability: None,
        extensions: Vec::new(),
        available,
        attribution: None,
    }
}

/// The ellipsoid as terrain: a grid at height 0 on every tile.
struct Ellipsoid {
    asked: Arc<Asked>,
    wire: Arc<Wire>,
    latency: Duration,
}

#[async_trait::async_trait]
impl TerrainSource for Ellipsoid {
    async fn fetch_tile(&self, coord: TileCoord) -> Result<Fetched<Vec<u8>>, TerrainSourceError> {
        self.asked.terrain.fetch_add(1, Ordering::Relaxed);
        if !self.latency.is_zero() {
            self.wire.answer(self.latency).await;
        }
        Ok(Fetched::undated(grid(coord)))
    }
}

/// Tile `coord`'s quantized mesh: a grid on the ellipsoid at height 0.
fn grid(coord: TileCoord) -> Vec<u8> {
    let rect = GeographicTilingScheme::default().tile_rect(coord);
    let n = STEPS + 1;
    let (mut u, mut v) = (Vec::with_capacity(n * n), Vec::with_capacity(n * n));
    for j in 0..n {
        for i in 0..n {
            u.push(i as f64 / STEPS as f64);
            v.push(j as f64 / STEPS as f64);
        }
    }
    let mut indices = Vec::with_capacity(STEPS * STEPS * 6);
    for j in 0..STEPS {
        for i in 0..STEPS {
            let a = (j * n + i) as u32;
            let (b, c, d) = (a + 1, a + n as u32, a + n as u32 + 1);
            indices.extend_from_slice(&[a, b, c, c, b, d]);
        }
    }
    let (lon, lat) = rect.center();
    let centre = geodetic_to_ecef(Geodetic {
        lon,
        lat,
        height: 0.0,
    });
    let radius = [
        (0.0, 0.0),
        (1.0, 0.0),
        (0.0, 1.0),
        (1.0, 1.0),
        (0.5, 0.0),
        (0.0, 0.5),
    ]
    .iter()
    .map(|(fu, fv)| {
        geodetic_to_ecef(Geodetic {
            lon: rect.west + fu * rect.width(),
            lat: rect.south + fv * rect.height(),
            height: 0.0,
        })
        .distance(centre)
    })
    .fold(1.0f64, f64::max);
    let header = Header {
        center: centre.to_array(),
        min_height: 0.0,
        max_height: 0.0,
        bounding_sphere_center: centre.to_array(),
        bounding_sphere_radius: radius,
        horizon_occlusion: (centre / WGS84_A).to_array(),
    };
    let line = |f: &dyn Fn(usize) -> usize| (0..n).map(|k| f(k) as u32).collect::<Vec<u32>>();
    let edges = [
        line(&|k| k * n),
        line(&|k| k),
        line(&|k| k * n + STEPS),
        line(&|k| (n - 1) * n + k),
    ];
    // Vertices renumbered in the order the triangles first name them: the
    // high-water-mark coding of the indices requires it.
    let mut order = vec![u32::MAX; n * n];
    let mut next = 0u32;
    for i in &mut indices {
        let slot = &mut order[*i as usize];
        if *slot == u32::MAX {
            *slot = next;
            next += 1;
        }
        *i = *slot;
    }
    let (mut uu, mut vv) = (vec![0.0; n * n], vec![0.0; n * n]);
    for (old, &new) in order.iter().enumerate() {
        uu[new as usize] = u[old];
        vv[new as usize] = v[old];
    }
    let edges = edges.map(|e| e.into_iter().map(|i| order[i as usize]).collect());
    encode(&header, &uu, &vv, &vec![0.0; n * n], &indices, None, &edges)
}

/// Made-up imagery: one colour a level, two shades in a checkerboard.
struct Colours {
    deepest: u32,
    asked: Arc<Asked>,
    wire: Arc<Wire>,
    latency: Duration,
    /// Each picture encoded once: `(level, shade)`.
    pictures: Mutex<BTreeMap<(u32, bool), Bytes>>,
}

#[async_trait::async_trait]
impl ImageryProvider for Colours {
    fn tiling_scheme(&self) -> TilingScheme {
        // On the terrain's own grid: a picture drapes without being
        // reprojected, which is the slowest thing a debug build does.
        TilingScheme {
            tile_size: IMAGERY_SIDE,
            maximum_level: self.deepest,
            ..TilingScheme::geographic()
        }
    }

    async fn fetch_tile_bytes(&self, coord: ImageryCoord) -> Result<Fetched<Bytes>, RasterError> {
        self.asked.imagery.fetch_add(1, Ordering::Relaxed);
        if !self.latency.is_zero() {
            self.wire.answer(self.latency).await;
        }
        let key = (coord.level, (coord.x + coord.y) % 2 == 1);
        let mut pictures = self
            .pictures
            .lock()
            .map_err(|_| RasterError::Image("the fixture's pictures".into()))?;
        if let Some(bytes) = pictures.get(&key) {
            return Ok(Fetched::undated(bytes.clone()));
        }
        let bytes = Bytes::from(picture(colour(key.0, key.1)).map_err(RasterError::Image)?);
        pictures.insert(key, bytes.clone());
        Ok(Fetched::undated(bytes))
    }
}

/// Level `level`'s colour: a hue turned forty-seven degrees a level, never
/// dark — the darkest channel of the darker shade is a third of full.
pub(crate) fn colour(level: u32, darker: bool) -> [u8; 3] {
    let hue = f64::from(level * 47 % 360) / 60.0;
    let (value, saturation) = (if darker { 0.78 } else { 0.95 }, 0.5);
    let chroma = value * saturation;
    let x = chroma * (1.0 - (hue % 2.0 - 1.0).abs());
    let (r, g, b) = match hue as u32 {
        0 => (chroma, x, 0.0),
        1 => (x, chroma, 0.0),
        2 => (0.0, chroma, x),
        3 => (0.0, x, chroma),
        4 => (x, 0.0, chroma),
        _ => (chroma, 0.0, x),
    };
    let m = value - chroma;
    [r, g, b].map(|c| ((c + m) * 255.0).round() as u8)
}

fn picture(rgb: [u8; 3]) -> Result<Vec<u8>, String> {
    let side = IMAGERY_SIDE as usize;
    let rgba: Vec<u8> = (0..side * side)
        .flat_map(|_| [rgb[0], rgb[1], rgb[2], 255])
        .collect();
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(
            &rgba,
            IMAGERY_SIDE,
            IMAGERY_SIDE,
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|e| e.to_string())?;
    Ok(png)
}

/// A tile store in memory, kept from one run to the next: what a relaunch
/// warms from.
///
/// Ordered, so nothing about it depends on a hash's seed. A store, not a
/// cache: it keeps everything, which is what a test of "nothing held is
/// fetched again" needs to be able to say.
#[derive(Debug, Clone, Default)]
pub struct Store(Arc<Mutex<BTreeMap<String, Bytes>>>);

impl Store {
    /// How many values it holds.
    pub fn len(&self) -> usize {
        self.0.lock().map_or(0, |held| held.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait::async_trait]
impl ContentStore for Store {
    async fn get(&self, key: &str) -> Option<Bytes> {
        self.0.lock().ok()?.get(key).cloned()
    }

    async fn put(&self, key: &str, value: Bytes, _ttl: Option<std::time::Duration>) {
        if let Ok(mut held) = self.0.lock() {
            held.insert(key.to_owned(), value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No colour of any level is dark enough to be read as the clear
    /// colour, lit by ambient alone; and two levels side by side differ.
    #[test]
    fn every_colour_is_bright_and_neighbouring_levels_differ() {
        for level in 0..=24 {
            for darker in [false, true] {
                let c = colour(level, darker);
                assert!(
                    c.iter().all(|&v| v >= 60),
                    "level {level}: {c:?} is too dark"
                );
            }
            assert_ne!(colour(level, false), colour(level + 1, false));
        }
    }

    /// Over a slow network an answer lands when the run's time reaches it,
    /// and not a moment before — however long the wall clock waits.
    #[test]
    fn a_slow_answer_lands_when_the_runs_time_reaches_it() {
        let fixture = Fixture::default().with_latency(Duration::from_millis(100));
        fixture.advance_to(Duration::ZERO);
        let (_, terrain) =
            futures_executor::block_on(Sources::terrain(&fixture, TERRAIN_ASSET)).expect("terrain");
        let waker = futures_util::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut answer = terrain.fetch_tile(TileCoord::new(0, 0, 0));
        assert!(answer.as_mut().poll(&mut cx).is_pending());
        std::thread::sleep(Duration::from_millis(120));
        fixture.advance_to(Duration::from_millis(99));
        assert!(answer.as_mut().poll(&mut cx).is_pending(), "early");
        fixture.advance_to(Duration::from_millis(100));
        assert!(answer.as_mut().poll(&mut cx).is_ready());
        assert_eq!(fixture.asked(), (1, 0));
    }

    /// The fixture's terrain decodes as a tile of the grid it was asked for.
    #[test]
    fn a_terrain_tile_decodes() {
        let mesh = tuile_terrain::decode(&grid(TileCoord::new(3, 5, 2))).expect("decodes");
        assert_eq!(mesh.u.len(), (STEPS + 1) * (STEPS + 1));
    }
}
