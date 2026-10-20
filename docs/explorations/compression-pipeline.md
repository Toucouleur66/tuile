<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Final-stage compression for rendered films

Status: research and design, 20 Oct. 2026; decided the same day and built
on branch `feat/film-compression` (§8, §9). Nothing is deployed.
Scope: the last stage of a film's pipeline, which turns rendered pictures into
the file a viewer gets. It is offered to the user as a choice of **compression
profile**. The GPU paths covered are NVIDIA (NVENC on L4 / RTX PRO 6000) and
Apple (VideoToolbox on the Mac).

Numbers marked *(to measure)* are engineering estimates. They are not
measurements, and the measurement plan in §6 exists to replace them.

---

## 1. Where encoding happens today

### 1.1 The chain

```
STL workflow (Temporal, stl-render-worker)
  prepare → plan_bake cN → converge cN           (stl-farm, k8s CPU workers)
  render_segment cN ──► tuile-film-serve          (Cloud Run "tuile-render", L4, europe-west1;
                         render() → Sink           spatial: RTX PRO 6000, europe-west4)
                         segment.mp4 → stl-renders (R2)
  assemble_film ──► tuile_farm::assemble          (stl-farm k8s CPU worker)
                     → tuile_farm::concat → <run>/render.mp4 (R2 stl-renders)
  publish ──► copy render.mp4 → stl-videos v/<id>/film.mp4   (stl-core video_export::publish)
```

- **Encoder = a `Sink`** (`crates/tuile-film-native/src/sink.rs`). The render
  hands it each picture, either as RGBA or as I420 converted on the GPU
  (`tuile-film-gpu/src/shaders/i420.wgsl`). The sink encodes on **its own
  thread**, a few pictures behind the render (`WAITING = 4`). It is chosen by
  `setup::film_sink(path, bitrate, codec)`:
  - Linux + `nvenc`: `NvencFilm`, which takes `h264`, `nvenc-av1` (the
    default when no codec is given) or `mv-hevc`
    (`crates/tuile-film-native/src/nvenc.rs`).
  - macOS + `videotoolbox`: `H264Film` (VTCompressionSession,
    `videotoolbox.rs`), or `SpatialFilm` (MV-HEVC through
    AVAssetWriter, `spatial.rs`).
  - Fallback: `Av1Film` (rav1e, speed 10, 8-bit).
- **The service's default** (`RenderSpec::default`, `engine.rs`) is
  `codec: "h264"` at `mbps: 12.0`, **whatever the resolution**. STL's
  `look_of()` (`stl-film-control`) overrides nothing for mono films, and
  for spatial films sets `mv-hevc` at 30 Mb/s for both eyes.
- **The join** (`crates/tuile-farm/src/concat.rs`) copies samples without
  decoding. It handles only `avc1` (H.264) and spatial `hvc1` (MV-HEVC
  through `tuile_mp4::spatial::join`), and it refuses segments whose SPS/PPS
  differ. **AV1 segments cannot be joined today.**
- **Assembly** runs on a k8s CPU worker (`stl-farm`'s `assemble_film`
  activity), and **publish** copies the assembled file as it is.

### 1.2 The encoder settings, as read in the code

| | NVENC H.264/AV1 (`nvenc.rs::encode`) | NVENC MV-HEVC (`encode_eyes`) | VideoToolbox H.264 | rav1e |
|---|---|---|---|---|
| Preset / tuning | P4, `HIGH_QUALITY` | P4, `HIGH_QUALITY` | `RealTime=false` | speed 10 |
| Rate control | VBR 1-pass, avg = bitrate, max = 2× | VBR, multipass off | `AverageBitRate` | bitrate |
| Lookahead | **off** | off | n/a | 1 frame (`low_latency`) |
| B-frames | **none** (`frameIntervalP = 1`) | none | **none** (`AllowFrameReordering=false`) | none |
| GOP | 2 s, an IDR per GOP | 2 s | 2 s | 2 s |
| Bit depth / chroma | 8-bit 4:2:0 (IYUV input) | 8-bit, Main | 8-bit 4:2:0 | 8-bit 4:2:0 |
| Colour tags | **none in VUI, no `colr`** | n/a | BT.709 on the session | BT.709 in the sequence header |
| Container | `tuile-mp4`: `ftyp, mdat, moov` | `ftyp, mdat, moov` | same | same |

The reasons for these choices are written in the code. Pictures come out in
input order because neither the muxer nor concat writes composition offsets
(`ctts`). Lookahead is off because the loop is "one picture in, one picture
out".

### 1.3 What that costs today (observed)

- **One bitrate for every size.** The first mobile export (720×1280, 30
  fps) weighed **273.8 MB** for about 3 minutes (`videos/README.md`,
  `stl-export-video-1-…`). That is 12 Mb/s on a resolution that needs 3–4
  Mb/s, so the file is about 3× too large to share. Going the other way, 4K
  films had to be overridden to `--mbps 45` by hand.
- **Not faststart.** `moov` comes last (README §`stl-export-video-1`), so a
  browser has to fetch the end of the file before it can play.
- **No B-frames and no lookahead.** At the same quality this is roughly
  15–30 % more bits than an encoder with both *(to measure)*.
- **8-bit everywhere, with a double quantisation.** The display output is
  `Rgba8Unorm` and then `i420.wgsl` rounds to limited-range 8-bit YUV, which
  leaves 220 luma codes. The second rounding has no dither. On the
  atmosphere's smooth gradients this is a direct source of banding, before the
  encoder adds its own (§3.4).
- **No colour description on the NVENC H.264 path.** Players have to guess
  BT.709. Most guess right for HD, but nothing guarantees it.
