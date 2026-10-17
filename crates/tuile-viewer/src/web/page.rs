// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The page side: a viewer on a canvas, and the handle the package's
//! JavaScript holds.
//!
//! A browser has one event loop per page, and winit runs it for good once it
//! is started. So the loop is started with the first viewer and kept: a
//! viewer destroyed is taken out of it, and the next one created is handed to
//! it ([`Pages`]). One viewer at a time; a second one created while the first
//! lives replaces it.

use std::cell::RefCell;
use std::rc::Rc;

use tuile_camera::CameraController;
use wasm_bindgen::prelude::*;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::platform::web::{EventLoopExtWebSys, WindowAttributesExtWebSys};
use winit::window::{Window, WindowId};

use super::options::{Options, ProfileChoice};
use super::session::{self, Plan};
use crate::app::{App, ViewerConfig};
use crate::launch::Retries;
use crate::profile::Profile;
use crate::start::StartView;
use crate::steer::{Command, Goto, Request};

/// What the page's handle asks of the loop.
#[derive(Debug, Clone, Copy)]
enum Wake {
    /// A viewer is waiting to be taken in.
    Adopt,
    /// A request is queued, or the host wants a frame: now, rather than at
    /// the next wake-up.
    Poke,
    /// The host says the view is off screen, or back.
    Hidden(bool),
    /// End the viewer and let go of everything it holds.
    Destroy,
}

