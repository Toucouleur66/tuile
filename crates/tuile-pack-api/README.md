<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-pack-api

The bench's API, served by a native process.

The routes and what they mean are `tuile_repository::bench`'s: this server
opens the buckets a configuration names, hands them to a `Bench`, and
turns each request into a call to it. Its readers are the farm's own
programs (`tuile-migrate-packs` lists the projects and their packs here)
and any client of the store routes (`TUILE_TILES_REMOTE`).

**Nothing is downloaded whole.** Each project's bucket is wrapped in
`Cached`: a pack runs to gigabytes and a reader asks for a few megabytes
of it at a time, so objects are fetched in fixed chunks, on demand, each
chunk once.

What is served is described by a configuration file (`--config`, default
`film-bench.toml`): the projects, each a bucket read through one layout,
and the tile store. See `crates/tuile-pack-api/film-bench.example.toml`. The
buckets share one endpoint and one key, taken from the environment as the
farm takes them — `TUILE_STORE_ENDPOINT`, `TUILE_STORE_ACCESS_KEY_ID`,
`TUILE_STORE_SECRET_ACCESS_KEY` — with a `.env` in the working directory
read first. Credentials stay in this process.

```bash
cargo run --release -p tuile-pack-api -- --config film-bench.toml
```

## Where it sits

A binary. Depends on `tuile-repository` (the routes), `tuile-storage-foyer`,
`tuile-farm`, `tuile-film`, `tuile-radiometry`, `tuile-tile-server` and
`tuile-core`. Its clients are the farm's programs and the film pages.

## Usage

```bash
cargo run --release -p tuile-pack-api -- \
    --config film-bench.toml --addr 127.0.0.1:8090 --cache .pack-cache
```

| Flag | Default | Meaning |
|---|---|---|
| `--config <path>` | `film-bench.toml` | The projects and the tile store to serve. |
| `--addr <ip:port>` | `127.0.0.1:8090` | Where to listen. |
| `--cache <dir>` | `$TUILE_PACK_CACHE`, else `.pack-cache` | Where the chunks read from the buckets are kept, one directory per project. |

`RUST_LOG` sets the log filter (default `info`).

## Documentation

- [`docs/22-film-jobs.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/22-film-jobs.md)
