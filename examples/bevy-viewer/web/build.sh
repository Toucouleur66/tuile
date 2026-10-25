#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# Builds the bevy-viewer for the browser, once per graphics API — Bevy
# compiles its shaders for WebGPU or for WebGL2, not both — into pkg/webgpu
# and pkg/webgl2 beside this script, with wasm-bindgen at the version in
# Cargo.lock.
#
#   examples/bevy-viewer/web/build.sh            # both, release
#   examples/bevy-viewer/web/build.sh webgl2     # one
#
# Then serve this directory (any static server that says application/wasm)
# and open index.html, with ?backend=webgl2 for the WebGL2 build.

set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../../.." && pwd)"
backends=("${@:-webgpu webgl2}")
read -r -a backends <<<"${backends[*]}"

wanted="$(awk '/^name = "wasm-bindgen"$/ { getline; gsub(/version = |"/, ""); print; exit }' "$root/Cargo.lock")"
have="$(wasm-bindgen --version 2>/dev/null | awk '{ print $2 }')"
if [[ "$have" != "$wanted" ]]; then
  echo "wasm-bindgen $wanted is needed (found ${have:-none}):" >&2
  echo "  cargo install wasm-bindgen-cli --version $wanted --locked" >&2
  exit 1
fi

cd "$root"
target="${CARGO_TARGET_DIR:-$root/target}"
for backend in "${backends[@]}"; do
  CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}" cargo build --release -p tuile-bevy-viewer \
    --target wasm32-unknown-unknown --features "$backend"
  rm -rf "$here/pkg/$backend"
  wasm-bindgen --target web --no-typescript --out-dir "$here/pkg/$backend" --out-name tuile_bevy_viewer \
    "$target/wasm32-unknown-unknown/release/tuile-bevy-viewer.wasm"
  wasm="$here/pkg/$backend/tuile_bevy_viewer_bg.wasm"
  # Smaller when Binaryen is installed: debug info stripped, optimised for
  # size. Optional — the module runs the same without it.
  if command -v wasm-opt >/dev/null; then
    wasm-opt -Oz --all-features --strip-debug --strip-producers -o "$wasm.opt" "$wasm"
    mv "$wasm.opt" "$wasm"
  fi
  echo "$backend: $(du -h "$wasm" | cut -f1)" \
    "($(gzip -9c "$wasm" | wc -c | awk '{ printf "%.1f MB gzipped", $1 / 1e6 }'))"
done
