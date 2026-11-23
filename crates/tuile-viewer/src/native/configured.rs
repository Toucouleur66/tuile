// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The host's terrain and imagery, with those a configuration file named in
//! their place (`crate::config_file`): the `terrain` asset and the
//! `[[imagery]]` layers. The token and the API address are the host's to
//! read, at each connection (`tuile_viewer::credential`,
//! `tuile_viewer::embed::configured_api`).
//!
//! A layer named by its URL template is answered here, by the number the
//! file's reading gave it, over the native HTTP stack — with its own token
//! when it takes one (`crate::import::layer_token`), sent under its own
//! header. Everything else is the host's.

use std::sync::Arc;

use tuile_core::fetch::Credential;
use tuile_core::raster::{ImageryProvider, TemplateProvider};
use tuile_native_fetchers::NativeHttp;
use tuile_terrain::{LayerJson, TerrainSource};

use crate::config_file::{IssuedAs, Settings, TemplateLayer};
use crate::embed::{Identity, ImageryChoice, Sources, TerrainChoice, ViewerHost};

/// `inner`, with what `settings` names in place of its own.
pub(crate) struct Configured {
    inner: Arc<dyn ViewerHost>,
    settings: Settings,
}

impl Configured {
    /// `inner` itself when the settings name no terrain and no layer.
    pub(crate) fn wrap(inner: Arc<dyn ViewerHost>, settings: Settings) -> Arc<dyn ViewerHost> {
        if settings.terrain.is_none() && settings.imagery.is_empty() {
            inner
        } else {
            Arc::new(Self { inner, settings })
        }
    }
}

/// The layers a file names, each stored where the host's layer of the same
/// asset is — so a layer the host already had starts as warm as before.
pub(crate) fn layers(host: &[ImageryChoice], named: &[ImageryChoice]) -> Vec<ImageryChoice> {
    named
        .iter()
        .map(|layer| match host.iter().find(|h| h.asset == layer.asset) {
            Some(same) => layer.clone().cached_as(&same.cache),
            None => layer.clone(),
        })
        .collect()
}

#[async_trait::async_trait]
impl ViewerHost for Configured {
    fn identity(&self) -> Identity {
        self.inner.identity()
    }

    fn terrain(&self) -> TerrainChoice {
        let host = self.inner.terrain();
        match self.settings.terrain {
            Some(asset) if asset != host.asset => TerrainChoice {
                asset,
                cache: format!("terrain-{asset}"),
            },
            _ => host,
        }
    }

    fn imagery(&self) -> Vec<ImageryChoice> {
        let host = self.inner.imagery();
        if self.settings.imagery.is_empty() {
            host
        } else {
            layers(&host, &self.settings.imagery)
        }
    }

    async fn connect(&self) -> Result<Arc<dyn Sources>, String> {
        let inner = self.inner.connect().await?;
        if self.settings.templates.is_empty() {
            return Ok(inner);
        }
        let http = NativeHttp::shared().await.map_err(|e| e.to_string())?;
        Ok(Arc::new(Templated {
            inner,
            http,
            templates: self.settings.templates.clone(),
            token: crate::import::layer_token,
        }))
    }
}

/// The host's sources, with the file's template layers answered by their
/// numbers.
struct Templated {
    inner: Arc<dyn Sources>,
    http: NativeHttp,
    templates: Vec<TemplateLayer>,
    /// A layer's token, by its key.
    token: fn(&str) -> Option<crate::config_file::Token>,
}

impl Templated {
    /// The provider of `layer`: its tiles over HTTP, with its credential
    /// when it takes one — or the sentence that says it has none to send.
    fn provider(&self, layer: &TemplateLayer) -> Result<Arc<dyn ImageryProvider>, String> {
        if let (Some(issuer), Some(header)) = (&layer.issuer, &layer.token_header) {
            let url = url::Url::parse(&issuer.url).map_err(|e| e.to_string())?;
            let answer = match &issuer.answer {
                IssuedAs::Cookie(name) => tuile_native_fetchers::IssuedAs::Cookie(name.clone()),
                IssuedAs::Field(name) => tuile_native_fetchers::IssuedAs::Json(name.clone()),
            };
            let issuer = tuile_native_fetchers::CredentialIssuer::new(url, answer)
                .map_err(|e| e.to_string())?;
            let renewing = crate::renew::Renewing::new(
                crate::renew::NativeTransport(self.http.clone()),
                crate::renew::NativeIssuer {
                    issuer,
                    header: header.clone(),
                },
            );
            return Ok(Arc::new(TemplateProvider::new(
                layer.template.clone(),
                Arc::new(renewing),
                layer.tiling(),
            )));
        }
        let Some(header) = &layer.token_header else {
            return Ok(Arc::new(TemplateProvider::new(
                layer.template.clone(),
                Arc::new(self.http.clone()),
                layer.tiling(),
            )));
        };
        let token = (self.token)(&layer.key).ok_or_else(|| {
            format!(
                "the layer {:?} takes a token and none is kept: open its configuration file again",
                layer.key
            )
        })?;
        let fetcher = self
            .http
            .with_credential(Credential::under(header, token.expose()));
        Ok(Arc::new(TemplateProvider::new(
            layer.template.clone(),
            Arc::new(fetcher),
            layer.tiling(),
        )))
    }
}

