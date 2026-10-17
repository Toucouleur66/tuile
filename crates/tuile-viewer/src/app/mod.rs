// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The winit application: owns the window, surface and render loop, feeds
//! pixel gestures to the render-agnostic [`tuile_camera::CameraController`] and
//! drives the geometry stream through the [`ContentPump`].

mod command;
mod demand;
mod frame;
mod gestures;
mod imagery;
mod input;
mod lifecycle;
mod loading;
mod pacing;
mod readout;
mod script;
mod setup;
mod stall;
mod tape;
mod touch;
mod warmup;

use std::sync::Arc;
use tuile_camera::CameraController;
use tuile_ui::NavWidget;
use tuile_wgpu::{ContentPump, GpuContext, OverlayRenderer, TileRenderer};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::window::{Window, WindowId};

/// How much air to put between the eye and the ground.
///
/// 1.0 is the physical atmosphere, and the physical atmosphere is the right
/// answer: Earth's own haze is already the cue people read, and anything above
/// it starts hiding the ground it is meant to place. This sat at 1.6 while the
/// optical depth was computed wrongly, which is the usual way a fudge factor
/// gets in — it was compensating for a bug, not for the physics.
const ATMOSPHERE_STRENGTH: f32 = 1.0;

/// How much of a surface's colour survives facing away from the light.
///
/// Not a physical quantity — it stands in for every bounce the single-bounce
/// lighting above does not model. At 0 an unlit slope is pure black, which no
/// daylit terrain ever is; at 1 the shading disappears and with it every hint of
/// relief the mesh carries.
const AMBIENT: f32 = 0.5;

/// Everything known before the window exists.
///
/// The session itself — the stream, the globe's handles — is not here: it
/// arrives through a [`crate::launch::Launch`], at once on a desktop and a few
/// seconds into the window's life on a phone.
pub struct ViewerConfig {
    pub controller: CameraController,
    pub title: String,
    /// The host's imagery layers, and which of them the session opens on.
    pub layers: &'static [crate::embed::ImageryChoice],
    pub layer: usize,
    /// Set by `--here`: ask for the current location at start, and place the
    /// view like this when it is known.
    pub here: Option<crate::location::Placement>,
    /// Steering from other programs, as it arrives: URLs still unread and
    /// untrusted (see [`crate::steer::parse_url`]), and a script's commands.
    pub requests: std::sync::mpsc::Receiver<crate::steer::Request>,
    /// Whether to leave the last view for the next session: see
    /// `App::remember_the_view`.
    pub remembers: bool,
    /// The instant the scene is lit for, UTC seconds since the Unix epoch —
    /// which sets where the sun is, and so where the terminator falls.
    pub lit_at_unix_seconds: f64,
    /// The level held in memory for the session, if any.
    pub pinned_level: Option<u32>,
    /// How hard the session may push the machine.
    pub(crate) profile: crate::profile::Profile,
    /// The session's async runtime, for flushing the store on the way out.
    pub runtime: tokio::runtime::Handle,
}

/// Where the on-screen controls live: the part of the frame not under a
/// notch, a rounded corner or the home indicator, and how many device pixels
/// one of the controls' units is.
///
/// On a desktop that is the whole frame at one pixel per unit — the controls
/// look exactly as they always did. On a phone it is the safe area, at the
/// display's scale, so the compass is a thumb's width and not a third of one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct UiFrame {
    /// The safe area's top-left corner, in device pixels.
    pub origin: (f64, f64),
    /// Device pixels per control unit.
    pub scale: f64,
    /// The safe area's size, in control units.
    pub size: (f64, f64),
}

impl UiFrame {
    /// From device pixels to the controls' units.
    pub(super) fn to(&self, px: (f64, f64)) -> (f64, f64) {
        (
            (px.0 - self.origin.0) / self.scale,
            (px.1 - self.origin.1) / self.scale,
        )
    }

    /// From the controls' units to device pixels.
    pub(super) fn from(&self, at: [f32; 2]) -> [f32; 2] {
        [
            (f64::from(at[0]) * self.scale + self.origin.0) as f32,
            (f64::from(at[1]) * self.scale + self.origin.1) as f32,
        ]
    }

