<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Web and native viewers on one base

Design, 2026-10-17, before the graphics budget (PR #64). It follows the status review in
`docs/explorations/web-viewer-status.md` (PR #65) and the manifesto (PR #59): one
foundation, every render mode. A fork of the engine for one mode is a design error.

## 1. The problem

On `main` the browser has its own viewer, and that viewer is a fork:

- `crates/tuile-web` runs the real geometry server in a Web Worker. That part is right.
- `examples/web-viewer` draws with its own JavaScript scene: its own tile bookkeeping, a
  GLSL copy of the imagery mosaic and the lighting, and a JavaScript camera. None of the
  renderer's guarantees reach it: no ancestor fallback, no new-before-old swap, no
  stitching shell, no on-screen controls, no attribution. It has not built since August.
- `examples/wasm-globe` is a fourth traversal driver, with its own residency and request
  loop.

Meanwhile the native viewer (`tuile-viewer` + `tuile-wgpu` + `tuile-ui` +
`tuile-camera`) carries every one of those guarantees, and wgpu on WebGPU is already
proven in this repository by the film path (`tuile-film-web`).

## 2. Decision

**The web viewer and the native viewer converge inside `tuile-viewer`.** No new crate.

- `tuile-viewer` holds the platform-agnostic core and three platform shells behind
  `cfg`.
- **The renderer is `tuile-wgpu` everywhere**: the same `ContentPump`, the same
  `TileRenderer`, the same WGSL, the same `tuile-ui` overlay.
- The browser gets WebGPU, with a **WebGL2 fallback** through wgpu's GL backend.

### 2.1 Layering

```text
                      tuile-viewer
  ┌──────────────────────────────────────────────────────────────────┐
  │ core (every target)                                               │
  │   app/      the winit application: frame step, input, gestures,   │
  │             touch, loading indicator, lifecycle, demand, pacing,  │
  │             imagery switch, attribution, warm-up                  │
  │   profile   desktop / handheld / browser numbers                  │
  │   backdrop, steer (URL vocabulary), embed (identity, layers)      │
  │   session   what a shell hands the app: stream, heights, detail,  │
  │             layer budget, switcher, gate                          │
  ├───────────────────────┬────────────────────┬─────────────────────┤
  │ shell: desktop        │ shell: ios         │ shell: web (wasm32) │
  │ (macOS/Linux/Windows) │                    │                     │
  │ tokio runtime thread, │ the same, with the │ page: winit web     │
  │ foyer store, HTTP     │ handheld profile,  │ backend on a canvas,│
  │ cache, menus, tray,   │ UIKit glue, device │ wgpu WebGPU/WebGL2, │
  │ journal, signals,     │ memory/thermal     │ JS API (Viewer)     │
  │ location, scripting,  │ (PR #62)           │ worker: the geometry│
  │ tapes, metrics        │                    │ server over         │
  │                       │                    │ postMessage         │
  └───────────────────────┴────────────────────┴─────────────────────┘
        │                         │                        │
  tuile-wgpu (ContentPump, TileRenderer, OverlayRenderer, WGSL) — all
  tuile-ui (compass, zoom, spinner)                              — all
  tuile-camera (controller, ground)                              — all
  tuile-planetary / tuile-core (globe, GeometryServer, protocol) — all
  tuile-web (browser transport: fetch + postMessage GeometryStream) — web
```

**Why winit stays in the core.** The core is the winit `ApplicationHandler`. winit is
the platform abstraction for windows and input on all three targets: the desktop, iOS
(PR #62) and the browser (its web backend binds a `<canvas>`, pointer, touch, wheel,
keyboard, resize and visibility events). Abstracting winit away would mean writing a
second input layer that winit already is. What stays out of the core is everything
winit does *not* abstract: the async runtime, the disk, the menus, the system services.

**What the core may not contain:** tokio's multi-thread runtime, foyer, the metrics
exporter, files, threads, signals, objc2, and `std::time::Instant`, which panics on
wasm32. `web_time::Instant` is used instead: it *is* `std::time::Instant` on native,
and `performance.now()` in the browser. `web-time` is already in the lock file through
winit.

### 2.2 The session seam

The app never builds its session; a shell hands it one. A session is:

| Handle | Desktop / iOS | Web |
|---|---|---|
| `stream: GeometryStream` | `InProcessStream` to a server on a tokio thread | `WorkerStream`: the same protocol over `postMessage` to a worker |
| `heights: TerrainHeights` | filled by the loader | filled on the page from each `Content`'s own vertices, forgotten on `Evict` |
| `detail: ImageryDetail` | shared atomic with the loader | the same type on the page; the stream forwards it to the worker when it changes |
| `layer_budget: LayerBudget` | shared atomic | the same; forwarded the same way |
| `switcher` (imagery layer) | resolves on the runtime | posts the layer to the worker, which resolves it there |
| `gate` (pause loads) | `tokio::sync::Notify` | posts pause/resume to the worker, which shuts its own `Gate` |
| store, server thread | foyer, joined on exit | none yet; the worker is terminated on `destroy()` |

The type names are the same in both shells, so the app's code does not branch.

### 2.3 The worker

**`tuile-web` stays a separate crate**, and becomes what its description says, the
browser *transport*:

- `WebFetcher` and `WebIonHttp` (unchanged), the counterpart of `tuile-native-fetchers`;
- **new:** `wire`, a binary codec of `ClientMessage` and `ServerMessage` (decoded
  content, ancestry, imagery layers, priming), compiled on every target so it is
  unit-tested on the host;
- **new:** `WorkerStream` (page side, a `GeometryStream`) and the matching endpoint on
  the worker side.

This is the third binding of the streaming protocol, beside in-process and WebSocket,
and like them it knows nothing about viewers. **Deleted:** `tuile-web/src/worker.rs`,
the JS-object encoder that fed the THREE.js page.

**The worker's entry point lives in `tuile-viewer`'s web shell**, not in its own crate.
A worker does not need its own wasm module: it needs its own exported function. The
page and the worker load the *same* wasm module (`--target web` modules run in module
workers). The worker assembles the globe exactly as the native session does
(`globe::assemble`, without the disk store, with the decode inline on the worker's
thread) and runs `GeometryServer::run` with `spawn_local`. That is viewer logic, so it
lives with the viewer.

Imagery pixels cross with every tile in this first version (the buffers are
*transferred*, not copied, across `postMessage`). Sending each `(source, coord)` once,
as the old encoder did, needs the page to tell the worker when the GPU lets a texture go;
that is a protocol addition, and it belongs with the budget's `Credit` flow.

### 2.4 The web package

The web build of `tuile-viewer` is an **ES module package** any page or application
imports, not only a demo page.

- `packages/tuile-viewer/` (npm name proposal `@lapoule-dev/tuile-viewer`; not
  published, the owner publishes): `package.json` with `exports`, a hand-written
  `index.js` + `index.d.ts`, `worker.js`, and the wasm-bindgen output under `wasm/`.
  Assets are resolved with `new URL(…, import.meta.url)`, so Vite, webpack, esbuild and a
  plain `<script type="module">` work without copying files.
- Built by `scripts/build-web-package.sh`: `cargo rustc -p tuile-viewer --crate-type
  cdylib --target wasm32-unknown-unknown`, then `wasm-bindgen --target web`. The crate's
  manifest keeps `crate-type = ["rlib"]`, so native builds do not link a `cdylib` they
  never use.
- Versioned with the workspace (0.7.x), `MIT OR Apache-2.0`.

**Public API** (typed, small):

```ts
createViewer(container: HTMLElement | HTMLCanvasElement, options): Promise<Viewer>

options = {
  sources: {
    catalog?: { url?: string; token?: string; getToken?: () => Promise<string> };
    terrain: { asset: number };
    imagery: Array<{ key; name; attribution; asset?: number;
                     template?: string; scheme?: 'webmercator' | 'geographic'; maxLevel? }>;
  };
  profile?: 'auto' | 'desktop' | 'handheld';
  backend?: 'auto' | 'webgpu' | 'webgl2';
  view?: { lon; lat; altitude; heading?; pitch? };
  ui?: boolean;                                   // tuile-ui overlay
  attribution?: 'bottom-right' | 'bottom-left' | 'none';
  showUnsupportedMessage?: boolean;
}

Viewer: backend, setView, flyTo, getView, on(event, cb), resize, pause, resume, destroy
events: ready { backend }, loading { fraction }, error, viewChanged, attributionChanged
errors: UnsupportedError (no WebGPU and no WebGL2), StartError
```

- The token is the host's: the name of the catalog and the label of its token field are
  the host page's. No third-party brand appears in the package's API or README.
- `destroy()` exits the event loop, drops the device and the surface, terminates the
  worker and removes every listener.
- **Room for the budget's control API** (PR #64 §4c: budget, GPU, frame readback,
  scripted moves) as a future `viewer.debug` namespace. Nothing is implemented now; the
  name is reserved in the typings.

### 2.5 Backends: WebGPU, then WebGL2

- `wgpu` with `webgpu` and `webgl` on wasm32. The instance is made with
  `wgpu::util::new_instance_with_webgpu_detection`, so a browser without WebGPU gets the
  GL backend. `backend` forces one, for hosts and tests.
- The viewer path uses no compute pipeline, no storage buffer and no storage texture.
  This is a **constraint from now on**: anything added to the shared viewer path
  (Hillaire LUTs, the budget's GPU accounting) must either avoid compute or ship a
  fragment / render-to-texture variant.
- On GL the device is asked for `Limits::downlevel_webgl2_defaults()` raised to what the
  adapter reports. What the renderer needs and WebGL2 gives:
  - `Depth24PlusStencil8`, the stencil for the fallback pass: native in WebGL2;
  - 4× MSAA: renderbuffers;
  - mip chains: built on the CPU (`raster::mip_chain`), no blit;
  - sampled textures per stage: the imagery slots are fitted to
    `max_sampled_textures_per_shader_stage` (16 on WebGL2);
  - the per-tile uniform (8 272 bytes) is under WebGL2's 16 KiB uniform binding.
- A host test validates every WGSL module of the viewer path with naga and translates it
  to GLSL ES 3.00 under WebGL2 downlevel capabilities, so a shader that breaks WebGL2
  breaks CI.

## 3. What moves where

| From | To |
|---|---|
| `std::time::Instant` throughout `tuile-viewer` and `tuile-wgpu`'s pump | `web_time::Instant` (identical on native) |
| `ContentPump` / `TileRenderer` / overlay on the page | already shared, now driven by the same `App` on the web |
| the catalog `Sources` of `examples/wgpu-viewer` (`Ion`) | `tuile_cesium_ion::sources::IonSources` (the connector, generic over the transport), used by the native host and the web shell |
| `tuile-viewer`'s tokio, foyer, metrics, ctrlc, rlimit, tape, objc2 dependencies | `[target.'cfg(not(target_arch = "wasm32"))'.dependencies]` |
| the session handles (`Started`) | `session` module, one struct per shell with the same field names |
| the window and GPU bring-up | split in two: the window (sync) and the device (async; `pollster` on native, `spawn_local` in the browser) |
| `ImageryDetail`'s target | a public getter in `tuile-planetary`, so the web stream can forward it |

## 4. What is deleted

- `examples/web-viewer/www/main.js`, `ground-material.js`, `engine-worker.js`: the
  THREE.js scene, its GLSL ground material and its JavaScript camera.
- `crates/tuile-web/src/worker.rs`: the JS-object encoder for that scene.
- `examples/wasm-globe` (Rust crate and page): the fourth traversal driver. Its one use,
  a geometry report in a browser, is what the viewer's own status line and the smoke
  test give now.

`examples/web-viewer` becomes a consumer of the package: an HTML page, a few lines of
JavaScript that import it, and a dev server script. A second example,
`examples/web-viewer-vite`, imports the package from a bundler build.

## 5. The token in the browser

- Real users type it in the host page's own field; it reaches the worker by
  `postMessage`, never a URL, never a log line.
- For development, `scripts/serve-web-viewer.py` serves the example on `127.0.0.1` and
  answers `GET /dev-token` with the value read from the repository's `.env` at request
  time. It is never printed, never written to disk, never in a URL, and the endpoint
  exists only on the loopback dev server.
- A public page must not embed a secret token: use `getToken` (a host endpoint that
  mints a scoped token) or a proxy.

## 6. Parity, after the change

| Feature | Native | Web |
|---|---|---|
| Terrain + imagery mosaic | yes | yes, same shader |
| Never-black ground (shell, ancestors, new before old) | yes | yes, same pump |
| Stitching | yes | yes |
| Camera controls, touch, keyboard, `tuile-ui` overlay | yes | yes, same `App` |
| Loading indicator | yes | yes |
| Attribution | title bar + menu | page overlay, from the same layer list |
| Pause when hidden | lifecycle | `visibilitychange` → lifecycle, gate to the worker |
| Atmosphere (aerial perspective) | yes | yes, it is in the shared renderer |
| Hillaire sky (PR #61) | when it lands in `tuile-atmosphere` | the same, under the no-compute constraint |
| Overlays API, stereo | film path | not yet: they come with the shared layers |
| Persistent store | foyer | none (in-memory only); the browser store comes with the budget |
| Steering URLs, scripting, tapes, metrics endpoint, menus | yes | no: desktop shell |

## 7. Budget work (PR #64) slots in without reshaping

- `tuile_core::budget` is pure and builds for wasm32: both shells get it through the
  core.
- `Credit { bytes }` travels on the stream. The web binding forwards `ClientMessage`
  unchanged, so a new variant only needs the codec to learn it.
- The headless control API (§4c) is a desktop socket; the browser's equivalent is the
  reserved `viewer.debug` namespace, driven by the same smoke test.
- `Tier::Stored` on the web needs a browser store (IndexedDB or OPFS) answering
  `holds()`. It plugs into the worker where the desktop plugs foyer.

## 8. Open decisions for the owner

1. **WebGL2 fallback** is in (wgpu GL backend). Whether to keep it once the budget adds
   GPU work: anything in the shared path must stay compute-free or carry a downlevel
   variant.
2. npm name and publishing.
3. Hosting of a public demo and its token policy (scoped tokens via `getToken`, or a
   proxy).
