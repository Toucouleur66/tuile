<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# The geometry loop: its instruments, and its baseline

Step 0 of the plan to speed up `GeometryServer::run`. Nothing here changes
what the loop does. It adds the instruments that the later steps will be
judged by, and it records what they read today on one machine. This page is
the reference every later step compares against.

## What the loop is today

Every `Event::Load(Ok)` runs `on_load_done` and then a full `retraverse`. The
loads sit in one `FuturesUnordered` that only the loop polls. While a pass
runs, no load moves forward: not a body read, not a decode hand-off, not a
store lookup.

## The instruments

All of them live in `tuile_core::metrics::metrics()`. All are wasm-safe:
`stamp()` is still a no-op there, so on wasm the timers read zero and the
counts stay right.

| metric | kind | what it measures |
|---|---|---|
| `traversal_seconds` | Timer | `traverse()` alone. It used to include the determinism trace and the `rendered_last` rebuild. |
| `select_seconds` | Timer | building and sending the `Select` at the end of each pass |
| `arrivals_per_pass` | Tally (sum, count, peak) | load completions handled by each pass that an arrival provoked. It is 1/1 today by construction. Camera passes are not counted. |
| `arrival_wait_seconds` | Timer | time from the **wake** that lets a load finish to the loop handling its result |
| `load_wake_wait_seconds` | Timer | each wake of a load, up to the poll that answers it. One sample per wake. |

Why the waits are measured from the wake: a load future runs only when the
loop polls it. A stamp taken when the future resolves therefore reads the
moment the loop got to it, which is always about zero. The first version of
this step did exactly that, and its bake read 0.002 ms. The load future's
waker is now wrapped (`runtime::Woken`). Every wake is stamped, then passed
straight on. The same waker is handed over on every poll, so `will_wake`
keeps answering yes and the polling pattern does not change.

`tuile-metrics` now publishes every `Timer` as `tuile_<name>_seconds_count`
(a counter), `_seconds_sum` and `_seconds_peak` (gauges, in seconds). The
`Tally` goes out as `tuile_arrivals_per_pass_count`, `_sum` and `_peak`. The
bake serves them when `TUILE_METRICS_ADDR` is set, and only then. Its
`MEMORY` line also carries `traversal_ms`, `select_ms`, `arrivals_per_pass`,
`arrival_wait_ms` and `load_wake_wait_ms`, each as mean/peak.

Tests:

- `crates/tuile-core/tests/loop_instruments.rs` is a whole session in its own
  test binary, so the process-wide counts can be compared for equality. It
  checks:
  - every arrival is waited on once;
  - each arrival buys its own pass (`arrivals_per_pass` = 1/1);
  - `traversals` = arrivals + 1;
  - every pass is timed and sends a `Select`.
- `runtime::tests::a_woken_load_carries_its_wake_not_its_poll` checks that a
  woken load carries its wake's stamp, 30 ms old, and not the stamp of the
  poll.
- `tuile-metrics` checks that the series exist, with the peak in seconds.

Each metric line was reverted in turn, and its test failed each time.

## Machine and commit

- Apple M2, 16 GiB, macOS 27.0.1, Metal. Branch `feat/loop-instruments`,
  measured on top of `a02566e` (main, 0.8.1). It was then rebased onto
  0.8.2, which is a version bump only.
- **The machine was shared.** A second worktree (`tuile-loop-core`) was
  running debug test suites and builds during several of these runs. Expect
  noise of tens of percent between runs. Repeat a run before comparing
  against it.
- The viewer's tile store (`~/Library/Caches/tuile/tiles-aerial`) and the
  shared HTTP cache had been warm since before these runs. Cache hits were
  66–98 %, so none of these numbers are cold.

## The tapes

The camera tapes come from `tuile-tape`, which is deterministic. They were
used as is: the viewer replays them frame by frame (`TUILE_REPLAY`), and the
bake takes the same files.

```bash
cargo build --release -p tuile-wgpu-viewer -p tuile-tape -p tuile-bake
target/release/tuile-tape zoom  dive.mcap 240                          # 692 frames: 200 m → 35 786 km and back, +5.2 %/frame, over (2.17, 42.52)
target/release/tuile-tape orbit orbit.mcap 600 2.17 42.52 8000 5000    # 600 frames: a full circle, r = 8 km, h = 5 km, same ground
```

## Native viewer

The viewer replays the tape, then quits by itself once the tape has run out.
A loop scrapes `127.0.0.1:9464/metrics` every 0.5 s and keeps the last
answer. That scrape lands up to about a second before the process exits, so
the totals are short by the last few dozen passes. `try-viewer.sh` reads the
token from `.env` and never prints it.

```bash
# run-viewer.sh <worktree> <tape> <out>
( while true; do curl -s -m 1 127.0.0.1:9464/metrics > "$out.tmp" && [ -s "$out.tmp" ] && mv "$out.tmp" "$out.prom"; sleep 0.5; done ) &
TUILE_REPLAY="$tape" "$root/scripts/try-viewer.sh" mac > "$out.log" 2>&1
```

