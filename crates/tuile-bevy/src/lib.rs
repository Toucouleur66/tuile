// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! # tuile-bevy
//!
//! The streamed globe of tuile — OGC 3D Tiles terrain with imagery draped on
//! it — inside a Bevy application, drawn on the application's own device,
//! into its own colour and depth, sharing depth with everything else it
//! draws.
//!
//! Nothing of the ground is ported. tuile-wgpu's renderer and its graphics
//! budget's executor run on Bevy's `RenderDevice` and `RenderQueue`, from a
//! system in Bevy's 3D schedule, through `tuile_wgpu::HostPass`: one ground
//! shader and one set of rules against black ground, shared with every other
//! tuile client.
//!
//! ```no_run
//! use bevy::prelude::*;
//! use tuile_bevy::{GlobeSources, TuileCamera, TuilePlugin, XyzTemplate};
//!
//! let terrain = "http://127.0.0.1:8092/terrain/layer.json".parse().expect("a URL");
//! let imagery = XyzTemplate::geographic("http://127.0.0.1:8092/imagery/{z}/{x}/{y}.png");
//! App::new()
//!     .add_plugins(DefaultPlugins)
//!     .add_plugins(TuilePlugin::new(GlobeSources::addresses(terrain, imagery)))
//!     .add_systems(Startup, |mut commands: Commands| {
//!         commands.spawn((
//!             Camera3d::default(),
//!             TuileCamera::looking(10.0, 0.0, 3_000.0, 0.0, 45.0, 35.0),
//!         ));
//!     })
//!     .run();
//! ```
//!
//! # What the host does
//!
//! - **Drives the camera** — a [`TuileCamera`] on its `Camera3d`, an `f64`
//!   pose the plugin turns into the camera's transform and lens every frame
//!   (see [`camera`] for the floating origin under it).
//! - **Places its own entities** with a [`GeoAnchor`], in `f64`, and draws
//!   them in its own pipelines: Bevy's main pass runs after the globe's and
//!   is depth-tested against the ground.
//! - **Says what it will need** with [`PreloadViews`]: poses traversed this
//!   frame and never drawn.
//! - **Sizes the globe's share** of the device with a [`TuileBudget`], and
//!   chooses whether imagery is held compressed
//!   ([`ImageryCompression`], off unless asked).
//!
//! # In a browser
//!
//! The same plugin, on WebGPU or WebGL2 (one Bevy build each). The session
//! runs on the page's thread and decodes in tuile's web decode workers: see
//! `web` (wasm32 only) for the three things the page does. On WebGL2 a camera draws
//! single-sampled (`Msaa::Off`): it can neither read a multisampled depth nor
//! shade per sample, which sharing a stencil-less depth needs.
//!
//! # Logging
//!
//! The engine traces every tile decision under the target `tuile_det`, at
//! `info`, for determinism checks. A host's console wants
//! `tuile_det=off` in its `LogPlugin` filter.
//!
//! The camera's depth must be bindable for the ground to share it; the plugin
//! adds `TEXTURE_BINDING` to a [`TuileCamera`]'s `Camera3d` itself. Its
//! perspective's `far` is the host's frustum culling distance for its own
//! meshes and nothing else: Bevy's projection has no far plane.

pub mod budget;
pub mod camera;
mod render;
mod session;
pub mod sources;
mod status;
#[cfg(target_arch = "wasm32")]
pub mod web;

use std::sync::Arc;

use bevy::prelude::*;
use bevy::transform::TransformSystems;

pub use budget::TuileBudget;
pub use camera::{
    local_frame, FloatingOrigin, GeoAnchor, PreloadView, PreloadViews, Priority, TuileCamera,
};
pub use render::BARE_GROUND;
pub use sources::{GlobeSources, ImagerySpec, TerrainSpec, XyzTemplate};
pub use status::{SessionState, TuileLight, TuileStatus};
pub use tuile_camera::GlobeCamera;
pub use tuile_core::texels::ImageryCompression;
pub use tuile_wgpu::compression_named;

