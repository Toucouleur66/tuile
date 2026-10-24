// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Imagery stored already compressed: a KTX 2.0 tile holding UASTC, BC7 or
//! ASTC 4×4 blocks, with its mips, instead of a JPEG or PNG.
//!
//! **Transcode only.** Nothing here encodes. A UASTC tile is transcoded,
//! block by block, into the block format the consumer chose and its device
//! samples, or unpacked to RGBA8, bit-exact, when it did not opt in or its
//! device samples neither. What happens is the consumer's
//! [`StoredPlan`](crate::texels::StoredPlan), made from its own
//! [`ImageryCompression`](crate::texels::ImageryCompression); the table of
//! cases is there.
//!
//! A stored tile is recognised by its content — the twelve bytes every KTX
//! 2.0 file starts with — so a layer serving `.ktx2` needs no setting, and a
//! JPEG or PNG never reaches this module: [`raster::decode_imagery`]
//! hands those to [`raster::decode_and_reproject`] exactly as before.
//!
//! **Never a black tile.** Any bytes give a texture or an error, never a
//! panic: the file is read by texcomp's bounds-checked reader, and every
//! block is checked as it is transcoded or unpacked. An error leaves the tile
//! not resident, and the consumer's climb to the nearest resident ancestor
//! covers it, as for a failed fetch.
//!
//! [`raster::decode_imagery`]: crate::raster::decode_imagery
//! [`raster::decode_and_reproject`]: crate::raster::decode_and_reproject

use texcomp::ktx2::{self, Format};
use texcomp::uastc::{self, UastcBlock};

use crate::content::DecodedTexture;
use crate::raster::RasterError;
use crate::texels::{BlockFormat, StoredPlan, Texels};

/// The identifier every KTX 2.0 file starts with (KTX 2.0 §3.1).
pub const KTX2_IDENTIFIER: [u8; 12] = [
    0xAB, b'K', b'T', b'X', b' ', b'2', b'0', 0xBB, b'\r', b'\n', 0x1A, b'\n',
];

/// Whether `bytes` are a KTX 2.0 file, by their first twelve bytes.
pub fn is_ktx2(bytes: &[u8]) -> bool {
    bytes.starts_with(&KTX2_IDENTIFIER)
}

/// A stored tile to its texels, by `plan`.
///
/// `on_grid` says the tile's layer is on geographic spacing already, the
/// render grid: blocks cannot be resampled, so a tile is held as blocks only
/// then. Off the grid, UASTC is unpacked to RGBA8 for the caller to reproject
/// as it would a JPEG, and stored BC7 or ASTC is an error.
///
/// Blocking and I/O-free, like [`crate::raster::decode_image`].
pub fn decode(
    bytes: &[u8],
    on_grid: bool,
    plan: StoredPlan,
) -> Result<DecodedTexture, RasterError> {
    let tex = ktx2::read(bytes).map_err(|e| invalid(e.to_string()))?;
    let (width, height) = (tex.width, tex.height);
    match tex.format {
        Format::Uastc(_) => {
            let target = if on_grid {
                plan.for_uastc().target.block()
            } else {
                None
            };
            match target {
                Some(format) => {
                    let levels = tex
                        .levels
                        .iter()
                        .enumerate()
                        .map(|(level, src)| transcode(src, format, level))
                        .collect::<Result<Vec<_>, _>>()?;
                    Ok(DecodedTexture {
                        width,
                        height,
                        texels: Texels::Blocks { format, levels },
                    })
                }
                None => {
                    let level0 = tex
                        .levels
                        .first()
                        .ok_or_else(|| invalid("no level 0".into()))?;
                    let pixels = unpack(level0, width, height, tex.srgb)?;
                    Ok(DecodedTexture::from_rgba8(width, height, pixels))
                }
            }
        }
        Format::Bc7 => keep(tex.levels, BlockFormat::Bc7, width, height, on_grid, plan),
        Format::Astc4x4 => keep(
            tex.levels,
            BlockFormat::Astc4x4,
            width,
            height,
            on_grid,
            plan,
        ),
        other => Err(invalid(format!("a block format not read here: {other:?}"))),
    }
}

/// Stored BC7 or ASTC: held as it is, or refused — nothing here decodes
/// either on the CPU.
fn keep(
    levels: Vec<Vec<u8>>,
    format: BlockFormat,
    width: u32,
    height: u32,
    on_grid: bool,
    plan: StoredPlan,
) -> Result<DecodedTexture, RasterError> {
    if !on_grid {
        return Err(invalid(format!(
            "{format:?} blocks off the geographic grid cannot be reprojected"
        )));
    }
    if !plan.keeps(format) {
        return Err(invalid(format!(
            "{format:?} blocks, which this consumer does not hold and nothing here decodes"
        )));
    }
    Ok(DecodedTexture {
        width,
        height,
        texels: Texels::Blocks { format, levels },
    })
}

