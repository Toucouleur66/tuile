<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# wgpu-viewer

The viewer, over the public connectors — the host every platform's build
runs: `main.rs` on a desktop and a phone, `android_main` on Android (named, not
linked: like `DEVELOPMENT_CATALOG` below, it exists only in an Android build).

The application itself — window, camera, controls, steering, scripting — is
the `tuile-viewer` library. This crate is the one decision a host makes:
**where terrain and imagery come from.** Here that is Cesium ion, with Bing
imagery behind it, over a native HTTP transport; the choice is made in this
file and nowhere deeper. Another host answers the same three questions with
its own service and reuses everything else — see
`docs/16-embedding-the-viewer.md`.

```text
CESIUM_ION_TOKEN=... cargo run -p tuile-wgpu-viewer
CESIUM_ION_TOKEN=... cargo run -p tuile-wgpu-viewer -- \
    --lon 6.86 --lat 45.83 --altitude 6000 --heading 120 --pitch 25
```

The access token is read from the environment, or — for a session started
from an icon — from one file the person owns:
`~/Library/Application Support/Tuile/token` on macOS.

**On Android there is no token at all.** The catalog is asked through the
catalog proxy (`tuile-catalog-proxy`), which holds the token on a server:
an APK carries nothing that would let anyone who unpacks it use the
account. The proxy's address is `TUILE_CATALOG_URL` — a launch setting,
else the value it had when the library was built, else
`DEVELOPMENT_CATALOG`, which `scripts/try-viewer.sh android` serves.

**Headless, on demand.** `examples/headless` plays the graphics budget's
motion patterns on this application without a window, over the real
catalog, tile store and clock, and holds each run to the specification's
obligations. It needs the network, a GPU and the token, read from
`TUILE_ION_TOKEN` by a catalog proxy served in its own process — the
application never sees it, and it is never printed:

```text
TUILE_ION_TOKEN=… cargo run --release -p tuile-wgpu-viewer --example headless -- run all
```

Session logs (MCAP) and a JSON summary go to `target/headless/`; the
runner exits 1 when an obligation is violated. See the example's own
documentation for the patterns and the flags.

## Built on

`tuile-viewer`, `tuile-cesium-ion`, `tuile-bing`, `tuile-native-fetchers`.

## Documentation

- [`docs/16-embedding-the-viewer.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/16-embedding-the-viewer.md)
- [`docs/11-crate-wgpu.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/11-crate-wgpu.md)
