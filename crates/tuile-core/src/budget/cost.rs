// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The deterministic cost estimator, in GPU bytes, shared by the budget and
//! the consumer. The consumer reports the actual bytes in its acknowledgement;
//! a drift above 5 % means this estimator is wrong.

use super::types::Bytes;

/// The per-tile uniform block.
pub const TILE_UNIFORM_BYTES: u64 = 8_272;

/// A mesh: vertex and index buffers, the per-tile uniform, and the CPU copy
/// kept for stitching (unified memory pays for it too).
pub fn mesh_bytes(vertices: u64, stride: u64, indices: u64, cpu_copy: bool) -> Bytes {
    let gpu = vertices
        .saturating_mul(stride)
        .saturating_add(indices.saturating_mul(4))
        .saturating_add(TILE_UNIFORM_BYTES);
    let cpu = if cpu_copy {
        vertices
            .saturating_mul(stride)
            .saturating_add(indices.saturating_mul(4))
    } else {
        0
    };
    Bytes(gpu.saturating_add(cpu))
}

/// A texture's pixel format, for its byte cost.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TextureFormat {
    /// 4 bytes per texel.
    Rgba8,
    /// ASTC or BC7: 16 bytes per 4×4 block.
    Block16,
    /// BC1: 8 bytes per 4×4 block.
    Block8,
}

/// A drape texture `w × h` with its full mip chain (+⅓).
pub fn drape_bytes(w: u32, h: u32, format: TextureFormat) -> Bytes {
    let (w, h) = (u64::from(w), u64::from(h));
    let base = match format {
        TextureFormat::Rgba8 => w * h * 4,
        TextureFormat::Block16 => w.div_ceil(4) * h.div_ceil(4) * 16,
        TextureFormat::Block8 => w.div_ceil(4) * h.div_ceil(4) * 8,
    };
    Bytes(base + base / 3)
}

/// Whether `actual` is within 5 % of `estimate`.
pub fn within_drift(estimate: Bytes, actual: Bytes) -> bool {
    let (e, a) = (u128::from(estimate.0), u128::from(actual.0));
    e.abs_diff(a) * 100 <= e * 5
}
