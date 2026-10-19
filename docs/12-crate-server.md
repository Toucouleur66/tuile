# 12 — `tuile-server` spec

`tuile-server` is the **network middleware** for the core's logical geometry server (see `01-architecture.md`) — the geometry server is not a network server, and it does not run here: it runs on the consumer's side, and this crate serves it what it fetches — plus a static "Martin of 3D Tiles" mode for interop and CDN. One binary, multiple sources, a removable cache, deployable from the laptop to the Cloudflare Worker.

## The two modes

### Streaming mode (the primary mode)

Streaming mode is NOT defined here: it is defined in the core, as a trait (`GeometryStream`, see `01-architecture.md`). The `GeometryServer` that drives it — traversal, the graphics budget, loads — **runs on the client**, whenever the client runs the core: the native viewers (desktop, iOS, Android, a headset), the web viewer's worker, Hydra through the C ABI, the film and bake sessions. This crate implements the **network transport underneath it**: a WebSocket over which the client's core requests the resources it has decided to load, and a **stateless resolver** that answers each one. A renderer that cannot link Rust is no exception: its geometry server runs in a local process beside it (`01-architecture.md`, the cross-boundary binding), and that process fetches from here like any other client.

**Decided on 2026-10-19** (`docs/explorations/graphics-budget-architecture.md`, D-A5): the per-session server-side `GeometryServer` this section used to describe is **replaced by a stateless resolver**. Why:

- the graphics budget decides what is loaded, uploaded, drawn and evicted, and its inputs live on the client: the GPU's memory and its hard limit (the guard against a jetsam or out-of-memory kill), memory pressure and the thermal state, the synchronous presence index of the local store (nothing held locally is fetched again), and the display rate that bounds uploads per frame;
- a budget in a server session decides one round trip late, and the hard limit would need a second enforcer on the client; a headset cannot afford decisions a round trip old;
- a stateless server scales out and caches: each response is a function of its key, so a CDN or a Cloudflare Worker serves it as it serves the static mode;
- every consumer that exists already runs the core.

None of this is built yet, so the decision changed no code and no current consumer.

How the transport works:

- **Requests, not a session.** The client sends `Request { id, key }` and `Cancel { id }`; the server answers `Reply { id, payload }` or `Error { id, … }`. A key names a resource the way a static `GET` would: a tileset's content by its path, a source tile — terrain, imagery — by its layer and address, as `tuile-tile-server` keeps it. Drapes are composed on the client, by its planetary loader, from those tiles. No `Hello` handshake beyond the envelope's version, no `ViewerState`, no `Ack`, no `Select`, no `Evict`: the client's own `GeometryServer` and its renderer exchange those — in-process, across a worker, or over a local socket — and they never leave the consumer's machine.
- **The serialization format for geometry on the wire is glb**: standard, compact, compressions (Draco/meshopt/KTX2) preserved. No homegrown binary format. Imagery travels as its source delivered it (PNG, JPEG, WebP, KTX2), as stored. The client decodes with the core's `content::decode()` — decoding always lives on the consumer side.
- The message envelope is minimal and versioned from the first byte (compact header + payload; exact format frozen in M2).
- Transport v1: WebSocket (axum/tokio-tungstenite native; web-sys on wasm clients — stable crates only). WebTransport/Connect = post-v1 tracks. The socket buys multiplexing and cancellation over one connection; the same keys stay reachable as plain `GET`s.
- Consumers, in order: 1) `tuile-wgpu-viewer --remote ws://…` — end-to-end proof and test tool: the viewer's own core, over this transport instead of HTTP; 2) **USD, notably the Hydra renderer** via the `tuile-hydra` facade, which runs the core through its C ABI — the server still does not know USD: it serves 3D Tiles resources, the client materializes.
- **Connections, not sessions.** Per connection, the server keeps only the ids of the requests in flight, bounded by a configurable maximum; an inactivity timeout and a maximum number of connections remain. A dropped connection loses nothing the client cannot re-ask.
- The server cache remains a source-bytes cache, shared with the static mode, and keyed like it: the same key, the same bytes, whichever transport asked.
- Workers: no session state, so no Durable Object is needed to hold one. A Worker answers each key from R2 and the Cache API as the static mode does; whether it also terminates the WebSocket or the client falls back to `GET` for the same keys is a deployment choice, not a protocol one.

### Static mode

Cacheable HTTP GET: tileset.json + contents. This is interop (any OGC 3D Tiles client: existing web viewers, three.js 3DTilesRenderer…) and CDN/Workers deployment. From the Tuile consumer's point of view, this mode is just the pull implementation of the same protocol (`HttpPullStream`: the `GeometryServer` runs on the client side and pulls the files via GET — exactly as it does over the WebSocket transport, which differs only in being one connection for many requests). Philosophy of THIS mode: **the server does nothing the CDN could do — except when there is no CDN.**

## Internal architecture

