<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# decode-bench — PNG tile decoding, timed two ways

Decodes real PNG tiles with the `png` crate through `image` (what
`tuile_core::raster::decode_image` runs today) and with `zune-png`, both into
tightly packed RGBA8, which is what the decode job hands on. The two outputs are
checked byte-identical before any number is printed: a faster decoder that
decodes differently is not a candidate.

It runs natively and as `wasm32-wasip1` under Node. It is a measurement, not a
library: `publish = false`, and no other crate depends on it.

## Run it

```text
# native
cargo run --release -p tuile-decode-bench -- <png files or directories>

# wasm32-wasip1 under Node's WASI (one directory of PNGs, preopened at /tiles)
cargo build --release -p tuile-decode-bench --target wasm32-wasip1
node examples/decode-bench/wasi-run.mjs target/wasm32-wasip1/release/tuile-decode-bench.wasm <dir>
```

Add `RUSTFLAGS="-C target-feature=+simd128"` to the wasm build to measure the
SIMD build.

## The tiles

The tiles are not committed. The numbers below come from 25 VIIRS Black Marble
tiles from NASA GIBS (public domain, 512² RGB, 270 KiB on average):
`VIIRS_Black_Marble/default/2016-01-01/500m/7/{17..20}/{78..81}.png` and
`.../5/{3..5}/{19..21}.png` under
`https://gibs.earthdata.nasa.gov/wmts/epsg4326/best/`.

## Result (Apple M2)

| | png 0.18 via image | zune-png 0.5.2 | ratio |
|---|---|---|---|
| native | 1.63 ms/tile | 2.11 ms | 0.78× |
| wasm32-wasip1 (Node 26) | 2.17 ms | 2.38 ms | 0.91× |
| wasm32-wasip1 + simd128 | 2.25 ms | 2.49 ms | 0.91× |

`zune-png` is slower on every target, so it does not replace PNG decoding in
the imagery decode. JPEG is not compared: `image` 0.25 already decodes it with
`zune-jpeg`.
