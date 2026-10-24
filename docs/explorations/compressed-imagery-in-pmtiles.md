<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# GPU-compressed imagery stored in PMTiles — an impact study

Status: study, 24 Oct. 2026. Nothing here is decided and no code changed.
The question: *the tile store must be able to serve GPU-compressed textures
from its PMTiles archives. What does that change in the bake and the file
preparation, and in rendering, if the downstream film product moves onto it?*

It revisits one line of the A6 design note (PR #107, §1): "nothing is stored
compressed". It builds on the PMTiles compression study (PR #63), which
measured KTX2/UASTC against the production JPEGs, and on P0 as approved:
`select(caps) -> ImageryPlan { target, encode: Gpu | Cpu | None }`, the texcomp
GPU encoders (BC7, ASTC 4×4), and the texcomp CPU port. In the port, UASTC
unpack, UASTC → ASTC and UASTC → BC7 are done and bit-exact with upstream
(texcomp `main` and `feat/cpu-uastc-bc7`). The UASTC encoder (levels 0–2)
has not started. RDO and Zstd are out of the port's scope.

Code read at `origin/main` (c2a6720). The downstream repository was read on
its production branch and was not modified. Numbers marked *(estimate)* are
not measurements.

**Short answer.**

- **What it buys:** CPU time on the client, not GPU memory. A6 already gets
  the ÷4 GPU memory by encoding at load. Stored blocks remove the client's
  JPEG decode, reprojection, mip chain and encode, and on WebGL2 they give
  the ÷4 without the UASTC encoder in wasm.
- **What it costs:**
  - **4–6× the bytes** of the source JPEG on disk and on the wire: raw
    UASTC with mips is 87.4 KB, against a 7–30 KiB source tile;
  - a UASTC encode per tile at preparation, 10–100 ms *(estimate)*;
  - reprojection moves to the bake;
  - pack digests and film goldens change.
- **For the film product:** none of this pays today.
  - Its renderer composes drapes into RGBA8 storage textures, which no block
    format can be.
  - Its imagery is commercial, and a stored derivative of it is exactly what
    the licence memo forbids until the terms are reopened.
  - The film's VRAM win, if wanted, comes from encoding **composed drapes**
    to BC7 on the L4. That is independent of how tiles are stored.
- **Recommendation:**
  - build the capability generically in tuile: a *prepared imagery* layer
    (KTX2, UASTC, full mips, geographic spacing) made by a bake step, and a
    fourth way to fill a tile on the client, transcode only;
  - give it first to open or generated imagery served to the viewer;
  - leave the film product on JPEG until the licence decision.

## 1. What to store

### 1.1 UASTC once, or BC7 and ASTC per target

| | UASTC (one layer) | BC7 + ASTC 4×4 (two layers) |
|---|---|---|
| Bytes per texel | 8 | 8 each, 16 if both kept |
| Client work | transcode: UASTC → ASTC is lossless and UASTC → BC7 loses 0.75–1.5 dB (texcomp `src/uastc/mod.rs`, upstream's figure). Upstream measured 0.37 ms (ASTC) and 0.45 ms (BC7) per 256² tile native (PR #63 §3.2); texcomp's port is not timed yet | none: upload as is |
| Uncompressed fallback (WebGL2 without the format, adapters without either) | UASTC unpack to RGBA8, bit-exact, already in texcomp | needs a BC7 or ASTC CPU decoder, which texcomp does not have |
| Encoder at preparation | UASTC encoder (texcomp 3c, CPU, not started) | texcomp's GPU kernels, which exist but need a GPU in the bake, or two CPU encoders that don't exist |
| Quality | one encode, then two exact or near-exact transcodes | each format at its own encoder's best: ASTC 36.4–60 dB, BC7 40–59 dB on NASA tiles (PR #107 §9.3) |
| Layers, digests, extracts | one | two, and an extract must choose |

**UASTC.** A single layer serves every consumer, the RGBA fallback is
already written, and the expensive part runs once, at preparation. The bake
runs on CPU machines (the downstream bake pods have no GPU), which rules out
the GPU kernels there.

### 1.2 Container per tile

| | KTX2 | Minimal block header |
|---|---|---|
| Overhead per 256² tile with 9 levels | 80 B header + 9 × 24 B level index + about 50 B data format descriptor, about **350 B = 0.4 %** | 16–32 B |
| Carries | format (UASTC through the descriptor), size, levels, and optional per-level supercompression (Zstd) | what we define |
| Interop | the Khronos container. glTF's `KHR_texture_basisu`, which 3D Tiles content uses, is KTX2 + UASTC/ETC1S. Any tool can open a tile cut out of the archive | ours alone |
| Reader cost | a small parser, about 200 lines, pure Rust, wasm-clean | trivial |

**KTX2.** 0.4 % is nothing, and the tile becomes a standard file: a server
that cuts it out of an archive answers `image/ktx2`, which other 3D Tiles and
glTF clients understand. The reader goes in texcomp (default build, wasm-clean),
next to the transcoders, so tuile-core gains no dependency.

### 1.3 Mips: in the tile, or derived from the pyramid

- **In the tile.** This is the recommendation, at +33 % (65,536 → 87,408 B
  for a 256² tile).
- **The pyramid's parent is not the child's mip.**
  - It is another capture, reprojected on its own rectangle, and graded by
    another level's table.
  - The measured rupture between z12 and z13 is ±1 stop, with a white
    balance drift.
  - A mip taken from the parent would bring that rupture inside one tile, at
    every mip transition.
- **Building mips on the client is not an option.** The client would need
  to unpack, filter and re-encode, which is the encoder this whole design
  avoids.
- **The film uses level 0 only.**
  - `create_imagery` has `mip_level_count: 1`, and the film builds its own
    mips on composed drapes by compute.
  - So an extract made for the film could keep level 0 alone: the extract
    writer would truncate each KTX2.
  - That is an option, not a need.

### 1.4 Sizes

Per 256² tile:

| Form | Bytes | bpp | vs source JPEG |
|---|---|---|---|
| Source JPEG, production commercial layer (PR #63 §1, measured 7–30 KiB; 20–60 kB by the downstream estimate) | 7–30 KiB | 0.9–3.8 | 1 |
| NASA fixtures (texcomp, 512² JPEG) | 11.7–52.4 KiB per 512² | 0.4–1.6 | — |
| RGBA8, level 0 / with mips | 262,144 / 349,524 | 32 | — |
| **UASTC raw, level 0 / with mips** | **65,536 / 87,408** | 8 / 10.7 | **2.2–9× / 2.8–12×**, about 5.7× at a 15 KiB median |
| + KTX2 | +about 350 | | |
| UASTC RDO 1.0 + Zstd in KTX2 (upstream basisu, PR #63 §3.2, level 0) | median 3.98× source (2.16–5.42 by group) | | SSIMULACRA 2 = 89, PSNR-Y 46 dB |
| UASTC RDO 3.0 + Zstd | median 3.45× | | SSIMULACRA 2 = 76 (visible) |
| Raw UASTC + Zstd, no RDO | *to measure*: expected −5 to −20 % *(estimate)* | | lossless over UASTC |

What the RDO and Zstd row says:

- On real tiles, RDO 1.0 with Zstd brings raw UASTC down by 0–40 %.
  - On a 29 KiB level-11 source tile: 2.16× source, about raw size.
  - On a 7 KiB level-19 source tile: 5.42×, about −41 % against raw.
- It brings more only on near-uniform tiles: 2.50× on a 2.1 KiB top tile.
- **Even then, the stored layer is 3.5–4× the JPEG.**

**What RDO and Zstd would cost to add** (both outside the port's scope today):

- **RDO.** It is an encoder pass: it perturbs block choices so that an LZ
  coder finds more matches. Porting upstream's UASTC RDO is about 1.5–2.5k
  more lines *(estimate)*, plus reference vectors for each RDO setting. It
  costs quality, about 1–3 dB, and is visible at 3.0. It slows the encode
  by 2–4× *(estimate)*.
- **Zstd.**
  - *Writer* (bake, native): the `zstd` crate, which binds C. That is fine
    in a native bake, and needs no C in wasm because wasm never writes.
  - *Readers*: `ruzstd`, pure Rust, about 60–100 KiB of wasm (PR #63 §4).
  - Every reader must first honour the archive's `tile_compression`, which
    none does today (PR #63 §1: `tuile-repository` and `TileStore::get`
    pass stored bytes as they are).
  - Zstd belongs in **PMTiles' per-archive `tile_compression`**, not in KTX2
    supercompression. That keeps one decompression point, the reader, for
    every layer, terrain gzip included.
- **Recommendation:**
  - store raw UASTC first, `tile_compression: none`;
  - measure Zstd alone on real prepared tiles;
  - decide on RDO only if the wire size blocks a consumer.

### 1.5 PMTiles tile type and compression

- **Tile type.** PMTiles v3's `tile_type` is unknown, mvt, png, jpeg, webp,
  avif or mlt. The workspace's patched `pmtiles` crate
  (`lapoule-dev/pmtiles-rs`, branch `tuile-accessors`) reads any other byte
  as `PmtError::InvalidTileType`.
  - A fork extension would make our archives unreadable by every other
    reader, and the fork exists to be dropped (`Cargo.toml:143-149`). **No.**
  - **Use `Unknown` + metadata.** This is the store's precedent: terrain is
    already `other` with `content_type: application/vnd.quantized-mesh`
    (`tuile-bake/src/open.rs`, `store_layers`), and extracts are `Unknown`
    with `{"tuile:extract": [{name, grid, band, content_type}]}`
    (`tuile-tile-server/src/extract.rs:246-261`).
  - A prepared layer's `LayerDef` is `tile_type: "other"`,
    `content_type: "image/ktx2"`. The decode dispatches on `content_type`.
- **Compression.** `tile_compression` is per archive and comes from the
  catalog (`catalog.rs:47,92-95`): `none` first, then `zstd` once the
  readers honour it.
- **Never `gzip` on a stored image.** PR #63 measured 0.984 on JPEG. UASTC
  without RDO is not much better.

## 2. Bake and preparation

### 2.1 Where the encode happens

Today the store keeps tiles **exactly as their source delivered them**
(`tuile-tile-server/src/layer.rs:37-40`, `archive.rs:6-7`). Every pixel
operation happens in the consumer: viewer, bake loader, film.

Blocks can't be resampled, cropped or tested by a consumer without first
unpacking them. So every pixel operation a consumer does to a tile on its
own moves ahead of the encode.

| Operation today | Where | With stored blocks |
|---|---|---|
| JPEG/PNG decode (`raster::decode_image`, `raster.rs:1610`) | decode job, film, bake loader | preparation |
| Web Mercator → geographic spacing, per tile on its own rectangle (`decode_and_reproject`, `raster.rs:1635`; `reproject_to_geographic`, `:645`) | every consumer, CPU, f64 | **preparation**, by the same function. The prepared tile keeps its source address (z/x/y on the source grid) and holds geographic-spaced texels, exactly what `decode_and_reproject` returns today |
| Opaque-black check (`tuile-planetary/src/lib.rs:176`, sampled at 100 points) | decode job | **preparation**, where a black tile becomes an **absence** in the layer's `.absent` sibling, the way the source's "no imagery" placeholder already is (`tuile-bing`). The client never needs to unpack for it |
| Mip chain (`raster::mip_chain`, `:1684`, CPU, Triangle filter) | viewer, on the uploading thread | preparation, each level encoded |
| Composition of a drape from several imagery tiles (`bake_layers`, `raster.rs:1318`; the film's `compose.wgsl`) | bake (embedded packs), film | **stays in the consumer**: it depends on the terrain tile, not on the imagery tile. The CPU replica (`bake_layers`, the film's translucent stacks at `render.rs:885`) unpacks UASTC → RGBA8 first, bit-exact. The GPU path reads blocks directly (§3.3) |
| `stitch_mosaic` (`:610`), `upsample_quadrant` (`:1651`) | no caller outside tests at `origin/main` | nothing to do; a future caller unpacks first |
| Radiometric calibration | gains measured on texels, stored as `tone.json` per layer, applied **in the shader at render** (`tuile-film-gpu/src/shaders/compose.wgsl:79-86`) | **not baked into the texels.** The grade is applied to `textureLoad × 255`, whatever format the texture holds. Measurement must read the texels the render draws, so the prepared layer gets its own `tone.json`, measured on unpacked UASTC |

Order of the preparation, per imagery tile:

1. fetch, through the store as today; the source layer is unchanged;
2. decode;
3. reproject;
4. check for opaque black, and record an absence if so;
5. build the mip chain;
6. encode each level to UASTC;
7. write the KTX2;
8. write it into the **prepared layer**, a separate catalog layer derived
   from the source layer.

**After convergence.** Encoding the whole pyramid is out of the question.
The bake knows the exact set of imagery tiles a film draws:

- its pack's references, which `tuile_film::store_tiles` lists;
- downstream, the `converge` activity, which already gathers that set into
  `_extracts/<run>/cNNNN.pmtiles`.

Preparation fits between the two: prepare the tiles of the set that the
prepared layer lacks, then extract from the prepared layer. Tiles are
prepared once and shared by every later film, as source tiles are today.

**After radiometric calibration.** This is true only in the sense that
`tone.json` is measured on the prepared layer, after it is written. The grade
stays a render-time multiply. Baking it into the texels would make every
retune a re-encode.

### 2.2 CPU cost

Encode per 256² tile with its mips, single thread, texcomp UASTC port,
*(estimate)*:

| Level | Native | Note |
|---|---|---|
| 0 | 10–20 ms | PR #107 §5.4: 3–10 Mtexel/s for levels 0–1 |
| 1 | 15–30 ms | |
| 2 | 30–100 ms | upstream's default. Its extra modes and partitions cost 2–4× *(estimate)* |
| Reprojection + mips + decode | 2–5 ms | already paid today, moved, not added |

**Per film**, downstream:

- Chunks are 300–900 frames: 1,600 frames give 4 × 400, and 5,400 give
  12 × 450.
- The only per-chunk tile count found is a test message naming **5,860**
  missing tiles, a lower bound for one chunk. The downstream converge metrics (tiles and extract bytes per chunk) hold
  the real figure.
- At 6–10k imagery tiles per chunk, all of them cold: **60–300 CPU-s per
  chunk at levels 0–1, up to 1,000 CPU-s at level 2.** That is 1–40 min of
  one core, and ÷8 on the bake's cores.
- Warm tiles cost nothing.
- **This becomes the bake's bottleneck on cold ground.**
  - The bake fetches about 850 tiles/s per process.
  - At 30 ms per tile, 8 cores encode about 270 tiles/s, about 3× slower
    than the fetch.

### 2.3 Storage and transfer

- **The source layer stays.** It is the only thing that can be prepared
  again: at another level, with RDO, or by a fixed encoder.
- **So the store grows by the prepared layer:** about 5.7× the source
  imagery's bytes raw, 3.5–4× with RDO 1.0 + Zstd.
- **Extracts carry the prepared tiles.**
  - Today, one chunk is about 6–10k × 15 KiB, or 90–150 MB of imagery.
  - Raw UASTC with mips makes it 520–870 MB; level 0 only, 390–650 MB.
- **Bucket egress is not billed on the store's object storage.** What grows
  is time:
  - a 2.3 GB pack took 88 s on one stream on an L4;
  - the extract is fetched in 32 MiB parts, in parallel;
  - so about +5–20 s of start-up per chunk *(estimate)*;
  - and a 4 GiB `TUILE_SERVE_CACHE_GIB` holds 4–6× fewer tiles.
- **For a CDN-served viewer, the wire is the cost.** A phone fetches
  87 KB per tile instead of 15 KiB, which is 4–6× the radio time and data
  plan, against a ÷4 GPU memory it already gets from A6.

### 2.4 Determinism and goldens

- **The encoder must be deterministic.**
  - Prepared bytes are what a pack's reference digests (FNV-1a over the
    stored bytes, `tuile-film/src/from_store.rs:54-63`).
  - texcomp's 3c targets bit-exact output where upstream is deterministic
    (single thread, integer). That must hold on every architecture, x86
    farm and ARM laptop alike, or digests differ by machine.
  - Test: the same fixture is prepared on Linux x86 and macOS ARM in CI,
    with identical KTX2 bytes.
- **No pack moves on its own.** A prepared layer is a new layer, so packs
  naming the source layer stay valid while it lives. A film moved onto the
  prepared layer is re-baked; the 6 Oct. migration rule already accepts that
  re-bakes need not be identical.
- **Film goldens.** `tuile-film-gpu/tests/headless.rs` freezes PNGs from
  fixture bytes.
  - A prepared fixture draws different texels: UASTC is lossy, about 46 dB
    PSNR-Y.
  - So it gets **new goldens of its own**. The JPEG goldens stay as they are
    for as long as JPEG is read.
  - Nothing is re-blessed blind.

## 3. Rendering

### 3.1 The viewer: a fourth way to fill a tile

P0's plan is `ImageryPlan { target: Rgba8 | Bc7 | Astc4x4, encode: Gpu | Cpu | None }`.
`encode` says how RGBA becomes the target. A stored tile has no RGBA to start
from, so the input's form joins the decision:

| Input | Target | Work in the decode worker | Notes |
|---|---|---|---|
| JPEG/PNG | as P0 | as P0 | unchanged |
| KTX2/UASTC | ASTC 4×4 | parse, then `transcode_to_astc_4x4` per level: lossless, about 0.4 ms native *(upstream figure)* | |
| KTX2/UASTC | BC7 | parse, then `transcode_to_bc7` per level, about 0.5 ms native | |
| KTX2/UASTC | RGBA8 | parse, then unpack, bit-exact; mips come from the tile | WebGL2 without a block format; never black |

- **Where it goes in P0's types.** It is a property of the payload, read
  from the layer's `content_type`, not a setting. `ImageryDecode` gains the
  payload kind, and its output is the `Texels::Blocks { format, levels }`
  that S1 defines.
- **Nothing else in S1, G1 or C1 changes.** Upload, wire and cost already
  handle `Blocks`.
- **No GPU encode for these tiles.** The pump's GPU encode cap doesn't
  apply to them. Staging is ÷4, which is the CPU path's figure (PR #107
  §9.5: 90,368 B per tile instead of 360,192).

**What it changes for C1 and for 3c.**

- **C1 (WebGL2) gets the ÷4 without the encoder in wasm** for stored
  layers. The transcoders are already written and bit-exact, so the long
  pole (3c) leaves the client's critical path for those layers.
- **3c is still needed, on the bake side.** It runs natively, multi-threaded,
  with no wasm speed constraint. The 10–30 ms wasm budget of PR #107 §5.4
  stops mattering for stored layers.
- **3c in wasm stays needed** for sources the viewer fetches as JPEG
  (commercial imagery, any template URL). The viewers fetch their sources
  directly today; the store is not on their path (rule of 17 Oct.).

**Getting stored tiles to the viewer.** Two routes, each of which already
exists for other content:

- a template URL that answers `.ktx2`. P1a's `--imagery-template` plus a
  content-type dispatch is the smallest change;
- the store read through `tuile-repository` (wasm-clean, `ArchivedTiles`),
  as film-web already does.

**Budget.** No change beyond S1. Resident bytes are `drape_cost(.., Block16)`,
staging is the blocks' staging, and the ack sends the real size.

**Fallbacks: never black ground.**

| Failure | What draws |
|---|---|
| KTX2 unreadable, or a level missing | the decode job errors. The tile is not resident, and the consumer's climb to the nearest resident ancestor covers it, as for a failed fetch. There is no RGBA source to fall back to, so the error is counted and reported, never silent |
| Target format unsupported | unpack to RGBA8 (bit-exact), the same tile, uncompressed |
| Tile absent (black at preparation) | the ancestor, as for any absence |
| Swap | unchanged: the new texture is active before the old one is released |

Tests:

- `no_black_ground` and `fast_zoom_never_empties` run with a prepared
  256² fixture layer;
- one case serves a corrupt KTX2 on every second tile and asserts coverage
  by ancestors;
- the PR #107 rule applies, fixtures 4-aligned and 256².

### 3.2 The web worker wire

- C1 already adds a block form to `tuile-web/src/wire.rs` (`texture()`, the
  format tag and the levels). Stored tiles use it unchanged.
- The worker posts 87,408 B (all levels) instead of today's 262,144 B
  (RGBA level 0).
- What grows is the network fetch into the worker, ×4–6 (§2.3).

### 3.3 The film renderer

Today:

- `tuile-film-native/src/render.rs:858-868` decodes each imagery tile with
  `imagery_texture` (`decode_and_reproject`).
- It uploads RGBA8 into a single-level `Rgba8Unorm` texture (`create_imagery`
  and `write_rgba`, `tuile-film-gpu/src/lib.rs:1820,2010`).
- It composes each terrain tile's drape on the GPU with `compose.wgsl`,
  which reads `textureLoad(src) × 255`. The grade comes in linear light,
  and the result goes into an RGBA8 **storage** texture: the drape, up to
  `TUILE_BAKE_MAX` = 2048², 16 MiB each.
- Translucent stacks are composed on the CPU.

Two ways to read a prepared layer:

| | A. Unpack UASTC → RGBA8 on the CPU | B. Transcode to BC7 and sample blocks on the L4 |
|---|---|---|
| Code change | `imagery_texture` dispatches on `content_type`. The rest is untouched | `create_imagery` takes a block format, `write_rgba` gets a block twin, and `compose.wgsl` is unchanged: `textureLoad` on `texture_2d<f32>` reads any filterable format |
| Determinism, Mac vs L4 | identical: the CPU unpack is integer and bit-exact | the L4 has BC and no ASTC. Both sides must use BC7, which means the lossy UASTC → BC7 step and a hardware BC7 decode. BC7 decoding is exactly specified, so it should match, but this needs a test |
| Quality vs goldens | the UASTC decode, about 46 dB from the JPEG. New goldens (§2.4) | the same, plus BC7's −0.75 to −1.5 dB |
| VRAM gain | none on imagery sources | ÷4 on the **source** imagery textures only |
| CPU per tile | about 0.5–1 ms unpack *(estimate)*. Reprojection, about 1–2 ms today, is gone because it moved to preparation | about 0.5 ms transcode |

**Neither touches where the film's VRAM goes.**

- That is the composed drapes: storage textures, RGBA8 by construction,
  up to 16 MiB each before mips.
- Imagery source textures are 256 KiB each and live for one chunk of
  tiles.
- **The film's real lever is an encode of each composed drape to BC7 on
  the L4** after `compose`, with texcomp's GPU kernel: 5 ms per 256² on an
  M2, so about 300 ms per 2048² drape *(estimate)*. That saves ÷4 on what
  is actually resident.
- It is the same lever whether tiles are stored as JPEG or as UASTC, and
  it touches goldens the same way, so it is a separate item.

**Recommendation for the film.** A, if and when the film reads a prepared
layer. The drape BC7 encode is measured separately against its own goldens.

## 4. Licences

| Source | Licence (catalog `licence`) | Stored compressed derivative |
|---|---|---|
| NASA Blue Marble, Black Marble (GIBS) | public domain (`open.rs`, `blue_marble_layer`, `black_marble_layer`) | free; attribution kept in the layer |
| Open orthophotos (e.g. Etalab 2.0) | allows derivatives, with attribution | free; attribution carried by the prepared layer |
| Sentinel-2 (the downstream `asset-3954`, the radiometric reference) | the source's terms as served by the connector | to check |
| Commercial aerial imagery (the downstream `asset-2`, the source behind the ion connector) | proprietary: "the source's terms of service" | **blocked until the terms are reopened** |

The memos are explicit:

- `poc-on-commercial-imagery-ok`: PoCs on the commercial imagery are fine
  "as long as nothing becomes a derived product deployed in production, and
  as long as everything is recomputed on the fly". The licence question is
  raised again **"when a derived layer is about to be stored or served in
  production. At that point an open source becomes the default again."** A
  prepared UASTC layer of commercial imagery is exactly that.
- `render-reads-the-store`: the store holds provider imagery and must not
  become durable. A prepared layer of a commercial source inherits the
  source's expiry (152 days downstream) and the same re-fetch-at-source rule.
  It is never `DURABLE_EPOCH`.
- `licence-lab-vs-shipped`: anything shipped, data included, must allow
  commercial use.
- PR #63 §4 reaches the same verdict for any pixel re-encoding.

**In code.** The preparation step derives a layer only from a source layer
whose catalog `licence` allows derivatives. It reads that from the data and
refuses otherwise. This is a data rule, not a flag.

## 5. One path

The 17 Oct. rule is one pipeline for every source: store → bake →
convergence → render, with no special path.

- **One format in the stock for every consumer** works only if *every*
  imagery layer a consumer reads is prepared. For the film product, that
  means the commercial layer, which is blocked (§4).
- **Mixing formats** keeps the pipeline single, with every stage the same,
  but gives each consumer two payload decoders: JPEG/PNG → RGBA, and KTX2 →
  blocks or RGBA.
  - That is data-driven dispatch on `content_type`, as JPEG vs PNG is today,
    not a switch.
  - It still means two code paths to test, on the same fixtures.

So the honest answer:

- **For the film product, one path means JPEG everywhere, now.** Prepared
  layers enter the film path only for all of its sources at once, after the
  licence decision.
- **For the viewer,** prepared layers are a second payload kind behind one
  decode entry point. The viewer already has two inputs: source bytes, and
  RGBA that the GPU encodes.

## 6. Recommendation and plan

**Recommendation.**

1. P0 goes ahead unchanged. Encoding at load stays the way the commercial
   imagery reaches the ÷4.
2. Stored GPU textures become a generic tuile capability, for open or
   generated imagery: a prepared KTX2/UASTC layer, and a transcode-only fill
   on the client.
3. The film product stays on JPEG. The film's VRAM lever is a BC7 encode of
   composed drapes on the L4, a separate item.
4. No commercial imagery is prepared until its terms are reopened. The
   preparation enforces that from the catalog.
5. Raw UASTC first. Zstd comes after the readers honour `tile_compression`;
   RDO only if a consumer's wire budget demands it.

**PRs, in order.**

| # | Repo | What | Size | Depends on |
|---|---|---|---|---|
| — | texcomp, tuile | **P0 as approved**, including 3c (UASTC encoder levels 0–2) | | |
| K1 | texcomp | KTX2 read/write for UASTC with mips: header, level index, data format descriptor. Pure Rust, wasm-clean, tests against files written by the upstream tool | S | |
| R1 | tuile | readers honour each archive's `tile_compression`: `tuile-repository` and `TileStore::get` (PR #63 §5, "readers first") | S–M | |
| T1 | tuile | **Stored fill**: `ImageryDecode` takes KTX2 by `content_type` and returns `Texels::Blocks` (transcode) or `Rgba8` (unpack); tests for never-black, corrupt tiles and the budget | M | S1, C1's wire, K1 |
| B1 | tuile | **Preparation step** in tuile-bake: source layer → prepared layer, in the order of §2.1. Licence rule from the catalog, cross-arch byte-identical test, CPU time per tile measured and recorded in this note | M–L | 3c, K1 |
| V1 | tuile | viewer: an imagery template or store layer that serves `.ktx2`, the P1a test server included. Measured on the reference phone: wire bytes, ms per tile, memory, against P0's encode-at-load | M | T1, B1 |
| F1 | tuile | film: `imagery_texture` dispatches on `content_type` (unpack, option A), with new goldens from a prepared NASA fixture | S–M | K1, B1 |
| F2 | tuile | film: BC7 encode of composed drapes on the L4, VRAM and goldens measured. Independent of everything above | M | P0's texcomp tag |
| Z1 | tuile, texcomp | Zstd on prepared layers (`tile_compression: zstd`, `ruzstd` in wasm); then, only if needed, the RDO port | M / L | R1 |
| D1 | downstream | **only after the licence decision**: catalog entry for the prepared layer, preparation in the `converge` activity before `extract_whole_into`, tuile tag bump, a farm render on the L4 fetched and opened | M | B1, F1, licence |

**Risks.**

- **Wire size.** 4–6× per tile is the real price, on phones above all. V1
  must show that the client CPU saved is worth the radio time spent, or the
  capability stays for desktop, headsets on LAN, and the film.
- **Encoder determinism across architectures.** A float-order difference in
  3c means digests that depend on the machine. The cross-architecture test
  in B1 is a gate, not a nice-to-have.
- **Cold-ground bake time.** Encode is about 3× slower than fetch per
  tile, so cold films bake slower until their tiles are prepared once.
- **Radiometry.** `tone.json` must be measured on the texels drawn. A grade
  measured on JPEG and applied to UASTC differs by the codec's error. The
  differences are small, but a seam check (`no_seams`) must run on prepared
  layers.
- **Two payload decoders** in every consumer, for as long as a commercial
  source stays JPEG.
- **Reprojection in the bake** freezes the f64 resampling into bytes. A
  later fix to `reproject_to_geographic` means re-preparing.

## ⚑ Decisions for Laurent

1. **What is it for?** Client CPU and WebGL2, on the viewer, or film VRAM?
   The answer picks V1 or F2 first.
2. **Commercial imagery.** Reopen its terms now, or keep it out of any
   prepared layer, so that the film product stays on JPEG?
3. **Container.** KTX2 (recommended) or a minimal header of our own?
4. **RDO and Zstd.** Leave them out of texcomp's scope until V1 measures the
   wire (recommended), or plan them now?
5. **UASTC level for preparation:** 1 or 2. Decided on B1's measured
   quality and time.
6. **Film,** if it ever reads prepared layers: CPU unpack, deterministic
   (recommended), or BC7 sampling on the L4?
