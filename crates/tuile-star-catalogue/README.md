<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-star-catalogue

The sky's star catalogue as tuile keeps it, and the table its shaders read,
made from it. This is a tool crate: no renderer links it, and Parquet and Arrow
reach no wasm build.

Two steps. Each is a binary of this crate, run once, with its output committed:

1. `convert` turns the Bright Star Catalogue, 5th Revised Edition (Hoffleit &
   Warren 1991), as the CDS distributes it (catalogue V/50), into
   `data/bright_stars.parquet`:
   - Typed columns: `hr` u32, `ra_j2000_deg` and `dec_j2000_deg` f64, proper
     motion f64, `v_mag` f64 and `b_v_mag` f64.
   - Each column is described in its field metadata, and the source's URL and
     SHA-256 are in the file's metadata.
   - It refuses a source whose checksum is not the recorded one.
2. `table` turns that Parquet file into `tuile-atmosphere/src/stars.table`,
   the binned texture `stars_seen` reads (`tuile_atmosphere::stars::StarTable`).

```text
mkdir /tmp/bsc5 && cd /tmp/bsc5 && curl -O https://cdsarc.cds.unistra.fr/ftp/V/50/catalog.gz
cargo run -p tuile-star-catalogue --bin convert -- /tmp/bsc5/catalog.gz \
    crates/tuile-star-catalogue/data/bright_stars.parquet
cargo run -p tuile-star-catalogue --bin table -- \
    crates/tuile-star-catalogue/data/bright_stars.parquet crates/tuile-atmosphere/src/stars.table
```

The tests check two things:
- the committed table is the committed catalogue binned;
- with `TUILE_BSC5_GZ` set to the source, the conversion gives the committed
  rows again.

The provenance and terms are in the repository's `NOTICE`.
