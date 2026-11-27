// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The globe a session shows, crossed from whatever the host's sources answer
//! with.
//!
//! Nothing here knows a provider: terrain and imagery arrive as the two traits
//! `tuile-planetary` crosses, asked for by asset number through
//! [`Sources`]. What this module adds is the part every host would otherwise
//! write again — the tile store between runs, with each source filed under its
//! own namespace, and the decode work moved off the server's thread.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tuile_core::offload::Offload;
use tuile_core::raster::{CachedImagery, ImageryProvider};
use tuile_core::source::{TileLoader, TileTree};
use tuile_core::storage::ContentStore;
use tuile_planetary::{
    globe_on, DeferredImagery, DeferredTerrain, GlobeOptions, Held, ImageryPlanHandle, LayerBudget,
    Memory, Sources, SwitchableImagery,
};
use tuile_terrain::{CachedTerrain, LayerJson, TerrainSource};

use crate::embed::{ImageryChoice, TerrainChoice};

/// Everything the session needs from the crossing.
pub(crate) struct Globe {
    pub tree: Box<dyn TileTree>,
    pub loader: Arc<dyn TileLoader>,
    /// The relief the camera stands on. A page records its own from the
    /// content it is sent, on the other side of the worker.
    #[cfg(not(target_arch = "wasm32"))]
    pub heights: Arc<tuile_terrain::TerrainHeights>,
    pub budget: LayerBudget,
    /// How the window's device holds imagery, told to the loader: a handle
    /// for the same reason as [`Self::budget`]. RGBA8 until it is set.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub plan: ImageryPlanHandle,
    /// What a change of imagery is resolved against.
    pub layers: Layers,
}

/// How a switch of imagery that was asked for ended.
pub(crate) struct Switched {
    /// The layer asked for, as an index into the host's list.
    pub layer: usize,
    /// The imagery generation the globe is now draping, or why it is not.
    pub outcome: Result<u64, String>,
    /// When it was asked for — the start of the wait a person sees.
    pub asked: web_time::Instant,
}

/// What changing the imagery of a running globe is resolved against: the
/// host's sources, the store, the switch the globe was built over, and the
/// layers already resolved.
///
/// Nothing here takes anything off the screen. A resolved layer is handed to
/// the [`SwitchableImagery`] the globe was built over, and the engine does the
/// rest: every tile keeps the old picture until its new drape is resident.
/// A layer that cannot be resolved changes nothing at all.
///
/// Resolving is a round trip or two to the host's service, so it is never
/// done on the window's thread: each shell runs [`Layers::switch_to`] where it
/// runs its other network work — the desktop's runtime, the browser's worker.
#[derive(Clone)]
pub(crate) struct Layers {
    sources: Arc<dyn Sources>,
    store: Option<Arc<dyn ContentStore>>,
    switch: SwitchableImagery,
    /// Layers already resolved, so that going back to one costs nothing — and
    /// does not open a second session with a service that counts them.
    resolved: Arc<Mutex<HashMap<usize, Arc<dyn ImageryProvider>>>>,
    /// The substitute lane the globe was built over, when some layer of the
    /// host's has a substitute: switched with the layer that names one.
    /// `None` otherwise — and then the loader has no substitute at all.
    substitute: Option<SwitchableImagery>,
}

impl Layers {
    /// Drapes `choice`, the host's layer `layer`. Returns the imagery
    /// generation now being draped, or why it could not be.
    pub(crate) async fn switch_to(
        &self,
        layer: usize,
        choice: &ImageryChoice,
    ) -> Result<u64, String> {
        let known = self
            .resolved
            .lock()
            .ok()
            .and_then(|r| r.get(&layer).cloned());
        let provider = match known {
            Some(provider) => provider,
            None => imagery(&self.sources, choice, self.store.as_ref())
                .await
                .map_err(|e| e.to_string())?,
        };
        if let Ok(mut resolved) = self.resolved.lock() {
            resolved.insert(layer, Arc::clone(&provider));
        }
        // The layer's substitute first: its drapes are only asked for once
        // the fallback specification that names it is sent, after this.
        if let (Some(lane), Some(sub)) = (&self.substitute, choice.substitute_layer()) {
            // One that cannot be resolved leaves the layer's own pictures in
            // its lane: the ground is the source's, never dark (§3.4).
            let provider = match imagery(&self.sources, &sub, self.store.as_ref()).await {
                Ok(substitute) => substitute,
                Err(e) => {
                    tracing::warn!(layer = %choice.key, "its substitute is not used: {e}");
                    Arc::clone(&provider)
                }
            };
            lane.switch_to(provider);
        }
        Ok(self.switch.switch_to(provider))
    }
}

