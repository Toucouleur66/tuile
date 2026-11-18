// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Imagery tiles that are not pictures of the ground.
//!
//! A source with nothing to show at a place does not always say so: it may
//! answer with a valid image instead — a uniform black square, a provider's
//! "no imagery" picture. Stored, such a tile becomes the finest one there is
//! wherever the source has no finer level (at sea, typically), and a film is
//! drawn with black water or with the provider's icon tiled across the bay.
//! Such a tile is an absence, and is to be kept and read as one: the ground
//! then stands on the coarser tile above it, never on nothing.
//!
//! The generic half of the recognition is here: a **uniform black** tile.
//! What a given provider answers with when it has nothing — a picture known
//! by its exact bytes — is its connector's knowledge, not this crate's.
//!
//! # Cost
//!
//! The check decodes only what can be uniform. A uniform image compresses to
//! almost nothing beyond its headers: a 256 × 256 black JPEG is 1.6 to
//! 2.4 KiB whatever its quality, a PNG a few hundred bytes.
//! [`could_be_uniform`] reads the image's size from its header (no pixel
//! decoded, some 10 µs) and lets through only a body small enough for that
//! size. Those, and only those, are decoded (some 300 µs for 256 × 256).
//!
//! Measured on Bing's aerial layer around Monaco and the Riviera: 1.6 % of a
//! million tiles are that small — but on a stretch of sea, 42 % of the tiles
//! of the zone (5 141 of 12 347): flat water compresses as well as black.
//! Affordable where a tile is about to be written, after a fetch; not on
//! every read of what a store holds — which is why a reader of a store looks
//! for the provider's known pictures by their digest instead, and a reader
//! that decodes anyway looks at the texels it decoded
//! ([`is_black_rgba8`]).

/// The brightest a channel of a texel may be and the texel still be black:
/// a decoder may round a pure-black JPEG's texels to 1 or 2.
pub const BLACK_LEVEL: u8 = 3;

/// The bytes a uniform image may take beyond its pixels' share: headers,
/// quantisation and Huffman tables, a colour profile.
const HEADER_ALLOWANCE: usize = 2048;

/// The bytes a uniform image takes per pixel at most, beyond its headers:
/// one 8 × 8 block of a uniform JPEG costs two bytes when its chroma is not
/// subsampled, so a pixel 1/32 of a byte.
const PIXELS_PER_BYTE: usize = 32;

/// FNV-1a, 64 bits, over a tile's bytes.
///
/// The digest a store's index keeps of each tile and a pack's reference to a
/// source tile carries — the same function, so a known tile can be named by
/// its digest wherever only the digest is at hand.
pub fn digest(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// Whether `bytes` start as a PNG does.
pub fn is_png(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
}

/// Whether `bytes` start as a JPEG does.
pub fn is_jpeg(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xff, 0xd8, 0xff])
}

/// Whether every texel of an RGBA8 buffer is black: each of red, green and
/// blue at most [`BLACK_LEVEL`], whatever its alpha. An empty buffer is not.
///
/// Stops at the first texel that is not black, so a real picture costs a
/// texel or two.
pub fn is_black_rgba8(rgba8: &[u8]) -> bool {
    let (texels, _) = rgba8.as_chunks::<4>();
    !texels.is_empty()
        && texels
            .iter()
            .all(|t| t[0] <= BLACK_LEVEL && t[1] <= BLACK_LEVEL && t[2] <= BLACK_LEVEL)
}

/// The image's size, if its header says it and the body is small enough for
/// that size to hold one colour only; `None` otherwise. Nothing is decoded.
pub fn could_be_uniform(bytes: &[u8]) -> Option<(u32, u32)> {
    if !(is_jpeg(bytes) || is_png(bytes)) {
        return None;
    }
    let (width, height) = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    let pixels = (width as usize).saturating_mul(height as usize);
    (pixels > 0 && bytes.len() <= HEADER_ALLOWANCE + pixels / PIXELS_PER_BYTE)
        .then_some((width, height))
}

