<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-film

A pre-baked film, read frame by frame.

Everything a frame needs was decided by the bake: which tiles, which
meshes, which imagery. What is left for a renderer is bookkeeping — which
tiles enter and leave between two frames — plus the camera and the look.
That bookkeeping lives here, free of any GPU and any I/O, so the same film
drives any renderer, a headless binary as well as one compiled to wasm.

## Where it sits

Depends on `tuile-core`, `tuile-pack`, `tuile-terrain`, `tuile-atmosphere` and
`tuile-overlay`. Used by `tuile-film-gpu` (which draws what it plans),
`tuile-film-native`, `tuile-bake` and `tuile-pack-api`.

## Example

```rust
use tuile_film::Look;

// The look a film gets unless it asks for another: no shadow, no air.
let look = Look::default();
assert_eq!(look.shadow, 0.0);
```

## Features

None. Wasm-clean.

## Documentation

- [`docs/22-film-jobs.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/22-film-jobs.md)
- [`docs/19-sky-atmosphere.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/19-sky-atmosphere.md)
