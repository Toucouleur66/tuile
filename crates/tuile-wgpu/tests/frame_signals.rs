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

mod support;

use support::{exact, fallback, gpu, mesh, select, Scripted};
use tuile_core::budget::FrameDeadline;
use tuile_core::content::DecodedTileContent;
use tuile_core::geo::{geodetic_to_ecef, Geodetic};
use tuile_core::source::TileId;
use tuile_terrain::{fill_content, GeographicTilingScheme, TileCoord};
use tuile_wgpu::ContentPump;

fn content(tile: TileId) -> DecodedTileContent {
    let (z, x, y) = tile.terrain_coord();
    let rect = GeographicTilingScheme::default().tile_rect(TileCoord::new(z, x, y));
    fill_content(&rect, [Some(0.0); 4], 0.0)
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
    let mut stream = Scripted::new([select(vec![exact(tile)], 1)]);
    pump.pump(&mut stream, &gpu, FrameDeadline::UNBOUNDED);

    // Drawn, and not on the GPU: a hole, counted as one.
    let _ = pump.resolve(&gpu);
    assert_eq!(m.frame_lost.get(), 1, "the hole is published");
    assert_eq!(m.frame_exact.get(), 0);
    let empty = m.frame_digest.get();

    // Its parent arrives, and stands in for it: covered, softly.
    stream.inbox.push_back(mesh(parent, 1, content(parent)));
    stream
        .inbox
        .push_back(select(vec![fallback(parent, &[tile])], 1));
    pump.pump(&mut stream, &gpu, FrameDeadline::UNBOUNDED);
    let _ = pump.resolve(&gpu);
    assert_eq!(m.frame_lost.get(), 0, "an ancestor covers it");
    assert_eq!(m.frame_coarser.get(), 1);
    let soft = m.frame_digest.get();
    assert_ne!(soft, empty, "drawing a fallback is a different picture");

    // The tile itself arrives: exact, and a third picture.
    stream.inbox.push_back(mesh(tile, 2, content(tile)));
    stream.inbox.push_back(select(vec![exact(tile)], 1));
    pump.pump(&mut stream, &gpu, FrameDeadline::UNBOUNDED);
    let _ = pump.resolve(&gpu);
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
    let _ = pump.resolve(&gpu);
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
