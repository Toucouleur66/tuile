// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#![doc = include_str!("../README.md")]

use async_trait::async_trait;
use bytes::Bytes;
use serde::Deserialize;
use std::sync::Arc;
use tuile_core::fetch::{FetchError, Fetched, TileFetcher};
use tuile_core::raster::{ImageryCoord, ImageryProvider, RasterError, TilingScheme};

/// Metadata of a Bing imagery layer (the parts we need).
#[derive(Debug, Clone)]
pub struct BingMetadata {
    /// Template with `{subdomain}` and `{quadkey}` placeholders.
    pub image_url: String,
    pub subdomains: Vec<String>,
    pub tile_width: u32,
    pub tile_height: u32,
}

/// Which imagery style to ask Bing for, from `TUILE_IMAGERY`.
///
/// **The single place that decides.** It lived in the viewer, and the warm-up
/// job did not know about it: the job filled a cache under one name while the
/// viewer read another, so a warmed session was cold and nothing said so. Two
/// hosts cannot agree on a name by both being careful — they agree by calling
/// the same function.
///
/// `labels` is the one worth knowing about. Labelled imagery is not a prettier
/// picture, it is a **legible** one: aerial ground is smooth and a seam between
/// two tiles can be invisible, while a road that stops at a straight edge or a
/// town name cut in half says exactly where one tile ends and whether its
/// neighbour arrived.
///
/// | `TUILE_IMAGERY` | Bing style |
/// |---|---|
/// | *unset* | whatever the endpoint states, else `Aerial` |
/// | `labels` | `AerialWithLabelsOnDemand` |
/// | `aerial` | `Aerial` |
/// | `road` | `RoadOnDemand` |
/// | anything else | passed through unchanged |
pub fn map_style(from_endpoint: Option<&str>) -> String {
    let Ok(asked) = std::env::var("TUILE_IMAGERY") else {
        return from_endpoint.unwrap_or("Aerial").to_owned();
    };
    match asked.trim().to_ascii_lowercase().as_str() {
        "labels" | "labelled" | "labeled" => "AerialWithLabelsOnDemand",
        "aerial" => "Aerial",
        "road" | "roads" => "RoadOnDemand",
        _ => asked.trim(),
    }
    .to_owned()
}

/// The on-disk cache directory for a style.
///
/// A prefix rather than one shared bucket, because the store is keyed by tile
/// coordinate: aerial and labelled imagery for the same coordinate are the same
/// key. One cache would serve whichever arrived first — silently, and looking
/// exactly like a working session showing the wrong picture. Separate
/// directories also stop the two evicting each other while they are compared.
pub fn cache_name(style: &str) -> String {
    format!("tiles-{}", style.to_lowercase())
}

/// The namespace tiles are filed under *inside* a store.
///
/// Belt and braces with [`cache_name`]: a host that shares one store between
/// several providers still needs its keys not to collide.
pub fn cache_namespace(style: &str) -> String {
    // The suffix is a **generation**, and it exists because the store is keyed
    // by tile coordinate rather than by URL: a change in what we ask Bing for
    // is invisible to it, and every tile already cached keeps being served as
    // it was.
    //
    // Generation 2 is the `n=z` change. Everything cached before it may be
    // Bing's "no imagery" placeholder — a pale square with a crossed-out icon,
    // served at HTTP 200, decoding into a perfectly valid texture that nothing
    // downstream can tell from aerial photography. They are indistinguishable
    // from real tiles in the store too, so the only honest move is to stop
    // reading the whole generation.
    format!("bing2-{}", style.to_lowercase())
}

impl BingMetadata {
    /// The Bing metadata REST URL for a map style + key.
    pub fn metadata_url(base: &str, map_style: &str, key: &str) -> String {
        format!(
            "{}/REST/v1/Imagery/Metadata/{}?incl=ImageryProviders&key={}&uriScheme=https",
            base.trim_end_matches('/'),
            map_style,
            key
        )
    }

