// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! `layer.json` — the metadata document of a quantized-mesh terrain set
//! (the terrain equivalent of `tileset.json`). Describes the format, tiling
//! scheme, tile URL templates, available extensions, and which tiles exist
//! at each level (availability ranges).
//!
//! Modelled on cesium-native's `LayerSpec` / CesiumJS `CesiumTerrainProvider`
//! `layer.json` parsing.

use crate::tiling::{GeoRect, GeographicTilingScheme, TileCoord};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum LayerError {
    #[error("invalid layer.json: {0}")]
    Json(#[from] serde_json::Error),
}

/// Why a tile's address could not be built from a layer.
#[derive(Debug, thiserror::Error)]
pub enum TileUrlError {
    #[error("layer.json has no tile template")]
    NoTemplate,
    #[error("tile address: {0}")]
    Url(#[from] url::ParseError),
}

/// A rectangle of available tile coordinates at one level
/// (`{startX, startY, endX, endY}`, inclusive, TMS convention).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct AvailabilityRange {
    #[serde(rename = "startX")]
    pub start_x: u64,
    #[serde(rename = "startY")]
    pub start_y: u64,
    #[serde(rename = "endX")]
    pub end_x: u64,
    #[serde(rename = "endY")]
    pub end_y: u64,
}

impl AvailabilityRange {
    pub fn contains(&self, x: u64, y: u64) -> bool {
        x >= self.start_x && x <= self.end_x && y >= self.start_y && y <= self.end_y
    }

    /// The tiles of `level` that `rect` (radians) covers any part of, in
    /// the geographic scheme, TMS rows: a tile that only touches the
    /// rectangle along an edge is not one of them. `None` for a rectangle
    /// of no area.
    pub fn covering(rect: GeoRect, level: u32) -> Option<Self> {
        use std::f64::consts::{FRAC_PI_2, PI};
        if !(rect.east > rect.west && rect.north > rect.south) {
            return None;
        }
        let scheme = GeographicTilingScheme::default();
        let (nx, ny) = (scheme.tiles_x(level), scheme.tiles_y(level));
        // Tile index of an angle, as a fraction: whole numbers on the
        // tiles' edges.
        let column = |lon: f64| (lon + PI) / (2.0 * PI) * nx as f64;
        let row = |lat: f64| (lat + FRAC_PI_2) / PI * ny as f64;
        let first = |f: f64, n: u64| (f.floor().max(0.0) as u64).min(n - 1);
        let last = |f: f64, n: u64| ((f.ceil() - 1.0).max(0.0) as u64).min(n - 1);
        Some(Self {
            start_x: first(column(rect.west), nx),
            start_y: first(row(rect.south), ny),
            end_x: last(column(rect.east), nx),
            end_y: last(row(rect.north), ny),
        })
    }
}

/// The JSON carried by a tile's `metadata` extension (quantized-mesh extension
/// id 4). Cesium World Terrain ships only shallow availability in `layer.json`
/// and delivers the deeper levels here, per tile: `available[offset]` lists the
/// existing tiles at level `tile_level + offset + 1`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TileMetadata {
    #[serde(default)]
    pub available: Vec<Vec<AvailabilityRange>>,
}

/// A `layer.json`, as parsed and as written.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct LayerJson {
    /// e.g. "quantized-mesh-1.0".
    pub format: String,
    /// "tms" (default) or "slippyMap".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheme: Option<String>,
    /// "EPSG:4326" (geographic) or "EPSG:3857".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection: Option<String>,
    /// URL templates, e.g. "{z}/{x}/{y}.terrain?v={version}".
    #[serde(default)]
    pub tiles: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default)]
    pub minzoom: u32,
    #[serde(default)]
    pub maxzoom: u32,
    /// `metadataAvailability` (Cesium World Terrain): the stride at which tiles
    /// carry deeper availability in their `metadata` extension. Its presence
    /// means `available` below is only a shallow seed — the rest is discovered
    /// per tile as they download.
    #[serde(
        default,
        rename = "metadataAvailability",
        skip_serializing_if = "Option::is_none"
    )]
    pub metadata_availability: Option<u32>,
    /// Available extensions: "octvertexnormals", "watermask", "metadata".
    #[serde(default)]
    pub extensions: Vec<String>,
    /// `available[level]` = ranges of existing tiles at that level (TMS).
    #[serde(default)]
    pub available: Vec<Vec<AvailabilityRange>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution: Option<String>,
}