    /// The safe area's centre, in device pixels.
    pub(super) fn centre(&self) -> (f64, f64) {
        (
            self.origin.0 + self.size.0 * self.scale * 0.5,
            self.origin.1 + self.size.1 * self.scale * 0.5,
        )
    }
}

struct Active {
    window: Arc<Window>,
    /// Kept to build the surface again when the application comes back from
    /// the background, where a phone's surface is let go of.
    instance: wgpu::Instance,
    /// `None` while released: see [`lifecycle`].
    surface: Option<wgpu::Surface<'static>>,
    surface_format: wgpu::TextureFormat,
    gpu: GpuContext,
    renderer: TileRenderer,
    overlay: OverlayRenderer,
    pump: ContentPump,
    /// The multisampled colour target and its depth buffer. The surface itself
    /// has one sample, so the scene is drawn here and *resolved* into it —
    /// which is what turns the staircase on the limb into a smooth edge.
    targets: tuile_wgpu::FrameTargets,
    /// The frame in **device** pixels: what the render targets are sized to and
    /// what the imagery resolution is chosen against.
    size: (u32, u32),
    /// Device pixels per point, as the display reports it — 2 on a Retina panel.
    ///
    /// Geometry and imagery are chosen for different units, and conflating them
    /// costs both ways. The mesh wants device pixels: that is what a triangle is
    /// actually rasterised into, and a coarse silhouette shows. Imagery wants
    /// *points*, because a map tile is drawn for them — its labels and its line
    /// weights are sized in the same units a stylesheet is. Asking for imagery at
    /// device density on a Retina display fetches one level deeper than the
    /// screen can show, which is four times the tiles for type at half the size
    /// it was drawn to be read at.
    scale: f64,
    /// The safe area, in device pixels: left, top, width, height. The whole
    /// frame on a desktop.
    safe: (f64, f64, f64, f64),
    /// Where a traced frame is copied before being read back.
    readback: tuile_wgpu::Readback,
    /// A whole-planet shell, five hundred metres below the ellipsoid, drawn
    /// before the tiles.
    ///
    /// The guarantee that ground is never bare, and it is a guarantee rather
    /// than a mitigation: whatever the traversal selects, whatever arrives
    /// late, whatever is evicted, this is already drawn behind it. Every
    /// attempt to promise the same thing upstream held only for the cases that
    /// had been thought of.
    shell: tuile_wgpu::PreparedTile,
}

/// The diagnostic views, in the order `D` cycles them.
///
/// Each one removes exactly one explanation for a black pixel, so pressing `D`
/// twice narrows four candidates to one. The order is deliberate: cheapest
/// question first.
///
/// | view | black here means |
/// |---|---|
/// | normal | one of four things, which is the problem |
/// | unlit | not the sun and not the air — they are switched off |
/// | coverage | never black: magenta = no imagery reached this pixel |
/// | geometry | nothing was drawn at all — a mesh is missing |
///
/// So: black in *normal* that turns to picture in *unlit* is a lighting or
/// atmosphere fault. Black that survives *unlit* and shows magenta in
/// *coverage* is missing imagery. Black that survives *unlit*, is green in
/// *coverage*, and is lit in *geometry* is imagery that is genuinely black —
/// look for the `BLACK IMAGERY` warning. Black that stays black in *geometry*
/// is no mesh at all, and belongs to traversal or upload, never to texturing.
pub(crate) const DIAGNOSTICS: [(&str, &str); 4] = [
    ("normal", "the real picture"),
    ("unlit", "imagery only; sun and air off"),
    (
        "coverage",
        "magenta = no imagery here, green = one layer, blue = several",
    ),
    ("geometry", "flat lit surface; black = no mesh drawn"),
];

