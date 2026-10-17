// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The streaming protocol as bytes, for `postMessage`.
//!
//! A Web Worker and its page share no memory: what crosses is a structured
//! clone, or an `ArrayBuffer` *transferred* — handed over without a copy. So
//! each [`ClientMessage`] and [`ServerMessage`] is written into one buffer,
//! which the sender transfers and the receiver reads back into the very same
//! types. Nothing is translated into a JavaScript shape on the way: the page
//! hands the decoded messages to the same consumer a desktop does.
//!
//! # Format
//!
//! Little-endian, one tag byte per enum, lengths as `u32`. Floats travel as
//! their bit patterns, so a value is the same value on both sides — the f64
//! origins that carry the anti-jitter rebasing included. There is no version
//! field: both ends are the same wasm module, built together.
//!
//! The matches below are exhaustive on purpose. A variant added to the
//! protocol is a compile error here until it is given an encoding, never a
//! message silently dropped between the worker and the page.

use std::sync::Arc;

use glam::{DVec2, DVec3, Mat4};
use tuile_core::content::{
    ContentFormat, DecodedMesh, DecodedTexture, DecodedTileContent, MaterialDesc, TileContent,
};
use tuile_core::protocol::{Ancestry, ClientMessage, Priming, ServerMessage};
use tuile_core::raster::{ImageryCoord, ImageryLayer};
use tuile_core::source::TileId;
use tuile_core::traversal::{TraversalStats, ViewState, ViewStateParams};

/// A buffer that does not read back into a message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WireError {
    #[error("the message ends {0} bytes early")]
    Truncated(usize),
    #[error("unknown tag {tag} for {what}")]
    Tag { what: &'static str, tag: u8 },
    #[error("{0} bytes left over after the message")]
    Trailing(usize),
    #[error("a message string is not UTF-8")]
    Utf8,
}

/// Writes a message from the page to the worker.
pub fn encode_client(message: &ClientMessage) -> Vec<u8> {
    let mut w = Writer::default();
    match message {
        ClientMessage::ViewerState { views, generation } => {
            w.u8(0);
            w.len(views.len());
            for view in views {
                let p = ViewStateParams::from(*view);
                w.dvec3(p.position);
                w.dvec3(p.direction);
                w.dvec3(p.up);
                w.f64(p.viewport_px.x);
                w.f64(p.viewport_px.y);
                w.f64(p.fovy_rad);
            }
            w.u64(*generation);
        }
        ClientMessage::Ack { tile } => {
            w.u8(1);
            w.u64(tile.0);
        }
        ClientMessage::Cancel { tile } => {
            w.u8(2);
            w.u64(tile.0);
        }
        ClientMessage::Trim { budget_bytes } => {
            w.u8(3);
            w.u64(*budget_bytes as u64);
        }
    }
    w.0
}

/// Reads a message the page wrote.
pub fn decode_client(bytes: &[u8]) -> Result<ClientMessage, WireError> {
    let mut r = Reader::new(bytes);
    let message = match r.u8()? {
        0 => {
            let n = r.len()?;
            let mut views = Vec::with_capacity(n.min(16));
            for _ in 0..n {
                let params = ViewStateParams {
                    position: r.dvec3()?,
                    direction: r.dvec3()?,
                    up: r.dvec3()?,
                    viewport_px: DVec2::new(r.f64()?, r.f64()?),
                    fovy_rad: r.f64()?,
                };
                views.push(ViewState::from(params));
            }
            ClientMessage::ViewerState {
                views,
                generation: r.u64()?,
            }
        }
        1 => ClientMessage::Ack {
            tile: TileId(r.u64()?),
        },
        2 => ClientMessage::Cancel {
            tile: TileId(r.u64()?),
        },
        3 => ClientMessage::Trim {
            budget_bytes: r.u64()? as usize,
        },
        tag => {
            return Err(WireError::Tag {
                what: "a client message",
                tag,
            })
        }
    };
    r.finish()?;
    Ok(message)
}

