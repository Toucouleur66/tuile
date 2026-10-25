<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# GPU-compressed imagery in the viewer

Status: **shipped in v0.14.0** (roadmap item A6). Rewritten on 25 Oct. 2026
to describe what was built; the 21 Oct. design it replaces is summarised in
§10. What remains is the budget's accounting of compression (§9.2).

Scope: the interactive viewers, native and web (`tuile-core`'s format seam,
`tuile-wgpu`, `tuile-viewer`, `tuile-web`, the decode jobs in
`tuile-planetary`). The bake and the film renderer stay RGBA8 and are out of
scope.

## 1. The rule: compression is the client's choice, off by default

- **tuile never decides.** A consumer of tuile (tuile-viewer, a game, a
  downstream product) chooses how its drape textures are held with an
  `ImageryCompression`. The default is `Off`, and `Off` is byte-identical to
  the RGBA8 path that existed before A6: no new device feature requested, no
  encoder created, the same goldens and digests.
- **No compression in the bake, none in the film.** Both pin RGBA8.
- **No licence rule in tuile.** Whether a source may be held compressed, or
  stored compressed, is the consumer's call; tuile encodes in memory only and
  guarantees nothing about it.
- **A chosen format the GPU cannot sample, or a tile that cannot be encoded,
  falls back to RGBA8**, never to a missing texture (§7).
- **÷4 only:** BC7 and ASTC 4×4, 8 bits per texel. No 4-bit tier (BC1, ETC2
  RGB8), and no ETC2 for now: every target that has ETC2 also has ASTC or BC.

The decision of 24 Oct. and its consequences for a tile store are in
[`compressed-imagery-in-pmtiles.md`](compressed-imagery-in-pmtiles.md).

## 2. What shipped

| Step | PR | What |
|---|---|---|
| S1 | #114 | the format seam in `tuile-core::texels`, wgpu-free |
| T1 | #121 | tiles **stored** compressed (KTX 2.0): transcode only |
| G1 | #122 | the GPU encode path in `tuile-wgpu`; `--imagery-compression` in the native viewer and the headless runner |
| C1 | #127 | the web: GPU encode on WebGPU, CPU encode in the decode workers on WebGL2; the `imageryCompression` page option |

The encoders live in their own crate,
[`lapoule-dev/texcomp`](https://github.com/lapoule-dev/texcomp) (public,
pinned by tag, `v0.2.0` today): WGSL compute kernels for BC7 and ASTC 4×4
behind its `gpu` feature, and a pure-Rust, wasm-clean CPU side (UASTC encoder,
UASTC → ASTC 4×4 and UASTC → BC7 transcoders, a KTX 2.0 reader).

### 2.1 The seam (`tuile-core/src/texels.rs`)

Nothing here names a graphics API.

- **`GpuTextureCaps { bc, astc, compute }`** — what the consumer's adapter can
  sample, and whether it runs the GPU encoder. Filled by the host (from wgpu
  with `tuile_wgpu::texture_caps`, from another engine, or by hand in a test).
- **`ImageryCompression { Off (default), Auto, Format(BlockFormat) }`** — what
  the consumer asks for. `plan(caps)` resolves it to an `ImageryPlan`:
  - `Off` → RGBA8 whatever the device can do;
  - `Auto` → `select(caps)`;
  - `Format(f)` → `f` where the device samples it and something can encode
    it, RGBA8 otherwise.
- **`ImageryPlan { target: ImageryFormat, encode: ImageryEncode }`** —
  `ImageryFormat { Rgba8, Bc7, Astc4x4 }` is what a texture is held in once
  resident; `ImageryEncode { None, Gpu, Cpu, Transcode }` is who turns it into
  that.
- **`select(caps)`** — the helper behind `Auto`, a pure function, called only
  by a consumer that opted in. In order: compute + BC → BC7 on the GPU;
  compute + ASTC → ASTC 4×4 on the GPU; ASTC without compute → ASTC 4×4 on the
  CPU; BC without ASTC or compute → BC7 on the CPU; else RGBA8. Where an
  adapter has both (Apple silicon, WebGPU on Apple), BC7 wins on quality
  (§8). The capability matrix is a unit test naming every target platform.
