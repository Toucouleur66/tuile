// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Imagery held block-compressed on the GPU: **the consumer's choice, off by
//! default.**
//!
//! A consumer that wants it resolves its [`ImageryCompression`] against its
//! adapter ([`texture_caps`]), creates its device with the features the
//! resulting plan needs ([`plan_features`]) and hands the plan to its context
//! ([`GpuContext::with_imagery_plan`](crate::GpuContext::with_imagery_plan)).
//! The loader marks what it decodes with the same plan, so the budget charges
//! each texture in the format it will be held in.
//!
//! At upload, RGBA8 marked for a block format is encoded on the GPU by
//! `texcomp`'s compute kernels, mip chain included, into a texture of that
//! format. **Any encode that cannot be done uploads RGBA8 instead**: a size
//! that is not whole blocks, a tile too large for the device, a context with
//! no encoder. The ground is never left without a texture, and the swap
//! order is the RGBA8 path's: the texture is complete before anything binds
//! it.
//!
//! The encodes are paced by the pump on the core's encode lane
//! (`tuile_core::budget::EncodeBucket`, CB6): a frame's share of blocks,
//! its time over the cost of a block — measured with timestamps where the
//! device has them, assumed from the viewer's measure elsewhere.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use crate::ops::OpCounters;
use texcomp::gpu::{EncodeError, EncoderScratch, GpuEncoder, Level};
use tuile_core::content::DecodedTexture;
use tuile_core::texels::{
    BlockFormat, GpuTextureCaps, ImageryEncode, ImageryFormat, ImageryPlan, StoredPlan,
};

pub use tuile_core::texels::ImageryCompression;

/// What `adapter` can do with block-compressed textures: the formats it can
/// sample, and whether it runs the GPU encoder.
pub fn texture_caps(adapter: &wgpu::Adapter) -> GpuTextureCaps {
    caps_of(
        adapter.features(),
        adapter.get_downlevel_capabilities().flags,
        &adapter.limits(),
    )
}

/// [`texture_caps`], from the parts an adapter reports.
pub fn caps_of(
    features: wgpu::Features,
    downlevel: wgpu::DownlevelFlags,
    limits: &wgpu::Limits,
) -> GpuTextureCaps {
    GpuTextureCaps {
        bc: features.contains(wgpu::Features::TEXTURE_COMPRESSION_BC),
        astc: features.contains(wgpu::Features::TEXTURE_COMPRESSION_ASTC),
        compute: texcomp::gpu::runs_on(downlevel, limits),
    }
}

/// The device features `plan` needs: its block format's, to sample it.
/// Nothing for RGBA8 — a consumer that did not ask requests nothing new.
pub fn plan_features(plan: ImageryPlan) -> wgpu::Features {
    match plan.target.block() {
        Some(format) => texcomp::gpu::feature(to_texcomp(format)),
        None => wgpu::Features::empty(),
    }
}

/// The device features a stored plan needs: each block format it holds
/// tiles in, to sample them.
pub fn stored_features(stored: StoredPlan) -> wgpu::Features {
    let mut features = match stored.uastc.block() {
        Some(format) => texcomp::gpu::feature(to_texcomp(format)),
        None => wgpu::Features::empty(),
    };
    if stored.keep_bc7 {
        features |= texcomp::gpu::feature(texcomp::BlockFormat::Bc7);
    }
    if stored.keep_astc {
        features |= texcomp::gpu::feature(texcomp::BlockFormat::Astc4x4);
    }
    features
}

/// The plan `choice` comes to on `adapter`, and the features to create the
/// device with for it: the format's, the formats tiles stored compressed are
/// held in under the same choice ([`ImageryCompression::stored`]), and
/// timestamps where the adapter has them and the GPU encodes — what
/// [`EncodePace`] measures with. Off is RGBA8 and no feature at all.
pub fn plan_for(
    choice: ImageryCompression,
    adapter: &wgpu::Adapter,
) -> (ImageryPlan, wgpu::Features) {
    let caps = texture_caps(adapter);
    let plan = choice.plan(caps);
    let mut features = plan_features(plan) | stored_features(choice.stored(caps));
    if plan.encode == ImageryEncode::Gpu {
        features |= adapter.features() & wgpu::Features::TIMESTAMP_QUERY;
    }
    (plan, features)
}

