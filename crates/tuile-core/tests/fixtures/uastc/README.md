<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# UASTC reference tile

One 64×64 tile and its UASTC blocks, copied unchanged from texcomp's
reference vectors (`tests/fixtures/uastc/coverage/level4/alps-0-crop.*` and
`coverage-tiles/alps-0-crop.png`, texcomp `main` at `990de23`), for
`tuile_core::stored`'s tests.

| File | Contents |
|---|---|
| `alps-0-crop.png` | the 64² RGBA8 input |
| `alps-0-crop.uastc` | its 256 UASTC blocks (16 bytes each, row-major), written by the upstream UASTC encoder at effort level 4 |
| `alps-0-crop.astc` | each block's ASTC 4×4 transcode, by upstream's transcoder |
| `alps-0-crop.bc7` | each block's BC7 transcode, by upstream's transcoder |
| `alps-0-crop.decoded` | per block, two little-endian `u32`: the 32-bit FNV-1a digest of the 64 RGBA8 bytes upstream unpacks, linear then sRGB |

The capture (upstream tool, version, command lines) is documented in
texcomp's `tests/fixtures/uastc/README.md`. The imagery is NASA Blue Marble
(NASA EOSDIS GIBS), in the public domain. Credit: NASA Earth Observatory.
