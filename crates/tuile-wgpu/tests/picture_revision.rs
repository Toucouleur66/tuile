// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! **The pump's revision moves when the picture may change, and only then.**
//!
//! A host that draws only when something changed reads
//! [`ContentPump::revision`]: equal at two frames, with the same camera, means
//! the same picture, so nothing is drawn. Two failures are possible and both
//! are tested here. A revision that stays still while a tile lands, leaves or
//! changes its texture leaves a stale picture on screen — the new ground is
//! never shown. A revision that moves on a still session draws the same
//! picture again, every frame, which is the cost the host was avoiding.

mod support;

use support::{exact, gpu, mesh, select, Scripted};
use tuile_core::budget::{FrameDeadline, ResourceId};
use tuile_core::content::DecodedTileContent;
use tuile_core::geo::{geodetic_to_ecef, Geodetic};
use tuile_core::protocol::ServerMessage;
use tuile_core::source::TileId;
use tuile_terrain::{fill_content, GeographicTilingScheme, TileCoord};
use tuile_wgpu::ContentPump;

fn content(tile: TileId) -> DecodedTileContent {
    let (z, x, y) = tile.terrain_coord();
    let rect = GeographicTilingScheme::default().tile_rect(TileCoord::new(z, x, y));
    fill_content(&rect, [Some(0.0); 4], 0.0)
}

#[test]
fn the_revision_moves_with_what_is_drawn_and_holds_still_with_a_still_session() {
    let Some(gpu) = gpu() else { return };
    let tile = TileId::from_terrain(3, 4, 4);
    let other = TileId::from_terrain(3, 5, 4);
    let eye = geodetic_to_ecef(Geodetic {
        lon: 0.0,
        lat: 0.0,
        height: 1.0e6,
    });
    let mut pump = ContentPump::new(eye);
    let mut stream = Scripted::new([
        mesh(tile, 1, content(tile)),
        select(vec![exact(tile)], 1),
    ]);
    pump.pump(&mut stream, &gpu, FrameDeadline::UNBOUNDED);
    let _ = pump.resolve(&gpu);
    let landed = pump.revision();
    assert!(landed > 0, "a tile landed and the revision did not move");

    // A still session: frames pumped and resolved, and the same selection
    // sent again, as the server does on every step — the same picture.
    for _ in 0..20 {
        pump.pump(&mut stream, &gpu, FrameDeadline::UNBOUNDED);
        let _ = pump.resolve(&gpu);
    }
    stream.inbox.push_back(select(vec![exact(tile)], 1));
    pump.pump(&mut stream, &gpu, FrameDeadline::UNBOUNDED);
    let _ = pump.resolve(&gpu);
    assert_eq!(
        pump.revision(),
        landed,
        "nothing changed, and the revision says the picture did"
    );

    // A tile delivered and not yet uploaded is not on screen yet; uploaded,
    // it is.
    stream.inbox.push_back(mesh(other, 2, content(other)));
    pump.drain(&mut stream);
    assert_eq!(pump.revision(), landed, "a delivery is not a picture");
    pump.pump(&mut stream, &gpu, FrameDeadline::UNBOUNDED);
    let uploaded = pump.revision();
    assert!(uploaded > landed, "an upload did not move the revision");

    // Drawn: a new draw set.
    stream
        .inbox
        .push_back(select(vec![exact(tile), exact(other)], 2));
    pump.pump(&mut stream, &gpu, FrameDeadline::UNBOUNDED);
    let _ = pump.resolve(&gpu);
    let both = pump.revision();
    assert!(both > uploaded, "a new draw set did not move the revision");

    // A source hidden, and shown again.
    pump.hide_source(other.tag(), true);
    let hidden = pump.revision();
    assert!(hidden > both, "hiding a source did not move the revision");
    pump.hide_source(other.tag(), true);
    assert_eq!(pump.revision(), hidden, "hiding it twice is no change");
    pump.hide_source(other.tag(), false);
    let shown = pump.revision();
    assert!(shown > hidden);

    // Evicted: gone from the picture.
    stream.inbox.push_back(ServerMessage::Evict {
        resources: vec![ResourceId::Mesh(other)],
    });
    stream.inbox.push_back(select(vec![exact(tile)], 3));
    pump.pump(&mut stream, &gpu, FrameDeadline::UNBOUNDED);
    let _ = pump.resolve(&gpu);
    assert!(
        pump.revision() > shown,
        "an eviction did not move the revision"
    );

    // And still again.
    let settled = pump.revision();
    for _ in 0..5 {
        pump.pump(&mut stream, &gpu, FrameDeadline::UNBOUNDED);
        let _ = pump.resolve(&gpu);
    }
    assert_eq!(pump.revision(), settled);
}
