<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-pack

The pre-baked scene container.

# Why this exists

Frame 1 of a farm job costs about 500 seconds and is **89 % of a 48-frame
job**: the globe from cold, entirely on the CPU — fetching from ion and
Bing, decoding quantized-mesh, resampling, draping, baking, encoding PNG.
Sixteen processes on one pod each pay it from zero, on a machine rented for
its GPUs. And it puts the network, an access token and a convergence race
on the critical path of a render.

So the work moves. One machine bakes a frame range once and writes it here;
the render opens a file. What that buys is not only the time: a render with
no network and no traversal is reproducible **by construction** — not
because a race was stabilised, but because there is no race.

# The shape of a file

```text
[ "TUILEPK\0" ][ u64 table length ][ FlatBuffers table ][ blob region ]
```

The table is read **in place**: `mmap` it, or hand this a `&[u8]`, and
nothing is deserialised — no allocation proportional to the tile count, no
parse at open time. That is what matters at several thousand entries.

Payloads live after the table, each an independent LZ4 block. Compression
and zero-copy stop contradicting each other once they are separated like
this: the structure is walked in place, and a payload costs a memcpy only
when something asks for it. LZ4 rather than zstd on purpose — this is read
on the critical path and written once on a machine that has the time.

# What is deliberately *not* here

Anything a reader would have to compute. In particular the selection: a
pack records, per frame, exactly which tiles were selected. A render does
not traverse. The consequence is stated where it hurts — see
[`PackWriter::culling`] — because a hole baked in is a hole for ever.

## Where it sits

Depends on `tuile-core`. Written by `tuile-bake` and `tuile-farm`; read by
`tuile-film`, `tuile-film-native`, `tuile-hydra` and `tuile-repository`.

## Example

```rust,no_run
let bytes = std::fs::read("0-47.tuilepack")?;
let pack = tuile_pack::Pack::open(&bytes)?;
let (first, last) = pack.frame_range();
println!("frames {first}..={last}, {} tiles", pack.tile_count());
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Features

None.

## Documentation

- [`docs/22-film-jobs.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/22-film-jobs.md)
- [`docs/01-architecture.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/01-architecture.md)