- **`Texels { Rgba8 { pixels, resident }, Blocks { format, levels } }`** on
  `DecodedTexture` — the format a texture will be held in is recorded on the
  texture itself (`Texels::resident`), so `cost()` takes no argument and the
  runtime that charges it and the consumer that uploads it read the same
  answer. RGBA8 that the GPU will encode travels as `Rgba8 { resident: Bc7 }`
  (or `Astc4x4`).
- **`StoredPlan`** (`ImageryCompression::stored(caps)`) — what to do with a
  tile stored already compressed (§5.4). `StoredPlan::OFF` by default.
- **`encode_on_cpu(texture, format)`** — the CPU path (§5.3).

### 2.2 The device side (`tuile-wgpu/src/compress.rs`)

- `texture_caps(adapter)` and `plan_for(choice, adapter)` give the plan and
  the device features it needs: the format's feature (and `TIMESTAMP_QUERY`
  where present) **only when the choice needs it**. `Off` requests nothing
  new.
- `GpuContext::with_imagery_plan(plan)` and `GpuContext::headless_with(choice)`
  hold the plan; the loader marks what it decodes with the same plan through
  `GlobeOptions::imagery_plan` (an `ImageryPlanHandle`, set once the device
  exists).
- `compression_named` parses `off | auto | bc7 | astc`, the names the native
  flag and the web option share.

## 3. Formats per backend (wgpu 30)

| Platform / backend | BC7 | ASTC 4×4 | Compute | `Auto` gives |
|---|---|---|---|---|
| macOS, Metal (Apple silicon) | yes | yes | yes | BC7, GPU |
| iOS / iPadOS, Metal | no (wgpu reports no BC off macOS) | yes | yes | ASTC, GPU |
| Android / Quest, Vulkan | rarely | mostly | yes | ASTC, GPU |
| Windows / Linux desktop, Vulkan or DX12 | yes | rarely | yes | BC7, GPU |
| WebGPU desktop | `texture-compression-bc` | on Apple | yes | BC7, GPU |
| WebGPU mobile | rarely | `texture-compression-astc` | yes | ASTC, GPU |
| WebGL2, phone | rarely | `WEBGL_compressed_texture_astc` | **no** | ASTC, CPU |
| WebGL2, desktop | with all four s3tc / s3tc_srgb / rgtc / bptc extensions | rarely | **no** | BC7, CPU |

All formats are sRGB (`Bc7RgbaUnormSrgb`, `Astc { B4x4, UnormSrgb }`), so
sampling matches `Rgba8UnormSrgb`. The bind group layout and the ground
shader are unchanged: a filterable `texture_2d<f32>` binding takes any of
them. A block texture can be neither a render target nor a storage texture,
so the GPU encoder writes a storage buffer and copies it into the texture
level by level. Level 0 must be whole 4×4 blocks; any other size stays RGBA8.

## 4. The encoders (texcomp)

**GPU (`texcomp::gpu`, WGSL, our own code written from the Khronos Data
Format Specification).** One invocation per 4×4 block, 8×8 workgroups, one
compute pass per call with a dispatch per (tile, level), blocks written to a
storage buffer then `copy_buffer_to_texture`. The CPU builds the mip chain
(`raster::mip_chain`); the kernels encode each level.

- **BC7:** modes 6 and 1 (64 partitions, chosen by an estimate then fitted
  fully), a principal-axis fit with least-squares refinement, an early out
  when mode 6 fits; opaque alpha kept at 255.
- **ASTC 4×4:** two fixed single-partition configurations (CEM 8 opaque,
  CEM 12 with alpha). Its weak case is bright points on black (night lights,
  36.4 dB); a two-partition mode is the lever if that ever shows on screen.

**CPU (texcomp default build, pure Rust, `wasm32-unknown-unknown` clean).** A
port of the block layer of Basis Universal v2.50 (Apache-2.0, attributed in
texcomp's `NOTICE`): the UASTC LDR 4×4 encoder, UASTC → ASTC 4×4 (lossless by
construction), UASTC → BC7 (within a small loss), the UASTC unpack, and a
KTX 2.0 reader. No ETC1S, RDO, Zstd or ETC2.

