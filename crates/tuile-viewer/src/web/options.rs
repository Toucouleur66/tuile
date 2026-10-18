// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What a page asks for when it creates a viewer, read into typed Rust.
//!
//! The package's `createViewer` normalises the host's options — resolves
//! `profile: 'auto'`, reads the device's memory, picks up the token — and
//! hands this module a plain object. Everything a host can get wrong is said
//! here, in one sentence, before a worker is started or a byte fetched.

use serde::{Deserialize, Serialize};

use crate::embed::{ImageryChoice, TerrainChoice};
use crate::start::StartView;

/// The options, as the package hands them over.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Options {
    /// The application's name, for messages: the host's, never ours.
    #[serde(default = "default_name")]
    pub name: String,
    pub sources: SourcesSpec,
    /// `desktop` or `handheld`; the package resolves `auto`.
    pub profile: ProfileChoice,
    /// The device's memory in gibibytes, when the browser says.
    #[serde(default)]
    pub device_memory: Option<f64>,
    #[serde(default)]
    pub backend: BackendChoice,
    #[serde(default)]
    pub view: Option<ViewSpec>,
    /// Whether the on-screen controls and the menu are drawn.
    #[serde(default = "yes")]
    pub ui: bool,
    /// Which bottom corner the caption — whose pictures these are — keeps to.
    #[serde(default)]
    pub attribution: CaptionSide,
    /// Whether a viewer that cannot draw says so in its container.
    #[serde(default = "yes")]
    pub show_unsupported_message: bool,
    /// Whether a viewer opened without a `view` opens where the last one
    /// ended, as a desktop's application does.
    #[serde(default = "yes")]
    pub remember_view: bool,
}

/// Which bottom corner the caption keeps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum CaptionSide {
    BottomLeft,
    #[default]
    BottomRight,
}

fn default_name() -> String {
    "Globe".to_owned()
}

fn yes() -> bool {
    true
}

/// Which numbers the session runs with: see `Profile::browser`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ProfileChoice {
    Desktop,
    Handheld,
}

/// Which graphics backend to draw with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum BackendChoice {
    /// WebGPU where the browser has it, WebGL2 otherwise.
    #[default]
    Auto,
    Webgpu,
    Webgl2,
}

impl BackendChoice {
    /// The backends the instance may choose from.
    pub(crate) fn backends(self) -> wgpu::Backends {
        match self {
            Self::Auto => wgpu::Backends::BROWSER_WEBGPU | wgpu::Backends::GL,
            Self::Webgpu => wgpu::Backends::BROWSER_WEBGPU,
            Self::Webgl2 => wgpu::Backends::GL,
        }
    }
}

/// Where the eye opens: degrees and metres, as the start flags speak.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ViewSpec {
    pub lon: f64,
    pub lat: f64,
    pub altitude: f64,
    #[serde(default)]
    pub heading: f64,
    #[serde(default = "straight_down")]
    pub pitch: f64,
}

fn straight_down() -> f64 {
    90.0
}

impl ViewSpec {
    /// The view, held to the same bounds as the flags.
    pub(crate) fn view(self) -> Result<StartView, String> {
        Ok(StartView {
            lon: crate::start::checked("lon", self.lon)?,
            lat: crate::start::checked("lat", self.lat)?,
            altitude: crate::start::checked("altitude", self.altitude)?,
            heading: crate::start::checked("heading", self.heading)?,
            pitch: crate::start::checked("pitch", self.pitch)?,
        })
    }
}

/// Where terrain and imagery come from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SourcesSpec {
    /// The catalog that resolves asset numbers to endpoints: the address of
    /// the proxy that holds its token. A page never holds one.
    #[serde(default)]
    pub catalog: Option<CatalogSpec>,
    pub terrain: AssetSpec,
    pub imagery: Vec<LayerSpec>,
}

/// The catalog: the proxy that answers for it and holds its token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CatalogSpec {
    pub url: String,
}

/// An asset of the catalog, by number.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AssetSpec {
    pub asset: i64,
}

/// One imagery layer: from the catalog by asset number, or from a URL
/// template.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct LayerSpec {
    pub key: String,
    pub name: String,
    /// Whose pictures these are: kept on screen while the layer is.
    pub attribution: String,
    #[serde(default)]
    pub asset: Option<i64>,
    /// `https://…/{z}/{x}/{y}.png`.
    #[serde(default)]
    pub template: Option<String>,
    #[serde(default)]
    pub scheme: Option<Grid>,
    #[serde(default)]
    pub max_level: Option<u32>,
}

