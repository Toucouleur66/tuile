#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# Builds the phone application from the command line.
#
#   ./build.sh simulator            # → build/Build/Products/Release-iphonesimulator/Tuile.app
#   ./build.sh simulator --run      # … and installs and launches it on a booted simulator
#   TUILE_TEAM=ABCDE12345 ./build.sh device          # signed for a device
#   TUILE_TEAM=ABCDE12345 ./build.sh device --install # … and installs it on the connected one
#
# Three steps, each one tool: the property list and assets from
# `tuile-viewer-bundle phone`, the Xcode project from `xcodegen`, the bundle
# from `xcodebuild` (which runs cargo for the executable and signs).
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
workspace="$(cd "$here/../../.." && pwd)"
kind="${1:-simulator}"
then="${2:-}"
identifier="dev.lapoule.tuile.viewer"

cargo run --quiet --manifest-path "$workspace/Cargo.toml" -p tuile-viewer-bundle -- \
  phone --out "$here/Generated"
(cd "$here" && xcodegen generate --quiet)

case "$kind" in
  simulator)
    destination="generic/platform=iOS Simulator"
    products="Release-iphonesimulator"
    signing=(CODE_SIGNING_ALLOWED=YES CODE_SIGN_IDENTITY=- DEVELOPMENT_TEAM=)
    ;;
  device)
    if [[ -z "${TUILE_TEAM:-}" ]]; then
      echo "error: set TUILE_TEAM to the signing team (Xcode → Settings → Accounts)" >&2
      exit 2
    fi
    destination="generic/platform=iOS"
    products="Release-iphoneos"
    signing=(-allowProvisioningUpdates DEVELOPMENT_TEAM="$TUILE_TEAM")
    ;;
  *)
    echo "usage: $0 simulator|device [--run|--install]" >&2
    exit 2
    ;;
esac

xcodebuild -project "$here/Tuile.xcodeproj" -scheme Tuile -configuration Release \
  -destination "$destination" -derivedDataPath "$here/build" -quiet \
  "${signing[@]}" build
app="$here/build/Build/Products/$products/Tuile.app"
echo "$app"

case "$then" in
  --run)
    xcrun simctl install booted "$app"
    xcrun simctl launch booted "$identifier"
    ;;
  --install)
    # The first paired, connected device, from devicectl's JSON (its table
    # changes between Xcode versions).
    listing="$(mktemp)"
    xcrun devicectl list devices --quiet --json-output "$listing" >/dev/null 2>&1 || true
    device="$(python3 -c '
import json, sys
devices = json.load(open(sys.argv[1])).get("result", {}).get("devices", [])
for d in devices:
    c = d.get("connectionProperties", {})
    if c.get("pairingState") == "paired" and c.get("tunnelState") == "connected":
        print(d["identifier"]); break
' "$listing" 2>/dev/null || true)"
    rm -f "$listing"
    if [[ -z "$device" ]]; then
      echo "error: no paired device is connected" >&2
      exit 3
    fi
    xcrun devicectl device install app --device "$device" "$app"
    xcrun devicectl device process launch --device "$device" "$identifier"
    ;;
esac