/// One level of UASTC blocks to `format`, block for block.
fn transcode(src: &[u8], format: BlockFormat, level: usize) -> Result<Vec<u8>, RasterError> {
    let mut out = vec![0; src.len()];
    match format {
        BlockFormat::Bc7 => uastc::transcode_to_bc7(src, &mut out),
        BlockFormat::Astc4x4 => uastc::transcode_to_astc_4x4(src, &mut out),
    }
    .map_err(|e| invalid(format!("level {level}: {e}")))?;
    Ok(out)
}

/// Level 0's UASTC blocks to tightly packed RGBA8, as ASTC's LDR decoder
/// computes them: the texels a GPU sampling the ASTC transcode would read.
/// A block straddling the right or bottom edge gives only its texels inside.
fn unpack(blocks: &[u8], width: u32, height: u32, srgb: bool) -> Result<Vec<u8>, RasterError> {
    let side = BlockFormat::BLOCK_SIDE as usize;
    let (w, h) = (width as usize, height as usize);
    let across = w.div_ceil(side);
    let (blocks, rest) = blocks.as_chunks::<{ uastc::BLOCK_BYTES }>();
    if !rest.is_empty() || blocks.len() != across * h.div_ceil(side) {
        return Err(invalid(format!(
            "{} blocks of level 0 for a {width}×{height} texture",
            blocks.len()
        )));
    }
    let mut pixels = vec![0; w * h * 4];
    for (index, block) in blocks.iter().enumerate() {
        let texels = UastcBlock::from_bytes(*block)
            .decode_rgba8(srgb)
            .map_err(|e| invalid(format!("block {index}: {e}")))?;
        let (bx, by) = (index % across * side, index / across * side);
        for (t, texel) in texels.iter().enumerate() {
            let (x, y) = (bx + t % side, by + t / side);
            if x < w && y < h {
                let at = (y * w + x) * 4;
                pixels[at..at + 4].copy_from_slice(texel);
            }
        }
    }
    Ok(pixels)
}

