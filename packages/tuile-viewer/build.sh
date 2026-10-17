#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# Builds the web package: `tuile-viewer` for wasm32, as a `cdylib`, and its
# JavaScript bindings beside this script, under `wasm/`.
#
#   packages/tuile-viewer/build.sh            # release
#   packages/tuile-viewer/build.sh --dev      # unoptimised, with debug info
#
# The crate's manifest stays `rlib` only: native builds do not link a
# library they never load. `cargo rustc --crate-type cdylib` asks for the
# cdylib here, for this one build. The bindings come from `wasm-bindgen` at
# exactly the version in Cargo.lock, which it refuses to work without.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
profile=release
if [[ "${1:-}" == "--dev" ]]; then
  profile=dev
fi
dir=$([[ $profile == dev ]] && echo debug || echo release)

wanted="$(awk '/^name = "wasm-bindgen"$/ { getline; gsub(/version = |"/, ""); print; exit }' "$root/Cargo.lock")"
have="$(wasm-bindgen --version 2>/dev/null | awk '{ print $2 }')"
if [[ "$have" != "$wanted" ]]; then
  echo "wasm-bindgen $wanted is needed (found ${have:-none}):" >&2
  echo "  cargo install wasm-bindgen-cli --version $wanted --locked" >&2
  exit 1
fi

cd "$root"
CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}" cargo rustc -p tuile-viewer --lib \
  --profile "$profile" --target wasm32-unknown-unknown --crate-type cdylib

rm -rf "$here/wasm"
wasm-bindgen --target web --out-dir "$here/wasm" --out-name tuile_viewer \
  "${CARGO_TARGET_DIR:-$root/target}/wasm32-unknown-unknown/$dir/tuile_viewer.wasm"
echo "built $(du -h "$here/wasm/tuile_viewer_bg.wasm" | cut -f1) into $here/wasm"
