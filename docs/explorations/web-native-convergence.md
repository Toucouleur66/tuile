<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Web and native viewers on one base

Design, 2026-10-17, written before the code and kept in step with what was built. It comes
before the graphics budget (PR #64) and follows two earlier documents:

- the status review in `docs/explorations/web-viewer-status.md` (PR #65);
- the manifesto (PR #59): one foundation, every render mode. A fork of the engine for one
  mode is a design error.

## 1. The problem

On `main` the browser had its own viewer, and that viewer was a fork:

- `crates/tuile-web` ran the real geometry server in a Web Worker. That part was right.
- `examples/web-viewer` drew with its own JavaScript scene: its own tile bookkeeping, a
  GLSL copy of the imagery mosaic and the lighting, and a JavaScript camera.
  - None of the renderer's guarantees reached it: no ancestor fallback, no
    new-before-old swap, no stitching shell, no on-screen controls, no attribution.
  - It had not built since August.
- `examples/wasm-globe` was a fourth traversal driver, with its own residency and request
  loop.

The native viewer, meanwhile, carries every one of those guarantees:

- `tuile-viewer`, `tuile-wgpu`, `tuile-ui` and `tuile-camera`;
- wgpu on WebGPU was already proven in this repository by the film path.

## 2. Decision

**The web viewer and the native viewer converge inside `tuile-viewer`.** No new crate.

- `tuile-viewer` holds the platform-agnostic application and two shells behind `cfg`:
  - `native`, for the desktop and the phone;
  - `web`, for wasm32.
- **The renderer is `tuile-wgpu` everywhere**: the same `ContentPump`, the same
  `TileRenderer`, the same WGSL, the same `tuile-ui` overlay.
- The browser draws with WebGPU, and falls back to **WebGL2** through wgpu's GL backend.

### 2.1 Layering

```text
                         tuile-viewer
  ┌───────────────────────────────────────────────────────────────────┐
  │ shared (every target)                                              │
  │   app/      the winit application: frame step, input, gestures,    │
  │             touch, loading indicator, lifecycle, demand, pacing,   │
  │             imagery switch, glides (flight.rs), warm-up            │
  │   launch    Launch and Retries: a start-up collected without       │
  │             blocking, retried with back-off                        │
  │   globe     assemble(): the globe over a host's Sources, any       │
  │             store, any decode offload; Layers::switch_to           │
  │   profile   desktop / handheld / browser numbers                   │
  │   desktop   platform services: alerts, caption, clipboard,         │
  │             steering queue, and drawing/loading/view for a page    │
  │   steer, start, snapshot, embed, gate, backdrop, ...               │
  ├──────────────────────────────────┬────────────────────────────────┤
  │ native/ (desktop, phone)         │ web/ (wasm32)                  │
  │  main and the command line,      │  page.rs: startViewer, the     │
  │  the tokio runtime, the server   │   page's event loop, WebViewer │
  │  thread, the foyer store         │  session.rs: the worker and    │
  │  (store.rs), env settings;       │   the proxies to it            │
  │  menus, location, scripting and  │  worker.rs: runWorker, the     │
  │  the phone's UIKit glue stay in  │   geometry server              │
  │  their platform `system` modules │  sources.rs, options.rs,       │
  │                                  │   control.rs, system.rs, log.rs│
  │                                  │  screen.rs: caption, alerts    │
  │                                  │  menu.rs: menus from a button  │
  └──────────────────────────────────┴────────────────────────────────┘
        tuile-wgpu, tuile-ui, tuile-camera, tuile-planetary, tuile-core: all targets
        tuile-web (fetch, and GeometryStream over postMessage): wasm32
        packages/tuile-viewer: the ES module wrapping the wasm build
```

**Why winit stays in the shared application.** The application is the winit
`ApplicationHandler`. winit is already the platform abstraction for windows and input on
all three targets:

- the desktop;
- iOS (PR #62);
- the browser: its web backend binds a `<canvas>`, pointer, touch, wheel, keyboard,
  resize, intersection and page visibility.

Abstracting winit away would mean writing a second input layer that winit already is.
What stays out of the shared code is everything winit does *not* abstract: the async
runtime, the disk, the menus, the system services.

**What the shared code may not contain:**

- tokio's multi-thread runtime, foyer, the metrics exporter, files, threads, signals,
  objc2;
- `std::time::Instant`, which panics on wasm32. `web_time::Instant` is used instead: it
  *is* `std::time::Instant` on native, and `performance.now()` in a browser. It was
  already in the lock file through winit.

Native-only items (the command line, `Profile::current`, the store path, `adopt`) are
gated with `cfg(not(target_arch = "wasm32"))`. No `allow(dead_code)` was added.

### 2.2 The session seam

The application never builds its session; a shell hands it one, under the same names
(`crate::shell::{Started, Detail, Budget, Gate}`):

| Handle | Desktop and phone | Web |
|---|---|---|
| `stream` | `InProcessStream` to the server on its own thread | `PageStream`: the protocol over `postMessage` to the worker |
| `heights` | filled by the loader | filled on the page from each `Content`'s highest vertex |
| `detail` | the loader's `ImageryDetail` | a proxy that posts the texel spacing when it changes |
| `layer_budget` | the loader's `LayerBudget` | a local `LayerBudget`, plus a post of what the device reported |
| `switcher` | `Layers::switch_to` on the runtime | a post; the worker runs `Layers::switch_to` and posts the outcome back |
| `gate` | `tokio::sync::Notify` | a post; the worker shuts its own `Gate` |
| `close`, `suspend_store`, `resume_store` | flush foyer, join the server | terminate the worker; no store |

### 2.3 The worker and `tuile-web`

**`tuile-web` stays a separate crate.** It is the browser *transport*, the counterpart of
`tuile-native-fetchers`, and knows nothing about viewers:

- `WebFetcher` and `WebIonHttp`, over the global `fetch`;
- `wire`: a binary codec of `ClientMessage` and `ServerMessage`. It is compiled and tested
  on the host. Its matches are exhaustive, so a new protocol variant (the budget's
  `Credit`) fails to compile until it is given an encoding;
- `post`: `WorkerStream` on the page and `serve()` in the worker. This is the third
  binding of the streaming protocol, beside the in-process one and the WebSocket.

**Deleted from `tuile-web`:**

- `worker.rs`, the JS-object encoder of the THREE.js page;
- on the branch built from PR #66, its `forward.rs`. It applied the consumer's rules
  (acks, Fill refused over real content, Retire narrowed to stand-ins) in the worker,
  because the page could not. The page now runs `tuile-wgpu`'s `ContentPump`, which
  applies exactly those rules, so a second copy would be duplicated logic.

**The worker's entry point is in `tuile-viewer`'s web shell.** A worker needs its own
exported function, not its own wasm module, so:

- the page and the worker load the *same* module: `startViewer` on the page, `runWorker`
  in the worker;
- the worker assembles the globe as the desktop does: `globe::assemble`, then `Gated`,
  then `in_process_with`;
- it has no store, and decoding runs inline (`offload::Inline`) on its one thread;
- the page's handles post small `control::ToWorker` objects, and the worker answers
  with `ToPage`.

**Imagery** pixels cross with every tile in this version, transferred rather than copied.
Sending each `(source, coord)` once needs the page to tell the worker when the GPU lets a
texture go. That is a protocol addition, and it belongs with the budget's `Credit` flow.

### 2.4 The web package

`packages/tuile-viewer` is the web build as an ES module package. The npm name proposal is
`@lapoule-dev/tuile-viewer`; it is not published, which is the owner's decision. It holds:

- `package.json` with `exports`;
- `index.js` and `index.d.ts`, written by hand;
- `worker.js`, with no top-level await, so a bundler may ship it as a classic script;
- `wasm/`, which `build.sh` produces:
  - `cargo rustc -p tuile-viewer --crate-type cdylib --target wasm32-unknown-unknown`;
  - then `wasm-bindgen --target web` at the locked version.
  - The crate itself stays `rlib`, so native builds link nothing they do not use.

How it resolves its files:

- The worker is created in `index.js`, written literally as
  `new Worker(new URL("./worker.js", import.meta.url), { type: "module" })`.
- Rust receives a factory, not a URL, and calls it for every start-up attempt.
- That literal form is what lets Vite, webpack and esbuild ship the worker with its
  imports. The Vite example's build emits one wasm asset shared by the page and the
  worker.

**API:**

- `createViewer(container, options)` resolves on the first `ready` event.
- It rejects with `UnsupportedError` (neither backend, or not the forced one) or
  `StartError` (wrong options). A session that cannot start is not a rejection: the
  viewer says it over the view and tries again, as the phone does.
- Options: `sources` (the catalog proxy's `url`, and/or URL templates), `name`,
  `profile`, `backend`, `view`, `ui`, `attribution` (the caption's corner),
  `showUnsupportedMessage`, `rememberView`. No token: see §5.
- Viewer: `backend`, `setView`, `flyTo` (a glide), `getView`, `on`, `resize`, `pause`,
  `resume`, `destroy`.
- Events: `ready`, `loading`, `error`, `viewChanged`, `attributionChanged`.
- `debug` is reserved in the typings for the budget's control API (PR #64 §4c). Nothing
  is implemented yet.

**One viewer per page at a time.**

- winit runs one event loop per page and never starts a second.
- The web shell starts the loop with the first viewer and keeps it. A viewer destroyed
  is taken out of the loop, and the next one is handed to it.
- Each handle carries its viewer's number, so a stale handle cannot act on its
  successor.
- `createViewer` destroys the previous viewer before it mounts the new canvas.

**Brand rule.**

- The package's API and README name no third-party service.
- The catalog is "a catalog that resolves asset numbers to tile endpoints".
- Its implementation is the connector's `IonSources`, now shared with the native host.
- The example page, which is a host, and the proxy's binary are the places that name
  the service.

### 2.5 Backends: WebGPU, then WebGL2

- On wasm32, `wgpu` has the `webgpu` and `webgl` features.
- The instance is made with `wgpu::util::new_instance_with_webgpu_detection`, on every
  target: native behaviour is unchanged, since a native build has no browser WebGPU to
  detect.
- `backend: 'webgpu' | 'webgl2'` narrows the backends, for hosts and tests.
- The device is asked for `adapter.limits()`. That is by definition what the adapter
  supports, and on WebGL2 it is the downlevel set raised to what the context reports.
- A WebGPU canvas offers only linear formats. `tuile_wgpu::surface_format` keeps the
  linear surface and draws through its sRGB view (`view_formats`). On native the surface
  is sRGB and nothing changes.
- **The viewer path uses no compute pipeline, storage buffer or storage texture.** This is
  a constraint from now on: Hillaire LUTs and the budget's GPU accounting must avoid
  compute in the shared viewer path, or ship a fragment or render-to-texture variant.
- `tuile-wgpu/src/webgl2.rs` (a host test) holds every shader of the viewer path to WebGL2:
  - the ground at 9, 15 and 25 imagery slots, the overlay, the lines;
  - parsed and validated by naga with no optional capability;
  - written as GLSL ES 3.00 for WebGL, entry point by entry point;
  - the ground's fragment stage at WebGL2's slot budget fits the 16 texture units.
  - It was seen failing on a storage buffer, a compute stage, and 25 slots.

Checked to work on WebGL2, in Chrome 155 with ANGLE on Metal:

- `Depth24PlusStencil8` and the stencil of the fallback pass;
- 4× MSAA;
- CPU-built mip chains;
- 15 imagery slots;
- the 8 272-byte per-tile uniform.

**Known differences on WebGL2:**

- 15 imagery slots instead of up to 25, so a few drapes take two passes;
- no GPU timestamps.

## 3. What moved where

| From | To |
|---|---|
| `std::time::Instant` in `tuile-viewer` and in `tuile-wgpu`'s pump | `web_time::Instant` |
| the desktop host's private ion `Sources` | `tuile_cesium_ion::IonSources`, shared by the native host and the web worker |
| `tuile-viewer/src/lib.rs`'s `main` and session | `native/mod.rs` |
| `launch.rs`'s bring-up, `Started`, the switcher | `native/launch.rs`; the shared `Launch` and `Retries` stay in `launch.rs` |
| the foyer store functions (`globe.rs`, `settings.rs`) | `native/store.rs` |
| the store and runtime inside `globe::Switcher` | `globe::Layers`, generic over any `ContentStore`; each shell runs `switch_to` where its network work runs |
| window and GPU bring-up in one sync call | the window, then `GpuOnItsWay`: blocking on native, `spawn_local` on the web |
| tokio, foyer, metrics, ctrlc, pollster | `[target.'cfg(not(target_arch = "wasm32"))'.dependencies]` |
| mcap with its C compressors | without them on wasm32, unchanged elsewhere |
| the stalled-server watch reading the process-wide traversal counter | the selections the pump received, which a worker cannot hide |

New in the shared application:

- `Profile::browser`;
- the `pause_loads_when_hidden` and `owns_its_window` switches;
- a lifecycle that knows a hidden tab;
- glides: `Goto::seconds`, also accepted by steering URLs as `goto?…&seconds=`;
- `App::end`, `App::hidden` and `App::draw_soon`.

## 4. What was deleted

- `examples/web-viewer/www`: the THREE.js scene, its GLSL ground material, its
  JavaScript camera, its worker shim, and the old README.
- `crates/tuile-web/src/worker.rs`: the JS-object encoder for that scene.
- `examples/wasm-globe`: the fourth traversal driver. It is gone from the workspace, the
  README, the site pages and the CI.

`examples/web-viewer` is now a page that imports the package, served with the catalog by
`tuile-catalog-proxy` (`scripts/try-viewer.sh web`); its `serve.py` and its token field
are gone.
`examples/web-viewer-vite` imports it from a TypeScript Vite build.

## 5. No token in the browser

A page cannot keep a secret: any token it holds, anyone who opens it can read. So the
browser never holds one — not the account token, not an asset token, not an imagery key.

- The page names a **catalog proxy** in `sources.catalog.url`. The proxy holds the token
  and speaks the subset of ion's API the connector speaks, so the worker runs the same
  `IonSources` with another API base and no bearer (`tuile_cesium_ion::proxy`):

  | The worker asks | The proxy does |
  |---|---|
  | `v1/assets/{id}/endpoint` | asks ion with the account token; answers with the URLs pointing back at itself, `accessToken` empty, the imagery key empty |
  | `assets/{id}/{path}` | fetches under the asset's URL with its short-lived token, renewed on a refusal |
  | `bing/{id}/REST/v1/Imagery/Metadata/{style}` | asks the imagery service with the key ion gave the asset |

- The imagery tiles are fetched from the imagery service's CDN directly: the URLs the
  metadata names carry no key, and passing megabytes of pictures through the proxy would
  buy nothing.
- **Not an open proxy:** the configured assets only, these three routes, relative paths
  of a narrow alphabet, the two query parameters a terrain request carries. And no reply
  leaves if it would carry the token, an asset token or the key, whatever the upstream
  wrote.
- Cache-Control follows the origin (a day for tiles it is silent about, `no-store` for
  refusals); CORS is any origin for development, a list in production.
- `examples/catalog-proxy` (`tuile-catalog-proxy`) is the binary, the same for a
  developer's machine and production. It reads `TUILE_ION_TOKEN` from its environment.
  In production that is a secret of the deployment (a Pulumi secret, e.g. config key
  `catalogToken`, put by the stack into the platform's secret store and exposed to the
  process as `TUILE_ION_TOKEN`), never a flag, a file in an image, or front-end
  configuration. Deploying it is not part of this change.
- `scripts/try-viewer.sh web` starts it with the token read from `.env` on the server side,
  and serves the page beside it; `--lan` and `--https` do the same for a phone on the
  local network, which needs no token either.
- The native viewer is unchanged: it holds its own token, as an application on the
  person's own machine may.

Checked in Chrome over the DevTools protocol, page and worker: 829 responses from the
proxy and the page read, none carrying the token; no URL carrying it; no request to the
ion API from the browser.

## 6. Parity, after the change

Done: the same, from the shared code. Adapted: the same function, in the platform's own
terms. N/A: why it has no place there.

| Feature | Desktop (macOS) | Phone (iOS) | Web |
|---|---|---|---|
| Terrain and imagery mosaic, aerial perspective | done | done | done, same shaders (WebGPU, or WebGL2: 15 slots instead of 25) |
| Never-black ground: shell, ancestors, new before old | done | done | done, same pump; the container shows the phone's launch colour while the module loads |
| On-screen controls: compass, tilt, zoom, atmosphere box, safe area | done | done | done, same `tuile-ui` widget at the display's scale |
| Mouse: drag, right-drag, wheel toward the cursor | done | N/A, no mouse | done, same `App` |
| Keys N L C V I W D F | done | N/A, no keyboard | done, same table (`menu::KEYS`) |
| Esc quits | done | N/A | N/A: the page is not the viewer's to close |
| Touch: pan, pinch, twist, two-finger tilt, double tap, hold for location | N/A | done | done, same `touch` module (winit's touch events) |
| Loading indicator at launch, corner indicator after a jump | holds its window until warm | done | done, plus `loading` events |
| Attribution | title bar | caption label | adapted: caption over the canvas, from the same text; `attributionChanged` |
| Notices ("link copied", location, "trying again in N s") | title bar | caption | adapted: the same caption |
| Alerts: cannot start, retrying, no GPU | NSAlert | UIAlertController | adapted: in-page alert, one at a time; `error` event |
| No WebGPU nor WebGL2 | N/A | N/A | adapted: a note in the container, `UnsupportedError` |
| Session retries with back-off | N/A (fails at once) | done | done, same `Retries` |
| Menus: View, Go, Imagery with keys and check marks | menu bar | N/A, no menu bar | adapted: menus from a button over the view, same `menu::layout` |
| About, Keys help | app menu, Help | N/A | adapted: menu items, in-page alert; the help lists keys and gestures, not the flags |
| Window menu, Services, Hide, Quit | done | N/A | N/A: the browser's |
| Current location | Core Location | Core Location | adapted: Geolocation API, permission question timed apart; needs a secure page |
| Copy link / go to copied link | NSPasteboard | UIPasteboard | adapted: the browser's clipboard; paste answers later, through the steering queue |
| Remember the last view | when started from its icon | done | adapted: local storage per application name, unless the page names a `view` |
| Glides | steering URL `seconds=` | N/A | `flyTo` |
| Steering URLs, AppleScript dictionary | done | N/A | adapted: the `setView`/`flyTo`/`getView` API and events |
| Menu-bar readout (status item) | done | N/A | N/A: no tray in a page; `viewChanged` carries the view |
| Pause when hidden | occlusion | lifecycle | hidden tab or off-screen canvas: loads held, trimmed, resumed |
| Tapes, traces (`TUILE_RECORD`, `TUILE_REPLAY`, `TUILE_TRACE`) | done | done | N/A for now: environment variables and files; a page would need a download API (the reserved `debug` namespace) |
| Picture snapshot | N/A | N/A | N/A: not in native either |
| Metrics endpoint, journal file | done | journal | N/A: the console carries the log |
| Persistent tile store | foyer | foyer | N/A for now: in memory in the worker; a browser store comes with the budget |
| Credentials | token in its environment or support file | settings, bundle or Documents file | adapted: none — a catalog proxy holds it on the server (§5) |
| Network bounds: timeout, retry, abort on cancel, one fetch per imagery tile | reqwest timeouts and retries; coalescing shared | same | done: per-attempt timeout and abort, bounded retries; coalescing shared (§11) |
| Hillaire sky, Moon, night lights (PR #61) | film path | film path | not in the viewer path on any target yet; must stay compute-free for WebGL2 |
| Overlays API, stereo | film path only | — | not yet; they come with the shared layers |

## 7. Out of scope

**The PMTiles tile store** (`tuile-tile-server`, `tuile-repository`, packs) serves the film
pipeline, not the interactive viewer.

- No shell depends on it.
- The seam stays open: a store would become one more `Sources` (or a `ContentStore` under
  `globe::assemble`) without touching the application.

## 8. How the budget work (PR #64) slots in

- `tuile_core::budget` is pure and builds for wasm32, so both shells get it through the
  core.
- `Credit { bytes }` travels on the stream. The web binding forwards `ClientMessage`
  through an exhaustive codec, so a new variant only needs an encoding.
- The headless control API (§4c) is a desktop socket. The browser's equivalent is the
  reserved `viewer.debug` namespace, which a headless-browser test would drive.
- `Tier::Stored` on the web needs a browser store (IndexedDB or OPFS) answering `holds()`.
  It plugs into the worker where the desktop plugs foyer.

## 9. Tests

| Test | Covers |
|---|---|
| `tuile-web` `wire` | every message reads back as itself, with the f64 origins bit for bit; damaged buffers are refused |
| `tuile-wgpu` `surface` | the sRGB view of a linear-only surface |
| `tuile-wgpu` `webgl2` | every shader of the viewer path under WebGL2 |
| `tuile-viewer` `lifecycle` | a browser tab hidden and shown |
| `tuile-viewer` `profile` | the browser profile |
| `tuile-viewer` `flight` | glides: endpoints, the climb, heading wrap, antipodes |
| `tuile-viewer` `steer` | `seconds=` |
| `tuile-viewer` `launch` | retries and an abandoned launch |
| `tuile-viewer` `web::options` | options read on the host |
| `tuile-cesium-ion` `sources` | asset resolution |
| `tuile-cesium-ion` `proxy` | endpoints rewritten with no credential; tiles with the asset token, renewed on a refusal; imagery metadata with the key; a reply carrying a credential withheld; unknown assets, paths and queries refused before anything goes upstream; CORS, preflight and cache headers |
| `tuile-catalog-proxy` | flags; the token read from a `.env` |
| `tuile-planetary` | one imagery tile asked for at once is fetched once; a cancelled fetch hands the tile to the next asker |
| `tuile-core` `runtime` | a load the network never answers holds one slot: the rest arrive, the parent stays selected, views keep being answered (and with one slot, it stalls); a server answered from memory still hands its host a turn |
| `tuile-web` `patience` | three attempts with growing pauses, final answers not retried, nothing waits more than ~40 s |
| `tuile-viewer` `start` | a page's help is the keys and gestures; a remembered link is a whole view or none |

- Each new test was seen failing against a broken implementation.
- In CI:
  - the wasm guard runs `clippy -D warnings` on `tuile-viewer`, `tuile-web` and
    `tuile-wgpu` for wasm32;
  - a `web-package` job builds the package, type-checks the Vite example with `tsc` and
    builds it with Vite.
- **Not in CI:** a headless-browser smoke test. It was run locally, in Chrome 155 over
  the DevTools protocol, page and worker, on both backends:
  - zoom, drag, zoom out and destroy/recreate, with no console errors;
  - `destroy()` terminated every worker the page created and removed the canvas;
  - the menu, the keys and about alerts, the caption, a layer switch, the copy notice;
  - no response to the browser carrying the token (§5);
  - the imagery service held unanswered for 50 s after the first view: never more than one
    request per tile in flight, held requests aborted and retried, coarse imagery and the
    indicator on screen throughout, frames and gestures going on, the picture recovering
    once the service answers.
  - Running it in CI needs a browser with a GPU (or SwiftShader) and a token or recorded
    tiles. It is proposed, not added.

## 10. Open decisions for the owner

1. **WebGL2 fallback**: in. Keeping it once the budget adds GPU work means anything in the
   shared path must stay compute-free or carry a downlevel variant.
2. npm name and publishing.
3. Hosting a public demo: where the catalog proxy runs, and the origins it allows.
4. A CI headless-browser smoke test: GPU in CI, tiles without a token.

## 11. The page that stopped

Reported from Chrome: dozens of imagery requests pending for 50 s and more, the same
quadkeys two and three times at once, a few patches of imagery and the rest bare, and a
page that no longer progressed. Four causes, in the shared core and in the browser's
transport and worker:

1. **Duplicates.** `PlanetaryLoader::fetch_imagery` looked only in its decoded cache, not
   at the fetches in flight. An imagery tile spans several terrain tiles; their loads
   asked for it together and each fetched it. The browser serialises identical requests
   behind its cache lock, and each held a connection. The desktop did the same, hidden by
   HTTP/2 and a disk cache. Now one fetch per `(generation, coord)`: the first asker
   fetches, the others wait for its answer, and a cancelled first asker hands the tile to
   the next.
2. **No bound.** The browser's `fetch` had no timeout. A request never answered held the
   terrain load awaiting it (`join_all` over its imagery), and the load held one of the
   server's 32 slots for good. Now every attempt is aborted after 12 s and retried twice
   after 0.4 s and 0.8 s; a transient status likewise; then the tile is drawn from its
   coarser floor. The desktop's client already had 30 s and retries.
3. **No abort.** A load the server cancelled dropped its future, and the browser went on
   fetching for nobody. Now dropping the future aborts the request.

4. **No turn for the worker's messages.** Found while checking a phone-sized page: once
   the residency is near its budget, loads are answered from the loader's memory and
   complete on their first poll, and so does the traversal each one triggers. The server's
   loop then never waits, and on the worker's one thread the page's messages — the camera,
   the acks — are never delivered: the page drew on, but nothing refined, and the
   stalled-server watch fired. The server now takes a breather every sixteen events
   handled back to back (`GeometryServer::with_breather`); the worker's breather is a timer,
   a macrotask, which lets its messages in. The desktop's server runs on a thread pool and
   is given none. That the budget churns at all near its ceiling is the budget core's to
   settle (PR #64).

The imagery service and the catalog are HTTP/2, so the browser's six-connections-per-host
limit of HTTP/1.1 is not what bound them; it does bind the catalog proxy on a machine's
own HTTP, which `--https` lifts. Per-host quotas and priorities are the budget core's
(PR #64), which formalises request deduplication and grace-time cancellation.

