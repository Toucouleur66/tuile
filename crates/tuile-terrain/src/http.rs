// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Terrain from any `layer.json` URL, over plain HTTP.
//!
//! The `layer.json` is fetched once; its `tiles` template is resolved
//! against the document's own address ([`LayerJson::tile_request_url`], the
//! one tile-address builder every HTTP terrain source shares), with the
//! extensions the layer offers asked for in the `extensions=` query. No
//! token, no account: what a static file server, a CDN or a tile server
//! answers. The bytes travel through the core's [`TileFetcher`], so the same
//! source runs natively and in a browser.

use std::sync::Arc;

use tuile_core::fetch::{FetchError, Fetched, TileFetcher};
use url::Url;

use crate::layer::{LayerError, LayerJson, TileUrlError};
use crate::source::{TerrainSource, TerrainSourceError};
use crate::tiling::TileCoord;

/// Why a terrain could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum HttpTerrainError {
    #[error("layer.json: {0}")]
    Fetch(#[from] FetchError),
    #[error(transparent)]
    Layer(#[from] LayerError),
    #[error("{url} is not quantized-mesh terrain (format {format:?})")]
    NotQuantizedMesh { url: Url, format: String },
}

/// Quantized-mesh terrain served at a `layer.json` URL.
pub struct HttpTerrainSource<F: ?Sized> {
    fetcher: Arc<F>,
    layer_url: Url,
    layer: LayerJson,
}

impl<F: TileFetcher + ?Sized> HttpTerrainSource<F> {
    /// Fetches and reads the `layer.json` at `layer_url`.
    pub async fn open(fetcher: Arc<F>, layer_url: Url) -> Result<Self, HttpTerrainError> {
        let bytes = fetcher.fetch(&layer_url).await?;
        let layer = LayerJson::from_slice(&bytes)?;
        if !layer.is_quantized_mesh() {
            return Err(HttpTerrainError::NotQuantizedMesh {
                url: layer_url,
                format: layer.format,
            });
        }
        Ok(Self::with_layer(fetcher, layer_url, layer))
    }

    /// A terrain whose `layer.json`, found at `layer_url`, is already read.
    pub fn with_layer(fetcher: Arc<F>, layer_url: Url, layer: LayerJson) -> Self {
        Self {
            fetcher,
            layer_url,
            layer,
        }
    }

    /// The terrain's `layer.json`: its levels, its availability.
    pub fn layer(&self) -> &LayerJson {
        &self.layer
    }

    /// Where tile `coord` is asked for.
    pub fn tile_url(&self, coord: TileCoord) -> Result<Url, TileUrlError> {
        self.layer.tile_request_url(&self.layer_url, coord)
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl<F: TileFetcher + ?Sized> TerrainSource for HttpTerrainSource<F> {
    async fn fetch_tile(&self, coord: TileCoord) -> Result<Fetched<Vec<u8>>, TerrainSourceError> {
        let url = self
            .tile_url(coord)
            .map_err(|e| TerrainSourceError::failed(e.to_string()))?;
        match self.fetcher.fetch_cacheable(&url).await {
            Ok(fetched) => Ok(fetched.map(|bytes| bytes.to_vec())),
            // The server's statement that the tile does not exist: worth
            // remembering. Anything else is about this moment, never cached.
            Err(e @ (FetchError::NotFound(_) | FetchError::Status { status: 404, .. })) => {
                Err(TerrainSourceError::no_such_tile(e.to_string()))
            }
            Err(e) => Err(TerrainSourceError::failed(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heightfield::{Grid, HeightfieldSource};
    use crate::tiling::GeoRect;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tuile_core::fetch::HttpFetcher;

    type Handler = dyn Fn(&str) -> (u16, Vec<u8>) + Send + Sync;

    /// A server on a local port, answering each request path (query
    /// included) as `handler` says, and remembering the paths asked for.
    async fn serve(handler: Arc<Handler>) -> (Url, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let at = listener.local_addr().expect("address");
        let asked = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&asked);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let handler = Arc::clone(&handler);
                let log = Arc::clone(&log);
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => request.extend_from_slice(&buf[..n]),
                        }
                    }
                    let request = String::from_utf8_lossy(&request);
                    let path = request.split(' ').nth(1).unwrap_or("/").to_string();
                    log.lock().expect("log").push(path.clone());
                    let (status, body) = handler(&path);
                    let reason = match status {
                        200 => "OK",
                        404 => "Not Found",
                        _ => "Internal Server Error",
                    };
                    let head = format!(
                        "HTTP/1.1 {status} {reason}\r\ncontent-length: {}\r\n\
                         connection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(&body).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        (Url::parse(&format!("http://{at}/")).expect("url"), asked)
    }

    const LAYER: &str = r#"{
      "format": "quantized-mesh-1.0", "version": "1.0.0", "scheme": "tms",
      "tiles": ["{z}/{x}/{y}.terrain?v={version}"],
      "maxzoom": 5, "extensions": ["octvertexnormals"],
      "available": [[{ "startX": 0, "startY": 0, "endX": 1, "endY": 0 }]]
    }"#;

    /// The layer is read from its URL; a tile is asked for beside it, its
    /// extensions in the query, and its bytes handed over as served.
    #[tokio::test(flavor = "current_thread")]
    async fn a_tile_is_fetched_beside_its_layer_with_its_extensions() {
        let (base, asked) = serve(Arc::new(|path: &str| match path {
            "/terrain/layer.json" => (200, LAYER.as_bytes().to_vec()),
            "/terrain/0/1/0.terrain?v=1.0.0&extensions=octvertexnormals" => {
                (200, b"a tile".to_vec())
            }
            "/terrain/5/3/2.terrain?v=1.0.0&extensions=octvertexnormals" => (500, Vec::new()),
            _ => (404, Vec::new()),
        }))
        .await;
        let layer_url = base.join("terrain/layer.json").expect("url");
        let source = HttpTerrainSource::open(Arc::new(HttpFetcher::default()), layer_url)
            .await
            .expect("open");
        assert_eq!(source.layer().maxzoom, 5);

        let tile = source
            .fetch_tile(TileCoord::new(0, 1, 0))
            .await
            .expect("tile");
        assert_eq!(tile.value, b"a tile");

        // 404: the tile does not exist — an absence, remembered.
        let missing = source
            .fetch_tile(TileCoord::new(4, 0, 0))
            .await
            .expect_err("404");
        assert!(missing.is_absence(), "{missing}");
        // 500: a failure of the moment, never cached as an absence.
        let failed = source
            .fetch_tile(TileCoord::new(5, 3, 2))
            .await
            .expect_err("500");
        assert!(!failed.is_absence(), "{failed}");

        let asked = asked.lock().expect("log").clone();
        assert_eq!(asked[0], "/terrain/layer.json");
        assert!(asked[1..]
            .iter()
            .all(|p| p.contains("extensions=octvertexnormals")));
    }

    /// A server that does not answer at all is a failure, not an absence;
    /// a layer that is not there cannot be opened.
    #[tokio::test(flavor = "current_thread")]
    async fn no_server_is_a_failure_and_no_layer_cannot_be_opened() {
        let (base, _) = serve(Arc::new(|_: &str| (404, Vec::new()))).await;
        let fetcher = Arc::new(HttpFetcher::default());
        let missing =
            HttpTerrainSource::open(Arc::clone(&fetcher), base.join("layer.json").expect("url"))
                .await;
        assert!(matches!(
            missing,
            Err(HttpTerrainError::Fetch(FetchError::NotFound(_)))
        ));

        // A port nobody listens on.
        let closed = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
            listener.local_addr().expect("address")
        };
        let layer = LayerJson::from_slice(LAYER.as_bytes()).expect("layer");
        let source = HttpTerrainSource::with_layer(
            fetcher,
            Url::parse(&format!("http://{closed}/layer.json")).expect("url"),
            layer,
        );
        let e = source
            .fetch_tile(TileCoord::new(0, 0, 0))
            .await
            .expect_err("nobody there");
        assert!(!e.is_absence(), "{e}");
    }

    /// A relief served as a server would serve it — its layer, its tiles —
    /// read back through the HTTP source: the bytes are the relief's.
    #[tokio::test(flavor = "current_thread")]
    async fn a_heightfield_served_over_http_reads_back_the_same() {
        let rect = GeoRect::from_degrees(0.01, -0.072, 0.154, 0.072);
        let grid = Grid::new(
            3,
            3,
            vec![10.0, 20.0, 30.0, 40.0, 50.0, 60.0, 70.0, 80.0, 90.0],
            rect,
        )
        .expect("grid");
        let relief = Arc::new(HeightfieldSource::new(grid, 12));
        let layer = relief.layer().to_json();
        let served = Arc::clone(&relief);
        let (base, _) = serve(Arc::new(move |path: &str| {
            if path == "/layer.json" {
                return (200, layer.clone());
            }
            // `/{z}/{x}/{y}.terrain?extensions=…`
            let tile = path.trim_start_matches('/').split('?').next().unwrap_or("");
            let parts: Vec<u64> = tile
                .trim_end_matches(".terrain")
                .split('/')
                .filter_map(|p| p.parse().ok())
                .collect();
            match parts[..] {
                [z, x, y] => match served.tile(TileCoord::new(z as u32, x, y)) {
                    Ok(bytes) => (200, bytes),
                    Err(_) => (404, Vec::new()),
                },
                _ => (404, Vec::new()),
            }
        }))
        .await;
        let source = HttpTerrainSource::open(
            Arc::new(HttpFetcher::default()),
            base.join("layer.json").expect("url"),
        )
        .await
        .expect("open");
        assert_eq!(source.layer(), &relief.layer());
        let coord = relief.tiles().last().expect("a tile");
        let fetched = source.fetch_tile(coord).await.expect("tile");
        assert_eq!(fetched.value, relief.tile(coord).expect("tile"));
        let below = TileCoord::new(13, coord.x * 2, coord.y * 2);
        assert!(source
            .fetch_tile(below)
            .await
            .expect_err("none")
            .is_absence());
    }
}
