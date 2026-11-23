// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A credential that runs out, renewed: a tile server that hands its
//! visitors a token of a day, say.
//!
//! [`Renewing`] is a [`TileFetcher`] over a transport that can send a
//! [`Credential`] ([`FetchWith`]) and a place that hands credentials out
//! ([`CredentialSource`]). It asks the source when it holds none, again a
//! little before the lifetime the source stated runs out, and again when
//! the server refuses the one it holds (401 or 403) — then asks the tile
//! once more with the new one.
//!
//! - **One renewal at a time.** However many tiles are refused together,
//!   the source is asked once: the others wait for that answer and use it.
//! - **Bounded.** A renewal that fails is not tried again before a pause
//!   that doubles with each failure, from [`Renewing::FIRST_PAUSE`] to
//!   [`Renewing::LONGEST_PAUSE`]; meanwhile every tile fails at once, as an
//!   unreachable server's do. Nothing here sleeps: the caller's own retry
//!   and back-off decide when to ask again, and a substitute layer whose
//!   tiles fail is suspended and the ground drawn from its source — never a
//!   hole (`docs/explorations/imagery-layer-substitution.md`, §3.4).
//! - **In memory only.** The credential is never printed (see
//!   [`Credential`]) and never stored by this type.

use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use url::Url;
use web_time::Instant;

use tuile_core::fetch::{Credential, FetchError, Fetched, TileFetcher};

/// A transport that can send a credential with a request.
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
pub trait FetchWith: Send + Sync {
    /// `url`, asked with `credential`: a 404 is [`FetchError::NotFound`],
    /// any other refusal [`FetchError::Status`].
    async fn fetch_with(
        &self,
        url: &Url,
        credential: &Credential,
    ) -> Result<Fetched<Bytes>, FetchError>;
}

/// A credential, and how long it is good for when its source said so.
#[derive(Debug, Clone)]
pub struct Issued {
    pub credential: Credential,
    pub lifetime: Option<Duration>,
}

/// Where credentials come from: asked when one is needed.
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
pub trait CredentialSource: Send + Sync {
    /// A fresh credential, or the sentence that says why there is none —
    /// never the value of anything secret.
    async fn obtain(&self) -> Result<Issued, String>;
}

/// What is held: the credential, its generation, when to renew it, and the
/// pause a failed renewal imposed.
#[derive(Default)]
struct Held {
    credential: Option<Credential>,
    /// Counts renewals: a tile refused under generation `g` renews only if
    /// nobody renewed since.
    generation: u64,
    renew_at: Option<Instant>,
    failures: u32,
    paused_until: Option<Instant>,
}

/// See the module.
pub struct Renewing<T, S> {
    transport: T,
    source: S,
    held: Mutex<Held>,
    /// Held while the source is asked: one renewal at a time.
    renewal: tokio::sync::Mutex<()>,
}

impl<T: FetchWith, S: CredentialSource> Renewing<T, S> {
    /// The first pause after a failed renewal.
    pub const FIRST_PAUSE: Duration = Duration::from_secs(5);
    /// The longest.
    pub const LONGEST_PAUSE: Duration = Duration::from_secs(300);
    /// How long before the stated end of its lifetime a credential is
    /// renewed: a tenth of it, at most this.
    pub const AHEAD: Duration = Duration::from_secs(600);

    pub fn new(transport: T, source: S) -> Self {
        Self {
            transport,
            source,
            held: Mutex::new(Held::default()),
            renewal: tokio::sync::Mutex::new(()),
        }
    }

    /// The credential to ask with now, and its generation: the one held
    /// while it is good, else a fresh one.
    async fn current(&self) -> Result<(Credential, u64), FetchError> {
        let (held, generation) = {
            let held = self.held.lock().expect("the credential's lock");
            let fresh = held.renew_at.is_none_or(|at| Instant::now() < at);
            (held.credential.clone().filter(|_| fresh), held.generation)
        };
        match held {
            Some(credential) => Ok((credential, generation)),
            None => self.renew(generation).await,
        }
    }

