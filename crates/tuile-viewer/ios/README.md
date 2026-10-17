# The viewer on a phone

The same application as the desktop one — `tuile-viewer`, hosted by
`examples/wgpu-viewer` — built for iOS. One program, one library: what differs
is a [`Profile`](../src/profile.rs) of numbers and a handful of platform calls.

```text
./build.sh simulator --run                       # build, install on the booted simulator, launch
TUILE_TEAM=ABCDE12345 ./build.sh device --install # build signed, install on the connected iPhone
```

Needs Xcode, `xcodegen` (`brew install xcodegen`) and the two Rust targets
(`rustup target add aarch64-apple-ios aarch64-apple-ios-sim`).

## How it is built

| step | tool | what it does |
|---|---|---|
| resources | `tuile-viewer-bundle phone` | `Generated/Info.plist` and the asset catalogue (icon, launch colour), written and tested in Rust |
| project | `xcodegen` | `Tuile.xcodeproj` from `project.yml` — generated, never committed |
| bundle | `xcodebuild` | runs `cargo build --target aarch64-apple-ios[-sim]` for the executable, compiles the assets, embeds a provisioning profile and signs |

Why this and not another tool: the executable is an ordinary cargo binary
(winit runs `UIApplicationMain` itself), so nothing needs to generate Rust or
wrap it in a static library — which is most of what `cargo-mobile2` does, along
with templates this workspace would then carry. What cannot be done without
Apple's tools is signing for a device: a provisioning profile is created by
Xcode's automatic signing, and `xcodebuild -allowProvisioningUpdates` is the
command-line door to it. `xcodegen` keeps that project a 60-line YAML file
instead of a committed `.pbxproj`.

## The access token

The phone reads `CESIUM_ION_TOKEN` from its environment (a simulator launched
with `SIMCTL_CHILD_CESIUM_ION_TOKEN=…`) or from the file `token` in the
application's **Documents** — visible in the Files application under *On My
iPhone › Tuile*. Put the token there on a line of its own. Nothing puts a
token in the bundle.

## What it does differently on a phone

- **Launch**: the window opens at once on the globe's shell — the launch
  screen is the same blue, so there is no step — with a loading indicator
  while the session connects and the coarse pyramid arrives, all of it on the
  async runtime. The main thread never waits on the network
  (`launch::tests::polling_a_stalled_launch_never_waits`,
  `app::stall`).
- **Memory**: the resident budget is a fifth of what the system leaves the
  process (`os_proc_available_memory`), between 160 and 768 MiB; a memory
  warning trims residency to half, the background to a quarter — never the
  tiles the view draws or their ancestors. The tile store lives in
  `Library/Caches` with a 768 MiB disk cap, and is started afresh if the
  system purged it from under the application.
- **Threads**: two runtime workers, four blocking threads, one to three
  decoders, sixteen fetches in flight over one HTTP/2 connection, and an
  upload budget per frame of four tiles or 6 ms.
- **Lifecycle**: no frame and no GPU work outside the foreground; loads held
  at a gate while away; the surface and render targets released in the
  background and rebuilt before the first frame back.
- **Energy**: frames are drawn only while something changes — a gesture, an
  ease, an upload, the indicator — and four times a second otherwise; 30 fps
  when the device runs hot or saves power, 20 when critical.
- **Touch**: one finger drags the ground, two pinch and twist, two sliding
  together tilt, a double tap zooms in, a finger held still centres on the
  current location. The compass and zoom buttons sit in the safe area at the
  display's scale.

`TUILE_TOUCH_SCRIPT=tour` plays a fixed set of gestures through the same path
once the first view is covered, for a film that can be made again.
