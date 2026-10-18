#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# Copyright (c) lapoule.dev
#
# Builds the Android application from the command line: an APK for arm64
# devices and emulators, debug-signed.
#
#   ./build.sh                    # → build/Tuile.apk
#   ./build.sh --install          # … installed on the device adb sees
#   ./build.sh --run              # … installed and started
#
# No Gradle: the application is a native library and one small activity, and
# every step is one of the SDK's own tools —
#
#   1. `tuile-viewer-bundle android`: the manifest, the launch theme and the
#      icons, written and tested in Rust;
#   2. `cargo ndk … rustc --crate-type cdylib`: the host's library
#      (`examples/wgpu-viewer` by default) for aarch64-linux-android;
#   3. `javac` + R8: the activity (`java/`), the TLS verifier's Kotlin half
#      (shipped inside the `rustls-platform-verifier-android` crate) and the
#      part of Kotlin's standard library it uses, as `classes.dex`;
#   4. `aapt2 compile` and `aapt2 link`: the resources and the manifest;
#   5. `zip`, `zipalign -P 16`, `apksigner`: the library stored uncompressed
#      on 16 KiB pages, then signed.
#
# Needs: ANDROID_HOME (or ~/Library/Android/sdk) with platforms;android-35,
# build-tools and an NDK; a JDK 17 or newer (found by /usr/libexec/java_home
# on macOS when JAVA_HOME is unset); network access to Maven Central once; `cargo install cargo-ndk`;
# `rustup target add aarch64-linux-android`.
#
# NO TOKEN GOES INTO THE APK. The catalog is asked through the catalog proxy
# (`tuile-catalog-proxy`), which holds the token on a server.
# TUILE_CATALOG_URL, when set here, is built in as the proxy's address
# (default: http://127.0.0.1:8091/catalog/, which `adb reverse` maps to the
# developer's machine — see `scripts/try-viewer.sh android`). It is an
# address, not a secret.
#
# TUILE_HOST_PACKAGE builds another host's library (a crate with an
# `android_main`); TUILE_APP_NAME and TUILE_APP_ID name its application.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
workspace="$(cd "$here/../../.." && pwd)"
then="${1:-}"
package="${TUILE_HOST_PACKAGE:-tuile-wgpu-viewer}"
library="lib${package//-/_}.so"
name="${TUILE_APP_NAME:-Tuile}"
identifier="${TUILE_APP_ID:-dev.lapoule.tuile.viewer}"
platform=35
minimum=26
build="$here/build"

sdk="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Library/Android/sdk}}"
if [[ ! -d "$sdk/platforms/android-$platform" ]]; then
  echo "error: no Android SDK with platforms;android-$platform at $sdk (set ANDROID_HOME)" >&2
  exit 2
fi
newest() { find "$1" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | sort -V | tail -n 1; }
tools="$(newest "$sdk/build-tools")"
ndk="${ANDROID_NDK_HOME:-$(newest "$sdk/ndk")}"
if [[ -z "$tools" || -z "$ndk" ]]; then
  echo "error: build-tools and an NDK are needed under $sdk" >&2
  exit 2
fi
export ANDROID_HOME="$sdk" ANDROID_NDK_HOME="$ndk"
# d8 and apksigner need a JDK 17 or newer; the first `java` on the PATH may be
# an older one. On macOS the system's chooser finds a recent one.
if [[ -z "${JAVA_HOME:-}" && -x /usr/libexec/java_home ]]; then
  JAVA_HOME="$(/usr/libexec/java_home -v 17+ 2>/dev/null || true)"
fi
if [[ -n "${JAVA_HOME:-}" ]]; then
  export JAVA_HOME PATH="$JAVA_HOME/bin:$PATH"
fi
android_jar="$sdk/platforms/android-$platform/android.jar"
# The NDK ships llvm-strip as a link to llvm-objcopy.
strip="$(find "$ndk/toolchains/llvm/prebuilt" -name llvm-strip \( -type f -o -type l \) | head -n 1)"
if [[ -z "$strip" ]]; then
  echo "error: no llvm-strip in the NDK at $ndk" >&2
  exit 2
fi

rm -rf "$build"
mkdir -p "$build/classes" "$build/dex" "$build/apk/lib/arm64-v8a"

# 1. Manifest and resources.
cargo run --quiet --manifest-path "$workspace/Cargo.toml" -p tuile-viewer-bundle -- \
  android --out "$build/generated" --name "$name" --identifier "$identifier" --package "$package" >/dev/null

# 2. The native library.
(cd "$workspace" && CARGO_INCREMENTAL="${CARGO_INCREMENTAL:-0}" CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}" \
  cargo ndk -t arm64-v8a --platform "$minimum" \
  rustc -p "$package" --lib --release --crate-type cdylib)
built="$workspace/target/aarch64-linux-android/release/$library"
# Stripped in the package only: the library under target/ keeps its symbols,
# for reading a native crash's stack.
"$strip" --strip-unneeded -o "$build/apk/lib/arm64-v8a/$library" "$built"

