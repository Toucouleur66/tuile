// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Bringing up the window, the adapter, the device and the surface.
//!
//! Runs once. Everything chosen here — the surface format, the sample count,
//! the features asked of the adapter — is a decision the rest of the session
//! cannot revisit, so each one says out loud what it picked and why it might
//! not have got it.

use super::{Active, App};
use glam::DVec3;
use std::sync::Arc;
use tuile_wgpu::{ContentPump, GpuContext, OverlayRenderer, TileRenderer, DEPTH_FORMAT};
use winit::event_loop::ActiveEventLoop;
use winit::window::Window;

impl App {
    /// Creates the window and everything that hangs off it.
    pub(super) fn start_window(&mut self, event_loop: &ActiveEventLoop) {
        self.statusbar = super::readout::StatusBar::new();
        // Here and not earlier: a menu wants the application object the event
        // loop creates, and the main thread.
        self.menu = crate::menu::Installed::install(self.imagery.layers);
        if self.statusbar.is_none() {
            tracing::info!("no status-bar item on this platform; the readout is off");
        }
        // Born hidden, shown once the coarse pyramid is on the GPU.
        //
        // A globe drawn before its floor exists is a globe with nothing behind
        // its holes — the one state this whole design prevents, on screen at
        // the moment someone forms a first impression. See
        // [`App::still_warming`], which also decides when to give up waiting.
        //
        // A phone's window is the exception: the system ends an application
        // that shows nothing, so it opens at once on the globe's shell and the
        // loading indicator — see `loading`.
        let attrs = Window::default_attributes()
            .with_title(self.title.clone())
            .with_visible(!self.profile.hold_window_until_warm);
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        // The drawable is the whole window, edge to edge; on a phone the
        // inner size is only its safe area, which the controls keep to.
        let size = if self.profile.hold_window_until_warm {
            window.inner_size()
        } else {
            window.outer_size()
        };
        let size = (size.width.max(1), size.height.max(1));
        let scale = window.scale_factor().max(1.0);
        // Said out loud, because everything downstream divides by it and a wrong
        // value is invisible: the ground still draws, the imagery is merely a
        // level off and the type a size off, which reads as a rendering opinion
        // rather than a number to check.
        tracing::info!(
            device_px = format!("{}x{}", size.0, size.1),
            points = format!(
                "{:.0}x{:.0}",
                f64::from(size.0) / scale,
                f64::from(size.1) / scale
            ),
            scale,
            "display"
        );

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = instance
            .create_surface(window.clone())
            .expect("create surface");
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: Some(&surface),
        }))
        .expect("adapter");
        // Which GPU, and through which API. Worth a line: every performance
        // question that follows — what a present costs, what a buffer write
        // costs, whether a timing tool applies at all — is answered differently
        // per backend, and guessing which one is in play wastes the whole
        // investigation.
        let info = adapter.get_info();
        tracing::info!(
            backend = %info.backend,
            device = %info.name,
            kind = ?info.device_type,
            driver = %info.driver_info,
            "adapter"
        );
        // What the GPU costs, asked for only where it is on offer.
        //
        // Nothing was requested here at all, so there was no way to time a
        // render pass: every performance question had to be answered from the
        // CPU side, where a frame that is merely *waiting* for the GPU looks
        // exactly like a frame that is idle. Timestamps are what tell those two
        // apart.
        //
        // Intersected with what the adapter actually has rather than named
        // outright, because a hard requirement here would turn a missing
        // stopwatch into a viewer that refuses to start — on someone else's
        // machine, over a diagnostic they were not using.
        let wanted =
            wgpu::Features::TIMESTAMP_QUERY | wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS;
        let features = adapter.features() & wanted;
        // Said out loud either way. A capability that is silently absent is one
        // that gets reported later as a broken tool.
        let missing = wanted - features;
        if missing.is_empty() {
            tracing::info!(?features, "gpu timing available");
        } else {
            tracing::info!(?features, ?missing, "gpu timing partly unavailable");
        }
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("viewer"),
            required_limits: adapter.limits(),
            required_features: features,
            ..Default::default()
        }))
        .expect("device");

        let caps = surface.get_capabilities(&adapter);
        let surface_format = caps
            .formats
            .iter()
            .copied()
            .find(|f| f.is_srgb())
            .unwrap_or(caps.formats[0]);
        let gpu = GpuContext::new(device, queue);
        // The loader has been running against the correctness floor since the
        // stream was built; this is the first moment anything knows what the
        // device will actually bind. On the WebGPU baseline it stays at the
        // floor, on Metal it opens up to the fetch-cost ceiling.
        tracing::info!(
            reported = gpu.device.limits().max_sampled_textures_per_shader_stage,
            "imagery layers a drape may bind"
        );
        let renderer = TileRenderer::new(&gpu, surface_format);
        // The viewer's pass carries depth, so the overlay must declare it too.
        let overlay = OverlayRenderer::new(&gpu, surface_format, Some(DEPTH_FORMAT));
        // The render origin tracks the eye (set each frame); start there.
        let pump = ContentPump::new(self.controller.camera.position);
        let safe = safe_area(&window, size);
        let targets = tuile_wgpu::FrameTargets::new(&gpu, surface_format, size);
        // Built and uploaded once for the whole session.
        let shell = tuile_wgpu::prepare(&gpu, &crate::backdrop::shell_content(), DVec3::ZERO);

        let active = Active {
            window,
            instance,
            surface: Some(surface),
            surface_format,
            gpu,
            renderer,
            overlay,
            pump,
            targets,
            size,
            scale,
            safe,
            readback: tuile_wgpu::Readback::default(),
            shell,
        };
        configure_surface(&active);
        active.window.request_redraw();
        self.active = Some(active);
        // The loader has been running against the correctness floor; this is
        // the first moment anything knows what the device will bind.
        self.fit_the_layer_budget();
        tracing::info!(slots = self.layer_budget.get(), "imagery layers per drape");
    }

    /// Lets go of the surface and shrinks the render targets to nothing: a
    /// phone's application in the background keeps neither.
    pub(super) fn release_the_surface(&mut self) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        active.surface = None;
        active.targets.resize(&active.gpu, (1, 1));
        tracing::info!("surface released");
    }

    /// Builds the surface again, before any frame is drawn into it.
    pub(super) fn restore_the_surface(&mut self) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if active.surface.is_some() {
            return;
        }
        match active.instance.create_surface(active.window.clone()) {
            Ok(surface) => {
                active.surface = Some(surface);
                active.targets.resize(&active.gpu, active.size);
                configure_surface(active);
                tracing::info!("surface restored");
                // Said again on the next turn, over the new surface.
                self.title_shown.clear();
            }
            Err(e) => tracing::error!("the surface could not be made again: {e}"),
        }
    }
}

