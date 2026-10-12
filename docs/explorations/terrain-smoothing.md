<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# Terrain smoothing — an exploration

The ground of a film can read as chiselled: large flat facets with hard
edges. This looks at where facets come from in what is drawn today, what
could smooth them, by what kind of shader, at what cost, and what each does
to the truth of the relief. **Nothing here is a decision.** Two prototypes
were built to be looked at; they are switches on the film renderer of branch
`explore/terrain-smoothing` (which sits on `feat/seam-stitching`) and are
marked `PROTOTYPE` in the code.

```
tuile-film-render <film> --smooth off|shading|geometry [--smooth-strength 0..1] [--smooth-bound <m>] [--wire]
```

`off` is the default, and its picture is the stitching branch's to the byte
(frame 4500 compared: 0 pixels differ). `--wire` draws the mesh as lines: the
facets are what is being judged.

How things were measured: three scenes drawn by the film's own renderer, low
sun and shadows on so that relief reads — frame 4500 of film 577d (gentle
hills, 1920×1080), frame 1 of the island orbit (1280×720), and **one** frame
of high relief, frame 13700 of a Pyrenees trajectory, a preview pack baked
at a screen-space error of 8 and rendered here at 1920×1080. High relief was
judged on that single frame at a coarse threshold; no film of it exists.
Pictures are in `videos/` (`smoothing-*`), with their commands in its README.

## 1. Where the facets come from

**Not from the shading, today.** The first thing measured was the one
expected to be the cause: are faces lit flat? They are not. Every tile of the
three scenes — 253, 162 and 874 of them — comes from the store **with the
source's own normals** (the oct-encoded normals a terrain tile can carry),
and the resolve already interpolates them across each triangle; so does the
interactive renderer. What a face lit flat looks like is the first panel of
each sheet (`--smooth-strength 0`): that is the chiselled picture, and it is
not the one a film shows.

| scene | pixels that change by more than 8/255 between flat faces and today's picture |
|---|---|
| frame 4500, gentle hills | 19 690 of 2 073 600 |
| Pyrenees, high relief | 49 324 of 2 073 600, 172 of them by more than 32/255 |

So facets are seen where a mesh reaches a renderer **without** normals. In
this repository that is: a pack that carries its own meshes baked from a
source asked without the normals extension; any path that drops them — the
USD export and the Hydra hand-over were not checked here; and content that is
not terrain. The interactive renderer computes a normal a vertex when a mesh
has none, the film renderer does not: it falls back to the face's own.

**From the mesh**, three ways that normals do not hide:

- *Silhouettes.* A ridge against the sky or against a far slope is the
  mesh's outline: straight segments, whatever lights them.
- *Shadows.* The sun's depth map is drawn from the triangles, so a shadow's
  edge and the line where a slope turns away from a low sun follow facets
  even when the lit side is smooth.
- *The level drawn.* The source's triangles are sparse where it judged the
  ground simple — in the wire panels of frame 4500 a triangle is a hundred
  pixels across — and a film baked at a screen-space error of 8 or 16 draws
  tiles two or three levels coarser than one baked at 2.

## 2. What can be done, and by what kind of shader

| | what it is | where it runs | max distance from the source's surface | cost |
|---|---|---|---|---|
| A. Normals where there are none | a normal a vertex, from the faces round it, for a mesh that has none | CPU at tile load; the **compute resolve** interpolates (it already does) | 0 m | 0.1 ms a tile, 12 bytes a vertex; no triangle added |
| B. Curved patches | each triangle cut in four, new vertices lifted onto the patch the vertex normals describe, bounded | **cannot be a draw-time shader here**: wgpu/WebGPU has no tessellation or geometry stage. A CPU step at tile load (prototype), or a compute pass writing a refined mesh | the bound: 2 m in the prototype (mean 0.6 m) | triangles × 3.2 to 3.5; 0.3 to 1.3 ms a tile |
| C. Geomorph between levels | a tile's vertices slide from its parent's surface to its own with distance | the **vertex stage** (film: the same once-per-change compute pass as stitching); needs the parent's heights a tile | none at rest; between two real surfaces while it slides | not prototyped |
| D. Softer shadow edges | a wider filter of the sun's map, or its bias along the interpolated normal | the compute resolve | 0 m | not prototyped |
| E. A finer bake | a lower screen-space error: the source's own finer tiles | the bake | 0 m — it is the source | tiles and memory: a 4K frame at an error of 1 did not fit a 16 GB machine |
| F. Resample to a grid, bicubic across tiles | every tile rebuilt as a regular height field, heights filtered across neighbours' borders | CPU at tile load | unbounded in principle: it replaces the source's triangles | not prototyped; see below |