fn invalid(why: String) -> RasterError {
    RasterError::Image(format!("KTX2: {why}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::cost::{drape_cost, TextureFormat};
    use crate::raster::{self, ImageryCoord, TilingScheme};
    use crate::texels::{GpuTextureCaps, ImageryCompression, ImageryFormat};
    use texcomp::ktx2::{Texture, UastcChannels};

    /// texcomp's reference tile: 64², its UASTC blocks and upstream's
    /// transcodes and unpack digests (`tests/fixtures/uastc/README.md`).
    const SIDE: u32 = 64;
    const UASTC: &[u8] = include_bytes!("../tests/fixtures/uastc/alps-0-crop.uastc");
    const ASTC: &[u8] = include_bytes!("../tests/fixtures/uastc/alps-0-crop.astc");
    const BC7: &[u8] = include_bytes!("../tests/fixtures/uastc/alps-0-crop.bc7");
    const DECODED: &[u8] = include_bytes!("../tests/fixtures/uastc/alps-0-crop.decoded");
    const PNG: &[u8] = include_bytes!("../tests/fixtures/uastc/alps-0-crop.png");

    const APPLE: GpuTextureCaps = GpuTextureCaps {
        bc: true,
        astc: true,
        compute: true,
    };
    const PHONE: GpuTextureCaps = GpuTextureCaps {
        bc: false,
        astc: true,
        compute: true,
    };

    fn ktx2_file(format: Format, srgb: bool, levels: Vec<Vec<u8>>) -> Vec<u8> {
        ktx2::write(&Texture {
            format,
            srgb,
            width: SIDE,
            height: SIDE,
            levels,
        })
        .expect("a valid texture")
    }

    fn uastc_file(srgb: bool) -> Vec<u8> {
        ktx2_file(
            Format::Uastc(UastcChannels::Rgb),
            srgb,
            vec![UASTC.to_vec()],
        )
    }

    /// A full chain, 64² to 1², each level the first blocks of the reference
    /// tile: real UASTC blocks, and each level's reference transcode is the
    /// same prefix of upstream's.
    fn chain(of: &[u8]) -> Vec<Vec<u8>> {
        (0..7)
            .map(|l| of[..BlockFormat::level_bytes(SIDE >> l, SIDE >> l)].to_vec())
            .collect()
    }

    fn blocks(t: &DecodedTexture) -> (BlockFormat, &[Vec<u8>]) {
        match &t.texels {
            Texels::Blocks { format, levels } => (*format, levels),
            Texels::Rgba8 { .. } => panic!("RGBA8, not blocks"),
        }
    }

    fn fnv1a(bytes: &[u8]) -> u32 {
        bytes.iter().fold(0x811c_9dc5, |h, b| {
            (h ^ u32::from(*b)).wrapping_mul(0x0100_0193)
        })
    }

    /// Transcode only: a client that chose BC7 or ASTC, on a device that
    /// samples it, gets upstream's transcode of every block, byte for byte.
    #[test]
    fn uastc_transcodes_to_the_reference_blocks() {
        for (choice, caps, format, reference) in [
            (ImageryCompression::Auto, APPLE, BlockFormat::Bc7, BC7),
            (
                ImageryCompression::Format(BlockFormat::Bc7),
                APPLE,
                BlockFormat::Bc7,
                BC7,
            ),
            (ImageryCompression::Auto, PHONE, BlockFormat::Astc4x4, ASTC),
            (
                ImageryCompression::Format(BlockFormat::Astc4x4),
                APPLE,
                BlockFormat::Astc4x4,
                ASTC,
            ),
        ] {
            let t = decode(&uastc_file(true), true, choice.stored(caps)).expect("transcodes");
            assert_eq!((t.width, t.height), (SIDE, SIDE));
            let (got, levels) = blocks(&t);
            assert_eq!(got, format, "{choice:?} on {caps:?}");
            assert_eq!(levels.len(), 1);
            assert!(levels[0] == reference, "{choice:?}: not upstream's blocks");
        }
    }

    /// Every level of the file is transcoded: the chain comes from the file,
    /// none is built, and the budget charges the blocks held.
    #[test]
    fn the_whole_chain_is_transcoded_and_charged_as_blocks() {
        let file = ktx2_file(Format::Uastc(UastcChannels::Rgb), true, chain(UASTC));
        let t = decode(&file, true, ImageryCompression::Auto.stored(PHONE)).expect("transcodes");
        let (format, levels) = blocks(&t);
        assert_eq!(format, BlockFormat::Astc4x4);
        assert_eq!(levels, chain(ASTC).as_slice());
        let held = drape_cost(SIDE, SIDE, TextureFormat::Block16);
        assert_eq!(t.cost().gpu, held.gpu);
        assert_eq!(t.texels.byte_len() as u64, held.gpu.0);
        assert!(
            t.cost().gpu
                < DecodedTexture::from_rgba8(SIDE, SIDE, vec![0; 64 * 64 * 4])
                    .cost()
                    .gpu
        );
    }

    /// Not opted in, or nothing to transcode to: level 0 unpacked to RGBA8,
    /// each block's texels upstream's to the bit, linear and sRGB alike.
    #[test]
    fn uastc_unpacks_bit_exact_when_not_opted_in() {
        let nothing = GpuTextureCaps::default();
        for (plan, srgb) in [
            (StoredPlan::OFF, false),
            (StoredPlan::OFF, true),
            (ImageryCompression::Off.stored(APPLE), true),
            (ImageryCompression::Auto.stored(nothing), true),
            (
                ImageryCompression::Format(BlockFormat::Bc7).stored(PHONE),
                true,
            ),
        ] {
            let t = decode(&uastc_file(srgb), true, plan).expect("unpacks");
            let pixels = t.rgba8().expect("RGBA8");
            assert_eq!(t.resident_format(), ImageryFormat::Rgba8);
            assert_eq!(pixels.len(), 64 * 64 * 4);
            let digests = DECODED.as_chunks::<4>().0;
            let across = (SIDE / 4) as usize;
            for b in 0..across * across {
                let (bx, by) = (b % across * 4, b / across * 4);
                let mut block = Vec::with_capacity(64);
                for y in by..by + 4 {
                    let at = (y * 64 + bx) * 4;
                    block.extend_from_slice(&pixels[at..at + 16]);
                }
                let want = u32::from_le_bytes(digests[b * 2 + usize::from(srgb)]);
                assert_eq!(fnv1a(&block), want, "block {b}, srgb {srgb}, {plan:?}");
            }
        }
    }

    /// Blocks cannot be resampled: on a grid that is not the render grid, the
    /// tile is unpacked and reprojected exactly as a decoded JPEG would be,
    /// whatever the client chose.
    #[test]
    fn off_the_render_grid_the_tile_takes_the_rgba8_path() {
        let scheme = TilingScheme::web_mercator();
        let coord = ImageryCoord {
            level: 2,
            x: 1,
            y: 1,
        };
        let file = uastc_file(true);
        let unpacked = decode(&file, false, StoredPlan::OFF).expect("unpacks");
        let wanted = raster::reproject_tile_to_geographic(&unpacked, &scheme, coord)
            .expect("a level-2 Mercator tile is resampled");
        for plan in [StoredPlan::OFF, ImageryCompression::Auto.stored(APPLE)] {
            let t = raster::decode_imagery(&file, &scheme, coord, plan).expect("decodes");
            assert_eq!(t.rgba8(), wanted.rgba8(), "{plan:?}");
            assert_eq!(t.resident_format(), ImageryFormat::Rgba8);
        }
        // On the grid, the same choice keeps blocks.
        let t = raster::decode_imagery(
            &file,
            &TilingScheme::geographic(),
            coord,
            ImageryCompression::Auto.stored(APPLE),
        )
        .expect("transcodes");
        assert_eq!(blocks(&t).0, BlockFormat::Bc7);
    }

    /// A JPEG or PNG never reaches this module: whatever the plan, it is
    /// decoded and resampled exactly as before.
    #[test]
    fn a_png_is_decoded_as_before_whatever_the_choice() {
        let coord = ImageryCoord {
            level: 2,
            x: 1,
            y: 1,
        };
        for scheme in [TilingScheme::geographic(), TilingScheme::web_mercator()] {
            let before = raster::decode_and_reproject(PNG, &scheme, coord).expect("decodes");
            for plan in [StoredPlan::OFF, ImageryCompression::Auto.stored(APPLE)] {
                let t = raster::decode_imagery(PNG, &scheme, coord, plan).expect("decodes");
                assert_eq!((t.width, t.height), (before.width, before.height));
                assert_eq!(t.texels, before.texels, "{plan:?}");
            }
        }
        assert!(!is_ktx2(PNG));
        assert!(is_ktx2(&uastc_file(false)));
    }

    /// Stored BC7 or ASTC is held as it is where the client holds that
    /// format, and refused elsewhere: nothing here decodes it, so the tile
    /// is an absence the ancestor covers, never a black texture.
    #[test]
    fn stored_blocks_are_kept_or_refused() {
        let bc7 = ktx2_file(Format::Bc7, true, chain(BC7));
        let t = decode(&bc7, true, ImageryCompression::Auto.stored(APPLE)).expect("kept");
        assert_eq!(blocks(&t), (BlockFormat::Bc7, chain(BC7).as_slice()));
        assert!(decode(&bc7, true, StoredPlan::OFF).is_err(), "not opted in");
        assert!(
            decode(&bc7, true, ImageryCompression::Auto.stored(PHONE)).is_err(),
            "not sampled"
        );
        let off_grid = decode(&bc7, false, ImageryCompression::Auto.stored(APPLE));
        assert!(off_grid.is_err(), "off the grid");
        let astc = ktx2_file(Format::Astc4x4, true, vec![ASTC.to_vec()]);
        let t = decode(&astc, true, ImageryCompression::Auto.stored(PHONE)).expect("kept");
        assert_eq!(blocks(&t).0, BlockFormat::Astc4x4);
    }

    /// Corrupt files and blocks are errors, never a panic and never a
    /// texture: every truncation, a flipped byte anywhere, and a block whose
    /// mode is reserved.
    #[test]
    fn a_corrupt_file_is_an_error_and_never_a_panic() {
        let file = ktx2_file(Format::Uastc(UastcChannels::Rgb), true, chain(UASTC));
        let plans = [StoredPlan::OFF, ImageryCompression::Auto.stored(APPLE)];
        // Every byte of the header, index, level index and descriptor, then
        // a stride through the levels: the same ground in a debug build's
        // time.
        let probes = || (0..file.len().min(320)).chain((320..file.len()).step_by(41));
        for len in probes() {
            for plan in plans {
                assert!(decode(&file[..len], true, plan).is_err(), "cut at {len}");
            }
        }
        for at in probes() {
            let mut bad = file.clone();
            bad[at] ^= 0xA5;
            for plan in plans {
                // Some flips land in texel data and still read: any answer
                // but a panic will do.
                let _ = decode(&bad, true, plan);
            }
        }
        let reserved = (0..=255u8)
            .map(|b| {
                let mut block = [0u8; 16];
                block[0] = b;
                block
            })
            .find(|block| UastcBlock::from_bytes(*block).mode().is_err())
            .expect("UASTC reserves a mode code");
        let mut level = UASTC.to_vec();
        level[16 * 37..16 * 38].copy_from_slice(&reserved);
        let file = ktx2_file(Format::Uastc(UastcChannels::Rgb), true, vec![level]);
        for plan in plans {
            let e = decode(&file, true, plan).expect_err("a reserved block");
            assert!(e.to_string().contains("block 37"), "{e}");
        }
    }
}
