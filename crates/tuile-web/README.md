<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-web

The browser side of tuile: a `fetch`-backed transport, so the same engine
that runs on a laptop runs in a Web Worker, and the streaming protocol over
`postMessage`, so the page that draws it consumes the same messages a
desktop does.

# Why a crate rather than an example

`tuile-core` takes no backend, and the browser is a backend like any other —
`web-sys` is as much a transport dependency as `reqwest` is. This crate is
the browser's counterpart to `tuile-native-fetchers`: it implements the same
two seams ([`TileFetcher`](tuile_core::fetch::TileFetcher) and
[`IonHttp`](tuile_cesium_ion::IonHttp)) against `window.fetch`, and adds
nothing else to the engine's vocabulary.

# Where it runs

In a **Worker**, not on the page's thread. That is the entire point: the
engine's traversal, its decodes and its resamples are CPU work, and on the
main thread they are work the browser cannot paint through. `fetch` is
available in a worker through `WorkerGlobalScope`, which is why
`global_fetch` looks for either scope rather than assuming a `Window`.

# Single-threaded, and honest about it

A `Response` is a JS object and JS objects are not `Send`. The engine's
traits are already declared `?Send` under `target_arch = "wasm32"`, so the
futures here never claim otherwise, and the server is driven by
`spawn_local` rather than a work-stealing runtime. Nothing here would be
made faster by pretending: a worker has one thread, and parallelism comes
from running several workers, not from a scheduler inside one.

# Three pieces

- `WebFetcher` and `WebIonHttp`: the engine's two fetch seams over the
  global `fetch`, the counterpart of `tuile-native-fetchers` — each
  request bounded in time, retried as [`patience`] says, and aborted when
  the load that awaits it is dropped.
- [`wire`]: [`ClientMessage`](tuile_core::protocol::ClientMessage) and
  [`ServerMessage`](tuile_core::protocol::ServerMessage) as bytes. Pure
  Rust, compiled and tested on every target.
- `post`: the third binding of the streaming protocol, beside the
  in-process channels and the WebSocket — a
  [`GeometryStream`](tuile_core::protocol::GeometryStream) on the page whose
  server runs in a worker.

None of it knows what draws the tiles, or why.

## Where it sits

Depends on `tuile-core` and `tuile-cesium-ion` (for the `IonHttp` seam). Used
by `tuile-viewer`'s web shell, built into the `@lapoule-dev/tuile-viewer`
package.

## Example

The wire framing, which builds on every target:

```rust
use tuile_web::wire::{frame, unframe};

let framed = frame(&[vec![1, 2], vec![3]]);
assert_eq!(unframe(&framed)?, vec![&[1u8, 2][..], &[3][..]]);
# Ok::<(), tuile_web::wire::WireError>(())
```

## Features

None. Everything but [`wire`] is built for `wasm32-unknown-unknown` only.

## Documentation

- [`docs/01-architecture.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/01-architecture.md)
- [`docs/16-embedding-the-viewer.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/16-embedding-the-viewer.md)
