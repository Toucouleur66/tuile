// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The push from a page, or from its worker: the same registry, the same
//! encoding as a desktop's, sent with `fetch` on the realm's own timer.
//!
//! A browser cannot be sampled as a native process is, so there are no
//! profiles here. What it adds instead is what the Performance API says of
//! every tile request: its duration, its bytes on the wire, and whether the
//! browser's cache answered — a `transferSize` of zero with a body is a
//! cache hit — under the names and labels a native transport records its own
//! with (`tuile_tiles_fetch_*{source="cache"|"upstream", kind}`). A
//! cross-origin response without `Timing-Allow-Origin` hides its sizes, so
//! whether it came from the cache is unknown: `source="unknown"`.

use std::cell::RefCell;

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use crate::timing::{kind, source};
use crate::{Config, Identity, Registry};

/// Who is told of the switch.
type Listeners = RefCell<Vec<Box<dyn Fn(bool)>>>;

thread_local! {
    /// Told when the metrics are switched: a page passes it on to its worker.
    static LISTENERS: Listeners = RefCell::new(Vec::new());
}

/// Calls `listener` whenever [`crate::set_metrics`] is called in this realm:
/// how a page's switch reaches the worker beside it.
pub fn on_switch(listener: Box<dyn Fn(bool)>) {
    LISTENERS.with(|l| l.borrow_mut().push(listener));
}

pub(crate) fn switched(on: bool) {
    LISTENERS.with(|l| {
        for listener in l.borrow().iter() {
            listener(on);
        }
    });
}

/// The realm's global object: a window, or a worker's scope.
enum Scope {
    Window(web_sys::Window),
    Worker(web_sys::WorkerGlobalScope),
}

impl Scope {
    fn here() -> Option<Self> {
        let global = js_sys::global();
        if let Some(window) = global.dyn_ref::<web_sys::Window>() {
            return Some(Self::Window(window.clone()));
        }
        global
            .dyn_into::<web_sys::WorkerGlobalScope>()
            .ok()
            .map(Self::Worker)
    }

    fn every(&self, f: &js_sys::Function, ms: i32) -> Result<i32, JsValue> {
        match self {
            Self::Window(w) => w.set_interval_with_callback_and_timeout_and_arguments_0(f, ms),
            Self::Worker(w) => w.set_interval_with_callback_and_timeout_and_arguments_0(f, ms),
        }
    }

    fn fetch(&self, request: &web_sys::Request) -> js_sys::Promise {
        match self {
            Self::Window(w) => w.fetch_with_request(request),
            Self::Worker(w) => w.fetch_with_request(request),
        }
    }
}

