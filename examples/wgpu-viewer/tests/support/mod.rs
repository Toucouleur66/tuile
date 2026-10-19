// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What the headless runs share: the catalog proxy that holds the token, the
//! host that asks it, one run at a time, a directory a run keeps its store
//! in, and the obligations a frame is held to.
//!
//! # The token
//!
//! Read from `TUILE_ION_TOKEN` in this process's environment, handed to the
//! catalog proxy served here on a loopback port, and to nothing else: the
//! application asks the proxy, as a page and an Android build do, and never
//! holds it. It is never printed, logged or written — not in a log line, a
//! URL, a file, a session log or a test's output.
//!
//! # Sessions
//!
//! The proxy keeps each asset's endpoint for most of an hour, so a test
//! process opens one catalog session per asset it asks for — terrain and the
//! opening imagery — however many runs it holds. [`sessions`] counts what
//! actually went upstream.

#![allow(dead_code, reason = "each test uses its own part of this")]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use tuile_cesium_ion::proxy::{router, Catalog, Origins, ProxyConfig};
use tuile_cesium_ion::{HttpResponse, IonError, IonHttp, ReqwestHttp};
use tuile_native_fetchers::NativeHttp;
use tuile_viewer::control::{Headless, Refused, Start, State};
use tuile_viewer::{Identity, ImageryChoice, Sources, TerrainChoice, ViewerHost};
use url::Url;

/// The environment variable the token is read from.
pub const TOKEN: &str = "TUILE_ION_TOKEN";

/// The assets the runs ask for: Cesium World Terrain, and the aerial
/// imagery the application opens on.
const ASSETS: [u64; 2] = [1, 2];

/// What went to the catalog itself, through the proxy.
#[derive(Debug, Default)]
pub struct Upstream {
    /// Asset endpoints: the catalog's sessions.
    pub endpoints: AtomicU64,
    /// The imagery service's metadata: its sessions.
    pub imagery_metadata: AtomicU64,
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

/// The catalog proxy, served in this process for as long as it lives.
struct Proxy {
    catalog: Url,
    upstream: Arc<Upstream>,
    _runtime: tokio::runtime::Runtime,
}

fn proxy() -> &'static Proxy {
    static PROXY: OnceLock<Proxy> = OnceLock::new();
    PROXY.get_or_init(|| {
        #[allow(clippy::panic, reason = "a run without its catalog cannot start")]
        let token = std::env::var(TOKEN)
            .ok()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| {
                panic!("{TOKEN} is not set: the headless runs ask the real catalog")
            });
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("catalog-proxy")
            .enable_all()
            .build()
            .expect("a runtime for the proxy");
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
            .expect("a loopback port for the proxy");
        let address = listener.local_addr().expect("the proxy's address");
        runtime.spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Proxy {
            catalog: Url::parse(&format!("http://{address}/catalog/"))
                .expect("the proxy's catalog"),
            upstream,
            _runtime: runtime,
        }
    })
}

/// The catalog sessions this process has opened so far: asset endpoints,
/// and imagery metadata.
pub fn sessions() -> (u64, u64) {
    let up = &proxy().upstream;
    (
        up.endpoints.load(Ordering::SeqCst),
        up.imagery_metadata.load(Ordering::SeqCst),
    )
}

/// The application's own host — who it is, its terrain and its layers —
/// connected through the proxy, with its HTTP cache in the run's directory.
pub struct ThroughProxy {
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
            proxy().catalog.clone(),
        ))
    }
}

/// Where a run keeps what it keeps between launches: its tile store and its
/// HTTP cache. A new one is a cold start; reused, a warm one. Removed with
/// it.
pub struct Place {
    dir: tempfile::TempDir,
}

impl Place {
    pub fn new() -> Self {
        Self {
            dir: tempfile::Builder::new()
                .prefix("tuile-headless-")
                .tempdir()
                .expect("a directory for the run"),
        }
    }

    pub fn host(&self) -> Arc<dyn ViewerHost> {
        Arc::new(ThroughProxy {
            http_cache: self.dir.path().join("http"),
        })
    }

    pub fn store(&self) -> PathBuf {
        self.dir.path().join("tiles")
    }

    /// A run's start from this place.
    pub fn start(&self) -> Start {
        Start::new(self.host(), self.store())
    }
}

/// Where a run's session log is kept after the run: the build's own
/// scratch directory, so a red run's log is there to open and to fly again.
pub fn log_for(name: &str) -> PathBuf {
    let slug: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("headless");
    std::fs::create_dir_all(&dir).expect("a directory for the logs");
    dir.join(format!("{slug}.mcap"))
}