#[async_trait::async_trait]
impl Sources for Templated {
    async fn terrain(&self, asset: i64) -> Result<(LayerJson, Arc<dyn TerrainSource>), String> {
        self.inner.terrain(asset).await
    }

    async fn imagery(&self, asset: i64) -> Result<Arc<dyn ImageryProvider>, String> {
        match self.templates.iter().find(|t| t.asset == asset) {
            Some(layer) => self.provider(layer),
            None => self.inner.imagery(asset).await,
        }
    }

    async fn tileset(&self, asset: i64) -> Result<tuile_planetary::TilesetSource, String> {
        self.inner.tileset(asset).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuile_core::raster::{ImageryCoord, RasterError};

    const TILES: &str = "tiles.0123456789abcdef.not-a-real-token";

    /// A tile server on a local port that wants `X-Tile-Token: TILES`: tile
    /// 3/1/2 is a WebP picture, every other tile absent (404), and a request
    /// without the token refused (401).
    async fn tile_server() -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let at = listener.local_addr().expect("address");
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([10, 120, 200, 255]));
        let mut webp = Vec::new();
        image::codecs::webp::WebPEncoder::new_lossless(&mut webp)
            .encode(img.as_raw(), 2, 2, image::ExtendedColorType::Rgba8)
            .expect("encode");
        let webp = Arc::new(webp);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let webp = Arc::clone(&webp);
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => request.extend_from_slice(&buf[..n]),
                        }
                    }
                    let request = String::from_utf8_lossy(&request).to_ascii_lowercase();
                    let path = request.split(' ').nth(1).unwrap_or("/").to_owned();
                    let keyed = request.contains(&format!("\r\nx-tile-token: {TILES}\r\n"));
                    let (status, body): (&str, &[u8]) = match (keyed, path.as_str()) {
                        (false, _) => ("401 Unauthorized", b""),
                        (true, "/3/1/2.webp") => ("200 OK", &webp[..]),
                        (true, _) => ("404 Not Found", b""),
                    };
                    let head = format!(
                        "HTTP/1.1 {status}\r\ncontent-type: image/webp\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(body).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        format!("http://{at}/{{z}}/{{x}}/{{y}}.webp")
    }

    /// A server that hands visitors a token in a cookie (`/visit`, counted;
    /// a 503 while `down`) and serves its tiles to the current token only.
    #[derive(Clone, Default)]
    struct Visits {
        generation: Arc<std::sync::atomic::AtomicU64>,
        issued: Arc<std::sync::atomic::AtomicUsize>,
        down: Arc<std::sync::atomic::AtomicBool>,
    }

    async fn visitor_server(visits: Visits) -> String {
        use std::sync::atomic::Ordering;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let at = listener.local_addr().expect("address");
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let visits = visits.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => request.extend_from_slice(&buf[..n]),
                        }
                    }
                    let request = String::from_utf8_lossy(&request).to_ascii_lowercase();
                    let path = request.split(' ').nth(1).unwrap_or("/").to_owned();
                    let current = format!("v{}", visits.generation.load(Ordering::SeqCst));
                    let (status, extra, body) = if path == "/visit" {
                        if visits.down.load(Ordering::SeqCst) {
                            ("503 Service Unavailable", String::new(), &b""[..])
                        } else {
                            visits.issued.fetch_add(1, Ordering::SeqCst);
                            (
                                "200 OK",
                                format!("set-cookie: Visit={current}; Path=/; Max-Age=86399\r\n"),
                                &b"<html>"[..],
                            )
                        }
                    } else if request.contains(&format!("\r\nx-tile-token: {current}\r\n")) {
                        ("200 OK", String::new(), &b"not a picture"[..])
                    } else {
                        ("401 Unauthorized", String::new(), &b""[..])
                    };
                    let head = format!(
                        "HTTP/1.1 {status}\r\n{extra}content-length: {}\r\n\
                         cache-control: no-store\r\nconnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(body).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        format!("http://{at}/")
    }

    /// **A token handed out by its issuer is renewed when it runs out**:
    /// the server refuses the old one, one renewal, the tile again; with the
    /// issuer down a tile fails as a load does — an error, never an
    /// absence — which is what returns a substitute's ground to its source
    /// (the substitution doc, §3.4).
    #[test]
    fn an_issued_token_is_renewed_when_it_runs_out() {
        use std::sync::atomic::Ordering;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("dir");
            let http = NativeHttp::with_config(tuile_native_fetchers::CacheConfig {
                dir: dir.path().to_path_buf(),
                memory_bytes: 1 << 20,
                disk_bytes: 16 << 20,
            })
            .await
            .expect("transport");
            let visits = Visits::default();
            let base = visitor_server(visits.clone()).await;
            let layer = TemplateLayer {
                key: "far".into(),
                asset: crate::config_file::FIRST_TEMPLATE_ASSET,
                template: format!("{base}{{z}}/{{x}}/{{y}}.webp"),
                geographic: false,
                max_level: None,
                token_header: Some("X-Tile-Token".into()),
                issuer: Some(crate::config_file::TokenIssuer {
                    url: format!("{base}visit"),
                    answer: IssuedAs::Cookie("Visit".into()),
                }),
            };
            let sources = Templated {
                inner: Arc::new(Host),
                http,
                templates: vec![layer.clone()],
                token: none_kept,
            };
            let imagery = sources.imagery(layer.asset).await.expect("the layer");
            let raw = |level: u32| {
                let imagery = Arc::clone(&imagery);
                async move {
                    imagery
                        .fetch_tile_bytes(ImageryCoord { level, x: 0, y: 0 })
                        .await
                }
            };
            assert!(raw(1).await.is_ok(), "asked with the issued token");
            assert_eq!(visits.issued.load(Ordering::SeqCst), 1);
            visits.generation.store(1, Ordering::SeqCst);
            assert!(raw(2).await.is_ok(), "renewed, then asked again");
            assert_eq!(visits.issued.load(Ordering::SeqCst), 2);
            visits.generation.store(2, Ordering::SeqCst);
            visits.down.store(true, Ordering::SeqCst);
            let failed = raw(3).await;
            assert!(
                matches!(
                    failed,
                    Err(RasterError::Fetch(
                        tuile_core::fetch::FetchError::Io { .. }
                            | tuile_core::fetch::FetchError::Status { .. }
                    ))
                ),
                "{failed:?}"
            );
        });
    }

    /// The host's sources, which answer nothing.
    struct Host;

    #[async_trait::async_trait]
    impl Sources for Host {
        async fn terrain(
            &self,
            _asset: i64,
        ) -> Result<(LayerJson, Arc<dyn TerrainSource>), String> {
            Err("the host's terrain".into())
        }
        async fn imagery(&self, _asset: i64) -> Result<Arc<dyn ImageryProvider>, String> {
            Err("the host's imagery".into())
        }
    }

    fn kept(key: &str) -> Option<crate::config_file::Token> {
        (key == "far").then(|| crate::config_file::Token::new(TILES))
    }

    fn none_kept(_: &str) -> Option<crate::config_file::Token> {
        None
    }

    /// **A file's template layer is answered by its number, with its token
    /// under its header**: its WebP tile decodes, a tile it does not have
    /// is an absence (404, `NotFound`) — never an error that would suspend
    /// it — and the host answers every other number. Without a kept token
    /// the layer says so by key, without a value.
    #[test]
    fn a_template_layer_is_answered_with_its_token() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        runtime.block_on(async {
            let dir = tempfile::tempdir().expect("dir");
            let http = NativeHttp::with_config(tuile_native_fetchers::CacheConfig {
                dir: dir.path().to_path_buf(),
                memory_bytes: 1 << 20,
                disk_bytes: 16 << 20,
            })
            .await
            .expect("transport");
            let layer = TemplateLayer {
                key: "far".into(),
                asset: crate::config_file::FIRST_TEMPLATE_ASSET,
                template: tile_server().await,
                geographic: false,
                max_level: Some(18),
                token_header: Some("X-Tile-Token".into()),
                issuer: None,
            };
            let sources = Templated {
                inner: Arc::new(Host),
                http: http.clone(),
                templates: vec![layer.clone()],
                token: kept,
            };
            let imagery = sources
                .imagery(layer.asset)
                .await
                .expect("the template layer");
            assert_eq!(imagery.tiling_scheme().maximum_level, 18);
            let tile = imagery
                .fetch_tile(ImageryCoord {
                    level: 3,
                    x: 1,
                    y: 2,
                })
                .await
                .expect("a WebP tile");
            assert_eq!((tile.value.width, tile.value.height), (2, 2));
            let absent = imagery
                .fetch_tile(ImageryCoord {
                    level: 3,
                    x: 0,
                    y: 0,
                })
                .await;
            assert!(
                matches!(
                    absent,
                    Err(RasterError::Fetch(tuile_core::fetch::FetchError::NotFound(
                        _
                    )))
                ),
                "{absent:?}"
            );
            assert_eq!(
                sources.imagery(2).await.err().as_deref(),
                Some("the host's imagery")
            );

            let without = Templated {
                inner: Arc::new(Host),
                http,
                templates: vec![layer.clone()],
                token: none_kept,
            };
            let e = without.imagery(layer.asset).await.err().expect("no token");
            assert!(e.contains("\"far\"") && e.contains("token"), "{e}");
        });
    }

    #[test]
    fn a_named_layer_of_a_known_asset_keeps_its_store() {
        let host = vec![ImageryChoice::new("aerial", "Aerial", 2, "a").cached_as("old-namespace")];
        let named = vec![
            ImageryChoice::new("photo", "Photo", 2, "a"),
            ImageryChoice::new("mine", "Mine", 99, "b"),
        ];
        let out = layers(&host, &named);
        assert_eq!(out[0].cache, "old-namespace");
        assert_eq!(out[0].key, "photo");
        assert_eq!(out[1].cache, "imagery-mine");
    }
}