/// Writes a message from the worker to the page.
pub fn encode_server(message: &ServerMessage) -> Vec<u8> {
    let mut w = Writer::default();
    match message {
        ServerMessage::Select {
            tiles,
            ancestry,
            stats,
            generation,
        } => {
            w.u8(0);
            w.len(tiles.len());
            for (tile, sse) in tiles {
                w.u64(tile.0);
                w.f64(*sse);
            }
            w.len(ancestry.len());
            for (tile, a) in ancestry {
                w.u64(tile.0);
                w.ancestry(a);
            }
            w.stats(stats);
            w.u64(*generation);
        }
        ServerMessage::Content {
            tile,
            ancestry,
            content,
        } => {
            w.u8(1);
            w.u64(tile.0);
            w.ancestry(ancestry);
            match content {
                TileContent::Raw { format, bytes } => {
                    w.u8(0);
                    w.u8(match format {
                        ContentFormat::Glb => 0,
                        ContentFormat::B3dm => 1,
                    });
                    w.bytes(bytes);
                }
                TileContent::Decoded(decoded) => {
                    w.u8(1);
                    w.decoded(decoded);
                }
            }
        }
        ServerMessage::Fill {
            tile,
            ancestry,
            content,
        } => {
            w.u8(2);
            w.u64(tile.0);
            w.ancestry(ancestry);
            w.decoded(content);
        }
        ServerMessage::Evict { tiles } => {
            w.u8(3);
            w.tiles(tiles);
        }
        ServerMessage::Retire { tiles } => {
            w.u8(4);
            w.tiles(tiles);
        }
        ServerMessage::Error { tile, message } => {
            w.u8(5);
            w.option_u64(tile.map(|t| t.0));
            w.bytes(message.as_bytes());
        }
        ServerMessage::Priming(p) => {
            w.u8(6);
            w.u32(p.total);
            w.u32(p.unavailable);
            w.u32(p.outstanding);
        }
    }
    w.0
}

/// Reads a message the worker wrote.
pub fn decode_server(bytes: &[u8]) -> Result<ServerMessage, WireError> {
    let mut r = Reader::new(bytes);
    let message = match r.u8()? {
        0 => {
            let n = r.len()?;
            let mut tiles = Vec::with_capacity(n);
            for _ in 0..n {
                tiles.push((TileId(r.u64()?), r.f64()?));
            }
            let n = r.len()?;
            let mut ancestry = Vec::with_capacity(n);
            for _ in 0..n {
                ancestry.push((TileId(r.u64()?), r.ancestry()?));
            }
            ServerMessage::Select {
                tiles,
                ancestry,
                stats: r.stats()?,
                generation: r.u64()?,
            }
        }
        1 => {
            let tile = TileId(r.u64()?);
            let ancestry = r.ancestry()?;
            let content = match r.u8()? {
                0 => {
                    let format = match r.u8()? {
                        0 => ContentFormat::Glb,
                        1 => ContentFormat::B3dm,
                        tag => {
                            return Err(WireError::Tag {
                                what: "a content format",
                                tag,
                            })
                        }
                    };
                    TileContent::Raw {
                        format,
                        bytes: bytes::Bytes::from(r.bytes()?.to_vec()),
                    }
                }
                1 => TileContent::Decoded(r.decoded()?),
                tag => {
                    return Err(WireError::Tag {
                        what: "a tile content",
                        tag,
                    })
                }
            };
            ServerMessage::Content {
                tile,
                ancestry,
                content,
            }
        }
        2 => ServerMessage::Fill {
            tile: TileId(r.u64()?),
            ancestry: r.ancestry()?,
            content: r.decoded()?,
        },
        3 => ServerMessage::Evict { tiles: r.tiles()? },
        4 => ServerMessage::Retire { tiles: r.tiles()? },
        5 => ServerMessage::Error {
            tile: r.option_u64()?.map(TileId),
            message: String::from_utf8(r.bytes()?.to_vec()).map_err(|_| WireError::Utf8)?,
        },
        6 => ServerMessage::Priming(Priming {
            total: r.u32()?,
            unavailable: r.u32()?,
            outstanding: r.u32()?,
        }),
        tag => {
            return Err(WireError::Tag {
                what: "a server message",
                tag,
            })
        }
    };
    r.finish()?;
    Ok(message)
}

#[derive(Default)]
struct Writer(Vec<u8>);