## 5. The paths in the viewers

### 5.1 Native, GPU encode (G1)

- The loader decodes JPEG/PNG to RGBA8 as before and marks it with the
  plan's target (`Texels::Rgba8 { resident }`), so the budget charges blocks.
- At upload, `upload_texture` encodes it with `texcomp::GpuEncoder` and a
  reused `EncoderScratch`, mip chain included, into a texture of the target
  format; ready blocks upload row by row.
- **Any encode that cannot be done uploads RGBA8**: a size that is not whole
  blocks, a tile too large for the device, a context without an encoder.
  `EncodeStats { encoded, fell_back, measured_ns_per_block }` counts them.
- Pacing: the pump gives encodes their own GPU-time share per frame,
  `EncodePace` (4 ms by default), divided by the cost of a block — measured
  with timestamps around the compute pass where the device has
  `TIMESTAMP_QUERY`, assumed from texcomp's published cost otherwise. The
  first encode of a frame always goes. This sits **outside** the budget's own
  accounting; §9.2 replaces it.
- The viewer: `--imagery-compression off|auto|bc7|astc` (also on the headless
  runner), and *Settings › Tuile › Imagery compression* on iOS, read at
  launch. The status line shows what is held, e.g. `imagery BC7 31 MiB`.

### 5.2 Web, WebGPU (C1)

The page owns the device, so the page resolves the choice (`plan_for` on its
own adapter) and sends the plan to the session's worker in its start message;
the worker's loader passes it to the decode workers inside each imagery job.
The decode workers send RGBA8 marked with its resident format, and the page's
pump encodes it on the GPU exactly as natively. With compression off the
session starts at once, as before; with it on, the start waits a fraction of
a second for the device's answer, so that no tile is decoded under the wrong
plan.

### 5.3 Web, WebGL2: the CPU path (C1)

WebGL2 has no compute shaders. Each decode worker runs
`tuile_core::texels::encode_on_cpu`: the whole mip chain is built in the
worker, each level encoded to **UASTC at level 0** (the fastest effort level)
and transcoded to ASTC 4×4 (phones) or BC7 (desktops with the BPTC
extensions). The blocks cross the worker → page wire as `Texels::Blocks` and
upload as blocks. A tile that cannot be encoded crosses as RGBA8. Measured in
desktop Chrome (wasm, M-series): 16–25 ms per 256² tile, mean ≈ 20 ms, peak
≈ 45 ms, in the worker, off the page's thread.

### 5.4 Tiles stored compressed: transcode only (T1)

A tile served as a KTX 2.0 file is recognised by its content, never by a
setting, and filled without any encoder by the consumer's `StoredPlan`:

| Stored | Plan | Texels |
|---|---|---|
| UASTC | a block format the device samples | `Blocks`, every level of the file transcoded (`ImageryEncode::Transcode`) |
| UASTC | `Off`, or nothing sampled | `Rgba8`, level 0 unpacked bit-exact, then the usual path |
| BC7 / ASTC 4×4 | the client holds that format | `Blocks`, the file's levels as they are |
| BC7 / ASTC 4×4 | not held | an error: not resident, the ancestor climb covers it |

Blocks are never resampled: off the geographic grid, UASTC is unpacked and
reprojected like a JPEG. tuile reads KTX 2.0; it never writes it (the bake
writes PNG). Whether a store should serve such tiles is the subject of
[`compressed-imagery-in-pmtiles.md`](compressed-imagery-in-pmtiles.md).

## 6. The budget today

- `Texels::cost(w, h)` charges the resident format for `gpu` and the format
  that crosses for `staging`: RGBA8 staging for a GPU encode, blocks for a CPU
  encode or a transcode. `Cost { gpu, staging }` already separated the two.
- **The ack carries the real resident bytes:** a drape is acknowledged in the
  format each tile actually went resident in (`DrapeContent::cost_with`,
  `DecodedTexture::cost_as`), so an RGBA8 fallback is charged as RGBA8.
