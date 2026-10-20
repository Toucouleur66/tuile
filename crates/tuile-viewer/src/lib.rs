// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The interactive globe viewer, as a library — on a desktop, a phone and in a
//! browser.
//!
//! A winit window driving the logical geometry server in **progressive** mode:
//! the server (terrain × imagery) runs off the window's thread — on a tokio
//! runtime on a desktop and a phone, in a Web Worker in a browser — and the
//! window sends the camera every frame and pumps decoded tiles to the GPU as
//! they arrive, refining as you orbit and zoom.
//!
//! # One application, two shells
//!
//! The application itself — the `app` module's frame step, gestures, loading
//! indicator, lifecycle, steering — is shared by every target, and so are its
//! renderer (`tuile-wgpu`), its controls (`tuile-ui`) and its camera
//! (`tuile-camera`). What differs is the *shell* around it: where the session
//! is brought up and what it is made of. The desktop and the phone are the
//! `native` shell; the browser is the `web` shell, a WebGPU or WebGL2 canvas
//! with the geometry server in a worker, exported as an ES module. Both hand
//! the application the same session handles under the same names, so no line
//! of the application asks which one it runs in. See
//! `docs/explorations/web-native-convergence.md`.
//!
//! Everything a person sees is here — the window, the camera and its controls,
//! the start flags, the steering URLs, the scripting dictionary, the journal —
//! and none of it knows where the ground comes from. A binary is a
//! [`ViewerHost`]: who the application is, which layers it offers, and a
//! [`Sources`] to fetch them through. See [`embed`] for the three answers and
//! an example, and `docs/16-embedding-the-viewer.md` for the walk-through;
//! `examples/wgpu-viewer` is the host that answers with the public connectors.
//!
//! Drag: orbit. Right-drag: pan. Wheel: zoom. W: wireframe. F: freeze. Esc: quit.
//! N, or a click on the compass ring: north up, about the point at the centre
//! of the view. L: centre on the current location, at the present altitude.
//!
//! The session opens straight down on France from 2 000 km. These flags move
//! that, and are the only flags there are (`--help` lists them with the keys):
//!
//! ```text
//! --lon <deg> --lat <deg>   where the eye is, east and north positive
//! --altitude <m>            its height above the ellipsoid, 1 to 1e8
//! --heading <deg>           where it looks, 0 = north, clockwise
//! --pitch <deg>             how far below the horizon, 90 = straight down
//! --here                    over the current location, instead of --lon/--lat
//! ```
//!
//! C copies a link to the present view. A running viewer can be steered by
//! other programs with URLs of the same form — `tuile://goto?…`, `north`,
//! `here`, `view?…`: see `steer` for the vocabulary and what it cannot do. The
//! application bundle (`macos/README.md`) registers the scheme and carries a
//! scripting dictionary, through which a script reads the view — position,
//! attitude, target, range, tile counts: see `snapshot` — and commands it.
//!
//! The current location comes from the operating system's location service
//! (macOS, cargo feature `current-location`, on by default), which asks the
//! person before it answers; elsewhere `--here` and L say it is not available
//! and change nothing. The coordinates are never logged or written anywhere.
//!
//! `TUILE_RECORD=path.mcap` writes the camera path; `TUILE_REPLAY=path.mcap`
//! flies it again exactly. The format and the replay live in the `tuile-tape`
//! crate.
//!
//! With the `control` feature — tests and debug builds only — `control` runs
//! the same application headless, steps it frame by frame and reads it back:
//! see that module.

mod app;
mod backdrop;
mod desktop;
mod device;
pub mod embed;
/// What an embedding application may do with each frame before it is drawn:
/// the camera it is drawn from, and the views streamed for it. `host` below
/// is the machine the viewer runs on, which is a different thing entirely.
pub mod frame_host;
mod gate;
mod globe;
mod host;
mod journal;
mod launch;
mod location;
mod menu;
mod profile;
mod recording;
mod session;
mod signals;
// Only a script asks; without the desktop glue the answers have no reader.
#[cfg_attr(
    not(all(target_os = "macos", feature = "application")),
    allow(dead_code)
)]
mod snapshot;
mod start;
mod steer;

#[cfg(feature = "control")]
pub mod control;

// The control API is native: a page is driven through its own debug
// namespace, and has no thread a socket could be served from.
#[cfg(all(feature = "control", target_arch = "wasm32"))]
compile_error!("the `control` feature is native; a browser's viewer is driven from its page");

// Never in an application that ships to a phone: a socket that moves the view
// is a debug build's, and an iOS release build that has one is refused here
// rather than found later.
#[cfg(all(feature = "control", target_os = "ios", not(debug_assertions)))]
compile_error!("the `control` feature is for tests and debug builds; an iOS release has none");

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(not(target_arch = "wasm32"))]
use native as shell;
#[cfg(not(target_arch = "wasm32"))]
pub use native::main;

/// The Android entry: a host's `android_main` hands its activity and itself
/// to [`android::main`]. See `android/README.md`.
#[cfg(target_os = "android")]
pub mod android {
    pub use crate::native::android::main;
    pub use winit::platform::android::activity::AndroidApp;
}

// The browser's options are read and checked on the host too, by its tests.
#[cfg(any(target_arch = "wasm32", test))]
mod web;
#[cfg(target_arch = "wasm32")]
use web as shell;

/// The door a Rust host comes in by on the web: the viewer on a canvas, with
/// an application owning each frame ([`frame_host`]).
///
/// The function rather than the module: what a host is promised is this call,
/// and the shell around it is the viewer's own business.
#[cfg(target_arch = "wasm32")]
pub use web::page::start_with_host;

/// The worker entries a host's own module must re-export.
///
/// A host that starts the viewer with [`start_with_host`] *is* the wasm
/// module the page loads, so the package's `worker.js` and
/// `decode-worker.js` import **its** name, not the viewer's — and they call
/// these. Re-exporting them is how a host's module carries them; they are
/// `#[wasm_bindgen]` already, so naming them here is enough for the bindings
/// to appear in the host's own JS.
#[cfg(target_arch = "wasm32")]
pub use web::decode::run_decode_worker;
#[cfg(target_arch = "wasm32")]
pub use web::worker::run_worker;

pub use embed::{
    credential, renamed_by_environment, Identity, ImageryChoice, MissingCredential, Sources,
    TerrainChoice, ViewerHost,
};
