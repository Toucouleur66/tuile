<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-b3dm

Codec for the Batched 3D Model container (b3dm, 3D Tiles 1.0): a 28-byte
header, feature/batch tables, and an embedded binary glTF.

This crate only handles the *container*: [`parse`] hands back table
slices and the glb payload without copying; [`encode`] builds a valid
b3dm around a glb (handy for tests and tilers). Decoding the embedded
glTF is the caller's business.

No I/O, no unsafe, wasm-clean.

## Where it sits

No workspace dependency. Used by `tuile-core`, whose content decoder unwraps
b3dm tiles with it before decoding the glb inside.

## Example

```rust
let glb = b"glTF....";
let b3dm = tuile_b3dm::encode(glb, r#"{"BATCH_LENGTH":0}"#);
let parsed = tuile_b3dm::parse(&b3dm)?;
assert_eq!(parsed.glb, glb);
# Ok::<(), tuile_b3dm::B3dmError>(())
```

## Features

None.

## Documentation

- [`docs/02-3d-tiles-primer.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/02-3d-tiles-primer.md)
- [`docs/10-crate-core.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/10-crate-core.md)
