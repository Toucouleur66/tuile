<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-usd

The manifest stage: a recorded camera path plus one `Globe` prim.

This crate is the answer to "how does a track become a render" (docs/15):
a tape (`tuile-tape`, MCAP) is turned into a tiny `.usda` — an animated
`UsdGeomCamera` written as `matrix4d` time samples, and a
`GenerativeProcedural` prim whose primvars configure the TuileGlobe
procedural. Every Hydra host renders the flight from there: `usdrecord`
reads it natively, Blender imports the camera and re-references the prim.

Deliberately free of any USD dependency — the manifest is text, and text
is what a review reads.

## Where it sits

Depends on `tuile-tape` and `tuile-core`; no USD library. Nothing in the
workspace depends on it: its output is read by a USD host and the
`tuile-hydra` procedural.

## Example

```rust
use tuile_tape::Frame;
use tuile_usd::{write_manifest, ManifestConfig};

let frames = [Frame {
    position: [6_378_137.0 + 1_000.0, 0.0, 0.0],
    direction: [-1.0, 0.0, 0.0],
    up: [0.0, 0.0, 1.0],
    fovy: 1.0,
}];
let mut usda = Vec::new();
write_manifest(&frames, &ManifestConfig::default(), &mut usda)?;
assert!(String::from_utf8(usda)?.starts_with("#usda"));
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Command line

`tuile-tape-to-usd-stage` writes the manifest stage of a recorded tape.

```bash
cargo run -p tuile-usd --bin tuile-tape-to-usd-stage -- flight.mcap flight.usda \
    [--fps 24] [--viewport 1920x1440] [--terrain <asset>] [--imagery <asset>] [--sse <px>]
```

## Features

None.

## Documentation

- [`docs/13-crate-usd.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/13-crate-usd.md)
- [`docs/15-usd-scene-index.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/15-usd-scene-index.md)
