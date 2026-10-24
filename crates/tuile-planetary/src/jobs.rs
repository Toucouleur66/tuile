// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The loader's CPU work, stated as data so it can leave the thread that
//! polls the server — and the worker, where a thread is a Web Worker that
//! shares no memory.
//!
//! Two jobs, and they are the whole of what a load does that is both heavy and
//! pure, bytes in and buffers out:
//!
//! - [`ImageryDecode`]: a JPEG or PNG to RGBA8, resampled onto geographic
//!   spacing when the provider's grid is not, and the opaque-black check. By
//!   far the largest: a drape binds up to a dozen imagery tiles, each a full
//!   image decode and a per-texel resample. A tile stored already compressed
//!   (KTX 2.0) is transcoded or unpacked instead, by the consumer's
//!   [`StoredPlan`].
//! - [`TerrainDecode`]: a quantized-mesh tile to its vertices, indices,
//!   normals, edges and the availability it reveals.
//!
//! What stays on the loader's thread is what reads or writes its state — the
//! caches, the shared availability, the drape's composition — and the mesh
//! built from a decoded tile, which needs the tile's rectangle and is small
//! beside the two above.
//!
//! Each job implements [`Portable`]: natively it runs as a closure on the
//! host's pool and is never encoded; in a browser it is written to bytes,
//! posted to a decode worker, run there by [`serve`], and read back.

use bytes::Bytes;
use tuile_core::content::DecodedTexture;
use tuile_core::offload::{self, Portable};
use tuile_core::raster::{self, ImageryCoord, Projection, TilingScheme};
use tuile_core::texels::{BlockFormat, ImageryFormat, StoredPlan, Texels};
use tuile_terrain::{AvailabilityRange, Header, QuantizedMesh};

/// Runs the job of this kind from its bytes: the whole registry a decode
/// worker needs. An unknown kind is a host built from another version of this
/// crate, and is said so rather than guessed at.
pub fn serve(kind: &str, input: Vec<u8>) -> Result<Vec<u8>, String> {
    match kind {
        ImageryDecode::KIND => offload::serve::<ImageryDecode>(input),
        TerrainDecode::KIND => offload::serve::<TerrainDecode>(input),
        other => Err(format!("no decode job is called {other:?}")),
    }
}

/// One imagery tile's bytes, to the texture drawn from it.
#[derive(Debug, Clone)]
pub struct ImageryDecode {
    /// The provider's encoded image.
    pub bytes: Bytes,
    /// The provider's grid, which says whether and how to resample.
    pub scheme: TilingScheme,
    /// The tile, which places the resampling.
    pub coord: ImageryCoord,
    /// What the consumer does with a tile stored already compressed: its own
    /// choice, [`StoredPlan::OFF`] unless it opted in. A JPEG or PNG ignores
    /// it.
    pub stored: StoredPlan,
}

/// The texture, and whether it is entirely opaque black — a provider's way
/// of saying "no data", named where it is decoded. The error is the decoder's,
/// in its words.
pub type ImageryDecoded = Result<(DecodedTexture, bool), String>;

/// The scheme, the coord and the stored plan, after the image: 29 + 20 + 3
/// bytes.
const IMAGERY_TRAILER: usize = 1 + 8 + 8 + 4 + 4 + 4 + 4 + 8 + 8 + 3;

/// The form of an imagery answer's texels, before its outcome byte.
const FORM_RGBA8: u8 = 0;
const FORM_BC7: u8 = 1;
const FORM_ASTC: u8 = 2;

/// An imagery answer's numbers, after its texels: width, height, the black
/// flag, the form and the level count.
const ANSWER_TRAILER: usize = 4 + 4 + 1 + 1 + 1;

fn format_tag(f: ImageryFormat) -> u8 {
    match f {
        ImageryFormat::Rgba8 => FORM_RGBA8,
        ImageryFormat::Bc7 => FORM_BC7,
        ImageryFormat::Astc4x4 => FORM_ASTC,
    }
}

fn tag_format(tag: u8) -> Result<ImageryFormat, String> {
    match tag {
        FORM_RGBA8 => Ok(ImageryFormat::Rgba8),
        FORM_BC7 => Ok(ImageryFormat::Bc7),
        FORM_ASTC => Ok(ImageryFormat::Astc4x4),
        other => Err(format!("no texel form is numbered {other}")),
    }
}

