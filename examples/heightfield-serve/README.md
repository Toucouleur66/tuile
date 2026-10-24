<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# heightfield-serve — a relief of your own, served over plain HTTP

A synthetic relief — ridges and hills, 16 km square on the equator, a 2049 × 2049 grid
at an 8 m step — served as quantized-mesh terrain and as imagery coloured by height, for
`tuile-viewer` to show with no catalog and no token. It is the reference for a server that
makes its own maps: the relief is a `tuile_terrain::Grid`, the tiles come from
`tuile_terrain::HeightfieldSource`, and everything else is a few routes.

```sh
cargo run --release -p tuile-heightfield-serve            # http://127.0.0.1:8092
```

It prints the commands that show the relief:

```sh
# Native, on this machine
cargo run --release -p tuile-wgpu-viewer -- \
  --terrain-url http://127.0.0.1:8092/terrain/layer.json \
  --imagery-template 'http://127.0.0.1:8092/imagery/{z}/{x}/{y}.png' \
  --imagery-scheme geographic --imagery-max-level 15 \
  --lon 10.0 --lat 0.0 --altitude 3000 --pitch 35

# Web: start the page, then open the URL the server printed
scripts/try-viewer.sh web
#   http://localhost:8091/examples/web-viewer/?terrain=…&imagery=…&scheme=geographic&maxLevel=15&…
```

## Routes

| Route | Content-Type | What |
|---|---|---|
| `/terrain/layer.json` | `application/json` | the terrain's description: `"scheme": "tms"`, the `octvertexnormals` extension, the levels and tiles available |
| `/terrain/{z}/{x}/{y}.terrain` | `application/vnd.quantized-mesh;extensions=octvertexnormals` | one tile, **TMS**; uncompressed |
| `/imagery/{z}/{x}/{y}.png` | `image/png` | 256 × 256, geographic grid, **XYZ** |

## What a server of its own has to get right

- **Two ways of counting rows.** Quantized-mesh terrain is addressed **TMS**: on the
  geographic grid (two tiles at level 0), row 0 is along the **south** edge. The viewer reads
  that from the `layer.json`, so `--terrain-url` (or `terrain: { url }` on a page) takes
  nothing else. Imagery templates are addressed **XYZ**: row 0 along the **north** edge.
  The grid they are cut on is the template's to say: `--imagery-scheme geographic`
  (`scheme: "geographic"`) for this one, `webmercator` (the default) for the usual
  web map tiles.
- **CORS.** A page is served from another origin than its tiles (here `:8091` and
  `:8092`), so every answer carries `Access-Control-Allow-Origin: *`. No credentials are
  sent and none are needed. A native viewer does not care.
- **HTTPS beside an HTTPS page.** A page served over HTTPS (a phone needs one for WebGPU)
  may not fetch plain HTTP. `--tls-cert PEM --tls-key PEM` serves HTTPS with the same
  `mkcert` certificate `scripts/try-viewer.sh web --lan --https` makes.
- **Edges ramped down to 0 m.** Outside the map, the server answers the WGS84 ellipsoid
  (height 0) down to level 14, so there is ground everywhere. A relief that stopped at its
  edge at full height would show a cliff in every tile straddling the border; this one comes
  down smoothly to 0 m over the last 1.5 km. A real map should do the same.
- **`maxzoom` and upsampling.** Terrain stops at level 15, where a tile is about 611 m
  wide and 77 vertices put one every 8 m — the grid's own step. Deeper tiles answer 404,
  which the `layer.json` already says; the viewer then refines by cutting the level-15
  tile, so the ground stays continuous however close the eye comes. Imagery is served to
  level 15 (`--imagery-max-level 15`); deeper, the viewer stretches it.
- **No crack between tiles.** Heights are sampled at each vertex's own position
  (bilinearly at the finest levels, the mean over the vertex's footprint at coarser
  ones), so two neighbours sample the same points on their shared edge.

## Flags

| Flag | |
|---|---|
| `--listen ADDR` | where to listen; `127.0.0.1:8092` unless said, `0.0.0.0:8092` for a phone |
| `--tls-cert PEM --tls-key PEM` | serve HTTPS |
| `--public-url URL` | the address the printed commands use, when the server is reached from another machine |

## On a phone

On the Mac, in two terminals (the certificate is the one the first command makes, in
`target/try-viewer-certs`; the phone must trust mkcert's root once, see
`scripts/try-viewer.sh`):

```sh
scripts/try-viewer.sh web --lan --https
cargo run --release -p tuile-heightfield-serve -- --listen 0.0.0.0:8092 \
  --tls-cert target/try-viewer-certs/cert.pem --tls-key target/try-viewer-certs/key.pem \
  --public-url https://$(ipconfig getifaddr en0):8092
```

then open, on the phone, the web URL the server prints (its page is on the same address,
port 8091).
