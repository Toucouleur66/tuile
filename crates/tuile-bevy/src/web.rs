// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The globe in a browser: what a page's Bevy application needs beside the
//! plugin.
//!
//! The application and the geometry server share the page's thread. The CPU
//! work that would hold it — decoding terrain and imagery — goes to **decode
//! workers**, the same pool tuile's own web viewer uses (`tuile_web::decode`):
//! each worker is the application's own wasm module again, instantiated from
//! the compiled module the page already holds, entering through the
//! `runDecodeWorker` export this module adds to it.
//!
//! What the host does:
//!
//! 1. ship a decode worker script beside its module, which imports the
//!    module's JS bindings and calls `runDecodeWorker()` once instantiated
//!    (`examples/bevy-viewer/web/decode-worker.js`);
//! 2. leave its `main` at once inside such a worker — the bindings call
//!    `main` on every instantiation: [`is_decode_worker`];
//! 3. name that script's URL to the plugin
//!    ([`TuilePlugin::with_decode_workers`](crate::TuilePlugin::with_decode_workers)),
//!    or start the pool itself with [`start_decode_workers`].
//!
//! Without workers the decoding runs on the page's thread: slower to
//! converge, never a hole.

use std::sync::Arc;

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

/// The decode worker's entry point, called by the host's decode worker
/// script once the module is instantiated: answers jobs until terminated.
#[wasm_bindgen(js_name = runDecodeWorker)]
pub fn run_decode_worker() {
    tuile_web::decode::serve_jobs(tuile_planetary::jobs::serve);
}

/// Whether this instance of the module runs in a worker rather than on the
/// page: a host's `main` returns at once when it does.
pub fn is_decode_worker() -> bool {
    js_sys::global().dyn_ref::<web_sys::Window>().is_none()
}

/// Starts the decode workers from the module worker script at `script`
/// (relative to the page): one per core the browser reports less one, at
/// most `most` (four when `None`; zero starts none).
pub fn start_decode_workers(script: &str, most: Option<u32>) {
    let count = tuile_web::schedule::pool_size(most, tuile_web::decode::hardware_concurrency());
    if count == 0 {
        return;
    }
    let script = script.to_owned();
    let make = Closure::<dyn Fn() -> JsValue>::new(move || {
        let options = web_sys::WorkerOptions::new();
        options.set_type(web_sys::WorkerType::Module);
        match web_sys::Worker::new_with_options(&script, &options) {
            Ok(worker) => worker.into(),
            Err(e) => {
                tracing::warn!("a decode worker could not be made: {e:?}");
                JsValue::NULL
            }
        }
    })
    .into_js_value()
    .unchecked_into::<js_sys::Function>();
    tuile_web::decode::start(make, count);
}

/// A pause on the page's timer, between two bursts of the server's work: a
/// macrotask, so a frame and the input queued behind the burst go first.
pub(crate) fn breathe() -> tuile_core::runtime::Pause {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        if let Some(window) = web_sys::window() {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0);
        }
    });
    Box::pin(async move {
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    })
}

/// The server's clock: `performance.now()`.
pub(crate) fn clock() -> tuile_core::runtime::Clock {
    let origin = web_time::Instant::now();
    Arc::new(move || {
        tuile_core::budget::Micros(origin.elapsed().as_micros().min(u128::from(u64::MAX)) as u64)
    })
}