impl Portable for ImageryDecode {
    const KIND: &'static str = "imagery";
    type Output = ImageryDecoded;

    fn run(self) -> ImageryDecoded {
        let tex = raster::decode_imagery(&self.bytes, &self.scheme, self.coord, self.stored)
            .map_err(|e| e.to_string())?;
        let black = crate::is_opaque_black(&tex);
        Ok((tex, black))
    }

    /// The image first and the numbers after, so the far side reads them off
    /// the end and keeps the image where it lies: no copy of the bulk.
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.bytes.len() + IMAGERY_TRAILER);
        out.extend_from_slice(&self.bytes);
        let s = &self.scheme;
        out.push(match s.projection {
            Projection::WebMercator => 0,
            Projection::Geographic => 1,
        });
        out.extend_from_slice(&s.root_tiles_x.to_le_bytes());
        out.extend_from_slice(&s.root_tiles_y.to_le_bytes());
        out.extend_from_slice(&s.tile_size.to_le_bytes());
        out.extend_from_slice(&s.minimum_level.to_le_bytes());
        out.extend_from_slice(&s.maximum_level.to_le_bytes());
        out.extend_from_slice(&self.coord.level.to_le_bytes());
        out.extend_from_slice(&self.coord.x.to_le_bytes());
        out.extend_from_slice(&self.coord.y.to_le_bytes());
        out.push(format_tag(self.stored.uastc));
        out.push(u8::from(self.stored.keep_bc7));
        out.push(u8::from(self.stored.keep_astc));
        out
    }

    fn decode(mut bytes: Vec<u8>) -> Result<Self, String> {
        let start = bytes
            .len()
            .checked_sub(IMAGERY_TRAILER)
            .ok_or("an imagery job shorter than its own numbers")?;
        let mut r = Reader::new(&bytes[start..]);
        let projection = match r.u8()? {
            0 => Projection::WebMercator,
            1 => Projection::Geographic,
            other => return Err(format!("no projection is numbered {other}")),
        };
        let scheme = TilingScheme {
            projection,
            root_tiles_x: r.u64()?,
            root_tiles_y: r.u64()?,
            tile_size: r.u32()?,
            minimum_level: r.u32()?,
            maximum_level: r.u32()?,
        };
        let coord = ImageryCoord {
            level: r.u32()?,
            x: r.u64()?,
            y: r.u64()?,
        };
        let stored = StoredPlan {
            uastc: tag_format(r.u8()?)?,
            keep_bc7: r.u8()? != 0,
            keep_astc: r.u8()? != 0,
        };
        bytes.truncate(start);
        Ok(Self {
            bytes: Bytes::from(bytes),
            scheme,
            coord,
            stored,
        })
    }

    /// The texels first and the numbers after, for the same reason as the
    /// job's: the answer is a quarter of a megabyte, its header a few bytes.
    ///
    /// RGBA8 crosses as its pixels; blocks as their levels end to end, each
    /// level's length following from the size and the format.
    fn encode_output(output: ImageryDecoded) -> Vec<u8> {
        match output {
            Ok((tex, black)) => {
                let (width, height) = (tex.width, tex.height);
                let (mut out, form, levels) = match tex.texels {
                    Texels::Rgba8 { pixels, .. } => (pixels, FORM_RGBA8, 1),
                    Texels::Blocks { format, levels } => {
                        let Ok(count) = u8::try_from(levels.len()) else {
                            return failed(format!("{} levels of blocks", levels.len()));
                        };
                        (levels.concat(), format_tag(format.into()), count)
                    }
                };
                out.extend_from_slice(&width.to_le_bytes());
                out.extend_from_slice(&height.to_le_bytes());
                out.push(u8::from(black));
                out.push(form);
                out.push(levels);
                out.push(OK);
                out
            }
            Err(why) => failed(why),
        }
    }

    fn decode_output(mut bytes: Vec<u8>) -> Result<ImageryDecoded, String> {
        match bytes.pop() {
            Some(OK) => {}
            Some(FAILED) => return Ok(Err(utf8(bytes)?)),
            _ => return Err("an imagery answer with no outcome".into()),
        }
        let start = bytes
            .len()
            .checked_sub(ANSWER_TRAILER)
            .ok_or("an imagery answer shorter than its own numbers")?;
        let mut r = Reader::new(&bytes[start..]);
        let (width, height, black) = (r.u32()?, r.u32()?, r.u8()? != 0);
        let (form, count) = (tag_format(r.u8()?)?, r.u8()?);
        bytes.truncate(start);
        let Some(format) = form.block() else {
            if bytes.len() != width as usize * height as usize * 4 {
                return Err(format!(
                    "{} bytes of pixels for a {width}×{height} texture",
                    bytes.len()
                ));
            }
            return Ok(Ok((
                DecodedTexture::from_rgba8(width, height, bytes),
                black,
            )));
        };
        let mut levels = Vec::with_capacity(usize::from(count));
        let mut rest = bytes.as_slice();
        for level in 0..u32::from(count) {
            let len = BlockFormat::level_bytes(
                width.checked_shr(level).unwrap_or(0).max(1),
                height.checked_shr(level).unwrap_or(0).max(1),
            );
            if rest.len() < len {
                return Err(format!(
                    "level {level} of a {width}×{height} texture cut short"
                ));
            }
            let (head, tail) = rest.split_at(len);
            levels.push(head.to_vec());
            rest = tail;
        }
        if !rest.is_empty() || levels.is_empty() {
            return Err(format!(
                "{} bytes of blocks that are not {count} levels of a {width}×{height} texture",
                bytes.len()
            ));
        }
        Ok(Ok((
            DecodedTexture {
                width,
                height,
                texels: Texels::Blocks { format, levels },
            },
            black,
        )))
    }
}