thread_local! {
    /// The loop, once started: it is never started twice.
    static LOOP: RefCell<Option<EventLoopProxy<Wake>>> = const { RefCell::new(None) };
    /// A viewer created and not yet taken in by the loop.
    static WAITING: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// The page's viewer, as the package holds it. The package wraps it in the
/// typed API it documents; this is the narrow, untyped seam underneath.
#[wasm_bindgen]
pub struct WebViewer {
    proxy: EventLoopProxy<Wake>,
}

/// Creates a viewer on `canvas`.
///
/// `options` is the package's normalised options object; `make_worker`
/// returns a new worker running the package's worker script; `token` the catalog's
/// bearer, kept in this page and its worker only; `events` is called as
/// `events(kind, detail)`. Fails with a sentence when the options are wrong
/// or the page cannot run an event loop.
#[wasm_bindgen(js_name = startViewer)]
pub fn start_viewer(
    canvas: web_sys::HtmlCanvasElement,
    options: JsValue,
    make_worker: js_sys::Function,
    token: Option<String>,
    events: js_sys::Function,
) -> Result<WebViewer, JsError> {
    super::log::install();
    let options: Options = serde_wasm_bindgen::from_value(options)
        .map_err(|e| JsError::new(&format!("options: {e}")))?;
    let (_, layers) = options.sources.choices().map_err(|e| JsError::new(&e))?;
    // Per viewer, for the viewer's life and a little past it: a few strings.
    let layers: &'static [crate::embed::ImageryChoice] = Box::leak(layers.into_boxed_slice());
    let view = options
        .view
        .map(super::options::ViewSpec::view)
        .transpose()
        .map_err(|e| JsError::new(&e))?
        .unwrap_or_default();
    let handheld = options.profile == ProfileChoice::Handheld;
    let profile = Profile::browser(handheld, options.device_memory);
    tracing::info!(
        handheld,
        resident_mib = profile.resident_budget_bytes >> 20,
        backend = ?options.backend,
        "browser profile"
    );
    super::system::connect(events);
    let plan = Rc::new(Plan {
        make_worker,
        sources: options.sources.clone(),
        token,
        profile: options.profile,
        device_memory: options.device_memory,
    });
    let launch = session::start(&plan);
    let retries = Retries::new(move || session::start(&plan));
    let config = ViewerConfig {
        controller: CameraController::new(view.camera()).with_min_altitude(1.0),
        title: options.name,
        layers,
        layer: 0,
        here: None,
        requests: crate::desktop::listen(),
        remembers: false,
        lit_at_unix_seconds: web_time::SystemTime::now()
            .duration_since(web_time::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64()),
        pinned_level: Some(profile.pinned_level),
        profile,
        retries: Some(retries),
        window: Window::default_attributes()
            .with_canvas(Some(canvas))
            // The wheel zooms the globe, not the page around it.
            .with_prevent_default(true),
        controls: options.ui,
        backends: Some(options.backend.backends()),
    };
    let app = App::new(config, launch);
    WAITING.with(|w| *w.borrow_mut() = Some(app));
    let proxy = match LOOP.with(|l| l.borrow().clone()) {
        Some(proxy) => {
            proxy
                .send_event(Wake::Adopt)
                .map_err(|_| JsError::new("the page's event loop has stopped"))?;
            proxy
        }
        None => {
            let event_loop = EventLoop::<Wake>::with_user_event()
                .build()
                .map_err(|e| JsError::new(&format!("event loop: {e}")))?;
            event_loop.set_control_flow(ControlFlow::Wait);
            let proxy = event_loop.create_proxy();
            LOOP.with(|l| *l.borrow_mut() = Some(proxy.clone()));
            event_loop.spawn_app(Pages { app: None });
            proxy
        }
    };
    Ok(WebViewer { proxy })
}

#[wasm_bindgen]
impl WebViewer {
    /// Puts the eye at `lon`, `lat` and `altitude`, looking `heading`
    /// clockwise from north and `pitch` below the horizon — or glides there
    /// over `seconds`, when given.
    #[wasm_bindgen(js_name = goTo)]
    pub fn go_to(
        &self,
        lon: f64,
        lat: f64,
        altitude: f64,
        heading: Option<f64>,
        pitch: Option<f64>,
        seconds: Option<f64>,
    ) -> Result<(), JsError> {
        let checked = |name: &str, value: f64| crate::start::checked(name, value);
        let goto = Goto {
            lon: Some(checked("lon", lon).map_err(|e| JsError::new(&e))?),
            lat: Some(checked("lat", lat).map_err(|e| JsError::new(&e))?),
            altitude: Some(checked("altitude", altitude).map_err(|e| JsError::new(&e))?),
            heading: heading
                .map(|h| checked("heading", h))
                .transpose()
                .map_err(|e| JsError::new(&e))?,
            pitch: pitch
                .map(|p| checked("pitch", p))
                .transpose()
                .map_err(|e| JsError::new(&e))?,
            seconds: seconds.filter(|s| s.is_finite() && *s > 0.0),
        };
        crate::desktop::queue(Request::Do(Command::Goto(goto)));
        self.wake(Wake::Poke)
    }

    /// The view the last frame was drawn from: `{lon, lat, altitude,
    /// heading, pitch}`, or `undefined` before the first frame.
    #[wasm_bindgen(js_name = getView)]
    pub fn get_view(&self) -> JsValue {
        let Some(snapshot) = crate::snapshot::current() else {
            return JsValue::UNDEFINED;
        };
        let StartView {
            lon,
            lat,
            altitude,
            heading,
            pitch,
        } = snapshot.view;
        let object = js_sys::Object::new();
        for (key, value) in [
            ("lon", lon),
            ("lat", lat),
            ("altitude", altitude),
            ("heading", heading),
            ("pitch", pitch),
        ] {
            let _ = js_sys::Reflect::set(&object, &key.into(), &value.into());
        }
        object.into()
    }

    /// The graphics backend the viewer draws through — `webgpu` or `gl` —
    /// once it does.
    #[wasm_bindgen(getter)]
    pub fn backend(&self) -> Option<String> {
        super::system::backend()
    }

    /// Draws a frame now: after a change of size no layout saw, say.
    pub fn redraw(&self) -> Result<(), JsError> {
        self.wake(Wake::Poke)
    }

    /// Stops drawing and holds the loads, as a hidden tab does.
    pub fn pause(&self) -> Result<(), JsError> {
        self.wake(Wake::Hidden(true))
    }

    /// Draws and loads again.
    pub fn resume(&self) -> Result<(), JsError> {
        self.wake(Wake::Hidden(false))
    }

    /// Ends the viewer: the GPU device, the surface, the worker and every
    /// listener are let go of. The canvas stays the page's.
    pub fn destroy(&self) -> Result<(), JsError> {
        super::system::disconnect();
        self.wake(Wake::Destroy)
    }

    fn wake(&self, wake: Wake) -> Result<(), JsError> {
        self.proxy
            .send_event(wake)
            .map_err(|_| JsError::new("the page's event loop has stopped"))
    }
}

/// The page's event loop: the viewer of the moment, if there is one.
struct Pages {
    app: Option<App>,
}

impl Pages {
    /// Takes in the viewer waiting, ending the one before it.
    fn adopt(&mut self, event_loop: &ActiveEventLoop) {
        let Some(mut app) = WAITING.with(|w| w.borrow_mut().take()) else {
            return;
        };
        if let Some(mut old) = self.app.take() {
            old.end();
        }
        ApplicationHandler::resumed(&mut app, event_loop);
        self.app = Some(app);
    }
}

impl ApplicationHandler<Wake> for Pages {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        match self.app.as_mut() {
            Some(app) => ApplicationHandler::resumed(app, event_loop),
            None => self.adopt(event_loop),
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, wake: Wake) {
        match wake {
            Wake::Adopt => self.adopt(event_loop),
            // `about_to_wait` drains the queue; the frame shows the result.
            Wake::Poke => {
                if let Some(app) = self.app.as_mut() {
                    app.draw_soon();
                }
            }
            Wake::Hidden(hidden) => {
                if let Some(app) = self.app.as_mut() {
                    app.hidden(hidden);
                }
            }
            Wake::Destroy => {
                if let Some(mut app) = self.app.take() {
                    app.ended_by = Some("the page destroyed the viewer");
                    app.end();
                }
                event_loop.set_control_flow(ControlFlow::Wait);
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if let Some(app) = self.app.as_mut() {
            app.window_event(event_loop, id, event);
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        match self.app.as_mut() {
            Some(app) => app.about_to_wait(event_loop),
            // Nothing to draw until the next viewer: sleep until woken.
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }

    fn suspended(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(app) = self.app.as_mut() {
            app.suspended(event_loop);
        }
    }

    fn exiting(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(app) = self.app.as_mut() {
            app.exiting(event_loop);
        }
    }
}
