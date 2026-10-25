// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Bringing the geometry server up, off the frame.
//!
//! Resolving the sources is a round trip or two to a server, and the frame
//! must not wait for it: the globe's pass draws its backdrop from the first
//! frame, and the session joins it when it is up. The outcome is left in a
//! [`SessionSlot`] the render world takes it from.

use std::sync::{Arc, Mutex};

use tuile_core::protocol::InProcessStream;
use tuile_core::texels::{GpuTextureCaps, ImageryCompression};
use tuile_planetary::{globe_on, GlobeOptions, ImageryPlanHandle, LayerBudget};
use tuile_terrain::TerrainHeights;

use crate::budget::TuileBudget;
use crate::sources::GlobeSources;

/// A running session: the client end of the stream, and the handles the
/// renderer completes once the device is known.
pub(crate) struct Started {
    pub stream: InProcessStream,
    /// The relief recorded as tiles decode.
    #[allow(dead_code)]
    pub heights: Arc<TerrainHeights>,
    pub layer_budget: LayerBudget,
    pub plan: ImageryPlanHandle,
}

/// Where a launch leaves its outcome: empty while it runs.
#[derive(Clone, Default)]
pub(crate) struct SessionSlot(pub Arc<Mutex<Option<Result<Started, String>>>>);

impl SessionSlot {
    pub(crate) fn take(&self) -> Option<Result<Started, String>> {
        self.0.lock().ok()?.take()
    }

    fn put(&self, outcome: Result<Started, String>) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = Some(outcome);
        }
    }
}

/// What a launch needs, all of it known when the plugin is finished.
pub(crate) struct Plan {
    pub sources: GlobeSources,
    pub budget: TuileBudget,
    pub compression: ImageryCompression,
    /// What the adapter can sample, for imagery stored already compressed.
    pub caps: GpuTextureCaps,
}

/// The runtime the loads, the network and the server run on: the plugin's
/// own, since a Bevy application has none.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct Runtime(Option<tokio::runtime::Runtime>);

#[cfg(not(target_arch = "wasm32"))]
impl Runtime {
    pub(crate) fn new(workers: usize) -> Result<Self, String> {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers.max(1))
            .thread_name("tuile")
            .enable_all()
            .build()
            .map(|r| Self(Some(r)))
            .map_err(|e| format!("the tile runtime: {e}"))
    }

    pub(crate) fn handle(&self) -> Option<tokio::runtime::Handle> {
        self.0.as_ref().map(|r| r.handle().clone())
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for Runtime {
    /// Without waiting for the loads in flight: the application is leaving.
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            runtime.shutdown_background();
        }
    }
}

/// Starts the launch on `runtime` and returns at once.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn launch(runtime: &tokio::runtime::Handle, plan: Plan) -> SessionSlot {
    let slot = SessionSlot::default();
    let out = slot.clone();
    let server_runtime = runtime.clone();
    runtime.spawn(async move {
        let outcome = bring_up(plan, server_runtime).await;
        if let Err(why) = &outcome {
            tracing::error!("the globe's session did not start: {why}");
        }
        out.put(outcome);
    });
    slot
}

#[cfg(not(target_arch = "wasm32"))]
async fn bring_up(plan: Plan, runtime: tokio::runtime::Handle) -> Result<Started, String> {
    let resolved = plan.sources.resolve().await?;
    let decode = Arc::new(tuile_core::offload::ThreadPool::with_threads(
        plan.budget.decode_threads.max(1),
    ));
    let layer_budget = LayerBudget::default();
    let options = GlobeOptions {
        imagery_slots: layer_budget.clone(),
        max_level: Some(resolved.imagery.tiling_scheme().maximum_level),
        ..Default::default()
    }
    .with_imagery_compression(plan.compression, plan.caps);
    let imagery_plan = options.imagery_plan.clone();
    let (tree, loader, heights) = globe_on(
        tuile_planetary::Held(resolved.terrain),
        tuile_planetary::Held(resolved.imagery),
        resolved.layer,
        options,
        decode,
    );
    let (stream, server) = tuile_core::runtime::in_process_with(
        tree,
        loader,
        plan.budget.traversal(),
        plan.budget.config(),
    )
    .map_err(|e| format!("the graphics budget cannot run: {e}"))?;
    let server = server.with_spawn(tuile_core::runtime::Spawn::Tokio(runtime.clone()));
    // Its own thread, blocking on the runtime: the session is one long
    // future the frame talks to over channels. It ends when the stream is
    // dropped.
    std::thread::Builder::new()
        .name("tuile-geometry-server".into())
        .spawn(move || runtime.block_on(server.run()))
        .map_err(|e| format!("the geometry server could not start: {e}"))?;
    Ok(Started {
        stream,
        heights,
        layer_budget,
        plan: imagery_plan,
    })
}
