<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-farm

A render farm's object storage, behind one repository.

Every byte a farm job moves — the pack in, the segments out, the logs, the
traces, the OptiX cache, the finished film — goes through [`RunStore`]. The
job holds a bucket's access key and secret and talks to the bucket itself;
nothing is presigned, and no launcher stands between the job and its data.

# Why a crate and not `curl`

`curl -o` on a presigned URL is one TCP stream. Measured on a Cloud Run L4
on 21 September 2026: a 2.3 GB pack in **88 seconds**, every task, before a
single frame — a single stream to object storage is capped far below what
the machine's network can carry. Many ranged reads in flight at once are
what the storage is built for, and a multipart upload is the only way to put
a film of several gigabytes at all. Both are the client's job, not ours:
[`object_store`] does the chunking, the part bookkeeping and the retries.

# One implementation, two backends

[`ObjectRunStore`] is written once against the `ObjectStore` interface and
built either on an S3-compatible bucket ([`ObjectRunStore::bucket`]) or on a
local directory ([`ObjectRunStore::local`]). The directory is not a mock:
it runs the same parallel reads and the same multipart writes, which is
what lets the contract below be checked on a workstation in a second and on
the real bucket with the same function.

## Where it sits

Depends on `tuile-pack` and `tuile-mp4`. Used by `tuile-repository`,
`tuile-pack-api` and `tuile-film-native`.

## Example

```rust,no_run
# async fn example() -> Result<(), Box<dyn std::error::Error>> {
use std::path::Path;
use tuile_farm::{ObjectRunStore, RunStore, Tuning};

// A directory, or `ObjectRunStore::from_env()` for the bucket.
let store = ObjectRunStore::local(Path::new("runs"), Tuning::default())?;
store.put(Path::new("film.mp4"), "films/demo/film.mp4").await?;
# Ok(())
# }
```

## Command-line tools

`tuile-farm` — a job's only way in and out of object storage.

```text
tuile-farm get <key> <dest>                  parallel ranged download
tuile-farm put <src> <key>                   multipart upload
tuile-farm exists <key>                      exit 0 present, 1 absent
tuile-farm list <prefix>                     "<size> <key>" per line
tuile-farm receipt <run> <task> <first> <last> <seg>[,<seg>…]
tuile-farm assemble <run> <task-count> <workdir> [fps]
tuile-farm concat <out> <segment>…           join locally, print the frames
tuile-farm render                            the render job itself
```

The store comes from the environment: `TUILE_STORE_DIR` for a directory,
otherwise `TUILE_STORE_ENDPOINT`, `TUILE_STORE_BUCKET`,
`TUILE_STORE_ACCESS_KEY_ID` and `TUILE_STORE_SECRET_ACCESS_KEY`.

`tuile-launch-job` — starts a farm execution, follows it and brings the film
home.

```text
tuile-launch-job bake   --trajectory orbit:4:2.17:42.52:8000:5000 --frames 1:4 --viewport 960x540 --sse 16
tuile-launch-job render --engine hydra --trajectory … --frames 1:4 --tasks 2 --width 960 --extra '--height 540'
tuile-launch-job film   --tape sky.mcap [--spatial] [--name <film>] -- <tuile-film-produce flags>
```

`tuile-migrate-packs` — re-bakes every pack a pack API lists so that it
refers to the tile store, in place, one pack at a time, with a ledger to
resume from.

```text
tuile-migrate-packs <api> <ledger.jsonl> [--max-gb N] [--only <substring>]
                    [--bake <tuile-bake>] [--work <dir>] [--tiles-bucket <name>]
                    [--dry-run] [--again]
```

Each binary's own rustdoc page lists its environment and exit codes.

## Features

None. Native only.

## Documentation

- [`docs/22-film-jobs.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/22-film-jobs.md)
