// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Imagery held block-compressed, through the pump (CB9: display at once,
//! compress what lasts): a compressible drape uploaded and drawn as RGBA8
//! at once; compressed in place when the budget asks, a tile at a time on
//! the encode lane, every drape naming a tile rebound to its compressed
//! copy; what the budget is told, and when.
//!
//! Compression is the consumer's choice and off by default; these scenarios
//! are the consumer that chose it, against the one that did not.
//!
//! A machine without a usable adapter skips, with a printed reason, and a
//! block format the adapter does not sample is skipped the same way. Set
//! `TUILE_REQUIRE_GPU=1` to turn the first into a failure.

mod support;

use support::{drape, gpu_with, painted, whole_drape, Scripted, PLANS};
use tuile_core::budget::{FrameDeadline, LayerId, ResourceId};
use tuile_core::protocol::{ClientMessage, ServerMessage};

const RED: &str = "the compression in place regressed";
use tuile_core::source::TileId;
use tuile_core::texels::ImageryFormat;
use tuile_wgpu::{ContentPump, GpuContext};

/// The bytes the pump acknowledged for `resource`.
fn acked(stream: &Scripted, resource: ResourceId) -> Option<u64> {
    stream
        .sent
        .lock()
        .expect("sent")
        .iter()
        .find_map(|m| match m {
            ClientMessage::Ack {
                resource: r, bytes, ..
            } if *r == resource => Some(*bytes),
            _ => None,
        })
}

/// An encode lane of one 256² tile a frame (5,463 blocks, its chain
/// included): the frame encodes one tile, then the lane is spent.
fn one_tile() -> FrameDeadline {
    FrameDeadline::UNBOUNDED.with_encode_blocks(5_463)
}

/// The GPU's work so far finished, and every acknowledgement it held back
/// sent: what a host's next frames do.
fn acks_landed(pump: &mut ContentPump, stream: &mut Scripted, gpu: &GpuContext) {
    pump.acknowledge(stream, gpu);
    gpu.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    pump.acknowledge(stream, gpu);
}

/// One drape of `size`² over a tile of its own, uploaded at once, and the
/// pump that holds it.
fn upload_one(gpu: &GpuContext, size: u32) -> (Scripted, ContentPump, ResourceId) {
    let tile = TileId::from_terrain(4, 3, 2);
    let texture = painted(gpu, size, size, |x, y| {
        [(x % 256) as u8, 90, (y % 256) as u8, 255]
    });
    let mut stream = Scripted::new([drape(tile, 1, whole_drape(tile, 4, texture))]);
    let mut pump = ContentPump::new(glam::DVec3::ZERO);
    pump.pump(&mut stream, gpu, FrameDeadline::UNBOUNDED);
    assert_eq!(pump.pending_uploads(), 0);
    acks_landed(&mut pump, &mut stream, gpu);
    (stream, pump, ResourceId::drape(tile, LayerId(0), 4))
}

/// The bytes the pump said `resource` holds once compressed in place.
fn compressed(stream: &Scripted, resource: ResourceId) -> Option<u64> {
    stream
        .sent
        .lock()
        .expect("sent")
        .iter()
        .find_map(|m| match m {
            ClientMessage::Compressed {
                resource: r, bytes, ..
            } if *r == resource => Some(*bytes),
            _ => None,
        })
}

/// The budget asks for `resources` (each of generation `gen`) to be
/// compressed in place.
fn ask(stream: &mut Scripted, resources: &[(ResourceId, u64)]) {
    stream.inbox.push_back(ServerMessage::Compress {
        resources: resources
            .iter()
            .map(|(r, g)| (*r, tuile_core::budget::Generation(*g)))
            .collect(),
    });
}

/// Whether `gpu`'s plan compresses on the GPU.
fn compresses(gpu: &GpuContext) -> bool {
    gpu.imagery_plan().target != ImageryFormat::Rgba8
}

