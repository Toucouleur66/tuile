<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Browser / WebAssembly viewer — status

Status on 2026-10-25, read on `origin/main` at `eaebfbd7` (v0.14.0, plus #129–#131 and
#135). It replaces the review of 2026-10-17, written at `70e17d9`, when the browser had
no working interactive viewer. Read from the code, the package and the CI workflow; no
browser run was made for this revision. The companion audit is
`docs/explorations/wasm-audit.md`.

**Summary.**
- The browser viewer is `tuile-viewer` itself, compiled to WebAssembly and published as
  the ES module package `@lapoule-dev/tuile-viewer` (`packages/tuile-viewer`): the same
  application, renderer (`tuile-wgpu`), gestures, menu, caption and budget as the desktop
  and the phones. There is no second renderer.
- It draws with **WebGPU**, or **WebGL2** where the browser has no WebGPU, both through
  wgpu.
- Its geometry server runs in a Web Worker, and imagery and terrain decode in a pool of
  **decode workers** beside it.
- **Imagery compression is the page's choice, off by default** (`imageryCompression`):
  encoded on the GPU under WebGPU, on the CPU in the decode workers under WebGL2.
- Terrain comes from a catalog asset **or from any quantized-mesh `layer.json` URL**.
- CI builds and type-checks the package and checks and lints every wasm crate, but runs
  nothing in a browser.

## 1. What changed since the 2026-10-17 review

The 2026-10-17 review found `tuile-web` not compiling since August, a THREE.js page that
forked the renderer, a fourth traversal driver in `examples/wasm-globe`, and only the film
path alive in a browser. The convergence (`docs/explorations/web-native-convergence.md`)
replaced all of it:

| Then (2026-10-17) | Now |
|---|---|
| `tuile-web` fails `cargo check` for wasm32 (3 × E0027) | `tuile-web` is the browser **transport**: the `fetch` seams (`transport.rs`) and the streaming protocol over `postMessage` (`post.rs`, `wire.rs`). Its old `worker.rs` is gone |
| A THREE.js page with its own GLSL ground, camera and residency | Removed (`7b76baf9`). The page is a package consumer: one import, one `createViewer` |
| `examples/wasm-globe`, a fourth traversal driver | Removed (`7b76baf9`) |
| `tuile-film-web`, `examples/film-web`, `tuile-pack-worker` (the film demo) | Removed (`a14cc95f`, `3c38b82e`). Films render natively; the browser hosts the interactive viewer |
| No ancestor fallback, `retire` ignored, errors overwritten | The desktop's own pump and swap order: never a black ground, new before old |
| `wasm-guard` checks `tuile-core` only | 10 crates checked under `-D warnings`, wasm clippy on the viewer, the transport and the renderer, and a `web-package` job (§5) |
| No attribution, no lifecycle handling | The caption names whose pictures are on screen (`attributionChanged`); a hidden tab holds drawing and loading (`pause`/`resume`) |
| `Config::default()` budget, saturating on wasm32 | The graphics budget runs in the worker, on the profile's numbers sized from `navigator.deviceMemory` (`profile`: `auto`, `desktop`, `handheld`) |

## 2. Inventory

| Item | Purpose |
|---|---|
| `crates/tuile-viewer/src/web` | The browser shell: `page` (the viewer on a canvas), `worker` (the geometry server), `decode` (the decode pool), `session`, `options`, `menu`, `screen`, `system`, `control` |
| `crates/tuile-web` | The transport: `WebFetcher` and `WebIonHttp` over `fetch`, the protocol over `postMessage`, the wire encoding of `ServerMessage` |
| `packages/tuile-viewer` | The npm package: `index.js` / `index.d.ts` (`createViewer`), `worker.js`, `decode-worker.js`, and the wasm built by `build.sh` |
| `examples/web-viewer` | A host page over the catalog proxy; `?terrain=`, `?imagery=`, `?backend=`, `?compression=` |
| `examples/web-viewer-vite` | The same package imported from a Vite build (built in CI) |
| `examples/catalog-proxy` | `tuile-catalog-proxy`: serves the page and the package, holds the catalog token on the server |
| `examples/heightfield-serve` | A synthetic relief served as quantized-mesh terrain plus imagery over HTTP with CORS; prints a `web-viewer` URL for it |
| `scripts/try-viewer.sh web` | Builds the package and serves the page (`--lan`, and `--lan --https` for a phone) |
| `scripts/web-bench.py` | A headless-Chrome bench over the public JS API: coverage, convergence, requests, budget, black patches, compression |

## 3. What runs

### Backends

- `backend: "auto"` (the default) takes WebGPU and falls back to WebGL2; `"webgpu"`
  refuses the fallback, and `"webgl2"` forces it. `createViewer` rejects with
  `UnsupportedError` when neither can draw.
- Known WebGL2 differences (package README): at most 15 imagery layers bound per pass
  instead of up to 25, so some tiles draw in two passes; no GPU timestamps; surfaces drawn
  through an sRGB view. World overlays draw on WebGL2 too (`6f6663de`).

### Sources

- **Terrain:** `sources.terrain` is `{ asset }`, resolved through the catalog proxy, or
  `{ url }`, the address of a quantized-mesh `layer.json` (`c8aa470e`). The URL form needs
  no token and no catalog; the server must send `Access-Control-Allow-Origin`, and tiles
  are addressed as the `layer.json` says (TMS, row 0 in the south). A URL terrain is cached
  under the edition its `layer.json` declares (`d41a556a`).
- **Imagery:** each layer is a catalog `asset` or a URL `template` with a `scheme`
  (`webmercator` or `geographic`) and an optional `maxLevel`.
- `sources.catalog` is required only when a source is named by asset number. The page
  never holds a token: the catalog proxy keeps it on the server.

### Worker and decode workers

- The geometry server runs in one Web Worker, assembled as the desktop's is
  (`crate::globe::assemble`, the same gate, the same budget).
- It starts up to `decodeWorkers` decode workers (4 by default, capped at the core count
  less one; `0` decodes on the geometry worker). They are handed the compiled
  `WebAssembly.Module`, so they fetch and compile nothing, and they exchange transferred
  `ArrayBuffer`s: no `SharedArrayBuffer`, so no COOP/COEP headers are asked of the host.
- A worker that fails is replaced (up to 8 times), and its job falls back to the
  geometry worker's own thread: never a hole.

### Imagery compression, the page's choice

`imageryCompression` is `"off"` (the default: RGBA8, as before), `"auto"`, `"bc7"` or
`"astc"`. The choice is resolved against the page's adapter when its device comes up
(`ImageryCompression::plan`, `tuile-core/src/texels.rs`) and passed to the worker in its
start message. With `"off"` the session starts at once, as before.

| Backend | Path | Where it runs |
|---|---|---|
| WebGPU | RGBA8 crosses to the page; `tuile-wgpu` encodes BC7 or ASTC 4×4 with texcomp's compute encoders at upload | the page's GPU |
| WebGL2 (no compute) | `encode_on_cpu`: UASTC level 0 (the fastest), transcoded to ASTC 4×4 (phones) or BC7 (desktops), whole mip chain; blocks cross to the page | the decode workers |
| Neither format sampled | RGBA8 | — |

- A tile that cannot be encoded is held as RGBA8, never left out.
- The caption says what is held (`imagery ASTC 4x4 23 MiB (276 × 85 KiB)`). A 256² tile
  with its chain is 341 KiB as RGBA8 and 85 KiB as blocks.
- The decode workers log their CPU encoder's cost (one console line per 32 tiles a thread
  encodes: mean and peak, mip chain included); `web-bench.py --compression` reads the
  caption's held line.
