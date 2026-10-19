// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Where a frame goes.
//!
//! A window's surface, as every session has always drawn into — or, for the
//! control API's headless viewer, textures of the session's own, one per eye,
//! read back after every frame. Nothing else differs between the two: the
//! same frame is eased, streamed, resolved and painted, and only the last
//! step — a present, or a copy — is the screen's.

use std::sync::Arc;
use winit::window::Window;

use tuile_wgpu::GpuContext;

/// A window and its surface.
pub(super) struct Windowed {
    pub(super) window: Arc<Window>,
    /// Kept to build the surface again when the application comes back from
    /// the background, where a phone's surface is let go of.
    pub(super) instance: wgpu::Instance,
    /// `None` while released: see [`super::lifecycle`].
    pub(super) surface: Option<wgpu::Surface<'static>>,
}

/// Where the frame goes: see the module.
pub(super) enum Screen {
    Window(Windowed),
    #[cfg(feature = "control")]
    Offscreen(super::offscreen::Offscreen),
}

impl Screen {
    /// Asks for a frame on the next turn of the loop.
    pub(super) fn request_redraw(&self) {
        match self {
            Self::Window(w) => w.window.request_redraw(),
            #[cfg(feature = "control")]
            Self::Offscreen(o) => o.request_redraw(),
        }
    }

    pub(super) fn set_title(&self, title: &str) {
        match self {
            Self::Window(w) => w.window.set_title(title),
            #[cfg(feature = "control")]
            Self::Offscreen(_) => {}
        }
    }

    /// Puts a window held shut since the start on screen, with a frame asked
    /// for.
    pub(super) fn show(&self) {
        match self {
            Self::Window(w) => w.window.set_visible(true),
            #[cfg(feature = "control")]
            Self::Offscreen(_) => {}
        }
        self.request_redraw();
    }

    /// Whether there is nothing to draw into: a phone's surface let go of in
    /// the background.
    pub(super) fn is_released(&self) -> bool {
        match self {
            Self::Window(w) => w.surface.is_none(),
            #[cfg(feature = "control")]
            Self::Offscreen(o) => o.is_released(),
        }
    }

    /// The safe area in device pixels, relative to the drawable: left, top,
    /// width, height.
    pub(super) fn safe_area(&self, size: (u32, u32)) -> (f64, f64, f64, f64) {
        match self {
            Self::Window(w) => super::setup::safe_area(&w.window, size),
            #[cfg(feature = "control")]
            Self::Offscreen(_) => (0.0, 0.0, f64::from(size.0), f64::from(size.1)),
        }
    }

    /// Fits what the frame is drawn into to `size`.
    pub(super) fn configure(
        &mut self,
        gpu: &GpuContext,
        format: tuile_wgpu::SurfaceFormat,
        size: (u32, u32),
    ) {
        match self {
            Self::Window(w) => {
                if let Some(surface) = w.surface.as_ref() {
                    super::setup::configure(surface, gpu, format, size);
                }
            }
            #[cfg(feature = "control")]
            Self::Offscreen(o) => o.resize(gpu, size),
        }
    }

    /// Lets go of what the frame is drawn into.
    pub(super) fn release(&mut self) {
        match self {
            Self::Window(w) => w.surface = None,
            #[cfg(feature = "control")]
            Self::Offscreen(o) => o.release(),
        }
    }

    /// Makes it again; says whether there is one now.
    pub(super) fn restore(&mut self, gpu: &GpuContext, size: (u32, u32)) -> Result<(), String> {
        match self {
            Self::Window(w) => {
                // A window's surface is sized by its configuration, after.
                let _ = (gpu, size);
                let surface = w
                    .instance
                    .create_surface(w.window.clone())
                    .map_err(|e| e.to_string())?;
                w.surface = Some(surface);
                Ok(())
            }
            #[cfg(feature = "control")]
            Self::Offscreen(o) => {
                o.restore(gpu, size);
                Ok(())
            }
        }
    }
}