pub struct App {
    controller: CameraController,
    detail: tuile_planetary::ImageryDetail,
    layer_budget: tuile_planetary::LayerBudget,
    title: String,
    /// Where the sun is for the instant the scene is lit at. Resolved once: the
    /// hour is fixed for a session, so recomputing the ephemeris every frame
    /// would answer the same question sixty times a second.
    sun: tuile_atmosphere::Sun,
    /// Everything that only exists once there is a window. `None` until
    /// `resumed`, and that is the only reason this is an `Option`.
    active: Option<Active>,
    /// The session, once it is up: its stream and the globe's handles.
    session: Option<crate::launch::Started>,
    /// The session on its way up; `None` once it has arrived or failed.
    launch: Option<crate::launch::Launch>,
    /// Why the session could not start, once that is known.
    failed: Option<String>,
    profile: crate::profile::Profile,
    runtime: tokio::runtime::Handle,
    lifecycle: lifecycle::Lifecycle,
    loading: loading::Loading,
    demand: demand::Demand,
    touches: touch::Touches,
    /// A scripted hand, when `TUILE_TOUCH_SCRIPT` names one.
    script: Option<script::TouchScript>,
    /// The last frame's resolution, which the next frame's indicator reads.
    resolution: Option<tuile_wgpu::Resolution>,
    /// Tiles uploaded on the last frame.
    uploaded: usize,
    /// The device's temperature and power saving, and when they were read.
    thermal: crate::profile::Thermal,
    low_power: bool,
    device_read: Option<std::time::Instant>,

    // The rest is grouped by the module that owns it, so that a field and the
    // code that reads it are never in different files. Each of these is defined
    // beside its behaviour rather than here.
    pointer: input::Pointer,
    views: input::Views,
    recording: tape::Recording,
    warmup: warmup::Warmup,
    pacing: pacing::Pacing,
    watch: pacing::Watch,

    /// The on-screen navigation control. It sees pointer events first and, when
    /// it takes one, the globe must not also act on it.
    nav: NavWidget,
    /// The request for the current location, from the key press or `--here`
    /// to the frame the system's answer arrives on.
    locator: crate::location::Locator,
    /// Whether the view is where it is because the location service put it
    /// there — in which case it is not reported to other programs.
    located: bool,
    /// The imagery: the host's layers, which one is draped, and a change of
    /// layer on its way — see `imagery.rs`.
    imagery: imagery::Imagery,
    /// Steering waiting to be carried out.
    requests: std::sync::mpsc::Receiver<crate::steer::Request>,
    /// Tiles drawn in the last frame, and whether all were the ones selected.
    drawn: (usize, bool),
    /// The last thing said to the person, and when.
    notice: Option<(String, std::time::Instant)>,
    /// What the title bar shows, and when it was last looked at.
    title_shown: String,
    title_at: std::time::Instant,
    /// Why the session is ending, once something has decided that it is.
    pub(crate) ended_by: Option<&'static str>,
    /// Whether the view is written down at the end for the next start.
    remembers: bool,
    /// The window is hidden — another window covers it, the display slept, the
    /// app was minimized. Rendering while occluded leaks GPU memory on Apple
    /// platforms.
    occluded: bool,
    /// The menu-bar readout, if the platform gave us one.
    statusbar: Option<readout::StatusBar>,
    /// The application's menu bar, where the platform has one.
    menu: Option<crate::menu::Installed>,
    /// Running totals, reported once per second and again on the way out.
    stats: crate::session::Stats,
}

