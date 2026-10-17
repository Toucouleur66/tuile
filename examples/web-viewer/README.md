<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# web-viewer — the globe in a page

A host page for the `@lapoule-dev/tuile-viewer` package (`packages/tuile-viewer`), over
Cesium ion's terrain and imagery. It is what a real user writes: one import and one
`createViewer` call. The viewer is the desktop's application, renderer and controls,
compiled to WebAssembly.

```sh
packages/tuile-viewer/build.sh            # once, and after any Rust change
python3 examples/web-viewer/serve.py      # http://127.0.0.1:8080/
```

- The page asks for an access token in a field of its own. It keeps the token in the
  page and never stores it.
- For development, `serve.py` answers `/dev-token` with `TUILE_ION_TOKEN` from the
  repository's `.env`, so the field is skipped. It does so only on 127.0.0.1, without
  printing or writing the value. Point it at another file with `--env PATH`.
- `?backend=webgl2` forces the WebGL2 path; `?backend=webgpu` refuses the fallback.

`examples/web-viewer-vite` imports the same package from a Vite build.
