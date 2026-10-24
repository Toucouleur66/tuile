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
}

#[cfg(not(target_arch = "wasm32"))]
impl ImageryChoice {
    /// A layer from a URL template, stored under a namespace of its own: two
    /// templates never serve each other's tiles, whatever their keys.
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
