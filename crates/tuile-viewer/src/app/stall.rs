// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The frame's promise, checked: whatever the network does, a frame returns.
//!
//! A phone's system ends an application whose main thread stops answering,
//! and the one way the viewer's main thread could stop is by waiting on a
//! tile. It never does by design — the stream is drained with a poll, uploads
//! take what has *already* been decoded, and everything that waits does so on
//! the runtime — and this test holds it to that with the worst network there
//! is: one that accepts every request and never answers any.
//!
//! The frame step is the window's own: [`tuile_wgpu::ContentPump::advance`]
//! within the frame's upload deadline, on a real (headless) GPU, against a real
//! geometry server whose loader is wrapped in the same gate the window uses.
//! It is run for [`FRAMES`] frames. A frame that waited on this network would
//! wait forever, so the promise is checked as "every frame comes back", with a
//! watchdog that turns a hang into a failure, and each frame within
//! [`FRAME_LIMIT`] — far below the seconds a phone's system allows, far above
//! what a slow shared CI GPU needs. The loading indicator must be up, at full
//! strength, throughout. Skipped, with a line, on a machine without a GPU
//! adapter.

#![cfg(test)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use web_time::Instant;

use glam::{DVec2, DVec3};
use tuile_core::math::{BoundingVolume, Sphere};
use tuile_core::source::{LoadError, Loaded, TileId, TileLoader, TileProperties, TileTree};
use tuile_core::tileset::Refine;

use super::loading::{Loading, Observed, Style};
use crate::gate::{Gate, Gated};
use crate::profile::{DeviceMemory, Profile};

/// How many frames the stalled network is held for.
const FRAMES: usize = 60;
/// The longest a single frame may take. A frame that waits on the network
/// never returns at all; this bound only catches a frame that waits *for a
/// while*, and leaves room for software rasterizers on shared runners.
const FRAME_LIMIT: Duration = Duration::from_millis(250);
/// After this long the test fails instead of hanging.
const WATCHDOG: Duration = Duration::from_secs(60);

/// One tile the size of the planet, with children, all of it on screen.
struct Planet;

impl TileTree for Planet {
    fn roots(&self) -> Vec<TileId> {
        vec![TileId(1)]
    }

    fn children(&self, id: TileId) -> Vec<TileId> {
        if id.0 < 64 {
            (0..4).map(|i| TileId(id.0 * 4 + i)).collect()
        } else {
            Vec::new()
        }
    }

    fn properties(&self, _id: TileId) -> TileProperties {
        TileProperties {
            bounding_volume: BoundingVolume::Sphere(Sphere {
                center: DVec3::ZERO,
                radius: 6_378_137.0,
            }),
            geometric_error: 100_000.0,
            refine: Refine::Replace,
            has_content: true,
        }
    }
}

/// A loader on the network nobody answers: every load is accepted, counted,
/// and never finishes.
#[derive(Default)]
struct NeverAnswers(std::sync::atomic::AtomicUsize);

#[async_trait::async_trait]
impl TileLoader for NeverAnswers {
    async fn load(&self, _id: TileId) -> Result<Loaded, LoadError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        std::future::pending().await
    }
}

#[test]
fn a_frame_returns_and_the_indicator_turns_while_the_network_stalls() {
    let gpu = match pollster::block_on(tuile_wgpu::GpuContext::headless()) {
        Ok(gpu) => gpu,
        Err(e) => {
            eprintln!("SKIP: no GPU adapter ({e})");
            return;
        }
    };
    let profile = Profile::handheld(
        DeviceMemory {
            physical: 6 << 30,
            available: Some(3 << 30),
        },
        6,
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(profile.runtime_workers)
        .enable_all()
        .build()
        .expect("a runtime");
    let asked = Arc::new(NeverAnswers::default());
    let loader: Arc<dyn TileLoader> = Arc::new(Gated {
        inner: Arc::clone(&asked) as Arc<dyn TileLoader>,
        gate: Gate::default(),
    });
    let (mut stream, server) = tuile_core::runtime::in_process_with(
        Box::new(Planet),
        loader,
        profile.traversal(),
        profile.budget(),
    )
    .expect("the profile's budget is valid");
    let handle = runtime.handle().clone();
    let _server = std::thread::spawn(move || handle.block_on(server.run()));

    let camera = crate::start::StartView::default().camera();
    let viewport = DVec2::new(1179.0, 2556.0);
    let mut pump = tuile_wgpu::ContentPump::new(camera.position);
    let mut loading = Loading::new(Instant::now());
    let finished = Arc::new(AtomicBool::new(false));
    let watched = finished.clone();
    std::thread::spawn(move || {
        std::thread::sleep(WATCHDOG);
        if !watched.load(Ordering::SeqCst) {
            eprintln!("a frame never came back from a network that never answers");
            std::process::exit(1);
        }
    });
    let (mut frames, mut slowest) = (0, Duration::ZERO);
    while frames < FRAMES {
        let t = Instant::now();
        let deadline = pump.deadline(60);
        pump.advance(
            &mut stream,
            &gpu,
            &[camera.view_state(viewport)],
            camera.position,
            deadline,
        );
        let (_, resolution) = pump.resolve(&gpu);
        let indicator = loading.step(
            Instant::now(),
            Observed {
                ready: true,
                warm: false,
                selected: resolution.exact + resolution.coarser + resolution.lost,
                lost: resolution.lost,
                coarser: resolution.coarser,
                loading: pump.stats.requested as usize,
                progress: Some(0.0),
                failed: false,
            },
        );
        slowest = slowest.max(t.elapsed());
        frames += 1;
        assert_eq!(indicator.style, Style::Launch);
        assert_eq!(
            indicator.opacity, 1.0,
            "the indicator let go at frame {frames}"
        );
    }
    finished.store(true, Ordering::SeqCst);
    assert!(
        slowest < FRAME_LIMIT,
        "a frame waited {slowest:?} on a network that never answers"
    );
    assert!(!pump.closed, "the server stopped");
    // The stall was real: the server asked the loader for tiles, and not one
    // of them came back while the frames above ran.
    // The frames are quick; the server's first pass may come after them.
    let asked_by = Instant::now() + Duration::from_secs(5);
    while asked.0.load(Ordering::SeqCst) == 0 && Instant::now() < asked_by {
        let deadline = pump.deadline(60);
        pump.advance(
            &mut stream,
            &gpu,
            &[camera.view_state(viewport)],
            camera.position,
            deadline,
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        asked.0.load(Ordering::SeqCst) > 0,
        "the loader was never asked: nothing stalled"
    );
    assert_eq!(pump.prepared_count(), 0);
    drop(stream);
    runtime.shutdown_background();
}