impl App {
    /// `launch` brings the session: already arrived on a desktop, on its way
    /// on a phone.
    pub(crate) fn new(mut config: ViewerConfig, launch: crate::launch::Launch) -> Self {
        // The queue is taken out of the configuration: there is one reader.
        let requests = std::mem::replace(&mut config.requests, std::sync::mpsc::channel().1);
        // Asked for here, on the main thread and before the loop turns, so the
        // system's question — if it has one — is up while the globe warms.
        let mut locator = crate::location::Locator::platform();
        if let Some(placement) = config.here {
            locator.request(std::time::Instant::now(), placement);
        }
        let now = std::time::Instant::now();
        let mut app = Self {
            imagery: imagery::Imagery::new(config.layers, config.layer),
            locator,
            located: false,
            requests,
            drawn: (0, false),
            notice: None,
            title_shown: String::new(),
            title_at: now,
            ended_by: None,
            remembers: config.remembers,
            controller: config.controller.clone(),
            detail: tuile_planetary::ImageryDetail::default(),
            layer_budget: tuile_planetary::LayerBudget::default(),
            title: config.title.clone(),
            sun: tuile_atmosphere::Sun::at_unix_seconds(config.lit_at_unix_seconds),
            active: None,
            session: None,
            launch: Some(launch),
            failed: None,
            lifecycle: lifecycle::Lifecycle::new(&config.profile),
            loading: loading::Loading::new(now),
            demand: demand::Demand::default(),
            touches: touch::Touches::default(),
            script: script::TouchScript::from_environment(),
            resolution: None,
            uploaded: 0,
            thermal: crate::profile::Thermal::Nominal,
            low_power: false,
            device_read: None,
            pointer: input::Pointer::default(),
            views: input::Views::default(),
            recording: tape::Recording {
                tape: crate::recording::open_tape(),
                trace: crate::recording::open_trace(),
                finished: false,
            },
            warmup: warmup::Warmup {
                // Only a desktop holds its window shut; a phone opens at once
                // with the loading indicator over the globe.
                holding: config.profile.hold_window_until_warm,
                pinned_level: config.pinned_level,
                since: now,
                last_report: now,
            },
            pacing: pacing::Pacing {
                last_frame: now,
                last_step: None,
                last_seconds: 0.0,
                frames_at_last_log: 0,
                last_heading: None,
            },
            watch: pacing::Watch {
                had_holes: false,
                last_traversals: 0,
                last_altitude: None,
                silent_passes: 0,
                last_log: now,
            },
            nav: NavWidget::new(),
            occluded: false,
            // Built with the window rather than here: a status item wants the
            // main thread and an event loop already running.
            statusbar: None,
            menu: None,
            stats: crate::session::Stats {
                started: Some(now),
                ..crate::session::Stats::default()
            },
            profile: config.profile,
            runtime: config.runtime,
        };
        // A desktop's launch has already arrived: adopt it before the first
        // turn of the loop, as the session always did.
        app.take_the_session();
        app
    }

    /// Takes the session if it has arrived since the last turn of the loop,
    /// or the reason it never will. Never waits.
    fn take_the_session(&mut self) {
        let Some(outcome) = self.launch.as_mut().and_then(crate::launch::Launch::poll) else {
            return;
        };
        self.launch = None;
        match outcome {
            Ok(started) => self.adopt(started),
            Err(why) => {
                crate::journal::note(&format!("session failed: {why}"));
                tracing::error!("the session could not start: {why}");
                crate::desktop::alert(&format!("{} cannot start yet", self.title), &why);
                self.failed = Some(why);
            }
        }
    }

    /// Takes over a session that has come up: its stream, the relief the
    /// camera stands on, the imagery detail the loader reads, how many layers
    /// a drape may carry.
    fn adopt(&mut self, started: crate::launch::Started) {
        self.controller = self.controller.clone().with_ground(started.heights.clone());
        self.detail = started.detail.clone();
        self.layer_budget = started.layer_budget.clone();
        self.fit_the_layer_budget();
        // Away already: the loads wait until the application is back.
        if self.lifecycle.phase() != lifecycle::Phase::Active {
            started.gate.shut();
        }
        if self.session.is_none() && !self.profile.hold_window_until_warm {
            tracing::info!(
                seconds = self
                    .stats
                    .started
                    .map_or(0.0, |t| t.elapsed().as_secs_f32()),
                "session up; the window was drawing all along"
            );
        }
        self.session = Some(started);
        self.demand.poke();
    }

    /// Tells the loader how many imagery layers the device binds, once both
    /// exist.
    fn fit_the_layer_budget(&mut self) {
        let Some(active) = self.active.as_ref() else {
            return;
        };
        self.layer_budget.set_from_device(
            active
                .gpu
                .device
                .limits()
                .max_sampled_textures_per_shader_stage,
        );
    }