- **One render gives one file.** A second deliverable (another codec, another
  size, a lighter one to share) means a whole new cook, about 20 min of farm
  per `render.rs`.

---

## 2. "The very end": a mezzanine, then one final encode per profile

### 2.1 The design

```
render_segment cN  (L4)          Sink = MezzanineFilm
   wgpu float output ─► P010 (10-bit, dithered, BT.709 limited) ─► NVENC HEVC Main10, near-transparent
   → <run>/mezz/<render-id>/seg<N>.mp4            (one per segment, closed GOP, IDR first)

compress_film <profile>  (one L4, NEW)          tuile-film-native::compress
   for seg in segments (in order, streamed from the store):
       NVDEC decode (P010, device memory)
         └─(optional) scale, 10→8 dither for 8-bit profiles
       → ONE NVENC session for the whole film (lookahead, B-frames, temporal AQ, multipass)
   → tuile-mp4 muxer: faststart, colr, ctts
   → <run>/films/<profile>@<pv>.mp4

publish  (unchanged)   copies <run>/films/<profile>@<pv>.mp4 → stl-videos v/<id>/film.mp4
```

The main choices:

1. **The mezzanine is never joined.** The final encoder decodes segments one
   after another into a *single* encoder session. Every segment starts on an
   IDR and is decoded on its own. The stream entering the encoder is
   continuous, so rate control and lookahead run across segment boundaries.
   `concat` is then no longer on the critical path for mono films, and its
   `avc1`-only limit stops mattering.
2. **One render, several files.** Each profile is one `compress_film` run
   against the same mezzanine. A new profile, or a retuned one, never
   re-renders.
3. **Quality is consistent across the whole film.** Today each segment's VBR
   controller starts from scratch, with no knowledge of the segments around
   it. The final encoder sees the whole film as one stream.
4. **True size caps.** NVENC's `multiPass` is a per-frame two-pass (a
   quarter- or full-resolution first pass on the same frame), *not* a
   whole-file two-pass. A whole-film two-pass is cheap here because the final
   encode is fast: encode at constant quality, check the size, and re-encode
   at the VBR rate that meets the cap only if the cap was exceeded (§3.1).

### 2.2 Honest assessment: what the final stage buys and what it does not

- Constant-quality rate control (CQ/CRF) already gives *consistent quality*
  per segment without seeing the whole film. Each segment is about 700–830
  frames and the lookahead is ≤ 32 frames, so a whole-film encoder gains
  little at boundaries over a per-segment CQ encode. **Most of the quality
  gain from §1.3 (B-frames, lookahead, AQ, a bitrate per resolution, 10-bit)
  could also be had per segment.**
- What *only* the final stage gives: several deliverables from one render; a
  size cap over the whole film; any codec in the output (AV1 and HEVC
  without extending `concat`); finishing (faststart, colour boxes, B-frame
  `ctts`) in a single place; changing a profile without a new cook; and the
  mezzanine kept as a master/archive.
- What it costs: a GPU step at the end, mezzanine storage and transfer
  (§2.4), and a decoder to build (§4.1).

The first quality fixes (§7, step 0) should therefore not wait for the
final stage. They fall straight out of it, but they stand without it.

### 2.3 Mezzanine specification

