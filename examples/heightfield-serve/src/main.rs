// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A synthetic relief served as quantized-mesh terrain and XYZ imagery over
//! plain HTTP, for `tuile-viewer` to show with no catalog and no token.
//!
//! | Route | What |
//! |---|---|
//! | `/terrain/layer.json` | the terrain's description: TMS, octvertexnormals, the levels available |
//! | `/terrain/{z}/{x}/{y}.terrain` | a quantized-mesh tile, TMS (row 0 in the south); the ellipsoid outside the map, 404 deeper than its levels |
//! | `/imagery/{z}/{x}/{y}.png` | the relief coloured by height, geographic grid, XYZ (row 0 in the north) |
//!
//! Flags: `--listen ADDR` (default `127.0.0.1:8092`), `--tls-cert PEM
//! --tls-key PEM` (HTTPS, for a phone whose page is HTTPS), `--public-url
//! URL` (the address the printed commands use, for a server reached from
//! another machine). See the README.

mod relief;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use axum::extract::{Path, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use tower_http::cors::CorsLayer;
use tuile_terrain::{Grid, HeightfieldSource, TileCoord};

/// The deepest terrain level: 77 vertices about 8 m apart at the equator,
/// the grid's own step. A viewer goes deeper by cutting these tiles.
const MAX_LEVEL: u32 = 15;

/// What every request reads.
struct Served {
    terrain: HeightfieldSource<Arc<Grid>>,
    layer_json: Vec<u8>,
    heights: Arc<Grid>,
}

type Shared = Arc<Served>;

fn served() -> Served {
    let heights = Arc::new(relief::grid());
    let terrain = HeightfieldSource::new(Arc::clone(&heights), MAX_LEVEL);
    let layer_json = terrain.layer().to_json();
    Served {
        terrain,
        layer_json,
        heights,
    }
}

/// The routes, with `Access-Control-Allow-Origin: *` on every answer: the
/// viewer's page comes from another origin than its tiles.
fn router(served: Shared) -> Router {
    Router::new()
        .route("/terrain/layer.json", get(layer_json))
        .route("/terrain/{z}/{x}/{file}", get(terrain_tile))
        .route("/imagery/{z}/{x}/{file}", get(imagery_tile))
        .layer(CorsLayer::permissive())
        .with_state(served)
}

async fn layer_json(State(served): State<Shared>) -> Response {
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )],
        served.layer_json.clone(),
    )
        .into_response()
}

/// `{y}.{extension}`'s row, when the extension is the one asked for.
fn row(file: &str, extension: &str) -> Option<u64> {
    file.strip_suffix(extension)?
        .strip_suffix('.')?
        .parse()
        .ok()
}

fn not_found() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