| run | frames (fps) | traversals | traversals / frame | traversal mean / peak ms | select mean / peak ms | arrivals / pass mean / peak | arrival wait mean / peak ms | wake wait mean / peak ms (wakes) | loads done / cancelled | frame mean / peak ms |
|---|---|---|---|---|---|---|---|---|---|---|
| dive #1 | 693 (51) | 3273 | 4.79 | 0.57 / 51.2 | 0.015 / 26.8 | 1.00 / 1 | 7.8 / 179 | 5.6 / 219 (10 668) | 2636 / 563 | 19.6 / 1167 |
| dive #2 | 693 (58) | 3368 | 5.12 | 0.51 / 28.4 | 0.007 / 2.4 | 1.00 / 1 | 8.4 / 244 | 4.4 / 244 (11 090) | 2803 / 506 | 17.4 / 1127 |
| orbit #1 | 601 (26) | 1801 | 3.00 | 1.82 / 84.0 | 0.003 / 0.1 | 1.00 / 1 | 11.1 / 203 | 4.8 / 204 (19 707) | 1320 / 1230 | 38.7 / 12 983 |
| orbit #2 | 601 (24) | 1508 | 2.54 | 1.54 / 182 | 0.003 / 1.0 | 1.00 / 1 | 4.5 / 193 | 1.7 / 194 (17 294) | 983 / 881 | 41.1 / 14 453 |

Reading:

- Each arrival costs a full pass: 2.5–5 traversals per displayed frame. A
  single walk is short (0.5–2 ms), but tiles that have arrived still wait
  5–11 ms on average, and close to 200 ms at worst, for the loop to get to
  them.
- The orbit's frame peak (13–14 s) is its first frame, waiting for the start
  view to be covered. It says nothing about the loop.
- The viewer's numbers swing a lot from run to run: uploads went 684 → 1475
  → 1958 → 2121 over four dives of the same tape. Compare against both runs,
  and on traversals per frame and the waits rather than on totals.

## Local bake

The bake is the orbit tape's first 96 frames at 1280 × 960, with the ion
sources and no tile store. `TUILE_CACHE_DIR` was a fresh directory, but the
shared native HTTP cache was warm. The token is read from `.env` into the
environment and never printed:

```bash
TUILE_ION_TOKEN=… TUILE_METRICS_ADDR=127.0.0.1:9465 TUILE_MEMORY_EVERY=5 TUILE_CACHE_DIR=<dir> \
  target/release/tuile-bake --tape orbit.mcap --frames 1:96 --out out.tuilepack --viewport 1280x960
```

| run | build | bake s | s / frame | traversals | traversals / frame | traversal mean / peak ms (sum) | arrivals / pass | arrival wait mean / peak ms | wake wait mean / peak ms (wakes) | loads done |
|---|---|---|---|---|---|---|---|---|---|---|
| #1 | this branch | 69.9 | 0.73 | 4687 | 48.8 | 4.68 / 99 (21.9 s) | 1.00 / 1 | 31.1 / 244 | 24.7 / 244 (26 034) | 4591 |
| #2 | this branch | 72.0 | 0.75 | 4683 | 48.8 | 4.81 / 83 (22.5 s) | 1.00 / 1 | 30.6 / 250 | 20.5 / 250 (26 844) | 4588 |
| main #1 | `a02566e` | 81.6 | 0.85 | 4691 | 48.9 | — | — | — | — | — |
| main #2 | `a02566e` | 106.3 | 1.11 | 4687 | 48.8 | — | — | — | — | — |
| first run | this branch, before the wake stamp | 152.6 | 1.59 | 4677 | 48.7 | 13.5 / 662 (63.4 s) | 1.00 / 1 | (stamp at resolve: 0.002, meaningless) | — | 4593 |

Reading:

- About 49 traversals per baked frame, one per arrival. They take roughly a
  third of the wall time (22 s of 70 s). An arrived tile waits about 30 ms on
  average for the loop, and up to 250 ms. Each of the ~26 000 wakes waits
  20–25 ms before its load moves again. This is the cost Steps 1 and 2 go
  after.
- The first run's 152 s ran while the other worktree's tests were busy. It
  shows how much a shared machine moves these numbers, so it is not a
  reference.
- **The bake is not deterministic from run to run, on `main` as well.**
  `tuile-bake --diff` between two bakes of the same tape on `a02566e` finds
  68 of 96 frames identical. Between two bakes on this branch it finds 78 of
  96. Between this branch and main it finds 87 of 96. The differences are a
  tile or a few at the deepest levels (17–19): a parent drawn in one run, its
  four children in the other. That is the known clock dependence: hysteresis,
  and holds that depend on arrival order. These instruments neither cause it
  nor fix it. Later steps should therefore be judged on the goldens with an
  instant loader (`QUAD_FLIGHT`, `projection_bake`, traversal.rs:1686). A
  real bake is not byte-reproducible today, even before any change.

## What was not measured

- **The web viewer.** On wasm `stamp()` is a no-op, so every timer reads
  zero there. Only the counts (traversals, arrivals per pass) can be compared,
  and they were not taken here.
- **A cold cache.** Both the viewer's store and the native HTTP cache were
  warm.
- **The farm** (stl-batch-1, the reference film): this is a local baseline
  only.
