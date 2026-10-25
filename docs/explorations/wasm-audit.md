<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# WebAssembly implementations — audit

Audit refreshed on 2026-10-25 against `origin/main` at `eaebfbd7` (v0.14.0, plus #129–#131
and #135). The first audit was written on 2026-10-17 at `70e17d9`; most of what it found
has since been fixed or deleted, and this revision records each finding's state rather
than repeating it. The viewer's status is in `docs/explorations/web-viewer-status.md`.

Read from the code and the CI workflow. Nothing was built, run in a browser, or probed on
a deployment for this revision.

## 1. What runs in wasm today

| Implementation | Build / CI | Tests on the host | Verdict |
|---|---|---|---|
| `tuile-viewer` web shell (`src/web`) and the npm package | `wasm-guard` clippy, `web-package` build + Vite build | options, decode scheduler, imagery bookkeeping | **keep**: the one browser viewer |
| `tuile-web` (transport, `postMessage` protocol, wire) | `wasm-guard` check + clippy | wire round trips, patience | **keep** |
| `tuile-wgpu` (WebGPU and WebGL2 through wgpu) | `wasm-guard` check + clippy | native GPU tests | **keep** |
| `tuile-core`, `-planetary`, `-terrain`, `-camera`, `-atmosphere`, `-ui` | `wasm-guard` check under `-D warnings` | native | **keep** |
| `tuile-film`, `tuile-film-gpu` | `wasm-guard` check | native | **keep**: checked for wasm, no browser consumer since the film page left |
| texcomp (git dependency, tag v0.2.0) | built into the package | in its own repository | **keep**; see L10 for its notice |

Removed since the first audit: `tuile-film-web` and `tuile-pack-worker` (`a14cc95f`),
`examples/film-web` (`3c38b82e`), the THREE.js page and `examples/wasm-globe`
(`7b76baf9`), and the signed S3 transport only the Worker used (`e41ab221`). The wasm half
of `tuile-repository` has no browser consumer left.

## 2. The 2026-10-17 findings, one by one

| # | Finding (2026-10-17) | State on 2026-10-25 |
|---|---|---|
| H1 | The demo Worker served any object of every configured bucket | **Code removed** with `tuile-pack-worker`. Whether the deployed `tuile-demo.lapoule.dev` Worker was taken down is not visible from the repository: an owner check |
| H2 | The Worker's S3 key was a general one, not read-only | **Code removed**. If that deployment still runs, rotating its secret remains an owner action |
| H3 | Anonymous callers spent a privileged upstream token | **Code removed** |
| M1 | Unbounded listings and amplified upstream work | **Code removed** (S3 transport and Worker). The `Bench` routes remain in `tuile-repository`, served natively by `tuile-pack-api`, which is a local tool; the same scoping would be owed before exposing it publicly |
| M2 | Upstream errors and bucket names reached anonymous clients | **Code removed** (Worker); same remark as M1 for `tuile-pack-api` |
| M3 | `tuile-web` did not compile; CI checked only `tuile-core` | **Fixed**: 10 crates checked under `-D warnings`, wasm clippy on viewer, transport and renderer, the package built in CI |
| M4 | `tuile-wgpu` would panic on its first frame (`std::time::Instant`) | **Fixed**: `tuile-wgpu`, `tuile-viewer` and `tuile-planetary` use `web-time` |
| M5 | Core timing silently zero in the browser | **Open** (§3) |
| M6 | No byte budget for the browser globe | **Fixed**: the graphics budget runs in the worker on a profile sized from `navigator.deviceMemory` |
| M7 | film-web's Cache Storage grew without bound | **Code removed** |
| M8 | The wasm-only crates had no tests | **Partly fixed**: the pure parts are host-tested (`tuile-web` wire, the viewer's options and decode scheduler). No browser test in CI (§3) |
| L1 | Hand-rolled HMAC/SigV4 in the wasm path | **Code removed** (`e41ab221`) |
| L2 | Browser plumbing written five times | **Fixed**: one transport (`tuile-web::transport`) |
| L3 | Copies across the JS boundary | **Open, minor** (§3) |
| L4 | `u64` → `usize` truncation in the pack reader | **Open, low**: `tuile-pack` still casts (`lib.rs:950` and nearby). It builds for wasm through `tuile-film` but ships in no browser package |
| L5 | A panic on server data in film-web | **Code removed** |
| L6 | `?api=` sent the demo page to any origin | **Code removed** |
| L7 | Errors swallowed when film-web opened the store | **Code removed** |
| L8 | THREE.js from unpkg without integrity | **Code removed** |
| L9 | No release profile for wasm | **Open** (§3) |
| L10 | A notice owed for rav1e in the film module | **Obsolete** (rav1e left with film-web), **replaced by a new one** (§3) |
| L11 | The demo deployed by hand, not from main | **Obsolete** for the repository (no Worker left); see H1–H2 for the deployment |
| L12 | The site claimed a browser renderer that did not run | **Now true**: `site/src/pages/architecture.astro` describes `tuile-wgpu` in the browser and the two web examples, which exist and build |
| L13 | The `?Send` split repeated by hand | **Open, low**: 59 `async_trait(?Send)` sites |

## 3. Open findings

**M5. Core timing is still zero in the browser.**
- `tuile-core/src/determinism.rs:99` returns `0` from `elapsed_ms()` on wasm32, and
  `tuile-core/src/metrics.rs:881` returns `Duration::ZERO` from `Stamp::elapsed()`.
- The viewer, the renderer and the decode workers measure with `web-time`; the core's own
  load, decode and stitch durations do not. Instruments built on the core's stamps read
  zero in a browser.
- The geometry server's clock is injected (`GeometryServer::with_clock`) and the browser
  shell passes its own, so the budget's decisions are not affected.
- **Fix:** `web-time` in the core's two stamps (stable, already in the lock file).

**M8 (remainder). No browser test in CI.**
- `scripts/web-bench.py` checks coverage, convergence, requests, budget, black patches
  and compression in headless Chrome, but only by hand.
- **Fix:** a CI job running its `still` pattern on WebGL2 and, where the runner allows,
  WebGPU.

**L3. Bodies copied out of JS memory.**
- `tuile-web/src/transport.rs:216` copies each fetched body with
  `Uint8Array::new(&buffer).to_vec()`, and the decode pool copies each job's input and
  output the same way (`tuile-viewer/src/web/decode.rs:177, 405`).
- Tiles are tens to hundreds of KiB, so this is a cost, not a fault. Measure before
  changing anything.

**L9. No size-tuned profile for the package.**
- `packages/tuile-viewer/build.sh` builds with the workspace `release` profile
  (`lto = "thin"`, `debug = "line-tables-only"`), tuned for profilable native builds, and
  runs no `wasm-opt`.
- On the pack Worker, the first audit measured −18 % gzip from `opt-level = "z"`, fat LTO,
  one codegen unit and no debug info. The package's size was not measured here.
- **Fix:** a `[profile.wasm]` used by `build.sh`, measured against today's build.

**L10 (new). The package ships ported code without its notice.**
- texcomp v0.2.0, linked into the package for the CPU encoder (UASTC level 0 and the
  transcodes), contains code ported from Basis Universal (Apache-2.0); texcomp's `NOTICE`
  records it.
- The npm package's `files` (`packages/tuile-viewer/package.json`) ships the wasm but no
  `NOTICE`, and tuile's own `NOTICE` says no third-party code is ported *into this
  repository*, which stays true.
- **Fix:** ship a third-party notice file with the package that carries texcomp's
  attribution.

**L13. The `?Send` split.**
- `#[cfg_attr(wasm32, async_trait(?Send))]` is written 59 times. One alias or macro in the
  core would remove the duplication. Cosmetic.

### Info (checked, fine)

- **No `SharedArrayBuffer`:** the decode pool transfers buffers, so the package needs no
  cross-origin isolation headers and no nightly `build-std`.
- **No token in the page:** the catalog proxy holds it; a terrain by `layer.json` URL and
  imagery by template need none.
- **`unsafe`:** none in the wasm crates (workspace `deny`).
- **Rule 8:** the browser path uses `wasm-bindgen`, `web-sys`, `js-sys`,
  `wasm-bindgen-futures`, `serde-wasm-bindgen` and `web-time`, plus wgpu and texcomp. No
  exotic pre-1.0 crate otherwise.
- **No third-party brand** in the package's public API: the example page names the
  catalog service, the package does not.

## 4. Alignment with the manifesto

The fork the first audit found is gone. The browser runs the same application as the
desktop and the phones — the same renderer, pump, budget and UI — and compression is a
choice the page makes through the same `ImageryCompression` every client uses. What the
browser still lacks (a persistent store, a test in CI) is a gap to fill, not a second
implementation.

## 5. Not verified

- No build, no browser run, no package size measurement for this revision.
- The state of the `tuile-demo.lapoule.dev` deployment and of its secret (H1–H2): not
  probed.
- texcomp's notice was read from the v0.2.0 source in the local cargo checkout, at the
  commit `Cargo.lock` pins.