    /// A new credential, unless one was obtained since `seen`.
    async fn renew(&self, seen: u64) -> Result<(Credential, u64), FetchError> {
        let _one = self.renewal.lock().await;
        {
            let held = self.held.lock().expect("the credential's lock");
            if held.generation != seen {
                if let Some(credential) = held.credential.clone() {
                    return Ok((credential, held.generation));
                }
            }
            if let Some(until) = held.paused_until {
                if Instant::now() < until {
                    return Err(unavailable("renewal paused after a failure"));
                }
            }
        }
        let obtained = self.source.obtain().await;
        let mut held = self.held.lock().expect("the credential's lock");
        match obtained {
            Ok(issued) => {
                let now = Instant::now();
                held.credential = Some(issued.credential.clone());
                held.generation += 1;
                held.renew_at = issued
                    .lifetime
                    .map(|life| now + life.saturating_sub((life / 10).min(Self::AHEAD)));
                held.failures = 0;
                held.paused_until = None;
                metrics::counter!("tuile_credential_renewals_total", "outcome" => "ok")
                    .increment(1);
                Ok((issued.credential, held.generation))
            }
            Err(why) => {
                held.failures += 1;
                let pause = Self::FIRST_PAUSE
                    .saturating_mul(1 << (held.failures - 1).min(8))
                    .min(Self::LONGEST_PAUSE);
                held.paused_until = Some(Instant::now() + pause);
                metrics::counter!("tuile_credential_renewals_total", "outcome" => "error")
                    .increment(1);
                tracing::warn!(
                    failures = held.failures,
                    ?pause,
                    "credential renewal failed: {why}"
                );
                Err(unavailable(&why))
            }
        }
    }
}

/// A tile that cannot be asked for: no credential to ask with.
fn unavailable(why: &str) -> FetchError {
    FetchError::Io {
        // No address of the tile: the error is the credential's.
        url: Url::parse("credential:renewal").expect("a constant URL"),
        message: format!("no credential: {why}"),
    }
}

fn refused(e: &FetchError) -> bool {
    matches!(
        e,
        FetchError::Status {
            status: 401 | 403,
            ..
        }
    )
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
impl<T: FetchWith, S: CredentialSource> TileFetcher for Renewing<T, S> {
    async fn fetch(&self, url: &Url) -> Result<Bytes, FetchError> {
        self.fetch_cacheable(url).await.map(|f| f.value)
    }

    async fn fetch_cacheable(&self, url: &Url) -> Result<Fetched<Bytes>, FetchError> {
        let (credential, generation) = self.current().await?;
        match self.transport.fetch_with(url, &credential).await {
            Err(e) if refused(&e) => {
                let (credential, _) = self.renew(generation).await?;
                self.transport.fetch_with(url, &credential).await
            }
            answer => answer,
        }
    }
}

/// The desktop's and the phone's transport: the native HTTP stack.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct NativeTransport(pub tuile_native_fetchers::NativeHttp);

#[cfg(not(target_arch = "wasm32"))]
#[async_trait]
impl FetchWith for NativeTransport {
    async fn fetch_with(
        &self,
        url: &Url,
        credential: &Credential,
    ) -> Result<Fetched<Bytes>, FetchError> {
        self.0.fetch_tile_with(url, credential).await
    }
}

/// The desktop's and the phone's issuer: a cookie or a JSON answer, sent
/// under `header`.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct NativeIssuer {
    pub issuer: tuile_native_fetchers::CredentialIssuer,
    pub header: String,
}

#[cfg(not(target_arch = "wasm32"))]
#[async_trait]
impl CredentialSource for NativeIssuer {
    async fn obtain(&self) -> Result<Issued, String> {
        let (value, lifetime) = self.issuer.obtain().await?;
        Ok(Issued {
            credential: Credential::under(&self.header, &value),
            lifetime,
        })
    }
}