- Bake and film pin `StoredPlan::OFF` and an RGBA8 plan; their digests and
  goldens did not move.

What this does **not** yet count — the encoder's scratch, the transient peak
while RGBA8 staging and blocks coexist, encode time inside `FrameDeadline`,
the difference between bytes moved and bytes resident in `plan` — is §9.2.

## 7. Never black ground

- An encode that cannot be done (GPU or CPU) gives an RGBA8 tile, never a
  missing one. A stored BC7/ASTC tile the device cannot hold is not resident,
  and the consumer's climb to the nearest resident ancestor covers it, as for
  a failed fetch.
- A slow encode only delays a tile; the ancestor climb covers it.
- Swap order unchanged: the compressed texture is created, encoded and
  copied before anything binds it, and the tile it replaces is released
  after.
- Tests: `no_black_ground` (with a draped scenario), `fast_zoom_never_empties`
  and `imagery_passes` run under Off, BC7 and ASTC 4×4 on 256² imagery and
  check the tiles really were encoded; `compressed_imagery` checks the
  block-sized ack, an unencodable 250² tile held, drawn and acked as RGBA8,
  and the pacing. Each was seen red with its behaviour reverted.
- Blocks never straddle tiles (sizes are multiples of 4) and imagery edges
  clamp, so compression adds no seam.

## 8. Quality gates (texcomp, Apple M2, 21 Oct.)

Measured by GPU decode (`textureLoad` in a compute pass) on five NASA GIBS
tiles cut into 256² tiles and four synthetic tiles, with PSNR and ΔE2000 (via
`palette`), plus a hole check (a texel the source draws decoding to black or
transparent fails, on every level). Each gate sits about 1 dB or 20 % beyond
the worst measured tile:

| | PSNR RGB ≥ | ΔE2000 mean ≤ | ΔE2000 p99 ≤ | alpha PSNR ≥ |
|---|---|---|---|---|
| BC7, NASA tiles, levels ≥ 8×8 | 39.0 dB (worst 40.30) | 2.5 (2.09) | 6.5 (5.35) | |
| BC7, synthetic, level 0 | 45.0 (48.13) | 0.5 (0.36) | 1.5 (1.18) | 55.0 (62.44) |
| ASTC 4×4, NASA tiles, levels ≥ 8×8 | 35.0 (36.41) | 3.0 (2.43) | 11.5 (9.40) | |
| ASTC 4×4, synthetic, level 0 | 48.0 (51.14) | 0.4 (0.26) | 1.5 (1.14) | 50.0 (56.65) |

These worst cases are why `select` prefers BC7 where an adapter has both.
With the kernels' `encode()` removed, the gate fails with 214 hole reports and
every PSNR below 14 dB.

## 9. Measured, and what remains

### 9.1 Measured in the viewer (G1, C1)

Native headless runner, `p2-slow` (Paris from 160 km to 5 km), desktop
1280×800 @2, Apple M-series, Metal — read from the runs' own verdict lines:

| plan | imagery on the GPU at its peak | **bytes per texture** | every upload landed |
|---|---|---|---|
| off | 80.7 MB | **349,524** | 5.0 s after settling |
| auto (BC7) | 15.5 MB | **87,408** | 10.2 s |
| astc | 13.8 MB | **87,408** | 12.2 s |

- **÷4 per texture**, read from the acks: 349,524 → 87,408 bytes for a 256²
  tile with its mip chain.
- **BC7 costs 9–10 ms of GPU per 256² tile**, from the encoder's timestamps
  (about 1.7–1.9 µs per block), against the 5.1 ms wall-clock figure texcomp
  published and `EncodePace` still assumes before its first measurement.
- **The regression:** with BC7 the meshes settle as fast, but the imagery
  lands at 10.2 s instead of 5.0 s (ASTC 12.2 s), because `EncodePace` lets
  the first encode of every frame through — at 9–10 ms a tile, about one tile
  a frame, well over its 4 ms share.
- Settled pictures, same view: ΔE2000 against off, median 0.79 (BC7) / 0.81
  (ASTC).
