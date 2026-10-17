#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# Builds the phone application from the command line.
#
#   xcrun simctl boot "iPhone 16 Pro"                  # once: a booted simulator
#   ./build.sh simulator                              # → build/Build/Products/Release-iphonesimulator/Tuile.app
#   ./build.sh simulator --run                        # … installed and launched on the booted simulator
#   TUILE_TEAM=ABCDE12345 ./build.sh device           # signed for a device
#   TUILE_TEAM=ABCDE12345 ./build.sh device --install # … installed and launched on the connected one
#
# Three steps, each one tool: the property list, assets and settings page from
# `tuile-viewer-bundle phone`, the Xcode project from `xcodegen`, the bundle
# from `xcodebuild` (which runs cargo for the executable and signs).
#
# The host — whose sources the application uses — says which credential they
# need, in a small file (`TUILE_HOST_CONF`, by default the public viewer's
# `examples/wgpu-viewer/phone.conf`):
#
#   CREDENTIAL_VARIABLE=…    the environment variable the host reads it from,
#                            and the key of the field on the settings page
#   CREDENTIAL_TITLE=…       what that field is called
#   BUNDLED_TOKEN_ALIASES=…  older names for TUILE_TILES_TOKEN, below
#
# The token, at run time, is looked for in this order: the environment, the
# field in Settings › Tuile, a value built into the bundle, the file `token`
# in the application's Documents (Files › On My iPhone › Tuile), then a
# message that says where to put one.
#
# A DEVICE BUILD BUILDS THE TOKEN IN by default, from TUILE_TILES_TOKEN (or an
# alias) in the environment, else in the workspace's `.env` — a worktree's main
# checkout's, failing that. FOR THE DEVELOPER'S OWN DEVICE ONLY: anything in a
# bundle can be read back out of the .app, so a build for the App Store,
# TestFlight or anyone else must not carry one. `TUILE_TILES_TOKEN=none`
# builds without. The value is never printed, never an argument, and kept out
# of xcodebuild's environment, which xcodebuild writes into its log.
#
# TUILE_DEVICE picks the device to install on (name or identifier) when more
# than one is connected.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
workspace="$(cd "$here/../../.." && pwd)"
kind="${1:-simulator}"
then="${2:-}"
identifier="dev.lapoule.tuile.viewer"

case "$kind" in
  simulator | device) ;;
  *)
    echo "usage: $0 simulator|device [--run|--install]" >&2
    exit 2
    ;;
esac

# What the host's sources need. Read as data, not run.
CREDENTIAL_VARIABLE="TUILE_TILES_TOKEN"
CREDENTIAL_TITLE="Tile service token"
BUNDLED_TOKEN_ALIASES=""
host_conf="${TUILE_HOST_CONF:-$workspace/examples/wgpu-viewer/phone.conf}"
if [[ -f "$host_conf" ]]; then
  while IFS='=' read -r key value; do
    case "$key" in
      CREDENTIAL_VARIABLE) CREDENTIAL_VARIABLE="$value" ;;
      CREDENTIAL_TITLE) CREDENTIAL_TITLE="$value" ;;
      BUNDLED_TOKEN_ALIASES) BUNDLED_TOKEN_ALIASES="$value" ;;
    esac
  done < <(grep -E '^[A-Z_]+=' "$host_conf")
fi
credential=(--credential "$CREDENTIAL_VARIABLE" --credential-title "$CREDENTIAL_TITLE")
# shellcheck disable=SC2206 # a list of names, split on spaces on purpose
names=(TUILE_TILES_TOKEN $BUNDLED_TOKEN_ALIASES)

# The build-time token, for a device: the environment first, then `.env`.
bundled=""
if [[ "$kind" == device ]]; then
  for name in "${names[@]}"; do
    [[ -z "$bundled" ]] && bundled="${!name:-}"
  done
  common="$(git -C "$workspace" rev-parse --path-format=absolute --git-common-dir)"
  for envfile in "${TUILE_ENV_FILE:-}" "$workspace/.env" "$common/../.env"; do
    [[ -n "$bundled" || -z "$envfile" || ! -f "$envfile" ]] && continue
    for name in "${names[@]}"; do
      [[ -n "$bundled" ]] && continue
      bundled="$(sed -n "s/^[[:space:]]*\(export[[:space:]]\{1,\}\)\{0,1\}$name=//p" "$envfile" \
        | tail -n 1 | sed "s/^[\"']//; s/[\"']\$//")"
    done
  done
fi
[[ "$bundled" == none ]] && bundled=""
if [[ -n "$bundled" ]]; then
  echo "note: a token is built into this bundle — for this developer's own device only" >&2
  TUILE_BUNDLED_CREDENTIAL="$bundled" cargo run --quiet --manifest-path "$workspace/Cargo.toml" \
    -p tuile-viewer-bundle -- phone --out "$here/Generated" "${credential[@]}"
else
  cargo run --quiet --manifest-path "$workspace/Cargo.toml" -p tuile-viewer-bundle -- \
    phone --out "$here/Generated" "${credential[@]}"
fi
unset bundled
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
esac

# Nothing secret reaches xcodebuild: it writes its environment into its log.
unset_secrets=(-u TUILE_BUNDLED_CREDENTIAL -u "$CREDENTIAL_VARIABLE")
for name in "${names[@]}"; do unset_secrets+=(-u "$name"); done
env "${unset_secrets[@]}" \
  xcodebuild -project "$here/Tuile.xcodeproj" -scheme Tuile -configuration Release \
  -destination "$destination" -derivedDataPath "$here/build" -quiet \
  "${signing[@]}" build
app="$here/build/Build/Products/$products/Tuile.app"
echo "$app"

case "$then" in
  "") ;;
  --run)
    xcrun simctl install booted "$app"
    xcrun simctl launch booted "$identifier"
    ;;
  --install)
    # The device, from devicectl's JSON (its table changes between Xcode
    # versions): TUILE_DEVICE by name or identifier, or the first paired one
    # that is reachable — over its tunnel, or plugged in before the tunnel is
    # up.
    listing="$(mktemp)"
    xcrun devicectl list devices --quiet --json-output "$listing" >/dev/null
    device="$(python3 - "$listing" "${TUILE_DEVICE:-}" <<'PY'
import json, sys
devices = json.load(open(sys.argv[1]))["result"]["devices"]
wanted = sys.argv[2]
def reachable(d):
    c = d.get("connectionProperties", {})
    return c.get("pairingState") == "paired" and (
        c.get("tunnelState") == "connected" or c.get("transportType") == "wired"
    )
for d in devices:
    name = d.get("deviceProperties", {}).get("name", "")
    if wanted and wanted not in (name, d["identifier"]):
        continue
    if reachable(d):
        print(d["identifier"])
        break
PY
)"
    rm -f "$listing"
    if [[ -z "$device" ]]; then
      echo "error: no paired device is reachable${TUILE_DEVICE:+ under the name $TUILE_DEVICE}" >&2
      exit 3
    fi
    xcrun devicectl device install app --device "$device" "$app"
    xcrun devicectl device process launch --device "$device" "$identifier"
    ;;
  *)
    echo "usage: $0 simulator|device [--run|--install]" >&2
    exit 2
    ;;
esac
