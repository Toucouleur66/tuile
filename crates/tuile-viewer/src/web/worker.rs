// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The worker: the geometry server a page draws from, assembled exactly as
//! the desktop's is.
//!
//! The same [`crate::globe::assemble`] over the page's sources, the same gate
//! in front of the loader, the same `GeometryServer`, run by the worker's
//! event loop. What differs is only what a worker has: no tile store, and one
//! thread, so decoding runs inline. The stream to the page is
//! [`tuile_web::post::serve`]; what the page asks beside it arrives as
//! [`ToWorker`] and is done to the loader's own handles.

use std::sync::Arc;

use tuile_core::source::TileLoader;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

use super::control::{ToPage, ToWorker};
use super::options::{ProfileChoice, SourcesSpec};
use super::sources::PageSources;
use crate::gate::{Gate, Gated};
use crate::globe::Layers;
use crate::profile::Profile;

/// The worker's entry point, called by the package's worker script once the
/// module is loaded: waits for the page's `start`.
#[wasm_bindgen(js_name = runWorker)]
pub fn run_worker() {
    super::log::install();
    let on_start =
        Closure::<dyn FnMut(web_sys::MessageEvent)>::new(|event: web_sys::MessageEvent| {
            match serde_wasm_bindgen::from_value::<ToWorker>(event.data()) {
                Ok(ToWorker::Start {
                    sources,
                    opening,
                    profile,
                    device_memory,
                }) => {
                    // The page speaks no more until it hears back; nothing else
                    // is listened for until the session decides who listens.
                    scope().set_onmessage(None);
                    wasm_bindgen_futures::spawn_local(async move {
                        let profile =
                            Profile::browser(profile == ProfileChoice::Handheld, device_memory);
                        if let Err(reason) = start(sources, opening, profile).await {
                            tracing::error!("the session could not start: {reason}");
                            post(&ToPage::Failed { reason });
                        }
                    });
                }
                Ok(other) => tracing::warn!("a message before the start was ignored: {other:?}"),
                Err(e) => tracing::error!("the page said something unreadable: {e}"),
            }
        });
    scope().set_onmessage(Some(on_start.as_ref().unchecked_ref()));
    // The handler lives as long as the worker: it is replaced, never dropped.
    on_start.forget();
}

/// Brings the session up and serves it to the page.
async fn start(spec: SourcesSpec, opening: usize, profile: Profile) -> Result<(), String> {
    let (terrain, layers) = spec.choices()?;
    let sources = Arc::new(PageSources::new(spec)?);
    let globe = crate::globe::assemble(
        sources,
        None,
        &terrain,
        &layers,
        opening,
        Arc::new(tuile_core::offload::Inline),
    )
    .await
    .map_err(|e| format!("{e:#}"))?;
    let gate = Gate::default();
    let loader: Arc<dyn TileLoader> = Arc::new(Gated {
        inner: globe.loader,
        gate: gate.clone(),
    });
    let config = profile.traversal(Some(profile.pinned_level));
    let (stream, server) = tuile_core::runtime::in_process_with(globe.tree, loader, config);
    // The worker's one thread is also where the page's messages arrive — the
    // camera, the acks: a burst answered from memory must let them in.
    let server = server.with_breather(BURST, Arc::new(breathe));
    wasm_bindgen_futures::spawn_local(async move {
        server.run().await;
        tracing::info!("the geometry server has stopped");
    });
    let handles = Handles {
        detail: globe.detail,
        budget: globe.budget,
        layers: globe.layers,
        choices: layers,
        gate,
    };
    let connection = tuile_web::post::serve(stream, move |value| handles.heard(value));
    // The worker is the session: the connection lasts as long as it does.
    std::mem::forget(connection);
    post(&ToPage::Started);
    Ok(())
}

/// Events the server handles back to back before the worker's messages get
/// their turn: a few milliseconds of work at most.
const BURST: usize = 16;

/// A pause on the worker's timer: a macrotask, so the messages queued behind
/// the server's burst are delivered before it goes on. A promise alone would
/// not do — its continuation is a microtask, which runs before them.
fn breathe() -> tuile_core::runtime::Pause {
    let promise = js_sys::Promise::new(&mut |resolve, _| {
        let _ = scope().set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, 0);
    });
    Box::pin(async move {
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    })
}

/// The loader's handles, which the page's messages act on.
struct Handles {
    detail: tuile_planetary::ImageryDetail,
    budget: tuile_planetary::LayerBudget,
    layers: Layers,
    choices: Vec<crate::embed::ImageryChoice>,
    gate: Gate,
}

impl Handles {
    fn heard(&self, value: JsValue) {
        match serde_wasm_bindgen::from_value::<ToWorker>(value) {
            Ok(ToWorker::Detail { metres }) => self.detail.set_target_texel_spacing(metres),
            Ok(ToWorker::Budget { reported }) => self.budget.set_from_device(reported),
            Ok(ToWorker::Pause { paused: true }) => self.gate.shut(),
            Ok(ToWorker::Pause { paused: false }) => self.gate.open(),
            Ok(ToWorker::Imagery { layer }) => {
                let Some(choice) = self.choices.get(layer).cloned() else {
                    post(&ToPage::Switched {
                        layer,
                        outcome: Err(format!("there is no layer {layer}")),
                    });
                    return;
                };
                let layers = self.layers.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    let outcome = layers.switch_to(layer, &choice).await;
                    post(&ToPage::Switched { layer, outcome });
                });
            }
            Ok(ToWorker::Start { .. }) => tracing::warn!("a second start was ignored"),
            Err(e) => tracing::error!("the page said something unreadable: {e}"),
        }
    }
}

fn scope() -> web_sys::DedicatedWorkerGlobalScope {
    js_sys::global().unchecked_into()
}

fn post(message: &ToPage) {
    match serde_wasm_bindgen::to_value(message) {
        Ok(value) => {
            if let Err(e) = scope().post_message(&value) {
                tracing::error!("the page could not be told: {e:?}");
            }
        }
        Err(e) => tracing::error!("a message to the page could not be written: {e}"),
    }
}
