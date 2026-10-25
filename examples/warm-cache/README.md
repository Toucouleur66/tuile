<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# warm-cache

Fills the tile store with the coarse pyramid, once, so a session never
starts cold.

The shallow levels are what every fallback lands on: a tile that has not
arrived is drawn by its nearest resident ancestor, and if that walk reaches
the top and finds nothing, the ground is bare. Leaving them to be discovered
by the camera builds the safety net out of exactly the tiles a fast movement
has not fetched yet — measured on ground already flown several times, level
5 held 291 of its 1024 tiles and level 3 held none at all.

A job rather than something the viewer does at startup. It is slow, it is
one-off, its result is on disk and shared by every later session, and it has
no business competing for bandwidth with the frame someone is waiting on.

```text
cargo run --release -p tuile-warm-cache          # levels 0..=5
cargo run --release -p tuile-warm-cache -- 6     # deeper, at four times the cost
```

The whole planet at level 5 is `4^5` imagery tiles — about 40 MiB
compressed, once. Every level below is a quarter of the one above, so the
sum is a third again. Level 8 would be 65 536 tiles and is not a warm-up.

## Built on

`tuile-planetary`, `tuile-cesium-ion`, `tuile-native-fetchers`, `tuile-storage-foyer`.
