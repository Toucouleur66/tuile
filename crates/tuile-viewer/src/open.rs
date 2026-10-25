// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Sources named by their address alone: a terrain's `layer.json` URL and an
//! imagery URL template. No catalog, no token — what any static file server,
//! CDN or tile server answers.
//!
//! The rest of the viewer speaks in asset numbers ([`crate::embed::Sources`]),
//! so an address is given a number no catalog uses — negative — and the
//! sources that hold the address answer that number. The page's options
//! (`web::options`) and the command line's flags (`start`) both come here.
//!
//! Two grids meet here, and they count rows from opposite ends:
//!
//! - **terrain** is quantized-mesh on the geographic grid, addressed **TMS**:
//!   row 0 along the **south** edge. The `layer.json` says so (`"scheme":
//!   "tms"`); nothing is asked of the person.
//! - **imagery** templates are addressed **XYZ**: row 0 along the **north**
//!   edge, `{z}/{x}/{y}` — on the web-mercator grid (one tile at level 0) or
//!   the geographic one (two), which the person names (`scheme` on a page,
//!   `--imagery-scheme` on the command line).

use tuile_core::raster::TilingScheme;
use tuile_terrain::LayerJson;

#[cfg(not(target_arch = "wasm32"))]
use crate::embed::ImageryChoice;
use crate::embed::TerrainChoice;

/// The number a terrain named by its `layer.json` URL is asked for by.
pub(crate) const URL_TERRAIN: i64 = -1;

/// The number the command line's imagery template is asked for by. A page
/// numbers its templates by their place in its list (`web::options`), the
/// first of which is this one.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) const TEMPLATE_IMAGERY: i64 = -1;