impl LayerJson {
    pub fn from_slice(bytes: &[u8]) -> Result<Self, LayerError> {
        Ok(serde_json::from_slice(bytes)?)
    }

    /// The `layer.json` of a terrain whose tiles are those covering `rect`
    /// (radians) at every level from 0 to `max_level`: quantized mesh, TMS,
    /// geographic, its tiles at `{z}/{x}/{y}.terrain` beside it, offering
    /// `extensions` (e.g. `"octvertexnormals"`).
    pub fn for_footprint(rect: GeoRect, max_level: u32, extensions: &[&str]) -> Self {
        let available = (0..=max_level)
            .map(|level| {
                AvailabilityRange::covering(rect, level)
                    .into_iter()
                    .collect()
            })
            .collect();
        Self {
            format: "quantized-mesh-1.0".into(),
            scheme: Some("tms".into()),
            projection: Some("EPSG:4326".into()),
            tiles: vec!["{z}/{x}/{y}.terrain".into()],
            version: Some("1.0.0".into()),
            minzoom: 0,
            maxzoom: max_level,
            metadata_availability: None,
            extensions: extensions.iter().map(|e| (*e).to_owned()).collect(),
            available,
            attribution: None,
        }
    }

    /// The JSON document, as a server answers it.
    pub fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a layer is plain data")
    }

    pub fn is_quantized_mesh(&self) -> bool {
        self.format.starts_with("quantized-mesh")
    }

    pub fn has_extension(&self, name: &str) -> bool {
        self.extensions.iter().any(|e| e == name)
    }

    /// Extension query value for tile requests, e.g.
    /// "octvertexnormals-watermask" (only the extensions this layer offers).
    pub fn extensions_query(&self) -> Option<String> {
        let wanted: Vec<&str> = ["octvertexnormals", "watermask", "metadata"]
            .into_iter()
            .filter(|e| self.has_extension(e))
            .collect();
        (!wanted.is_empty()).then(|| wanted.join("-"))
    }

    /// Whether a tile exists, per the availability ranges.
    pub fn is_available(&self, c: TileCoord) -> bool {
        match self.available.get(c.level as usize) {
            Some(ranges) => ranges.iter().any(|r| r.contains(c.x, c.y)),
            None => false,
        }
    }

    /// Expands a tile URL template (`{z}/{x}/{y}`, `{version}`) for a tile.
    /// `y` is given TMS (south = 0); the template is filled as-is.
    pub fn tile_url(&self, c: TileCoord) -> Option<String> {
        let template = self.tiles.first()?;
        let version = self.version.as_deref().unwrap_or("");
        Some(
            template
                .replace("{z}", &c.level.to_string())
                .replace("{x}", &c.x.to_string())
                .replace("{y}", &c.y.to_string())
                .replace("{version}", version),
        )
    }

    /// Where tile `c` is asked for: the template ([`Self::tile_url`])
    /// resolved against `base` — the `layer.json`'s own address, or the
    /// directory holding it — with the extensions this layer offers asked
    /// for in the query (`extensions=octvertexnormals-…`). Every HTTP
    /// terrain source builds its tile addresses here.
    pub fn tile_request_url(
        &self,
        base: &url::Url,
        c: TileCoord,
    ) -> Result<url::Url, TileUrlError> {
        let relative = self.tile_url(c).ok_or(TileUrlError::NoTemplate)?;
        let mut url = base.join(&relative)?;
        if let Some(ext) = self.extensions_query() {
            url.query_pairs_mut().append_pair("extensions", &ext);
        }
        Ok(url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A trimmed Cesium World Terrain layer.json shape.
    const LAYER: &str = r#"{
      "tilejson": "2.1.0",
      "format": "quantized-mesh-1.0",
      "version": "1.2.0",
      "scheme": "tms",
      "projection": "EPSG:4326",
      "tiles": ["{z}/{x}/{y}.terrain?v={version}"],
      "minzoom": 0,
      "maxzoom": 19,
      "bounds": [-180, -90, 180, 90],
      "extensions": ["watermask", "metadata", "octvertexnormals"],
      "available": [
        [{ "startX": 0, "startY": 0, "endX": 1, "endY": 0 }],
        [{ "startX": 0, "startY": 0, "endX": 3, "endY": 1 }]
      ]
    }"#;

    #[test]
    fn parses_cwt_layer() {
        let l = LayerJson::from_slice(LAYER.as_bytes()).expect("parse");
        assert!(l.is_quantized_mesh());
        assert_eq!(l.scheme.as_deref(), Some("tms"));
        assert_eq!(l.maxzoom, 19);
        assert!(l.has_extension("octvertexnormals"));
    }

    #[test]
    fn extensions_query_keeps_known_only() {
        let l = LayerJson::from_slice(LAYER.as_bytes()).expect("parse");
        // Order is normals/watermask/metadata regardless of layer order.
        assert_eq!(
            l.extensions_query().as_deref(),
            Some("octvertexnormals-watermask-metadata")
        );
    }

    #[test]
    fn availability_gates_tiles() {
        let l = LayerJson::from_slice(LAYER.as_bytes()).expect("parse");
        assert!(l.is_available(TileCoord::new(0, 0, 0)));
        assert!(l.is_available(TileCoord::new(0, 1, 0)));
        assert!(
            !l.is_available(TileCoord::new(0, 2, 0)),
            "x=2 absent at level 0"
        );
        assert!(l.is_available(TileCoord::new(1, 3, 1)));
        assert!(
            !l.is_available(TileCoord::new(5, 0, 0)),
            "level 5 has no ranges"
        );
    }

    #[test]
    fn tile_url_fills_template() {
        let l = LayerJson::from_slice(LAYER.as_bytes()).expect("parse");
        assert_eq!(
            l.tile_url(TileCoord::new(9, 541, 386)).as_deref(),
            Some("9/541/386.terrain?v=1.2.0")
        );
    }

    /// What is written is what is read: every field, the standard keys.
    #[test]
    fn a_layer_written_parses_back_the_same() {
        let parsed = LayerJson::from_slice(LAYER.as_bytes()).expect("parse");
        let written = parsed.to_json();
        let text = String::from_utf8(written.clone()).expect("utf-8");
        assert!(text.contains("\"startX\":0"), "{text}");
        assert_eq!(LayerJson::from_slice(&written).expect("parse back"), parsed);

        let with_stride = LayerJson {
            metadata_availability: Some(10),
            attribution: Some("someone".into()),
            ..parsed
        };
        let text = String::from_utf8(with_stride.to_json()).expect("utf-8");
        assert!(text.contains("\"metadataAvailability\":10"), "{text}");
        assert_eq!(
            LayerJson::from_slice(text.as_bytes()).expect("parse back"),
            with_stride
        );
        // Unset fields are left out, not written as null.
        let footprint = LayerJson::for_footprint(GeoRect::from_degrees(0.0, 0.0, 1.0, 1.0), 2, &[]);
        let text = String::from_utf8(footprint.to_json()).expect("utf-8");
        assert!(!text.contains("null"), "{text}");
        assert_eq!(
            LayerJson::from_slice(text.as_bytes()).expect("parse"),
            footprint
        );
    }

    /// 16 km on the equator, east of the prime meridian: one tile per level
    /// down to where it starts to span several, in TMS rows (y = 0 south).
    #[test]
    fn a_footprint_s_layer_lists_the_tiles_it_covers_at_each_level() {
        let rect = GeoRect::from_degrees(0.01, -0.072, 0.154, 0.072);
        let layer = LayerJson::for_footprint(rect, 15, &["octvertexnormals"]);
        assert_eq!(layer.maxzoom, 15);
        assert_eq!(layer.available.len(), 16);
        assert_eq!(
            layer.extensions_query().as_deref(),
            Some("octvertexnormals")
        );
        assert_eq!(layer.scheme.as_deref(), Some("tms"));
        // Level 0: the eastern hemisphere only.
        assert_eq!(
            layer.available[0],
            vec![AvailabilityRange {
                start_x: 1,
                start_y: 0,
                end_x: 1,
                end_y: 0
            }]
        );
        // Level 1: straddles the equator, so both rows of the eastern half.
        assert_eq!(
            layer.available[1],
            vec![AvailabilityRange {
                start_x: 2,
                start_y: 0,
                end_x: 2,
                end_y: 1
            }]
        );
        // Every level: exactly the tiles whose rectangle overlaps the footprint.
        let scheme = GeographicTilingScheme::default();
        for level in [5u32, 10, 15] {
            let range = layer.available[level as usize][0];
            for x in range.start_x.saturating_sub(1)..=range.end_x + 1 {
                for y in range.start_y.saturating_sub(1)..=range.end_y + 1 {
                    let t = scheme.tile_rect(TileCoord::new(level, x, y));
                    let overlaps = t.west < rect.east
                        && t.east > rect.west
                        && t.south < rect.north
                        && t.north > rect.south;
                    assert_eq!(range.contains(x, y), overlaps, "{level}/{x}/{y}");
                }
            }
        }
        // Level 15: 0.0055° tiles, 26 or 27 across 0.144°.
        let r = layer.available[15][0];
        assert!((26..=28).contains(&(r.end_x - r.start_x + 1)), "{r:?}");
        assert!(!layer.is_available(TileCoord::new(16, r.start_x * 2, r.start_y * 2)));
    }

    /// A footprint whose edge lies on tile edges takes no tile beyond it.
    #[test]
    fn a_footprint_on_tile_edges_takes_no_neighbour() {
        let rect = GeoRect::from_degrees(0.0, 0.0, 90.0, 45.0);
        assert_eq!(
            AvailabilityRange::covering(rect, 2),
            Some(AvailabilityRange {
                start_x: 4,
                start_y: 2,
                end_x: 5,
                end_y: 2
            })
        );
        assert_eq!(
            AvailabilityRange::covering(GeoRect::from_degrees(1.0, 1.0, 1.0, 2.0), 3),
            None
        );
    }

    /// A tile's address is its template resolved against the layer's own
    /// address, with the offered extensions asked for in the query.
    #[test]
    fn a_tile_request_resolves_against_the_layer_and_asks_for_extensions() {
        let l = LayerJson::from_slice(LAYER.as_bytes()).expect("parse");
        let layer_url = url::Url::parse("https://example.org/terrain/v1/layer.json").expect("url");
        let dir = url::Url::parse("https://example.org/terrain/v1/").expect("url");
        let expected = "https://example.org/terrain/v1/9/541/386.terrain?v=1.2.0&extensions=octvertexnormals-watermask-metadata";
        for base in [layer_url, dir] {
            assert_eq!(
                l.tile_request_url(&base, TileCoord::new(9, 541, 386))
                    .expect("url")
                    .as_str(),
                expected
            );
        }
        let bare = LayerJson {
            tiles: Vec::new(),
            ..l
        };
        assert!(matches!(
            bare.tile_request_url(&dir_url(), TileCoord::new(0, 0, 0)),
            Err(TileUrlError::NoTemplate)
        ));
    }

    fn dir_url() -> url::Url {
        url::Url::parse("https://example.org/").expect("url")
    }
}