/// Whether `bytes` are an image of one colour, black: every texel black as
/// [`is_black_rgba8`] says.
///
/// Only a body [`could_be_uniform`] lets through is decoded; anything else —
/// a real tile, bytes that are not a JPEG or a PNG, an image that does not
/// decode — is not black.
pub fn is_uniform_black(bytes: &[u8]) -> bool {
    if could_be_uniform(bytes).is_none() {
        return false;
    }
    let black = |v: &u8| *v <= BLACK_LEVEL;
    match image::load_from_memory(bytes) {
        // As decoded, without a copy into RGBA: a JPEG is RGB or grey.
        Ok(image::DynamicImage::ImageRgb8(rgb)) => {
            !rgb.as_raw().is_empty() && rgb.as_raw().iter().all(black)
        }
        Ok(image::DynamicImage::ImageLuma8(grey)) => {
            !grey.as_raw().is_empty() && grey.as_raw().iter().all(black)
        }
        Ok(picture) => is_black_rgba8(picture.to_rgba8().as_raw()),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jpeg(side: u32, rgb: [u8; 3], noise: bool) -> Vec<u8> {
        let picture = image::RgbImage::from_fn(side, side, |x, y| {
            if noise {
                let n = ((x * 7 + y * 13) % 9) as u8;
                image::Rgb([rgb[0] + n, rgb[1] + n, rgb[2] + n])
            } else {
                image::Rgb(rgb)
            }
        });
        let mut out = std::io::Cursor::new(Vec::new());
        picture
            .write_to(&mut out, image::ImageFormat::Jpeg)
            .expect("a jpeg");
        out.into_inner()
    }

    fn png(side: u32, rgba: [u8; 4]) -> Vec<u8> {
        let picture = image::RgbaImage::from_pixel(side, side, image::Rgba(rgba));
        let mut out = std::io::Cursor::new(Vec::new());
        picture
            .write_to(&mut out, image::ImageFormat::Png)
            .expect("a png");
        out.into_inner()
    }

    #[test]
    fn the_digest_is_fnv_1a() {
        assert_eq!(digest(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(digest(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(digest(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn a_uniform_black_tile_is_black_at_every_size_and_format() {
        for side in [256, 512] {
            assert!(
                is_uniform_black(&jpeg(side, [0, 0, 0], false)),
                "{side} jpeg"
            );
            assert!(is_uniform_black(&png(side, [0, 0, 0, 255])), "{side} png");
        }
    }

    #[test]
    fn dark_ground_is_not_black() {
        // Night-dark or deep water, but a picture: one texel past the level
        // is enough.
        assert!(!is_uniform_black(&jpeg(256, [0, 0, 0], true)));
        assert!(!is_uniform_black(&jpeg(256, [8, 10, 12], false)));
        // Uniform, but not black: a flat sea is a picture of the sea.
        assert!(!is_uniform_black(&jpeg(256, [20, 60, 110], false)));
        assert!(!is_uniform_black(&png(256, [255, 255, 255, 255])));
    }

    #[test]
    fn what_is_not_an_image_is_not_black() {
        assert!(!is_uniform_black(b""));
        assert!(!is_uniform_black(b"\xff\xd8\xff a photograph"));
        assert!(!is_uniform_black(b"\x89PNG\r\n\x1a\n and nothing"));
        assert!(!is_black_rgba8(&[]));
    }

    #[test]
    fn only_a_small_body_is_decoded() {
        // A picture of the ground does not compress to its headers.
        let mut real = Vec::new();
        let picture = image::RgbImage::from_fn(256, 256, |x, y| {
            image::Rgb([(x ^ y) as u8, (x * 3) as u8, (y * 5) as u8])
        });
        picture
            .write_to(
                &mut std::io::Cursor::new(&mut real),
                image::ImageFormat::Jpeg,
            )
            .expect("a jpeg");
        assert_eq!(could_be_uniform(&real), None, "{} bytes", real.len());
        assert_eq!(
            could_be_uniform(&jpeg(256, [0, 0, 0], false)),
            Some((256, 256))
        );
    }

    #[test]
    fn black_is_every_texel_up_to_the_level() {
        assert!(is_black_rgba8(&[0, 0, 0, 255, 3, 3, 3, 0]));
        assert!(!is_black_rgba8(&[0, 0, 0, 255, 0, 4, 0, 255]));
    }
}
