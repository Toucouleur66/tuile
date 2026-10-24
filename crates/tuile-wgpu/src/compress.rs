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
//! The encodes are paced by the pump under their own per-frame share of GPU
//! time ([`EncodePace`]), measured with timestamps where the device has
//! them and assumed from the encoder's measured cost per block elsewhere.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

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

/// GPU nanoseconds per block assumed before the device has measured any:
/// texcomp's published cost on Apple M2 (Metal), wall clock per 256² tile
/// with its mips (5463 blocks) — BC7 5.1 ms, ASTC 4×4 1.9 ms. Wall clock
/// includes the upload, so it over-states the kernel; a measure replaces it.
fn assumed_ns_per_block(format: BlockFormat) -> f64 {
    match format {
        BlockFormat::Bc7 => 5.1e6 / 5463.0,
        BlockFormat::Astc4x4 => 1.9e6 / 5463.0,
    }
}

/// The GPU's share of a frame the imagery encodes may take, and how many
/// blocks that is.
///
/// Deliberately simple, and outside the budget's own accounting (a later
/// step brings encode time into `FrameDeadline`): one number of GPU time per
/// frame, divided by the cost of a block — measured by timestamps around the
/// encoder's compute pass where the device has `TIMESTAMP_QUERY`, assumed
/// from the encoder's published cost elsewhere. A frame always takes the
/// first resource that needs encoding, however large, so nothing waits for
/// ever; the next one waits for a frame with room.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EncodePace {
    /// GPU time per frame for encodes, microseconds.
    pub share_micros: u64,
}

impl EncodePace {
    /// A quarter of a 60 Hz frame.
    pub const DEFAULT: EncodePace = EncodePace {
        share_micros: 4_000,
    };

    /// Blocks a frame may encode at `ns_per_block`.
    pub fn blocks_per_frame(self, ns_per_block: f64) -> u64 {
        (self.share_micros as f64 * 1_000.0 / ns_per_block.max(1.0)) as u64
    }
}

impl Default for EncodePace {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// What the encoder has done this session: tiles encoded, tiles that had to
/// stay RGBA8, and the GPU cost per block it last measured.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct EncodeStats {
    pub encoded: u64,
    pub fell_back: u64,
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
}

/// One timestamp pair in flight at a time: the pass's start and end,
/// resolved into a buffer that is read back once the GPU is done.
struct Timing {
    set: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    readback: wgpu::Buffer,
    /// [`IDLE`], [`MAPPING`] or [`READY`].
    state: Arc<AtomicU8>,
    /// The blocks the measured pass wrote.
    blocks: u64,
    period_ns: f32,
}

const IDLE: u8 = 0;
const MAPPING: u8 = 1;
const READY: u8 = 2;

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
        }
    }

    pub(crate) fn format(&self) -> BlockFormat {
        self.format
    }

    pub(crate) fn stats(&self) -> EncodeStats {
        self.stats
    }

    /// GPU nanoseconds per block: measured, or assumed until it is.
    pub(crate) fn ns_per_block(&self) -> f64 {
        self.stats
            .measured_ns_per_block
            .unwrap_or_else(|| assumed_ns_per_block(self.format))
    }

    /// Encodes `base` and its `mips` into a texture of the plan's format,
    /// submitted on `queue` before this returns. An error is a tile to upload
    /// as RGBA8.
    pub(crate) fn encode(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        base: &DecodedTexture,
        mips: &[DecodedTexture],
    ) -> Result<wgpu::Texture, EncodeError> {
        self.collect(device);
        let levels: Vec<Level<'_>> = std::iter::once(base)
            .chain(mips.iter())
            .map(|t| Level {
                width: t.width,
                height: t.height,
                rgba8: t.rgba8().unwrap_or_default(),
            })
            .collect();
        let mut cmd = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("tuile imagery encode"),
        });
        // Timed when the stopwatch is free; the others go untimed.
        let timed = self
            .timing
            .as_ref()
            .filter(|t| t.state.load(Ordering::Acquire) == IDLE);
        let writes = timed.map(|t| wgpu::ComputePassTimestampWrites {
            query_set: &t.set,
            beginning_of_pass_write_index: Some(0),
            end_of_pass_write_index: Some(1),
        });
        let result = self
            .encoder
            .encode_with(
                &mut self.scratch,
                device,
                queue,
                &mut cmd,
                &[&levels],
                true,
                writes,
            )
            .pop()
            .unwrap_or(Err(EncodeError::Empty));
        let measuring = timed.is_some() && result.is_ok();
        if measuring {
            if let Some(t) = &self.timing {
                cmd.resolve_query_set(&t.set, 0..2, &t.resolve, 0);
                cmd.copy_buffer_to_buffer(&t.resolve, 0, &t.readback, 0, 16);
            }
        }
        // Submitted now, before the next call rewrites the scratch.
        queue.submit([cmd.finish()]);
        match &result {
            Ok(_) => self.stats.encoded += 1,
            Err(_) => self.stats.fell_back += 1,
        }
        if measuring {
            if let Some(t) = &mut self.timing {
                t.blocks = tile_blocks(self.format, base.width, base.height);
                t.state.store(MAPPING, Ordering::Release);
                let state = Arc::clone(&t.state);
                t.readback
                    .slice(..)
                    .map_async(wgpu::MapMode::Read, move |done| {
                        state.store(if done.is_ok() { READY } else { IDLE }, Ordering::Release);
                    });
            }
        }
        result
    }

    /// Reads the last measured pass, if the GPU has finished it, into the
    /// cost per block: a moving average, so one slow pass does not halve the
    /// next frame's share.
    pub(crate) fn collect(&mut self, device: &wgpu::Device) {
        let Some(t) = &mut self.timing else {
            return;
        };
        if t.state.load(Ordering::Acquire) == MAPPING {
            // Lets a finished map call back; waits for nothing.
            let _ = device.poll(wgpu::PollType::Poll);
        }
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

    /// **The pace is GPU time over the cost of a block**: about one 256² BC7
    /// tile per frame at the assumed cost, nearly three of ASTC.
    #[test]
    fn the_pace_is_time_over_the_cost_of_a_block() {
        let pace = EncodePace::DEFAULT;
        let tile = tile_blocks(BlockFormat::Bc7, 256, 256);
        assert_eq!(tile, 5463);
        let bc7 = pace.blocks_per_frame(assumed_ns_per_block(BlockFormat::Bc7));
        let astc = pace.blocks_per_frame(assumed_ns_per_block(BlockFormat::Astc4x4));
        assert!(bc7 < tile && bc7 > tile / 2, "{bc7}");
        assert!(astc > 2 * tile && astc < 3 * tile, "{astc}");
        assert_eq!(
            pace.blocks_per_frame(0.0),
            4_000_000,
            "never divides by zero"
        );
    }
}