async fn terrain_tile(
    State(served): State<Shared>,
    Path((z, x, file)): Path<(u32, u64, String)>,
) -> Response {
    let Some(y) = row(&file, "terrain") else {
        return not_found();
    };
    let coord = TileCoord::new(z, x, y);
    let terrain = Arc::clone(&served);
    // A tile is tens of milliseconds of arithmetic: off the I/O threads.
    let tile = tokio::task::spawn_blocking(move || terrain.terrain.tile(coord)).await;
    match tile {
        Ok(Ok(bytes)) => (
            [
                (
                    header::CONTENT_TYPE,
                    HeaderValue::from_static(
                        "application/vnd.quantized-mesh;extensions=octvertexnormals",
                    ),
                ),
                (
                    header::CACHE_CONTROL,
                    HeaderValue::from_static("max-age=3600"),
                ),
            ],
            bytes,
        )
            .into_response(),
        // No such tile — deeper than the levels, or off the grid: a 404,
        // which the viewer remembers, and refines by cutting the parent.
        Ok(Err(_)) => not_found(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn imagery_tile(
    State(served): State<Shared>,
    Path((z, x, file)): Path<(u32, u64, String)>,
) -> Response {
    let Some(y) = row(&file, "png") else {
        return not_found();
    };
    if z > 30 || x >= 2u64 << z || y >= 1u64 << z {
        return not_found();
    }
    let heights = Arc::clone(&served.heights);
    match tokio::task::spawn_blocking(move || relief::imagery_png(&heights, z, x, y)).await {
        Ok(png) => (
            [
                (header::CONTENT_TYPE, HeaderValue::from_static("image/png")),
                (
                    header::CACHE_CONTROL,
                    HeaderValue::from_static("max-age=3600"),
                ),
            ],
            png,
        )
            .into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// The command line.
#[derive(Debug, Default)]
struct Flags {
    listen: Option<SocketAddr>,
    tls: (Option<PathBuf>, Option<PathBuf>),
    public: Option<String>,
}

fn flags(args: impl IntoIterator<Item = String>) -> Result<Flags> {
    let mut flags = Flags::default();
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((name, value)) => (name.to_owned(), Some(value.to_owned())),
            None => (arg.clone(), None),
        };
        if name == "-h" || name == "--help" {
            println!(
                "tuile-heightfield-serve [--listen ADDR] [--tls-cert PEM --tls-key PEM] \
                 [--public-url URL]\n\nSee examples/heightfield-serve/README.md."
            );
            std::process::exit(0);
        }
        let value = match inline {
            Some(value) => value,
            None => args
                .next()
                .with_context(|| format!("{name} needs a value"))?,
        };
        match name.as_str() {
            "--listen" => {
                flags.listen = Some(
                    value
                        .parse()
                        .with_context(|| format!("--listen {value:?}"))?,
                );
            }
            "--tls-cert" => flags.tls.0 = Some(value.into()),
            "--tls-key" => flags.tls.1 = Some(value.into()),
            "--public-url" => flags.public = Some(value.trim_end_matches('/').to_owned()),
            other => bail!("unknown flag {other:?}"),
        }
    }
    Ok(flags)
}

/// Where `scripts/try-viewer.sh web` serves the page: on this server's
/// host and scheme, port 8091.
fn page_origin(base: &str) -> String {
    let (scheme, rest) = base.split_once("://").unwrap_or(("http", base));
    let host = rest.split('/').next().unwrap_or(rest);
    let host = match host.rsplit_once(':') {
        Some((name, _port)) => name,
        None => host,
    };
    let host = if host == "127.0.0.1" || host == "0.0.0.0" {
        "localhost"
    } else {
        host
    };
    format!("{scheme}://{host}:8091")
}

/// The commands that show this server's relief.
fn instructions(base: &str) -> String {
    let [west, south, east, north] = relief::footprint_degrees();
    let terrain = format!("{base}/terrain/layer.json");
    let imagery = format!("{base}/imagery/{{z}}/{{x}}/{{y}}.png");
    let query = |s: &str| {
        s.replace('{', "%7B")
            .replace('}', "%7D")
            .replace(':', "%3A")
            .replace('/', "%2F")
    };
    format!(
        "\nThe relief: {west:.4}..{east:.4} E, {south:.4}..{north:.4} N; the border at lon {east:.4}.\n\n\
         Native (no token needed):\n  \
         cargo run --release -p tuile-wgpu-viewer -- \\\n    \
         --terrain-url {terrain} \\\n    \
         --imagery-template '{imagery}' \\\n    \
         --imagery-scheme geographic --imagery-max-level {MAX_LEVEL} \\\n    \
         --lon {lon:.4} --lat {lat:.4} --altitude 3000 --pitch 35\n\n\
         Web (after `scripts/try-viewer.sh web`):\n  \
         {page}/examples/web-viewer/?terrain={t}&imagery={i}&scheme=geographic&maxLevel={MAX_LEVEL}&lon={lon:.4}&lat={lat:.4}&altitude=3000&pitch=35\n  \
         (add &backend=webgl2 to force WebGL2)\n",
        page = page_origin(base),
        lon = relief::CENTRE_LON,
        lat = relief::CENTRE_LAT,
        t = query(&terrain),
        i = query(&imagery),
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let flags = flags(std::env::args().skip(1))?;
    let listen = flags
        .listen
        .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 8092)));
    let scheme = if flags.tls.0.is_some() {
        "https"
    } else {
        "http"
    };
    let base = flags
        .public
        .clone()
        .unwrap_or_else(|| format!("{scheme}://{listen}"));
    let started = std::time::Instant::now();
    let served = Arc::new(served());
    tracing::info!(
        ms = started.elapsed().as_millis(),
        samples = relief::SAMPLES,
        "relief made"
    );
    let app = router(served);
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("cannot listen on {listen}"))?;
    println!("{}", instructions(&base));
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    match &flags.tls {
        (None, None) => {
            tracing::info!(%listen, "serving http");
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown)
                .await?;
        }
        (Some(cert), Some(key)) => {
            let listener = tls::Listener::new(listener, cert, key)?;
            tracing::info!(%listen, "serving https");
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown)
                .await?;
        }
        _ => bail!("--tls-cert and --tls-key go together"),
    }
    Ok(())
}

/// HTTPS on the server's own listener, for the devices of a local network.
/// The catalog proxy's (`examples/catalog-proxy`), for the same reason.
mod tls {
    use std::path::Path;
    use std::sync::Arc;

    use anyhow::{Context, Result};
    use rustls_pki_types::pem::PemObject;
    use rustls_pki_types::{CertificateDer, PrivateKeyDer};
    use tokio::net::{TcpListener, TcpStream};
    use tokio_rustls::server::TlsStream;
    use tokio_rustls::{rustls, TlsAcceptor};

    pub(super) struct Listener {
        tcp: TcpListener,
        acceptor: TlsAcceptor,
    }

    impl Listener {
        pub(super) fn new(tcp: TcpListener, cert: &Path, key: &Path) -> Result<Self> {
            let chain = CertificateDer::pem_file_iter(cert)
                .with_context(|| format!("cannot read {}", cert.display()))?
                .collect::<Result<Vec<_>, _>>()
                .with_context(|| format!("no certificate in {}", cert.display()))?;
            let key = PrivateKeyDer::from_pem_file(key)
                .with_context(|| format!("no private key in {}", key.display()))?;
            let config = rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(chain, key)
                .context("the certificate and the key do not make a server")?;
            Ok(Self {
                tcp,
                acceptor: TlsAcceptor::from(Arc::new(config)),
            })
        }
    }

