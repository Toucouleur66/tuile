<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# orbit-probe

Headless orbit probe: flies a full turn over Lake Geneva and reports what
the streaming session actually did with its tiles.

The viewer can show that texture keeps vanishing and coming back as the
camera turns, but not *why*: a live session mixes camera easing, frame
pacing and GPU upload budgets into one blur. This runs the same server on a
scripted path with no window, so the numbers mean one thing —

- **loads** — every `Content` the server sent
- **distinct** — how many different tiles those covered
- **reloads** — content for a tile that had already been sent and dropped
- **evictions** — tiles the budget reclaimed

A healthy turn loads each tile about once. Reloads mean the session is
paying for the same tile repeatedly, which is what the eye reads as texture
flickering back in. Coming back round to a bearing already visited should
cost nothing: the second half of the orbit revisits the first half's tiles,
so reloads there say the residency did not hold them.

```text
cargo run --release -p tuile-orbit-probe             # 24 steps, no images
cargo run --release -p tuile-orbit-probe -- --png    # also write a frame per step
```
Reads `CESIUM_ION_TOKEN` (env or `.env`).

## Built on

`tuile-wgpu`, `tuile-planetary`, `tuile-cesium-ion`, `tuile-native-fetchers`, `tuile-storage-foyer`, `tuile-camera`.