- What remains — the encode's time and transient memory counted by the budget, the CPU
  path's reservation — is the compression-budget plan (B1–B8), not built yet.

### Embedding

- `createViewer(container, options)` draws into a canvas it creates, or into the one it
  is given. One viewer per page at a time.
- The viewer API: `setView`, `flyTo`, `getView`, `on` (`ready`, `loading`, `error`,
  `viewChanged`, `attributionChanged`), `resize`, `pause`/`resume`, `destroy`.
- A host that owns its own wgpu device and render graph uses `tuile-wgpu`'s `HostPass`
  (`889c9cc7`) instead of the package: tuile draws into the host's colour and depth
  targets. Its first consumer is the Bevy integration (`tuile-bevy`, native first); its
  web build (WebGPU and WebGL2) is a planned step, not on main.

## 4. Parity with the native viewer

The code is the same, so the question is only what a browser lacks:

| Feature | Web | Note |
|---|---|---|
| Terrain + imagery, never-black ground, new before old | Same as native | Shared pump and renderer |
| Controls, menu, caption, alerts, loading indicator | Same as native | Drawn by the shared UI |
| Budget | The same budget, on browser numbers | Profile sized from `navigator.deviceMemory` |
| Imagery compression | Yes, off by default | GPU encode on WebGPU, CPU encode on WebGL2 |
| Persistent tile cache | **Missing** | The worker keeps its residency in memory; a browser store (IndexedDB or OPFS) is still to come |
| GPU timestamps | WebGPU, where the browser exposes them | None on WebGL2 |
| Sky model beyond aerial perspective, stereo | Missing | Not in the shared layers yet |
| `debug` control API (budgets, readback, scripted moves) | Reserved, not available | Native has the headless control socket |
| Several viewers on one page | No | A second `createViewer` destroys the first |