    impl axum::serve::Listener for Listener {
        type Io = TlsStream<TcpStream>;
        type Addr = std::net::SocketAddr;

        async fn accept(&mut self) -> (Self::Io, Self::Addr) {
            loop {
                let (stream, addr) = axum::serve::Listener::accept(&mut self.tcp).await;
                match self.acceptor.accept(stream).await {
                    Ok(tls) => return (tls, addr),
                    Err(e) => tracing::debug!(%addr, "a TLS handshake failed: {e}"),
                }
            }
        }

        fn local_addr(&self) -> std::io::Result<Self::Addr> {
            self.tcp.local_addr()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::http::Request;
    use std::sync::OnceLock;
    use tower::ServiceExt;
    use tuile_terrain::{decode, LayerJson};

    /// One relief for every test: it takes a second to make.
    fn shared() -> Shared {
        static SERVED: OnceLock<Shared> = OnceLock::new();
        Arc::clone(SERVED.get_or_init(|| Arc::new(served())))
    }

    async fn get(path: &str) -> (StatusCode, Option<String>, Option<String>, Vec<u8>) {
        let response = router(shared())
            .oneshot(
                Request::get(path)
                    .header(header::ORIGIN, "http://localhost:8091")
                    .body(Body::empty())
                    .expect("a request"),
            )
            .await
            .expect("an answer");
        let text = |name| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let (kind, cors) = (
            text(header::CONTENT_TYPE),
            text(header::ACCESS_CONTROL_ALLOW_ORIGIN),
        );
        let status = response.status();
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("a body")
            .to_vec();
        (status, kind, cors, body)
    }

    /// The layer, a tile of the relief, one of the ellipsoid around it, one
    /// too deep, and an imagery tile: each with its type, all open to
    /// another origin.
    #[tokio::test]
    async fn the_routes_answer_with_their_types_and_cors() {
        let (status, kind, cors, body) = get("/terrain/layer.json").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(kind.as_deref(), Some("application/json"));
        assert_eq!(cors.as_deref(), Some("*"));
        let layer = LayerJson::from_slice(&body).expect("a layer.json");
        assert_eq!(layer.maxzoom, MAX_LEVEL);
        assert!(layer.is_quantized_mesh());

        // The level-15 tile under the map's centre, TMS.
        let scheme = tuile_terrain::GeographicTilingScheme::default();
        let size = std::f64::consts::PI / scheme.tiles_y(MAX_LEVEL) as f64;
        let x = ((relief::CENTRE_LON.to_radians() + std::f64::consts::PI) / size) as u64;
        let y = ((relief::CENTRE_LAT.to_radians() + std::f64::consts::FRAC_PI_2) / size) as u64;
        let (status, kind, cors, body) = get(&format!("/terrain/15/{x}/{y}.terrain")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(kind
            .expect("a type")
            .starts_with("application/vnd.quantized-mesh"));
        assert_eq!(cors.as_deref(), Some("*"));
        let mesh = decode(&body).expect("quantized-mesh");
        assert!(mesh.header.max_height > 0.0, "relief under the centre");

        // Far from the map: the ellipsoid, flat.
        let (status, _, _, body) = get("/terrain/3/1/1.terrain").await;
        assert_eq!(status, StatusCode::OK);
        let flat = decode(&body).expect("quantized-mesh");
        assert_eq!(flat.header.max_height, 0.0);

        // Deeper than the levels, and not a tile at all.
        for path in [
            format!("/terrain/16/{}/{}.terrain", 2 * x, 2 * y),
            "/terrain/3/1/1.png".to_owned(),
        ] {
            assert_eq!(get(&path).await.0, StatusCode::NOT_FOUND, "{path}");
        }

        // Imagery, XYZ: the map is in the eastern level-0 tile, x = 1.
        let (status, kind, cors, body) = get("/imagery/0/1/0.png").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(kind.as_deref(), Some("image/png"));
        assert_eq!(cors.as_deref(), Some("*"));
        let image = image::load_from_memory(&body).expect("a PNG");
        assert_eq!(image.width(), relief::TILE_PX);
        assert_eq!(get("/imagery/0/2/0.png").await.0, StatusCode::NOT_FOUND);
    }

    #[test]
    fn the_flags_are_read() {
        let f =
            flags(["--listen=0.0.0.0:9000", "--public-url", "https://h:9000/"].map(String::from))
                .expect("valid");
        assert_eq!(f.listen, Some("0.0.0.0:9000".parse().expect("an address")));
        assert_eq!(f.public.as_deref(), Some("https://h:9000"));
        assert!(flags(["--port".to_owned(), "1".to_owned()]).is_err());
        assert!(instructions("http://h").contains("--terrain-url http://h/terrain/layer.json"));
        assert_eq!(
            page_origin("http://127.0.0.1:8092"),
            "http://localhost:8091"
        );
        assert_eq!(
            page_origin("https://192.168.1.20:8092"),
            "https://192.168.1.20:8091"
        );
    }
}