/// The globe, in a Bevy application: see the crate.
#[derive(Clone)]
pub struct TuilePlugin {
    pub sources: GlobeSources,
    pub budget: TuileBudget,
    /// How imagery is held on the GPU: the host's choice, RGBA8 unless it
    /// asks. Resolved against the device; a format the device cannot sample
    /// falls back to RGBA8.
    pub compression: ImageryCompression,
    /// In a browser, the module worker script the decode workers are made
    /// from (`web` (wasm32 only)); `None` decodes on the page's thread. Ignored
    /// elsewhere, where decoding has threads of its own.
    pub decode_workers: Option<String>,
}

impl TuilePlugin {
    /// The globe made of `sources`, on the platform's default budget, with
    /// imagery uncompressed.
    pub fn new(sources: GlobeSources) -> Self {
        Self {
            sources,
            budget: TuileBudget::default(),
            compression: ImageryCompression::Off,
            decode_workers: None,
        }
    }

    /// In a browser, decode in workers made from the module worker script at
    /// `script`, relative to the page (`web` (wasm32 only)).
    #[must_use]
    pub fn with_decode_workers(mut self, script: impl Into<String>) -> Self {
        self.decode_workers = Some(script.into());
        self
    }

    #[must_use]
    pub fn with_budget(mut self, budget: TuileBudget) -> Self {
        self.budget = budget;
        self
    }

    #[must_use]
    pub fn with_compression(mut self, compression: ImageryCompression) -> Self {
        self.compression = compression;
        self
    }
}

impl Plugin for TuilePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<FloatingOrigin>()
            .init_resource::<PreloadViews>()
            .init_resource::<TuileLight>()
            .init_resource::<TuileStatus>()
            .init_resource::<status::SharedStatus>()
            .add_systems(First, camera::clear_preload_views)
            .add_systems(First, status::publish_status)
            .add_systems(
                PostUpdate,
                camera::place_in_world.before(TransformSystems::Propagate),
            );
    }

    /// The session starts here, once the renderer has said what its adapter
    /// can sample.
    fn finish(&self, app: &mut App) {
        let caps = app
            .world()
            .get_resource::<bevy::render::renderer::RenderAdapter>()
            .map(|adapter| tuile_wgpu::texture_caps(adapter))
            .unwrap_or_default();
        let shared = app.world().resource::<status::SharedStatus>().clone();
        let plan = session::Plan {
            sources: self.sources.clone(),
            budget: self.budget.clone(),
            compression: self.compression,
            caps,
            decode_workers: self.decode_workers.clone(),
        };
        let slot = start(app, plan, &shared);
        render::add_to(
            app,
            render::GlobeRender::new(slot, self.compression, Arc::clone(&shared.0)),
        );
    }
}

/// Starts the session on a runtime of the plugin's own, kept as long as the
/// application.
#[cfg(not(target_arch = "wasm32"))]
fn start(
    app: &mut App,
    plan: session::Plan,
    shared: &status::SharedStatus,
) -> session::SessionSlot {
    match session::Runtime::new(plan.budget.runtime_workers) {
        Ok(runtime) => {
            let slot = runtime
                .handle()
                .map(|handle| session::launch(&handle, plan))
                .unwrap_or_default();
            app.insert_resource(RuntimeKeeper(Arc::new(runtime)));
            slot
        }
        Err(why) => {
            if let Ok(mut status) = shared.0.lock() {
                status.session = SessionState::Failed(why);
            }
            session::SessionSlot::default()
        }
    }
}

/// Starts the session as a future on the page's thread.
#[cfg(target_arch = "wasm32")]
fn start(
    _app: &mut App,
    plan: session::Plan,
    _shared: &status::SharedStatus,
) -> session::SessionSlot {
    if let Some(script) = &plan.decode_workers {
        web::start_decode_workers(script, None);
    }
    session::launch(plan)
}

/// Keeps the plugin's runtime alive for the application's life.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Resource, Clone)]
struct RuntimeKeeper(#[allow(dead_code)] Arc<session::Runtime>);
