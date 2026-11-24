// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The deterministic cost estimator, shared by the budget and the consumer.
//!
//! A [`Cost`] says, per path a resource takes to the GPU:
//! - `gpu`: the bytes it keeps resident (and, on unified memory, the CPU
//!   copy kept beside it);
//! - `staging`: the row-aligned bytes its upload moves, against the
//!   delivery credit and the executor's byte lane;
//! - `transfer`: the texels in the form they cross in, on `gpu`'s basis —
//!   what a decoded copy holds and what a frame's upload plan is capped on;
//! - `transient`: the GPU bytes held beside `gpu` until an encode on the
//!   GPU has finished (the RGBA8 the encoder reads);
//! - `encode_blocks`: the 4×4 blocks that encode writes;
//! - `compressed`: the bytes it is held in once the GPU compresses it **in
//!   place**, later, if the budget decides so (CB9: display at once,
//!   compress what lasts) — zero when it cannot be;
//! - `compress_blocks`: the 4×4 blocks that compression writes.
//!
//! For RGBA8 and meshes `transfer == gpu` and the rest is zero
//! ([`Cost::is_identity`]): nothing compressed costs what it always did. A
//! texture the GPU may compress (CB9) crosses and is held as RGBA8 (349,524
//! bytes for 256² with its chain): `compressed` says it would hold 87,408
//! once compressed in place, and `compress_blocks` that compressing it
//! writes 5,463 blocks. An encode on the way to the GPU (CB2–CB6:
//! `transfer`, `transient`, `encode_blocks`) is still a cost the budget
//! reads, whatever produces it.
//!
//! The consumer reports the actual GPU bytes in its acknowledgement; a drift
//! above 5 % means this estimator is wrong — unless the actual lies in
//! `(gpu, transfer]`, which is an encode that fell back to the form it
//! crossed in ([`Cost::is_fallback`]), counted as such.
//!
//! What it accounts for, as wgpu lays it out:
//! - texture copies with `bytes_per_row` aligned to 256
//!   (`COPY_BYTES_PER_ROW_ALIGNMENT`), mip by mip, in the staging bytes;
//! - the full mip chain, level by level (about 4/3 of the base);
//! - block-compressed formats by whole 4×4 blocks;
//! - buffer sizes aligned to 4 (`COPY_BUFFER_ALIGNMENT`), and the per-tile
//!   uniform to 256 (the minimum uniform offset alignment).

use super::types::Bytes;

/// The per-tile uniform block, before alignment.
pub const TILE_UNIFORM_BYTES: u64 = 8_272;
/// wgpu's `COPY_BYTES_PER_ROW_ALIGNMENT`.
pub const ROW_ALIGNMENT: u64 = 256;
/// wgpu's `COPY_BUFFER_ALIGNMENT`.
pub const BUFFER_ALIGNMENT: u64 = 4;
/// The minimum uniform buffer offset alignment wgpu guarantees.
pub const UNIFORM_ALIGNMENT: u64 = 256;

/// What a resource costs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cost {
    /// Bytes held while resident (GPU, plus the CPU copy on unified memory).
    pub gpu: Bytes,
    /// Transient bytes of the upload (staging), against the delivery credit.
    pub staging: Bytes,
    /// The texels in their crossing form, on `gpu`'s basis: `gpu` unless
    /// the GPU encodes them on the way.
    pub transfer: Bytes,
    /// Extra GPU bytes held until the GPU's encode has finished.
    pub transient: Bytes,
    /// The 4×4 blocks the GPU's encode writes.
    pub encode_blocks: u64,
    /// The bytes held once the GPU compresses it in place, on `gpu`'s
    /// basis; zero when it cannot be (CB9).
    pub compressed: Bytes,
    /// The 4×4 blocks that compression writes.
    pub compress_blocks: u64,
}

impl Cost {
    /// A cost whose staging equals its resident bytes.
    pub const fn flat(b: Bytes) -> Self {
        Cost::new(b, b)
    }

    /// Resident `gpu`, moving `staging`, crossing as it is held: no encode
    /// on the way.
    pub const fn new(gpu: Bytes, staging: Bytes) -> Self {
        Cost {
            gpu,
            staging,
            transfer: gpu,
            transient: Bytes::ZERO,
            encode_blocks: 0,
            compressed: Bytes::ZERO,
            compress_blocks: 0,
        }
    }

    /// `self`, compressible in place into `to` bytes by writing `blocks`
    /// blocks (CB9).
    pub const fn compressible(mut self, to: Bytes, blocks: u64) -> Self {
        self.compressed = to;
        self.compress_blocks = blocks;
        self
    }