What was measured for the two prototypes:

**A, as built** (`--smooth shading`) recomputes normals from the mesh for
every tile, including those that have the source's. Against today's picture
that changes 34 pixels of frame 4500 by more than 8/255 and 27 of the
island's: the source's normals and the mesh's own agree. Its use is for the
meshes that have none; for those it is the whole difference between the first
panel and the second. No vertex moves, so stitching, the seam instrument and
a bake's exactness are untouched. Left out of the prototype: normals are
computed a tile at a time, so along a tile's border a vertex knows only its
own tile's faces — two tiles can light a shared edge differently. The
source's normals do not have that fault. Tying tiles together for this means
averaging the border normals of the tiles `stitch::plan` already pairs.

**B, as built** (`--smooth geometry`, bound 2 m): the source's vertices stay
where they are, each triangle becomes four, a new vertex is lifted off its
chord by at most the bound, and an edge on a tile's border stays straight —
so tiles still meet on a line and the seam instrument reads 0 edges over a
quarter pixel in all three scenes, as without it.

| scene | triangles | mean lift | pixels changed by more than 8/255 against today |
|---|---|---|---|
| frame 4500 | 42 747 → 137 406 | 0.57 m | 12 526 (75 by more than 32/255) |
| island | 13 369 → 35 836 | 0.43 m | 123 |
| Pyrenees | 1 315 868 → 4 629 560 | 0.60 m | 2 964 (57) |
| Pyrenees, bound 8 m | the same | 1.27 m | 7 371 (170) |

Looked at (`smoothing-*-modes-and-wire.png`): beside today's picture the
geometry panel is hard to tell apart. On gentle hills the triangles are large
but the ground is nearly flat across them; in the Pyrenees they are already
near a pixel wide at this threshold. It rounds silhouettes by a metre or two,
which at these distances is under a pixel. It costs three times the
triangles in every pass, and it puts ground where the source has none, if by
little.

## 3. What each does to silhouettes, drape and height

- **Silhouettes**: A and D leave them as they are. B rounds them within its
  bound; C and E change them to another real surface.
- **Imagery drape**: A, C, D, E do not move a texel. B keeps each new vertex
  at the middle of its edge in the tile's coordinates and moves it in space:
  a texel slides by at most the bound.
- **Height**: A and D, 0 m. B, the bound. C, between two levels of the source
  while a tile slides. F is the one that cannot be bounded honestly: a
  filter across tiles rounds off real ridges and fills real gullies by an
  amount that depends on the relief, and it throws away the source's choice
  of where to put its triangles.
- **Stitching**: A, D, E nothing. B was built to keep border edges straight
  and runs before the plan; C would be the same mechanism as stitching — an
  offset a vertex by where it is in its tile — and would make a stitched
  edge the special case of a tile fully slid to its neighbour.
- **Exactness of a film**: A, B, D are functions of the tile alone. C depends
  on distance, so on the camera: deterministic, but a tile's mesh is then no
  longer one thing a frame.

## 4. Ranked

1. **A — give a normal to meshes that have none**, in the film renderer as
   the interactive one already does, and check the two paths not looked at
   here (USD export, Hydra hand-over) carry normals through. Cheap, moves
   nothing, and it is where a chiselled picture comes from.
2. **E — bake finer where relief is strong.** The honest triangles are the
   source's. This is a question of memory at bake time, not of rendering.
3. **D — soften what shadows do with facets**, if the low-sun pictures still
   read as cut. Not prototyped; small.
4. **C — geomorph**, if films show levels popping. It shares stitching's
   machinery. Not prototyped; it is the largest piece of work here.
5. **B — curved patches**: built, measured, and not worth its triangles on
   what was looked at. It would matter only very near the ground, on a
   source with large triangles over curved relief.
6. **F — resampling to a grid**: not recommended. It trades the source's
   surface for a smoother one of our own making.

## 5. What was not established

- Which picture the chiselled relief was seen on. If it was a film rendered
  from the store, its tiles had normals and the cause is elsewhere — shadows,
  silhouettes, or the level drawn — and A changes nothing there.
- High relief beyond one frame at a coarse threshold.
- Any cost in frames a second: the machine was shared while these were
  rendered; only triangle counts and CPU time a tile are given.
- Whether the source's normals agree across a boundary between two levels.
- The interactive renderer was not given the switch: its branches for the
  application are not merged. It needs a normal strength in its view uniform
  (the fragment stage mixing the face's own normal, from screen derivatives,
  with the vertex's), `smoothing::smooth` moved into the engine and called
  in `prepare`, and tiles prepared again when the setting changes.