/// **Display at once:** a compressible drape is uploaded, acknowledged and
/// held as RGBA8 in the frame it arrives — 349,524 bytes for a 256² tile
/// with its chain, whatever the plan — and three of them go in one frame
/// however small the encode lane: no upload waits for an encode.
#[test]
fn a_compressible_drape_goes_up_as_rgba8_at_once() {
    for (plan, compression) in PLANS {
        let Some(gpu) = gpu_with(compression) else {
            continue;
        };
        let tiles = [
            TileId::from_terrain(4, 3, 2),
            TileId::from_terrain(4, 4, 2),
            TileId::from_terrain(4, 5, 2),
        ];
        let mut stream = Scripted::new(tiles.iter().enumerate().map(|(i, t)| {
            let texture = painted(&gpu, 256, 256, |x, _| [x as u8, 100, 50, 255]);
            drape(*t, i as u64 + 1, whole_drape(*t, 4, texture))
        }));
        let mut pump = ContentPump::new(glam::DVec3::ZERO);
        assert_eq!(pump.pump(&mut stream, &gpu, one_tile()), 3, "{plan}: {RED}");
        for t in tiles {
            let r = ResourceId::drape(t, LayerId(0), 4);
            assert_eq!(acked(&stream, r), Some(349_524), "{plan}: acknowledged at once");
        }
        assert_eq!(pump.acks_waiting_for_the_gpu(), 0, "{plan}");
        let u = pump.last_uploads();
        assert_eq!((u.encoded_tiles, u.deferred_encode), (0, 0), "{plan}");
        let formats = gpu.imagery.lock().expect("imagery").live_by_format();
        assert_eq!(formats, vec![(ImageryFormat::Rgba8, 3 * 349_524)], "{plan}");
    }
}

/// **Compressed in place when asked:** the drape's texture is read on the
/// GPU into a block texture, which replaces it in the pool; the drape is
/// acknowledged at a quarter — 87,408 bytes — once the GPU has finished,
/// not before; the GPU then holds only the blocks. Off has no encoder: the
/// drape stays as it is and says so.
#[test]
fn a_compression_asked_replaces_the_texture_and_acks_a_quarter() {
    for (plan, compression) in PLANS {
        let Some(gpu) = gpu_with(compression) else {
            continue;
        };
        let (mut stream, mut pump, resource) = upload_one(&gpu, 256);
        ask(&mut stream, &[(resource, 1)]);
        pump.pump(&mut stream, &gpu, one_tile());
        let _ = pump.resolve(&gpu);
        if compresses(&gpu) {
            assert_eq!(
                compressed(&stream, resource),
                None,
                "{plan}: acknowledged before the GPU ran the encode"
            );
            assert_eq!(pump.acks_waiting_for_the_gpu(), 1, "{plan}");
        }
        acks_landed(&mut pump, &mut stream, &gpu);
        let want = if compresses(&gpu) { 87_408 } else { 349_524 };
        assert_eq!(compressed(&stream, resource), Some(want), "{plan}: {RED}");
        let (count, live) = gpu.imagery.lock().expect("imagery").live();
        assert_eq!((count, live as u64), (1, want), "{plan}");
        assert_eq!(pump.held_bytes()[tuile_core::budget::Class::Drape], want, "{plan}");
    }
}

/// **A tile the encoder does not take stays RGBA8:** 250² is not whole
/// blocks — never compressible, never missing; asked anyway, the drape is
/// acknowledged at what it holds.
#[test]
fn a_tile_that_cannot_be_compressed_stays_rgba8() {
    for (plan, compression) in PLANS {
        let Some(gpu) = gpu_with(compression) else {
            continue;
        };
        let (mut stream, mut pump, resource) = upload_one(&gpu, 250);
        let rgba8 = tuile_core::budget::cost::drape_cost(
            250,
            250,
            tuile_core::budget::cost::TextureFormat::Rgba8,
        )
        .gpu
        .0;
        assert_eq!(acked(&stream, resource), Some(rgba8), "{plan}");
        ask(&mut stream, &[(resource, 1)]);
        pump.pump(&mut stream, &gpu, one_tile());
        acks_landed(&mut pump, &mut stream, &gpu);
        assert_eq!(compressed(&stream, resource), Some(rgba8), "{plan}");
        let formats = gpu.imagery.lock().expect("imagery").live_by_format();
        assert_eq!(formats, vec![(ImageryFormat::Rgba8, rgba8 as usize)], "{plan}");
    }
}