    /// The Web Mercator tiling scheme for this layer. Bing serves no level-0
    /// tile (the empty quadkey is invalid), so draping clamps to level 1.
    pub fn tiling_scheme(&self) -> TilingScheme {
        let mut scheme = TilingScheme::web_mercator();
        scheme.tile_size = self.tile_width.max(1);
        scheme.minimum_level = 1;
        scheme
    }

    /// Bing quadkey of a Web Mercator tile: interleave x/y bits, MSB → level 1.
    pub fn quadkey(x: u64, y: u64, level: u32) -> String {
        let mut s = String::with_capacity(level as usize);
        for i in (1..=level).rev() {
            let mask = 1u64 << (i - 1);
            let mut digit = 0u8;
            if x & mask != 0 {
                digit += 1;
            }
            if y & mask != 0 {
                digit += 2;
            }
            s.push((b'0' + digit) as char);
        }
        s
    }

    /// The image URL for a tile: subdomain + quadkey substituted into the
    /// metadata template. Pure (no fetch) — usable from a wasm host that lets
    /// JS do the fetching.
    pub fn tile_url(&self, coord: ImageryCoord) -> String {
        let quadkey = Self::quadkey(coord.x, coord.y, coord.level);
        let subdomain = if self.subdomains.is_empty() {
            ""
        } else {
            let i = (coord.x + coord.y + u64::from(coord.level)) as usize % self.subdomains.len();
            &self.subdomains[i]
        };
        let url = self
            .image_url
            .replace("{subdomain}", subdomain)
            .replace("{quadkey}", &quadkey);
        // `n=z`: answer a tile Bing does not have with a **zero-length body**
        // rather than with a placeholder image.
        //
        // The default is the trap. Bing answers a missing tile with a pale
        // square carrying a "no imagery" icon, at HTTP 200, and it decodes into
        // a perfectly valid texture — so nothing downstream can tell it from
        // aerial photography. It is cached, draped, drawn, and reads on screen
        // as a flat pale rectangle over ground that has real imagery beside it.
        // Every instrument agrees the tile is fine, including
        // `tiles_without_imagery`, because the tile *does* have imagery.
        //
        // An empty body is a miss, and a miss is what makes the drape fall back
        // to the ancestor's picture — coarser, and of the actual ground.
        //
        // The reference implementation sends the same parameter for the same
        // stated reason (`BingMapsImageryProvider.js`, `buildImageResource`).
        let separator = if url.contains('?') { '&' } else { '?' };
        format!("{url}{separator}n=z")
    }

    /// Parses the Bing metadata JSON document.
    pub fn from_json(bytes: &[u8]) -> Result<Self, RasterError> {
        let doc: MetadataDoc =
            serde_json::from_slice(bytes).map_err(|e| RasterError::Image(e.to_string()))?;
        let resource = doc
            .resource_sets
            .into_iter()
            .next()
            .and_then(|rs| rs.resources.into_iter().next())
            .ok_or_else(|| RasterError::Image("Bing metadata has no resources".into()))?;
        Ok(Self {
            image_url: resource.image_url,
            subdomains: resource.image_url_subdomains,
            tile_width: resource.image_width.unwrap_or(256),
            tile_height: resource.image_height.unwrap_or(256),
        })
    }
}

#[derive(Deserialize)]
struct MetadataDoc {
    #[serde(rename = "resourceSets")]
    resource_sets: Vec<ResourceSet>,
}
#[derive(Deserialize)]
struct ResourceSet {
    resources: Vec<Resource>,
}
#[derive(Deserialize)]
struct Resource {
    #[serde(rename = "imageUrl")]
    image_url: String,
    #[serde(rename = "imageUrlSubdomains")]
    image_url_subdomains: Vec<String>,
    #[serde(rename = "imageWidth")]
    image_width: Option<u32>,
    #[serde(rename = "imageHeight")]
    image_height: Option<u32>,
}

/// Bing imagery provider over a [`TileFetcher`].
pub struct BingImageryProvider<F: TileFetcher> {
    fetcher: Arc<F>,
    metadata: BingMetadata,
    scheme: TilingScheme,
}

