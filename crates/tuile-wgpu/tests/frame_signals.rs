// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! **What a frame drew is published, and taken back when the pump goes.**
//!
//! The headless viewer reads a frame's state through the process's metrics —
//! the same numbers the exporter publishes — rather than through accessors of
//! its own. So the numbers have to move when the frame changes and stand
//! still when it does not, and a pump that is dropped must not leave its
//! residency counted for the next one.
//!
//! One test in this file, on purpose: the metrics are the process's, and two
//! tests writing them at once would read each other's numbers.

use std::collections::VecDeque;
use std::task::{Context, Poll};

use tuile_core::content::{DecodedTileContent, TileContent};
use tuile_core::geo::{geodetic_to_ecef, Geodetic};
use tuile_core::protocol::{Ancestry, ClientMessage, GeometryStream, ServerMessage, StreamError};
use tuile_core::source::TileId;
use tuile_core::traversal::TraversalStats;
use tuile_terrain::{fill_content, GeographicTilingScheme, TileCoord};
use tuile_wgpu::{ContentPump, GpuContext};

/// A GPU, or a stated reason there is none; `TUILE_REQUIRE_GPU=1` turns the
/// skip into a failure.
fn gpu() -> Option<GpuContext> {
    match pollster::block_on(GpuContext::headless()) {
        Ok(gpu) => Some(gpu),
        Err(e) => {
            assert!(
                std::env::var("TUILE_REQUIRE_GPU").is_err(),
                "TUILE_REQUIRE_GPU is set and there is no usable adapter: {e}"
            );
            eprintln!("skipping GPU test: no usable adapter ({e})");
            None
        }
    }
}

/// Hands the pump what it is given, one message at a time.
#[derive(Default)]
struct Scripted(VecDeque<ServerMessage>);

impl GeometryStream for Scripted {
    fn send(&self, _msg: ClientMessage) -> Result<(), StreamError> {
        Ok(())
    }

    fn poll_message(&mut self, _cx: &mut Context<'_>) -> Poll<Option<ServerMessage>> {
        match self.0.pop_front() {
            Some(msg) => Poll::Ready(Some(msg)),
            None => Poll::Pending,
        }
    }
}

fn ancestry(tile: TileId) -> Ancestry {
    let (z, x, y) = tile.terrain_coord();
    Ancestry {
        level: z,
        parent: (z > 0).then(|| TileId::from_terrain(z - 1, x / 2, y / 2)),
    }
}

fn shape(tiles: &[TileId]) -> Vec<(TileId, Ancestry)> {
    let mut out = Vec::new();
    for tile in tiles {
        let mut cur = Some(*tile);
        while let Some(id) = cur {
            out.push((id, ancestry(id)));
            cur = ancestry(id).parent;
        }
    }
    out
}

fn content(tile: TileId) -> DecodedTileContent {
    let (z, x, y) = tile.terrain_coord();
    let rect = GeographicTilingScheme::default().tile_rect(TileCoord::new(z, x, y));
    fill_content(&rect, [Some(0.0); 4], 0.0)
}

fn select(tiles: &[TileId]) -> ServerMessage {
    ServerMessage::Select {
        tiles: tiles.iter().map(|t| (*t, 0.0)).collect(),
        ancestry: shape(tiles),
        stats: TraversalStats::default(),
        generation: 0,
    }
}

fn arrive(tile: TileId) -> ServerMessage {
    ServerMessage::Content {
        tile,
        ancestry: ancestry(tile),
        content: TileContent::Decoded(content(tile)),
    }
}

#[test]
fn what_a_frame_drew_is_published_and_a_dropped_pump_takes_its_meshes_back() {
    let Some(gpu) = gpu() else { return };
    let m = tuile_core::metrics::metrics();
    let mesh_bytes_before = m.mesh_bytes_by_level.total();

    let tile = TileId::from_terrain(3, 4, 4);
    let parent = TileId::from_terrain(2, 2, 2);
    let eye = geodetic_to_ecef(Geodetic {
        lon: 0.0,
        lat: 0.0,
        height: 1.0e6,
    });
    let mut pump = ContentPump::new(eye);
    let mut stream = Scripted(VecDeque::from([select(&[tile])]));
    pump.pump(&mut stream, &gpu, 8);

    // Selected, and nothing on its chain: a hole, counted as one.
    let _ = pump.resolve(&gpu.queue);
    assert_eq!(m.frame_lost.get(), 1, "the hole is published");
    assert_eq!(m.frame_exact.get(), 0);
    let empty = m.frame_digest.get();

    // Its parent arrives: covered, softly.
    stream.0.push_back(arrive(parent));
    pump.pump(&mut stream, &gpu, 8);
    let _ = pump.resolve(&gpu.queue);
    assert_eq!(m.frame_lost.get(), 0, "an ancestor covers it");
    assert_eq!(m.frame_coarser.get(), 1);
    let soft = m.frame_digest.get();
    assert_ne!(soft, empty, "drawing a fallback is a different picture");

    // The tile itself arrives: exact, and a third picture.
    stream.0.push_back(arrive(tile));
    pump.pump(&mut stream, &gpu, 8);
    let _ = pump.resolve(&gpu.queue);
    assert_eq!(
        (
            m.frame_exact.get(),
            m.frame_coarser.get(),
            m.frame_lost.get()
        ),
        (1, 0, 0)
    );
    let sharp = m.frame_digest.get();
    assert_ne!(sharp, soft);

    // Nothing changes: nothing moves.
    let _ = pump.resolve(&gpu.queue);
    assert_eq!(m.frame_digest.get(), sharp, "a still frame reads the same");

    assert!(m.mesh_bytes_by_level.total() > mesh_bytes_before);
    assert_eq!(m.prepared_tiles.get(), 2);
    drop(pump);
    assert_eq!(
        m.mesh_bytes_by_level.total(),
        mesh_bytes_before,
        "a dropped pump leaves its meshes counted"
    );
    assert_eq!(m.prepared_tiles.get(), 0);
}