/// A stable name for an address, for a store namespace: two servers never
/// share tiles, and the same server finds its own again in the next session.
///
/// FNV-1a, 64 bits: the standard library's hasher is not promised to give
/// the same answer from one release of the compiler to the next, and a store
/// on disk outlives the binary that wrote it.
pub(crate) fn address_key(address: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in address.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

impl TerrainChoice {
    /// The terrain whose `layer.json` is at `url`, stored under a namespace
    /// of its own.
    pub(crate) fn at_url(url: &str) -> Self {
        Self {
            asset: URL_TERRAIN,
            cache: format!("terrain-url-{}", address_key(url)),
        }
    }

    /// The namespace its tiles are stored under, once its `layer.json` has
    /// been read.
    ///
    /// A catalogued terrain keeps [`TerrainChoice::cache`] as its host named
    /// it: the tiles stored by earlier releases are found again. A terrain at
    /// a URL adds the edition its `layer.json` declares — its `version`, or,
    /// when it has none, a hash of everything it says — so a server that
    /// regenerates a different terrain at the same address does not get the
    /// old one's tiles back from the store.
    ///
    /// What this cannot see: a server that changes its tiles while answering
    /// the same `layer.json` word for word, `version` included. Bumping the
    /// `version` is how such a server says the tiles are new.
    pub(crate) fn namespace_for(&self, layer: &LayerJson) -> String {
        if self.asset != URL_TERRAIN {
            return self.cache.clone();
        }
        let edition = match layer.version.as_deref() {
            Some(version) => format!("v{}", address_key(version)),
            // As read, written back: the fields in the order they are
            // declared, whatever order the server sent them in.
            None => format!(
                "h{}",
                address_key(&serde_json::to_string(layer).unwrap_or_default())
            ),
        };
        format!("{}-{edition}", self.cache)
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl ImageryChoice {
    /// A layer from a URL template, stored under a namespace of its own: two
    /// templates never serve each other's tiles, whatever their keys.
    ///
    /// The namespace is the template and nothing else. A template server
    /// publishes no document to read an edition from, and its tiles' own
    /// validators (`ETag`, `Last-Modified`) would cost a request per stored
    /// tile to compare — the round trip the store is there to save. A server
    /// that draws a different map at the same template says so by changing
    /// the template: a query such as `?v=2`, which it may ignore, is enough.
    pub(crate) fn from_template(
        key: &str,
        name: &str,
        asset: i64,
        attribution: &str,
        template: &str,
    ) -> Self {
        Self::new(key, name, asset, attribution)
            .cached_as(&format!("imagery-template-{}", address_key(template)))
    }
}

/// `text` read as an absolute URL, or the sentence that says why not.
pub(crate) fn absolute_url(what: &str, text: &str) -> Result<url::Url, String> {
    url::Url::parse(text.trim()).map_err(|e| format!("{what} {text:?} is not a URL: {e}"))
}

/// A template's tiles are found by `{z}`, `{x}` and `{y}`: all three, or the
/// sentence that says which is missing.
pub(crate) fn check_template(what: &str, template: &str) -> Result<(), String> {
    for name in ["{z}", "{x}", "{y}"] {
        if !template.contains(name) {
            return Err(format!(
                "{what} {template:?} names no {name}: a template names {{z}}, {{x}} and {{y}}"
            ));
        }
    }
    let probe = template
        .replace("{z}", "0")
        .replace("{x}", "0")
        .replace("{y}", "0");
    absolute_url(what, &probe).map(|_| ())
}

/// The grid a template's tiles are cut on, as deep as it says they go.
pub(crate) fn template_tiling(geographic: bool, max_level: Option<u32>) -> TilingScheme {
    let mut scheme = if geographic {
        TilingScheme::geographic()
    } else {
        TilingScheme::web_mercator()
    };
    if let Some(deepest) = max_level {
        scheme.maximum_level = deepest;
    }
    scheme
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A namespace is the same from one run to the next and differs between
    /// two servers.
    #[test]
    fn an_address_has_one_stable_namespace() {
        let a = TerrainChoice::at_url("http://127.0.0.1:8092/terrain/layer.json");
        assert_eq!(a.asset, URL_TERRAIN);
        assert_eq!(
            a.cache,
            format!(
                "terrain-url-{}",
                address_key("http://127.0.0.1:8092/terrain/layer.json")
            )
        );
        assert_eq!(address_key(""), "cbf29ce484222325", "FNV-1a's offset basis");
        assert_eq!(address_key("a"), "af63dc4c8601ec8c", "FNV-1a 64 of \"a\"");
        assert_ne!(
            a.cache,
            TerrainChoice::at_url("http://127.0.0.1:8093/terrain/layer.json").cache
        );
    }

    /// A `layer.json` as a server would answer it, at `version`.
    fn layer(version: Option<&str>, maxzoom: u32) -> LayerJson {
        let mut layer = LayerJson::from_slice(
            br#"{"format":"quantized-mesh-1.0","tiles":["{z}/{x}/{y}.terrain"]}"#,
        )
        .expect("a layer.json");
        layer.version = version.map(str::to_owned);
        layer.maxzoom = maxzoom;
        layer
    }

    /// A server that regenerates a different terrain at the same URL says so
    /// in its `layer.json` — its `version`, or else its contents — and the
    /// store does not hand back the old one's tiles.
    #[test]
    fn a_terrain_at_a_url_is_stored_by_its_edition() {
        let at = TerrainChoice::at_url("http://127.0.0.1:8092/terrain/layer.json");
        let v1 = at.namespace_for(&layer(Some("1.0.0"), 14));
        assert_eq!(v1, at.namespace_for(&layer(Some("1.0.0"), 14)), "stable");
        assert!(v1.starts_with(&at.cache), "{v1}");
        assert_ne!(v1, at.namespace_for(&layer(Some("2.0.0"), 14)));

        // No version: what the layer.json says, all of it.
        let bare = at.namespace_for(&layer(None, 14));
        assert_eq!(bare, at.namespace_for(&layer(None, 14)), "stable");
        assert_ne!(bare, at.namespace_for(&layer(None, 15)));
        assert_ne!(bare, v1);

        // Pinned: a release that changes these strings throws every stored
        // tile of a URL terrain away.
        assert_eq!(
            at.namespace_for(&layer(Some("1.0.0"), 14)),
            format!("{}-v{}", at.cache, address_key("1.0.0"))
        );
    }

    /// A terrain a catalog names keeps the namespace its host gave it, read
    /// or not: the tiles stored before this release are still found.
    #[test]
    fn a_catalogued_terrain_keeps_its_namespace() {
        let ion = TerrainChoice {
            asset: 1,
            cache: "terrain-1".into(),
        };
        assert_eq!(ion.namespace_for(&layer(Some("1.2.0"), 14)), "terrain-1");
        assert_eq!(ion.namespace_for(&layer(None, 3)), "terrain-1");
    }

    #[test]
    fn a_template_names_its_three_coordinates() {
        assert!(check_template("t", "http://h/{z}/{x}/{y}.png").is_ok());
        assert!(check_template("t", "http://h/{z}/{x}.png")
            .unwrap_err()
            .contains("{y}"));
        assert!(check_template("t", "h/{z}/{x}/{y}.png")
            .unwrap_err()
            .contains("not a URL"));
    }
}
