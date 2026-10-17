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
//! The frame step is the window's own: [`tuile_wgpu::ContentPump::advance_while`]
//! with the phone's upload budget, on a real (headless) GPU, against a real
//! geometry server whose loader is wrapped in the same gate the window uses.
//! It is run for two seconds of frames; each must come back within a frame's
//! time, and the loading indicator must be up, at full strength, throughout.
//! Skipped, with a line, on a machine without a GPU adapter.

#![cfg(test)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use glam::{DVec2, DVec3};
use tuile_core::math::{BoundingVolume, Sphere};
use tuile_core::source::{LoadError, Loaded, TileId, TileLoader, TileProperties, TileTree};
use tuile_core::tileset::Refine;

use super::loading::{Loading, Observed, Style};
use crate::gate::{Gate, Gated};
use crate::profile::{DeviceMemory, Profile};

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

/// A loader on the network nobody answers: every load is accepted and never
/// finishes.
struct NeverAnswers;

#[async_trait::async_trait]
impl TileLoader for NeverAnswers {
    async fn load(&self, _id: TileId) -> Result<Loaded, LoadError> {
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
    let loader: Arc<dyn TileLoader> = Arc::new(Gated {
        inner: Arc::new(NeverAnswers),
        gate: Gate::default(),
    });
    let (mut stream, server) = tuile_core::runtime::in_process_with(
        Box::new(Planet),
        loader,
        profile.traversal(Some(profile.pinned_level)),
    );
    let handle = runtime.handle().clone();
    let _server = std::thread::spawn(move || handle.block_on(server.run()));

    let camera = crate::start::StartView::default().camera();
    let viewport = DVec2::new(1179.0, 2556.0);
    let mut pump = tuile_wgpu::ContentPump::new(camera.position);
    let mut loading = Loading::new(Instant::now());
    let budget = profile.uploads;
    let (began, mut frames, mut slowest) = (Instant::now(), 0, Duration::ZERO);
    while began.elapsed() < Duration::from_secs(2) {
        let t = Instant::now();
        pump.advance_while(
            &mut stream,
            &gpu,
            &[camera.view_state(viewport)],
            camera.position,
            |done, spent| budget.allows(done, spent),
        );
        let (_, resolution) = pump.resolve(&gpu.queue);
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
        std::thread::sleep(Duration::from_millis(16));
    }
    assert!(frames > 60, "only {frames} frames in two seconds");
    assert!(
        slowest < Duration::from_millis(16),
        "a frame waited {slowest:?} on a network that never answers"
    );
    assert!(!pump.closed, "the server stopped");
    drop(stream);
    runtime.shutdown_background();
}
