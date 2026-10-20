// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What the GPU scenarios share: an adapter or a stated reason there is none,
//! a scripted stream, the messages a server sends, and the loop that drives a
//! real server and the pump until the view has converged.
//!
//! Each scenario uses a part of it; the rest is dead code there.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use glam::DVec3;

use tuile_core::budget::{
    Bytes, Class, DrapeChoice, DrawItem, DrawSet, FrameDeadline, Generation, PerClass, ResourceId,
    Role, UploadPlan, UvTransform,
};
use tuile_core::content::{DecodedTileContent, DrapeContent};
use tuile_core::protocol::{
    Ancestry, ClientMessage, GeometryStream, ResourceContent, ServerMessage, StreamError,
};
use tuile_core::source::TileId;
use tuile_core::traversal::{TraversalStats, ViewState};
use tuile_wgpu::{ContentPump, GpuContext};

/// A GPU, or a stated reason there is none.
///
/// `TUILE_REQUIRE_GPU=1` turns the skip into a failure. Without that escape a
/// headless CI would report success for a test it never ran, which is worse
/// than not having the test.
pub fn gpu() -> Option<GpuContext> {
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

/// A stream that hands the pump a scripted list of messages, and keeps what
/// the pump sent.
///
/// `Pending` once drained, which is exactly what a live server looks like
/// between arrivals, so the pump takes the same path it does in a session.
#[derive(Default)]
pub struct Scripted {
    pub inbox: VecDeque<ServerMessage>,
    pub sent: Mutex<Vec<ClientMessage>>,
}

impl Scripted {
    pub fn new(messages: impl IntoIterator<Item = ServerMessage>) -> Self {
        Self {
            inbox: messages.into_iter().collect(),
            sent: Mutex::default(),
        }
    }
}

impl GeometryStream for Scripted {
    fn send(&self, msg: ClientMessage) -> Result<(), StreamError> {
        self.sent.lock().expect("sent").push(msg);
        Ok(())
    }

    fn poll_message(&mut self, _cx: &mut Context<'_>) -> Poll<Option<ServerMessage>> {
        match self.inbox.pop_front() {
            Some(msg) => Poll::Ready(Some(msg)),
            None => Poll::Pending,
        }
    }
}

/// The terrain ancestry of a tile, as the server states it on the wire.
pub fn ancestry(tile: TileId) -> Ancestry {
    let (z, x, y) = tile.terrain_coord();
    Ancestry {
        level: z,
        parent: (z > 0).then(|| TileId::from_terrain(z - 1, x / 2, y / 2)),
    }
}

/// The shape of the tree around some tiles: every tile named, **and every
/// ancestor of one**, up to the root.
pub fn tree_shape(tiles: &[TileId]) -> Vec<(TileId, Ancestry)> {
    let mut out = Vec::new();
    for tile in tiles {
        let mut cur = Some(*tile);
        while let Some(id) = cur {
            let a = ancestry(id);
            out.push((id, a));
            cur = a.parent;
        }
    }
    out.sort_by_key(|(t, _)| *t);
    out.dedup_by_key(|(t, _)| *t);
    out
}

/// A terrain mesh delivered, at generation `gen`.
pub fn mesh(tile: TileId, gen: u64, content: DecodedTileContent) -> ServerMessage {
    ServerMessage::Content {
        resource: ResourceId::Mesh(tile),
        gen: Generation(gen),
        class: Class::TerrainMesh,
        ancestry: ancestry(tile),
        content: ResourceContent::Mesh(Arc::new(content)),
    }
}

/// A drape delivered, at generation `gen`.
pub fn drape(tile: TileId, gen: u64, content: DrapeContent) -> ServerMessage {
    ServerMessage::Content {
        resource: ResourceId::drape(tile, content.layer, content.level),
        gen: Generation(gen),
        class: Class::Drape,
        ancestry: ancestry(tile),
        content: ResourceContent::Drape(Arc::new(content)),
    }
}

/// A mesh drawn as itself.
pub fn exact(tile: TileId) -> DrawItem {
    DrawItem {
        mesh: ResourceId::Mesh(tile),
        role: Role::Exact,
        drapes: Vec::new(),
        serves: vec![tile],
    }
}

/// Each of `tiles` drawn as itself.
pub fn exacts(tiles: &[TileId]) -> Vec<DrawItem> {
    tiles.iter().map(|t| exact(*t)).collect()
}

/// An ancestor standing in for `serves`, under the fallback pass.
pub fn fallback(tile: TileId, serves: &[TileId]) -> DrawItem {
    let mut serves = serves.to_vec();
    serves.sort();
    DrawItem {
        mesh: ResourceId::Mesh(tile),
        role: Role::Fallback,
        drapes: Vec::new(),
        serves,
    }
}

/// `item` wearing `drape` (a resource of `owner`'s) through `uv`.
pub fn wearing(mut item: DrawItem, drape: ResourceId, level: u32, uv: UvTransform) -> DrawItem {
    let ResourceId::Drape { layer, .. } = drape else {
        unreachable!("a drape resource");
    };
    item.drapes.push(DrapeChoice {
        resource: drape,
        layer,
        level,
        uv,
    });
    item
}

/// The draw set the server sends, at generation `generation`: the items
/// sorted by mesh, and the shape of the tree around everything they name.
pub fn select(mut items: Vec<DrawItem>, generation: u64) -> ServerMessage {
    items.sort_by_key(|i| i.mesh);
    let mut named: Vec<TileId> = Vec::new();
    for item in &items {
        named.push(item.mesh.tile());
        named.extend(item.serves.iter().copied());
        named.extend(item.drapes.iter().map(|d| d.resource.tile()));
    }
    ServerMessage::Select {
        draw: DrawSet { items },
        plan: UploadPlan::default(),
        ancestry: tree_shape(&named),
        stats: TraversalStats::default(),
        hard_limit: PerClass::splat(Bytes::MAX),
        frame_ready: false,
        converged: false,
        lost: 0,
        bare: 0,
        generation,
    }
}

/// A server to drive by hand: polled on the test's thread, so a scenario is
/// a sequence, not a race.
pub type Server = Pin<Box<dyn Future<Output = ()>>>;

/// Polls the server once.
pub fn poll(server: &mut Server) {
    let waker = futures_util::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    let _ = server.as_mut().poll(&mut cx);
}

/// Sends `views` and drives the server and the pump until the view has
/// converged: the server says so for the camera just sent, and nothing is
/// left to upload. Returns whether it did within the bound.
pub fn settle<S: GeometryStream>(
    server: &mut Server,
    stream: &mut S,
    pump: &mut ContentPump,
    gpu: &GpuContext,
    views: &[ViewState],
    origin: DVec3,
) -> bool {
    poll(server);
    pump.advance(stream, gpu, views, origin, FrameDeadline::UNBOUNDED);
    for _ in 0..4_000 {
        poll(server);
        pump.pump(stream, gpu, FrameDeadline::UNBOUNDED);
        if pump.generation_selected() >= pump.generation_sent()
            && (pump.converged || pump.frame_ready)
            && pump.pending_uploads() == 0
        {
            return true;
        }
    }
    false
}
