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
2. **Compress what lasts.** The budget asks for a GPU copy to be compressed when both:
   - its probability of use — the progressive score's `P` (#289) — is at least
     `compress_p` (25 % of certainty by default: a neighbour one radius away, two levels
     above the need, the graded coarse levels; a copy the view left keeps a 64th of its
     `P` and falls under it);
   - it has been drawable for at least `compress_after_ms` (2 s by default) — its expected
     residency, read off its age: a copy that has lasted is likely to last (the
     operating systems' "inactive" pages). A fresh copy during a zoom is likely replaced
     before it would pay back its encode.
3. **Highest value first.** Among the eligible copies the budget asks first for those the
   release order keeps longest (the eviction key, highest first: the floor, then the
   score `P × R / bytes` over GreedyDual's `L`). A copy due for eviction soonest is never
   encoded before it.
4. **Compression before eviction.** When a class is over its low watermark — where
   evictions start — the age is waived: everything eligible by score is compressed at
   once. And the bytes the compressions in flight will give back count as given back:
   an eviction sweep evicts only what compression cannot free (the cache the view left
   stays).
5. **Charged as held, new before old.** From the decision until the consumer says it is
   done, a copy is charged both its RGBA8 and its compressed bytes (both exist on the
   GPU); then what the consumer reports. On the GPU, the compressed texture is made,
   then bound to every drape that names it, then the RGBA8 texture is let go: never a
   frame without a texture, at most one frame with both.
6. **Spare GPU time only.** The compressions in flight are capped by the step's encode
   lane (`BudgetConfig::encode_blocks_per_step`, thermal-scaled) and never push a class
   over its hard limit. The consumer runs them on its encode lane (CB5/CB6), in the frame's
   measured headroom (#311), **after** the frame's uploads.
7. **No switch, Off unchanged.** The thresholds are specification numbers
   (`compress_p`, `compress_after_ms`, `compress_visits`: re-specifiable at run time by
   `tuile://budget?…`, recorded with the step, in the digest). A cost that cannot be
   compressed — every cost under `Off`, the film, the bake — makes nothing here run: the
   decisions, their digests and the goldens are the ones they were.

## Policy and mechanism

- **Policy** (`tuile-budget/src/compress.rs`): `eligible(spec, P, age, pressed)` and
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
- The expected residency is the copy's age (Lindy). A next-use estimate from the views
  ahead (step C of the roadmap) or the films' known schedule would replace it there.
