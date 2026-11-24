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
//! **Display at once, compress what lasts (CB9).** RGBA8 marked for a
//! block format is uploaded and drawn as RGBA8 at once, its texture kept
//! readable (`COPY_SRC`). When the graphics budget decides a drape is worth
//! compressing (`ServerMessage::Compress`), `texcomp`'s compute kernels read
//! each of its resident textures, mip chain included, straight from the GPU
//! — no readback, no re-upload — into a new texture of the block format;
//! the new one is bound to every drape that names it, and only then is the
//! RGBA8 one let go: never a gap, at most one frame of both. **A tile that
//! cannot be encoded stays RGBA8**: never a hole.
//!
//! The compressions are paced by the pump on the core's encode lane
//! (`tuile_core::budget::EncodeBucket`, CB6): a frame's share of blocks,
//! its measured headroom over the cost of a block — measured with
//! timestamps where the device has them, assumed from the viewer's measure
//! elsewhere — after the frame's uploads, which never wait for it.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use crate::ops::OpCounters;
use texcomp::gpu::{
    Destination, EncodeError, EncoderScratch, GpuEncoder, Quality, Source, SourceLevel, TileInto,
};
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

/// The encode level a person names: a whole number from 0 to
/// [`MAX_ENCODE_LEVEL`]. Anything else is an error that says so, never a
/// level guessed at.
pub fn encode_level_named(text: &str) -> Result<u32, String> {
    match text.trim().parse::<u32>() {
        Ok(level) if level <= MAX_ENCODE_LEVEL => Ok(level),
        _ => Err(format!(
            "imagery encode level {text:?} is not a whole number from 0 to {MAX_ENCODE_LEVEL}"
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

/// How hard the GPU encoder searches each block: texcomp's
/// `Quality::level`, 0 (fastest) to [`MAX_ENCODE_LEVEL`]. A number, not a
/// switch: each level trades GPU time for quality, and a host sets the one
/// its device affords ([`GpuContext::with_encode_level`](crate::GpuContext::with_encode_level)).
///
/// 0 by default: the search tuile has always encoded with, at the cost the
/// pump's lane was measured on. Level 1 gains up to 2.8 dB on ASTC 4×4
/// aerial imagery for about 19 times the GPU time, 0.1 dB on BC7 for three
/// times (texcomp's README, Apple M2).
pub const DEFAULT_ENCODE_LEVEL: u32 = 0;

/// The highest encode level; a higher one is taken as this.
pub const MAX_ENCODE_LEVEL: u32 = Quality::MAX_LEVEL;

/// GPU nanoseconds per block assumed before the device has measured any, at
/// encode `level`.
///
/// Level 0 is the viewer's measure on Apple M-series (Metal), timestamps
/// around the encode pass — about 1.7–1.9 µs a block, 9.5 ms a 256² BC7 tile
/// with its chain (`gpu-compressed-textures.md` §9.1), taken for ASTC 4×4
/// too until its own first measure: an over-estimate paces slower, never
/// faster. Above it, the larger of that and texcomp's published GPU time per
/// 256² tile with its chain (5,463 blocks) at that level on Apple M2 —
/// level 3, which it does not publish, is assumed at level 4's cost.
fn assumed_ns_per_block(format: BlockFormat, level: u32) -> f64 {
    const VIEWER: f64 = 1_739.0;
    // GPU ms per 256² tile with its chain, aerial imagery, levels 1 to 4.
    let tile_ms: f64 = match (format, level.min(MAX_ENCODE_LEVEL)) {
        (_, 0) => return VIEWER,
        (BlockFormat::Bc7, 1) => 12.1,
        (BlockFormat::Bc7, 2) => 32.0,
        (BlockFormat::Bc7, _) => 144.1,
        (BlockFormat::Astc4x4, 1) => 26.6,
        (BlockFormat::Astc4x4, 2) => 36.0,
        (BlockFormat::Astc4x4, _) => 100.4,
    };
    (tile_ms * 1e6 / 5_463.0).max(VIEWER)
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

/// One resident RGBA8 texture to compress in place: its texture (with
/// `COPY_SRC`), its size and its levels.
pub(crate) struct InPlace<'a> {
    pub(crate) texture: &'a wgpu::Texture,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) levels: u32,
}

/// The GPU encoder of a context whose plan compresses on the GPU, with the
/// buffers it reuses and its stopwatch.
pub(crate) struct Compressor {
    format: BlockFormat,
    /// See [`DEFAULT_ENCODE_LEVEL`].
    level: u32,
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
    pub(crate) fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: BlockFormat,
        level: u32,
    ) -> Self {
        let level = level.min(MAX_ENCODE_LEVEL);
        let encoder = GpuEncoder::with_quality(device, to_texcomp(format), Quality::level(level));
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
            level,
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
            .unwrap_or_else(|| assumed_ns_per_block(self.format, self.level))
    }

    /// Records the compression in place of each tile — an RGBA8 texture
    /// already resident, every level of its chain — into `cmd`: one compute
    /// pass for them all, reading each level straight from its texture (no
    /// readback, no upload), and the copies into a new texture of the
    /// encoder's format per tile. The new textures hold their blocks once
    /// `cmd` is submitted, which must come before the next record
    /// ([`Self::busy`]); [`Self::submitted`] is told of it. An error is a
    /// tile to keep as it is.
    ///
    /// The pass is timed when the stopwatch is free: one measure per pass,
    /// whatever it holds.
    pub(crate) fn record_in_place(
        &mut self,
        device: &wgpu::Device,
        ops: &OpCounters,
        queue: &wgpu::Queue,
        cmd: &mut wgpu::CommandEncoder,
        tiles: &[InPlace<'_>],
    ) -> Vec<Result<wgpu::Texture, EncodeError>> {
        self.collect();
        let format = texcomp::gpu::texture_format(to_texcomp(self.format), true);
        let targets: Vec<wgpu::Texture> = tiles
            .iter()
            .map(|t| {
                device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("tuile base color, compressed in place"),
                    size: wgpu::Extent3d {
                        width: t.width,
                        height: t.height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: t.levels,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                })
            })
            .collect();
        let size = |t: &InPlace<'_>, l: u32| ((t.width >> l).max(1), (t.height >> l).max(1));
        let sources: Vec<Vec<SourceLevel<'_>>> = tiles
            .iter()
            .map(|t| {
                (0..t.levels)
                    .map(|l| {
                        let (width, height) = size(t, l);
                        SourceLevel {
                            width,
                            height,
                            source: Source::Texture {
                                texture: t.texture,
                                mip_level: l,
                                origin: wgpu::Origin3d::ZERO,
                            },
                        }
                    })
                    .collect()
            })
            .collect();
        let destinations: Vec<Vec<Destination<'_>>> = tiles
            .iter()
            .zip(&targets)
            .map(|(t, to)| {
                (0..t.levels)
                    .map(|l| Destination {
                        texture: to,
                        mip_level: l,
                        origin: wgpu::Origin3d::ZERO,
                    })
                    .collect()
            })
            .collect();
        let into: Vec<TileInto<'_>> = sources
            .iter()
            .zip(&destinations)
            .map(|(levels, destinations)| TileInto {
                levels,
                destinations,
            })
            .collect();
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
                .encode_into_with(&mut self.scratch, device, queue, cmd, &into, writes);
        let mut blocks = 0;
        let mut any = false;
        let mut out = Vec::with_capacity(tiles.len());
        for ((t, result), to) in tiles.iter().zip(results).zip(targets) {
            match result {
                Ok(()) => {
                    any = true;
                    self.stats.encoded += 1;
                    blocks += tile_blocks(self.format, t.width, t.height);
                    OpCounters::add(&ops.textures_created, 1);
                    OpCounters::add(&ops.encoded_tiles, 1);
                    out.push(Ok(to));
                }
                Err(e) => {
                    self.stats.fell_back += 1;
                    out.push(Err(e));
                }
            }
        }
        self.stats.blocks += blocks;
        if any {
            // The pass, and the encoder's table of jobs.
            OpCounters::add(&ops.encode_passes, 1);
            ops.staged(&ops.write_buffer, 256 * tiles.len() as u64);
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
        out
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
            let ms = tile as f64 * assumed_ns_per_block(format, DEFAULT_ENCODE_LEVEL) / 1e6;
            assert!((9.0..10.0).contains(&ms), "{format:?}: {ms} ms a tile");
        }
    }

    /// An encode level is a whole number within the encoder's, or an error.
    #[test]
    fn an_encode_level_is_named_by_its_number() {
        assert_eq!(encode_level_named("0"), Ok(0));
        assert_eq!(encode_level_named(" 4 "), Ok(4));
        for bad in ["", "5", "-1", "1.5", "high"] {
            assert!(encode_level_named(bad).is_err(), "{bad:?}");
        }
    }

    /// **A higher level costs at least as much, and never less than the
    /// viewer's measure**: the lane shrinks as the search grows, so a frame
    /// spends the same GPU time whatever the level. Beyond the highest,
    /// the highest.
    #[test]
    fn the_assumed_cost_climbs_with_the_level() {
        for format in [BlockFormat::Bc7, BlockFormat::Astc4x4] {
            let costs: Vec<f64> = (0..=MAX_ENCODE_LEVEL + 2)
                .map(|level| assumed_ns_per_block(format, level))
                .collect();
            assert!(
                costs.windows(2).all(|w| w[0] <= w[1]),
                "{format:?}: {costs:?}"
            );
            assert!(costs.iter().all(|&c| c >= 1_739.0), "{format:?}: {costs:?}");
            assert_eq!(costs[MAX_ENCODE_LEVEL as usize], costs[costs.len() - 1]);
        }
        // Level 1 at texcomp's measure: ASTC 4×4 26.6 ms a tile.
        let ms = 5_463.0 * assumed_ns_per_block(BlockFormat::Astc4x4, 1) / 1e6;
        assert!((26.0..27.0).contains(&ms), "{ms} ms");
    }
}