impl<F: TileFetcher> BingImageryProvider<F> {
    /// Builds from already-fetched metadata.
    pub fn new(fetcher: Arc<F>, metadata: BingMetadata) -> Self {
        let scheme = metadata.tiling_scheme();
        Self {
            fetcher,
            metadata,
            scheme,
        }
    }

    /// Fetches and parses the Bing metadata, then builds the provider.
    pub async fn from_metadata_url(
        fetcher: Arc<F>,
        metadata_url: &str,
    ) -> Result<Self, RasterError> {
        let url = metadata_url
            .parse()
            .map_err(|e: url::ParseError| RasterError::Image(e.to_string()))?;
        let bytes = fetcher.fetch(&url).await?;
        Ok(Self::new(fetcher, BingMetadata::from_json(&bytes)?))
    }

    /// Bing quadkey of a Web Mercator tile (see [`BingMetadata::quadkey`]).
    pub fn quadkey(x: u64, y: u64, level: u32) -> String {
        BingMetadata::quadkey(x, y, level)
    }

    /// Image URL of a tile (see [`BingMetadata::tile_url`]).
    pub fn tile_url(&self, coord: ImageryCoord) -> String {
        self.metadata.tile_url(coord)
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl<F: TileFetcher> ImageryProvider for BingImageryProvider<F> {
    fn tiling_scheme(&self) -> TilingScheme {
        self.scheme
    }

    async fn fetch_tile_bytes(&self, coord: ImageryCoord) -> Result<Fetched<Bytes>, RasterError> {
        let url = self
            .metadata
            .tile_url(coord)
            .parse()
            .map_err(|e: url::ParseError| RasterError::Image(e.to_string()))?;
        let fetched = self.fetcher.fetch_cacheable(&url).await?;
        // An empty body is Bing saying it has no imagery here — see the `n=z`
        // parameter in [`BingMetadata::tile_url`]. It arrives with HTTP 200, so
        // nothing below this line would call it a miss: the decoder would simply
        // fail on zero bytes, the whole tile load would fail with it, and the
        // ground would stay coarse for as long as the camera looked at it while
        // the traversal asked again and again.
        //
        // Named as `NotFound` because that is what it is, and because the drape
        // already knows what to do with one: climb to the ancestor and use its
        // picture of the same ground.
        if is_no_tile(None, &fetched.value) {
            return Err(RasterError::Fetch(FetchError::NotFound(url)));
        }
        Ok(fetched)
    }
}

/// The header Bing marks an answer with when it stands for a tile it does
/// not have: `X-VE-Tile-Info: no-tile`.
pub const TILE_INFO_HEADER: &str = "x-ve-tile-info";

/// Whether an answer Bing gave at HTTP 200 is **no tile**, and not a picture
/// to keep: `tile_info` is the value of [`TILE_INFO_HEADER`] if the answer
/// carried one, `body` what came with it.
///
/// Bing has no imagery everywhere at every level — at sea, past a level it
/// stops — and says so two ways. Asked with `n=z`, as
/// [`BingMetadata::tile_url`] asks, the body is empty. Asked without, the
/// body is a pale square with a crossed-out camera: a valid picture, which
/// nothing after this can tell from aerial photography. Both carry the
/// header. So the header is believed whatever the body, and an empty body
/// whatever the headers.
///
/// Here, once, for every host that fetches Bing's tiles itself instead of
/// through [`BingImageryProvider`]: a host that did not ask this stored the
/// placeholder as tiles, and a film was drawn with them over the sea.
///
/// Nor is the header always there: the placeholder and a uniform black tile
/// were stored without it. Whatever the header, a body
/// [`is_empty_imagery`] recognises is no tile either.
pub fn is_no_tile(tile_info: Option<&str>, body: &[u8]) -> bool {
    tile_info.is_some_and(|info| info.trim().eq_ignore_ascii_case("no-tile"))
        || is_empty_imagery(body)
}

/// A picture Bing answers with where it has no imagery, known by its exact
/// bytes: their length and their FNV-1a digest
/// ([`tuile_core::empty_imagery::digest`], the digest a store's index keeps
/// of each tile).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnownEmpty {
    pub len: usize,
    pub digest: u64,
    /// What it shows.
    pub what: &'static str,
}

/// Every picture Bing is known to answer with in place of imagery.
///
/// Both were found stored as tiles of its aerial layer, at sea, where Bing
/// has no level past 13 or so and these became the finest tiles there were:
/// a film drew the camera icon across a bay, and black water.
pub const KNOWN_EMPTY: &[KnownEmpty] = &[
    // A PNG, wherever it is stored: sha256 45d35034…ab05.
    KnownEmpty {
        len: 1033,
        digest: 0x5cc1_36d7_528c_e891,
        what: "the pale square with a crossed-out camera (\"no imagery\")",
    },
    // A JPEG: sha256 53dc8617…555b.
    KnownEmpty {
        len: 1651,
        digest: 0x7a6f_e998_2816_f01a,
        what: "a uniform black 256 × 256 JPEG",
    },
];

/// Whether `body` is one of Bing's [`KNOWN_EMPTY`] pictures. The digest is
/// taken only of a body of one of their lengths.
pub fn is_placeholder(body: &[u8]) -> bool {
    KNOWN_EMPTY.iter().any(|k| k.len == body.len())
        && is_empty_digest(tuile_core::empty_imagery::digest(body))
}

/// Whether a tile's digest — as a store's index keeps it, with no bytes at
/// hand — is that of one of Bing's [`KNOWN_EMPTY`] pictures.
pub fn is_empty_digest(digest: u64) -> bool {
    KNOWN_EMPTY.iter().any(|k| k.digest == digest)
}

/// Whether an imagery tile's bytes stand for **no imagery**, and are to be
/// kept and read as an absence — the ground standing on the coarser tile
/// above — rather than drawn: an empty body, one of Bing's
/// [`KNOWN_EMPTY`] pictures, or a uniform black image
/// ([`tuile_core::empty_imagery::is_uniform_black`]).
///
/// The one recogniser: what a store refuses to write and what a reader of a
/// store or of the provider refuses to draw. A dark picture is not black:
/// only one whose every texel is.
pub fn is_empty_imagery(body: &[u8]) -> bool {
    body.is_empty() || is_placeholder(body) || tuile_core::empty_imagery::is_uniform_black(body)
}

/// [`is_empty_imagery`], for a tile of a layer whose format is known: a PNG
/// in a JPEG layer is not a picture of that layer either — a source's
/// placeholder, stored where its pictures go.
pub fn is_empty_tile(body: &[u8], jpeg_layer: bool) -> bool {
    (jpeg_layer && tuile_core::empty_imagery::is_png(body)) || is_empty_imagery(body)
}

/// [`is_empty_tile`] without decoding anything: an empty body, a PNG in a
/// JPEG layer, one of the [`KNOWN_EMPTY`] pictures — a length compared, and
/// a digest taken of a body of one of their lengths only.
///
/// For a path that reads what is already stored, where a decode per small
/// tile is not affordable (on a stretch of sea, two tiles in five are small
/// enough to be looked at, at some 300 µs each): what was stored before
/// nothing was refused, as found in the archives, is the known pictures
/// and nothing else; what is stored since went through [`is_empty_tile`].
pub fn is_known_empty_tile(body: &[u8], jpeg_layer: bool) -> bool {
    body.is_empty()
        || (jpeg_layer && tuile_core::empty_imagery::is_png(body))
        || is_placeholder(body)
}

#[cfg(test)]
mod missing_tile_tests {
    use super::tests::MockFetcher;
    use super::*;