    /// Closes the session down: the stream, then the store, then the server.
    ///
    /// The store is closed *before* the server is joined: the server thread
    /// blocks on the runtime, and the runtime waits for the store's blocking
    /// workers — so waiting for the thread first and only then asking the
    /// store to close is a deadlock by construction. A phone, on its way out,
    /// is given seconds for all of it, and does not wait for the server.
    pub fn shut_down(&mut self, handle: &tokio::runtime::Handle) {
        let Some(session) = self.session.take() else {
            return;
        };
        let crate::launch::Started {
            stream,
            server,
            store,
            ..
        } = session;
        drop(stream);
        let patience = if self.profile.hold_window_until_warm {
            std::time::Duration::from_secs(60)
        } else {
            std::time::Duration::from_secs(3)
        };
        crate::settings::close_the_store(store, handle, patience);
        if self.profile.hold_window_until_warm {
            if let Some(server) = server {
                if server.join().is_err() {
                    tracing::error!("the geometry server panicked; its last work is lost");
                }
            }
        }
    }

    /// A one-line summary of the session, for the exit log.
    pub fn report(&self) -> String {
        self.stats.to_string()
    }

    /// Moves the view if the location that was asked for has arrived, and
    /// says how the request ended either way.
    fn take_the_location(&mut self) {
        let now = std::time::Instant::now();
        if let Some(outcome) = self.locator.tick(now, &mut self.controller) {
            self.located |= matches!(outcome, crate::location::Outcome::Centred { .. });
            self.say(&outcome.to_string());
        }
    }

    /// Leaves the view this session ends on for the next one to open on.
    ///
    /// Only for a session started from an icon: from a terminal, the view a
    /// command opens on is what the command says and nothing else. And never
    /// a view centred on the current location — that one is not written
    /// anywhere, and the file from before is removed so that the next start
    /// does not pretend to return to it.
    fn remember_the_view(&self) {
        if !self.remembers {
            return;
        }
        let Some(file) = crate::host::last_view_file(&crate::host::Machine) else {
            return;
        };
        if self.located {
            let _ = std::fs::remove_file(&file);
            return;
        }
        let link = crate::steer::link(&crate::steer::view_of(self.controller.target()));
        if let Some(dir) = file.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&file, format!("{link}\n"));
    }

    /// Tells the person something they asked about, where they are looking.
    ///
    /// In the title bar, beside the position, because that is the one piece of
    /// text this window has and because the log may be going nowhere: a
    /// session started from an icon has no terminal to read. The message
    /// stays a few seconds, or until the next one replaces it.
    fn say(&mut self, message: &str) {
        tracing::info!("{message}");
        let now = std::time::Instant::now();
        self.notice = Some((message.to_owned(), now));
        // Shown on the next turn of the loop, not a quarter of a second on.
        self.title_at = now - std::time::Duration::from_secs(1);
    }

    fn viewport(&self) -> (f64, f64) {
        self.active
            .as_ref()
            .map_or((1920.0, 1080.0), |a| (a.size.0 as f64, a.size.1 as f64))
    }

    /// Where the on-screen controls live this frame. See [`UiFrame`].
    fn ui_frame(&self) -> UiFrame {
        let (w, h) = self.viewport();
        let Some(active) = self.active.as_ref() else {
            return UiFrame {
                origin: (0.0, 0.0),
                scale: 1.0,
                size: (w, h),
            };
        };
        if self.profile.hold_window_until_warm {
            // A desktop: as it always was.
            return UiFrame {
                origin: (0.0, 0.0),
                scale: 1.0,
                size: (w, h),
            };
        }
        let (x, y, sw, sh) = active.safe;
        let scale = active.scale.max(1.0);
        UiFrame {
            origin: (x, y),
            scale,
            size: (sw / scale, sh / scale),
        }
    }
}

