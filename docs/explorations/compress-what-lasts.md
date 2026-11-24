<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Display at once, compress what lasts (CB9)

Status: **implemented, 2026-11-24**, branch `feat/budget-cb9-compress-what-lasts`, stacked
on #289 (the progressive score). Asked by Laurent on 2026-11-23, after CB8 measured that
encoding every imagery tile before drawing it made "every upload landed" 29 s in BC7
against 18.5 s Off on `p2-slow` (`gpu-compressed-textures.md` §9.1b).

## The rule

- A tile with a **low score**, or one that must be shown **now**, is uploaded and drawn as
  RGBA8 — not compressed.
- A tile with a **high score** that is **expected to stay resident a long time** is
  compressed.

So compression is a memory upgrade the budget decides, never on the display's critical
path.

## Prior art (inspiration only)

This is what a compressed-memory pool does for an operating system — Linux's zswap and
zram, the macOS memory compressor: a page just touched stays uncompressed; a page that has
lasted is compressed in the background; under memory pressure, compressing is the step
**before** eviction, as zswap stands before swap-out. One difference, and it is in the
GPU's favour: **a GPU samples BC7 and ASTC directly**, so a compressed texture costs
nothing to read, where a compressed page must be decompressed before it is used. Nothing
here is taken from those implementations beyond the idea.

## The laws, in plain terms

1. **Upload first.** A decoded imagery tile goes to the GPU as RGBA8 at once and is drawn.
   No upload ever waits for an encode. Until it is compressed it is charged as RGBA8.
2. **A tile survives its decisions with a very high score** (Laurent, 2026-11-24). The
   budget asks for a GPU copy to be compressed once its probability of use — the
   progressive score's `P` (#289) — has stayed at least `compress_p` (90 % of certainty)
   over `compress_decisions` decisions (10): each decision the pass samples it in the
   high-score set counts one, a dip starts the count again. **Camera poses, not time**
   (Laurent, 2026-11-24: the decider's timers count poses, not seconds): a decision is a
   pose, one camera handed to the decider. The count follows how fast the view changes — a
   fast zoom replaces its copies before they survive, a still view keeps them — and in a
   film a decision is a frame. **At rest** a copy whose score is that high is also
   compressed once the need has been unchanged for `compress_still_poses` consecutive poses
   (60: the second it was in time, at the viewers' 60 steps a second — a viewer sends its
   camera every frame and the geometry server steps on each, so a still view keeps
   stepping). The still need is a need that changed nothing since the step before (an
   empty delta); a pose at which it changed starts the count again. Nothing reads a clock
   inside `decide`.
3. **Highest value first.** Among the eligible copies the budget asks first for those the
   release order keeps longest (the eviction key, highest first: the floor, then the
   score `P × R / bytes` over GreedyDual's `L`). A copy due for eviction soonest is never
   encoded before it.
4. **Compression before eviction.** When a class is over its low watermark — where
   evictions start — both waits are waived (never the score): everything eligible by score is compressed at
   once. And the bytes the compressions in flight will give back count as given back:
   an eviction sweep evicts only what compression cannot free (the cache the view left
   stays).
5. **Charged as held, new before old.** From the decision until the consumer says it is
   done, a copy is charged both its RGBA8 and its compressed bytes (both exist on the
   GPU); then what the consumer reports. On the GPU, the compressed texture is made,
   then bound to every drape that names it, then the RGBA8 texture is let go: never a
   frame without a texture, at most one frame with both.
6. **Spare GPU time only.** The compressions in flight are capped by `compress_flight_tiles`
   (16 tiles of 256² with their chains, scaled with the thermal state as the encode lane
   is) and never push a class over its hard limit. The cap is the transient's bound and
   the consumer's queue: it must hold a few frames of the consumer's encode lane, or the
   round trip idles the lane — the first measurement capped it at the per-step upload
   encode lane (three tiles), one multi-tile drape at a time, and compressed 1.5 tiles a
   second where the lane allowed twenty. The consumer runs them on its encode lane (CB5/CB6), in the frame's
   measured headroom (#311), **after** the frame's uploads.
7. **No switch, Off unchanged.** The thresholds are specification numbers
   (`compress_p`, `compress_decisions`, `compress_still_poses`, `compress_visits`, `compress_flight_tiles`: re-specifiable at run time by
   `tuile://budget?…`, recorded with the step, in the digest). A cost that cannot be
   compressed — every cost under `Off`, the film, the bake — makes nothing here run: the
   decisions, their digests and the goldens are the ones they were.

## Policy and mechanism

- **Policy** (`tuile-budget/src/compress.rs`): `track` (the decision the score has stayed high from), `eligible(spec, survival, pressed)` and
  `ranks_before` — pure functions, integers only.
- **Mechanism** (`tuile-budget/src/compress_pass.rs`): visits up to `compress_visits`
  (64) waiting copies a step, round robin; checks what the policy proposes against the
  ledger (still on the GPU, same generation, not in transit, the lane, the hard limit);
  decides (`Decisions::compress`); applies the acknowledgement
  (`Budget::on_compressed`). Not compressible after all (the consumer could not) is held
  as it is and never asked again.
- **Executor** (`tuile-wgpu/src/in_place.rs`): `ServerMessage::Compress` queues; each
  frame, after the uploads, a tile at a time while the lane has blocks: texcomp 0.4's
  `encode_into` reads the resident RGBA8 texture (`Source::Texture`, no readback, no
  re-upload) into a new BC7 / ASTC 4×4 texture; the pool and every drape naming the tile
  take the new one, their wearers are dressed again (the drawn ones at this frame's
  resolve, in the same submit as the pass that fills the texture); a drape whose tiles
  are all done is acknowledged (`ClientMessage::Compressed`) once the GPU has finished the
  frame.

## Accounting

| | charged |
|---|---|
| delivered, uploaded | RGBA8 (`Cost::gpu`); `Cost::compressed` says what it would hold compressed, `Cost::compress_blocks` the work |
| compression decided, not acknowledged | RGBA8 + compressed (the transient), reported as `CompressStats::saving` per class |
| acknowledged | what the consumer reports (the compressed share; RGBA8 for a tile the encoder refused) |
| evicted while compressing | both, freed together |

A tile shared by two drapes is compressed once, for both; the second drape is charged its
RGBA8 share until it is itself acknowledged (an over-estimate, never an under-one), and
its own compression then encodes nothing.

## Where it is not yet

- A mesh's own textures (3D Tiles content, parked) are uploaded RGBA8 and never compressed
  in place: before CB9 they were encoded at upload.
- The CB2–CB6 fields of `Cost` for an encode on the way (`transfer`, `transient`,
  `encode_blocks`) are still read by the budget, and no producer sets them any more: their
  removal is a follow-up once Laurent agrees.
- The expected residency is read off the decisions survived, sampled at the pass's visits (64 a step, round robin): a dip between two visits goes unseen. A next-use estimate from the views
  ahead (step C of the roadmap) or the films' known schedule would replace it there.
