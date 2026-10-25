// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The decode workers' entry point: the pool itself is `tuile_web::decode`,
//! shared with every host that runs the geometry server in a browser.

use wasm_bindgen::prelude::*;

/// The decode worker's entry point, called by the package's decode worker
/// script once the module is instantiated: answers jobs until it is
/// terminated.
#[wasm_bindgen(js_name = runDecodeWorker)]
pub fn run_decode_worker() {
    super::log::install();
    tuile_web::decode::serve_jobs(tuile_planetary::jobs::serve);
}
