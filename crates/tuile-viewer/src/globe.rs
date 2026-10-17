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
    globe_on, GlobeOptions, Held, ImageryDetail, LayerBudget, Sources, SwitchableImagery,
};
use tuile_terrain::{CachedTerrain, TerrainSource};

use crate::embed::{ImageryChoice, TerrainChoice};

/// Everything the session needs from the crossing.
pub(crate) struct Globe {
    pub tree: Box<dyn TileTree>,
    pub loader: Arc<dyn TileLoader>,
    pub detail: ImageryDetail,
    /// The relief the camera stands on. A page records its own from the
    /// content it is sent, on the other side of the worker.
    #[cfg(not(target_arch = "wasm32"))]
    pub heights: Arc<tuile_terrain::TerrainHeights>,
    pub budget: LayerBudget,
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
            None => imagery(self.sources.as_ref(), choice, self.store.as_ref())
                .await
                .map_err(|e| e.to_string())?,
        };
        if let Ok(mut resolved) = self.resolved.lock() {
            resolved.insert(layer, Arc::clone(&provider));
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
pub(crate) async fn imagery(
    sources: &dyn Sources,
    choice: &ImageryChoice,
    store: Option<&Arc<dyn ContentStore>>,
) -> anyhow::Result<Arc<dyn ImageryProvider>> {
    let provider = sources
        .imagery(choice.asset)
        .await
        .map_err(|why| anyhow::anyhow!("imagery {:?}: {why}", choice.name))?;
    Ok(match store {
        Some(store) => Arc::new(CachedImagery::new(
            Held(provider),
            Arc::clone(store),
            choice.cache.clone(),
        )),
        None => provider,
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
) -> anyhow::Result<Globe> {
    let opening_layer = layers
        .get(opening)
        .ok_or_else(|| anyhow::anyhow!("the host offers no imagery layer"))?;
    let (sources_kept, sources) = (Arc::clone(&sources), sources.as_ref());

    let (layer, ground) = sources
        .terrain(terrain.asset)
        .await
        .map_err(|why| anyhow::anyhow!("terrain: {why}"))?;
    let ground: Arc<dyn TerrainSource> = match &store {
        Some(store) => Arc::new(CachedTerrain::new(
            Held(ground),
            Arc::clone(store),
            terrain.cache.clone(),
        )),
        None => ground,
    };
    let picture = imagery(sources, opening_layer, store.as_ref()).await?;
    tracing::info!(layer = %opening_layer.name, "imagery");
    let deepest = picture.tiling_scheme().maximum_level;
    // The globe is built over a switch, not over the layer: the layer can
    // then be changed under a running session, and the engine refreshes each
    // tile behind the old picture. See `tuile_planetary::SwitchableImagery`.
    let switch = SwitchableImagery::new(Arc::clone(&picture));

    // How many imagery layers a drape may carry is the renderer's answer, and
    // the renderer does not exist yet — the window comes after the stream. The
    // handle is passed now and filled in by `start_window`; until then it reads
    // the correctness floor.
    let budget = LayerBudget::default();
    let opts = GlobeOptions {
        imagery_slots: budget.clone(),
        max_level: Some(if layers.len() > 1 {
            deepest.max(DEPTH_FOR_ANY_LAYER)
        } else {
            deepest
        }),
        ..Default::default()
    };
    let (tree, loader, detail, heights) =
        globe_on(Held(ground), switch.clone(), layer, opts, decode);
    // The worker's relief stays in the worker: the page keeps its own.
    #[cfg(target_arch = "wasm32")]
    drop(heights);
    Ok(Globe {
        tree,
        loader,
        detail,
        #[cfg(not(target_arch = "wasm32"))]
        heights,
        budget,
        layers: Layers {
            sources: sources_kept,
            store,
            switch,
            resolved: Arc::new(Mutex::new(HashMap::from([(opening, picture)]))),
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
        ))
        .err()
        .expect("there is no terrain");
        assert_eq!(refused.to_string(), "terrain: no terrain here");
        assert_eq!(*asked.0.lock().expect("asked"), ["terrain 41"]);

        let refused = futures_executor::block_on(imagery(asked.as_ref(), &layer, None))
            .err()
            .expect("there is no imagery");
        assert_eq!(refused.to_string(), "imagery \"Survey\": no imagery here");
        assert_eq!(
            *asked.0.lock().expect("asked"),
            ["terrain 41", "imagery 77"]
        );
    }
}
