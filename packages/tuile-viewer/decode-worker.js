// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// One of the decode workers the geometry server's worker starts: the same
// wasm module again, entering through `runDecodeWorker`. The first message is
// the module itself, already compiled, so nothing is fetched or compiled
// twice; every later one is a job, answered with its buffer transferred.

import init, { runDecodeWorker } from "./wasm/tuile_viewer.js";

self.onmessage = (event) => {
  self.onmessage = null;
  init({ module_or_path: event.data.module }).then(
    () => runDecodeWorker(),
    (e) => self.postMessage({ failed: String(e?.message ?? e) }),
  );
};
