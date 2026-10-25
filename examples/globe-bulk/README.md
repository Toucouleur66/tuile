<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# globe-bulk

Bulk multi-tile globe renders on live data: Cesium World Terrain
(quantized-mesh) draped with Bing imagery, both via ion, selected by the
core SSE traversal and pulled through the async `GeometryServer` in **bulk**
mode (`drive_until_complete` — wait until the whole selection is resident),
then rendered to a high-resolution PNG.

```text
cargo run -p tuile-globe-bulk -- hemisphere [out.png] [size]
cargo run -p tuile-globe-bulk -- pyrenees   [out.png] [size]
```
The ion token is read from `CESIUM_ION_TOKEN` (a local `.env` is loaded if
present) and never stored.

## Built on

`tuile-wgpu`, `tuile-planetary`, `tuile-cesium-ion`, `tuile-bing`, `tuile-native-fetchers`.
