<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-hydra

The C ABI an OpenUSD/Hydra plugin calls to turn a camera into tiles.

A **specialised façade**, in the sense `docs/01-architecture.md` means it:
there is no generic FFI layer, and this crate exposes what a Hydra plugin
consumes — one frame at a time, fully resolved, with buffers laid out the
way Hydra wants to read them.

The work itself lives in [`tuile_bake`], which knows nothing about OpenUSD.
That separation is not tidiness: `tuile-bake` produces a container any
renderer can read, and while the session lived here, the binary that wrote
that container linked a crate named after one particular host. The format is
neutral; the dependency graph said otherwise.

# Rules at the boundary

- **No panic may cross.** An unwind through `extern "C"` aborts the process,
  so every entry point catches and returns a status instead.
- **Rust owns every buffer.** The C++ side borrows pointers that stay valid
  until it releases the frame, and frees nothing itself.
- **Calls arrive from Hydra's worker threads.** `GetPrim` and
  `GetChildPrimPaths` are documented as thread-safe, so anything reachable
  from them must be too.

## Where it sits

Depends on `tuile-bake`, `tuile-pack` and `tuile-core`. Built as a static
library and linked by the OpenUSD/Hydra plugin in `integrations/hydra`;
nothing in the workspace depends on it.

## Example

The plugin's side (`integrations/hydra/src/ffi.h` declares it): open a
session, ask it for a frame, read the frame's tiles, release both.

```text
tuile_session_new(…)        → a session over a scene
tuile_session_frame(…)      → one frame, fully resolved, for a camera
tuile_frame_tile_count(…), tuile_frame_tile(…), tuile_frame_texture(…)
tuile_frame_free(…), tuile_session_free(…)
```

```bash
cargo build --release -p tuile-hydra    # → target/release/libtuile_hydra.a
```

## Features

None.

## Documentation

- [`docs/14-crate-hydra.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/14-crate-hydra.md)
- [`docs/15-usd-scene-index.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/15-usd-scene-index.md)
- [`docs/17-hdgp-incremental.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/17-hdgp-incremental.md)
