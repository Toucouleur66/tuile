// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The imagery format seam: which format a consumer holds its drape textures
//! in, decided once from what its GPU can do, and the texels a
//! [`DecodedTexture`](crate::content::DecodedTexture) carries.
//!
//! Nothing here names a graphics API. A consumer describes its adapter with a
//! [`GpuTextureCaps`] — filled from wgpu, from another engine's adapter, or by
//! hand in a test — and [`select`] answers with an [`ImageryPlan`]: the format
//! textures are held in once resident, and who encodes them into it. The plan
//! is a pure function of the capabilities, so the capability matrix is a unit
//! test and every frontend that fills the struct the same way gets the same
//! answer.
//!
//! The format a texture is held in is recorded on the texture itself
//! ([`Texels::resident`]), so its cost needs no argument and the runtime that
//! charges it and the consumer that uploads it read the same answer.

use crate::budget::cost::{drape_cost, Cost, TextureFormat};

/// What a consumer's GPU can do with block-compressed textures.
///
/// Filled by the host from its own adapter, so the core never sees a graphics
/// API type. Each flag means the format can be **sampled** once the device is
/// created with the matching feature requested.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct GpuTextureCaps {
    /// The BC family (BC1–BC7) can be sampled.
    pub bc: bool,
    /// ASTC LDR 4×4 can be sampled.
    pub astc: bool,
    /// The GPU block encoder runs here: compute shaders, and the storage
    /// buffers and workgroup limits the encoder needs. False on WebGL2, which
    /// has no compute.
    pub compute: bool,
}

/// A block-compressed format a texture is held in: 16 bytes per 4×4 block,
/// a quarter of RGBA8, alpha included.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlockFormat {
    /// BC7, sRGB.
    Bc7,
    /// ASTC LDR 4×4, sRGB.
    Astc4x4,
}

impl BlockFormat {
    /// Bytes per 4×4 block.
    pub const BLOCK_BYTES: usize = 16;
    /// Texels on each side of a block.
    pub const BLOCK_SIDE: u32 = 4;

    /// Its byte arithmetic, for the budget's estimator.
    pub const fn cost_format(self) -> TextureFormat {
        TextureFormat::Block16
    }

    /// The bytes of one `width × height` level: whole blocks, a level smaller
    /// than a block still taking one.
    pub const fn level_bytes(width: u32, height: u32) -> usize {
        width.div_ceil(Self::BLOCK_SIDE) as usize
            * height.div_ceil(Self::BLOCK_SIDE) as usize
            * Self::BLOCK_BYTES
    }
}

/// The format a drape texture is held in once resident on the GPU.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ImageryFormat {
    /// Uncompressed, 4 bytes per texel: what every consumer can sample.
    #[default]
    Rgba8,
    /// BC7: a quarter of the bytes.
    Bc7,
    /// ASTC 4×4: a quarter of the bytes.
    Astc4x4,
}

impl ImageryFormat {
    /// The block format, if it is one.
    pub const fn block(self) -> Option<BlockFormat> {
        match self {
            ImageryFormat::Rgba8 => None,
            ImageryFormat::Bc7 => Some(BlockFormat::Bc7),
            ImageryFormat::Astc4x4 => Some(BlockFormat::Astc4x4),
        }
    }

    /// Its byte arithmetic, for the budget's estimator.
    pub const fn cost_format(self) -> TextureFormat {
        match self.block() {
            None => TextureFormat::Rgba8,
            Some(b) => b.cost_format(),
        }
    }
}

impl From<BlockFormat> for ImageryFormat {
    fn from(b: BlockFormat) -> Self {
        match b {
            BlockFormat::Bc7 => ImageryFormat::Bc7,
            BlockFormat::Astc4x4 => ImageryFormat::Astc4x4,
        }
    }
}

/// Who turns decoded RGBA8 into the plan's block format.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ImageryEncode {
    /// Nobody: the texture is held as RGBA8.
    #[default]
    None,
    /// The consumer's GPU, at upload: the texels cross as RGBA8.
    Gpu,
    /// The decode worker, on the CPU: the texels cross as blocks.
    Cpu,
}

/// How a consumer holds its drape textures: the format, and who encodes it.
///
/// `encode` is [`ImageryEncode::None`] exactly when `target` is
/// [`ImageryFormat::Rgba8`]. [`select`] is what makes one; the default is the
/// uncompressed plan, which is what bake, film and every consumer that does
/// not ask get.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ImageryPlan {
    /// The format textures are held in once resident.
    pub target: ImageryFormat,
    /// Who encodes them into it.
    pub encode: ImageryEncode,
}

impl ImageryPlan {
    /// Uncompressed: RGBA8, no encoder.
    pub const RGBA8: ImageryPlan = ImageryPlan {
        target: ImageryFormat::Rgba8,
        encode: ImageryEncode::None,
    };
}

