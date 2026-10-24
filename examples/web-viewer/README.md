<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# web-viewer — the globe in a page

A host page for the `@lapoule-dev/tuile-viewer` package (`packages/tuile-viewer`), over
Cesium ion's terrain and imagery. It is what a real user writes: one import and one
`createViewer` call. The viewer is the desktop's application, renderer, controls, menu and
messages, compiled to WebAssembly.

```sh
scripts/try-viewer.sh web          # builds the package, then http://localhost:8091/
```

- The page holds no token. Its catalog is `/catalog/`, answered by `tuile-catalog-proxy`
  (`examples/catalog-proxy`), which the script starts with `TUILE_ION_TOKEN` read from the
  repository's `.env` on the server side, and which also serves this page and the package.
- `?backend=webgl2` forces the WebGL2 path; `?backend=webgpu` refuses the fallback.
- `?terrain=<layer.json URL>&imagery=<template>` shows a terrain and an imagery template named
  by address instead, with no catalog: `scheme=geographic|webmercator` and `maxLevel=N`
  describe the template's grid, `lon`, `lat`, `altitude`, `heading` and `pitch` the view.
  `examples/heightfield-serve` prints such a URL for its synthetic relief.

`examples/web-viewer-vite` imports the same package from a Vite build.
