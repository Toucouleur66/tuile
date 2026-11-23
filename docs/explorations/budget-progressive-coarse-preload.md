<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Progressive coarse and neighbour preload: one score for loading and releasing

Status: **implemented, 2026-11-23**, branch `feat/budget-progressive-coarse-preload`, for
Laurent's review. Asked by Laurent ("ça doit être corrigé asap"): holes and missing coarse
ground on a zoom-out or a rotation (`videos/holes-study/`).

## 1. What was wrong

On `main` (2719c2f3) a coarse tile got no treatment of its own:

- the coarse band was empty (`coarse_through = 0` in every specification) and the floor
  stopped at level 2;
- a preloaded ancestor was the chain of the drawn tiles only, worth a flat 25 % of a drawn
  tile, and nothing loaded the tiles *around* the view at any level (no ring without views
  ahead, no siblings);
- the release order of everything that was wanted but not drawn was recency then
  restore cost per byte, the same for a level-12 tile next to the eye and a level-6 tile
  three hundred kilometres away.

So a zoom-out or a turn found nothing resident beside the chain under the eye, and the
ground fell back to the floor (or showed a hole while the floor was still on its way).

## 2. The score

For a tile `t` of level `ℓ`, the view needing level `n` at its nearest point `F` (the centre
of the finest drawn ground tile nearest the eye), and `d` the distance from `F` to `t`
measured **in tiles of level `ℓ`** (`d = dist(F, volume(t)) / s_ℓ`, `s_ℓ` the size of the
level-`ℓ` tile under `F`):

```
score(t) = P(ℓ, n, d) × R(tier) / bytes(t)          (release: lowest first)
gain(t)  = P(ℓ, n, d) × S(t) × (H − T(tier))⁺        (load: highest first)

P(ℓ, n, d) = A(ℓ, n) × D(d)                          the probability of use

A(ℓ, n) = 1                       ℓ = n   (the present need)
        = a0 × 2^−(n−1−ℓ)         ℓ < n   (n−1: a0, n−2: a0/2, then halving per level up)
        = a↓ × 2^−(ℓ−n−1)         ℓ > n   (n+1: a↓; only n+1 is listed)

D(d)    = ρ² / (ρ + d)²                                (1 at F, ¼ at d = ρ)
```

- `R(tier)` is the restore cost of the tier a release sends the copy back to — a GPU copy
  whose decoded copy is still held comes back by an upload (2 ms), any other by the network
  (150 ms) — the specification's `RestoreCosts`, replaced by measured ones as they come
  (spec §4.2). Dear to restore ⇒ kept longer.
- `bytes(t)` is the GPU copy's size, floored at `ref_bytes / 16` (16 KiB).
- `S(t)` is the screen weight (½ at the edge of the view, 1 at its centre) and
  `(H − T(tier))⁺` the part of the horizon left once it arrives: a load does not know its
  bytes yet, it pays its fetch time instead. **The two sides share `P`**: that is the one
  cost function — what is preloaded first is what is released last.
- A tile that leaves the neighbourhood keeps its last `P`, and its release key ages as
  GreedyDual-Size's does (`L + P·R/bytes`, `L` the time it was last needed): nothing drops
  from one score to zero.