/// A page's transport: the browser's `fetch`.
#[cfg(target_arch = "wasm32")]
pub(crate) struct WebTransport;

#[cfg(target_arch = "wasm32")]
#[async_trait(?Send)]
impl FetchWith for WebTransport {
    async fn fetch_with(
        &self,
        url: &Url,
        credential: &Credential,
    ) -> Result<Fetched<Bytes>, FetchError> {
        tuile_web::fetch_tile_with(url, credential).await
    }
}

/// A page's issuer: a JSON answer (a page cannot read another site's
/// cookies), sent under `header`.
#[cfg(target_arch = "wasm32")]
pub(crate) struct WebIssuer {
    pub url: Url,
    pub field: String,
    pub header: String,
}

#[cfg(target_arch = "wasm32")]
#[async_trait(?Send)]
impl CredentialSource for WebIssuer {
    async fn obtain(&self) -> Result<Issued, String> {
        let (value, lifetime) = tuile_web::issue_json(&self.url, &self.field).await?;
        Ok(Issued {
            credential: Credential::under(&self.header, &value),
            lifetime,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A tile server that takes the token of the current generation only.
    #[derive(Clone, Default)]
    struct Server {
        valid: Arc<AtomicU64>,
        asked: Arc<AtomicUsize>,
    }

    #[cfg_attr(not(target_arch = "wasm32"), async_trait)]
    #[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
    impl FetchWith for Server {
        async fn fetch_with(
            &self,
            url: &Url,
            credential: &Credential,
        ) -> Result<Fetched<Bytes>, FetchError> {
            self.asked.fetch_add(1, Ordering::SeqCst);
            let expected = format!("t{}", self.valid.load(Ordering::SeqCst));
            if credential.expose() != expected {
                return Err(FetchError::Status {
                    status: 401,
                    url: url.clone(),
                });
            }
            if url.path().ends_with("absent") {
                return Err(FetchError::NotFound(url.clone()));
            }
            Ok(Fetched::undated(Bytes::from_static(b"tile")))
        }
    }

    /// Hands out the server's current token, counting; down when told.
    #[derive(Clone)]
    struct Issuer {
        server: Server,
        obtained: Arc<AtomicUsize>,
        down: Arc<AtomicBool>,
        lifetime: Option<Duration>,
    }

    #[cfg_attr(not(target_arch = "wasm32"), async_trait)]
    #[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
    impl CredentialSource for Issuer {
        async fn obtain(&self) -> Result<Issued, String> {
            self.obtained.fetch_add(1, Ordering::SeqCst);
            if self.down.load(Ordering::SeqCst) {
                return Err("the issuer answered 503".into());
            }
            // A real await point, so that concurrent askers overlap: the
            // others are refused and wait on this renewal.
            Yield(false).await;
            Ok(Issued {
                credential: Credential::under(
                    "X-Token",
                    &format!("t{}", self.server.valid.load(Ordering::SeqCst)),
                ),
                lifetime: self.lifetime,
            })
        }
    }

    /// Pending once, then ready: hands the executor to the other tasks.
    struct Yield(bool);

    impl std::future::Future for Yield {
        type Output = ();
        fn poll(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<()> {
            if self.0 {
                return std::task::Poll::Ready(());
            }
            self.0 = true;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    }

    fn setup(lifetime: Option<Duration>) -> (Server, Issuer, Renewing<Server, Issuer>) {
        let server = Server::default();
        let issuer = Issuer {
            server: server.clone(),
            obtained: Arc::default(),
            down: Arc::default(),
            lifetime,
        };
        let renewing = Renewing::new(server.clone(), issuer.clone());
        (server, issuer, renewing)
    }

    fn tile(name: &str) -> Url {
        Url::parse(&format!("https://tiles.test/{name}")).expect("url")
    }

    /// **A credential that ran out is renewed on the server's refusal and
    /// the tile asked again**: tiles again, one renewal; a 404 stays an
    /// absence.
    #[test]
    fn an_expired_credential_is_renewed_and_the_tile_asked_again() {
        futures_executor::block_on(async {
            let (server, issuer, renewing) = setup(None);
            assert!(renewing.fetch(&tile("a")).await.is_ok());
            assert_eq!(issuer.obtained.load(Ordering::SeqCst), 1);
            // The token runs out on the server's side.
            server.valid.store(1, Ordering::SeqCst);
            let again = renewing.fetch(&tile("b")).await.expect("renewed");
            assert_eq!(again.as_ref(), b"tile");
            assert_eq!(issuer.obtained.load(Ordering::SeqCst), 2);
            assert!(matches!(
                renewing.fetch(&tile("absent")).await,
                Err(FetchError::NotFound(_))
            ));
            assert_eq!(issuer.obtained.load(Ordering::SeqCst), 2);
        });
    }

    /// **Many tiles refused together renew once.**
    #[test]
    fn concurrent_refusals_renew_once() {
        futures_executor::block_on(async {
            let (server, issuer, renewing) = setup(None);
            renewing.fetch(&tile("a")).await.expect("first");
            server.valid.store(1, Ordering::SeqCst);
            let asks = (0..16).map(|i| renewing.fetch_cacheable_owned(tile(&format!("t{i}"))));
            let answers = futures_util::future::join_all(asks).await;
            assert!(answers.iter().all(Result::is_ok), "{answers:?}");
            assert_eq!(issuer.obtained.load(Ordering::SeqCst), 2, "one renewal");
        });
    }

    /// **A source that is down makes tiles fail, at once and boundedly**:
    /// no renewal during the pause, an error (never a picture of nothing),
    /// and tiles again once it answers.
    #[test]
    fn a_source_that_is_down_fails_soft_and_boundedly() {
        futures_executor::block_on(async {
            let (_, issuer, renewing) = setup(None);
            issuer.down.store(true, Ordering::SeqCst);
            for _ in 0..5 {
                assert!(matches!(
                    renewing.fetch(&tile("a")).await,
                    Err(FetchError::Io { .. })
                ));
            }
            assert_eq!(
                issuer.obtained.load(Ordering::SeqCst),
                1,
                "paused after one"
            );
            let said = format!("{:?}", renewing.fetch(&tile("a")).await);
            assert!(!said.contains("t0"), "{said}");
            // The pause over, it answers again.
            issuer.down.store(false, Ordering::SeqCst);
            renewing.held.lock().expect("lock").paused_until = Some(Instant::now());
            assert!(renewing.fetch(&tile("a")).await.is_ok());
            assert_eq!(renewing.held.lock().expect("lock").failures, 0);
        });
    }

    /// **A stated lifetime is renewed ahead of its end**, without waiting
    /// for a refusal.
    #[test]
    fn a_stated_lifetime_is_renewed_ahead() {
        futures_executor::block_on(async {
            let (_, issuer, renewing) = setup(Some(Duration::from_secs(86_400)));
            renewing.fetch(&tile("a")).await.expect("first");
            let renew_at = renewing
                .held
                .lock()
                .expect("lock")
                .renew_at
                .expect("a time");
            let ahead = renew_at.duration_since(Instant::now());
            assert!(ahead <= Duration::from_secs(86_400 - 600), "{ahead:?}");
            assert!(ahead > Duration::from_secs(86_000 - 600), "{ahead:?}");
            renewing.held.lock().expect("lock").renew_at = Some(Instant::now());
            renewing.fetch(&tile("b")).await.expect("renewed ahead");
            assert_eq!(issuer.obtained.load(Ordering::SeqCst), 2);
        });
    }

    impl<T: FetchWith, S: CredentialSource> Renewing<T, S> {
        async fn fetch_cacheable_owned(&self, url: Url) -> Result<Fetched<Bytes>, FetchError> {
            self.fetch_cacheable(&url).await
        }
    }
}
