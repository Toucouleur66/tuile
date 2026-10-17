<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# web-viewer-vite — the package from a bundler

The same package as `examples/web-viewer`, imported by name from a Vite application in
TypeScript. It shows that the worker and the wasm module are found and shipped by the
bundler with no configuration of the package's own.

```sh
packages/tuile-viewer/build.sh
cd examples/web-viewer-vite
npm install
npm run build        # tsc --noEmit, then vite build
```

The page fetches its token from `/api/catalog-token`, an endpoint of the host's own: a
public page must not carry a secret.
