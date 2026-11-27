// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Origins that may be out of reach.
//!
//! A globe needs two descriptions before it has a single tile: the terrain's
//! `layer.json` (its tree is built from it) and the imagery's tiling scheme
//! (its drapes are addressed by it). Both used to come from the origin on
//! every start, so a session started without a network drew nothing at all,
//! a full tile store on the disk notwithstanding.
//!
//! Here the two descriptions are **remembered** in the tile store beside the
//! tiles they describe ([`recall_layer`], [`recall_scheme`]), and the sources
//! they come from are **resolved when they can be, not before the globe may
//! start**: a [`DeferredTerrain`] or a [`DeferredImagery`] stands in for the
//! host's source, answers at once with a failure — never an absence — while
//! the origin cannot be resolved, and resolves it on a later request, with a
//! wait that doubles between attempts. The store in front of it serves what
//! it holds throughout; the graphics budget retries what failed, and its
//! retries are what bring the origin back: nothing here owns a timer, so the
//! browser's worker and a desktop's runtime run it alike.
//!
//! Whether each origin answers is published per role
//! ([`tuile_core::metrics::Metrics::terrain_offline`] and
//! [`imagery_offline`](tuile_core::metrics::Metrics::imagery_offline)), and
//! every change of it is said once in the log.

use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tuile_core::fetch::{FetchError, Fetched};
use tuile_core::raster::{ImageryCoord, ImageryProvider, Projection, RasterError, TilingScheme};
use tuile_core::storage::ContentStore;
use tuile_terrain::{LayerJson, TerrainSource, TerrainSourceError, TileCoord};
use web_time::Instant;

use crate::Sources;

/// Which half of the globe an origin serves: what its reach is reported as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Terrain,
    Imagery,
}

impl Role {
    fn name(self) -> &'static str {
        match self {
            Self::Terrain => "terrain",
            Self::Imagery => "imagery",
        }
    }

    fn gauge(self) -> &'static tuile_core::metrics::Gauge {
        let m = tuile_core::metrics::metrics();
        match self {
            Self::Terrain => &m.terrain_offline,
            Self::Imagery => &m.imagery_offline,
        }
    }
}

/// How an origin out of reach is tried again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Patience {
    /// The wait after the first failure; it doubles after each next one.
    pub first: Duration,
    /// The longest wait.
    pub most: Duration,
    /// How long an attempt may stay unanswered before another may start. A
    /// request a black-holed network never answers must not keep the origin
    /// unresolved for the rest of the session.
    pub attempt: Duration,
}

impl Default for Patience {
    fn default() -> Self {
        Self {
            first: Duration::from_secs(1),
            most: Duration::from_secs(60),
            attempt: Duration::from_secs(20),
        }
    }
}

impl Patience {
    fn wait(&self, failures: u32) -> Duration {
        let doublings = failures.saturating_sub(1).min(16);
        self.first.saturating_mul(1u32 << doublings).min(self.most)
    }
}

#[derive(Default)]
struct Attempts {
    failures: u32,
    /// No attempt before this.
    next: Option<Instant>,
    /// The attempt in flight, since when.
    since: Option<Instant>,
}

/// Whether an origin answers, and when it may be asked again: see the
/// module.
struct Reach {
    role: Role,
    patience: Patience,
    attempts: Mutex<Attempts>,
    /// What was last seen: `None` before anything was.
    online: Mutex<Option<bool>>,
}

impl Reach {
    fn new(role: Role) -> Self {
        Self {
            role,
            patience: Patience::default(),
            attempts: Mutex::new(Attempts::default()),
            online: Mutex::new(None),
        }
    }

    /// Whether an attempt to resolve may go now; when it may, it is the one
    /// in flight, and every other request is answered at once until it ends.
    fn begin(&self, now: Instant) -> bool {
        let Ok(mut a) = self.attempts.lock() else {
            return false;
        };
        let in_flight = a
            .since
            .is_some_and(|since| now.saturating_duration_since(since) < self.patience.attempt);
        if in_flight || a.next.is_some_and(|next| now < next) {
            return false;
        }
        a.since = Some(now);
        true
    }

    fn resolved(&self) {
        if let Ok(mut a) = self.attempts.lock() {
            *a = Attempts::default();
        }
        self.saw(true);
    }