/// The plan for an adapter that can do `caps`.
///
/// In order of preference:
/// 1. compute and BC: BC7, encoded on the GPU;
/// 2. compute and ASTC: ASTC 4×4, encoded on the GPU;
/// 3. ASTC without compute (WebGL2 on a phone): ASTC 4×4, encoded on the CPU
///    in the decode worker;
/// 4. anything else: RGBA8. BC without compute (WebGL2 on a desktop) lands
///    here too: the CPU encoder writes ASTC only for now, and a BC7 one is
///    deferred until a desktop WebGL2 consumer needs it.
///
/// BC7 comes before ASTC where an adapter has both — Apple silicon on macOS,
/// and desktop WebGPU on Apple. Both hold 8 bits per texel, so memory does not
/// choose; quality does. On the imagery the gates measure, the GPU encoder's
/// BC7 has a worst tile at 40.3 dB against ASTC 4×4's 36.4, and a p99 ΔE2000
/// of 5.4 against 9.4 (`docs/explorations/gpu-compressed-textures.md`, §3,
/// §4.4 and §9.3).
pub const fn select(caps: GpuTextureCaps) -> ImageryPlan {
    let (target, encode) = match (caps.compute, caps.bc, caps.astc) {
        (true, true, _) => (ImageryFormat::Bc7, ImageryEncode::Gpu),
        (true, false, true) => (ImageryFormat::Astc4x4, ImageryEncode::Gpu),
        (false, _, true) => (ImageryFormat::Astc4x4, ImageryEncode::Cpu),
        _ => (ImageryFormat::Rgba8, ImageryEncode::None),
    };
    ImageryPlan { target, encode }
}

/// The texels of a decoded texture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Texels {
    /// Tightly packed RGBA8, level 0 only: a consumer builds the mip chain.
    Rgba8 {
        /// The pixels, row-major, 4 bytes each.
        pixels: Vec<u8>,
        /// The format the consumer holds them in once uploaded: RGBA8, or a
        /// block format its GPU encodes them into on the way.
        resident: ImageryFormat,
    },
    /// Block-compressed, every mip level already encoded, level 0 first.
    Blocks {
        /// Their format.
        format: BlockFormat,
        /// The levels, each its whole blocks, row-major.
        levels: Vec<Vec<u8>>,
    },
}

impl Texels {
    /// RGBA8 held as RGBA8: what every producer makes today.
    pub fn rgba8(pixels: Vec<u8>) -> Self {
        Texels::Rgba8 {
            pixels,
            resident: ImageryFormat::Rgba8,
        }
    }

    /// The format these texels are held in once resident.
    pub fn resident(&self) -> ImageryFormat {
        match self {
            Texels::Rgba8 { resident, .. } => *resident,
            Texels::Blocks { format, .. } => (*format).into(),
        }
    }

    /// The RGBA8 pixels, if that is what is carried.
    pub fn as_rgba8(&self) -> Option<&[u8]> {
        match self {
            Texels::Rgba8 { pixels, .. } => Some(pixels),
            Texels::Blocks { .. } => None,
        }
    }

    /// The bytes carried, every level included.
    pub fn byte_len(&self) -> usize {
        match self {
            Texels::Rgba8 { pixels, .. } => pixels.len(),
            Texels::Blocks { levels, .. } => levels.iter().map(Vec::len).sum(),
        }
    }

