// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What the viewer asks of the page it runs in: `crate::desktop`'s services,
//! answered by the host's script.
//!
//! Everything the application would say in a title bar, a menu or an alert is
//! an event for the page instead, delivered to the one callback the package
//! installs: `ready` (or `unsupported`), `loading`, `viewChanged`,
//! `attributionChanged` and `error`. Steering comes the other way, through the same queue a desktop's
//! URLs and scripts use. One viewer at a time per page, so the callback and
//! the queue are the page's.

use std::cell::{Cell, RefCell};
use std::sync::mpsc::{channel, Receiver, Sender};

use wasm_bindgen::JsValue;

use crate::snapshot::Snapshot;
use crate::steer::Request;

thread_local! {
    /// The package's event callback: `(kind, detail)`.
    static EVENTS: RefCell<Option<js_sys::Function>> = const { RefCell::new(None) };
    /// Where steering from the page is queued for the render loop.
    static OUTBOX: RefCell<Option<Sender<Request>>> = const { RefCell::new(None) };
    /// The backend the window draws through, once it does.
    static BACKEND: RefCell<Option<String>> = const { RefCell::new(None) };
    /// What was last said, so each event is said on a change only.
    static LOADING: Cell<Option<(bool, Option<u8>)>> = const { Cell::new(None) };
    static ATTRIBUTION: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Installs the page's event callback, and forgets what the last viewer said.
pub(crate) fn connect(events: js_sys::Function) {
    EVENTS.with(|e| *e.borrow_mut() = Some(events));
    BACKEND.with(|b| *b.borrow_mut() = None);
    LOADING.with(|l| l.set(None));
    ATTRIBUTION.with(|a| *a.borrow_mut() = None);
}

/// Removes it: a destroyed viewer says nothing more.
pub(crate) fn disconnect() {
    EVENTS.with(|e| *e.borrow_mut() = None);
    OUTBOX.with(|o| *o.borrow_mut() = None);
}

/// The backend the window draws through, once it does.
pub(crate) fn backend() -> Option<String> {
    BACKEND.with(|b| b.borrow().clone())
}

/// Calls the page's callback with `kind` and an object built by `detail`.
fn emit(kind: &str, detail: impl FnOnce(&js_sys::Object)) {
    let Some(events) = EVENTS.with(|e| e.borrow().clone()) else {
        return;
    };
    let object = js_sys::Object::new();
    detail(&object);
    if let Err(e) = events.call2(&JsValue::NULL, &JsValue::from_str(kind), &object) {
        tracing::warn!("the page's {kind} handler threw: {e:?}");
    }
}

fn set(object: &js_sys::Object, key: &str, value: impl Into<JsValue>) {
    // Defining a property on a fresh object cannot fail.
    let _ = js_sys::Reflect::set(object, &JsValue::from_str(key), &value.into());
}

/// The queue the render loop drains.
pub(crate) fn listen() -> Receiver<Request> {
    let (tx, rx) = channel();
    OUTBOX.with(|o| *o.borrow_mut() = Some(tx));
    rx
}

/// Queues a request from the page.
pub(crate) fn send(request: Request) {
    OUTBOX.with(|o| {
        if let Some(outbox) = o.borrow().as_ref() {
            // A closed queue is a viewer already destroyed.
            let _ = outbox.send(request);
        }
    });
}

/// A message the person asked for — the keys, say: the console.
pub(crate) fn alert(title: &str, text: &str) {
    tracing::info!("{title}\n\n{text}");
}

/// Something went wrong that the person must hear about: an `error` event.
pub(crate) fn try_alert(title: &str, text: &str) -> bool {
    emit("error", |o| {
        set(o, "title", title);
        set(o, "message", text);
    });
    true
}

/// The caption line: the console, beside the log. The page draws the
/// attribution itself, from `attributionChanged`.
pub(crate) fn caption(text: &str) {
    if !text.is_empty() {
        tracing::debug!("{text}");
    }
}

/// No clipboard is reached without the page's own gesture handling.
pub(crate) fn copy(_text: &str) -> bool {
    false
}

pub(crate) fn paste() -> Option<String> {
    None
}

/// The window draws: `ready`, with the backend. Or it never will:
/// `unsupported`, which the package turns into the typed error a host
/// catches.
pub(crate) fn drawing(outcome: Result<&str, &str>) {
    match outcome {
        Ok(backend) => {
            BACKEND.with(|b| *b.borrow_mut() = Some(backend.to_owned()));
            emit("ready", |o| set(o, "backend", backend));
        }
        Err(why) => emit("unsupported", |o| set(o, "message", why)),
    }
}

/// The indicator: `loading`, when it comes or goes, or its progress moves by a
/// whole percent.
pub(crate) fn loading(shown: bool, progress: Option<f32>) {
    let now = (shown, progress.map(|p| (p.clamp(0.0, 1.0) * 100.0) as u8));
    if LOADING.with(|l| l.replace(Some(now))) == Some(now) {
        return;
    }
    emit("loading", |o| {
        set(o, "shown", shown);
        set(
            o,
            "progress",
            progress.map_or(JsValue::NULL, |p| f64::from(p).into()),
        );
    });
}

/// The view: `viewChanged`, and `attributionChanged` when the layer's owner
/// changes.
pub(crate) fn view(snapshot: &Snapshot) {
    let v = snapshot.view;
    emit("viewChanged", |o| {
        set(o, "lon", v.lon);
        set(o, "lat", v.lat);
        set(o, "altitude", v.altitude);
        set(o, "heading", v.heading);
        set(o, "pitch", v.pitch);
    });
    let Some(layer) = snapshot.imagery else {
        return;
    };
    let changed = ATTRIBUTION.with(|a| {
        let mut said = a.borrow_mut();
        let changed = said.as_deref() != Some(layer.key.as_str());
        *said = Some(layer.key.clone());
        changed
    });
    if changed {
        emit("attributionChanged", |o| {
            set(o, "layer", layer.key.as_str());
            set(o, "name", layer.name.as_str());
            set(o, "attribution", layer.attribution.as_str());
        });
    }
}