    #[test]
    fn an_answer_bing_marks_no_tile_or_leaves_empty_is_no_tile() {
        // As Bing answers a tile it does not have: empty when asked with
        // `n=z`, its placeholder picture when not — the mark on both.
        assert!(is_no_tile(Some("no-tile"), b""));
        assert!(is_no_tile(Some("no-tile"), b"\x89PNG a pale square"));
        assert!(is_no_tile(Some(" No-Tile "), b"\x89PNG a pale square"));
        // An empty body is no tile even from a relay that dropped the header.
        assert!(is_no_tile(None, b""));
        // A tile it has: a body, and no such mark.
        assert!(!is_no_tile(None, b"\xff\xd8\xff a photograph"));
        assert!(!is_no_tile(Some("tile"), b"\xff\xd8\xff a photograph"));
        assert_eq!(TILE_INFO_HEADER, "x-ve-tile-info");
    }

    /// Bing's "no imagery" picture as it was found stored, in a JPEG layer.
    const PLACEHOLDER: &[u8] = include_bytes!("../tests/fixtures/no-imagery-placeholder.png");
    /// Bing's black tile as it was found stored, and is still being answered.
    const BLACK: &[u8] = include_bytes!("../tests/fixtures/black-tile.jpg");

    #[test]
    fn the_pictures_bing_answers_with_no_imagery_are_no_tile_without_a_header() {
        assert_eq!(PLACEHOLDER.len(), 1033);
        assert!(is_placeholder(PLACEHOLDER));
        assert!(is_empty_imagery(PLACEHOLDER));
        assert!(is_no_tile(None, PLACEHOLDER));
        assert!(is_empty_digest(tuile_core::empty_imagery::digest(
            PLACEHOLDER
        )));

        assert_eq!(BLACK.len(), 1651);
        assert!(is_placeholder(BLACK));
        assert!(is_no_tile(None, BLACK));
        assert!(is_empty_digest(tuile_core::empty_imagery::digest(BLACK)));
    }