impl ApplicationHandler for App {
    /// The first time, the window; after that — on a phone, every time the
    /// application comes back to the front — the lifecycle's way back.
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.active.is_some() {
            let steps = self.lifecycle.activate();
            self.carry_out(steps);
            return;
        }
        self.start_window(event_loop);
    }

    /// A phone's application is about to leave the front.
    fn suspended(&mut self, _event_loop: &ActiveEventLoop) {
        let steps = self.lifecycle.resign();
        self.carry_out(steps);
    }

    /// The system is short of memory: the server lets go of what the view
    /// does not draw. Never what it draws, nor its ancestors.
    fn memory_warning(&mut self, _event_loop: &ActiveEventLoop) {
        tracing::warn!("memory warning; trimming the residency");
        crate::journal::note("memory warning");
        let steps = self.lifecycle.memory_warning();
        self.carry_out(steps);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        self.on_window_event(event_loop, event);
    }

    /// Called once as the loop unwinds, however it was left — the close box,
    /// Escape, or a replay reaching the end of its tape.
    ///
    /// The recording is finished here rather than on `WindowEvent::Destroyed`,
    /// which is not delivered on every platform before the loop returns: the
    /// first session recorded left a zero-byte file, because a container
    /// without its footer is not a file anything will open.
    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        // Written here and not after the loop returns: quitting from the
        // application menu ends the process inside the loop, and what follows
        // `run_app` never runs.
        crate::journal::note(&format!(
            "session ended: {}",
            self.ended_by.unwrap_or("the application was asked to quit")
        ));
        self.close_the_tape();
        self.remember_the_view();
        // A phone's loop never returns to `session`, so this is the last
        // chance to flush the store.
        if !self.profile.hold_window_until_warm {
            let handle = self.runtime.clone();
            self.shut_down(&handle);
        }
    }

    /// Drives the animation: one redraw per loop iteration while the window is
    /// visible. Requesting from here rather than from the `RedrawRequested`
    /// handler is what lets an occluded window stop cleanly — the loop parks on
    /// `Wait` until the next event instead of spinning.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Before anything that can return early: an answer about the current
        // location is taken on whatever turn of the loop it arrives, hidden
        // and warming included.
        self.take_the_session();
        self.take_the_location();
        self.take_the_commands();
        self.take_the_imagery();
        // Also on the turns that draw nothing — hidden and warming, or
        // occluded — so a script always has a view to read.
        self.publish_the_view(None);
        // A flown tape ends the session. Without this a replay leaves a window
        // sitting on its last frame, and the run has to be closed by hand —
        // which is exactly what stops anyone from putting it in a script.
        if self.recording.finished {
            self.ended_by = Some("the replayed tape reached its end");
            tracing::info!("{}", self.report());
            self.close_the_tape();
            event_loop.exit();
            return;
        }
        // Ctrl-C or `kill`: unwind rather than die, so the recording is closed.
        if crate::signals::INTERRUPTED.load(std::sync::atomic::Ordering::Relaxed) {
            self.ended_by = Some("a signal asked it to stop (interrupt, terminate or hang-up)");
            tracing::info!("interrupted; closing the session");
            self.close_the_tape();
            event_loop.exit();
            return;
        }
        // Hidden and warming: the window is not drawing, so nothing else is
        // draining the stream — and the uploads being waited for happen in the
        // pump. Driving it from here is what keeps the wait from being a
        // deadlock.
        if self.warmup.holding {
            self.pump_while_hidden();
            event_loop.set_control_flow(ControlFlow::Poll);
            return;
        }
        // A phone draws on demand, and only in the foreground.
        if self.profile.render_on_demand {
            self.pace_the_frames(event_loop);
            return;
        }
        event_loop.set_control_flow(if self.occluded {
            ControlFlow::Wait
        } else {
            ControlFlow::Poll
        });
        if !self.occluded {
            if let Some(active) = self.active.as_ref() {
                active.window.request_redraw();
            }
        }
    }
}

impl Drop for App {
    /// Last line of defence: a panic unwinding out of the event loop still has
    /// to leave a usable recording. Closing an already-closed tape is a no-op,
    /// so this costs nothing on the ordinary path.
    fn drop(&mut self) {
        self.close_the_tape();
    }
}
