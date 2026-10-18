// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// The viewer's geometry server, in this worker: the same wasm module as the
// page, entering through `runWorker`. A message the page sends before the
// module is ready is kept and handed over.

import init, { runWorker } from "./wasm/tuile_viewer.js";

// Written out here, literally, so a bundler sees the decode worker's script
// and ships it with everything it imports.
const makeDecodeWorker = () =>
  new Worker(new URL("./decode-worker.js", import.meta.url), { type: "module" });

// No top-level await: a bundler may ship this worker as a classic script.
const early = [];
self.onmessage = (event) => early.push(event);
init().then(() => {
  runWorker(makeDecodeWorker);
  for (const event of early) self.onmessage?.(event);
});
