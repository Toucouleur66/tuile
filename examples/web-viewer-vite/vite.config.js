// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

// Browsers with WebGPU or WebGL2 all run ES2022: top-level await included.
export default {
  build: { target: "es2022" },
  worker: { format: "es" },
};