```
Router (axum native | worker cloudflare)
   ├── GET /ws/{id} (streaming) ──► Connection { requests in flight } ──► Resolver (stateless: key → glb | texture)
   ▼
Static service (shared logic, independent of the HTTP framework)
   ├── TilesetRegistry: id → Source (resolution, lazy validation of tileset.json via core)
   ├── TileCache (trait, see core): Noop | MemoryLru | Disk | CacheApi/R2
   └── Storage (trait): LocalFs | ObjectStore (S3/GCS/R2 via the object_store crate)
```

Where the pieces already are: `tuile-tile-server` keeps the source tiles once (PMTiles archives per zone and per layer, on object storage), and its `service` fetches upstream once, stores, serves, and keeps the hot tiles and the source's absences in memory — the source-bytes cache this crate shares between its modes. `tuile-repository` already writes an API as a function from a request to a reply, and `tuile-pack-api` hosts it natively. `tuile-server` is the HTTP and WebSocket front planned over them: the 3D Tiles routes, the stateless resolver, the WebSocket transport, the axum and Workers adapters.

The service logic is written ONCE, framework-agnostic: `async fn get_tileset_json(...)`, `async fn get_content(...)` functions taking `&dyn Storage`, `&dyn TileCache` and returning a `ServiceResponse { status, headers, body }` type. axum and worker are only adapters. This is what makes the Workers target trivial instead of a fork.

## Routes

| Route | Behavior |
|---|---|
| `GET /healthz` | 200, version, uptime |
| `GET /tilesets` | list of registered tilesets (JSON) |
| `GET /tilesets/{id}/tileset.json` | from Storage; `Cache-Control: max-age=60`; ETag |
| `GET /tilesets/{id}/{path...}` | content (glb/b3dm/subtree); cache→storage; `immutable, max-age=31536000`; sha256 ETag; Range supported; correct Content-Type (`model/gltf-binary`, etc.) |
| `GET /metrics` | `metrics` feature: Prometheus counters (cache hits/miss, bytes served, latencies) |

CORS: permissive by default (`*`) because the consumers are third-party web viewers; restrictable via config.

## Configuration

Minimal CLI + optional TOML file:

```bash
tuile serve ./data                          # fs, NoopCache, :8080
tuile serve s3://bucket/prefix --cache lru --cache-budget 1GiB
tuile serve ./data --cache disk --cache-dir /var/cache/tuile --port 9000
```

```toml
[server]      port = 8080, cors = ["*"]
[cache]       kind = "lru" | "noop" | "disk", budget = "512MiB", dir = "..."
[[tilesets]]  id = "demo", source = "s3://bucket/demo/"
```

Principle: zero mandatory config — `tuile serve <dir>` must work immediately, auto-discovery of the directory's `tileset.json` files (each subfolder containing a tileset.json becomes an id).

## Cloudflare Workers target (`cloudflare` feature)

- `worker` crate, built via `worker-build`. The shared Service is compiled as-is (it is wasm-clean as long as Storage/Cache are).
- Storage: R2 binding. Cache: `NoopCache` by default (the Cloudflare CDN + immutable headers do the work); Cache API as an option.
- No DiskCache nor persistent MemoryLru (ephemeral isolates) — this is exactly the "cache removable depending on the runtime environment" case.
- Ship `examples/cloudflare/`: wrangler.toml, end-to-end instructions (create R2 bucket, upload fixture, deploy, consume from a standard web viewer).

## Explicitly out of scope

The server NEVER depends on `tuile-usd` and exposes no USD route. It serves the 3D Tiles standard, period. USDZ export lives in the `tuile-usd` lib/CLI (see 13-crate-usd.md); if an HTTP export service ever becomes necessary, it will be a separate binary composing the lib — never a feature of the tile server.

## Security and robustness

- Path traversal: the `{path...}` are resolved WITHIN the tileset prefix, rejection of `..`, strict validation. Dedicated test.
- Limits: configurable max response size, storage timeout, simple per-IP 429 as an optional feature (not in v1 core).
- Never panic on network input: everything is `Result`, errors → clean status codes (400/404/413/502).

## Tests

- Streaming: end-to-end over a WS connection on a fixture — the client's core (traversal + budget) drives the requests, and its selection and decisions equal those of the same core over the static HTTP mode on the same camera (parity tested programmatically); every reply equals the static `GET` of the same key, byte for byte (cacheability); a reply depends on its key alone (two connections, any order, the same bytes); cancellation; connection timeout; malformed messages → clean Error, never a panic.
- Integration (native): ephemeral server on the fixtures, assertions on status/headers/ETag/304/Range/CORS/path traversal.
- Consumer conformance: `tests/html/` pages (a reference 3D Tiles web viewer + three.js 3DTilesRenderer, via CDN) pointing at localhost — documented manual verification, to run before each release.
- Cache: exact LRU budget, hit/miss, Noop passthrough, key includes the tileset's ETag (invalidation if the source changes).