/// The plan `choice` comes to on the adapter a host will draw with, asked
/// before it has a window to create its device against: what a loader that
/// starts before the device marks its first tiles with. The device confirms
/// it ([`plan_for`]) once it exists. Off asks nothing of the system.
///
/// With it, what the same choice does with tiles stored already compressed.
pub async fn adapter_plan(
    choice: ImageryCompression,
    backends: Option<wgpu::Backends>,
) -> (ImageryPlan, StoredPlan) {
    if choice == ImageryCompression::Off {
        return (ImageryPlan::RGBA8, StoredPlan::OFF);
    }
    let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    if let Some(backends) = backends {
        descriptor.backends = backends;
    }
    let instance = wgpu::Instance::new(descriptor);
    match instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
            apply_limit_buckets: false,
        })
        .await
    {
        Ok(adapter) => {
            let caps = texture_caps(&adapter);
            (choice.plan(caps), choice.stored(caps))
        }
        Err(_) => (ImageryPlan::RGBA8, StoredPlan::OFF),
    }
}

/// The choice a person names: `off`, `auto`, `bc7` or `astc`, in any case.
pub fn compression_named(name: &str) -> Result<ImageryCompression, String> {
    match name.trim().to_ascii_lowercase().as_str() {
        "off" | "none" | "rgba8" => Ok(ImageryCompression::Off),
        "auto" => Ok(ImageryCompression::Auto),
        "bc7" => Ok(ImageryCompression::Format(BlockFormat::Bc7)),
        "astc" | "astc4x4" => Ok(ImageryCompression::Format(BlockFormat::Astc4x4)),
        other => Err(format!(
            "imagery compression {other:?} is not one of off, auto, bc7, astc"
        )),
    }
}

/// A short name for a resident format, for a status line.
pub fn format_name(format: ImageryFormat) -> &'static str {
    match format {
        ImageryFormat::Rgba8 => "RGBA8",
        ImageryFormat::Bc7 => "BC7",
        ImageryFormat::Astc4x4 => "ASTC 4x4",
    }
}

pub(crate) fn to_texcomp(format: BlockFormat) -> texcomp::BlockFormat {
    match format {
        BlockFormat::Bc7 => texcomp::BlockFormat::Bc7,
        BlockFormat::Astc4x4 => texcomp::BlockFormat::Astc4x4,
    }
}

/// The blocks encoding a `width × height` tile into `format` writes, mip
/// chain included: what the pump paces by.
pub(crate) fn tile_blocks(format: BlockFormat, width: u32, height: u32) -> u64 {
    texcomp::tile_cost(to_texcomp(format), width, height).blocks
}

/// The GPU bytes the encoder's scratch holds for `tiles` 256² tiles at once,
/// mip chains included, under `plan`: what a host reserves off the drapes'
/// limits once at setup (`BudgetConfig::reserved`, CB4) — the RGBA8 the
/// kernels read, the blocks they write with rows aligned for the copy, and
/// a job per level. Nothing for a plan that does not encode on the GPU.
pub fn encode_reservation(plan: ImageryPlan, tiles: u32) -> tuile_core::budget::Bytes {
    if plan.encode != ImageryEncode::Gpu {
        return tuile_core::budget::Bytes::ZERO;
    }
    let (mut w, mut h) = (256u64, 256u64);
    let (mut src, mut dst, mut levels) = (0u64, 0u64, 0u64);
    loop {
        src += w * h * 4;
        dst += (w.div_ceil(4) * 16).div_ceil(256) * 256 * h.div_ceil(4);
        levels += 1;
        if w == 1 && h == 1 {
            break;
        }
        w = (w / 2).max(1);
        h = (h / 2).max(1);
    }
    // A job's uniform is aligned to the device's offset alignment: 256 at
    // most.
    tuile_core::budget::Bytes(u64::from(tiles.max(1)) * (src + dst + levels * 256))
}

