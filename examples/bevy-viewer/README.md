<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# bevy-viewer — tuile's globe in a Bevy application

A Bevy 0.20 application over `tuile-bevy`. It drives a tilted orbit camera of the kind a strategy game has, draws pillars of its own stood on the drawn ground (one `TuileGround::ground_heights` batch a frame), and moves a marker that follows the cursor on that ground (`TuileGround::pick_viewport`).

```sh
cargo run --release -p tuile-heightfield-serve        # terminal 1: a 16 km map, :8092
cargo run --release -p tuile-bevy-viewer              # terminal 2
```

Mouse controls:
- left-drag pans;
- right-drag turns and tilts;
- the wheel zooms from 30 km down to 50 m.

The flags (`--range`, `--pitch`, `--msaa`, `--hdr`, `--compression`, `--gpu-mib`, `--screenshot PATH` and the sources) are listed at the head of `src/main.rs`.

## In a browser

```sh
examples/bevy-viewer/web/build.sh                              # the WebGPU and WebGL2 builds
python3 -m http.server 8093 -d examples/bevy-viewer/web        # the page
open 'http://127.0.0.1:8093/index.html'                        # ?backend=webgl2 for WebGL2
examples/bevy-viewer/web/check.py --out /tmp/bevy-web          # headless Chrome, both builds
```

## Known: screenshots of back-to-back launches

`--screenshot` saves the application's own window once the globe has settled. When several runs are launched back to back with no pause between them, some captures came out entirely black, even though the globe reported itself settled. The backdrop, which is drawn on every frame, was missing from those captures too. So this is a capture or presentation issue, a window not yet on screen or occluded, and not missing ground.

Leave about 3 seconds between launches: with that spacing, every capture came out right.