    /// Whether the GPU may compress it in place, and into how many bytes:
    /// `Some` only when that holds less than it does now.
    pub const fn compresses_to(&self) -> Option<Bytes> {
        if self.compress_blocks > 0 && self.compressed.0 < self.gpu.0 {
            Some(self.compressed)
        } else {
            None
        }
    }

    /// Crosses as it is held, and nothing is encoded on the GPU, on the way
    /// or later: what every cost was before compression, and every cost
    /// under `Off`.
    pub const fn is_identity(&self) -> bool {
        self.transfer.0 == self.gpu.0
            && self.transient.0 == 0
            && self.encode_blocks == 0
            && self.compressed.0 == 0
            && self.compress_blocks == 0
    }

    /// Whether `actual` acknowledged against this estimate is an encode
    /// that fell back to the form it crossed in: more than resident, no
    /// more than what crossed. Never for an identity cost.
    pub fn is_fallback(&self, actual: Bytes) -> bool {
        self.gpu < actual && actual <= self.transfer
    }

    /// The sum of two costs, part by part.
    pub const fn plus(self, o: Cost) -> Cost {
        Cost {
            gpu: self.gpu.plus(o.gpu),
            staging: self.staging.plus(o.staging),
            transfer: self.transfer.plus(o.transfer),
            transient: self.transient.plus(o.transient),
            encode_blocks: self.encode_blocks.saturating_add(o.encode_blocks),
            compressed: self.compressed.plus(o.compressed),
            compress_blocks: self.compress_blocks.saturating_add(o.compress_blocks),
        }
    }
}

const fn align(v: u64, to: u64) -> u64 {
    v.div_ceil(to).saturating_mul(to)
}

/// A mesh: vertex and index buffers and the per-tile uniform, aligned, plus
/// the CPU copy kept for stitching when `cpu_copy` (unified memory pays it).
pub fn mesh_cost(vertices: u64, stride: u64, indices: u64, cpu_copy: bool) -> Cost {
    let vb = align(vertices.saturating_mul(stride), BUFFER_ALIGNMENT);
    let ib = align(indices.saturating_mul(4), BUFFER_ALIGNMENT);
    let uniform = align(TILE_UNIFORM_BYTES, UNIFORM_ALIGNMENT);
    let staging = vb.saturating_add(ib).saturating_add(uniform);
    let copy = if cpu_copy { vb.saturating_add(ib) } else { 0 };
    Cost::new(Bytes(staging.saturating_add(copy)), Bytes(staging))
}

/// A texture's pixel format, for its byte cost.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TextureFormat {
    /// 4 bytes per texel.
    Rgba8,
    /// ASTC 4×4 or BC7: 16 bytes per 4×4 block.
    Block16,
    /// BC1: 8 bytes per 4×4 block.
    Block8,
}

/// A drape texture `w × h` with its full mip chain.
pub fn drape_cost(w: u32, h: u32, format: TextureFormat) -> Cost {
    let (mut w, mut h) = (u64::from(w.max(1)), u64::from(h.max(1)));
    let (mut gpu, mut staging) = (0u64, 0u64);
    loop {
        let (row, rows) = match format {
            TextureFormat::Rgba8 => (w * 4, h),
            TextureFormat::Block16 => (w.div_ceil(4) * 16, h.div_ceil(4)),
            TextureFormat::Block8 => (w.div_ceil(4) * 8, h.div_ceil(4)),
        };
        gpu = gpu.saturating_add(row.saturating_mul(rows));
        staging = staging.saturating_add(align(row, ROW_ALIGNMENT).saturating_mul(rows));
        if w == 1 && h == 1 {
            break;
        }
        w = (w / 2).max(1);
        h = (h / 2).max(1);
    }
    Cost::new(Bytes(gpu), Bytes(staging))
}

/// The 4×4 blocks a `w × h` texture with its full mip chain holds: what
/// encoding it on the GPU writes (5,463 for 256²).
pub fn chain_blocks(w: u32, h: u32) -> u64 {
    let (mut w, mut h) = (u64::from(w.max(1)), u64::from(h.max(1)));
    let mut blocks = 0u64;
    loop {
        blocks = blocks.saturating_add(w.div_ceil(4).saturating_mul(h.div_ceil(4)));
        if w == 1 && h == 1 {
            break;
        }
        w = (w / 2).max(1);
        h = (h / 2).max(1);
    }
    blocks
}

/// Whether `actual` is within 5 % of `estimate`.
pub fn within_drift(estimate: Bytes, actual: Bytes) -> bool {
    let (e, a) = (u128::from(estimate.0), u128::from(actual.0));
    e.abs_diff(a) * 100 <= e * 5
}
