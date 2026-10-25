<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-native-fetchers

Native HTTP fetchers for tuile.

[`NativeHttp`] is the native-target transport for the viewer apps: one
pooled `reqwest` client behind a browser-style HTTP cache (foyer — a hybrid
in-memory + on-disk cache with bounded capacity and eviction), exposed
through both core fetch seams at once:

- [`tuile_cesium_ion::IonHttp`] — bearer GET returning status + body, used
  by the ion terrain/imagery connectors.
- [`tuile_core::fetch::TileFetcher`] — plain GET returning bytes, used by
  everything else.

A single instance serves ion *and* imagery, so they share one connection
pool and one cache. The cache follows HTTP rules ([`CacheMode::ForceCache`]
— immutable tiles are stored regardless of headers, for true offline
reuse), except for `api.cesium.com` endpoint calls, which carry short-lived
tokens and are never cached.

This crate is native-only by construction (it pulls reqwest + foyer); it is
deliberately outside the render-agnostic, wasm-able core.

## Where it sits

Depends on `tuile-core` and `tuile-cesium-ion` (for the `IonHttp` seam). Used
by the native hosts: `tuile-viewer`, `tuile-bake` and the examples
(`wgpu-viewer`, `globe-bulk`, `orbit-probe`, `warm-cache`).

## Example

```rust,no_run
# async fn example() -> Result<(), Box<dyn std::error::Error>> {
use tuile_core::fetch::TileFetcher;
use tuile_native_fetchers::NativeHttp;

// Must run inside a tokio runtime.
let http = NativeHttp::shared().await?;
let url = url::Url::parse("https://example.com/tileset.json")?;
let bytes = http.fetch(&url).await?;
println!("{} bytes", bytes.len());
# Ok(())
# }
```

## Features

None. Native only.

## Documentation

- [`docs/01-architecture.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/01-architecture.md)
- [`docs/10-crate-core.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/10-crate-core.md)