    #[test]
    fn a_black_tile_bing_has_not_been_seen_answering_is_no_imagery_all_the_same() {
        let black = image::RgbImage::from_pixel(256, 256, image::Rgb([0, 0, 0]));
        let mut jpeg = std::io::Cursor::new(Vec::new());
        black
            .write_to(&mut jpeg, image::ImageFormat::Jpeg)
            .expect("a jpeg");
        let jpeg = jpeg.into_inner();
        assert!(!is_placeholder(&jpeg), "not one of the known bytes");
        assert!(is_empty_imagery(&jpeg));
        assert!(is_no_tile(None, &jpeg));
    }

    #[test]
    fn a_picture_of_the_ground_is_kept_however_dark() {
        // Dark water, with the texture of water: not one colour.
        let dark = image::RgbImage::from_fn(256, 256, |x, y| {
            let n = ((x * 7 + y * 13) % 9) as u8;
            image::Rgb([n, n + 2, n + 5])
        });
        let mut jpeg = std::io::Cursor::new(Vec::new());
        dark.write_to(&mut jpeg, image::ImageFormat::Jpeg)
            .expect("a jpeg");
        let jpeg = jpeg.into_inner();
        assert!(!is_empty_imagery(&jpeg));
        assert!(!is_empty_tile(&jpeg, true));
        assert!(!is_no_tile(None, &jpeg));
    }

    #[test]
    fn a_png_in_a_jpeg_layer_is_not_a_tile_of_it() {
        let flat = image::RgbaImage::from_pixel(256, 256, image::Rgba([240, 240, 235, 255]));
        let mut png = std::io::Cursor::new(Vec::new());
        flat.write_to(&mut png, image::ImageFormat::Png)
            .expect("a png");
        let png = png.into_inner();
        assert!(is_empty_tile(&png, true));
        assert!(!is_empty_tile(&png, false), "a PNG layer's own tile");
        assert!(is_empty_tile(PLACEHOLDER, false), "known wherever it is");
        assert!(is_known_empty_tile(&png, true));
        assert!(!is_known_empty_tile(&png, false));
    }

