<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-bake

The pack pipeline: a camera path in, a container out, and the same container
read back. Nothing here knows what will draw the result.

# Why the session lives beside the bake, and not beside a renderer

It used to live in `tuile-hydra`, and that had a visible consequence: **the
bake depended on Hydra**. The binary that writes a deliberately neutral
container — plain buffers, PNG, no host concepts anywhere in the schema —
linked a crate named after one particular OpenUSD host, and imported
`tuile_hydra::Session` to do work that has nothing to do with OpenUSD.
Anyone reading the dependency graph would conclude the pack was a Hydra
artefact. It is not, and the graph should not have said so.

So the split follows what the code actually does:

* here — the globe and its sources, the traversal, the session that resolves
  a camera into tiles, the writer that freezes that into a pack, and
  [`packed`], which reads one back. 3D Tiles and nothing else.
* in `tuile-hydra` — the C ABI, and only that: buffers laid out the way
  Hydra wants to read them, a status code instead of an unwind, and the
  thread-safety its worker pool requires.

Reading a pack sits here rather than with the consumer on purpose: the code
that writes a format and the code that reads it drift apart the moment they
live in different crates with different reasons to change.

# Why bulk and not streaming

The rest of the project streams: tiles arrive progressively and the picture
sharpens. That is right for a viewer and wrong for a renderer. In
progressive mode the selection depends on *which tiles happened to arrive*,
so two machines rendering the same frame can disagree — and a farm would
show it as flicker between frames rendered on different nodes, a defect that
is close to impossible to diagnose after the fact.

So a frame here is one blocking, converged answer
([`tuile_core::drive::drive_until_complete`]): the selection is a function of
the camera and the sources, not of the network's mood.

## Where it sits

Depends on `tuile-core`, `tuile-planetary`, `tuile-terrain`, the connectors
(`tuile-cesium-ion`, `tuile-bing`, `tuile-native-fetchers`), `tuile-pack`,
`tuile-film`, `tuile-tape`, `tuile-tile-server` and `tuile-metrics`. Used by
`tuile-film-native` (which bakes before it renders) and `tuile-hydra` (whose
C ABI wraps the session).

## Example

Read back a frame range baked earlier:

```rust,no_run
use tuile_bake::packed::Packed;

let pack = Packed::open(std::path::Path::new("shot.tuilepack"), None)?;
# Ok::<(), tuile_bake::PackedError>(())
```

## Command line

`tuile-bake` bakes a camera path's frame range into a pack, and inspects,
compares and checks packs.

```bash
TUILE_ION_TOKEN=… cargo run --release -p tuile-bake -- \
    --tape traj.mcap --frames 1:48 --out shot.tuilepack
```

```text
tuile-bake --tape <path.mcap> --frames <first>:<last> --out <path.tuilepack>
           [--viewport <w>x<h>] [--sse <error>]
           [--imagery <asset>] [--terrain <asset>]
           [--content embedded|references|both]
tuile-bake --rebake <old.tuilepack> --out <path.tuilepack> [--sse <error>]
           [--content references|both]
tuile-bake --inspect <path.tuilepack>
tuile-bake --reference <path.tuilepack> [--reference-asset <asset>] [--reference-cap <level>]
tuile-bake --diff <a.tuilepack> <b.tuilepack>
tuile-bake --dump <path.tuilepack> --frame <n>
tuile-bake --verify <path.tuilepack> --tape <path.mcap> [--viewport <w>x<h>]
```

`--rebake` re-bakes with the old pack's own cameras, frames and viewport, and
refuses unless every frame selects and drapes what the old pack did.
`--reference` brings a reference imagery's tiles into the tile store over the
ground the pack's imagery covers, and bakes nothing. `--accept-drift` lets a
re-bake through when its selection drifts.

Environment: `TUILE_ION_TOKEN` (required), `TUILE_CACHE_DIR` (the tile cache;
strongly advised), `TUILE_MAX_SSE` (overrides `--sse`, for sweeps), the thread
and fetch counts (`TUILE_SESSION_THREADS`, `TUILE_TILES_THREADS`,
`TUILE_DECODE_THREADS`, `TUILE_FINISH_THREADS`, `TUILE_FETCHES`,
`TUILE_HTTP_IDLE_PER_HOST`), the tile store's concurrency (see
`tuile-tile-server`), and every traversal setting the render honours.
`tuile-bake --help` prints the same list.

## Features

- `profiling` — a heap profiler (jemalloc + pprof). Off by default: it
  replaces the process's allocator.

## Documentation

- [`docs/22-film-jobs.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/22-film-jobs.md)
- [`docs/01-architecture.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/01-architecture.md)