/// One terrain tile's quantized-mesh bytes, to the mesh.
#[derive(Debug, Clone)]
pub struct TerrainDecode {
    /// The tile as the source served it, already out of any transfer encoding.
    pub bytes: Vec<u8>,
}

/// The mesh, or the decoder's reason in its words.
pub type TerrainDecoded = Result<QuantizedMesh, String>;

impl Portable for TerrainDecode {
    const KIND: &'static str = "terrain";
    type Output = TerrainDecoded;

    fn run(self) -> TerrainDecoded {
        tuile_terrain::decode(&self.bytes).map_err(|e| e.to_string())
    }

    fn encode(&self) -> Vec<u8> {
        self.bytes.clone()
    }

    fn decode(bytes: Vec<u8>) -> Result<Self, String> {
        Ok(Self { bytes })
    }

    fn encode_output(output: TerrainDecoded) -> Vec<u8> {
        let mesh = match output {
            Ok(mesh) => mesh,
            Err(why) => return failed(why),
        };
        let n = mesh.vertex_count();
        let mut w = Writer(Vec::with_capacity(
            128 + n * 3 * 8 + mesh.indices.len() * 4 + n * 12,
        ));
        let h = &mesh.header;
        w.f64s(&h.center);
        w.f32(h.min_height);
        w.f32(h.max_height);
        w.f64s(&h.bounding_sphere_center);
        w.f64(h.bounding_sphere_radius);
        w.f64s(&h.horizon_occlusion);
        w.len(n);
        w.f64s(&mesh.u);
        w.f64s(&mesh.v);
        w.f64s(&mesh.height);
        w.u32s(&mesh.indices);
        match &mesh.normals {
            Some(normals) => {
                w.0.push(1);
                w.len(normals.len());
                for normal in normals {
                    for c in normal {
                        w.f32(*c);
                    }
                }
            }
            None => w.0.push(0),
        }
        for edge in &mesh.edges {
            w.u32s(edge);
        }
        match &mesh.metadata_available {
            Some(levels) => {
                w.0.push(1);
                w.len(levels.len());
                for ranges in levels {
                    w.len(ranges.len());
                    for range in ranges {
                        for v in [range.start_x, range.start_y, range.end_x, range.end_y] {
                            w.0.extend_from_slice(&v.to_le_bytes());
                        }
                    }
                }
            }
            None => w.0.push(0),
        }
        w.0.extend_from_slice(&mesh.cut.to_le_bytes());
        w.0.push(OK);
        w.0
    }