/// GPU nanoseconds per block assumed before the device has measured any:
/// the viewer's measure on Apple M-series (Metal), timestamps around the
/// encode pass — about 1.7–1.9 µs a block, 9.5 ms a 256² BC7 tile with its
/// chain (`gpu-compressed-textures.md` §9.1). Taken for ASTC 4×4 too until
/// its own first measure: an over-estimate paces slower, never faster.
fn assumed_ns_per_block(_format: BlockFormat) -> f64 {
    1_739.0
}

/// What the encoder has done this session: tiles encoded, tiles that had to
/// stay RGBA8, and the GPU cost per block it last measured.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EncodeStats {
    pub encoded: u64,
    pub fell_back: u64,
    /// The 4×4 blocks the encoded tiles wrote, mip chains included.
    pub blocks: u64,
    /// Nanoseconds per block from timestamps, `None` until measured.
    pub measured_ns_per_block: Option<f64>,
}

/// The GPU encoder of a context whose plan encodes on the GPU, with the
/// buffers it reuses and its stopwatch.
pub(crate) struct Compressor {
    format: BlockFormat,
    encoder: GpuEncoder,
    scratch: EncoderScratch,
    timing: Option<Timing>,
    stats: EncodeStats,
    /// See [`Self::busy`].
    busy: bool,
}

/// One timestamp pair in flight at a time: the pass's start and end,
/// resolved into a buffer that is read back once the GPU is done.
struct Timing {
    set: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    readback: wgpu::Buffer,
    /// [`IDLE`], [`RECORDED`], [`MAPPING`] or [`READY`].
    state: Arc<AtomicU8>,
    /// The blocks the measured pass wrote.
    blocks: u64,
    period_ns: f32,
}

const IDLE: u8 = 0;
const MAPPING: u8 = 1;
const READY: u8 = 2;
/// Written by a recorded pass not yet submitted.
const RECORDED: u8 = 3;

