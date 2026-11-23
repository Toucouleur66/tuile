// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Abstract I/O: the core never knows where bytes come from.
//!
//! Implementations: [`FsFetcher`] (local files: tests, local viewer),
//! `HttpFetcher` (feature `http`, native), a wasm `fetch` implementation
//! lives with the web backend, and `tuile-cesium-ion` will wrap any of
//! them with endpoint resolution and bearer tokens.

use async_trait::async_trait;
use bytes::Bytes;
use url::Url;

/// Statuses worth trying again, for every transport — the desktop's and the
/// browser's alike.
///
/// The list is short on purpose. Every status *not* here is treated as the
/// origin's final answer, and that is the important half: a 404 retried five
/// times is five times the latency for the same answer, and a 401 retried is a
/// misconfiguration hidden behind a delay.
pub fn is_retryable_status(status: u16) -> bool {
    matches!(
        status,
        // Request Timeout, Too Early, Too Many Requests.
        408 | 425 | 429
        // Server-side: the request may well succeed unchanged. 501 and 505 are
        // excluded — the server is saying it will never do this.
        | 500 | 502 | 503 | 504 | 507 | 509
    )
}

/// What a fetch error says, with the values of every query parameter in it
/// withheld: see [`redact`].
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("not found: {}", redact(.0.as_str()))]
    NotFound(Url),
    #[error("unsupported url scheme {scheme:?} for {}", redact(.url.as_str()))]
    UnsupportedScheme { scheme: String, url: Url },
    #[error("i/o error fetching {}: {}", redact(.url.as_str()), redact(.message))]
    Io { url: Url, message: String },
    #[error("http status {status} for {}", redact(.url.as_str()))]
    Status { status: u16, url: Url },
}

/// `text` with the value of every query parameter of every URL in it
/// replaced by `…`, the names kept.
///
/// A service may put its key on every URL it is fetched under, and a session
/// beside it; a message that carries a URL — an error, a retry warning — must
/// not carry them into a log, a window or a report. What is withheld is the
/// value alone: which parameters a URL had is still said.
pub fn redact(text: &str) -> String {
    let delimiter = |c: char| c.is_whitespace() || "\"'()<>[]{}".contains(c);
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(scheme) = rest.find("://") {
        let start = rest[..scheme].rfind(delimiter).map_or(0, |i| i + 1);
        let end = rest[scheme..]
            .find(delimiter)
            .map_or(rest.len(), |i| scheme + i);
        out.push_str(&rest[..start]);
        out.push_str(&redact_url(&rest[start..end]));
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// One URL's query values withheld.
fn redact_url(url: &str) -> String {
    let Some((head, query)) = url.split_once('?') else {
        return url.to_owned();
    };
    let (query, fragment) = match query.split_once('#') {
        Some((q, f)) => (q, Some(f)),
        None => (query, None),
    };
    let pairs: Vec<String> = query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((name, _)) => format!("{name}=…"),
            None => pair.to_owned(),
        })
        .collect();
    let mut out = format!("{head}?{}", pairs.join("&"));
    if let Some(fragment) = fragment {
        out.push('#');
        out.push_str(fragment);
    }
    out
}

/// Something fetched, with how long it may be reused.
///
/// The lifetime is the origin's, carried from wherever it was stated — an HTTP
/// `Cache-Control: max-age`, a token expiry — down to whatever stores the
/// value. Deriving it locally instead would be a guess: only the origin knows
/// whether a tile is immutable or reissued nightly.
///
/// `None` means the origin said nothing. Treat that as "cacheable, freshness
/// unknown" and let the store apply its own policy — not as "do not cache",
/// which would throw away most of the benefit, most origins being silent.
#[derive(Debug, Clone)]
pub struct Fetched<T> {
    pub value: T,
    pub ttl: Option<std::time::Duration>,
}

impl<T> Fetched<T> {
    /// A value whose origin stated no lifetime.
    pub fn undated(value: T) -> Self {
        Self { value, ttl: None }
    }