## 5. CI

From `.github/workflows/ci.yml`:

- **`wasm-guard`:** `cargo check --target wasm32-unknown-unknown` under `-D warnings` for
  `tuile-core`, `tuile-web`, `tuile-wgpu`, `tuile-planetary`, `tuile-terrain`,
  `tuile-camera`, `tuile-atmosphere`, `tuile-film-gpu`, `tuile-ui` and `tuile-film`; then
  wasm clippy on `tuile-viewer`, `tuile-web` and `tuile-wgpu`.
- **`web-package`:** builds the package (`packages/tuile-viewer/build.sh --dev`) with
  `wasm-bindgen-cli` at the locked version, then runs `npm run build` in
  `examples/web-viewer-vite`.
- **Host tests** cover the pure parts of the web code: options parsing (terrain
  `asset`/`url`, compression names), the decode pool's scheduler and imagery bookkeeping,
  and the wire encoding (`tuile-web`).
- **The `test` job's Linux runner is pinned to `ubuntu-24.04`.** Its GPU tests render
  through Mesa's lavapipe. On 2026-10-25 `ubuntu-latest` moved to 26.04 (Mesa 26.0.8),
  whose lavapipe crashes with SIGSEGV in `tuile-film-gpu`'s headless and atmosphere tests.
  This concerns the native GPU tests, not the browser: no CI job runs a browser.
- **Missing:** a headless browser in CI. `scripts/web-bench.py` is the browser test, run
  by hand against `scripts/try-viewer.sh web`.

## 6. Open items

1. **A browser tile store** (IndexedDB or OPFS), answering `holds()` for the budget's
   stored tier. Today every reload fetches everything again.
2. **`web-bench.py` in CI**: headless Chrome on WebGPU (SwiftShader or a flag) and on
   WebGL2, at least the `still` pattern on both with the black-patch check.
3. **The compression budget** (the compression-budget plan, B1–B8): the GPU encode's time
   and transient memory counted by the budget, and the CPU path's reservation.
4. **Phone measurements** on the reference device, WebGPU and WebGL2, compression on and
   off (the B8 acceptance).
5. **The `debug` control API**, a web equivalent of the native headless runner.
6. **Third-party notices in the package** (see the audit, L10).

This PR also fixes the package README, whose options table still named the terrain by
`asset` only and called `sources.catalog` required.

## Not verified

- No browser run for this revision: the behaviour above is read from the code, the
  package README and the commit messages of the PRs that built it.
- The WebGL2 layer limit and timestamp behaviour are the package README's statements,
  not measured again here.