/// The window's safe area in device pixels, relative to the drawable:
/// left, top, width, height.
pub(super) fn safe_area(window: &Window, size: (u32, u32)) -> (f64, f64, f64, f64) {
    let whole = (0.0, 0.0, f64::from(size.0), f64::from(size.1));
    let (Ok(inner), Ok(outer)) = (window.inner_position(), window.outer_position()) else {
        return whole;
    };
    let inner_size = window.inner_size();
    let (x, y) = (
        f64::from(inner.x - outer.x).max(0.0),
        f64::from(inner.y - outer.y).max(0.0),
    );
    let (w, h) = (
        f64::from(inner_size.width).min(f64::from(size.0) - x),
        f64::from(inner_size.height).min(f64::from(size.1) - y),
    );
    if w <= 0.0 || h <= 0.0 {
        return whole;
    }
    (x, y, w, h)
}

pub(super) fn configure_surface(active: &Active) {
    if let Some(surface) = active.surface.as_ref() {
        configure(surface, &active.gpu, active.surface_format, active.size);
    }
}

/// The same, by parts.
///
/// Taken field by field rather than as a whole `Active` so that a closure
/// needing only the surface can be handed to the backend while another closure
/// holds a mutable borrow of some *other* field — which is exactly what drawing
/// a traced frame does.
pub(super) fn configure(
    surface: &wgpu::Surface<'static>,
    gpu: &GpuContext,
    format: wgpu::TextureFormat,
    size: (u32, u32),
) {
    surface.configure(
        &gpu.device,
        &wgpu::SurfaceConfiguration {
            // COPY_SRC only when a trace is being written: it is free to ask
            // for on every backend that matters, and asking for capabilities a
            // session does not use is how a viewer stops working on someone
            // else's GPU.
            usage: if crate::recording::trace_path().is_some() {
                wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC
            } else {
                wgpu::TextureUsages::RENDER_ATTACHMENT
            },
            format,
            width: size.0,
            height: size.1,
            present_mode: wgpu::PresentMode::AutoVsync,
            desired_maximum_frame_latency: 2,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![],
        },
    );
}