    /// Applies `f` to the value, keeping the lifetime — the usual shape when a
    /// layer decodes what a lower one fetched.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Fetched<U> {
        Fetched {
            value: f(self.value),
            ttl: self.ttl,
        }
    }

    /// Like [`Self::map`], for a decode that can fail.
    pub fn try_map<U, E>(self, f: impl FnOnce(T) -> Result<U, E>) -> Result<Fetched<U>, E> {
        Ok(Fetched {
            value: f(self.value)?,
            ttl: self.ttl,
        })
    }
}

/// The credential a tile server asks for, sent as one request header:
/// `Authorization: Bearer <token>` by default, or the token as it is under
/// a header the server names (an API key header, say).
///
/// Never printed: no `Display`, and a `Debug` that names the header and
/// withholds the value — a fetch error, a retry warning or a log line that
/// carries a request never carries its secret.
#[derive(Clone, PartialEq, Eq)]
pub struct Credential {
    header: String,
    value: String,
}

impl Credential {
    /// The header a credential goes in when nothing else is said.
    pub const AUTHORIZATION: &'static str = "Authorization";

    /// `Authorization: Bearer <token>`.
    pub fn bearer(token: &str) -> Self {
        Self {
            header: Self::AUTHORIZATION.to_owned(),
            value: format!("Bearer {}", token.trim()),
        }
    }

    /// `token` under the header `name`: a bearer token when `name` is
    /// `Authorization` (in any case), the token alone otherwise.
    pub fn under(name: &str, token: &str) -> Self {
        if name.trim().eq_ignore_ascii_case(Self::AUTHORIZATION) {
            Self::bearer(token)
        } else {
            Self {
                header: name.trim().to_owned(),
                value: token.trim().to_owned(),
            }
        }
    }

    /// The header's name.
    pub fn header(&self) -> &str {
        &self.header
    }

    /// The header's value, for the one place that needs it: the request.
    pub fn expose(&self) -> &str {
        &self.value
    }
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Credential({}: <withheld>)", self.header)
    }
}

/// Abstract byte source. `Send + Sync` bounds are the native baseline; the
/// single-threaded wasm relaxation (`maybe_send`) is an M2 follow-up noted
/// in `docs/01-architecture.md`.
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
pub trait TileFetcher: Send + Sync {
    async fn fetch(&self, url: &Url) -> Result<Bytes, FetchError>;

    /// Like [`Self::fetch`], but also reports how long the bytes may be reused.
    ///
    /// Provided so the many fetchers that have no notion of freshness — a
    /// filesystem, a test double — stay one method. Transports that do carry it
    /// (HTTP, with `Cache-Control`) override this; callers that intend to store
    /// what they fetch should prefer it.
    async fn fetch_cacheable(&self, url: &Url) -> Result<Fetched<Bytes>, FetchError> {
        Ok(Fetched::undated(self.fetch(url).await?))
    }
}

/// Reads `file://` URLs from the local filesystem. Meant for tests and the
/// local viewer; blocking reads, deliberately simple. Not available on wasm
/// (no filesystem, and `Url::to_file_path` is compiled out there).
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Default, Clone)]
pub struct FsFetcher;

#[cfg(not(target_arch = "wasm32"))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl TileFetcher for FsFetcher {
    async fn fetch(&self, url: &Url) -> Result<Bytes, FetchError> {
        if url.scheme() != "file" {
            return Err(FetchError::UnsupportedScheme {
                scheme: url.scheme().to_owned(),
                url: url.clone(),
            });
        }
        let path = url
            .to_file_path()
            .map_err(|()| FetchError::NotFound(url.clone()))?;
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Bytes::from(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(FetchError::NotFound(url.clone()))
            }
            Err(e) => Err(FetchError::Io {
                url: url.clone(),
                message: e.to_string(),
            }),
        }
    }
}