/// The runs' numbers are the process's: one run at a time.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

/// A run, and the turn it holds.
pub struct Run {
    pub run: Headless,
    pub turn: MutexGuard<'static, ()>,
}

impl std::ops::Deref for Run {
    type Target = Headless;
    fn deref(&self) -> &Headless {
        &self.run
    }
}

impl std::ops::DerefMut for Run {
    fn deref_mut(&mut self) -> &mut Headless {
        &mut self.run
    }
}

/// Waits for the turn and starts a run from `start`.
pub fn start(start: Start) -> Run {
    let turn = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    // The proxy first, so the run's first ask finds it.
    let _ = proxy();
    match Headless::start(start) {
        Ok(run) => Run { run, turn },
        #[allow(clippy::panic, reason = "a run that cannot start fails its test")]
        Err(Refused::NoGpu(why)) => panic!("no GPU to draw with: {why}"),
        #[allow(clippy::panic, reason = "a run that cannot start fails its test")]
        Err(why) => panic!("the run did not start: {why}"),
    }
}

/// Starts the run again over its store: the application killed and opened
/// again. The turn is kept.
pub fn relaunch(run: Run, log: Option<PathBuf>) -> Run {
    let Run { run, turn } = run;
    Run {
        run: run.relaunch(log).expect("the run starts again"),
        turn,
    }
}

/// What a stretch of a run came to, against the specification's per-frame
/// obligations — the numbers a red test reports, so that its baseline is in
/// its failure.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Report {
    pub seconds: f64,
    pub frames: usize,
    pub drawn: usize,
    /// The longest a frame took, seconds, and the median.
    pub slowest_frame: f64,
    pub median_frame: f64,
    /// Frames with ground left the clear colour, in any eye; the worst count
    /// of such pixels; the first second it happened at.
    pub black_frames: usize,
    pub worst_black: u64,
    pub first_black: Option<f64>,
    /// Frames with selected ground drawn by nothing (`frame_lost`); the most
    /// tiles; the first second.
    pub lost_frames: usize,
    pub worst_lost: u64,
    pub first_lost: Option<f64>,
    /// Frames drawn with coarser ancestors standing in; the most tiles.
    pub coarser_frames: usize,
    pub worst_coarser: u64,
    /// The most decoded bytes the server held, and the frames over the
    /// profile's budget.
    pub worst_resident_bytes: u64,
    pub over_budget_frames: usize,
    /// The most GPU bytes held, meshes and imagery.
    pub worst_gpu_bytes: u64,
    /// Origin fetches and store hits over the stretch.
    pub store_misses: u64,
    pub store_hits: u64,
}

/// Reads `states` against the obligations, with `budget` the profile's
/// resident bytes.
pub fn report(states: &[State], budget: u64) -> Report {
    let mut r = Report {
        frames: states.len(),
        ..Report::default()
    };
    let (Some(first), Some(last)) = (states.first(), states.last()) else {
        return r;
    };
    r.seconds = last.seconds - first.seconds + first.took;
    r.store_misses = last.metrics.store_misses - first.metrics.store_misses;
    r.store_hits = last.metrics.store_hits - first.metrics.store_hits;
    let mut took: Vec<f64> = states.iter().filter(|s| s.drew).map(|s| s.took).collect();
    took.sort_by(f64::total_cmp);
    r.slowest_frame = took.last().copied().unwrap_or(0.0);
    r.median_frame = took.get(took.len() / 2).copied().unwrap_or(0.0);
    for s in states {
        let m = &s.metrics;
        r.worst_resident_bytes = r.worst_resident_bytes.max(m.resident_bytes);
        r.worst_gpu_bytes = r.worst_gpu_bytes.max(m.mesh_bytes + m.texture_bytes);
        if m.resident_bytes > budget {
            r.over_budget_frames += 1;
        }
        if !s.drew {
            continue;
        }
        r.drawn += 1;
        let black = s.black();
        if black > 0 {
            r.black_frames += 1;
            r.worst_black = r.worst_black.max(black);
            r.first_black.get_or_insert(s.seconds);
        }
        if m.frame_lost > 0 {
            r.lost_frames += 1;
            r.worst_lost = r.worst_lost.max(m.frame_lost);
            r.first_lost.get_or_insert(s.seconds);
        }
        if m.frame_coarser > 0 {
            r.coarser_frames += 1;
            r.worst_coarser = r.worst_coarser.max(m.frame_coarser);
        }
    }
    r
}
