<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-viewer

The interactive globe viewer, as a library — on a desktop, a phone and in a
browser.

A winit window driving the logical geometry server in **progressive** mode:
the server (terrain × imagery) runs off the window's thread — on a tokio
runtime on a desktop and a phone, in a Web Worker in a browser — and the
window sends the camera every frame and pumps decoded tiles to the GPU as
they arrive, refining as you orbit and zoom.

# One application, two shells

The application itself — the `app` module's frame step, gestures, loading
indicator, lifecycle, steering — is shared by every target, and so are its
renderer (`tuile-wgpu`), its controls (`tuile-ui`) and its camera
(`tuile-camera`). What differs is the *shell* around it: where the session
is brought up and what it is made of. The desktop and the phone are the
`native` shell; the browser is the `web` shell, a WebGPU or WebGL2 canvas
with the geometry server in a worker, exported as an ES module. Both hand
the application the same session handles under the same names, so no line
of the application asks which one it runs in. See
`docs/explorations/web-native-convergence.md`.

Everything a person sees is here — the window, the camera and its controls,
the start flags, the steering URLs, the scripting dictionary, the journal —
and none of it knows where the ground comes from. A binary is a
[`ViewerHost`]: who the application is, which layers it offers, and a
[`Sources`] to fetch them through. See [`embed`] for the three answers and
an example, and `docs/16-embedding-the-viewer.md` for the walk-through;
`examples/wgpu-viewer` is the host that answers with the public connectors.

Drag: orbit. Right-drag: pan. Wheel: zoom. W: wireframe. F: freeze. Esc: quit.
N, or a click on the compass ring: north up, about the point at the centre
of the view. L: centre on the current location, at the present altitude.

The session opens straight down on France from 2 000 km. These flags move
that, and are the only flags there are (`--help` lists them with the keys):

```text
--lon <deg> --lat <deg>   where the eye is, east and north positive
--altitude <m>            its height above the ellipsoid, 1 to 1e8
--heading <deg>           where it looks, 0 = north, clockwise
--pitch <deg>             how far below the horizon, 90 = straight down
--here                    over the current location, instead of --lon/--lat
--time <when>             the instant the scene is lit for: now, sunset,
                          sunrise (the next one over the start view), or
                          RFC 3339 (2026-06-21T19:42:00Z)
--time-offset <d>         added to it: -30m, -3h30m, 45m, 90s
--time-rate <x>           scene seconds a second: 1, 30, 0 (frozen), -1
```

C copies a link to the present view and its moment. A running viewer can be
steered by other programs with URLs of the same form — `tuile://goto?…`,
`north`, `here`, `view?…`: see `steer` for the vocabulary and what it cannot
do.

A `goto` takes the clock's three flags as `time=`, `timeOffset=` and
`timeRate=`, read by the same parser as the flags and a page's options. A
`sunset` or a `sunrise` is the next one *where the link leads*, not where the
eye was; without `time=` the clock is left as it is. Half an hour before
sunset at the Pic du Midi, an hour of the scene every two minutes:

```text
open "tuile://goto?lon=0.1418&lat=42.9365&altitude=3100&heading=290&pitch=0&time=sunset&timeOffset=-30m&timeRate=30"
```

The link C copies carries the clock the same way: counted from the nearest
sunset or sunrise when the clock was started from one
(`time=sunset&timeOffset=-12m`), so it brings back that moment of the evening
on whatever day it is opened; as an RFC 3339 instant otherwise; with
`timeRate=` when the clock does not run in real time. The
application bundle (`macos/README.md`) registers the scheme and carries a
scripting dictionary, through which a script reads the view — position,
attitude, target, range, tile counts: see `snapshot` — and commands it.

The current location comes from the operating system's location service
(macOS, cargo feature `current-location`, on by default), which asks the
person before it answers; elsewhere `--here` and L say it is not available
and change nothing. The coordinates are never logged or written anywhere.

`TUILE_RECORD=path.mcap` writes the camera path; `TUILE_REPLAY=path.mcap`
flies it again exactly. The format and the replay live in the `tuile-tape`
crate.

With the `control` feature — tests and debug builds only — `control` runs
the same application headless, steps it frame by frame and reads it back:
see that module.

## Where it sits

Depends on `tuile-wgpu`, `tuile-ui`, `tuile-camera`, `tuile-planetary`,
`tuile-terrain`, `tuile-atmosphere`, `tuile-overlay`, `tuile-tape` and
`tuile-core`; natively on `tuile-native-fetchers`, `tuile-storage-foyer`,
`tuile-metrics` and `tuile-cesium-ion`, in a browser on `tuile-web`. Its hosts
are `examples/wgpu-viewer` (desktop, phone), the `@lapoule-dev/tuile-viewer`
package (browser) and any application that embeds it. The example of a host
is in [`embed`].

## Features

- `current-location` *(default)* — the operating system's location service
  on macOS and iOS, for `--here` and L.
- `application` *(default)* — what makes the viewer an application:
  steering URLs, a message window, the clipboard; on a phone, the device's
  memory and temperature. macOS and iOS; elsewhere it compiles to nothing.
- `control` — the viewer headless, driven frame by frame over a localhost
  socket. Tests and debug builds only; refused in an iOS release build.

## Documentation

- [`docs/16-embedding-the-viewer.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/16-embedding-the-viewer.md)
- [`docs/11-crate-wgpu.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/11-crate-wgpu.md)
