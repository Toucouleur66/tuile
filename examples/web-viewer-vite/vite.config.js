// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

// Browsers with WebGPU or WebGL2 all run ES2022: top-level await included.
// `npm run dev` passes the catalog to the proxy on this machine, which holds the
// token (`tuile-catalog-proxy`); in production the host serves it at the same path.
export default {
  build: { target: "es2022" },
  worker: { format: "es" },
  server: { proxy: { "/catalog": "http://127.0.0.1:8091" } },
};