impl Compressor {
    pub(crate) fn new(device: &wgpu::Device, queue: &wgpu::Queue, format: BlockFormat) -> Self {
        let encoder = GpuEncoder::new(device, to_texcomp(format));
        // Room for one 256² tile up front: the size every drape tile is.
        let scratch = EncoderScratch::with_capacity(device, &encoder, 1, 256, 256);
        let timing = device
            .features()
            .contains(wgpu::Features::TIMESTAMP_QUERY)
            .then(|| Timing {
                set: device.create_query_set(&wgpu::QuerySetDescriptor {
                    label: Some("tuile imagery encode"),
                    ty: wgpu::QueryType::Timestamp,
                    count: 2,
                }),
                resolve: device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("tuile imagery encode timestamps"),
                    size: 16,
                    usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                    mapped_at_creation: false,
                }),
                readback: device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("tuile imagery encode timestamps, read back"),
                    size: 16,
                    usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }),
                state: Arc::new(AtomicU8::new(IDLE)),
                blocks: 0,
                period_ns: queue.get_timestamp_period(),
            });
        Self {
            format,
            encoder,
            scratch,
            timing,
            stats: EncodeStats::default(),
            busy: false,
        }
    }

    pub(crate) fn format(&self) -> BlockFormat {
        self.format
    }

    pub(crate) fn stats(&self) -> EncodeStats {
        self.stats
    }

    /// The GPU bytes its scratch holds now.
    pub(crate) fn scratch_bytes(&self) -> u64 {
        self.scratch.bytes()
    }

    /// GPU nanoseconds per block: measured, or assumed until it is.
    pub(crate) fn ns_per_block(&self) -> f64 {
        self.stats
            .measured_ns_per_block
            .unwrap_or_else(|| assumed_ns_per_block(self.format))
    }

    /// Records the encode of each tile — a base and its mips — into `cmd`:
    /// one compute pass for them all, and the copies into their textures.
    /// The texels land with `cmd`'s submit, which must come before the next
    /// record ([`Self::busy`]); [`Self::submitted`] is told of it. An error
    /// is a tile to upload as RGBA8.
    ///
    /// The pass is timed when the stopwatch is free: one measure per pass,
    /// whatever it holds.
    pub(crate) fn record(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        ops: &OpCounters,
        cmd: &mut wgpu::CommandEncoder,
        tiles: &[(&DecodedTexture, &[DecodedTexture])],
    ) -> Vec<Result<wgpu::Texture, EncodeError>> {
        self.collect();
        let levels: Vec<Vec<Level<'_>>> = tiles
            .iter()
            .map(|(base, mips)| {
                std::iter::once(*base)
                    .chain(mips.iter())
                    .map(|t| Level {
                        width: t.width,
                        height: t.height,
                        rgba8: t.rgba8().unwrap_or_default(),
                    })
                    .collect()
            })
            .collect();
        let chains: Vec<&[Level<'_>]> = levels.iter().map(Vec::as_slice).collect();
        let timed = self
            .timing
            .as_ref()
            .filter(|t| t.state.load(Ordering::Acquire) == IDLE);
        let writes = timed.map(|t| wgpu::ComputePassTimestampWrites {
            query_set: &t.set,
            beginning_of_pass_write_index: Some(0),
            end_of_pass_write_index: Some(1),
        });
        let results =
            self.encoder
                .encode_with(&mut self.scratch, device, queue, cmd, &chains, true, writes);
        let mut blocks = 0;
        let mut any = false;
        for ((base, _), (result, chain)) in tiles.iter().zip(results.iter().zip(&chains)) {
            match result {
                Ok(_) => {
                    any = true;
                    self.stats.encoded += 1;
                    blocks += tile_blocks(self.format, base.width, base.height);
                    // The encoder's texture, and its writes: one per level
                    // of texels.
                    OpCounters::add(&ops.textures_created, 1);
                    OpCounters::add(&ops.encoded_tiles, 1);
                    for level in chain.iter() {
                        ops.staged(&ops.write_buffer, level.rgba8.len() as u64);
                    }
                }
                Err(_) => self.stats.fell_back += 1,
            }
        }
        self.stats.blocks += blocks;
        if any {
            // The pass, and the encoder's table of jobs.
            OpCounters::add(&ops.encode_passes, 1);
            ops.staged(&ops.write_buffer, 256 * chains.len() as u64);
            self.busy = true;
            if timed.is_some() {
                if let Some(t) = &mut self.timing {
                    cmd.resolve_query_set(&t.set, 0..2, &t.resolve, 0);
                    cmd.copy_buffer_to_buffer(&t.resolve, 0, &t.readback, 0, 16);
                    t.blocks = blocks;
                    t.state.store(RECORDED, Ordering::Release);
                }
            }
        }
        results
    }

    /// Whether a record is waiting for its submit: the scratch holds its
    /// texels, and the next record would overwrite them.
    pub(crate) fn busy(&self) -> bool {
        self.busy
    }

    /// The encoder the last record went into was submitted: the scratch is
    /// free, and a timed pass's stopwatch is read back once the GPU is done
    /// — without waiting for it.
    pub(crate) fn submitted(&mut self, ops: &OpCounters) {
        self.busy = false;
        let Some(t) = &mut self.timing else {
            return;
        };
        if t.state.load(Ordering::Acquire) != RECORDED {
            return;
        }
        t.state.store(MAPPING, Ordering::Release);
        let state = Arc::clone(&t.state);
        OpCounters::add(&ops.readbacks, 1);
        t.readback
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |done| {
                state.store(if done.is_ok() { READY } else { IDLE }, Ordering::Release);
            });
    }

    /// Reads the last measured pass, if the GPU has finished it, into the
    /// cost per block: a moving average, so one slow pass does not halve the
    /// next frame's share.
    ///
    /// Nothing is polled here: a submit lets a finished map call back, and
    /// the frames submit.
    pub(crate) fn collect(&mut self) {
        let Some(t) = &mut self.timing else {
            return;
        };
        if t.state.load(Ordering::Acquire) != READY {
            return;
        }
        let ticks = t.readback.slice(..).get_mapped_range().ok().map(|range| {
            let stamps: &[u64] = bytemuck::cast_slice(&range[..16]);
            stamps[1].saturating_sub(stamps[0])
        });
        t.readback.unmap();
        t.state.store(IDLE, Ordering::Release);
        if let Some(ticks) = ticks.filter(|&ticks| ticks > 0 && t.blocks > 0) {
            let ns = ticks as f64 * f64::from(t.period_ns) / t.blocks as f64;
            self.stats.measured_ns_per_block = Some(match self.stats.measured_ns_per_block {
                Some(was) => 0.8 * was + 0.2 * ns,
                None => ns,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The capabilities are the adapter's**, and the encoder needs compute:
    /// WebGL2 reports no compute shaders and never runs it.
    #[test]
    fn caps_come_from_the_adapter() {
        let limits = wgpu::Limits::default();
        let both =
            wgpu::Features::TEXTURE_COMPRESSION_BC | wgpu::Features::TEXTURE_COMPRESSION_ASTC;
        let apple = caps_of(both, wgpu::DownlevelFlags::all(), &limits);
        assert_eq!(
            apple,
            GpuTextureCaps {
                bc: true,
                astc: true,
                compute: true
            }
        );
        let webgl2 = caps_of(
            wgpu::Features::TEXTURE_COMPRESSION_ASTC,
            wgpu::DownlevelFlags::empty(),
            &wgpu::Limits::downlevel_webgl2_defaults(),
        );
        assert!(webgl2.astc && !webgl2.bc && !webgl2.compute);
    }

    /// The four names a person types, and nothing else.
    #[test]
    fn a_choice_is_named_or_refused() {
        assert_eq!(compression_named("off"), Ok(ImageryCompression::Off));
        assert_eq!(compression_named(" AUTO "), Ok(ImageryCompression::Auto));
        assert_eq!(
            compression_named("bc7"),
            Ok(ImageryCompression::Format(BlockFormat::Bc7))
        );
        assert_eq!(
            compression_named("astc"),
            Ok(ImageryCompression::Format(BlockFormat::Astc4x4))
        );
        assert!(compression_named("etc2")
            .unwrap_err()
            .contains("off, auto, bc7, astc"));
    }

    /// **Off asks the device for nothing**; a block plan asks for its format.
    #[test]
    fn only_a_block_plan_asks_for_a_feature() {
        let caps = GpuTextureCaps {
            bc: true,
            astc: true,
            compute: true,
        };
        assert_eq!(
            plan_features(ImageryCompression::Off.plan(caps)),
            wgpu::Features::empty()
        );
        assert_eq!(
            plan_features(ImageryCompression::Auto.plan(caps)),
            wgpu::Features::TEXTURE_COMPRESSION_BC
        );
        assert_eq!(
            plan_features(ImageryCompression::Format(BlockFormat::Astc4x4).plan(caps)),
            wgpu::Features::TEXTURE_COMPRESSION_ASTC
        );
        // Stored tiles: nothing off, every format Auto keeps.
        assert_eq!(
            stored_features(ImageryCompression::Off.stored(caps)),
            wgpu::Features::empty()
        );
        assert_eq!(
            stored_features(ImageryCompression::Auto.stored(caps)),
            wgpu::Features::TEXTURE_COMPRESSION_BC | wgpu::Features::TEXTURE_COMPRESSION_ASTC
        );
    }

    /// **Until measured, a block costs what the viewer measured**: a 256²
    /// tile with its chain is 5,463 blocks, about 9.5 ms.
    #[test]
    fn the_assumed_cost_is_the_measured_one() {
        let tile = tile_blocks(BlockFormat::Bc7, 256, 256);
        assert_eq!(tile, 5463);
        for format in [BlockFormat::Bc7, BlockFormat::Astc4x4] {
            let ms = tile as f64 * assumed_ns_per_block(format) / 1e6;
            assert!((9.0..10.0).contains(&ms), "{format:?}: {ms} ms a tile");
        }
    }
}