    fn refused(&self, now: Instant) {
        if let Ok(mut a) = self.attempts.lock() {
            a.failures += 1;
            a.next = Some(now + self.patience.wait(a.failures));
            a.since = None;
        }
        self.saw(false);
    }

    fn online(&self) -> Option<bool> {
        self.online.lock().ok().and_then(|o| *o)
    }

    /// Notes whether the origin answered: published, and said when it
    /// changes.
    fn saw(&self, online: bool) {
        let Ok(mut last) = self.online.lock() else {
            return;
        };
        if *last == Some(online) {
            return;
        }
        let was_offline = *last == Some(false);
        *last = Some(online);
        drop(last);
        self.role.gauge().set(u64::from(!online));
        // From nothing known to answering is no change a person would see.
        if online && !was_offline {
            tracing::info!(role = self.role.name(), "origin online");
            return;
        }
        tuile_core::metrics::metrics().source_transitions.inc();
        if online {
            tracing::info!(role = self.role.name(), "origin back online");
        } else {
            tracing::warn!(
                role = self.role.name(),
                "origin out of reach: local tiles only until it answers"
            );
        }
    }
}

/// Where a source's description is remembered: the store its tiles are in,
/// and the key it is kept under.
#[derive(Clone)]
pub struct Memory {
    store: Arc<dyn ContentStore>,
    key: String,
}

/// How long a description is kept without being written again. Each time
/// the origin is resolved it is written again; a year is for a machine that
/// stays offline.
const KEPT: Duration = Duration::from_secs(365 * 24 * 60 * 60);

impl Memory {
    /// The terrain stored under `namespace`.
    pub fn terrain(store: Arc<dyn ContentStore>, namespace: &str) -> Self {
        Self {
            store,
            key: format!("descriptor/terrain/{namespace}"),
        }
    }

    /// The imagery stored under `namespace`.
    pub fn imagery(store: Arc<dyn ContentStore>, namespace: &str) -> Self {
        Self {
            store,
            key: format!("descriptor/imagery/{namespace}"),
        }
    }

    /// The `layer.json` last kept here.
    pub async fn layer(&self) -> Option<LayerJson> {
        let bytes = self.store.get(&self.key).await?;
        LayerJson::from_slice(&bytes).ok()
    }

    /// Keeps `layer`.
    pub async fn keep_layer(&self, layer: &LayerJson) {
        self.store
            .put(&self.key, Bytes::from(layer.to_json()), Some(KEPT))
            .await;
    }

    /// The tiling scheme last kept here.
    pub async fn scheme(&self) -> Option<TilingScheme> {
        let bytes = self.store.get(&self.key).await?;
        scheme_from(std::str::from_utf8(&bytes).ok()?)
    }

    /// Keeps `scheme`.
    pub async fn keep_scheme(&self, scheme: &TilingScheme) {
        self.store
            .put(&self.key, Bytes::from(scheme_text(scheme)), Some(KEPT))
            .await;
    }
}

/// The `layer.json` of the terrain stored under `namespace`, as last
/// resolved: what a globe can be built on with no network.
pub async fn recall_layer(store: &Arc<dyn ContentStore>, namespace: &str) -> Option<LayerJson> {
    Memory::terrain(Arc::clone(store), namespace).layer().await
}

/// The tiling scheme of the imagery stored under `namespace`, as last
/// resolved.
pub async fn recall_scheme(store: &Arc<dyn ContentStore>, namespace: &str) -> Option<TilingScheme> {
    Memory::imagery(Arc::clone(store), namespace).scheme().await
}

fn scheme_text(s: &TilingScheme) -> String {
    let projection = match s.projection {
        Projection::WebMercator => "web-mercator",
        Projection::Geographic => "geographic",
    };
    format!(
        "tiling 1 {projection} {} {} {} {} {}",
        s.root_tiles_x, s.root_tiles_y, s.tile_size, s.minimum_level, s.maximum_level
    )
}

fn scheme_from(text: &str) -> Option<TilingScheme> {
    let mut words = text.split_whitespace();
    if words.next()? != "tiling" || words.next()? != "1" {
        return None;
    }
    let projection = match words.next()? {
        "web-mercator" => Projection::WebMercator,
        "geographic" => Projection::Geographic,
        _ => return None,
    };
    let mut number = || words.next()?.parse::<u64>().ok();
    Some(TilingScheme {
        projection,
        root_tiles_x: number()?,
        root_tiles_y: number()?,
        tile_size: u32::try_from(number()?).ok()?,
        minimum_level: u32::try_from(number()?).ok()?,
        maximum_level: u32::try_from(number()?).ok()?,
    })
}