    fn decode_output(mut bytes: Vec<u8>) -> Result<TerrainDecoded, String> {
        match bytes.pop() {
            Some(OK) => {}
            Some(FAILED) => return Ok(Err(utf8(bytes)?)),
            _ => return Err("a terrain answer with no outcome".into()),
        }
        let mut r = Reader::new(&bytes);
        let header = Header {
            center: r.f64x3()?,
            min_height: r.f32()?,
            max_height: r.f32()?,
            bounding_sphere_center: r.f64x3()?,
            bounding_sphere_radius: r.f64()?,
            horizon_occlusion: r.f64x3()?,
        };
        let n = r.len()?;
        let u = r.f64s(n)?;
        let v = r.f64s(n)?;
        let height = r.f64s(n)?;
        let count = r.len()?;
        let indices = r.u32s(count)?;
        let normals = match r.u8()? {
            0 => None,
            _ => {
                let count = r.len()?;
                let mut normals = Vec::with_capacity(count);
                for _ in 0..count {
                    normals.push([r.f32()?, r.f32()?, r.f32()?]);
                }
                Some(normals)
            }
        };
        let mut edges: [Vec<u32>; 4] = Default::default();
        for edge in &mut edges {
            let count = r.len()?;
            *edge = r.u32s(count)?;
        }
        let metadata_available = match r.u8()? {
            0 => None,
            _ => {
                let levels = r.len()?;
                let mut out = Vec::with_capacity(levels);
                for _ in 0..levels {
                    let count = r.len()?;
                    let mut ranges = Vec::with_capacity(count);
                    for _ in 0..count {
                        ranges.push(AvailabilityRange {
                            start_x: r.u64()?,
                            start_y: r.u64()?,
                            end_x: r.u64()?,
                            end_y: r.u64()?,
                        });
                    }
                    out.push(ranges);
                }
                Some(out)
            }
        };
        let cut = r.u32()?;
        if !r.rest().is_empty() {
            return Err(format!("{} bytes after a terrain answer", r.rest().len()));
        }
        Ok(Ok(QuantizedMesh {
            header,
            u,
            v,
            height,
            indices,
            normals,
            edges,
            metadata_available,
            cut,
        }))
    }
}

/// The last byte of an answer: the job's own outcome.
const OK: u8 = 1;
const FAILED: u8 = 0;

/// A job's own failure, as an answer: the reason, then [`FAILED`].
fn failed(why: String) -> Vec<u8> {
    let mut out = why.into_bytes();
    out.push(FAILED);
    out
}

fn utf8(bytes: Vec<u8>) -> Result<String, String> {
    String::from_utf8(bytes).map_err(|_| "a reason that is not text".to_owned())
}

/// Little-endian, front to back.
struct Writer(Vec<u8>);