- Web (`scripts/web-bench.py`, desktop Chrome, 1280×800 @2): WebGPU auto holds
  BC7 at 85 KiB per texture against 341 KiB off; WebGL2 auto holds ASTC 4×4
  (CPU path) at 85 KiB; coverage holes 0 and black patches 0 on every run.
- **Not measured yet:** a real phone (WebGPU ASTC GPU encode, WebGL2 CPU
  path), a headset.

### 9.2 What remains: the compression budget (B1–B8)

The next step makes the graphics budget account for compression instead of
pacing it beside the budget. Its rules: no switch (configuration numbers
only), `Off` byte-identical (goldens and `det!` digests), never black
ground, `tuile-core` wasm-clean and wgpu-free.

1. **B1 — cost per path.** `Cost` gains `transfer`, `transient` and
   `encode_blocks` beside `gpu` (equal to today's values when nothing is
   compressed). GPU encode: transfer 349,524, resident 87,408, transient
   349,524 + 87,408, 5,463 blocks; CPU encode or stored transcode: 87,408
   everywhere; RGBA8: 349,524 everywhere. `plan` caps on `transfer` (today it
   caps on resident bytes while 349 KB crosses — a 4× under-count). Fallback
   acks are counted as fallbacks, not as estimator drift.
2. **B2 — transient peak and reservations.** A GPU-encode delivery is held
   at `gpu + transient` until the GPU has finished it; fixed reservations
   for the encoder scratch on the GPU, and for the web CPU path on the decoded
   tier. Effective limits subtract them.
3. **B3 — encode time in `FrameDeadline`.** An encode lane beside the byte
   lane: a token bucket, no overtaking, swap groups kept whole, a measured
   cost per block (timestamps, else an estimate reset to the measured 9.5 ms
   per BC7 tile), an adaptive share from measured headroom, and a
   deterministic cap in `decide`. `EncodePace` is removed (a breaking change).
4. **B4 — simulation.** `budget_sim` profiles for desktop BC7, handheld ASTC
   GPU and web WebGL2 CPU, with peak, ledger and encode invariants; existing
   goldens byte-identical, new pattern goldens.
5. **B5 — the wgpu pump runs the core's deadline.**
6. **B6 — the web CPU path's accounting.**
7. **B7 — signals:** encode fields in the frame record, metrics, an MCAP
   channel, a HUD line.
8. **B8 — measurement:** imagery peak ≤ 15.5 MB compressed, with peak +
   reserved + settling never above the hard limit; desktop BC7 all uploads
   landed ≤ 6.0 s (Off 5.0, today 10.2), ASTC ≤ 7.0 s (today 12.2); frame p99
   ≤ Off + 1 ms; the phone on WebGPU and WebGL2, filmed; holes 0 everywhere.

Outside that step: splitting one tile's encode across frames (a 90 Hz headset
cannot fit a 9.5 ms BC7 tile in one frame), re-encoding resident RGBA8,
compressed glTF/3D Tiles and terrain textures, ETC2, an ASTC two-partition
mode.

## 10. Design history, briefly

- **21 Oct.** First design: encode on the fly on the client, two encoders in
  `texcomp` (GPU kernels, then a CPU port of Basis Universal for WebGL2), and
  selection *strictly from what the adapter reports*, with no flag. The GPU
  kernels and the quality gate were built that day; ASTC went into the GPU
  encoder from the start because iOS and the Quest have no BC.
- **24 Oct.** Laurent reversed the selection rule: **the client chooses,
  off by default**, nothing in the bake, no licence rule in tuile. `select`
  became a helper for a client that opted in. The same day, the PMTiles study
  opened the stored-compressed path, built as T1.
- **Measured in G1:** the GPU encoder's real cost (9–10 ms per BC7 tile, not
  5) and the settle regression it causes, which §9.2 addresses.
- Dropped along the way: BC7 mode 5 (mode 6 alone gives 62 dB alpha on the
  alpha-edge tile), a 4-bit tier, ETC1S, and `zune-png` for PNG decoding
  (slower than `png` 0.18 natively and in wasm; the bench is PR #110).