/// The host's terrain `asset`, resolved when it can be: see the module.
pub struct DeferredTerrain {
    sources: Arc<dyn Sources>,
    asset: i64,
    reach: Reach,
    resolved: RwLock<Option<Arc<dyn TerrainSource>>>,
    memory: Option<Memory>,
}

impl DeferredTerrain {
    /// Not resolved yet: the first request resolves it.
    pub fn pending(sources: Arc<dyn Sources>, asset: i64, memory: Option<Memory>) -> Self {
        Self {
            sources,
            asset,
            reach: Reach::new(Role::Terrain),
            resolved: RwLock::new(None),
            memory,
        }
    }

    /// Resolved already, as `source`.
    pub fn resolved(
        sources: Arc<dyn Sources>,
        asset: i64,
        source: Arc<dyn TerrainSource>,
        memory: Option<Memory>,
    ) -> Self {
        let deferred = Self::pending(sources, asset, memory);
        *deferred.resolved.write().expect("fresh") = Some(source);
        deferred.reach.saw(true);
        deferred
    }

    /// Tried again on `patience` rather than the default's.
    #[must_use]
    pub fn with_patience(mut self, patience: Patience) -> Self {
        self.reach.patience = patience;
        self
    }

    /// Whether the origin has been resolved.
    pub fn is_resolved(&self) -> bool {
        self.resolved.read().is_ok_and(|r| r.is_some())
    }

    /// Whether the origin answered last time it was asked; `None` before it
    /// was.
    pub fn is_online(&self) -> Option<bool> {
        self.reach.online()
    }

    fn held(&self) -> Option<Arc<dyn TerrainSource>> {
        self.resolved.read().ok().and_then(|r| r.clone())
    }

