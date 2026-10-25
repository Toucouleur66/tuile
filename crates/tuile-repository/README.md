<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-repository

Where films live.

A film is baked into one pack or several, from source tiles — and each of
those is written to a bucket under a key layout. This crate is the read
side of those layouts, so that nothing downstream builds a key by hand:

- [`Objects`] — bytes in a bucket, read by range. A bucket or directory
  ([`tuile_farm::ObjectRunStore`]), a store served by someone else
  ([`RemoteLive`], [`RemoteBlocks`]), and [`Cached`], which wraps any other
  and keeps each chunk it has read in a [`ChunkStore`].
- [`FilmRepository`] — the films a bucket holds and what each is made of.
  [`ScenePacks`] reads the engine's own layout (packs keyed by scene
  digest); [`RunFilms`] reads run directories as an orchestrator lays
  them out, from a [`RunLayout`] it is configured with.
- [`TileRepository`] — the source tiles, by layer and coordinates.

Every implementation of a trait here can stand in for any other: the
contracts below are written so a caller never needs to know which one it
was handed. A film from either repository is the same [`Film`], its chunks
ordered, their frame ranges known, their keys readable through the same
[`Objects`] the repository was built on.

The contracts and the layouts compile to wasm32; the adapters that reach a
bucket from a native process (`native`) do not, and are not built there.

## Where it sits

Depends on `tuile-core`, `tuile-pack`, `tuile-farm`, `tuile-tile-server` and
`tuile-radiometry`. Used by `tuile-pack-api`, which serves it over HTTP, and by
`tuile-film-native`, which renders from it.

## Example

List the films a bucket holds, whatever reaches the bucket:

```rust,no_run
use std::sync::Arc;
use tuile_repository::{FilmRepository, Objects, RepoError, ScenePacks};

async fn list(bucket: Arc<dyn Objects>) -> Result<(), RepoError> {
    let films = ScenePacks::new(bucket, ["packs"]);
    for film in films.films().await? {
        println!("{film:?}");
    }
    Ok(())
}
```

## Features

None. The `native` adapters build on every target but wasm32.

## Documentation

- [`docs/22-film-jobs.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/22-film-jobs.md)
- [`docs/12-crate-server.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/12-crate-server.md)
