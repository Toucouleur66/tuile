<!-- SPDX-License-Identifier: MIT OR Apache-2.0 -->
<!-- Copyright (c) lapoule.dev -->

# tuile-viewer-bundle

Packages the viewer as a macOS application: `Tuile.app`, with its icon,
its `Info.plist` and an ad-hoc signature.

```text
cargo run -p tuile-viewer-bundle -- app                # → target/bundle/Tuile.app
cargo run -p tuile-viewer-bundle -- app --out ~/Applications
cargo run -p tuile-viewer-bundle -- icon               # redraw icon-1024.png
cargo run -p tuile-viewer-bundle -- phone --out DIR    # a phone's plist and assets
cargo run -p tuile-viewer-bundle -- android --out DIR  # an Android manifest and resources

# another host's application, or a second copy under another name:
cargo run -p tuile-viewer-bundle -- app --out /tmp/apps --name "Tuile Test" \
    --identifier dev.lapoule.tuile.viewer.test --scheme tuile-test
```

`app` builds the viewer in release, lays the bundle out, writes the
property list, packs the icon from the committed master, signs, and then
checks its own work with the system's `plutil -lint` and
`codesign --verify`. See `examples/wgpu-viewer/macos/README.md` for what the bundle is, what
the signature is worth, and why this is a small program rather than an
installed packaging tool.

The layout, the property list and the icon container are written in Rust
and tested here. The signature is the system's `codesign`: it is the tool
whose opinion the system then asks, and it is one command.

## Where it sits

A binary with no workspace dependency: it builds `tuile-wgpu-viewer` (or a
host's own package) with cargo and lays the result out.

## Flags

| Flag | Commands | Meaning |
|---|---|---|
| `--out DIR` | all | where the bundle or the files go |
| `--name NAME` | `app`, `phone`, `android` | the application's name |
| `--identifier ID` | `app`, `phone`, `android` | its bundle identifier |
| `--package PACKAGE` | `app`, `phone`, `android` | the executable, a cargo package |
| `--scheme SCHEME` | `app` | the URL scheme it registers for steering |
| `--binary PATH` | `app` | an executable built elsewhere, instead of building one |
| `--icon PNG` | `app` | a 1024-pixel PNG, in place of the project's icon |
| `--credential VARIABLE` | `phone` | a credential bundled for a developer's own device |
| `--credential-title TITLE` | `phone` | its title |

Run without a command, it prints the same usage.

## Documentation

- [`docs/16-embedding-the-viewer.md`](https://github.com/lapoule-dev/tuile/blob/main/docs/16-embedding-the-viewer.md)
