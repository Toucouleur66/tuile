// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The catalog proxy that holds the token, served in this process, and the
//! application's host asking it.
//!
//! The token is read from `TUILE_ION_TOKEN` and handed to the proxy, and to
//! nothing else: the application asks the proxy, as a page and an Android
//! build do, and never holds it. It is never printed, logged or written —
//! not in a line of output, a URL, a file, a session log or a summary.
//!
//! The proxy keeps each asset's endpoint for most of an hour, so a process
//! opens one catalog session per asset it asks for, however many runs it
//! makes; [`Proxy::sessions`] counts what actually went upstream.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tuile_cesium_ion::proxy::{router, Catalog, Origins, ProxyConfig};
use tuile_cesium_ion::{HttpResponse, IonError, IonHttp, ReqwestHttp};
use tuile_native_fetchers::NativeHttp;
use tuile_viewer::{Identity, ImageryChoice, Sources, TerrainChoice, ViewerHost};
use url::Url;

/// The environment variable the token is read from.
pub const TOKEN: &str = "TUILE_ION_TOKEN";

/// The assets the runs ask for: Cesium World Terrain, and the aerial
/// imagery the application opens on.
const ASSETS: [u64; 2] = [1, 2];

/// What went to the catalog itself, through the proxy.
#[derive(Debug, Default)]
struct Upstream {
    /// Asset endpoints: the catalog's sessions.
    endpoints: AtomicU64,
    /// The imagery service's metadata: its sessions.
    imagery_metadata: AtomicU64,
}

/// The proxy's transport, counting what it asks.
struct Counted {
    inner: ReqwestHttp,
    upstream: Arc<Upstream>,
}

#[async_trait::async_trait]
impl IonHttp for Counted {
    async fn get(&self, url: &Url, bearer: Option<&str>) -> Result<HttpResponse, IonError> {
        let path = url.path();
        if path.contains("/v1/assets/") && path.ends_with("/endpoint") {
            self.upstream.endpoints.fetch_add(1, Ordering::SeqCst);
        }
        if path.contains("/Imagery/Metadata/") {
            self.upstream
                .imagery_metadata
                .fetch_add(1, Ordering::SeqCst);
        }
        self.inner.get(url, bearer).await
    }
}

/// The catalog proxy, served on a loopback port for as long as it lives.
pub struct Proxy {
    catalog: Url,
    upstream: Arc<Upstream>,
    _runtime: tokio::runtime::Runtime,
}

impl Proxy {
    /// Serves the catalog with the token from the environment; says why not
    /// when it cannot, without the token.
    pub fn serve() -> Result<Self, String> {
        let token = std::env::var(TOKEN)
            .ok()
            .filter(|t| !t.trim().is_empty())
            .ok_or_else(|| format!("{TOKEN} is not set: the runs ask the real catalog"))?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("catalog-proxy")
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let upstream = Arc::new(Upstream::default());
        let catalog = Catalog::new(
            Arc::new(Counted {
                inner: ReqwestHttp::default(),
                upstream: Arc::clone(&upstream),
            }),
            ProxyConfig::new(token.trim(), ASSETS),
        );
        let app =
            axum::Router::new().nest("/catalog", router(catalog, None, "/catalog", Origins::Any));
        let listener = runtime
            .block_on(tokio::net::TcpListener::bind(SocketAddr::from((
                [127, 0, 0, 1],
                0,
            ))))
            .map_err(|e| e.to_string())?;
        let address = listener.local_addr().map_err(|e| e.to_string())?;
        runtime.spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self {
            catalog: Url::parse(&format!("http://{address}/catalog/"))
                .map_err(|e| e.to_string())?,
            upstream,
            _runtime: runtime,
        })
    }

    /// The catalog sessions opened so far: asset endpoints, and imagery
    /// metadata.
    pub fn sessions(&self) -> (u64, u64) {
        (
            self.upstream.endpoints.load(Ordering::SeqCst),
            self.upstream.imagery_metadata.load(Ordering::SeqCst),
        )
    }

    /// The application's own host, asking this proxy, with its HTTP cache
    /// in `http_cache`.
    pub fn host(&self, http_cache: PathBuf) -> Arc<dyn ViewerHost> {
        Arc::new(ThroughProxy {
            catalog: self.catalog.clone(),
            http_cache,
        })
    }
}

/// The application's own host — who it is, its terrain and its layers —
/// connected through the proxy.
struct ThroughProxy {
    catalog: Url,
    http_cache: PathBuf,
}

#[async_trait::async_trait]
impl ViewerHost for ThroughProxy {
    fn identity(&self) -> Identity {
        tuile_wgpu_viewer::Public.identity()
    }

    fn terrain(&self) -> TerrainChoice {
        tuile_wgpu_viewer::Public.terrain()
    }

    fn imagery(&self) -> Vec<ImageryChoice> {
        tuile_wgpu_viewer::Public.imagery()
    }

    async fn connect(&self) -> Result<Arc<dyn Sources>, String> {
        let http = NativeHttp::new(&self.http_cache)
            .await
            .map_err(|e| e.to_string())?;
        Ok(tuile_wgpu_viewer::through_catalog(
            Arc::new(http),
            self.catalog.clone(),
        ))
    }
}
