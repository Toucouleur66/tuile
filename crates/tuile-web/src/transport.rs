// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! `fetch`-backed transports: the two seams the engine needs, over the global
//! `fetch` — every request bounded in time, retried as [`patience`] says, and
//! aborted when nobody awaits it any more.

use async_trait::async_trait;
use bytes::Bytes;
use url::Url;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

use tuile_cesium_ion::{HttpResponse, IonError, IonHttp};
use tuile_core::fetch::{FetchError, Fetched, TileFetcher};

use crate::patience;

/// `fetch` from whichever global this code is running under.
///
/// A Worker has no `window`; a page has no `WorkerGlobalScope`. Asking for the
/// wrong one is the classic way for browser code to work in a demo page and
/// fail in the worker it was written for, so both are tried and the failure is
/// a stated error rather than a panic in JS.
fn global_fetch(request: &web_sys::Request) -> Result<js_sys::Promise, JsValue> {
    let global = js_sys::global();
    if let Some(window) = global.dyn_ref::<web_sys::Window>() {
        return Ok(window.fetch_with_request(request));
    }
    if let Some(worker) = global.dyn_ref::<web_sys::WorkerGlobalScope>() {
        return Ok(worker.fetch_with_request(request));
    }
    Err(JsValue::from_str(
        "no fetch: neither a Window nor a WorkerGlobalScope",
    ))
}

/// Turns whatever JS threw into something printable. `JsValue` has no `Display`
/// and its `Debug` is noisy, so this is the one place the conversion happens.
fn js_message(value: &JsValue) -> String {
    value
        .as_string()
        .or_else(|| {
            value
                .dyn_ref::<js_sys::Error>()
                .map(|e| String::from(e.message()))
        })
        .unwrap_or_else(|| format!("{value:?}"))
}

/// The global scope's timers, from whichever global this runs under.
fn set_timeout(callback: &js_sys::Function, millis: i32) -> Option<i32> {
    let global = js_sys::global();
    if let Some(window) = global.dyn_ref::<web_sys::Window>() {
        return window
            .set_timeout_with_callback_and_timeout_and_arguments_0(callback, millis)
            .ok();
    }
    global
        .dyn_ref::<web_sys::WorkerGlobalScope>()?
        .set_timeout_with_callback_and_timeout_and_arguments_0(callback, millis)
        .ok()
}

fn clear_timeout(handle: i32) {
    let global = js_sys::global();
    if let Some(window) = global.dyn_ref::<web_sys::Window>() {
        window.clear_timeout_with_handle(handle);
    } else if let Some(worker) = global.dyn_ref::<web_sys::WorkerGlobalScope>() {
        worker.clear_timeout_with_handle(handle);
    }
}

/// Waits `pause` on the global scope's clock.
async fn sleep(pause: std::time::Duration) {
    let millis = i32::try_from(pause.as_millis()).unwrap_or(i32::MAX);
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        let _ = set_timeout(&resolve, millis);
    });
    let _ = JsFuture::from(promise).await;
}

/// One attempt's `fetch`, aborted when its time is up — and when it is
/// dropped: a tile load the server cancels drops the future awaiting this,
/// and without the abort the browser would carry on fetching for nobody,
/// holding a connection the next request waits for.
struct Attempt {
    controller: web_sys::AbortController,
    timer: Option<i32>,
    /// Keeps the timer's callback alive until the timer is cleared.
    _expire: wasm_bindgen::closure::Closure<dyn FnMut()>,
    timed_out: std::rc::Rc<std::cell::Cell<bool>>,
}

impl Attempt {
    fn start(timeout: std::time::Duration) -> Result<Self, String> {
        let controller = web_sys::AbortController::new().map_err(|e| js_message(&e))?;
        let timed_out = std::rc::Rc::new(std::cell::Cell::new(false));
        let expire = {
            let (controller, timed_out) = (controller.clone(), std::rc::Rc::clone(&timed_out));
            wasm_bindgen::closure::Closure::<dyn FnMut()>::new(move || {
                timed_out.set(true);
                controller.abort();
            })
        };
        let millis = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        let timer = set_timeout(expire.as_ref().unchecked_ref(), millis);
        Ok(Self {
            controller,
            timer,
            _expire: expire,
            timed_out,
        })
    }
}

impl Drop for Attempt {
    fn drop(&mut self) {
        if let Some(timer) = self.timer {
            clear_timeout(timer);
        }
        // After the response was read this does nothing; before, it frees the
        // connection.
        self.controller.abort();
    }
}

/// One answer: its status, its bytes and its stated lifetime.
type Answer = (u16, Bytes, Option<u64>);

/// The bytes of one response, or the reason there are none — asked as many
/// times as [`patience`] allows `asker`.
async fn get(url: &Url, bearer: Option<&str>, asker: patience::Asker) -> Result<Answer, String> {
    let mut attempt = 1;
    loop {
        let (ended, outcome) = match get_once(url, bearer, asker).await {
            Ok(answer) => (patience::Ended::Answered(answer.0), Ok(answer)),
            Err((ended, why)) => (ended, Err(why)),
        };
        match patience::again(asker, attempt, ended) {
            Some(pause) => {
                tracing::debug!(%url, attempt, ?ended, "asking again");
                sleep(pause).await;
                attempt += 1;
            }
            None => return outcome,
        }
    }
}