    /// What a `width × height` texture of these texels costs once resident,
    /// mip chain included, and what its upload moves.
    ///
    /// Held: the whole chain in the resident format. Moved: the texels as
    /// they cross — RGBA8 for a texture the GPU encodes on the way, blocks
    /// for one the worker already encoded.
    pub fn cost(&self, width: u32, height: u32) -> Cost {
        let held = drape_cost(width, height, self.resident().cost_format());
        let crossing = match self {
            Texels::Rgba8 { .. } => TextureFormat::Rgba8,
            Texels::Blocks { format, .. } => format.cost_format(),
        };
        Cost {
            gpu: held.gpu,
            staging: drape_cost(width, height, crossing).staging,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::Bytes;

    fn caps(bc: bool, astc: bool, compute: bool) -> GpuTextureCaps {
        GpuTextureCaps { bc, astc, compute }
    }

    const fn plan(target: ImageryFormat, encode: ImageryEncode) -> ImageryPlan {
        ImageryPlan { target, encode }
    }

    /// Every target platform, named, as its adapter reports it.
    #[test]
    fn the_capability_matrix() {
        use ImageryEncode::{Cpu, Gpu};
        use ImageryFormat::{Astc4x4, Bc7};
        let matrix = [
            (
                "desktop NVIDIA/AMD/Intel, Vulkan or DX12",
                caps(true, false, true),
                plan(Bc7, Gpu),
            ),
            (
                "Apple silicon, Metal on macOS",
                caps(true, true, true),
                plan(Bc7, Gpu),
            ),
            (
                "iPhone/iPad, Metal on iOS",
                caps(false, true, true),
                plan(Astc4x4, Gpu),
            ),
            (
                "Android, Vulkan on Mali/Adreno",
                caps(false, true, true),
                plan(Astc4x4, Gpu),
            ),
            (
                "Quest, Vulkan on Adreno",
                caps(false, true, true),
                plan(Astc4x4, Gpu),
            ),
            (
                "WebGPU desktop, texture-compression-bc",
                caps(true, false, true),
                plan(Bc7, Gpu),
            ),
            (
                "WebGPU desktop on Apple, -bc and -astc",
                caps(true, true, true),
                plan(Bc7, Gpu),
            ),
            (
                "WebGPU mobile, -astc",
                caps(false, true, true),
                plan(Astc4x4, Gpu),
            ),
            (
                "WebGL2 mobile, ASTC, no compute",
                caps(false, true, false),
                plan(Astc4x4, Cpu),
            ),
            (
                "WebGL2 desktop, the four BC extensions",
                caps(true, false, false),
                ImageryPlan::RGBA8,
            ),
            (
                "WebGL2 with BC and ASTC, no compute",
                caps(true, true, false),
                plan(Astc4x4, Cpu),
            ),
            (
                "compute, no block format",
                caps(false, false, true),
                ImageryPlan::RGBA8,
            ),
            ("nothing", GpuTextureCaps::default(), ImageryPlan::RGBA8),
        ];
        for (platform, caps, wanted) in matrix {
            assert_eq!(select(caps), wanted, "{platform}: {caps:?}");
        }
    }

    /// The plan names its encoder exactly when it names a block format.
    #[test]
    fn a_plan_encodes_exactly_when_it_compresses() {
        for bits in 0..8u8 {
            let p = select(caps(bits & 1 != 0, bits & 2 != 0, bits & 4 != 0));
            assert_eq!(
                p.target == ImageryFormat::Rgba8,
                p.encode == ImageryEncode::None,
                "{p:?}"
            );
        }
        assert_eq!(ImageryPlan::default(), ImageryPlan::RGBA8);
    }

    #[test]
    fn a_level_takes_whole_blocks() {
        assert_eq!(BlockFormat::level_bytes(256, 256), 64 * 64 * 16);
        assert_eq!(BlockFormat::level_bytes(2, 2), 16);
        assert_eq!(BlockFormat::level_bytes(1, 1), 16);
        assert_eq!(BlockFormat::level_bytes(6, 4), 32);
    }

    /// A 256² tile with its chain: 349,524 bytes held as RGBA8, 87,408 as
    /// blocks — a quarter, give or take the small levels' whole blocks.
    #[test]
    fn blocks_are_charged_a_quarter() {
        let rgba = Texels::rgba8(vec![0; 256 * 256 * 4]).cost(256, 256);
        assert_eq!(rgba.gpu, Bytes(349_524));
        for format in [BlockFormat::Bc7, BlockFormat::Astc4x4] {
            let levels = (0..9)
                .map(|l| vec![0; BlockFormat::level_bytes(256 >> l, 256 >> l)])
                .collect();
            let blocks = Texels::Blocks { format, levels };
            assert_eq!(blocks.byte_len(), 87_408, "{format:?}");
            let c = blocks.cost(256, 256);
            assert_eq!(c.gpu, Bytes(87_408), "{format:?}");
            assert!(c.staging < rgba.staging, "{format:?}: blocks cross smaller");
        }
    }

    /// Encoded on the GPU: held as blocks, but RGBA8 crosses.
    #[test]
    fn a_gpu_encoded_texture_is_held_small_and_moved_whole() {
        let rgba = Texels::rgba8(vec![0; 256 * 256 * 4]).cost(256, 256);
        for resident in [ImageryFormat::Bc7, ImageryFormat::Astc4x4] {
            let t = Texels::Rgba8 {
                pixels: vec![0; 256 * 256 * 4],
                resident,
            };
            assert_eq!(t.resident(), resident);
            let c = t.cost(256, 256);
            assert_eq!(c.gpu, Bytes(87_408), "{resident:?}");
            assert_eq!(c.staging, rgba.staging, "{resident:?}");
        }
    }

    #[test]
    fn byte_len_per_form() {
        assert_eq!(Texels::rgba8(vec![1; 64]).byte_len(), 64);
        let blocks = Texels::Blocks {
            format: BlockFormat::Astc4x4,
            levels: vec![vec![0; 64], vec![0; 16], vec![0; 16]],
        };
        assert_eq!(blocks.byte_len(), 96);
        assert_eq!(blocks.as_rgba8(), None);
        assert_eq!(blocks.resident(), ImageryFormat::Astc4x4);
    }
}