/// One imagery layer of the host's, through the store when there is one.
///
/// Namespaced by layer, and it matters: the store is keyed by tile address, so
/// two layers under one namespace would serve whichever was stored first —
/// silently, and looking exactly like a working session showing the wrong
/// picture.
///
/// A layer whose tiling scheme the store remembers is not resolved here: it
/// is drawn from the store at once and its origin resolved when it answers
/// (see [`tuile_planetary::reach`]), so a session starts with no network. One
/// never resolved before is resolved now, and its scheme remembered.
pub(crate) async fn imagery(
    sources: &Arc<dyn Sources>,
    choice: &ImageryChoice,
    store: Option<&Arc<dyn ContentStore>>,
) -> anyhow::Result<Arc<dyn ImageryProvider>> {
    let memory = store.map(|store| Memory::imagery(Arc::clone(store), &choice.cache));
    let remembered = match &memory {
        Some(memory) => memory.scheme().await,
        None => None,
    };
    let provider = match remembered {
        Some(scheme) => {
            tracing::info!(layer = %choice.name, "imagery on its remembered grid");
            DeferredImagery::pending(Arc::clone(sources), choice.asset, scheme, memory)
        }
        None => {
            let provider = sources
                .imagery(choice.asset)
                .await
                .map_err(|why| anyhow::anyhow!("imagery {:?}: {why}", choice.name))?;
            if let Some(memory) = &memory {
                memory.keep_scheme(&provider.tiling_scheme()).await;
            }
            DeferredImagery::resolved(Arc::clone(sources), choice.asset, provider, memory)
        }
    };
    let provider: Arc<dyn ImageryProvider> = Arc::new(provider);
    Ok(match store {
        Some(store) => Arc::new(CachedImagery::new(
            Held(provider),
            Arc::clone(store),
            choice.cache.clone(),
        )),
        None => provider,
    })
}

/// The host's terrain: its `layer.json` and its tiles, through the store when
/// there is one — on the description the store remembers, when it does, with
/// the origin resolved when it answers; resolved now otherwise. See
/// [`imagery`].
async fn terrain_of(
    sources: &Arc<dyn Sources>,
    terrain: &TerrainChoice,
    store: Option<&Arc<dyn ContentStore>>,
) -> anyhow::Result<(LayerJson, Arc<dyn TerrainSource>)> {
    let memory = store.map(|store| Memory::terrain(Arc::clone(store), &terrain.cache));
    let remembered = match &memory {
        Some(memory) => memory.layer().await,
        None => None,
    };
    let (layer, ground) = match remembered {
        Some(layer) => {
            tracing::info!("terrain on its remembered description");
            let ground = DeferredTerrain::pending(Arc::clone(sources), terrain.asset, memory);
            (layer, ground)
        }
        None => {
            let (layer, ground) = sources
                .terrain(terrain.asset)
                .await
                .map_err(|why| anyhow::anyhow!("terrain: {why}"))?;
            if let Some(memory) = &memory {
                memory.keep_layer(&layer).await;
            }
            let ground =
                DeferredTerrain::resolved(Arc::clone(sources), terrain.asset, ground, memory);
            (layer, ground)
        }
    };
    let ground: Arc<dyn TerrainSource> = Arc::new(ground);
    Ok(match store {
        Some(store) => {
            let namespace = terrain.namespace_for(&layer);
            let cached = CachedTerrain::new(Held(ground), Arc::clone(store), namespace);
            (layer, Arc::new(cached))
        }
        None => (layer, ground),
    })
}

/// How deep the quadtree divides when the host offers several layers.
///
/// With one layer the tree goes as deep as that imagery does. With several it
/// cannot follow the layer being shown — the tree is built once — so it goes
/// as deep as the sharpest photography in general use, and a session that
/// opened on a coarse mosaic still has the tiles a sharper layer needs when
/// it is switched to. Under a coarse layer the extra depth costs tiles only
/// where the eye is close enough to select them.
const DEPTH_FOR_ANY_LAYER: u32 = 21;