    async fn source(&self) -> Result<Arc<dyn TerrainSource>, TerrainSourceError> {
        if let Some(source) = self.held() {
            return Ok(source);
        }
        if !self.reach.begin(Instant::now()) {
            return Err(TerrainSourceError::failed(
                "the terrain's origin is out of reach",
            ));
        }
        match self.sources.terrain(self.asset).await {
            Ok((layer, source)) => {
                if let Some(memory) = &self.memory {
                    memory.keep_layer(&layer).await;
                }
                if let Ok(mut held) = self.resolved.write() {
                    held.get_or_insert_with(|| Arc::clone(&source));
                }
                self.reach.resolved();
                Ok(self.held().unwrap_or(source))
            }
            Err(why) => {
                self.reach.refused(Instant::now());
                Err(TerrainSourceError::failed(format!(
                    "the terrain's origin is out of reach: {why}"
                )))
            }
        }
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl TerrainSource for DeferredTerrain {
    async fn fetch_tile(&self, coord: TileCoord) -> Result<Fetched<Vec<u8>>, TerrainSourceError> {
        let fetched = self.source().await?.fetch_tile(coord).await;
        // An absence is an answer.
        self.reach
            .saw(fetched.as_ref().map_or_else(|e| e.is_absence(), |_| true));
        fetched
    }

    fn holds(&self, coord: TileCoord) -> bool {
        self.held().is_some_and(|source| source.holds(coord))
    }
}

/// The host's imagery `asset`, resolved when it can be, on the tiling
/// scheme it was last resolved with: see the module.
///
/// The scheme is fixed for the provider's life, as every provider's is. An
/// origin that answers on another grid than the one remembered is not
/// adopted — its tiles would be draped at the wrong addresses — but its grid
/// is remembered, and the next session starts on it.
pub struct DeferredImagery {
    sources: Arc<dyn Sources>,
    asset: i64,
    scheme: TilingScheme,
    reach: Reach,
    resolved: RwLock<Option<Arc<dyn ImageryProvider>>>,
    /// Set when the origin answered on another grid.
    regridded: Mutex<bool>,
    memory: Option<Memory>,
}

impl DeferredImagery {
    /// Not resolved yet, on `scheme`: the first request resolves it.
    pub fn pending(
        sources: Arc<dyn Sources>,
        asset: i64,
        scheme: TilingScheme,
        memory: Option<Memory>,
    ) -> Self {
        Self {
            sources,
            asset,
            scheme,
            reach: Reach::new(Role::Imagery),
            resolved: RwLock::new(None),
            regridded: Mutex::new(false),
            memory,
        }
    }

    /// Resolved already, as `provider`, on its own scheme.
    pub fn resolved(
        sources: Arc<dyn Sources>,
        asset: i64,
        provider: Arc<dyn ImageryProvider>,
        memory: Option<Memory>,
    ) -> Self {
        let deferred = Self::pending(sources, asset, provider.tiling_scheme(), memory);
        *deferred.resolved.write().expect("fresh") = Some(provider);
        deferred.reach.saw(true);
        deferred
    }

    /// Tried again on `patience` rather than the default's.
    #[must_use]
    pub fn with_patience(mut self, patience: Patience) -> Self {
        self.reach.patience = patience;
        self
    }

    /// Whether the origin has been resolved.
    pub fn is_resolved(&self) -> bool {
        self.resolved.read().is_ok_and(|r| r.is_some())
    }

    /// Whether the origin answered last time it was asked; `None` before it
    /// was.
    pub fn is_online(&self) -> Option<bool> {
        self.reach.online()
    }

    fn held(&self) -> Option<Arc<dyn ImageryProvider>> {
        self.resolved.read().ok().and_then(|r| r.clone())
    }

    fn unreachable(why: String) -> RasterError {
        RasterError::Provider(why)
    }

    async fn provider(&self) -> Result<Arc<dyn ImageryProvider>, RasterError> {
        if let Some(provider) = self.held() {
            return Ok(provider);
        }
        if self.regridded.lock().is_ok_and(|r| *r) {
            return Err(Self::unreachable(
                "the imagery's origin moved to another grid; it is taken at the next start".into(),
            ));
        }
        if !self.reach.begin(Instant::now()) {
            return Err(Self::unreachable(
                "the imagery's origin is out of reach".into(),
            ));
        }
        match self.sources.imagery(self.asset).await {
            Ok(provider) => {
                let scheme = provider.tiling_scheme();
                if let Some(memory) = &self.memory {
                    memory.keep_scheme(&scheme).await;
                }
                self.reach.resolved();
                if scheme != self.scheme {
                    tracing::warn!(
                        asset = self.asset,
                        "the imagery's origin answers on another grid than the one \
                         remembered; the store serves this session, the next one starts on it"
                    );
                    if let Ok(mut regridded) = self.regridded.lock() {
                        *regridded = true;
                    }
                    return Err(Self::unreachable("the imagery's grid changed".into()));
                }
                if let Ok(mut held) = self.resolved.write() {
                    held.get_or_insert_with(|| Arc::clone(&provider));
                }
                Ok(self.held().unwrap_or(provider))
            }
            Err(why) => {
                self.reach.refused(Instant::now());
                Err(Self::unreachable(format!(
                    "the imagery's origin is out of reach: {why}"
                )))
            }
        }
    }
}

/// Whether an imagery failure is an answer from the origin: "there is no
/// such tile".
fn is_absence(e: &RasterError) -> bool {
    matches!(e, RasterError::Fetch(FetchError::NotFound(_)))
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl ImageryProvider for DeferredImagery {
    fn tiling_scheme(&self) -> TilingScheme {
        self.scheme
    }

    async fn fetch_tile_stamp(&self, coord: ImageryCoord) -> tuile_core::storage::Stamp {
        match self.held() {
            Some(provider) => provider.fetch_tile_stamp(coord).await,
            None => tuile_core::storage::Stamp::Unknown,
        }
    }

    fn holds(&self, coord: ImageryCoord) -> bool {
        self.held().is_some_and(|provider| provider.holds(coord))
    }

    async fn fetch_tile_bytes(&self, coord: ImageryCoord) -> Result<Fetched<Bytes>, RasterError> {
        let fetched = self.provider().await?.fetch_tile_bytes(coord).await;
        self.reach
            .saw(fetched.as_ref().map_or_else(is_absence, |_| true));
        fetched
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

    /// An origin that refuses, never answers, or serves — as it is told.
    pub(super) struct Origin {
        pub mode: AtomicU8,
        pub asked: AtomicUsize,
    }

    pub(super) const REFUSES: u8 = 0;
    pub(super) const HANGS: u8 = 1;
    pub(super) const SERVES: u8 = 2;

    impl Origin {
        fn new(mode: u8) -> Arc<Self> {
            Arc::new(Self {
                mode: AtomicU8::new(mode),
                asked: AtomicUsize::new(0),
            })
        }

        async fn answer(&self) -> Result<(), String> {
            self.asked.fetch_add(1, Ordering::SeqCst);
            match self.mode.load(Ordering::SeqCst) {
                REFUSES => Err("dns error: no such host".into()),
                HANGS => std::future::pending().await,
                _ => Ok(()),
            }
        }
    }

    /// A store in memory.
    #[derive(Default)]
    struct Kept(Mutex<std::collections::HashMap<String, Bytes>>);

    #[async_trait]
    impl ContentStore for Kept {
        async fn get(&self, key: &str) -> Option<Bytes> {
            self.0.lock().expect("kept").get(key).cloned()
        }
        async fn put(&self, key: &str, value: Bytes, _ttl: Option<Duration>) {
            self.0.lock().expect("kept").insert(key.to_owned(), value);
        }
    }

    struct Tiles;

    #[async_trait]
    impl TerrainSource for Tiles {
        async fn fetch_tile(
            &self,
            _coord: TileCoord,
        ) -> Result<Fetched<Vec<u8>>, TerrainSourceError> {
            Ok(Fetched::undated(vec![7]))
        }
    }

    #[async_trait]
    impl ImageryProvider for Tiles {
        fn tiling_scheme(&self) -> TilingScheme {
            TilingScheme::web_mercator()
        }
        async fn fetch_tile_bytes(
            &self,
            _coord: ImageryCoord,
        ) -> Result<Fetched<Bytes>, RasterError> {
            Ok(Fetched::undated(Bytes::from_static(b"jpeg")))
        }
    }

    #[async_trait]
    impl Sources for Origin {
        async fn terrain(
            &self,
            _asset: i64,
        ) -> Result<(LayerJson, Arc<dyn TerrainSource>), String> {
            self.answer().await?;
            let layer = LayerJson::from_slice(
                br#"{"format":"quantized-mesh-1.0","tiles":["{z}/{x}/{y}.terrain"],"version":"2"}"#,
            )
            .map_err(|e| e.to_string())?;
            Ok((layer, Arc::new(Tiles)))
        }
        async fn imagery(&self, _asset: i64) -> Result<Arc<dyn ImageryProvider>, String> {
            self.answer().await?;
            Ok(Arc::new(Tiles))
        }
    }

    fn quick() -> Patience {
        Patience {
            first: Duration::from_millis(40),
            most: Duration::from_millis(40),
            attempt: Duration::from_millis(100),
        }
    }

    const TILE: TileCoord = TileCoord {
        level: 3,
        x: 1,
        y: 2,
    };

    /// **An origin that refuses is answered for at once, and not asked again
    /// before its wait is over; once it serves, the next request after the
    /// wait resolves it and gets its tile.** Red with the deferral gone:
    /// every request asks the origin.
    #[test]
    fn a_refusing_origin_is_asked_after_its_wait_and_comes_back() {
        let origin = Origin::new(REFUSES);
        let terrain = DeferredTerrain::pending(Arc::clone(&origin) as Arc<dyn Sources>, 1, None)
            .with_patience(quick());
        let fetch = || futures_executor::block_on(terrain.fetch_tile(TILE));
        assert!(fetch().is_err());
        assert!(!fetch().expect_err("waiting").is_absence(), "an absence");
        assert!(fetch().is_err());
        assert_eq!(
            origin.asked.load(Ordering::SeqCst),
            1,
            "asked during its wait"
        );
        assert_eq!(terrain.is_online(), Some(false));
        origin.mode.store(SERVES, Ordering::SeqCst);
        assert!(fetch().is_err(), "asked again before its wait was over");
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(fetch().expect("back").value, vec![7]);
        assert!(terrain.is_resolved());
        assert_eq!(terrain.is_online(), Some(true));
        assert_eq!(origin.asked.load(Ordering::SeqCst), 2);
        assert_eq!(fetch().expect("resolved").value, vec![7]);
        assert_eq!(origin.asked.load(Ordering::SeqCst), 2, "resolved twice");
    }

    /// **An origin that never answers holds one request, not every one**:
    /// the others are answered at once while it is in flight, and once its
    /// patience is over another attempt may go — a request lost in a
    /// black-holed network does not keep the origin unresolved for good.
    #[test]
    fn a_hanging_origin_holds_one_request_and_is_tried_again() {
        let origin = Origin::new(HANGS);
        let imagery = Arc::new(
            DeferredImagery::pending(
                Arc::clone(&origin) as Arc<dyn Sources>,
                2,
                TilingScheme::web_mercator(),
                None,
            )
            .with_patience(quick()),
        );
        let coord = ImageryCoord {
            level: 1,
            x: 0,
            y: 0,
        };
        // The first request hangs on its own thread, as a load would on the
        // runtime.
        let hung = Arc::clone(&imagery);
        std::thread::spawn(move || futures_executor::block_on(hung.fetch_tile_bytes(coord)));
        while origin.asked.load(Ordering::SeqCst) == 0 {
            std::thread::yield_now();
        }
        // On a thread of their own too: a request that waited on the origin
        // must fail this test, not hang it.
        let others = Arc::clone(&imagery);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let began = Instant::now();
            let refused = (0..10)
                .all(|_| futures_executor::block_on(others.fetch_tile_bytes(coord)).is_err());
            let _ = tx.send((refused, began.elapsed()));
        });
        let (refused, took) = rx
            .recv_timeout(Duration::from_secs(2))
            .expect("a request waited on the hanging origin");
        assert!(refused);
        assert!(took < Duration::from_millis(50), "{took:?}");
        assert_eq!(origin.asked.load(Ordering::SeqCst), 1);
        origin.mode.store(SERVES, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(120));
        let tile = futures_executor::block_on(imagery.fetch_tile_bytes(coord)).expect("back");
        assert_eq!(&tile.value[..], b"jpeg");
        assert!(imagery.is_resolved());
    }

    /// **A description resolved is remembered, and read back as it was**;
    /// and an origin that comes back on another grid than the one remembered
    /// is not draped at the wrong addresses, but its grid is kept for the
    /// next start.
    #[test]
    fn descriptions_are_remembered_and_a_new_grid_waits_for_the_next_start() {
        let store: Arc<dyn ContentStore> = Arc::new(Kept::default());
        let origin = Origin::new(SERVES);
        let terrain = DeferredTerrain::pending(
            Arc::clone(&origin) as Arc<dyn Sources>,
            1,
            Some(Memory::terrain(Arc::clone(&store), "t")),
        );
        futures_executor::block_on(terrain.fetch_tile(TILE)).expect("served");
        let layer = futures_executor::block_on(recall_layer(&store, "t")).expect("kept");
        assert_eq!(layer.version.as_deref(), Some("2"));
        assert!(futures_executor::block_on(recall_layer(&store, "other")).is_none());

        let geographic = TilingScheme::geographic();
        let imagery = DeferredImagery::pending(
            Arc::clone(&origin) as Arc<dyn Sources>,
            2,
            geographic,
            Some(Memory::imagery(Arc::clone(&store), "i")),
        );
        let coord = ImageryCoord {
            level: 1,
            x: 0,
            y: 0,
        };
        assert!(futures_executor::block_on(imagery.fetch_tile_bytes(coord)).is_err());
        assert_eq!(
            imagery.tiling_scheme(),
            geographic,
            "the grid changed under it"
        );
        assert_eq!(
            futures_executor::block_on(recall_scheme(&store, "i")),
            Some(TilingScheme::web_mercator())
        );
        assert!(futures_executor::block_on(imagery.fetch_tile_bytes(coord)).is_err());
        assert_eq!(origin.asked.load(Ordering::SeqCst), 2, "asked again");
    }

    #[test]
    fn a_scheme_reads_back_as_written_and_nonsense_reads_as_none() {
        for scheme in [TilingScheme::web_mercator(), TilingScheme::geographic()] {
            assert_eq!(scheme_from(&scheme_text(&scheme)), Some(scheme));
        }
        assert_eq!(scheme_from("tiling 2 geographic 2 1 256 0 18"), None);
        assert_eq!(scheme_from("tiling 1 elsewhere 2 1 256 0 18"), None);
        assert_eq!(scheme_from("tiling 1 geographic 2 1"), None);
    }
}
