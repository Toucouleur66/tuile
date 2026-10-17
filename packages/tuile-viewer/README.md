<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# @lapoule-dev/tuile-viewer

A streaming terrain and imagery globe for any web page or application.

The viewer is the same Rust application as the desktop and phone viewers, compiled to
WebAssembly. It has the same renderer, controls, loading indicator and guarantees: the
ground is never drawn black, and new tiles replace old ones without a gap. It draws with
WebGPU, or with WebGL2 where the browser has no WebGPU. Its geometry server runs in a Web
Worker, so decoding never blocks the page.

## Install

```sh
npm install @lapoule-dev/tuile-viewer
```

The package is an ES module and resolves its own worker and wasm files with
`new URL(…, import.meta.url)`. It works:

- with Vite, webpack 5, esbuild or Rollup, with no extra configuration;
- from a plain `<script type="module">` that serves the package directory as is.

To build it from this repository, run `packages/tuile-viewer/build.sh`. It needs the
`wasm32-unknown-unknown` target and `wasm-bindgen-cli` at the version in `Cargo.lock`.

## Use

```js
import { createViewer, UnsupportedError } from "@lapoule-dev/tuile-viewer";

try {
  const viewer = await createViewer(document.querySelector("#globe"), {
    getToken: () => fetch("/api/catalog-token").then((r) => r.text()),
    view: { lon: 6.86, lat: 45.83, altitude: 12000, heading: 120, pitch: 35 },
    sources: {
      catalog: {},
      terrain: { asset: 1 },
      imagery: [{ key: "aerial", name: "Aerial", asset: 2, attribution: "© …" }],
    },
  });
  viewer.on("viewChanged", (view) => console.log(view));
  viewer.flyTo({ lon: 7.66, lat: 45.98, altitude: 9000 });
} catch (e) {
  if (!(e instanceof UnsupportedError)) throw e;
}
```

`createViewer(container, options)` fills `container` with a canvas, or draws into it if it
is a canvas. It resolves once the viewer draws.

## Options

| Option | Meaning |
|---|---|
| `sources.catalog` | A catalog that resolves asset numbers to tile endpoints. `url` is its API base; the connector's own when left out. |
| `sources.terrain` | `{ asset }`: the terrain, from the catalog. |
| `sources.imagery` | The layers, in order. Each has `key`, `name` and `attribution`, plus either a catalog `asset` or a URL `template` (`…/{z}/{x}/{y}.png`) with an optional `scheme` (`webmercator` or `geographic`) and `maxLevel`. |
| `token` / `getToken` | The catalog's bearer, or a function that returns it. See below. |
| `name` | Your application's name, used in messages. |
| `profile` | `auto` (the default), `desktop` or `handheld`: memory budget, upload pacing and frame rate. |
| `backend` | `auto` (the default: WebGPU, else WebGL2), `webgpu` or `webgl2`. |
| `view` | Where the eye opens: `lon`, `lat`, `altitude`, and optionally `heading` and `pitch`. |
| `ui` | Whether the compass and zoom controls are drawn (`true` by default). |
| `attribution` | `bottom-right` (the default), `bottom-left`, or `none` if you show it yourself. You must show it somewhere. |
| `showUnsupportedMessage` | Whether a viewer that cannot draw says so in the container (`true` by default). |

## The viewer

| Member | Does |
|---|---|
| `backend` | `"webgpu"` or `"webgl2"`. |
| `setView(view)` | Puts the eye there at once. |
| `flyTo(view, { duration })` | Glides there: up, across and down. |
| `getView()` | The view of the last frame. |
| `on(event, listener)` | Subscribes; returns the function that unsubscribes. |
| `resize()` | Draws a frame now. The canvas size is observed, so this is rarely needed. |
| `pause()` / `resume()` | Holds drawing and loading, as a hidden tab does (that one is automatic). |
| `destroy()` | Releases the GPU device, the worker and every listener, and removes the canvas the viewer created. |

`debug` is reserved for the graphics budget's control API (budgets, GPU figures, frame
readback, scripted moves). It is not available yet.

## Events

| Event | Detail |
|---|---|
| `ready` | `{ backend }`: the viewer draws. |
| `loading` | `{ shown, progress }`: the loading indicator came or went, or its progress moved. |
| `error` | `{ title, message }`: something the person should hear about, such as a session that could not start. The viewer keeps retrying. |
| `viewChanged` | `{ lon, lat, altitude, heading, pitch }`, a few times a second at most. |
| `attributionChanged` | `{ layer, name, attribution }`: whose pictures are on screen. |

Errors:

- `createViewer` rejects with `UnsupportedError` when the browser can draw with neither
  WebGPU nor WebGL2, or not with the backend that was asked for;
- it rejects with `StartError` when the options are wrong.

## Browser support

**WebGPU first, WebGL2 as the fallback.** Current Chrome, Edge and Safari have one or the
other, and so does Firefox. Known differences on WebGL2:

- a drape binds at most 15 imagery layers per pass instead of up to 25, so a few tiles
  are drawn in two passes;
- there are no GPU timestamps;
- surfaces are always drawn through an sRGB view.

The picture is otherwise the same: the same shaders, translated by wgpu.

## Tokens

- Never put a secret token in a public page: anyone can read it.
- Use `getToken` to fetch a short-lived, scoped token from your own server, or put a proxy
  in front of the catalog.
- The token stays in the page and its worker. It is sent only to the catalog, and never
  logged or put in a URL.

## Not yet

- No persistent tile cache in the browser. The worker keeps tiles in memory, and a browser
  store comes with the graphics budget.
- One viewer per page at a time: a second `createViewer` destroys the first.

## License

MIT OR Apache-2.0.
