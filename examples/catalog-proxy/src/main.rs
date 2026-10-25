// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

#![doc = include_str!("../README.md")]

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use axum::http::{header, HeaderValue};
use axum::response::Redirect;
use axum::Router;
use tower::ServiceBuilder;
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;
use tuile_cesium_ion::proxy::{router, Catalog, Origins, ProxyConfig};
use tuile_cesium_ion::ReqwestHttp;
use url::Url;

/// The environment variable the token is read from.
const TOKEN_VARIABLE: &str = "TUILE_ION_TOKEN";

#[derive(Debug, Default)]
struct Flags {
    assets: BTreeSet<u64>,
    listen: Option<SocketAddr>,
    mount: Option<String>,
    public: Option<Url>,
    origins: Vec<HeaderValue>,
    env_file: Option<PathBuf>,
    serve: Vec<(String, PathBuf)>,
    tls: (Option<PathBuf>, Option<PathBuf>),
}

fn flags(args: impl IntoIterator<Item = String>) -> Result<Flags> {
    let mut flags = Flags::default();
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        let (name, inline) = match flag.split_once('=') {
            Some((name, value)) if name.starts_with("--") => {
                (name.to_owned(), Some(value.to_owned()))
            }
            _ => (flag.clone(), None),
        };
        if name == "--help" || name == "-h" {
            println!("{}", HELP);
            std::process::exit(0);
        }
        let value = match inline {
            Some(value) => value,
            None => args
                .next()
                .with_context(|| format!("{name} needs a value"))?,
        };
        match name.as_str() {
            "--assets" => {
                for asset in value.split(',').filter(|a| !a.is_empty()) {
                    flags.assets.insert(
                        asset
                            .trim()
                            .parse()
                            .with_context(|| format!("asset {asset:?}"))?,
                    );
                }
            }
            "--listen" => flags.listen = Some(value.parse().context("--listen")?),
            "--mount" => flags.mount = Some(value),
            "--public-url" => {
                let mut url = Url::parse(&value).context("--public-url")?;
                if !url.path().ends_with('/') {
                    url.set_path(&format!("{}/", url.path()));
                }
                flags.public = Some(url);
            }
            "--allow-origin" => flags.origins.push(value.parse().context("--allow-origin")?),
            "--env-file" => flags.env_file = Some(value.into()),
            "--serve" => {
                let (url, dir) = value.split_once('=').context("--serve takes URL=DIR")?;
                flags
                    .serve
                    .push((format!("/{}", url.trim_matches('/')), PathBuf::from(dir)));
            }
            "--tls-cert" => flags.tls.0 = Some(value.into()),
            "--tls-key" => flags.tls.1 = Some(value.into()),
            other => bail!("unknown flag {other}; see --help"),
        }
    }
    if flags.assets.is_empty() {
        bail!(
            "--assets names no asset: the proxy answers for the assets it is given, and no other"
        );
    }
    Ok(flags)
}

const HELP: &str = "tuile-catalog-proxy --assets 1,2,… [--listen ADDR] [--mount PATH] \
[--public-url URL] [--allow-origin ORIGIN]… [--env-file PATH] [--serve URL=DIR]… \
[--tls-cert PEM --tls-key PEM]

The token is read from TUILE_ION_TOKEN, or from --env-file. See the crate's documentation.";