| | Mono | Spatial |
|---|---|---|
| Codec | HEVC Main10, 4:2:0, `hvc1` | HEVC Main10, the two eyes **side by side, full width** (3840×1080 for 1080p eyes; level 5.1) |
| Rate control | Constant QP / CQ near-transparent: target QP ≈ 16–20 *(to tune: VMAF ≥ 98.5 and CAMBI ≤ 1 vs the float reference)*, with a ceiling: VBR max ≈ 2.5× the HQ profile | same |
| GOP | 1 s closed, IDR first; B-frames as references (`useBFramesAsRef = middle`), 3–4 B | same |
| Preset | P5 + `HIGH_QUALITY` (the render node's encoder is never the bottleneck, see the README timings) | same |
| Colour | BT.709 primaries/transfer/matrix, limited range, in VUI **and** `colr nclx` | same |
| Expected size *(to measure)* | 1080p30 ≈ 30–50 Mb/s; 1080p60 ≈ 45–70; 4K30 ≈ 100–150; 4K60 ≈ 150–220 | 1080p eyes, 30 fps ≈ 60–90 Mb/s |

Why not lossless, intra-only or ProRes:
- Lossless HEVC (`NV_ENC_TUNING_INFO_LOSSLESS`) at 4K is 1–2 Gb/s. That is
  ten times the storage and egress for a gain no delivery profile can show.
- Intra-only mezzanines decode faster but are 3–5× larger at equal quality.
  NVDEC is never the bottleneck here.
- ProRes is not encodable on the L4 (licence; §5.3). DNxHR and FFV1 have no
  Rust or NVENC path.

Why the spatial mezzanine is side by side and not MV-HEVC: NVDEC is not
documented to decode the second layer of MV-HEVC (*to verify on the RTX PRO
6000, driver 580*). A side-by-side Main10 stream is decoded whole by any
NVDEC, and the final encoder splits it back into two eyes before MV-HEVC
encoding. `tuile_mp4::spatial` already writes the eye metadata (baseline,
field of view), so it moves to the final stage unchanged.

### 2.4 Cost of the stage

**Storage.** A 3-minute 1080p30 film at about 40 Mb/s makes a mezzanine of
about **0.9 GB**, against about 0.27 GB for today's 12 Mb/s `render.mp4`. 4K60
at about 180 Mb/s is about **4 GB**. In R2 at 0.015 $/GB-month that is
0.014 $ (1080p30) and 0.06 $ (4K60) a month. Storage is not the issue.

**Transfer — the real marginal cost.** Segments are rendered in GCP
(europe-west1) and written to R2. That is *internet egress from GCP*
(≈ 0.08–0.12 $/GB, *to check against the current price sheet*), and the
mezzanine is 3–5× today's segment bytes. For 3 min at 1080p30, that is
≈ 0.9 GB × 0.1 ≈ **0.09 $ per film**, against ≈ 0.03 $ today. The final
encoder then reads it back. R2 egress is free and GCP ingress is free.

Two options for Laurent:
- **(A)** Put the mezzanine in **GCS, same region** as the L4 service
  (europe-west1). Same-region transfer is free. A lifecycle rule (e.g. 30 or
  90 days) bounds storage at 0.02 $/GB-month. Only the finals go to R2, as
  today. *Recommended.*
- **(B)** Keep everything in R2 (`stl-renders`) for simplicity, and accept
  ≈ 0.05–0.3 $ per film of egress depending on size.

**Compute.** The final encode is one L4 for the length of a decode plus an
encode. NVIDIA does not publish per-preset L4 throughput tables that I can
quote. Order of magnitude for Ada NVENC *(to measure)*: 1080p HEVC/AV1 at
P5–P7 with lookahead reaches a few hundred fps per engine; 4K reaches about
a quarter of that, and the UHQ tuning roughly halves the speed again. NVDEC
(4 engines on the L4) is faster than NVENC (2 engines). Under these
assumptions a 3-minute 1080p30 film (5 400 frames) takes **15–40 s** of
encode, plus ~10 s to fetch the mezzanine and a possible cold start. At
roughly 1 $/h for a Cloud Run L4 with its vCPUs (*to check*), that is
**≈ 0.01–0.02 $ per profile**. A 3-minute 4K60 film (10 800 frames at 4K)
takes about 2–4 min, ≈ 0.05 $.

**Render node.** It encodes HEVC Main10 instead of H.264 8-bit, on the
NVENC engine and on its own thread. The render is shading-bound (831 frames
1080p in 78–88 s per segment, README), so the expected impact is negligible
*(to measure: `Sink::spent`)*.

### 2.5 The reuse key: one render, compressions cached separately

Today a film is reused on `source_version` + option marks + the generation
mark (`stl-core/src/video_export/render.rs`, `FilmOptions::version_of`), and
one key means one file. With the final stage there are **two keys**:

| Level | Key | What it names | Invalidated by |
|---|---|---|---|
| Render (mezzanine) | `source_version` with today's marks (`+high-follow`, `+mascot`, `+atmosphere`, `+zoom-out`, `+g2`) **+ `format`** (viewport, fps, spatial) | the segments `<run>/mezz/<render-id>/seg<N>.mp4` | the picture changing (the generation mark), as today |
| Delivery | render key **+ `profile@pv`** (e.g. `web@1`) | `<run>/films/web@1.mp4` | a change to that profile's settings (bump `pv`); never the render |

Rules:
- **The compression is not a mark in `source_version`.** If it were, every
  new profile would cost a cook. It is a separate column, `videos.compression`
  (e.g. `web`, `high`), and the delivered file is looked up on
  (render key, compression).
- A request whose render key has a mezzanine but not that compression goes
  straight to `compress_film`. That is seconds of GPU instead of about
  20 min of cook.
- The profile version `pv` sits in tuile's profile table, and STL never
  edits it by hand. Raising it re-makes no delivered file. Old files keep
  their key and are still served, exactly as the generation mark works today.
- Credits are a decision for Laurent. Suggested: a compression of an
  existing mezzanine costs **0 or 1 credit**, and a new render costs what it
  costs today.

---

## 3. Delivery profiles

Business names are STL's. In tuile, profiles are generic typed specs
(`Profile { codec, bit_depth, rate, gop, size_cap, … }`), and STL's config
maps its names onto them. Memory note: no STL names in tuile.

### 3.1 The profiles

| UX name (FR) | Id | Codec / profile | Depth / chroma | Rate control | GOP | Container | For |
|---|---|---|---|---|---|---|---|
| **Partage** (default) | `web` | H.264 High, level by size (4.0 / 4.2 / 5.1 / 5.2) | 8-bit 4:2:0, **dithered from the 10-bit mezzanine** | CQ (NVENC VBR with `targetQuality`) + max-rate cap; if the film's size exceeds the profile cap → re-encode VBR at cap/duration | 2 s closed, 3 B (B as reference), lookahead 32, spatial + temporal AQ | mp4 `isom/avc1/mp41`, **faststart**, `colr nclx` 1/1/1 limited, `pasp` 1:1 | WhatsApp, Instagram, browsers, old Android: plays everywhere |
| **Haute qualité** | `high` | HEVC Main10, `hvc1` (L4.1 / 5.0 / 5.1, Main tier) | 10-bit 4:2:0 | CQ high + cap; UHQ tuning if speed allows; full-res multipass | 1 s, 4 B, lookahead + `lookaheadLevel`, temporal filter (needs `frameIntervalP ≥ 5`) | mp4 or mov `hvc1`, faststart, `colr` | iPhone/Mac (Photos, Final Cut), YouTube/Vimeo upload, a TV |
| **Montage / master** | `master` | the **mezzanine itself**, remuxed with faststart (HEVC Main10, 1 s GOP, high rate) | 10-bit 4:2:0 | — (no re-encode) | 1 s closed | mov/mp4 `hvc1` | editing (Resolve, Premiere, FCP decode HEVC 10-bit in hardware), archiving. ProRes only from a Mac (§5.3) |
| **Spatial** (format `spatial_hd`, not a separate compression) | `spatial-web` / `spatial-high` | MV-HEVC (two layers, Main), `hvc1` + eye metadata | 8-bit 4:2:0 today (MV-HEVC Main10 *to verify* on NVENC 13 and on Apple players) | VBR/CQ, multipass off as today | 1 s | mov/mp4 spatial (`tuile_mp4::spatial`) | Vision Pro in depth; iPhone plays it flat |

H.264 rather than HEVC as the sharing default: it is the only codec every
messaging app, social network and browser accepts without exception. HEVC
plays on iPhone and Safari, and in Chrome/Edge only with hardware support.
Firefox's HEVC support depends on the platform. A shared file can end up
anywhere.

### 3.2 Targets by resolution × fps

These are the **caps** (max-rate) and the expected **mean** for a CQ encode
of our content, in Mb/s *(to calibrate in §6, targets in §6.2)*:

| Profile | 720×1280 30 | 1080p30 | 1080p60 | 4K30 | 4K60 |
|---|---|---|---|---|---|
| `web` H.264 (mean / cap) | 3 / 5 | 6 / 10 | 9 / 14 | 25 / 40 | 38 / 60 |
| `high` HEVC Main10 | 6 / 10 | 14 / 24 | 20 / 32 | 45 / 70 | 65 / 100 |
| `master` (mezzanine) | ≈ 15 | ≈ 30–50 | ≈ 45–70 | ≈ 100–150 | ≈ 150–220 |
| spatial MV-HEVC (both eyes, 1080p eyes) | — | 20 web / 40 high | 30 / 60 | — | — |

For comparison, YouTube's SDR upload recommendations are 8 / 12 Mb/s for
1080p 30 / 60 and 35–45 / 53–68 Mb/s for 4K 30 / 60 in H.264. `high` sits
above them in HEVC, which is a deliberate safety margin for a file that the
platform will re-encode.

Expected file sizes (3-minute film, `web` mean): 720×1280 ≈ 68 MB
(against 274 MB today), 1080p30 ≈ 135 MB, 4K30 ≈ 560 MB. A sharing cap such
as "≤ 100 MB" or "≤ 16 MB/min" is a product decision (§8). A cap triggers
the second pass.

### 3.3 Colour

- The renderer is **SDR display-referred** (tone-mapped into `Rgba8Unorm`).
  Every profile is BT.709 / BT.709 / BT.709, limited range, written in two
  places: the bitstream's VUI (H.264/HEVC) or sequence header (AV1), **and**
  `colr nclx` in the mp4. NVENC H.264 writes neither today.
- **HDR (PQ/HLG) is out of scope.** It needs a scene-referred grade, an HDR
  tone-map in the renderer (`HDR_FORMAT` is `Rgba16Float` inside, so the path
  exists), and HLG/Dolby Vision 8.4 signalling for iPhone. That is a separate
  study. The 10-bit mezzanine is the right container for it later.

### 3.4 Banding (the atmosphere's gradients)

Banding comes from three places, and each one has its own fix:

1. **The renderer's quantisation.** `Rgba8Unorm`, then rounding to 8-bit
   limited-range YUV (220 luma levels) with no dither. **Fix:** convert to
   YUV in the shader from the *float* output (before any 8-bit rounding),
   with a dither of ±½–1 LSB (static blue noise, or triangular PDF), straight
   to **P010** for the mezzanine. To coordinate with the agent working on
   atmosphere banding in the renderer: it fixes the picture, and this fixes
   the conversion to the encoder. The work order is the same in both.
2. **10 → 8 bits for `web` (H.264).** NVENC only converts 8→10 bits
   (`nvEncodeAPI.h`, `inputBitDepth`/`outputBitDepth`: "Support for 8 bit
   input to 10 bit encode conversion only"). The 10→8 conversion with dither
   is therefore ours: in Rust on the CPU after an NVDEC → host copy (a 4K
   P010 frame is 24 MB; a SIMD pass with rayon takes a few ms), then an upload
   to NVENC. PCIe Gen4 is enough for ≥ 100 fps in 4K *(to measure)*.
3. **The encoder.** At low rates it flattens smooth areas and erases the
   dither. **Fix:** spatial AQ (lower QP on flat blocks) plus temporal AQ,
   a minimum quality (QP ceiling) on flat areas, and 10-bit codecs wherever the
   profile allows (`high`).

The banding measure is **CAMBI** (Netflix, part of libvmaf), not VMAF.
VMAF is not very sensitive to banding (§6).

---

## 4. The NVIDIA path

### 4.1 What the bindings provide

- The crate is `nvidia-video-codec-sdk`, lapoule-dev's fork at
  `647c790` (version `0.5.0-lapoule.1`), **MIT**. The encoder headers are
  **NVENC API 13.0** (nv-codec-headers `n13.0.19.0`) and need driver R570 or
  later. `nvEncodeAPI.h`, `cuviddec.h` and `nvcuvid.h` carry NVIDIA's **MIT
  notice** ("This copyright notice applies to this header file only"), so
  they are compatible with MIT OR Apache-2.0. The `.so` files
  (`libnvidia-encode`, `libnvcuvid`) come with the driver and are loaded at
  run time. They are not redistributed (Dockerfile:
  `libnvidia-encode-${NVIDIA_DRIVER}` from the host). CUDA is reached through
  `cudarc` 0.16 with dynamic loading.
- **Safe API: encoder only.** The decoder exists only as raw `sys` bindings,
  and those are SDK 12.1. **To build in the fork** (fork rule, memory note):
  a safe NVDEC wrapper. `cuvidCreateVideoParser` takes care of parsing the
  Annex-B stream itself, so no H.264/HEVC parser is needed on our side. It
  also needs `cuvidCreateDecoder` with `ulTargetWidth/Height` (a scaler built
  into the decoder, enough for 4K → 1080p; Lanczos on the CPU with
  `fast_image_resize` if the quality falls short) and mapping frames to
  CUDA device memory. A zero-copy decode → encode stays on the device
  (`nvEncRegisterResource` of a CUDA pointer) for profiles with no dither.
  The fork is also where `safe::EncoderInitParams` gains what is missing:
  10-bit input (`NV_ENC_BUFFER_FORMAT_YUV420_10BIT`), the
  `outputBitDepth`/`inputBitDepth` fields, and `lookaheadLevel`.

### 4.2 What the L4 (Ada) encodes

- **2 NVENC, 4 NVDEC.** It encodes H.264 (8-bit; H.264 10-bit is Blackwell
  only), **HEVC Main / Main10, 4:2:0 and 4:4:4**, and **AV1 Main 8/10-bit**.
  It does not encode 4:2:2 (Blackwell does) or MV-HEVC (Blackwell only, hence
  the RTX PRO 6000 for spatial).
- **Quality tools** (API 13):
  - Presets P1 to P7, with tunings `HIGH_QUALITY`, `ULTRA_HIGH_QUALITY`
    (HEVC and AV1) and `LOSSLESS`.
  - Lookahead (`enableLookahead`, `lookaheadDepth` ≤ 32) and
    `lookaheadLevel` 0–3 (HEVC/AV1).
  - Spatial AQ, plus temporal AQ (`enableTemporalAQ`, check
    `NV_ENC_CAPS_SUPPORT_TEMPORAL_AQ`).
  - `multiPass` quarter/full resolution.
  - B-frames as reference (`useBFramesAsRef` middle/each).
  - `tfLevel` temporal filter (HEVC/AV1, `frameIntervalP ≥ 5`).
  - CQ through `targetQuality` (VBR) or `CONSTQP`.
  - `NV_ENC_CAPS_NUM_ENCODER_ENGINES` and split encode, for 4K speed.

### 4.3 NVENC settings per profile (proposed starting points)

| | Mezzanine (render node) | `web` H.264 | `high` HEVC10 |
|---|---|---|---|
| Preset / tuning | P5 / HQ | P6 / HQ | P7 / UHQ (P6/HQ if UHQ is too slow at 4K60) |
| RC | CONSTQP ≈ 16–20 (or VBR `targetQuality`) | VBR `targetQuality` ≈ 23–26, `maxBitRate` = cap | VBR `targetQuality` ≈ 20–22, cap |
| multiPass | off | full-res | full-res |
| Lookahead | off (in-order output kept here, no state across segments) | 32 | 32, level 3 |
| AQ | spatial | spatial + temporal | spatial + temporal |
| B-frames | 3, B ref middle | 3, B ref middle | 4, B ref middle, `tfLevel` 4 |
| GOP | 1 s, IDR | 2 s, IDR | 1 s |

The `targetQuality` values are guesses to be calibrated (§6). Lookahead and
B-frames break the "one picture in, one out" loop in `nvenc.rs`. The final
encoder has to keep its input buffers until they are released (the
NV_ENC_ERR_NEED_MORE_INPUT pattern) and must write `ctts`.

### 4.4 Where it runs

- The **final encode for mono films runs on an L4**: the Cloud Run service
  `tuile-render` (same image, a new `POST /compress` route in
  `tuile-film-serve`), called by a new activity `compress_film` in the STL
  workflow after the last `render_segment`. Being a service and not a job
  gives no cold start once warm, and it reuses `renders.acquire()`. It must
  limit itself to one session per NVENC engine, so as not to steal the
  encoder from concurrent renders.
- The **final encode for spatial films runs on the RTX PRO 6000**
  (europe-west4), the only card that encodes MV-HEVC.
- **Not on the k8s CPU workers.** No pure-Rust HEVC or H.264 encoder exists
  (x264 and x265 are GPL, and openh264 is C). rav1e (BSD-2) would do AV1 on
  the CPU, but at speeds unworkable for minutes of 1080p.
- `assemble_film` (CPU) stays for the receipts, the frame count and the
  cadence (`check_cadence`). Those checks are applied to the final file
  instead of a join.

---

## 5. The Mac path

### 5.1 What exists

- The Mac is an **M2** (README). `H264Film` uses VTCompressionSession,
  H.264 High AutoLevel, `AverageBitRate`, `RealTime=false`, **no reordering**,
  and BT.709 tags. `SpatialFilm` uses AVAssetWriter with HEVC and MV-HEVC
  layer tags, written as mp4 or mov. Bindings are `objc2-video-toolbox`,
  `objc2-av-foundation` and `objc2-core-video` 0.3.
- The Mac renders locally for development and the README's films. It is not
  part of the production farm.

### 5.2 What VideoToolbox can do on M2

- **Encode:** H.264, HEVC Main / **Main10**
  (`kVTProfileLevel_HEVC_Main10_AutoLevel`, input
  `kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange`), **ProRes 422 /
  422 HQ / 4444** (the M2 has a ProRes engine), and MV-HEVC (macOS 14+).
  **No AV1 encode** on any Apple Silicon as far as I know. AV1 *decode* is
  hardware from M3 / A17 Pro.
- **Constant quality:** `kVTCompressionPropertyKey_Quality` (0–1) with no
  `AverageBitRate`, on Apple Silicon. Caps come from `DataRateLimits`. Also
  available: `MaxAllowedFrameQP`, `PrioritizeEncodingSpeedOverQuality = false`,
  and `AllowFrameReordering = true` (B-frames, which need `ctts` in the muxer
  as on NVENC). Multipass (`MultiPassStorage`) exists for H.264, and *needs
  verification for HEVC on M2*.
- **Decode:** VTDecompressionSession, hardware HEVC Main10, for a final
  encode run on the Mac.

### 5.3 Mac role and the ProRes licence

- **The same design runs locally.** The render produces the mezzanine with
  `MezzanineFilm` on VT HEVC Main10 (P010 from the same dithering shader).
  `compress` then runs on VT decode → VT encode. The profile code is shared,
  and only the decode and encode backends change. This is how a Mac render
  gets checked (memory: native first) without the farm.
- **ProRes:** Apple licenses ProRes encoding to third parties, and the
  reverse-engineered encoders (FFmpeg's `prores_ks`) are not licensed. On
  macOS, encoding through VideoToolbox is Apple's own encoder, used under the
  system licence, so there is no problem. **Off Apple platforms (L4) there is
  no licensed ProRes encoder within our reach.** So **no ProRes in
  production.** The `master` profile is HEVC Main10, which every editing
  suite decodes in hardware. A `master-prores` profile can exist on the Mac
  only (mov; ProRes in mp4 is not standard), for a local render or a one-off
  job. Any shipped licence question should be put to Apple before ProRes is
  offered as a product.

---

## 6. Measurement plan

### 6.1 Excerpts (2–3, 10–20 s each)

| # | Excerpt | Why | Source |
|---|---|---|---|
| E1 | **Zoom-out over the sea/sky**: descent from space and climb back (option `zoom_out`), atmosphere on, 1080p30 and 4K30 | smooth gradients → banding (CAMBI), large flat areas | an export of track 1557732 or 1554285 with `+zoom-out`, ~15 s of the opening |
| E2 | **Fast mountain flight**: Pyrenees loop frames 13651–14100, 4K30, sunset look (README: `pyrenees-cloudrun-l4-…`) | detail + motion → VMAF, bitrate pressure | packs of `_tests/tuile-film/packs/3dcfbde7a3c3a344.references` |
| E3 | **HUD / overlays / text**: run 577d frames 4400–4520, 1080p, badge, altitude profile, trail (README: `…-wgpu-overlays-hud-badge-icon`) | thin text and saturated colours → 4:2:0 chroma, ringing (SSIM/PSNR on crops of the HUD) | run `1557732/20260924T221734Z-577d` |

Plus 720×1280 (the mobile format) on E1, scored with VMAF's phone model.

### 6.2 Reference and metrics

- **Reference** = the renderer's float frames, written lossless by a
  reference `Observer` (memory: instruments are Observers). The format is
  16-bit RGB PNG or a 10-bit/16-bit Y4M, so that 10-bit profiles are not
  judged against an 8-bit reference. The existing 8-bit `Pictures` sink is
  kept as a second reference, to isolate banding from the renderer's
  quantisation.
- **Metrics:**
  - VMAF (`vmaf_v0.6.1` for HD, `vmaf_4k_v0.6.1` for 4K, `phone` for
    mobile): mean, **1st percentile**, and harmonic mean.
  - **CAMBI** (banding, scale 0–24; ≈ < 5 hardly visible).
  - PSNR-Y / XPSNR and SSIM / MS-SSIM.
  - Size and actual bitrate.
  - Encode time per minute of film, as wall-clock fps on the node.
- **Tools:** in the lab (allowed for evaluation only, per the licence rule),
  `ffmpeg -lavfi libvmaf` (VMAF + CAMBI) to decode and score, in a notebook
  with the typed Python lib (memory: image-lab method; notebook provisioned,
  Laurent runs it). x264/x265 serve as *quality references* in the lab
  only, and never in the product. In the **shipped path**, a Rust quality
  gate on samples uses **`av-metrics`** (rust-av, MIT: PSNR, SSIM, MS-SSIM,
  CIEDE2000). Its input frames come from our own NVDEC/VT decoder. `dssim` is
  AGPL, so excluded. VMAF in Rust is not mature (libvmaf is C, BSD-2-Clause-
  Patent): a `-sys` binding is possible later if a VMAF gate turns out to be
  needed in production.

### 6.3 Proposed targets

| Profile | VMAF mean | VMAF P1 | CAMBI max | Other |
|---|---|---|---|---|
| mezzanine | ≥ 98.5 | ≥ 96 | ≤ 1 | PSNR-Y ≥ 48 dB |
| `high` | ≥ 96 | ≥ 92 | ≤ 2 | |
| `web` | ≥ 93 | ≥ 85 | ≤ 4 | SSIM on the E3 HUD crop ≥ 0.97 |
| spatial | ≥ 93 per eye | ≥ 85 | ≤ 4 | |

These targets are measured **mezzanine → profile** *and* **reference →
profile**. The second is the one that counts, and the gap between the two is
what the mezzanine costs.

### 6.4 Runs

1. **Rate-quality curves.** For each excerpt × codec (H.264, HEVC10)
   × preset (P4, P6, P7 / UHQ), sweep `targetQuality` / QP (6–8 points) and
   plot VMAF and CAMBI against the bitrate. Those curves fix §3.2 and §4.3.
2. **A/B on the current state.** Today's H.264 P4 VBR 12 Mb/s with no B and
   no lookahead, against `web` at the same size: Δ VMAF, Δ CAMBI.
3. **Dither on/off**, 8-bit vs 10-bit, on E1 (CAMBI). Coordinated with the
   atmosphere fix.
4. **Throughput:**
   - L4: NVDEC → NVENC fps for each profile at 1080p30 / 4K60, one session
     vs two (2 engines), with the CPU 10→8 copy vs zero copy.
   - Render node: the extra `Sink::spent` cost of HEVC10 against H.264.
   - Mac M2: VT HEVC10 / H.264 / ProRes, in fps.
5. **Segment boundaries.** Per-frame VMAF around the joins, comparing a
   final encode over the whole film with today's join. This checks the claim
   of §2.2.

Execution: on the farm, a **detached Cloud Run L4 job** that stops itself
(memory: GPU jobs stop themselves). The Mac runs only short VT tests at low
resolution: a fragile Mac, no heavy local encode. Every film produced goes
into `videos/` with its parameters, and gets opened.

---

## 7. Recommendation

### 7.1 Pipeline

**Mezzanine + final encode**, as in §2.1. The new path replaces the old one
once the tests prove it, with no switch (memory: no switches):

- every render node writes an HEVC Main10 mezzanine dithered from float;
- every delivered film comes from `compress_film`;
- `assemble_film` checks the final file.

### 7.2 Defaults

- **Compression `web` (Partage), H.264 High 8-bit, faststart, BT.709
  tagged, CQ + cap per resolution.** It is the default because a shared film
  has to play everywhere.
- Spatial: the `spatial_hd` format only, with `spatial-web` by default.

### 7.3 UX and API

- **The panel** (collapsible "Options de rendu", with the other agent's
  duration, fps, spatial and detail):

  **Compression :** `Partage — léger, lisible partout` (default) ·
  `Haute qualité — iPhone, Mac, YouTube` · `Master — pour le montage`.

  The size estimate is shown next to each choice (from §3.2, through
  `GET /v1/videos/estimate`).
- **The API:** `POST /v1/videos` gains
  `"compression": "web" | "high" | "master"` (optional, default `"web"`;
  refused when it is not in the config list for that format).
  `GET /v1/videos/config` lists, for each format, the compressions allowed
  (spatial: `web`, `high`) with their name, description and estimate.
  `VideoView` returns `compression`. A row gets `videos.compression`.
- **Reuse** (§2.5): look up (render key, compression) first, then (render
  key) for a mezzanine. Both are found → served. Mezzanine only → compress
  (seconds). Neither → full cook, then compress.

### 7.4 What to build first

- **0. Quick wins, independent of the final stage.** Each item is cheap
  and proven by a test that fails first:
  - (a) A bitrate per resolution instead of a flat 12 Mb/s, in `RenderSpec`
    or STL's `look_of`.
  - (b) BT.709 VUI tags on NVENC H.264, plus `colr nclx` in `tuile-mp4`.
  - (c) **Faststart** in `tuile-mp4` and `concat`: compute `moov` first and
    shift `stco` / `co64`.
  - (d) A dithered I420 shader from the float output. Coordinate with the
    atmosphere work.

  This is enough to bring the mobile film from 274 MB to about 70–90 MB and
  to remove part of the banding.
- **1. Lab measurements** (§6.4, runs 1–2) on the current encoder and on
  candidate settings. They fix the numbers before any code goes into the
  product.
- **2. B-frames and `ctts`** in `tuile-mp4` and in `concat` (which already
  copies composition offsets), plus lookahead in `NvencFilm`.
- **3. Mezzanine:**
  - the P010 shader;
  - a mono HEVC Main10 NVENC sink;
  - a single-layer `hvc1` muxer (reusing `tuile_mp4::spatial`);
  - segments under `<run>/mezz/…`.
- **4. NVDEC in the fork** (safe decoder), then `tuile-film-native::compress`
  (profiles, decode → [scale, dither] → encode, one session) and
  `tuile-film-serve POST /compress`.
- **5. STL:**
  - the `compress_film` activity;
  - `videos.compression`;
  - the API field and config;
  - two-level reuse;
  - the panel.
- **6.** The spatial path (side-by-side mezzanine → MV-HEVC on the RTX PRO
  6000), then the Mac backend (VT decode/encode, local `master-prores`).

### 7.5 Expected costs per film (3 min)

| | 1080p30 | 4K60 |
|---|---|---|
| Mezzanine storage (GCS, 30 days) | ≈ 0.02 $ | ≈ 0.08 $ |
| Mezzanine egress (only with option B, R2) | ≈ 0.09 $ | ≈ 0.4 $ |
| Final encode per profile (L4) | ≈ 0.01–0.02 $ | ≈ 0.05 $ |
| Render node encode (HEVC10 instead of H.264) | ≈ 0 | ≈ 0 |
| A profile added later, without a re-cook | ≈ 0.02 $ instead of a ≈ 20 min cook | |

All *(to confirm against the current GCP price sheet and the measured fps)*.

---

## 8. Decided (Laurent, 20 Oct. 2026)

1. **Where the stage sits:** between the join of the film and its
   publication — render segments → join → **compress** (one final encode
   per chosen profile) → publish.
2. **Credits:** untouched. No price for a compression, no billing change.
3. **Profiles kept:** sharing (H.264, `web`), high quality (HEVC Main10,
   `high`), and spatial (MV-HEVC, as today). **The AV1 sharing profile is
   dropped**; `master` is not offered for now.
4. **Defaults by format:** the vertical (phone) format shares by default
   (`web`); HD and 4K default to `high`, which they must offer; spatial is
   MV-HEVC.
5. **The choice is the user's**, in the render options panel; spatial has
   only MV-HEVC.

Taken from this study's recommendations where nothing was said:

- **Mezzanine store: GCS, same region as the L4 service, deleted after 30
  days** (option A of §2.4).
- **Sharing cap: 100 MB**, with a second encode at the mean rate that fits
  when the first passes it.
- **No ProRes, no `master` in the product.**

## 9. What is built (branch `feat/film-compression`)

- **`tuile_mp4::film`**: one video track of H.264, HEVC (one layer or two)
  or AV1, read and written with `mp4-atom`. Every film it writes — the
  muxers' (`Muxer::finish`), the spatial muxer's, the join's — is
  **faststart** (`ftyp, moov, mdat`) with **`colr nclx` 1/1/1 limited** and
  `pasp` 1:1. B-frames: composition offsets kept non-negative behind an edit
  list. `tuile_farm::concat` is `film::join` for every codec (the spatial
  join folded into it); a join may let each segment go once copied, so a
  film and its segments are never held whole twice where the disk is
  memory.
- **`tuile_farm::compress`**: the generic profiles (`h264`, `hevc-main10`,
  `mezzanine`) with their version in their files' key
  (`<run>/films/<name>-v<n>.mp4`), the **rate tables by pixel rate** of
  §3.2 in place of 12 Mb/s for every size, the size cap; the order and the
  answer of `POST /compress`; the mezzanine store (`TUILE_MEZZANINE_STORE`,
  `gs://<bucket>` by the machine's own identity); `assemble::gather`.
- **The render node** (`nvenc.rs`): one NVENC loop for every single-view
  film, with a pool of buffers so that **B-frames and lookahead** hold
  pictures; outputs read back in decoding order with the frame each is
  shown as; **BT.709 in the VUI** (H.264, HEVC) and in AV1's colour config.
  The `mezzanine` codec is HEVC Main10 by NVENC's own 8 → 10-bit conversion
  of today's 8-bit I420 (the float → P010 dithering shader of §3.4 (1) is
  still to do, with the atmosphere work). H.264 and AV1 written by a render
  node take the H.264 profile's rate by the picture's size unless one is
  asked for.
- **The final stage** (`compress.rs`): the joined film decoded by **NVDEC**
  (a safe decoder added to lapoule-dev's `nvidia-video-codec-sdk` fork,
  branch `feat/decoder`), **10 → 8 bits dithered** for H.264 (a fixed
  interleaved-gradient pattern: the mean of a smooth gradient kept, no
  temporal noise), one NVENC session for the whole film, a second pass at
  the capped rate when the cap is passed. `tuile-film-serve` gains
  **`POST /compress`** (receipts from the run store, segments from the
  mezzanine store, joined, delivered, checked frame for frame and for
  cadence) and the image gains **`tuile-film-compress`** (the same by hand,
  and `--probe`: what a player finds in a film).
- **Spatial** is unchanged: MV-HEVC segments, joined (now faststart and
  colour-tagged by the join), delivered as they are (`copy`).

Measured on a Cloud Run L4: see §9.1.


### 9.1 Measured end to end (Cloud Run L4, driver 580.126.20, 20 Oct. 2026)

One job (`tuile-compress-trial`, image `tuile/film:7a86d06`) ran the service
path a film takes: `tuile-film-serve` with `TUILE_MEZZANINE_STORE` on a GCS
bucket in europe-west1; every pack of a production run asked of
`POST /segments` as the `mezzanine` codec into that bucket; the receipts
deposited in the run store; then `POST /compress` once per delivery
(gather from GCS → join → NVDEC → dither → one NVENC session → faststart
mp4 → run store). One L4, segments one after another. The renders carry no
STL overlays (tuile's own service) and one sun for the whole film
(`sun: [200, 35]`); "today" is the production `render.mp4` of the same run
(STL's service, its own sun, its HUD).

| Film | File | Size | Mean rate | Encode |
|---|---|---|---|---|
| mobile 720×1280 30 fps, 4266 frames (142 s), run `1572170/…-d539` | today `render.mp4` (H.264, no B, index last, no colour) | 191.7 MB | 10.8 Mb/s | — |
| | mezzanine (7 segments, HEVC Main10) | 105.8 MB | 6.0 Mb/s | rendered in 314 s |
| | **`web`** `h264-v1` (default) | **39.2 MB** | 2.2 Mb/s | 16.5 s, 1 pass |
| | `high` `hevc-main10-v1` | 53.0 MB | 3.0 Mb/s | 15.7 s, 1 pass |
| HD 1920×1080 30 fps, 5400 frames (180 s), run `1570484/…-620f` | today `render.mp4` (H.264, no B, index last, no colour) | 278.1 MB | 12.4 Mb/s | — |
| | mezzanine (13 segments) | 536.4 MB | 23.8 Mb/s | rendered in 536 s |
| | **`high`** `hevc-main10-v1` (default) | **276.4 MB** | 12.3 Mb/s | 45.1 s, 1 pass |
| | `web` `h264-v1` | 98.2 MB | 4.4 Mb/s | 91.0 s, **2 passes**: 191.3 MB at constant quality, over the 100 MB cap, again at 4.36 Mb/s |

Every delivered file probes as `ftyp, moov, mdat` (faststart), `colr nclx
1/1/1` limited, B-frames reordered, a key frame every 2 s (`web`) or 1 s
(`high`), every frame of the joined mezzanine at its cadence. The final
stage runs at 260 fps (mobile) and 120 fps (HD `high`) on one NVENC
session, fetch from GCS included in seconds; a compression asked again of
the same mezzanine costs that, not a render.

What this does not measure: VMAF/CAMBI (§6 is still to run); the
float → P010 dither at the render node (the mezzanine is NVENC's own 8 → 10
bits of today's I420); the spatial path (unchanged, not run); 4K.