/// Resolves the host's terrain and its opening imagery layer and crosses them.
///
/// `store` keeps tiles between runs where the shell has one; `decode` is where
/// decoding and resampling run — a pool of threads on a desktop, inline in a
/// browser's worker, which has one thread.
pub(crate) async fn assemble(
    sources: Arc<dyn Sources>,
    store: Option<Arc<dyn ContentStore>>,
    terrain: &TerrainChoice,
    layers: &[ImageryChoice],
    opening: usize,
    decode: Arc<dyn Offload>,
    stored_imagery: tuile_core::texels::StoredPlan,
) -> anyhow::Result<Globe> {
    let opening_layer = layers
        .get(opening)
        .ok_or_else(|| anyhow::anyhow!("the host offers no imagery layer"))?;
    let (layer, ground) = terrain_of(&sources, terrain, store.as_ref()).await?;
    let picture = imagery(&sources, opening_layer, store.as_ref()).await?;
    tracing::info!(layer = %opening_layer.name, "imagery");
    let deepest = picture.tiling_scheme().maximum_level;
    // The globe is built over a switch, not over the layer: the layer can
    // then be changed under a running session, and the engine refreshes each
    // tile behind the old picture. See `tuile_planetary::SwitchableImagery`.
    let switch = SwitchableImagery::new(Arc::clone(&picture));
    // A substitute lane only where some layer names a substitute: it starts
    // on the opening layer's, or on the opening picture, which nothing asks
    // of it until a fallback specification names it.
    let substitute = if layers.iter().any(|l| l.substitute.is_some()) {
        // One that cannot be resolved leaves the opening layer's own
        // pictures in the lane: a session never fails to open, nor the
        // ground goes dark, for want of a substitute (the substitution doc,
        // §3.4).
        let first = match opening_layer.substitute_layer() {
            Some(sub) => match imagery(&sources, &sub, store.as_ref()).await {
                Ok(substitute) => substitute,
                Err(e) => {
                    tracing::warn!(
                        layer = %opening_layer.key,
                        "its substitute is not used: {e}"
                    );
                    Arc::clone(&picture)
                }
            },
            None => Arc::clone(&picture),
        };
        Some(SwitchableImagery::new(first))
    } else {
        None
    };

    // How many imagery layers a drape may carry is the renderer's answer, and
    // the renderer does not exist yet — the window comes after the stream. The
    // handle is passed now and filled in by `start_window`; until then it reads
    // the correctness floor.
    let budget = LayerBudget::default();
    let plan = ImageryPlanHandle::default();
    let opts = GlobeOptions {
        imagery_slots: budget.clone(),
        imagery_plan: plan.clone(),
        // What the person's choice does with tiles stored already compressed;
        // off, they are unpacked to RGBA8.
        stored_imagery,
        max_level: Some(if layers.len() > 1 {
            deepest.max(DEPTH_FOR_ANY_LAYER)
        } else {
            deepest
        }),
        substitute: substitute.clone().map(|lane| {
            tuile_planetary::SubstituteImagery(Arc::new(lane) as Arc<dyn ImageryProvider>)
        }),
        ..Default::default()
    };
    let (tree, loader, heights) = globe_on(Held(ground), switch.clone(), layer, opts, decode);
    // The worker's relief stays in the worker: the page keeps its own.
    #[cfg(target_arch = "wasm32")]
    drop(heights);
    Ok(Globe {
        tree,
        loader,
        #[cfg(not(target_arch = "wasm32"))]
        heights,
        budget,
        plan,
        layers: Layers {
            sources,
            store,
            switch,
            resolved: Arc::new(Mutex::new(HashMap::from([(opening, picture)]))),
            substitute,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Sources that say what was asked of them, and have nothing.
    #[derive(Default)]
    struct Asked(Mutex<Vec<String>>);

    #[async_trait::async_trait]
    impl Sources for Asked {
        async fn terrain(
            &self,
            asset: i64,
        ) -> Result<(tuile_terrain::LayerJson, Arc<dyn TerrainSource>), String> {
            self.0
                .lock()
                .expect("asked")
                .push(format!("terrain {asset}"));
            Err("no terrain here".to_owned())
        }

        async fn imagery(&self, asset: i64) -> Result<Arc<dyn ImageryProvider>, String> {
            self.0
                .lock()
                .expect("asked")
                .push(format!("imagery {asset}"));
            Err("no imagery here".to_owned())
        }
    }

    /// The host's numbers are the numbers its sources are asked for, and a
    /// refusal reaches the person in the host's own words, with what it was
    /// about.
    #[test]
    fn the_globe_asks_the_hosts_sources_for_the_hosts_assets() {
        let asked = Arc::new(Asked::default());
        let terrain = TerrainChoice {
            asset: 41,
            cache: "t".into(),
        };
        let layer = ImageryChoice::new("survey", "Survey", 77, "© someone");
        let layers = [layer.clone()];
        let refused = futures_executor::block_on(assemble(
            Arc::clone(&asked) as Arc<dyn Sources>,
            None,
            &terrain,
            &layers,
            0,
            Arc::new(tuile_core::offload::Inline),
            tuile_core::texels::StoredPlan::OFF,
        ))
        .err()
        .expect("there is no terrain");
        assert_eq!(refused.to_string(), "terrain: no terrain here");
        assert_eq!(*asked.0.lock().expect("asked"), ["terrain 41"]);

        let asked_sources = Arc::clone(&asked) as Arc<dyn Sources>;
        let refused = futures_executor::block_on(imagery(&asked_sources, &layer, None))
            .err()
            .expect("there is no imagery");
        assert_eq!(refused.to_string(), "imagery \"Survey\": no imagery here");
        assert_eq!(
            *asked.0.lock().expect("asked"),
            ["terrain 41", "imagery 77"]
        );
    }

    /// An origin that serves, never answers, or refuses — as it is told —
    /// and counts what it is asked.
    struct Origin {
        mode: std::sync::atomic::AtomicU8,
        asked: std::sync::atomic::AtomicUsize,
    }

    const SERVES: u8 = 0;
    const HANGS: u8 = 1;
    const REFUSES: u8 = 2;

    impl Origin {
        fn set(&self, mode: u8) {
            self.mode.store(mode, std::sync::atomic::Ordering::SeqCst);
        }
        fn asked(&self) -> usize {
            self.asked.load(std::sync::atomic::Ordering::SeqCst)
        }
        async fn answer(&self) -> Result<(), String> {
            self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match self.mode.load(std::sync::atomic::Ordering::SeqCst) {
                HANGS => std::future::pending().await,
                REFUSES => Err("dns error: no such host".into()),
                _ => Ok(()),
            }
        }
    }

    /// The origin's tiles, once it is resolved.
    struct Served;

    #[async_trait::async_trait]
    impl TerrainSource for Served {
        async fn fetch_tile(
            &self,
            _coord: tuile_terrain::TileCoord,
        ) -> Result<tuile_core::fetch::Fetched<Vec<u8>>, tuile_terrain::TerrainSourceError>
        {
            Ok(tuile_core::fetch::Fetched::undated(b"mesh".to_vec()))
        }
    }

    #[async_trait::async_trait]
    impl ImageryProvider for Served {
        fn tiling_scheme(&self) -> tuile_core::raster::TilingScheme {
            tuile_core::raster::TilingScheme::web_mercator()
        }
        async fn fetch_tile_bytes(
            &self,
            _coord: tuile_core::raster::ImageryCoord,
        ) -> Result<tuile_core::fetch::Fetched<bytes::Bytes>, tuile_core::raster::RasterError>
        {
            Ok(tuile_core::fetch::Fetched::undated(
                bytes::Bytes::from_static(b"jpeg"),
            ))
        }
    }

    #[async_trait::async_trait]
    impl Sources for Origin {
        async fn terrain(
            &self,
            _asset: i64,
        ) -> Result<(tuile_terrain::LayerJson, Arc<dyn TerrainSource>), String> {
            self.answer().await?;
            let layer = tuile_terrain::LayerJson::from_slice(
                br#"{"format":"quantized-mesh-1.0","tiles":["{z}/{x}/{y}.terrain"],"maxzoom":12}"#,
            )
            .map_err(|e| e.to_string())?;
            Ok((layer, Arc::new(Served)))
        }
        async fn imagery(&self, _asset: i64) -> Result<Arc<dyn ImageryProvider>, String> {
            self.answer().await?;
            Ok(Arc::new(Served))
        }
    }

    /// A store in memory, as the disk's is between two sessions.
    #[derive(Default)]
    struct Kept(Mutex<HashMap<String, bytes::Bytes>>);

    #[async_trait::async_trait]
    impl ContentStore for Kept {
        async fn get(&self, key: &str) -> Option<bytes::Bytes> {
            self.0.lock().expect("kept").get(key).cloned()
        }
        async fn put(&self, key: &str, value: bytes::Bytes, _ttl: Option<std::time::Duration>) {
            self.0.lock().expect("kept").insert(key.to_owned(), value);
        }
        fn holds(&self, key: &str) -> bool {
            self.0.lock().expect("kept").contains_key(key)
        }
    }

    /// `work`, on a thread of its own, or `None` if it has not finished
    /// within `patience` — a test that waits on a network that never
    /// answers must fail, not hang.
    fn within<T: Send + 'static>(
        patience: std::time::Duration,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Option<T> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(work());
        });
        rx.recv_timeout(patience).ok()
    }

    const TILE: tuile_terrain::TileCoord = tuile_terrain::TileCoord {
        level: 2,
        x: 1,
        y: 1,
    };
    const ELSEWHERE: tuile_terrain::TileCoord = tuile_terrain::TileCoord {
        level: 2,
        x: 3,
        y: 0,
    };
    const PICTURE: tuile_core::raster::ImageryCoord = tuile_core::raster::ImageryCoord {
        level: 2,
        x: 1,
        y: 1,
    };

    /// **A session whose origins are out of reach starts at once on what the
    /// store holds, draws it, and goes back online by itself when the origin
    /// answers again** — the path a desktop and a page both assemble their
    /// globe on. Red before the descriptions were remembered: the second
    /// globe waited on the origin for ever.
    #[test]
    fn a_globe_starts_offline_on_its_store_and_comes_back_online() {
        let origin = Arc::new(Origin {
            mode: std::sync::atomic::AtomicU8::new(SERVES),
            asked: std::sync::atomic::AtomicUsize::new(0),
        });
        let sources = Arc::clone(&origin) as Arc<dyn Sources>;
        let store: Arc<dyn ContentStore> = Arc::new(Kept::default());
        let terrain = TerrainChoice {
            asset: 1,
            cache: "t".into(),
        };
        let layer = ImageryChoice::new("aerial", "Aerial", 2, "©");
        let layers = vec![layer.clone()];

        // Online: a session that saw one tile of each.
        let (_, ground) =
            futures_executor::block_on(terrain_of(&sources, &terrain, Some(&store))).expect("up");
        let picture =
            futures_executor::block_on(imagery(&sources, &layer, Some(&store))).expect("up");
        futures_executor::block_on(ground.fetch_tile(TILE)).expect("a tile");
        futures_executor::block_on(picture.fetch_tile_bytes(PICTURE)).expect("a picture");
        assert_eq!(origin.asked(), 2);

        // The network goes; a new session starts on the store, within a
        // breath, asking the origin nothing.
        origin.set(HANGS);
        let (s, k, t, l) = (
            Arc::clone(&sources),
            Arc::clone(&store),
            terrain.clone(),
            layers.clone(),
        );
        let globe = within(std::time::Duration::from_secs(2), move || {
            futures_executor::block_on(assemble(
                s,
                Some(k),
                &t,
                &l,
                0,
                Arc::new(tuile_core::offload::Inline),
                tuile_core::texels::StoredPlan::OFF,
            ))
            .map(|_| ())
            .map_err(|e| e.to_string())
        })
        .expect("the globe waited on an origin out of reach");
        globe.expect("the globe came up");
        assert_eq!(origin.asked(), 2, "the origin was asked at start-up");

        // What the store holds is drawn, with the origin still hanging.
        let (s, k, t) = (Arc::clone(&sources), Arc::clone(&store), terrain.clone());
        let (l2, k2, s2) = (layer.clone(), Arc::clone(&store), Arc::clone(&sources));
        let served = within(std::time::Duration::from_secs(2), move || {
            futures_executor::block_on(async move {
                let (_, ground) = terrain_of(&s, &t, Some(&k)).await.expect("up");
                let picture = imagery(&s2, &l2, Some(&k2)).await.expect("up");
                (
                    ground.fetch_tile(TILE).await.map(|f| f.value),
                    picture.fetch_tile_bytes(PICTURE).await.map(|f| f.value),
                )
            })
        })
        .expect("a stored tile waited on the origin");
        assert_eq!(served.0.expect("the stored mesh"), b"mesh".to_vec());
        assert_eq!(&served.1.expect("the stored picture")[..], b"jpeg");

        // Refused: what the store lacks fails at once — a failure, never an
        // absence, so it is not remembered as a hole — and the origin is not
        // asked again before its wait is over.
        origin.set(REFUSES);
        let (_, ground) =
            futures_executor::block_on(terrain_of(&sources, &terrain, Some(&store))).expect("up");
        let before = origin.asked();
        let first = futures_executor::block_on(ground.fetch_tile(ELSEWHERE));
        assert!(!first.expect_err("refused").is_absence());
        assert!(futures_executor::block_on(ground.fetch_tile(ELSEWHERE)).is_err());
        assert_eq!(origin.asked(), before + 1, "asked during its wait");

        // The network comes back: the next request after the wait resolves
        // the origin, with no new session, and gets its tile.
        origin.set(SERVES);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let back = futures_executor::block_on(ground.fetch_tile(ELSEWHERE)).expect("back online");
        assert_eq!(back.value, b"mesh".to_vec());
        assert_eq!(origin.asked(), before + 2);
    }
}