impl Writer {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn f32(&mut self, v: f32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn f64(&mut self, v: f64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    /// Lengths are `u32`: no message comes near four billion of anything.
    fn len(&mut self, n: usize) {
        self.u32(u32::try_from(n).unwrap_or(u32::MAX));
    }
    fn bytes(&mut self, b: &[u8]) {
        self.len(b.len());
        self.0.extend_from_slice(b);
    }
    fn dvec3(&mut self, v: DVec3) {
        self.f64(v.x);
        self.f64(v.y);
        self.f64(v.z);
    }
    fn floats(&mut self, values: &[f32]) {
        self.len(values.len());
        self.0.reserve(values.len() * 4);
        for v in values {
            self.f32(*v);
        }
    }
    fn option_u64(&mut self, v: Option<u64>) {
        match v {
            Some(v) => {
                self.u8(1);
                self.u64(v);
            }
            None => self.u8(0),
        }
    }
    fn tiles(&mut self, tiles: &[TileId]) {
        self.len(tiles.len());
        for t in tiles {
            self.u64(t.0);
        }
    }
    fn ancestry(&mut self, a: &Ancestry) {
        self.u32(a.level);
        self.option_u64(a.parent.map(|p| p.0));
    }
    fn stats(&mut self, s: &TraversalStats) {
        let TraversalStats {
            held_but_drawn,
            deferred_subtrees,
            visited,
            culled,
            selected,
            requested,
            max_depth,
            gaps,
        } = *s;
        for v in [
            held_but_drawn,
            deferred_subtrees,
            visited,
            culled,
            selected,
            requested,
            max_depth,
            gaps,
        ] {
            self.u32(v);
        }
    }
    fn texture(&mut self, t: &DecodedTexture) {
        self.u32(t.width);
        self.u32(t.height);
        self.bytes(&t.rgba8);
    }
    fn decoded(&mut self, c: &DecodedTileContent) {
        let DecodedTileContent {
            meshes,
            textures,
            imagery,
            withheld_drape,
            local_origin_ecef,
            transform_local,
        } = c;
        self.len(meshes.len());
        for m in meshes {
            let DecodedMesh {
                positions,
                normals,
                uvs,
                indices,
                material,
            } = m;
            self.floats(positions.as_flattened());
            match normals {
                Some(n) => {
                    self.u8(1);
                    self.floats(n.as_flattened());
                }
                None => self.u8(0),
            }
            match uvs {
                Some(uv) => {
                    self.u8(1);
                    self.floats(uv.as_flattened());
                }
                None => self.u8(0),
            }
            self.len(indices.len());
            self.0.reserve(indices.len() * 4);
            for i in indices {
                self.u32(*i);
            }
            for f in material.base_color_factor {
                self.f32(f);
            }
            self.option_u64(material.base_color_texture.map(|t| t as u64));
        }
        self.len(textures.len());
        for t in textures {
            self.texture(t);
        }
        self.len(imagery.len());
        for layer in imagery {
            let ImageryLayer {
                coord,
                source,
                texture,
                coverage,
                translation,
                scale,
            } = layer;
            self.u32(coord.level);
            self.u64(coord.x);
            self.u64(coord.y);
            self.u32(*source);
            self.texture(texture);
            for f in coverage.iter().chain(translation).chain(scale) {
                self.f32(*f);
            }
        }
        self.option_u64(*withheld_drape);
        self.dvec3(*local_origin_ecef);
        for f in transform_local.to_cols_array() {
            self.f32(f);
        }
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        let end = self.at.saturating_add(n);
        let slice = self
            .bytes
            .get(self.at..end)
            .ok_or(WireError::Truncated(end.saturating_sub(self.bytes.len())))?;
        self.at = end;
        Ok(slice)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], WireError> {
        let mut out = [0; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }
    fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.array::<1>()?[0])
    }
    fn u32(&mut self) -> Result<u32, WireError> {
        Ok(u32::from_le_bytes(self.array()?))
    }
    fn u64(&mut self) -> Result<u64, WireError> {
        Ok(u64::from_le_bytes(self.array()?))
    }
    fn f32(&mut self) -> Result<f32, WireError> {
        Ok(f32::from_le_bytes(self.array()?))
    }
    fn f64(&mut self) -> Result<f64, WireError> {
        Ok(f64::from_le_bytes(self.array()?))
    }
    fn len(&mut self) -> Result<usize, WireError> {
        Ok(self.u32()? as usize)
    }
    fn bytes(&mut self) -> Result<&'a [u8], WireError> {
        let n = self.len()?;
        self.take(n)
    }
    fn dvec3(&mut self) -> Result<DVec3, WireError> {
        Ok(DVec3::new(self.f64()?, self.f64()?, self.f64()?))
    }
    fn floats(&mut self) -> Result<Vec<f32>, WireError> {
        let n = self.len()?;
        let raw = self.take(n.saturating_mul(4))?;
        Ok(raw
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| f32::from_le_bytes(*c))
            .collect())
    }
    fn vec3s(&mut self) -> Result<Vec<[f32; 3]>, WireError> {
        Ok(self.floats()?.as_chunks::<3>().0.to_vec())
    }
    fn vec2s(&mut self) -> Result<Vec<[f32; 2]>, WireError> {
        Ok(self.floats()?.as_chunks::<2>().0.to_vec())
    }
    fn option_u64(&mut self) -> Result<Option<u64>, WireError> {
        match self.u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.u64()?)),
            tag => Err(WireError::Tag {
                what: "an option",
                tag,
            }),
        }
    }
    fn tiles(&mut self) -> Result<Vec<TileId>, WireError> {
        let n = self.len()?;
        let mut tiles = Vec::with_capacity(n);
        for _ in 0..n {
            tiles.push(TileId(self.u64()?));
        }
        Ok(tiles)
    }
    fn ancestry(&mut self) -> Result<Ancestry, WireError> {
        Ok(Ancestry {
            level: self.u32()?,
            parent: self.option_u64()?.map(TileId),
        })
    }
    fn stats(&mut self) -> Result<TraversalStats, WireError> {
        Ok(TraversalStats {
            held_but_drawn: self.u32()?,
            deferred_subtrees: self.u32()?,
            visited: self.u32()?,
            culled: self.u32()?,
            selected: self.u32()?,
            requested: self.u32()?,
            max_depth: self.u32()?,
            gaps: self.u32()?,
        })
    }
    fn texture(&mut self) -> Result<DecodedTexture, WireError> {
        Ok(DecodedTexture {
            width: self.u32()?,
            height: self.u32()?,
            rgba8: self.bytes()?.to_vec(),
        })
    }
    fn f32s<const N: usize>(&mut self) -> Result<[f32; N], WireError> {
        let mut out = [0.0; N];
        for v in &mut out {
            *v = self.f32()?;
        }
        Ok(out)
    }
    fn decoded(&mut self) -> Result<DecodedTileContent, WireError> {
        let n = self.len()?;
        let mut meshes = Vec::with_capacity(n.min(64));
        for _ in 0..n {
            let positions = self.vec3s()?;
            let normals = match self.u8()? {
                0 => None,
                _ => Some(self.vec3s()?),
            };
            let uvs = match self.u8()? {
                0 => None,
                _ => Some(self.vec2s()?),
            };
            let count = self.len()?;
            let raw = self.take(count.saturating_mul(4))?;
            let indices = raw
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| u32::from_le_bytes(*c))
                .collect();
            let material = MaterialDesc {
                base_color_factor: self.f32s()?,
                base_color_texture: self.option_u64()?.map(|t| t as usize),
            };
            meshes.push(DecodedMesh {
                positions,
                normals,
                uvs,
                indices,
                material,
            });
        }
        let n = self.len()?;
        let mut textures = Vec::with_capacity(n.min(64));
        for _ in 0..n {
            textures.push(self.texture()?);
        }
        let n = self.len()?;
        let mut imagery = Vec::with_capacity(n.min(64));
        for _ in 0..n {
            let coord = ImageryCoord {
                level: self.u32()?,
                x: self.u64()?,
                y: self.u64()?,
            };
            let source = self.u32()?;
            let texture = Arc::new(self.texture()?);
            imagery.push(ImageryLayer {
                coord,
                source,
                texture,
                coverage: self.f32s()?,
                translation: self.f32s()?,
                scale: self.f32s()?,
            });
        }
        Ok(DecodedTileContent {
            meshes,
            textures,
            imagery,
            withheld_drape: self.option_u64()?,
            local_origin_ecef: self.dvec3()?,
            transform_local: Mat4::from_cols_array(&self.f32s()?),
        })
    }
    fn finish(self) -> Result<(), WireError> {
        match self.bytes.len() - self.at {
            0 => Ok(()),
            left => Err(WireError::Trailing(left)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content() -> DecodedTileContent {
        DecodedTileContent {
            meshes: vec![DecodedMesh {
                positions: vec![[1.0, -2.5, 3.25], [f32::MIN_POSITIVE, 0.0, -0.0]],
                normals: Some(vec![[0.0, 0.0, 1.0], [0.0, 1.0, 0.0]]),
                uvs: None,
                indices: vec![0, 1, 1, u32::MAX],
                material: MaterialDesc {
                    base_color_factor: [0.1, 0.2, 0.3, 1.0],
                    base_color_texture: Some(0),
                },
            }],
            textures: vec![DecodedTexture {
                width: 1,
                height: 1,
                rgba8: vec![1, 2, 3, 4],
            }],
            imagery: vec![ImageryLayer {
                coord: ImageryCoord {
                    level: 12,
                    x: 2048,
                    y: 1_000_001,
                },
                source: 3,
                texture: Arc::new(DecodedTexture {
                    width: 2,
                    height: 1,
                    rgba8: vec![9; 8],
                }),
                coverage: [0.0, 0.25, 0.5, 1.0],
                translation: [0.125, -0.5],
                scale: [2.0, 4.0],
            }],
            withheld_drape: Some(u64::MAX - 1),
            // The anti-jitter origin: every bit of the f64 must survive.
            local_origin_ecef: DVec3::new(4_201_234.123_456_789, 172_001.000_000_1, -4_779_000.5),
            transform_local: Mat4::from_translation(glam::Vec3::new(1.0, 2.0, 3.0)),
        }
    }

    fn same_content(a: &DecodedTileContent, b: &DecodedTileContent) {
        assert_eq!(a.meshes.len(), b.meshes.len());
        for (m, n) in a.meshes.iter().zip(&b.meshes) {
            assert_eq!(m.positions, n.positions);
            assert_eq!(m.normals, n.normals);
            assert_eq!(m.uvs, n.uvs);
            assert_eq!(m.indices, n.indices);
            assert_eq!(m.material.base_color_factor, n.material.base_color_factor);
            assert_eq!(m.material.base_color_texture, n.material.base_color_texture);
        }
        assert_eq!(a.textures.len(), b.textures.len());
        for (t, u) in a.textures.iter().zip(&b.textures) {
            assert_eq!((t.width, t.height, &t.rgba8), (u.width, u.height, &u.rgba8));
        }
        assert_eq!(a.imagery.len(), b.imagery.len());
        for (l, k) in a.imagery.iter().zip(&b.imagery) {
            assert_eq!((l.coord, l.source), (k.coord, k.source));
            assert_eq!(l.texture.rgba8, k.texture.rgba8);
            assert_eq!(
                (l.coverage, l.translation, l.scale),
                (k.coverage, k.translation, k.scale)
            );
        }
        assert_eq!(a.withheld_drape, b.withheld_drape);
        assert_eq!(
            a.local_origin_ecef.to_array().map(f64::to_bits),
            b.local_origin_ecef.to_array().map(f64::to_bits)
        );
        assert_eq!(a.transform_local, b.transform_local);
    }

    fn round_trip(message: &ServerMessage) -> ServerMessage {
        decode_server(&encode_server(message)).expect("reads back")
    }

    /// Content crosses whole: meshes, owned textures, draped imagery with its
    /// placement, and the f64 origin bit for bit.
    #[test]
    fn content_crosses_whole() {
        let ancestry = Ancestry {
            level: 7,
            parent: Some(TileId(41)),
        };
        let back = round_trip(&ServerMessage::Content {
            tile: TileId(42),
            ancestry,
            content: TileContent::Decoded(content()),
        });
        let ServerMessage::Content {
            tile,
            ancestry: a,
            content: TileContent::Decoded(decoded),
        } = back
        else {
            panic!("not content: {back:?}");
        };
        assert_eq!((tile, a), (TileId(42), ancestry));
        same_content(&content(), &decoded);
    }

    /// A stand-in stays a stand-in: the consumer must be able to refuse it.
    #[test]
    fn a_fill_stays_a_fill() {
        let back = round_trip(&ServerMessage::Fill {
            tile: TileId(9),
            ancestry: Ancestry {
                level: 0,
                parent: None,
            },
            content: content(),
        });
        let ServerMessage::Fill {
            tile,
            ancestry,
            content: c,
        } = back
        else {
            panic!("not a fill: {back:?}");
        };
        assert_eq!(tile, TileId(9));
        assert_eq!(ancestry.parent, None);
        same_content(&content(), &c);
    }

    /// The selection keeps its closure of ancestors and its generation: the
    /// fallback chain and the camera it answers.
    #[test]
    fn a_selection_keeps_its_ancestry_and_generation() {
        let stats = TraversalStats {
            held_but_drawn: 1,
            deferred_subtrees: 2,
            visited: 3,
            culled: 4,
            selected: 5,
            requested: 6,
            max_depth: 7,
            gaps: 8,
        };
        let message = ServerMessage::Select {
            tiles: vec![(TileId(5), 1.5), (TileId(6), f64::INFINITY)],
            ancestry: vec![(
                TileId(5),
                Ancestry {
                    level: 3,
                    parent: Some(TileId(1)),
                },
            )],
            stats,
            generation: 77,
        };
        let ServerMessage::Select {
            tiles,
            ancestry,
            stats: s,
            generation,
        } = round_trip(&message)
        else {
            panic!("not a selection");
        };
        assert_eq!(tiles, vec![(TileId(5), 1.5), (TileId(6), f64::INFINITY)]);
        assert_eq!(ancestry[0].1.parent, Some(TileId(1)));
        assert_eq!((s, generation), (stats, 77));
    }

    /// Evict and Retire have different blast radii and must not be confused.
    #[test]
    fn evict_and_retire_stay_apart() {
        let tiles = vec![TileId(1), TileId(u64::MAX)];
        assert!(matches!(
            round_trip(&ServerMessage::Evict { tiles: tiles.clone() }),
            ServerMessage::Evict { tiles: t } if t == tiles
        ));
        assert!(matches!(
            round_trip(&ServerMessage::Retire { tiles: tiles.clone() }),
            ServerMessage::Retire { tiles: t } if t == tiles
        ));
    }

    #[test]
    fn errors_priming_and_raw_content_cross() {
        assert!(matches!(
            round_trip(&ServerMessage::Error { tile: Some(TileId(3)), message: "é 404".into() }),
            ServerMessage::Error { tile: Some(TileId(3)), message } if message == "é 404"
        ));
        let p = Priming {
            total: 85,
            unavailable: 4,
            outstanding: 2,
        };
        assert!(
            matches!(round_trip(&ServerMessage::Priming(p)), ServerMessage::Priming(q) if q == p)
        );
        let raw = ServerMessage::Content {
            tile: TileId(1),
            ancestry: Ancestry {
                level: 0,
                parent: None,
            },
            content: TileContent::Raw {
                format: ContentFormat::B3dm,
                bytes: bytes::Bytes::from_static(b"b3dm"),
            },
        };
        assert!(matches!(
            round_trip(&raw),
            ServerMessage::Content {
                content: TileContent::Raw { format: ContentFormat::B3dm, bytes },
                ..
            } if &bytes[..] == b"b3dm"
        ));
    }

    /// The camera crosses exactly: the server culls against what the page
    /// sees.
    #[test]
    fn client_messages_cross() {
        let view = ViewState::perspective(
            DVec3::new(6_400_000.0, 1.0, -2.0),
            DVec3::new(-1.0, 0.0, 0.0),
            DVec3::Z,
            DVec2::new(1280.0, 720.0),
            0.8,
        );
        let message = ClientMessage::ViewerState {
            views: vec![view, view],
            generation: 12,
        };
        let ClientMessage::ViewerState { views, generation } =
            decode_client(&encode_client(&message)).expect("reads back")
        else {
            panic!("not a viewer state");
        };
        assert_eq!(generation, 12);
        assert_eq!(views.len(), 2);
        let (a, b) = (ViewStateParams::from(view), ViewStateParams::from(views[1]));
        assert_eq!(
            (a.position, a.direction, a.up),
            (b.position, b.direction, b.up)
        );
        assert_eq!((a.viewport_px, a.fovy_rad), (b.viewport_px, b.fovy_rad));
        for message in [
            ClientMessage::Ack { tile: TileId(4) },
            ClientMessage::Cancel { tile: TileId(5) },
            ClientMessage::Trim {
                budget_bytes: 1 << 30,
            },
        ] {
            let back = decode_client(&encode_client(&message)).expect("reads back");
            assert_eq!(format!("{back:?}"), format!("{message:?}"));
        }
    }

    /// A damaged buffer is an error, never a panic or a wrong message.
    #[test]
    fn a_damaged_buffer_is_refused() {
        let bytes = encode_server(&ServerMessage::Fill {
            tile: TileId(1),
            ancestry: Ancestry {
                level: 0,
                parent: None,
            },
            content: content(),
        });
        for cut in 0..bytes.len() {
            assert!(decode_server(&bytes[..cut]).is_err(), "cut at {cut}");
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert_eq!(decode_server(&longer).err(), Some(WireError::Trailing(1)));
        assert!(matches!(
            decode_server(&[99]),
            Err(WireError::Tag { tag: 99, .. })
        ));
        assert!(decode_client(&[]).is_err());
    }
}