/// Starts the push in this realm (a page or a worker): installs the
/// registry as the realm's recorder, follows its tile requests through the
/// Performance API, and posts every `config.interval`. Returns whether it
/// started: not without an endpoint, and not when the realm already has a
/// recorder.
pub fn start_web(config: &Config, identity: Identity<'_>) -> bool {
    let resource = crate::describe(config, &identity);
    crate::adopt(config, resource);
    let Some(endpoint) = config.endpoint.clone() else {
        return false;
    };
    let Some(scope) = Scope::here() else {
        tracing::warn!("telemetry: neither a window nor a worker; nothing sent");
        return false;
    };
    let registry = Registry::new();
    if metrics::set_global_recorder(registry.clone()).is_err() {
        tracing::warn!("telemetry: this realm already has a metrics recorder; nothing sent");
        return false;
    }
    tuile_metrics::describe();
    follow_tile_requests();
    let url = format!("{endpoint}/v1/metrics");
    let logs_url = format!("{endpoint}/v1/logs");
    let headers = config.headers.clone();
    let start_ns = crate::unix_nanos();
    let failing = std::rc::Rc::new(std::cell::Cell::new(false));
    let tick = Closure::<dyn FnMut()>::new(move || {
        if !crate::metrics_on() {
            return;
        }
        tuile_metrics::publish(tuile_core::metrics::metrics());
        let encoded = crate::otlp::encode(
            &crate::resource(),
            &crate::points(&registry),
            start_ns,
            crate::unix_nanos(),
        );
        let Some(scope) = Scope::here() else { return };
        let request = match request(&url, &headers, &encoded.body) {
            Ok(request) => request,
            Err(why) => {
                tracing::warn!("telemetry: no request ({why})");
                return;
            }
        };
        // The budget's decisions with their score, as log records
        // (`crate::logs`): as a desktop sends them, fire and forget.
        let records = tuile_core::metrics::take_score_records();
        if let Some(body) = crate::logs::encode(&crate::resource(), &records, crate::unix_nanos()) {
            if let Ok(logs) = request(&logs_url, &headers, &body) {
                let sent = scope.fetch(&logs);
                wasm_bindgen_futures::spawn_local(async move {
                    let _ = wasm_bindgen_futures::JsFuture::from(sent).await;
                });
            }
        }
        let failing = std::rc::Rc::clone(&failing);
        let sent = scope.fetch(&request);
        wasm_bindgen_futures::spawn_local(async move {
            let outcome = match wasm_bindgen_futures::JsFuture::from(sent).await {
                Ok(response) => match response.dyn_into::<web_sys::Response>() {
                    Ok(r) if r.ok() => Ok(()),
                    Ok(r) => Err(format!("HTTP {}", r.status())),
                    Err(_) => Err("not a response".to_owned()),
                },
                // A refused CORS preflight lands here too, without a status.
                Err(_) => Err("the request failed".to_owned()),
            };
            match (outcome, failing.get()) {
                (Ok(()), true) => {
                    tracing::info!("telemetry: metrics sent again");
                    failing.set(false);
                }
                (Err(why), false) => {
                    tracing::warn!("telemetry: metrics not sent ({why}); retried every interval");
                    failing.set(true);
                }
                _ => {}
            }
        });
    });
    let ms = config.interval.as_millis().min(i32::MAX as u128) as i32;
    if let Err(e) = scope.every(tick.as_ref().unchecked_ref(), ms) {
        tracing::warn!("telemetry: no timer ({e:?}); nothing sent");
        return false;
    }
    // Lives as long as the realm.
    tick.forget();
    tracing::info!(
        "telemetry: metrics to {endpoint} every {} s{}",
        config.interval.as_secs_f64(),
        if config.headers.is_empty() {
            ""
        } else {
            ", with credentials"
        }
    );
    true
}

fn request(url: &str, headers: &crate::Headers, body: &str) -> Result<web_sys::Request, String> {
    let init = web_sys::RequestInit::new();
    init.set_method("POST");
    init.set_body(&JsValue::from_str(body));
    let h = web_sys::Headers::new().map_err(|_| "no headers".to_owned())?;
    h.set("Content-Type", "application/json")
        .map_err(|_| "content type refused".to_owned())?;
    for (name, value) in headers.expose() {
        // The name only: the value is the credential.
        h.set(name, value)
            .map_err(|_| format!("header {name} refused"))?;
    }
    init.set_headers(&h);
    web_sys::Request::new_with_str_and_init(url, &init).map_err(|_| "not a request".to_owned())
}

/// Records every tile request this realm makes, as it completes. Nothing of
/// the address is kept but its kind.
fn follow_tile_requests() {
    let observed = Closure::<dyn FnMut(web_sys::PerformanceObserverEntryList)>::new(
        |list: web_sys::PerformanceObserverEntryList| {
            for entry in list.get_entries().iter() {
                let Ok(timing) = entry.dyn_into::<web_sys::PerformanceResourceTiming>() else {
                    continue;
                };
                let Some(kind) = kind(&timing.name()) else {
                    continue;
                };
                let source = source(timing.transfer_size(), timing.encoded_body_size());
                metrics::histogram!("tuile_tiles_fetch_seconds", "source" => source, "kind" => kind)
                    .record(timing.duration() / 1000.0);
                metrics::counter!(
                    "tuile_tiles_fetch_total",
                    "source" => source,
                    "kind" => kind,
                    "outcome" => "hit"
                )
                .increment(1);
                let bytes = timing.encoded_body_size();
                if bytes > 0.0 {
                    metrics::counter!("tuile_tiles_fetch_bytes_total", "source" => source, "kind" => kind)
                        .increment(bytes as u64);
                }
            }
        },
    );
    let observer = match web_sys::PerformanceObserver::new(observed.as_ref().unchecked_ref()) {
        Ok(observer) => observer,
        Err(e) => {
            tracing::info!("telemetry: no Performance API here ({e:?}); no request timings");
            return;
        }
    };
    let types = js_sys::Array::of1(&JsValue::from_str("resource"));
    #[allow(deprecated)]
    let init = web_sys::PerformanceObserverInit::new(&types);
    observer.observe(&init);
    // Both live as long as the realm.
    observed.forget();
    std::mem::forget(observer);
}