/// HTTP(S) fetcher backed by reqwest (native targets, feature `http`).
#[cfg(all(feature = "http", not(target_arch = "wasm32")))]
#[derive(Debug, Clone, Default)]
pub struct HttpFetcher {
    client: reqwest::Client,
}

#[cfg(all(feature = "http", not(target_arch = "wasm32")))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl TileFetcher for HttpFetcher {
    async fn fetch(&self, url: &Url) -> Result<Bytes, FetchError> {
        let response = self
            .client
            .get(url.clone())
            .send()
            .await
            .map_err(|e| FetchError::Io {
                url: url.clone(),
                message: e.to_string(),
            })?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(FetchError::NotFound(url.clone()));
        }
        if !status.is_success() {
            return Err(FetchError::Status {
                status: status.as_u16(),
                url: url.clone(),
            });
        }
        response.bytes().await.map_err(|e| FetchError::Io {
            url: url.clone(),
            message: e.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A credential goes as a bearer token unless another header is named,
    /// and its value never shows in what is printed of it.
    #[test]
    fn a_credential_is_a_header_and_is_never_printed() {
        let bearer = Credential::bearer(" s3cr3t ");
        assert_eq!(bearer.header(), "Authorization");
        assert_eq!(bearer.expose(), "Bearer s3cr3t");
        assert_eq!(Credential::under("authorization", "s3cr3t"), bearer);
        let keyed = Credential::under("X-Api-Key", "s3cr3t");
        assert_eq!(keyed.header(), "X-Api-Key");
        assert_eq!(keyed.expose(), "s3cr3t");
        for printed in [format!("{bearer:?}"), format!("{keyed:?}")] {
            assert!(!printed.contains("s3cr3t"), "{printed}");
        }
        assert_eq!(format!("{keyed:?}"), "Credential(X-Api-Key: <withheld>)");
    }

    /// No value of a query parameter survives in what an error says, and
    /// everything else does.
    #[test]
    fn errors_withhold_query_values() {
        let url = Url::parse("https://tiles.test/v1/a.glb?session=S3CR3T&key=K3Y").expect("url");
        let said = FetchError::Status {
            status: 403,
            url: url.clone(),
        }
        .to_string();
        assert_eq!(
            said,
            "http status 403 for https://tiles.test/v1/a.glb?session=…&key=…"
        );
        let said = FetchError::Io {
            url,
            message: "error sending request for url (https://tiles.test/b.json?key=K3Y)".into(),
        }
        .to_string();
        assert!(!said.contains("K3Y") && !said.contains("S3CR3T"), "{said}");
        assert!(
            said.ends_with("(https://tiles.test/b.json?key=…)"),
            "{said}"
        );
        assert_eq!(redact("no url at all"), "no url at all");
        assert_eq!(
            redact("file:///data/tileset.json and https://x.test/a?flag"),
            "file:///data/tileset.json and https://x.test/a?flag"
        );
    }
    use futures_util::FutureExt;

    #[test]
    fn fs_fetcher_reads_and_misses() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("tile.glb");
        std::fs::write(&path, b"hello").expect("write");

        let fetcher = FsFetcher;
        let url = Url::from_file_path(&path).expect("url");
        let bytes = fetcher
            .fetch(&url)
            .now_or_never()
            .expect("ready")
            .expect("ok");
        assert_eq!(&bytes[..], b"hello");

        let missing = Url::from_file_path(dir.path().join("nope.glb")).expect("url");
        let err = fetcher.fetch(&missing).now_or_never().expect("ready");
        assert!(matches!(err, Err(FetchError::NotFound(_))));
    }

    #[test]
    fn fs_fetcher_rejects_other_schemes() {
        let fetcher = FsFetcher;
        let url = Url::parse("https://example.com/x.glb").expect("url");
        let err = fetcher.fetch(&url).now_or_never().expect("ready");
        assert!(matches!(err, Err(FetchError::UnsupportedScheme { .. })));
    }
}