    #[test]
    fn what_is_known_needs_no_decode_and_what_is_not_is_left_to_the_writer() {
        assert!(is_known_empty_tile(PLACEHOLDER, false));
        assert!(is_known_empty_tile(BLACK, true));
        assert!(is_known_empty_tile(b"", true));
        // A black tile of other bytes is told by its pixels only: not here.
        let black = image::RgbImage::from_pixel(256, 256, image::Rgb([0, 0, 0]));
        let mut jpeg = std::io::Cursor::new(Vec::new());
        black
            .write_to(&mut jpeg, image::ImageFormat::Jpeg)
            .expect("a jpeg");
        assert!(!is_known_empty_tile(jpeg.get_ref(), true));
        assert!(is_empty_tile(jpeg.get_ref(), true));
    }

    fn metadata(template: &str) -> BingMetadata {
        BingMetadata {
            image_url: template.into(),
            subdomains: vec!["t0".into()],
            tile_width: 256,
            tile_height: 256,
        }
    }

    /// **Every tile request asks for a zero-length response where Bing has no
    /// imagery, rather than for a picture of a crossed-out camera.**
    ///
    /// Bing's default is to answer a tile it does not have with a *placeholder
    /// image* — a pale square carrying a "no imagery" icon. It arrives with HTTP
    /// 200, it decodes, and it is a perfectly valid texture, so nothing
    /// downstream can tell it from aerial photography: it is draped, drawn, and
    /// reads on screen as a flat pale rectangle over ground that has real
    /// imagery beside it. `tiles_without_imagery` stays at zero throughout,
    /// because the tile *does* have imagery. It has that.
    ///
    /// `n=z` asks for a zero-length body instead, which the drape treats as "no
    /// tile at this level" and answers with the ancestor's imagery — coarser,
    /// and a picture of the actual ground. The reference implementation sends
    /// exactly this parameter, and says why in the same words:
    /// "this parameter tells the Bing servers to send a zero-length response
    /// instead of a placeholder image for missing tiles"
    /// (`BingMapsImageryProvider.js`, `buildImageResource`).
    /// **An empty body is a miss, not a decode failure.**
    ///
    /// The other half of `n=z`, and useless without it. Bing answers a tile it
    /// does not have with HTTP **200** and zero bytes, so nothing between the
    /// socket and the drape would call it missing on its own: the decoder would
    /// fail on an empty buffer, the failure would take the whole geometry tile
    /// down with it, and the ground would stay coarse for as long as the camera
    /// looked at it — while the traversal asked for the same tile again every
    /// pass.
    ///
    /// `NotFound` is what the drape already knows how to answer: climb to the
    /// ancestor and drape its picture of the same ground, which is exactly the
    /// behaviour that was wanted.
    #[test]
    fn an_empty_body_is_reported_as_a_missing_tile() {
        let fetcher = Arc::new(MockFetcher::default());
        let coord = ImageryCoord {
            level: 12,
            x: 2100,
            y: 1500,
        };
        let meta = metadata("https://example.test/{subdomain}/{quadkey}.jpeg");
        fetcher.put(&meta.tile_url(coord), Vec::new());
        let meta = metadata("https://example.test/{subdomain}/{quadkey}.jpeg");
        let provider = BingImageryProvider::new(Arc::clone(&fetcher), meta);

        let got = futures_executor::block_on(provider.fetch_tile_bytes(coord));
        assert!(
            matches!(got, Err(RasterError::Fetch(FetchError::NotFound(_)))),
            "an empty Bing body must read as a missing tile so the drape \
             climbs to the ancestor; got {got:?}"
        );
    }