Everything is in integers: `P` in 1/32 000ths (`value::CERTAIN`, a multiple of 1 000 so a
probability in ‰ is exact; fifteen halvings reach one unit), `d` in sixteenths of a tile,
times in µs. No float reaches a decision; the same on wasm. Sixteenths, not quarters: one
step of `D` near `F` is then `(1 + 1/(16ρ))²`, 1.06 at ρ = 2, under the margin's 1.25, so
two tiles of one level cannot trade places for an infinitesimal move of `F` (the proof
notebook's finding: with quarters, `(1 + 1/8)²` = 1.27 at ρ = 2 exceeded it).

### Constants as implemented

| Constant | Value | Where |
|---|---|---|
| floor `F` | levels `0..=3`, mesh and drape, loaded first, never released | `Profile::pinned_level` (`TUILE_PIN_LEVEL` overrides) |
| `a0` | 800 ‰ | `Params::progressive.a0_permille` (`a0=` in a query) |
| `a↓` | 200 ‰ | `Params::progressive.descend_permille` (`a_desc=`) |
| `ρ` | desktop and desktop page 4, phone and phone page 2, film 0 | `BudgetConfig::reach_tiles` |
| `K(ρ)` | `⌊π(ρ+1)²⌋` tiles per level: 78 at ρ = 4, 28 at ρ = 2 | `tuile-core` runtime |
| `m` | 25 % (interactive, known path), 15 % (FPV), 0 (film) | `Params::margin_permille` |
| `R` | decoded 2 ms, stored 6 ms, browser cache 15 ms, network 150 ms | `Params::restore` |
| `H` | 1 s interactive, 8 s known path, 2 s FPV | `Params::horizon` |
| preload slots | ≤ 75 % of each class's network slots (≥ 1): a quarter always free for the present need, and a hole preempts a preload in flight | `Params::speculation.slots_permille` |
| preload memory | delivered only below the target's low watermark, never displacing the present need | `deliver_one` |

### Where the tiles come from

The geometry server lists them (`crates/tuile-core/src/neighbourhood.rs`): the chain of every
drawn tile stays the **ancestors** list (the fallback of what is drawn, protected as it was),
each placed against the need; every other tile of the neighbourhood goes in the **ring** list
(urgency `Ring`): wanted, scored, released by the score, never protected. A preload's drapes
take the preload's own urgency and score, never the present need's.

## 3. Above the score: the absolute laws

The score orders the value band only. Before it, in this order and whatever the score:

1. **the floor** (levels ≤ `F`): loaded first, never released;
2. **coverage**: a wanted tile with nothing drawable (a hole) loads before any value;
3. **the swap**: what is drawn, and the children a drawn parent waits on, is never released;
4. **the hard limit**.

A preload never displaces anything the present need wants; it displaces another preload only
when `P(new) > P(old) × (1 + m)` (hysteresis: two tiles of nearly equal score do not trade
places at every step).

## 4. The bound

The runtime lists, per level `ℓ` from `F + 1` to `n + 1`, the `K(ρ) = ⌊π(ρ+1)²⌋` tiles of
level `ℓ` nearest `F` among those with `d ≤ ρ` (nearest first, ties by tile id). So

```
preloaded tiles ≤ |floor| + (n + 1 − F) × ⌊π(ρ+1)²⌋
```

by construction, whatever the tree. With `n = 13`, `F = 3`: 11 levels; at ρ = 4, 858 tiles;
at ρ = 2, 308. A level-3 floor is 85 tiles on a 4ˡ pyramid. At ≈ 340 KiB a tile (drape and
its mipmaps, as `TUILE_PIN_LEVEL`'s note measures it) plus its mesh: about 300 MiB on a
desktop (target ≥ 1 GiB) and 105 MiB on a phone (target 160–768 MiB), the floor 28 MiB more.
Beyond what fits under the low watermark the lowest scores are simply not delivered: the
bound is on what is *asked*, the target on what is *held*.

## 5. Coverage

On a locally square grid of side `s_ℓ`, the tiles meeting a disc of radius `r` around `F` lie
inside the disc of radius `r + √2` (a tile's diagonal is `√2`), so there are at most
`π(r + √2)²` of them. They are also the nearest to `F`: any tile not meeting the disc is
farther. Keeping the `K = ⌊π(ρ+1)²⌋` nearest therefore keeps every tile meeting the disc of

```
r_ℓ = (ρ + 1 − √2) × s_ℓ          ≈ 3.59 s_ℓ at ρ = 4, 1.59 s_ℓ at ρ = 2
```

and `s_ℓ` doubles per level up. Once those tiles are resident, **any zoom-out or rotation
whose new view stays within `r_ℓ` of `F` finds level-`ℓ` ground under it**, for every `ℓ`
between the floor and `n`; beyond the coarsest listed level's disc, the floor covers the
globe. With `s_13` ≈ 4.9 km (a 4ˡ pyramid on the equator), the level-12 disc reaches 35 km,
level 10 140 km, level 7 1 100 km, level 4 9 000 km — a zoom-out from level 13 to the whole
globe crosses only resident ground. On a geographic grid near a pole a tile is narrower than
it is tall: the count `K` stays the bound, and the disc shrinks by `cos φ` in longitude.

Order of arrival: at equal distance a level `n−1` tile scores `a0 = 0.8` of a drawn one,
`n−2` 0.4, and the intermediate levels halve again each step up — they are loaded last and
released first, after every tile nearer the present need. Neighbours of the present level
come before the far coarse ones by `D`: at `d = ρ` a level-`n` tile scores ¼.

## 6. The work

- The neighbourhood is walked again only when `n`, the level-`n` tile under `F`, or `ρ`
  change: a still or slowly moving camera costs nothing. The walk visits at most
  `4 K` tiles per level (the children of the tiles within `ρ/2`).
- The budget reads it as part of the need's delta, tile by tile: a tile's `P` changes only
  when its `d` or `n` did. Its release key is re-filed then, never in a per-step sort.
- The decision stays pure CPU and never waits on I/O; preloads are started with free slots
  only, at most three quarters of them (the last quarter stays free for the present need), and a hole preempts a preload in flight.

## 7. Instruments: one source, the window and Grafana

The budget fills the numbers while it decides; the window, the metrics and the log records
read them and never compute their own (`crates/tuile-budget/src/score.rs`).

- **Per decision** (`Decisions::scores`): every preload started and every GPU copy evicted,
  with its terms `A`, `D`, `P`, `R`, `bytes`, `score`, its level, distance band, side and
  reason (`preload`, `admission`, `swap`, `sweep`, `pressure`, `recycled`).
- **Per step** (`BudgetStats::score`): preloads in flight and evictions by level × distance
  band, and the parameters in force.
- **The held set** (`Budget::held_report`, taken by the geometry server at each publish,
  every 0.5 s, never in a step): per level × band the count, sum, min and max of each term,
  and the 32 lowest and 32 highest scored GPU copies outside the floor, with their state
  (`kept`: drawn or in a swap; `wanted`; `left`).

Units, the same everywhere: `A`, `D`, `P` in 1/32 000ths (`CERTAIN`; the window prints them
as fractions), `R` in µs, bytes in bytes (the window in MiB), the **score in µflouz per
reference byte** — `P × R × ref_bytes / bytes`, µs at certainty for a 256 KiB resource: the
unit of the release key's credit.

Metrics (`tuile_budget_` prefix; labels `level`, `distance` ∈ {0, 1, 2, 3, 4+}, `side` ∈
{preload, evict, held}, `term` ∈ {score, a, d, r_us, bytes}, `name`; never a tile):

| Series | Kind | Meaning |
|---|---|---|
| `param{name}` | gauge | `a0_permille`, `a_desc_permille`, `rho_tiles`, `margin_permille`, `floor_level`, `preload_slots_permille`, `need_level` |
| `need_level` | gauge | `n` |
| `preload_by_level_distance{level,distance}` | gauge | preloads in flight |
| `evictions_by_level_distance{level,distance}` | gauge | the last step's evictions |
| `score_decisions_total{level,distance,side}` | counter | preloads started, copies evicted |
| `score_sum_total{level,distance,side,term}` | counter | each term summed over them (mean = sum / decisions) |
| `score_min`, `score_max{level,distance,side,term}` | gauge | over the last publish period |
| `score_held_tiles{level,distance,side="held"}` | gauge | GPU copies held |
| `score_held_sum`, `score_held_min`, `score_held_max{level,distance,side,term}` | gauge | their terms |
| `score_events_dropped_total` | counter | decisions beyond the log records' rate limit |

Log records (OTLP `/v1/logs` → Loki), one per decision (`body` `tuile.budget.decision`) and
one per listed held copy (`tuile.budget.held`, `end` lowest | highest, `rank`), attributes
`tile` (`level/x/y`), `resource`, `class`, `level`, `distance`, `side`, `reason` or `state`,
`a`, `d`, `p`, `r_us`, `bytes`, `score`; at most 2 000 decisions a push, the rest counted.
**A tile's address leaves the process here and only here**, as a log attribute, at
Laurent's request (2026-11-23); it is never a metric label.

The window prints the formula with its parameters in its header, the level × band table
(preloads in flight / evicted, or started / evicted since the cumulative view's start), the
terms per level and side, and the two held lists.

### Grafana panels (proposal, to port into the STL dashboards)

```json
{
  "title": "Budget — progressive score",
  "panels": [
    {"type": "stat", "title": "Parameters",
     "targets": [{"expr": "max by (name) (tuile_budget_param)", "legendFormat": "{{name}}"}]},
    {"type": "heatmap", "title": "Preloads in flight, level × distance",
     "targets": [{"expr": "sum by (level, distance) (tuile_budget_preload_by_level_distance)", "format": "table"}]},
    {"type": "heatmap", "title": "Evictions /s, level × distance",
     "targets": [{"expr": "sum by (level, distance) (rate(tuile_budget_score_decisions_total{side=\"evict\"}[1m]))", "format": "table"}]},
    {"type": "timeseries", "title": "Mean score by level and side",
     "targets": [{"expr": "sum by (level, side) (rate(tuile_budget_score_sum_total{term=\"score\"}[5m])) / sum by (level, side) (rate(tuile_budget_score_decisions_total[5m]))", "legendFormat": "{{side}} z{{level}}"}]},
    {"type": "timeseries", "title": "Held score range by level",
     "targets": [{"expr": "min by (level) (tuile_budget_score_held_min{term=\"score\"})", "legendFormat": "min z{{level}}"},
                 {"expr": "max by (level) (tuile_budget_score_held_max{term=\"score\"})", "legendFormat": "max z{{level}}"}]},
    {"type": "timeseries", "title": "Mean A and D of evictions",
     "targets": [{"expr": "sum by (term) (rate(tuile_budget_score_sum_total{side=\"evict\",term=~\"a|d\"}[5m])) / ignoring(term) group_left sum(rate(tuile_budget_score_decisions_total{side=\"evict\"}[5m])) / 32000", "legendFormat": "{{term}}"}]},
    {"type": "table", "title": "Next released (Loki)", "datasource": "Loki",
     "targets": [{"expr": "{service_name=\"tuile-viewer\"} | json | body=\"tuile.budget.held\" | end=\"lowest\" | line_format \"{{.tile}} z{{.level}} d{{.distance}} score {{.score}}\""}]},
    {"type": "logs", "title": "Evictions by tile (Loki)", "datasource": "Loki",
     "targets": [{"expr": "{service_name=\"tuile-viewer\"} | json | side=\"evict\""}]}
  ]
}
```

## 8. Tests

`crates/tuile-budget/tests/budget_progressive.rs`, the need built as the geometry server
lists it (each red with the neighbourhood left out of the need, or with the probability
taken out of the release key):

1. `the_floor_through_level_three_stays_under_pressure` — a budget cut to a twentieth evicts
   preloads, never a tile of levels 0–3;
2. `an_aggressive_zoom_out_from_level_13_finds_its_ground_resident` — levels 11 and 12 around
   the footprint resident before it starts; from 13 to 5, one level every two steps, no load
   completing: no hole, no tile drawn by a coarser ancestor;
3. `a_rotation_near_the_ground_finds_its_neighbours_resident` — the view turns from east to
   west at level 13 and draws exactly at once;
4. `the_release_order_follows_the_score` — property: under a random cut, every preload
   evicted scores at most the least preload kept;
5. `the_preloads_are_bounded_per_profile` — at most `⌊π(ρ+1)²⌋` a level, and `A`, `D` as
   written in §2;
6. `preloads_continue_when_idle_and_never_delay_the_present_need` — a still view keeps
   starting preloads until its neighbourhood is in; a hole opening then starts on the step it
   opens;
7. `a_preload_worth_more_by_the_margin_takes_a_weaker_ones_place` — a zoom-in with the room
   full: the levels above the new need replace the coarse ones by swaps.

Beside them: `tuile-core`'s `neighbourhood::tests` (on a flat quadtree: the per-level bound,
the levels listed, every tile meeting the disc of radius `(ρ + 1 − √2) s_ℓ` listed, no walk
while nothing moved), `budget_signals::the_score_series_are_the_decisions_terms` (the series
are the decisions' terms, one source), the window's `the_score_rows_are_the_series_numbers`
and the log records' `a_decision_record_carries_its_terms`.
