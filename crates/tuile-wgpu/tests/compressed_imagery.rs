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

/// A flat mesh over `tile`.
fn flat(tile: TileId) -> tuile_core::content::DecodedTileContent {
    let (z, x, y) = tile.terrain_coord();
    let rect = tuile_terrain::GeographicTilingScheme::default()
        .tile_rect(tuile_terrain::TileCoord::new(z, x, y));
    tuile_terrain::fill_content(&rect, [Some(0.0); 4], 0.0)
}

/// **The encode pacing never splits a swap group.** A drape takes the
/// frame's first encode; behind it, a tile's mesh and its drape form a swap
/// group. The drape does not fit the encodes' share — and the mesh, which
/// encodes nothing, waits with it rather than be drawn bare. Both go the
/// next frame, together. Off: everything goes at once.
#[test]
fn encode_pacing_never_splits_a_swap_group() {
    use support::{mesh, select};
    use tuile_core::budget::{Bytes, SwapGroup, UploadPlan};
    use tuile_core::protocol::ServerMessage;
    for (plan, compression) in PLANS {
        let Some(gpu) = gpu_with(compression) else {
            continue;
        };
        let (first, paired) = (TileId::from_terrain(4, 3, 2), TileId::from_terrain(4, 4, 2));
        let texture = |seed: u8| painted(&gpu, 256, 256, move |x, _| [x as u8, seed, 50, 255]);
        let lead = ResourceId::drape(first, LayerId(0), 4);
        let (m, d) = (
            ResourceId::Mesh(paired),
            ResourceId::drape(paired, LayerId(0), 4),
        );
        let mut members = vec![m, d];
        members.sort_unstable();
        let mut selected = select(Vec::new(), 1);
        if let ServerMessage::Select { plan, .. } = &mut selected {
            *plan = UploadPlan {
                items: vec![lead, m, d],
                bytes: Bytes::ZERO,
                groups: vec![SwapGroup { members }],
            };
        }
        let mut stream = Scripted::new([
            drape(first, 1, whole_drape(first, 4, texture(10))),
            mesh(paired, 2, flat(paired)),
            drape(paired, 3, whole_drape(paired, 4, texture(20))),
            selected,
        ]);
        let mut pump = ContentPump::new(glam::DVec3::ZERO);
        pump.set_encode_pace(EncodePace { share_micros: 1 });
        pump.pump(&mut stream, &gpu, FrameDeadline::UNBOUNDED);
        acks_landed(&mut pump, &mut stream, &gpu);
        let first_frame = (acked(&stream, m).is_some(), acked(&stream, d).is_some());
        let uploads = pump.last_uploads();
        if gpu.imagery_plan().target == ImageryFormat::Rgba8 {
            assert_eq!(first_frame, (true, true), "{plan}");
            assert_eq!(uploads.deferred_encode, 0, "{plan}");
            continue;
        }
        assert!(
            acked(&stream, lead).is_some(),
            "{plan}: the first encode goes"
        );
        assert_eq!(
            first_frame,
            (false, false),
            "{plan}: the group was split — its mesh went without its drape"
        );
        assert_eq!(uploads.deferred_encode, 2, "{plan}: the whole group waits");
        assert_eq!(uploads.deferred, 0, "{plan}: not the byte lane's");
        pump.pump(&mut stream, &gpu, FrameDeadline::UNBOUNDED);
        acks_landed(&mut pump, &mut stream, &gpu);
        assert!(
            acked(&stream, m).is_some() && acked(&stream, d).is_some(),
            "{plan}: the group goes whole the next frame"
        );
        assert_eq!(pump.pending_uploads(), 0, "{plan}");
    }
}

/// **A frame's encodes are counted, and its deferrals by lane.** Three
/// drapes under a share too small for one tile: the first frame encodes one
/// 256² tile — 5,463 blocks, mip chain included — and defers two to the
/// encode lane, none to the byte lane. Off encodes and defers nothing.
#[test]
fn a_frames_encodes_and_deferrals_are_counted_by_lane() {
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
        pump.pump(&mut stream, &gpu, FrameDeadline::UNBOUNDED);
        let u = pump.last_uploads();
        let want = match gpu.imagery_plan().target {
            ImageryFormat::Rgba8 => (3, 0, 0, 0, 0),
            _ => (1, 0, 2, 1, 5_463),
        };
        assert_eq!(
            (
                u.uploaded,
                u.deferred,
                u.deferred_encode,
                u.encoded_tiles,
                u.encode_blocks
            ),
            want,
            "{plan}"
        );
        assert_eq!(u.encode_gpu_micros > 0, want.4 > 0, "{plan}");
        let totals = pump.totals();
        assert_eq!(
            (totals.deferred_encode, totals.encode_blocks),
            (u64::from(want.2), want.4),
            "{plan}"
        );
    }
}

/// **A GPU-encoded acknowledgement waits for the GPU** (CB4): until the
/// GPU has run the encode, the encoder's RGBA8 is held beside the blocks,
/// and the budget must go on charging both. So the pump that uploads a
/// drape it encodes does not acknowledge it; once the frame is submitted
/// and the GPU has finished, the next acknowledgement round does. Off is
/// acknowledged in the frame that uploads it, as always.
#[test]
fn a_gpu_encoded_ack_waits_for_the_gpu() {
    for (plan, compression) in PLANS {
        let Some(gpu) = gpu_with(compression) else {
            continue;
        };
        let tile = TileId::from_terrain(4, 3, 2);
        let texture = painted(&gpu, 256, 256, |x, y| [x as u8, 90, y as u8, 255]);
        let mut stream = Scripted::new([drape(tile, 1, whole_drape(tile, 4, texture))]);
        let mut pump = ContentPump::new(glam::DVec3::ZERO);
        pump.pump(&mut stream, &gpu, FrameDeadline::UNBOUNDED);
        let resource = ResourceId::drape(tile, LayerId(0), 4);
        let encoded = gpu.imagery_plan().target != ImageryFormat::Rgba8;
        assert_eq!(
            acked(&stream, resource).is_some(),
            !encoded,
            "{plan}: acknowledged before the GPU ran its encode"
        );
        assert_eq!(
            pump.acks_waiting_for_the_gpu(),
            usize::from(encoded),
            "{plan}"
        );
        acks_landed(&mut pump, &mut stream, &gpu);
        assert!(
            acked(&stream, resource).is_some(),
            "{plan}: never acknowledged"
        );
        assert_eq!(pump.acks_waiting_for_the_gpu(), 0, "{plan}");
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