/// **A compression of a drape no longer held is dropped:** evicted, or of
/// another generation, nothing is encoded and nothing acknowledged.
#[test]
fn a_compression_of_a_drape_gone_is_dropped() {
    for (plan, compression) in PLANS {
        let Some(gpu) = gpu_with(compression) else {
            continue;
        };
        let (mut stream, mut pump, resource) = upload_one(&gpu, 256);
        ask(&mut stream, &[(resource, 2)]);
        stream.inbox.push_back(ServerMessage::Evict {
            resources: vec![resource],
        });
        ask(&mut stream, &[(resource, 1)]);
        pump.pump(&mut stream, &gpu, one_tile());
        acks_landed(&mut pump, &mut stream, &gpu);
        assert_eq!(compressed(&stream, resource), None, "{plan}");
        assert_eq!(pump.last_uploads().encoded_tiles, 0, "{plan}");
    }
}

/// **The reservation covers the encoder's scratch** (CB4): what a host
/// takes off the drapes' limits at setup is no less than what the scratch
/// holds for one tile, and no more than its jobs' alignment over it.
/// Nothing for a plan that does not encode on the GPU.
#[test]
fn the_reservation_covers_the_encoders_scratch() {
    for (plan, compression) in PLANS {
        let Some(gpu) = gpu_with(compression) else {
            continue;
        };
        let reserved = tuile_wgpu::encode_reservation(gpu.imagery_plan(), 1).0;
        match gpu.imagery_scratch_bytes() {
            None => assert_eq!(reserved, 0, "{plan}"),
            Some(held) => {
                assert!(
                    held <= reserved && reserved - held <= 9 * 256,
                    "{plan}: the scratch holds {held}, {reserved} reserved"
                );
            }
        }
    }
}

/// A drape of `tile` made of `n` imagery tiles side by side, each its own
/// texture and coord: what a drape climbing levels above its terrain is.
fn mosaic(gpu: &GpuContext, tile: TileId, n: u32, seed: u8) -> tuile_core::content::DrapeContent {
    let mut d = whole_drape(
        tile,
        4,
        painted(gpu, 256, 256, |x, _| [x as u8, seed, 0, 255]),
    );
    let first = d.tiles[0].clone();
    d.tiles = (0..n)
        .map(|i| {
            let mut l = first.clone();
            l.coord.x = first.coord.x * 64 + u64::from(i);
            l.coord.level = first.coord.level + 6;
            l.texture = std::sync::Arc::new(painted(gpu, 256, 256, move |x, y| {
                [x as u8, y as u8, seed.wrapping_add(i as u8), 255]
            }));
            let w = 1.0 / n as f32;
            l.coverage = [i as f32 * w, 0.0, (i + 1) as f32 * w, 1.0];
            l
        })
        .collect();
    d
}