/// The token: the environment first, then the `.env` file named.
fn token(env_file: Option<&PathBuf>) -> Result<String> {
    if let Ok(token) = std::env::var(TOKEN_VARIABLE) {
        if !token.trim().is_empty() {
            return Ok(token.trim().to_owned());
        }
    }
    if let Some(file) = env_file {
        let text = std::fs::read_to_string(file)
            .with_context(|| format!("cannot read {}", file.display()))?;
        for line in text.lines() {
            let line = line.trim().trim_start_matches("export ").trim();
            if let Some(value) = line.strip_prefix(&format!("{TOKEN_VARIABLE}=")) {
                let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
                if !value.is_empty() {
                    return Ok(value.to_owned());
                }
            }
        }
    }
    bail!("no token: set {TOKEN_VARIABLE} in the environment, or name a file that has it with --env-file")
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let flags = flags(std::env::args().skip(1))?;
    let token = token(flags.env_file.as_ref())?;
    let config = ProxyConfig::new(token, flags.assets.iter().copied());
    let catalog = Catalog::new(Arc::new(ReqwestHttp::default()), config);
    let mount = flags.mount.clone().unwrap_or_else(|| "/catalog".to_owned());
    let mount = format!("/{}", mount.trim_matches('/'));
    let origins = if flags.origins.is_empty() {
        Origins::Any
    } else {
        Origins::Only(flags.origins.clone())
    };
    let mut app = Router::new().nest(
        &mount,
        router(catalog, flags.public.clone(), &mount, origins),
    );
    // Files served for development are asked again on every load: a page
    // rebuilt a minute ago must not run last minute's module from the cache.
    for (url, dir) in &flags.serve {
        let files = ServiceBuilder::new()
            .layer(SetResponseHeaderLayer::overriding(
                header::CACHE_CONTROL,
                HeaderValue::from_static("no-cache"),
            ))
            .service(ServeDir::new(dir));
        app = app.nest_service(url, files);
    }
    if let Some((first, _)) = flags.serve.first() {
        let index = format!("{first}/");
        app = app.route(
            "/",
            axum::routing::get(move || async move { Redirect::temporary(&index) }),
        );
    }
    let listen = flags
        .listen
        .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 8091)));
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("cannot listen on {listen}"))?;
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    match &flags.tls {
        (None, None) => {
            tracing::info!(%listen, mount, assets = ?flags.assets, "serving http; the token stays here");
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown)
                .await?;
        }
        (Some(cert), Some(key)) => {
            let listener = tls::Listener::new(listener, cert, key)?;
            tracing::info!(%listen, mount, assets = ?flags.assets, "serving https; the token stays here");
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown)
                .await?;
        }
        _ => bail!("--tls-cert and --tls-key go together"),
    }
    Ok(())
}

/// HTTPS on the proxy's own listener, for the devices of a local network.
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

        /// The next connection that completes its handshake; one that does
        /// not is dropped and the next one waited for.
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

    fn read(args: &[&str]) -> Result<Flags> {
        flags(args.iter().map(|a| (*a).to_owned()))
    }

    #[test]
    fn the_flags_are_read_and_an_empty_asset_list_refused() {
        let f = read(&[
            "--assets",
            "1,2,3954",
            "--listen=0.0.0.0:9000",
            "--public-url",
            "https://tiles.test/catalog",
            "--serve",
            "/examples/web-viewer=examples/web-viewer",
        ])
        .expect("valid");
        assert_eq!(f.assets.into_iter().collect::<Vec<_>>(), [1, 2, 3954]);
        assert_eq!(f.listen, Some("0.0.0.0:9000".parse().expect("an address")));
        assert_eq!(
            f.public.map(String::from).as_deref(),
            Some("https://tiles.test/catalog/")
        );
        assert_eq!(f.serve[0].0, "/examples/web-viewer");
        assert!(read(&["--listen", "127.0.0.1:1"]).is_err());
        assert!(read(&["--assets", "x"]).is_err());
    }

    /// The token comes from the file when the environment has none, quoted
    /// or not; the error never repeats it.
    #[test]
    fn the_token_is_read_from_an_env_file() {
        let dir = std::env::temp_dir().join(format!("catalog-proxy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a directory");
        let file = dir.join(".env");
        std::fs::write(&file, "OTHER=1\nexport TUILE_ION_TOKEN=\"abc.def\"\n").expect("written");
        if std::env::var(TOKEN_VARIABLE).is_err() {
            assert_eq!(token(Some(&file)).expect("a token"), "abc.def");
            std::fs::write(&file, "OTHER=1\n").expect("written");
            let error = token(Some(&file)).expect_err("none").to_string();
            assert!(error.contains(TOKEN_VARIABLE));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
