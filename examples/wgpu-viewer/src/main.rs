// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The viewer, over the public connectors: see the library, which holds the
//! host so that Android's build, a library, runs the very same one.
//!
//! ```text
//! CESIUM_ION_TOKEN=... cargo run -p tuile-wgpu-viewer
//! CESIUM_ION_TOKEN=... cargo run -p tuile-wgpu-viewer -- \
//!     --lon 6.86 --lat 45.83 --altitude 6000 --heading 120 --pitch 25
//! ```

fn main() {
    tuile_viewer::main(tuile_wgpu_viewer::Public)
}
