// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A frame's uploads are one batch: one encoder, one submit, their bytes
//! through reused staging, every encode in one pass, and nothing read back
//! or waited for on the way.
//!
//! Counted with the context's own counters (`GpuContext::upload_ops`), on a
//! real device: a skip without one, a failure under `TUILE_REQUIRE_GPU=1`.

mod support;

use support::{drape, exacts, gpu_with, mesh, painted, select, wearing, whole_drape, Scripted};
use tuile_core::budget::{FrameDeadline, LayerId, ResourceId, UvTransform};
use tuile_core::source::TileId;
use tuile_terrain::{fill_content, GeographicTilingScheme, TileCoord};
use tuile_wgpu::{ContentPump, GpuContext, ImageryCompression};

/// Whether this device stages through the crate's belt (native backends);
/// the browser's WebGPU and GL keep their queue writes.
fn belted(gpu: &GpuContext) -> bool {
    matches!(
        gpu.device.adapter_info().backend,
        wgpu::Backend::Metal | wgpu::Backend::Vulkan | wgpu::Backend::Dx12
    )
}

/// `n` meshes and their drapes, delivered, and a draw set wearing them.
fn deliveries(gpu: &GpuContext, n: u32, row: u64) -> Scripted {
    let mut stream = Scripted::default();
    let tiles: Vec<TileId> = (0..n)
        .map(|i| TileId::from_terrain(8, u64::from(i), row))
        .collect();
    let mut items = Vec::new();
    for (i, tile) in tiles.iter().enumerate() {
        let (z, x, y) = tile.terrain_coord();
        let rect = GeographicTilingScheme::default().tile_rect(TileCoord::new(z, x, y));
        stream.inbox.push_back(mesh(
            *tile,
            i as u64 + 1,
            fill_content(&rect, [Some(0.0); 4], 0.0),
        ));
        let texture = painted(gpu, 64, 64, |x, y| [x as u8, y as u8, i as u8, 255]);
        stream.inbox.push_back(drape(
            *tile,
            1000 + i as u64,
            whole_drape(*tile, z, texture),
        ));
        items.push(wearing(
            exacts(&[*tile]).remove(0),
            ResourceId::drape(*tile, LayerId(0), z),
            z,
            UvTransform::IDENTITY,
        ));
    }
    stream.inbox.push_back(select(items, 1));
    stream
}

/// **A frame of uploads is one submit.** Sixteen meshes and sixteen drapes
/// in one frame, pumped and resolved: one encoder, one submit, and no `write_buffer` or
/// `write_texture` of their own — each of those was a staging buffer the
/// device allocated and freed. The staging is a few chunks the first time;
/// a second frame like it reuses them and allocates nothing.
#[test]
fn a_frame_of_uploads_is_one_submit_through_reused_staging() {
    let Some(gpu) = gpu_with(ImageryCompression::Off) else {
        return;
    };
    let mut pump = ContentPump::new(glam::DVec3::ZERO);
    let mut stream = deliveries(&gpu, 16, 3);
    let before = gpu.upload_ops();
    assert_eq!(pump.pump(&mut stream, &gpu, FrameDeadline::UNBOUNDED), 32);
    let _ = pump.resolve(&gpu);
    let first = gpu.upload_ops() - before;
    assert_eq!(first.submits, 1, "{first:?}");
    assert_eq!(first.encoders, 1, "{first:?}");
    if belted(&gpu) {
        assert_eq!(
            (first.write_buffer, first.write_texture),
            (0, 0),
            "{first:?}"
        );
        assert!(first.staging_allocs <= 4, "{first:?}");
        assert!(first.belt_bytes > 0, "{first:?}");
    }
    // The GPU done with the first frame, its chunks are mapped again.
    let _ = gpu.device.poll(wgpu::PollType::wait_indefinitely());
    let mut stream = deliveries(&gpu, 16, 5);
    let before = gpu.upload_ops();
    pump.pump(&mut stream, &gpu, FrameDeadline::UNBOUNDED);
    let _ = pump.resolve(&gpu);
    let second = gpu.upload_ops() - before;
    assert_eq!(second.submits, 1, "{second:?}");
    assert_eq!((second.readbacks, second.polls), (0, 0), "{second:?}");
    if belted(&gpu) {
        assert_eq!(second.staging_allocs, 0, "{second:?}");
    }
}

/// **A frame's encodes are one pass and one submit**, under each block
/// plan: sixteen drapes encoded together, nothing polled, at most one
/// timestamp read started — without waiting for it.
#[test]
fn a_frames_encodes_are_one_pass() {
    for compression in [
        ImageryCompression::Format(tuile_core::texels::BlockFormat::Bc7),
        ImageryCompression::Format(tuile_core::texels::BlockFormat::Astc4x4),
    ] {
        let Some(gpu) = gpu_with(compression) else {
            continue;
        };
        let mut pump = ContentPump::new(glam::DVec3::ZERO);
        // Room for every encode of the frame: an unbounded deadline has no
        // encode lane.
        let mut stream = deliveries(&gpu, 16, 7);
        let before = gpu.upload_ops();
        assert_eq!(pump.pump(&mut stream, &gpu, FrameDeadline::UNBOUNDED), 32);
        let _ = pump.resolve(&gpu);
        let ops = gpu.upload_ops() - before;
        assert_eq!(ops.encoded_tiles, 16, "{compression:?} {ops:?}");
        assert_eq!(ops.encode_passes, 1, "{compression:?} {ops:?}");
        assert_eq!(ops.submits, 1, "{compression:?} {ops:?}");
        assert_eq!(ops.polls, 0, "{compression:?} {ops:?}");
        assert!(ops.readbacks <= 1, "{compression:?} {ops:?}");
        let stats = gpu.imagery_encode_stats().expect("an encoder");
        assert_eq!((stats.encoded, stats.fell_back), (16, 0), "{compression:?}");
    }
}