/// The grid a template's tiles are cut on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Grid {
    Webmercator,
    Geographic,
}

impl SourcesSpec {
    /// The terrain, and the layers as the application lists them.
    ///
    /// A template layer has no catalog number, so it is given one the catalog
    /// never uses — negative, by its place in the list — which is what the
    /// worker's sources answer it by.
    pub(crate) fn choices(&self) -> Result<(TerrainChoice, Vec<ImageryChoice>), String> {
        self.catalog_base()?;
        if self.imagery.is_empty() {
            return Err("`sources.imagery` lists no layer".to_owned());
        }
        let mut layers = Vec::with_capacity(self.imagery.len());
        for (index, layer) in self.imagery.iter().enumerate() {
            let asset = match (&layer.asset, &layer.template) {
                (Some(asset), None) if *asset >= 0 => *asset,
                (None, Some(template)) if template.contains("{z}") => template_asset(index),
                (None, Some(_)) => {
                    return Err(format!(
                        "imagery {:?}: a template names {{z}}, {{x}} and {{y}}",
                        layer.key
                    ))
                }
                _ => {
                    return Err(format!(
                        "imagery {:?}: give either a catalog `asset` or a `template`",
                        layer.key
                    ))
                }
            };
            if layers.iter().any(|l: &ImageryChoice| l.key == layer.key) {
                return Err(format!("imagery {:?} is listed twice", layer.key));
            }
            layers.push(ImageryChoice::new(
                &layer.key,
                &layer.name,
                asset,
                &layer.attribution,
            ));
        }
        let terrain = TerrainChoice {
            asset: self.terrain.asset,
            cache: format!("terrain-{}", self.terrain.asset),
        };
        Ok((terrain, layers))
    }

    /// The template layer an asset number stands for, if it is one.
    pub(crate) fn template_of(&self, asset: i64) -> Option<&LayerSpec> {
        let index = usize::try_from(-1 - asset).ok()?;
        self.imagery.get(index).filter(|l| l.template.is_some())
    }

    /// The catalog proxy's address, ending in `/` so that the catalog's
    /// paths are read under it.
    pub(crate) fn catalog_base(&self) -> Result<url::Url, String> {
        let Some(url) = self.catalog.as_ref().map(|c| c.url.as_str()) else {
            return Err(
                "terrain comes from a catalog: `sources.catalog.url` is required".to_owned(),
            );
        };
        let mut base = url::Url::parse(url).map_err(|e| format!("catalog url {url:?}: {e}"))?;
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        Ok(base)
    }
}

impl LayerSpec {
    /// The grid a template layer's tiles are cut on, as deep as the page
    /// says they go.
    pub(crate) fn tiling(&self) -> tuile_core::raster::TilingScheme {
        let mut scheme = match self.scheme.unwrap_or(Grid::Webmercator) {
            Grid::Webmercator => tuile_core::raster::TilingScheme::web_mercator(),
            Grid::Geographic => tuile_core::raster::TilingScheme::geographic(),
        };
        if let Some(deepest) = self.max_level {
            scheme.maximum_level = deepest;
        }
        scheme
    }
}

