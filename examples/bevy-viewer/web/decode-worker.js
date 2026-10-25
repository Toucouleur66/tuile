// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev
//
// One of the decode workers the page starts (`tuile_bevy::web`): the same
// wasm module again, entering through `runDecodeWorker`. The first message is
// the module itself, already compiled, so nothing is fetched or compiled
// twice; every later one is a job, answered with its buffer transferred.
// `?pkg=` names the build the page runs, so the worker loads the same one.

const pkg = new URL(self.location.href).searchParams.get("pkg") === "webgl2" ? "webgl2" : "webgpu";

self.onmessage = (event) => {
  self.onmessage = null;
  import(`./pkg/${pkg}/tuile_bevy_viewer.js`)
    .then(({ default: init, runDecodeWorker }) =>
      init({ module_or_path: event.data.module }).then(() => runDecodeWorker()))
    .catch((e) => self.postMessage({ failed: String(e?.message ?? e) }));
};
