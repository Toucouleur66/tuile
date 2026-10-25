<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# catalog-proxy

The tile catalog behind a proxy that holds its token.

A web page cannot keep a secret, so the web viewer is never given the
catalog's token. It is pointed at this proxy instead (`sources.catalog.url`),
which holds the token in its own environment and answers the catalog's
endpoints and tiles without it: see `tuile_cesium_ion::proxy` for what it
answers and what it refuses. The same binary serves a developer's machine
and production.

```text
TUILE_ION_TOKEN=… tuile-catalog-proxy --assets 1,2,3,3954
tuile-catalog-proxy --env-file .env --assets 1,2,3,3954 \
    --serve /examples/web-viewer=examples/web-viewer \
    --serve /packages/tuile-viewer=packages/tuile-viewer
```

| Flag | Meaning |
|---|---|
| `--assets 1,2,…` | the asset numbers it answers for; any other is a 404 |
| `--listen ADDR` | where it listens; `127.0.0.1:8091` unless said |
| `--mount PATH` | where the catalog is served; `/catalog` unless said |
| `--public-url URL` | the catalog's address as pages reach it, behind a load balancer; the request's own address unless said |
| `--allow-origin ORIGIN` | a page origin that may read the replies; repeat for several; any origin unless said |
| `--env-file PATH` | a `.env` file to read the token from, for development |
| `--serve URL=DIR` | also serve the files of `DIR` at `URL`, for development; the first is where `/` leads |
| `--tls-cert PEM --tls-key PEM` | serve HTTPS with this certificate: a phone on the local network gives WebGPU, geolocation and the clipboard only to a secure page |

**The token** is read from `TUILE_ION_TOKEN` — in production, a secret
injected into the process's environment by the deployment, never a flag,
never a file in an image. It is never printed, and the proxy refuses to
send any reply that would carry it.

## Built on

`tuile-cesium-ion` (feature `proxy`). Serves the web viewer and the Android build.