    #[test]
    fn a_tile_request_asks_for_no_placeholder() {
        // Bing's own template already carries a query string; a second one
        // would make the whole parameter silently ignored.
        for template in [
            "https://ecn.{subdomain}.tiles.virtualearth.net/tiles/a{quadkey}.jpeg?g=1&mkt=en-GB",
            "https://example.test/{subdomain}/{quadkey}.jpeg",
        ] {
            let url = metadata(template).tile_url(ImageryCoord {
                level: 12,
                x: 2100,
                y: 1500,
            });
            assert!(
                url.contains("n=z"),
                "{url} would be answered with Bing's placeholder image, which is \
                 indistinguishable from real imagery once decoded"
            );
            assert_eq!(
                url.matches('?').count(),
                1,
                "{url} has two query strings, so the parameter is not read"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use tuile_core::fetch::FetchError;
    use url::Url;

    #[derive(Default)]
    pub(super) struct MockFetcher {
        files: Mutex<HashMap<String, Vec<u8>>>,
        log: Mutex<Vec<String>>,
    }
    impl MockFetcher {
        pub(super) fn put(&self, url: &str, bytes: Vec<u8>) {
            self.files
                .lock()
                .expect("lock")
                .insert(url.to_owned(), bytes);
        }
    }
    #[cfg_attr(not(target_arch = "wasm32"), async_trait)]
    #[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
    impl TileFetcher for MockFetcher {
        async fn fetch(&self, url: &Url) -> Result<Bytes, FetchError> {
            self.log.lock().expect("lock").push(url.to_string());
            self.files
                .lock()
                .expect("lock")
                .get(url.as_str())
                .map(|b| Bytes::from(b.clone()))
                .ok_or_else(|| FetchError::NotFound(url.clone()))
        }
    }

    fn png_2x2() -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([10, 20, 30, 255]));
        let mut buf = std::io::Cursor::new(Vec::new());
        img.write_to(&mut buf, image::ImageFormat::Png)
            .expect("encode");
        buf.into_inner()
    }

    #[test]
    fn quadkey_matches_bing_reference() {
        // Verified against the live Bing tile fetched for the Alps:
        // (x=541, y=364, z=10) → "1202213301".
        assert_eq!(
            BingImageryProvider::<MockFetcher>::quadkey(541, 364, 10),
            "1202213301"
        );
        // Canonical examples.
        assert_eq!(BingImageryProvider::<MockFetcher>::quadkey(0, 0, 1), "0");
        assert_eq!(BingImageryProvider::<MockFetcher>::quadkey(1, 0, 1), "1");
        assert_eq!(BingImageryProvider::<MockFetcher>::quadkey(0, 1, 1), "2");
        assert_eq!(BingImageryProvider::<MockFetcher>::quadkey(1, 1, 1), "3");
        assert_eq!(BingImageryProvider::<MockFetcher>::quadkey(3, 5, 3), "213");
    }

    #[test]
    fn metadata_parses_real_shape() {
        let json = r#"{ "resourceSets": [{ "resources": [{
            "imageUrl": "https://ecn.{subdomain}.tiles.virtualearth.net/tiles/a{quadkey}.jpeg?g=1",
            "imageUrlSubdomains": ["t0","t1","t2","t3"],
            "imageWidth": 256, "imageHeight": 256 }]}]}"#;
        let m = BingMetadata::from_json(json.as_bytes()).expect("parse");
        assert_eq!(m.subdomains.len(), 4);
        assert!(m.image_url.contains("{quadkey}"));
    }

    #[test]
    fn builds_tile_url_and_fetches_decoded_texture() {
        let meta = BingMetadata {
            image_url: "https://ecn.{subdomain}.tiles.virtualearth.net/tiles/a{quadkey}.jpeg"
                .into(),
            subdomains: vec!["t0".into(), "t1".into(), "t2".into(), "t3".into()],
            tile_width: 256,
            tile_height: 256,
        };
        let fetcher = Arc::new(MockFetcher::default());
        let coord = ImageryCoord {
            level: 10,
            x: 541,
            y: 364,
        };
        let provider = BingImageryProvider::new(Arc::clone(&fetcher), meta);
        let url = provider.tile_url(coord);
        // subdomain index = (541+364+10) % 4 = 915 % 4 = 3 → "t3"; quadkey as above.
        assert_eq!(
            url,
            "https://ecn.t3.tiles.virtualearth.net/tiles/a1202213301.jpeg?n=z"
        );
        fetcher.put(&url, png_2x2());

        let tex = futures_executor::block_on(provider.fetch_tile(coord)).expect("fetch");
        assert_eq!((tex.value.width, tex.value.height), (2, 2));
        assert_eq!(&tex.value.rgba8().unwrap()[0..4], &[10, 20, 30, 255]);
    }
}