# 3. The activity, the verifier's half and what it runs on, as one dex.
#
# The verifier's half is written in Kotlin and calls into Kotlin's standard
# library, which a Gradle build adds on its own and this one fetches: the
# release below from Maven Central, checked against its digest, kept in the
# user's cache. R8 (the SDK's shrinker, inside build-tools) then keeps only
# what the activity and the verifier reach — a few hundred methods of the
# library's tens of thousands.
verifier="$(cd "$workspace" && cargo metadata --format-version 1 --filter-platform aarch64-linux-android \
  | python3 -c '
import json, os, sys
for p in json.load(sys.stdin)["packages"]:
    if p["name"] == "rustls-platform-verifier-android":
        print(os.path.dirname(p["manifest_path"]))
        break
')"
aar="$(find "$verifier/maven" -name '*.aar' | head -n 1)"
if [[ -z "$aar" ]]; then
  echo "error: the TLS verifier's Android component was not found" >&2
  exit 2
fi
(cd "$build" && unzip -q -o "$aar" classes.jar -d verifier)
kotlin_version=2.0.21
kotlin_sha256=f31cc53f105a7e48c093683bbd5437561d1233920513774b470805641bedbc09
kotlin="${XDG_CACHE_HOME:-$HOME/.cache}/tuile/kotlin-stdlib-$kotlin_version.jar"
if [[ ! -f "$kotlin" ]] || ! echo "$kotlin_sha256  $kotlin" | shasum -a 256 -c --status; then
  mkdir -p "$(dirname "$kotlin")"
  curl -sfL -o "$kotlin.part" \
    "https://repo1.maven.org/maven2/org/jetbrains/kotlin/kotlin-stdlib/$kotlin_version/kotlin-stdlib-$kotlin_version.jar"
  if ! echo "$kotlin_sha256  $kotlin.part" | shasum -a 256 -c --status; then
    rm -f "$kotlin.part"
    echo "error: Kotlin's standard library did not match its digest" >&2
    exit 2
  fi
  mv "$kotlin.part" "$kotlin"
fi
javac -source 8 -target 8 -Xlint:-options \
  -bootclasspath "$android_jar" -classpath "$build/verifier/classes.jar" \
  -d "$build/classes" $(find "$here/java" -name '*.java')
(cd "$build/classes" && jar cf "$build/activity.jar" .)
# Both are entered from native code, by name: nothing in the dex refers to
# them, so the shrinker is told to keep them whole.
cat >"$build/keep.pro" <<'RULES'
-keep class dev.lapoule.tuile.** { *; }
-keep class org.rustls.platformverifier.** { *; }
-dontobfuscate
-dontwarn kotlin.**
-dontwarn org.jetbrains.annotations.**
RULES
java -cp "$tools/lib/d8.jar" com.android.tools.r8.R8 --release --min-api "$minimum" \
  --lib "$android_jar" --pg-conf "$build/keep.pro" --output "$build/dex" \
  "$build/activity.jar" "$build/verifier/classes.jar" "$kotlin"

# 4. Resources and manifest.
"$tools/aapt2" compile --dir "$build/generated/res" -o "$build/resources.zip"
"$tools/aapt2" link -I "$android_jar" --manifest "$build/generated/AndroidManifest.xml" \
  -o "$build/unaligned.apk" "$build/resources.zip"

# 5. Code in, aligned, signed.
cp "$build/dex/classes.dex" "$build/apk/"
(cd "$build/apk" && zip -q -0 -r "$build/unaligned.apk" lib && zip -q "$build/unaligned.apk" classes.dex)
"$tools/zipalign" -P 16 -f 4 "$build/unaligned.apk" "$build/aligned.apk"
# The SDK's debug key, made the way Android Studio makes it if there is none:
# a development signature, which a store would refuse.
keystore="$HOME/.android/debug.keystore"
if [[ ! -f "$keystore" ]]; then
  mkdir -p "$(dirname "$keystore")"
  keytool -genkeypair -keystore "$keystore" -storepass android -keypass android \
    -alias androiddebugkey -dname "CN=Android Debug,O=Android,C=US" \
    -keyalg RSA -keysize 2048 -validity 10000 >/dev/null 2>&1
fi
apk="$build/$name.apk"
"$tools/apksigner" sign --ks "$keystore" --ks-pass pass:android --key-pass pass:android \
  --out "$apk" "$build/aligned.apk"
rm -f "$build/unaligned.apk" "$build/aligned.apk" "$apk.idsig"
echo "$apk"

adb="$sdk/platform-tools/adb"
case "$then" in
  "") ;;
  --install) "$adb" install -r "$apk" >/dev/null ;;
  --run)
    "$adb" install -r "$apk" >/dev/null
    "$adb" shell am start -n "$identifier/dev.lapoule.tuile.TuileActivity" >/dev/null
    ;;
  *)
    echo "usage: $0 [--install|--run]" >&2
    exit 2
    ;;
esac