/// **Compressed over frames, a share at a time; drawn throughout.** A
/// drape of four tiles at a one-tile share: a tile a frame, each swapped in
/// as soon as it is made — the drape is never without a texture — and
/// acknowledged once, after its last. Off acknowledges it at once.
#[test]
fn a_four_tile_drape_compresses_a_tile_a_frame() {
    for (plan, compression) in PLANS {
        let Some(gpu) = gpu_with(compression) else {
            continue;
        };
        let tile = TileId::from_terrain(4, 3, 2);
        let resource = ResourceId::drape(tile, LayerId(0), 4);
        let mut stream = Scripted::new([drape(tile, 1, mosaic(&gpu, tile, 4, 7))]);
        let mut pump = ContentPump::new(glam::DVec3::ZERO);
        pump.pump(&mut stream, &gpu, one_tile());
        acks_landed(&mut pump, &mut stream, &gpu);
        assert!(acked(&stream, resource).is_some(), "{plan}: uploaded at once");
        ask(&mut stream, &[(resource, 1)]);
        let mut per_frame = Vec::new();
        for _ in 0..8 {
            pump.pump(&mut stream, &gpu, one_tile());
            per_frame.push(pump.last_uploads().encoded_tiles);
            let _ = pump.resolve(&gpu);
            let (count, _) = gpu.imagery.lock().expect("imagery").live();
            assert_eq!(count, 4, "{plan}: every tile held, old or new");
            if compressed(&stream, resource).is_none() {
                acks_landed(&mut pump, &mut stream, &gpu);
            }
        }
        acks_landed(&mut pump, &mut stream, &gpu);
        let acks = stream
            .sent
            .lock()
            .expect("sent")
            .iter()
            .filter(|m| matches!(m, ClientMessage::Compressed { resource: r, .. } if *r == resource))
            .count();
        assert_eq!(acks, 1, "{plan}: acknowledged once");
        if compresses(&gpu) {
            assert!(per_frame.iter().all(|n| *n <= 2), "{plan}: {per_frame:?}");
            assert_eq!(per_frame.iter().sum::<u32>(), 4, "{plan}: each tile once: {RED}");
            let formats = gpu.imagery.lock().expect("imagery").live_by_format();
            assert_eq!(formats.len(), 1, "{plan}: {formats:?}");
            assert_eq!(formats[0].1, 4 * 87_408, "{plan}");
        } else {
            assert_eq!(per_frame.iter().sum::<u32>(), 0, "{plan}");
        }
    }
}

/// **A tile shared by two drapes is compressed once, and both wear it.**
/// Two drapes over neighbouring tiles name one imagery tile: compressing
/// the first rebinds the second to the compressed copy — the RGBA8 one
/// let go — and compressing the second encodes nothing more.
#[test]
fn a_shared_tile_is_compressed_once_for_every_drape() {
    for (plan, compression) in PLANS {
        let Some(gpu) = gpu_with(compression) else {
            continue;
        };
        if !compresses(&gpu) {
            continue;
        }
        let (a, b) = (TileId::from_terrain(4, 3, 2), TileId::from_terrain(4, 4, 2));
        let shared = std::sync::Arc::new(painted(&gpu, 256, 256, |x, y| [x as u8, y as u8, 3, 255]));
        let over = |t: TileId| {
            let mut d = whole_drape(t, 4, painted(&gpu, 4, 4, |_, _| [0; 4]));
            d.tiles[0].texture = std::sync::Arc::clone(&shared);
            d.tiles[0].coord.x = 99;
            d
        };
        let (ra, rb) = (
            ResourceId::drape(a, LayerId(0), 4),
            ResourceId::drape(b, LayerId(0), 4),
        );
        let mut stream = Scripted::new([drape(a, 1, over(a)), drape(b, 2, over(b))]);
        let mut pump = ContentPump::new(glam::DVec3::ZERO);
        pump.pump(&mut stream, &gpu, FrameDeadline::UNBOUNDED);
        acks_landed(&mut pump, &mut stream, &gpu);
        ask(&mut stream, &[(ra, 1)]);
        pump.pump(&mut stream, &gpu, FrameDeadline::UNBOUNDED);
        let _ = pump.resolve(&gpu);
        acks_landed(&mut pump, &mut stream, &gpu);
        let (count, live) = gpu.imagery.lock().expect("imagery").live();
        assert_eq!((count, live), (1, 87_408), "{plan}: one compressed copy");
        ask(&mut stream, &[(rb, 2)]);
        pump.pump(&mut stream, &gpu, FrameDeadline::UNBOUNDED);
        acks_landed(&mut pump, &mut stream, &gpu);
        assert_eq!(pump.last_uploads().encoded_tiles, 0, "{plan}: nothing more");
        assert_eq!(compressed(&stream, rb), Some(87_408), "{plan}");
        let stats = gpu.imagery_encode_stats().expect("an encoder");
        assert_eq!(stats.encoded, 1, "{plan}");
    }
}