/// One attempt.
async fn get_once(
    url: &Url,
    bearer: Option<&str>,
    asker: patience::Asker,
) -> Result<Answer, (patience::Ended, String)> {
    let failed = |e: String| (patience::Ended::Failed, e);
    let timeout = patience::attempt_timeout(asker);
    let attempt = Attempt::start(timeout).map_err(failed)?;
    let opts = web_sys::RequestInit::new();
    opts.set_method("GET");
    // Tiles are served cross-origin (ion, Bing) and answer with permissive CORS
    // headers; `Cors` is what lets the bytes be *read* rather than merely
    // fetched opaquely, which `NoCors` would give us.
    opts.set_mode(web_sys::RequestMode::Cors);
    opts.set_signal(Some(&attempt.controller.signal()));

    let request = web_sys::Request::new_with_str_and_init(url.as_str(), &opts)
        .map_err(|e| failed(js_message(&e)))?;
    // An empty bearer is no bearer: a catalog behind a proxy is asked with
    // none, and an `Authorization` header would only cost a preflight.
    if let Some(token) = bearer.filter(|t| !t.is_empty()) {
        request
            .headers()
            .set("Authorization", &format!("Bearer {token}"))
            .map_err(|e| failed(js_message(&e)))?;
    }
    // A rejection is a network failure, or the timer's abort.
    let ended = |e: JsValue| {
        let why = if attempt.timed_out.get() {
            (
                patience::Ended::TimedOut,
                format!("no answer within {timeout:?}"),
            )
        } else {
            (patience::Ended::Failed, js_message(&e))
        };
        why
    };

    let promise = global_fetch(&request).map_err(|e| failed(js_message(&e)))?;
    let response: web_sys::Response = JsFuture::from(promise)
        .await
        .map_err(ended)?
        .dyn_into()
        .map_err(|e| failed(js_message(&e)))?;

    let status = response.status();
    let max_age = response
        .headers()
        .get("cache-control")
        .ok()
        .flatten()
        .as_deref()
        .and_then(parse_max_age);

    let buffer = JsFuture::from(
        response
            .array_buffer()
            .map_err(|e| failed(js_message(&e)))?,
    )
    .await
    .map_err(ended)?;
    // One copy out of the JS heap into the wasm heap. Unavoidable without
    // shared memory, and the only copy on this path.
    let bytes = Bytes::from(js_sys::Uint8Array::new(&buffer).to_vec());
    Ok((status, bytes, max_age))
}

/// `max-age` from a `Cache-Control` header, when it states one.
///
/// Deliberately tolerant: anything unparsable means "no stated lifetime", which
/// is the same answer as a missing header and is always safe — the caller then
/// invents nothing and re-asks.
fn parse_max_age(header: &str) -> Option<u64> {
    header.split(',').find_map(|part| {
        let part = part.trim();
        let rest = part.strip_prefix("max-age")?.trim_start();
        rest.strip_prefix('=')?.trim().parse().ok()
    })
}

/// Fetches tiles with the browser's own `fetch`.
///
/// Stateless, which is what lets it satisfy the engine's `Send + Sync` bound on
/// a target where JS values are neither: it holds no `Response`, no `Window`,
/// nothing from the JS heap — each call reaches for the global scope itself.
#[derive(Debug, Default, Clone, Copy)]
pub struct WebFetcher;

#[async_trait(?Send)]
impl TileFetcher for WebFetcher {
    async fn fetch(&self, url: &Url) -> Result<Bytes, FetchError> {
        Ok(self.fetch_cacheable(url).await?.value)
    }

    async fn fetch_cacheable(&self, url: &Url) -> Result<Fetched<Bytes>, FetchError> {
        // A tile load: its retries are the budget's.
        let (status, bytes, max_age) =
            get(url, None, patience::Asker::Load)
                .await
                .map_err(|message| FetchError::Io {
                    url: url.clone(),
                    message,
                })?;
        // 404 is a fact about the data — the ocean has no terrain tile — and the
        // engine has a distinct arm for it that stops the re-asking. Collapsing
        // it into a transport error would make every absent tile a permanent
        // retry.
        if status == 404 {
            return Err(FetchError::NotFound(url.clone()));
        }
        if !(200..300).contains(&status) {
            return Err(FetchError::Status {
                status,
                url: url.clone(),
            });
        }
        Ok(Fetched {
            value: bytes,
            ttl: max_age.map(std::time::Duration::from_secs),
        })
    }
}

/// The same transport, wearing the ion API's hat.
///
/// ion needs a bearer token and reports its own status codes, so it has its own
/// seam; both land on the same [`get`].
#[derive(Debug, Default, Clone, Copy)]
pub struct WebIonHttp;

#[async_trait(?Send)]
impl IonHttp for WebIonHttp {
    async fn get(&self, url: &Url, bearer: Option<&str>) -> Result<HttpResponse, IonError> {
        let (status, body, max_age) = get(url, bearer, patience::Asker::Session)
            .await
            .map_err(|e| IonError::Transport(format!("{url}: {e}")))?;
        Ok(HttpResponse {
            status,
            body,
            max_age: max_age.map(std::time::Duration::from_secs),
        })
    }
}
