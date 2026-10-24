// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Imagery held block-compressed, through the pump: what the budget is told,
//! what happens when an encode cannot be done, and how the encodes are paced.
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
use tuile_core::protocol::ClientMessage;
use tuile_core::source::TileId;
use tuile_core::texels::ImageryFormat;
use tuile_wgpu::{ContentPump, EncodePace, GpuContext};

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
    (stream, pump, ResourceId::drape(tile, LayerId(0), 4))
}

/// **The ack carries what the drape really holds**: a 256² tile with its
/// chain is 349,524 bytes as RGBA8 and 87,408 as blocks — a quarter, read
/// from what the pump told the budget, not computed here.
#[test]
fn the_ack_carries_the_block_size() {
    for (plan, compression) in PLANS {
        let Some(gpu) = gpu_with(compression) else {
            continue;
        };
        let (stream, _held, resource) = upload_one(&gpu, 256);
        let bytes = acked(&stream, resource).expect("acknowledged");
        let want = match gpu.imagery_plan().target {
            ImageryFormat::Rgba8 => 349_524,
            ImageryFormat::Bc7 | ImageryFormat::Astc4x4 => 87_408,
        };
        assert_eq!(bytes, want, "{plan}");
        let (count, live) = gpu.imagery.lock().expect("imagery").live();
        assert_eq!(count, 1, "{plan}");
        assert_eq!(
            live as u64, want,
            "{plan}: the GPU holds what was acknowledged"
        );
    }
}

/// **An encode that cannot be done uploads RGBA8, and says so.** 250 is not
/// whole blocks, so the encoder refuses the tile: it is held, drawn and
/// acknowledged as RGBA8 — never missing, never charged a quarter of what it
/// holds.
#[test]
fn a_refused_encode_falls_back_to_rgba8() {
    for (plan, compression) in PLANS {
        let Some(gpu) = gpu_with(compression) else {
            continue;
        };
        let (stream, _held, resource) = upload_one(&gpu, 250);
        let rgba8 = tuile_core::budget::cost::drape_cost(
            250,
            250,
            tuile_core::budget::cost::TextureFormat::Rgba8,
        )
        .gpu
        .0;
        assert_eq!(acked(&stream, resource), Some(rgba8), "{plan}");
        let formats = gpu.imagery.lock().expect("imagery").live_by_format();
        assert_eq!(
            formats,
            vec![(ImageryFormat::Rgba8, rgba8 as usize)],
            "{plan}"
        );
        if let Some(stats) = gpu.imagery_encode_stats() {
            assert_eq!((stats.encoded, stats.fell_back), (0, 1), "{plan}");
        }
    }
}

/// **The encodes are paced by their own share**: with a share too small for
/// one tile, a frame takes its first encode and leaves the others queued, one
/// a frame; without an encoder — Off — the same frame takes all three.
#[test]
fn the_encodes_take_one_frame_each_under_a_tiny_share() {
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
        pump.set_encode_pace(EncodePace { share_micros: 1 });
        let per_frame: Vec<usize> = (0..3)
            .map(|_| pump.pump(&mut stream, &gpu, FrameDeadline::UNBOUNDED))
            .collect();
        let want = match gpu.imagery_plan().target {
            ImageryFormat::Rgba8 => vec![3, 0, 0],
            _ => vec![1, 1, 1],
        };
        assert_eq!(per_frame, want, "{plan}");
        assert_eq!(pump.pending_uploads(), 0, "{plan}: nothing waits for ever");
    }
}