impl Writer {
    fn len(&mut self, n: usize) {
        self.0.extend_from_slice(&(n as u64).to_le_bytes());
    }
    fn f32(&mut self, v: f32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn f64(&mut self, v: f64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn f64s(&mut self, vs: &[f64]) {
        for v in vs {
            self.f64(*v);
        }
    }
    fn u32s(&mut self, vs: &[u32]) {
        self.len(vs.len());
        for v in vs {
            self.0.extend_from_slice(&v.to_le_bytes());
        }
    }
}

/// [`Writer`]'s mirror, refusing to read past the end.
struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }
    fn take<const N: usize>(&mut self) -> Result<[u8; N], String> {
        if self.bytes.len() < N {
            return Err("a decode answer cut short".into());
        }
        let (head, rest) = self.bytes.split_at(N);
        self.bytes = rest;
        Ok(head.try_into().expect("N bytes"))
    }
    fn rest(&self) -> &'a [u8] {
        self.bytes
    }
    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take::<1>()?[0])
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take()?))
    }
    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.take()?))
    }
    fn f32(&mut self) -> Result<f32, String> {
        Ok(f32::from_le_bytes(self.take()?))
    }
    fn f64(&mut self) -> Result<f64, String> {
        Ok(f64::from_le_bytes(self.take()?))
    }
    fn f64x3(&mut self) -> Result<[f64; 3], String> {
        Ok([self.f64()?, self.f64()?, self.f64()?])
    }
    /// A count, checked against what is left so a corrupt one cannot ask for
    /// an allocation the size of the address space.
    fn len(&mut self) -> Result<usize, String> {
        let n = usize::try_from(self.u64()?).map_err(|_| "a count too large")?;
        if n > self.bytes.len() {
            return Err(format!(
                "a count of {n} with {} bytes left",
                self.bytes.len()
            ));
        }
        Ok(n)
    }
    fn f64s(&mut self, n: usize) -> Result<Vec<f64>, String> {
        (0..n).map(|_| self.f64()).collect()
    }
    fn u32s(&mut self, n: usize) -> Result<Vec<u32>, String> {
        (0..n).map(|_| self.u32()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real PNG, encoded here: 8×4, a gradient, so a texel out of place
    /// shows.
    fn png() -> Vec<u8> {
        let img = image::RgbaImage::from_fn(8, 4, |x, y| {
            image::Rgba([x as u8 * 30, y as u8 * 60, 7, 255])
        });
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png)
            .expect("encode png");
        out.into_inner()
    }

    fn geographic_job() -> ImageryDecode {
        ImageryDecode {
            bytes: Bytes::from(png()),
            scheme: TilingScheme::geographic(),
            coord: ImageryCoord {
                level: 3,
                x: 5,
                y: 2,
            },
            stored: StoredPlan::OFF,
        }
    }

    /// The job crosses whole: the far side reads exactly the bytes, grid and
    /// tile the near side wrote.
    #[test]
    fn an_imagery_job_reads_back_as_it_was_written() {
        let mut job = geographic_job();
        job.scheme = TilingScheme {
            minimum_level: 1,
            maximum_level: 17,
            ..TilingScheme::web_mercator()
        };
        job.coord = ImageryCoord {
            level: 17,
            x: u64::from(u32::MAX) + 3,
            y: 99,
        };
        job.stored = StoredPlan {
            uastc: ImageryFormat::Astc4x4,
            keep_bc7: false,
            keep_astc: true,
        };
        let back = ImageryDecode::decode(job.encode()).expect("decode");
        assert_eq!(back.bytes, job.bytes);
        assert_eq!(back.scheme, job.scheme);
        assert_eq!(back.coord, job.coord);
        assert_eq!(back.stored, job.stored);
        assert!(ImageryDecode::decode(vec![0; 10]).is_err());
    }

    /// Through bytes and back, the answer is the one running the job here
    /// gives: pixel for pixel, size, and the black flag.
    #[test]
    fn an_imagery_answer_through_bytes_is_the_answer_run_here() {
        for scheme in [TilingScheme::geographic(), TilingScheme::web_mercator()] {
            let job = ImageryDecode {
                scheme,
                ..geographic_job()
            };
            let here = job.clone().run().expect("decodes");
            let there = ImageryDecode::decode_output(
                serve(ImageryDecode::KIND, job.encode()).expect("served"),
            )
            .expect("readable")
            .expect("decodes");
            assert_eq!(
                (there.0.width, there.0.height),
                (here.0.width, here.0.height)
            );
            assert_eq!(there.0.rgba8().unwrap(), here.0.rgba8().unwrap());
            assert_eq!(there.1, here.1);
        }
    }

    /// A tile that does not decode is an answer, not a transport failure: it
    /// comes back as the decoder's own error, so nobody runs it twice.
    #[test]
    fn a_corrupt_image_comes_back_as_the_decoders_error() {
        let job = ImageryDecode {
            bytes: Bytes::from_static(b"not an image"),
            ..geographic_job()
        };
        let here = job.clone().run().expect_err("not an image");
        let answer = serve(ImageryDecode::KIND, job.encode()).expect("an answer");
        let there = ImageryDecode::decode_output(answer)
            .expect("readable")
            .expect_err("not an image");
        assert_eq!(there, here);
    }

    /// An all-black tile is named on the far side too.
    #[test]
    fn a_black_tile_is_named_through_bytes() {
        let black = image::RgbaImage::from_pixel(4, 4, image::Rgba([0, 0, 0, 255]));
        let mut png = std::io::Cursor::new(Vec::new());
        black
            .write_to(&mut png, image::ImageFormat::Png)
            .expect("encode");
        let job = ImageryDecode {
            bytes: Bytes::from(png.into_inner()),
            ..geographic_job()
        };
        let (_, is_black) =
            ImageryDecode::decode_output(serve(ImageryDecode::KIND, job.encode()).expect("served"))
                .expect("readable")
                .expect("decodes");
        assert!(is_black);
    }

    const UASTC: &[u8] = include_bytes!("../../tuile-core/tests/fixtures/uastc/alps-0-crop.uastc");
    const ASTC: &[u8] = include_bytes!("../../tuile-core/tests/fixtures/uastc/alps-0-crop.astc");

    /// A KTX 2.0 tile of texcomp's reference UASTC blocks, 64² with its full
    /// chain (each level the first blocks of level 0).
    fn ktx2_tile() -> Vec<u8> {
        use texcomp::ktx2::{Format, Texture, UastcChannels};
        let levels = (0..7)
            .map(|l| UASTC[..BlockFormat::level_bytes(64 >> l, 64 >> l)].to_vec())
            .collect();
        texcomp::ktx2::write(&Texture {
            format: Format::Uastc(UastcChannels::Rgb),
            srgb: true,
            width: 64,
            height: 64,
            levels,
        })
        .expect("a valid texture")
    }

    fn ktx2_job(stored: StoredPlan) -> ImageryDecode {
        ImageryDecode {
            bytes: Bytes::from(ktx2_tile()),
            stored,
            ..geographic_job()
        }
    }

    /// A stored tile, for a client that chose ASTC: transcoded in the worker,
    /// and every level crosses back as upstream's blocks, byte for byte.
    #[test]
    fn a_transcoded_tile_crosses_as_its_blocks() {
        let stored = StoredPlan {
            uastc: ImageryFormat::Astc4x4,
            keep_bc7: false,
            keep_astc: true,
        };
        let job = ktx2_job(stored);
        let here = job.clone().run().expect("transcodes");
        let there =
            ImageryDecode::decode_output(serve(ImageryDecode::KIND, job.encode()).expect("served"))
                .expect("readable")
                .expect("transcodes");
        assert_eq!((there.0.width, there.0.height), (64, 64));
        assert_eq!(there.0.texels, here.0.texels);
        let Texels::Blocks { format, levels } = &there.0.texels else {
            panic!("blocks expected");
        };
        assert_eq!(*format, BlockFormat::Astc4x4);
        assert_eq!(levels.len(), 7);
        for (l, level) in levels.iter().enumerate() {
            assert!(level[..] == ASTC[..level.len()], "level {l}");
        }
        assert!(!there.1, "blocks are never called black");
        assert_eq!(there.0.cost().gpu, here.0.cost().gpu);
    }

    /// Not opted in: the same tile comes back as RGBA8, unpacked, the same
    /// on both sides of the wire.
    #[test]
    fn a_stored_tile_unpacks_when_not_opted_in() {
        let job = ktx2_job(StoredPlan::OFF);
        let here = job.clone().run().expect("unpacks");
        let there =
            ImageryDecode::decode_output(serve(ImageryDecode::KIND, job.encode()).expect("served"))
                .expect("readable")
                .expect("unpacks");
        assert_eq!(there.0.resident_format(), ImageryFormat::Rgba8);
        assert_eq!(there.0.rgba8().map(<[u8]>::len), Some(64 * 64 * 4));
        assert_eq!(there.0.texels, here.0.texels);
    }

    /// A corrupt KTX 2.0 tile is the decoder's error on both sides — a failed
    /// tile the ancestor covers — never a panic and never a texture.
    #[test]
    fn a_corrupt_ktx2_tile_comes_back_as_the_decoders_error() {
        let mut bytes = ktx2_tile();
        bytes.truncate(bytes.len() - 5);
        for stored in [
            StoredPlan::OFF,
            StoredPlan {
                uastc: ImageryFormat::Bc7,
                keep_bc7: true,
                keep_astc: false,
            },
        ] {
            let job = ImageryDecode {
                bytes: Bytes::from(bytes.clone()),
                ..ktx2_job(stored)
            };
            let here = job.clone().run().expect_err("corrupt");
            assert!(here.contains("KTX2"), "{here}");
            let there = ImageryDecode::decode_output(
                serve(ImageryDecode::KIND, job.encode()).expect("an answer"),
            )
            .expect("readable")
            .expect_err("corrupt");
            assert_eq!(there, here);
        }
    }

    /// Blocks whose levels do not add up to their size are refused.
    #[test]
    fn short_blocks_are_refused() {
        let blocks = DecodedTexture {
            width: 8,
            height: 8,
            texels: Texels::Blocks {
                format: BlockFormat::Bc7,
                levels: vec![vec![1; 64], vec![2; 16], vec![3; 16], vec![4; 16]],
            },
        };
        let answer = ImageryDecode::encode_output(Ok((blocks.clone(), false)));
        let back = ImageryDecode::decode_output(answer.clone())
            .expect("readable")
            .expect("blocks");
        assert_eq!(back.0.texels, blocks.texels);
        let mut short = answer;
        short.remove(0);
        assert!(ImageryDecode::decode_output(short).is_err());
    }

    /// A pixel count that does not match the size is refused, not drawn.
    #[test]
    fn a_short_texture_is_refused() {
        let mut answer = ImageryDecode::encode_output(Ok((
            DecodedTexture::from_rgba8(2, 2, vec![9; 16]),
            false,
        )));
        answer.remove(0);
        assert!(ImageryDecode::decode_output(answer).is_err());
        assert!(ImageryDecode::decode_output(Vec::new()).is_err());
    }

    fn mesh(normals: bool, metadata: bool) -> QuantizedMesh {
        QuantizedMesh {
            header: Header {
                center: [4_000_000.5, 300_000.25, 4_800_000.125],
                min_height: -12.5,
                max_height: 4810.0,
                bounding_sphere_center: [1.0, 2.0, 3.0],
                bounding_sphere_radius: 9876.5,
                horizon_occlusion: [0.1, 0.2, 0.3],
            },
            u: vec![0.0, 1.0, 0.0, 1.0 / 3.0],
            v: vec![0.0, 0.0, 1.0, 0.7],
            height: vec![0.5, 0.25, 1.0, 0.0],
            indices: vec![0, 1, 2, 1, 3, 2],
            normals: normals.then(|| vec![[0.0, 0.0, 1.0]; 4]),
            edges: [vec![0, 2], vec![0, 1], vec![1, 3], vec![2, 3]],
            metadata_available: metadata.then(|| {
                vec![
                    vec![AvailabilityRange {
                        start_x: 0,
                        start_y: 1,
                        end_x: 2,
                        end_y: 3,
                    }],
                    Vec::new(),
                ]
            }),
            cut: 2,
        }
    }

    /// Every field of a mesh crosses bit for bit, with and without its
    /// optional parts.
    #[test]
    fn a_mesh_reads_back_bit_for_bit() {
        for (normals, metadata) in [(false, false), (true, false), (false, true), (true, true)] {
            let sent = mesh(normals, metadata);
            let back = TerrainDecode::decode_output(TerrainDecode::encode_output(Ok(sent.clone())))
                .expect("readable")
                .expect("a mesh");
            assert_eq!(format!("{back:?}"), format!("{sent:?}"));
        }
        let truncated = {
            let mut answer = TerrainDecode::encode_output(Ok(mesh(true, true)));
            answer.remove(40);
            answer
        };
        assert!(TerrainDecode::decode_output(truncated).is_err());
    }

    /// A real tile, decoded on the far side, is the mesh decoding it here
    /// gives.
    #[test]
    fn a_decoded_tile_through_bytes_is_the_tile_decoded_here() {
        let m = mesh(true, false);
        let bytes = tuile_terrain::decode::encode(
            &m.header,
            &m.u,
            &m.v,
            &m.height,
            &m.indices,
            m.normals.as_deref(),
            &m.edges,
        );
        let job = TerrainDecode { bytes };
        let here = job.clone().run().expect("decodes");
        let there =
            TerrainDecode::decode_output(serve(TerrainDecode::KIND, job.encode()).expect("served"))
                .expect("readable")
                .expect("decodes");
        assert_eq!(format!("{there:?}"), format!("{here:?}"));
    }

    /// The decoder's refusal of a corrupt tile comes back as itself.
    #[test]
    fn a_corrupt_terrain_tile_comes_back_as_the_decoders_error() {
        let job = TerrainDecode {
            bytes: vec![1, 2, 3],
        };
        let here = job.clone().run().expect_err("truncated");
        let there = TerrainDecode::decode_output(
            serve(TerrainDecode::KIND, job.encode()).expect("an answer"),
        )
        .expect("readable")
        .expect_err("truncated");
        assert_eq!(there, here);
    }

    #[test]
    fn an_unknown_job_is_named() {
        assert!(serve("teapot", Vec::new()).unwrap_err().contains("teapot"));
    }
}