/// The asset number of the template layer at `index`.
fn template_asset(index: usize) -> i64 {
    -1 - i64::try_from(index).unwrap_or(i64::MAX - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(json: &str) -> Result<Options, String> {
        serde_json::from_str(json).map_err(|e| e.to_string())
    }

    const CATALOG: &str = r#"{
        "name": "Atlas",
        "profile": "desktop",
        "sources": {
            "catalog": { "url": "https://tiles.test/catalog" },
            "terrain": { "asset": 1 },
            "imagery": [
                { "key": "aerial", "name": "Aerial", "attribution": "© survey", "asset": 2 },
                { "key": "streets", "name": "Streets", "attribution": "© maps",
                  "template": "https://tiles.test/{z}/{x}/{y}.png", "scheme": "webmercator" }
            ]
        }
    }"#;

    /// Catalog layers keep their numbers, template layers get numbers the
    /// catalog never uses, and the worker finds each template by its number.
    #[test]
    fn layers_from_a_catalog_and_from_templates() {
        let o = options(CATALOG).expect("valid");
        let (terrain, layers) = o.sources.choices().expect("valid");
        assert_eq!(terrain.asset, 1);
        assert_eq!(layers[0].asset, 2);
        assert!(layers[1].asset < 0);
        assert_eq!(layers[1].attribution, "© maps");
        let template = o.sources.template_of(layers[1].asset).expect("a template");
        assert_eq!(template.key, "streets");
        assert!(o.sources.template_of(layers[0].asset).is_none());
        assert!(o.sources.template_of(-99).is_none());
        assert!(o.ui, "the controls are on unless the page says otherwise");
        assert_eq!(o.attribution, CaptionSide::BottomRight);
        assert!(o.show_unsupported_message && o.remember_view);
        assert_eq!(o.backend, BackendChoice::Auto);
        assert_eq!(o.profile, ProfileChoice::Desktop);
        assert_eq!(
            template.tiling(),
            tuile_core::raster::TilingScheme::web_mercator()
        );
        assert_eq!(
            o.sources.catalog_base().map(String::from).as_deref(),
            Ok("https://tiles.test/catalog/"),
            "a base the catalog's paths are read under"
        );
    }

    /// Every option the package documents reaches Rust as it was given.
    #[test]
    fn every_option_is_read() {
        let o = options(
            r#"{
            "name": "Atlas", "profile": "handheld", "deviceMemory": 4, "backend": "webgl2",
            "ui": false, "view": { "lon": 6.86, "lat": 45.83, "altitude": 6000 },
            "attribution": "bottom-left", "showUnsupportedMessage": false, "rememberView": false,
            "sources": {
                "catalog": { "url": "https://catalog.test/" },
                "terrain": { "asset": 7 },
                "imagery": [{ "key": "s", "name": "S", "attribution": "©", "template":
                    "https://t.test/{z}/{x}/{y}.jpg", "scheme": "geographic", "maxLevel": 12 }]
            }}"#,
        )
        .expect("valid");
        assert_eq!(o.name, "Atlas");
        assert_eq!(o.profile, ProfileChoice::Handheld);
        assert_eq!(o.device_memory, Some(4.0));
        assert_eq!(o.backend, BackendChoice::Webgl2);
        assert!(!o.ui);
        assert_eq!(o.attribution, CaptionSide::BottomLeft);
        assert!(!o.show_unsupported_message);
        assert!(!o.remember_view);
        let view = o.view.expect("a view").view().expect("in bounds");
        assert_eq!((view.heading, view.pitch), (0.0, 90.0));
        assert_eq!(
            String::from(o.sources.catalog_base().expect("a url")),
            "https://catalog.test/"
        );
        let tiling = o.sources.imagery[0].tiling();
        assert_eq!(
            tiling.projection,
            tuile_core::raster::TilingScheme::geographic().projection
        );
        assert_eq!(tiling.maximum_level, 12);
        assert_eq!(o.sources.imagery[0].scheme, Some(Grid::Geographic));
        assert_ne!(
            BackendChoice::Webgpu.backends(),
            BackendChoice::Webgl2.backends()
        );
    }

    /// What a host can get wrong is refused in a sentence that names it.
    #[test]
    fn a_wrong_source_is_named() {
        let mut o = options(CATALOG).expect("valid");
        o.sources.imagery[0].template = Some("https://x/{z}/{x}/{y}".into());
        assert!(o.sources.choices().unwrap_err().contains("\"aerial\""));
        o.sources.imagery[0].template = None;
        o.sources.imagery[1].template = Some("https://x/tile.png".into());
        assert!(o.sources.choices().unwrap_err().contains("{z}"));
        o.sources.imagery.clear();
        assert!(o.sources.choices().is_err());
        let mut o = options(CATALOG).expect("valid");
        o.sources.catalog = None;
        assert!(o.sources.choices().unwrap_err().contains("catalog"));
        let mut o = options(CATALOG).expect("valid");
        o.sources.imagery[1].key = "aerial".into();
        assert!(o.sources.choices().unwrap_err().contains("twice"));
        assert!(options(
            r#"{"profile":"desktop","sources":{"terrain":{"asset":1},"imagery":[]},"tokn":"x"}"#
        )
        .unwrap_err()
        .contains("tokn"));
    }

    /// A view is held to the flags' bounds; the backend choice maps to the
    /// instance's backends.
    #[test]
    fn views_and_backends() {
        let view = ViewSpec {
            lon: 6.86,
            lat: 45.83,
            altitude: 6000.0,
            heading: 120.0,
            pitch: 25.0,
        };
        assert_eq!(view.view().expect("in bounds").lat, 45.83);
        assert!(ViewSpec { lat: 95.0, ..view }.view().is_err());
        assert_eq!(BackendChoice::Webgl2.backends(), wgpu::Backends::GL);
        assert!(BackendChoice::Auto
            .backends()
            .contains(wgpu::Backends::BROWSER_WEBGPU | wgpu::Backends::GL));
    }
}
