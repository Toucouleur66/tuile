// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The viewer headless, and the API that drives it — for tests, and for
//! debug builds a device is driven through.
//!
//! # The same viewer
//!
//! [`Headless`] is the application every window runs: the same frame (eased,
//! streamed, resolved, painted), the same pump and upload budget, the same
//! profile, lifecycle and steering. Only the ends differ:
//!
//! - **the screen** is a texture per eye, read back after every frame and
//!   scanned for ground left the clear colour — one eye for a window, two
//!   for a headset's rig ([`Eyes`]);
//! - **the clock** is stepped: a frame is one [`Headless::step`], and the
//!   interval the camera eases by is the run's refresh, exactly;
//! - **the network** is a [`Fixture`] globe, and the geometry server is
//!   polled between two frames until it has nothing left to do, every load
//!   inline — so what reaches a frame is a function of the frames before it,
//!   and two runs of one script draw the same frames;
//! - **the device** is said, not asked: a desktop's numbers or a phone's,
//!   from a memory a test states ([`Device`]).
//!
//! # Reading it
//!
//! Through the numbers the engine publishes — `tuile_core::metrics`, the
//! series `tuile-metrics` exports — never through accessors of its own. A
//! [`State`] is one step's: the view a script reads ([`crate::snapshot`]),
//! the metrics as a [`Snapshot`] since the run began, what each eye's scan
//! found, what the network was asked, and what every [`Observer`] added.
//!
//! # The session log
//!
//! A run may write itself down as one MCAP file ([`Start::log`]), on the
//! timeline a tape keeps: the camera on `/camera` (a tape, which
//! `TUILE_REPLAY` flies again), each step's [`State`] on `/state`, each
//! observer on its own topic, and — when asked — every eye's picture on
//! `/frame` (one eye; `tuile-scan-black` reads it) or `/eye/<n>/frame`. Two
//! runs of one script write the same messages ([`tuile_tape::messages`]).
//!
//! # Driving it
//!
//! [`Action`]s — a jump or a glide of the eye (a steering command, the queue
//! a URL arrives on), a finger, the wheel, a resize, the density, memory
//! pressure, the background — one at a time with [`Headless::act`], or as a
//! [`Series`] timed in frames ([`Headless::play`]); a tape with
//! [`Headless::fly`]. [`patterns`] holds the motion patterns of the budget's
//! specification as series.
//!
//! # The socket
//!
//! [`Headless::listen`] serves the same commands to other programs: JSON, one
//! request a line, on a loopback address only — see [`socket`] for the
//! vocabulary, version 1.
//!
//! One headless viewer per process at a time: the numbers it reads are the
//! process's.

pub mod fixture;
mod log;
pub mod patterns;
mod series;
pub mod socket;

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

pub use fixture::{Fixture, Store};
pub use log::Observer;
pub use series::{Action, Finger, Pressure, Series};
pub use tuile_core::metrics::Snapshot;

use crate::app::{App, Clock, Rig, Stepper, Turn, ViewerConfig};
use crate::steer::{Command, Goto, Request};

/// Where the eye is and how it looks, as the command line and a steering URL
/// say it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    /// Degrees, east positive.
    pub lon: f64,
    /// Degrees, north positive.
    pub lat: f64,
    /// Metres above the ellipsoid.
    pub altitude: f64,
    /// Degrees clockwise from north.
    pub heading: f64,
    /// Degrees below the horizon; 90 looks straight down.
    pub pitch: f64,
}

impl Default for View {
    /// Straight down at France from 2 000 km: what the viewer opens on.
    fn default() -> Self {
        crate::start::StartView::default().into()
    }
}

impl From<crate::start::StartView> for View {
    fn from(v: crate::start::StartView) -> Self {
        Self {
            lon: v.lon,
            lat: v.lat,
            altitude: v.altitude,
            heading: v.heading,
            pitch: v.pitch,
        }
    }
}

impl From<View> for crate::start::StartView {
    fn from(v: View) -> Self {
        Self {
            lon: v.lon,
            lat: v.lat,
            altitude: v.altitude,
            heading: v.heading,
            pitch: v.pitch,
        }
    }
}

impl View {
    /// The steering command that puts the eye here: at once, or gliding over
    /// `seconds`.
    pub(crate) fn command(&self, seconds: Option<f64>) -> Command {
        Command::Goto(Goto {
            lon: Some(self.lon),
            lat: Some(self.lat),
            altitude: Some(self.altitude),
            heading: Some(self.heading),
            pitch: Some(self.pitch),
            seconds,
        })
    }

    pub(crate) fn to_json(self) -> serde_json::Value {
        serde_json::json!({
            "lon": self.lon,
            "lat": self.lat,
            "altitude": self.altitude,
            "heading": self.heading,
            "pitch": self.pitch,
        })
    }
}

/// The eyes a frame is drawn for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Eyes {
    /// One, as a window has.
    Mono,
    /// A headset's pair, `separation` metres apart — 0.063 for a person.
    Stereo { separation: f64 },
}

impl Eyes {
    fn rig(self) -> Rig {
        match self {
            Self::Mono => Rig::MONO,
            Self::Stereo { separation } => Rig {
                eyes: 2,
                separation,
            },
        }
    }

    /// How many.
    pub fn count(self) -> usize {
        self.rig().eyes
    }
}

/// The machine the run is to behave as, said rather than asked.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Device {
    /// The desktop application, as it always runs.
    Desktop,
    /// A phone or a tablet with `memory` bytes, `available` of them still
    /// left to the process when the system says, and `cores` processors.
    Handheld {
        memory: u64,
        available: Option<u64>,
        cores: usize,
    },
    /// The same, on a system that takes the surface away whenever the
    /// application leaves the front: Android.
    Android {
        memory: u64,
        available: Option<u64>,
        cores: usize,
    },
}

impl Device {
    /// An iPhone of today: 6 GiB, half of it left to the process.
    pub const PHONE: Self = Self::Handheld {
        memory: 6 << 30,
        available: Some(3 << 30),
        cores: 6,
    };

    /// A phone the system has left little: the budget's floor.
    pub const LOW_MEMORY_PHONE: Self = Self::Handheld {
        memory: 2 << 30,
        available: Some(400 << 20),
        cores: 4,
    };

    pub(crate) fn profile(self) -> crate::profile::Profile {
        use crate::profile::{DeviceMemory, Profile};
        let memory = |physical, available| DeviceMemory {
            physical,
            available,
        };
        match self {
            Self::Desktop => Profile::desktop(),
            Self::Handheld {
                memory: m,
                available,
                cores,
            } => Profile::handheld(memory(m, available), cores),
            Self::Android {
                memory: m,
                available,
                cores,
            } => Profile {
                suspend_takes_the_surface: true,
                ..Profile::handheld(memory(m, available), cores)
            },
        }
    }
}

/// The numbers the run's profile holds it to, as today's viewer has them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Decoded content the geometry server keeps resident, bytes.
    pub resident_bytes: u64,
    /// Tiles it keeps resident.
    pub resident_tiles: u64,
    /// Tiles uploaded to the GPU in one frame, at most.
    pub uploads_per_frame: u64,
}

/// What a run starts from.
#[derive(Debug, Clone)]
pub struct Start {
    pub view: View,
    /// One eye's frame, in device pixels.
    pub size: (u32, u32),
    /// Device pixels per point: 2 on a Retina panel, 3 on a phone.
    pub scale: f64,
    pub eyes: Eyes,
    pub device: Device,
    /// Frames a second: the stepped clock moves a frame's worth each step.
    pub refresh_hz: f64,
    /// The ground.
    pub fixture: Fixture,
    /// The tile store: a fresh one is a cold start, one a previous run filled
    /// is a warm one.
    pub store: Store,
    /// The instant the scene is lit for, UTC seconds.
    pub lit_at: f64,
    /// Where the session log is written, if anywhere: see the module.
    pub log: Option<PathBuf>,
    /// Whether every eye's picture goes into the log too.
    pub log_pictures: bool,
}

/// Midsummer noon over Greenwich, 2026: the sun high over the default view,
/// the same instant every run.
pub const MIDSUMMER_NOON: f64 = 1_782_043_200.0;

impl Default for Start {
    fn default() -> Self {
        Self {
            view: View::default(),
            size: (256, 256),
            scale: 1.0,
            eyes: Eyes::Mono,
            device: Device::Desktop,
            refresh_hz: 60.0,
            fixture: Fixture::default(),
            store: Store::default(),
            lit_at: MIDSUMMER_NOON,
            log: None,
            log_pictures: false,
        }
    }
}

/// Why a run did not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// There is no GPU to draw with.
    NoGpu(String),
    /// Another headless viewer is running in this process.
    Busy,
    /// The session could not be brought up.
    Session(String),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoGpu(why) => write!(f, "no GPU to draw with: {why}"),
            Self::Busy => f.write_str("another headless viewer runs in this process"),
            Self::Session(why) => write!(f, "the session did not start: {why}"),
        }
    }
}

impl std::error::Error for Refused {}

/// What one eye's scan found: ground pixels, and those left the clear colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EyeReading {
    pub ground: u64,
    pub black: u64,
}

/// One step, read: see the module.
#[derive(Debug, Clone, PartialEq)]
pub struct State {
    /// Steps since the run began, this one counted.
    pub frame: u64,
    /// Whether this step drew a frame.
    pub drew: bool,
    /// The view, as a script reads it.
    pub view: View,
    /// One eye's frame, device pixels, and the density.
    pub size: (u32, u32),
    pub scale: f64,
    /// What each eye's scan of the last frame drawn found.
    pub eyes: Vec<EyeReading>,
    /// The engine's numbers since the run began.
    pub metrics: Snapshot,
    /// What the network was asked since the run began: terrain tiles and
    /// imagery tiles.
    pub network: (u64, u64),
    /// What each observer read, by topic.
    pub observed: BTreeMap<String, serde_json::Value>,
}

impl State {
    /// As the session log and the socket write it.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "frame": self.frame,
            "drew": self.drew,
            "view": self.view.to_json(),
            "size": [self.size.0, self.size.1],
            "scale": self.scale,
            "eyes": self.eyes.iter().map(|e| serde_json::json!({
                "ground": e.ground,
                "black": e.black,
            })).collect::<Vec<_>>(),
            "metrics": serde_json::to_value(self.metrics).unwrap_or_default(),
            "network": {"terrain": self.network.0, "imagery": self.network.1},
            "observed": self.observed,
        })
    }

    /// Whether any eye saw ground left the clear colour.
    pub fn black(&self) -> u64 {
        self.eyes.iter().map(|e| e.black).max().unwrap_or(0)
    }

    /// Whether the picture is what the selection asked for: every selected
    /// tile drawn by itself, and nothing standing in — the specification's
    /// convergence (I3), short of the digest holding still, which
    /// [`Headless::settle`] watches over frames.
    pub fn complete(&self) -> bool {
        let m = &self.metrics;
        self.drew
            && m.frame_exact > 0
            && m.frame_lost == 0
            && m.frame_coarser == 0
            && m.frame_stand_ins == 0
    }

    /// Whether nothing is loading and nothing waits to be uploaded.
    pub fn idle(&self) -> bool {
        self.metrics.loads_in_flight == 0 && self.metrics.pending_uploads == 0
    }
}

/// What a series of steps came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settled {
    /// Steps taken.
    pub frames: u64,
    /// Whether the picture settled within them.
    pub settled: bool,
}

/// Steps the picture must hold still, complete, to count as settled.
const STILL_FOR: usize = 3;

/// Polls of the geometry server one step allows. Far above anything a step
/// needs; a server that is still busy after them is churning, and the step
/// goes on without it rather than spinning for ever.
const SERVER_POLLS: u64 = 1_000_000;

/// Whether a headless viewer is running in this process.
static RUNNING: AtomicBool = AtomicBool::new(false);

/// The process's one headless viewer, claimed: let go of when dropped,
/// however the run ends — a start that fails or panics included.
struct Claim;

impl Claim {
    fn take() -> Option<Self> {
        (!RUNNING.swap(true, Ordering::SeqCst)).then_some(Self)
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::SeqCst);
    }
}

/// Notes that the server woke itself.
#[derive(Debug, Default)]
struct Woken(AtomicBool);

impl Wake for Woken {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.store(true, Ordering::SeqCst);
    }
}

type ServerFuture = Pin<Box<dyn Future<Output = ()>>>;

/// The viewer, headless: see the module.
pub struct Headless {
    app: App,
    hand: Stepper,
    server: Option<ServerFuture>,
    woken: Arc<Woken>,
    requests: Sender<Request>,
    /// The switcher's: a change of imagery resolves on it.
    runtime: Option<tokio::runtime::Runtime>,
    dt: Duration,
    /// The run's time, as its clock has it.
    elapsed: Duration,
    frame: u64,
    baseline: Snapshot,
    network: (u64, u64),
    start: Start,
    limits: Limits,
    history: Vec<State>,
    observers: Vec<Box<dyn Observer>>,
    log: Option<log::SessionLog>,
    socket: Option<socket::Listener>,
    /// The server's polls the last step took.
    polls: u64,
    /// Last, so it is let go of once everything else has gone.
    _claim: Claim,
}

impl Headless {
    /// Brings a run up: the GPU, the fixture globe, the session in lockstep,
    /// and the application on its offscreen eyes.
    pub fn start(start: Start) -> Result<Self, Refused> {
        let claim = Claim::take().ok_or(Refused::Busy)?;
        Self::bring_up(start, claim)
    }

    fn bring_up(start: Start, claim: Claim) -> Result<Self, Refused> {
        let gpu = pollster::block_on(tuile_wgpu::GpuContext::headless())
            .map_err(|e| Refused::NoGpu(e.to_string()))?;
        let profile = start.device.profile();
        let limits = Limits {
            resident_bytes: profile.resident_budget_bytes as u64,
            resident_tiles: profile.resident_tile_limit as u64,
            uploads_per_frame: profile.uploads.tiles as u64,
        };
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("tuile-headless")
            .enable_all()
            .build()
            .map_err(|e| Refused::Session(e.to_string()))?;
        // The network's time starts again with the run's.
        start.fixture.advance_to(Duration::ZERO);
        let globe = pollster::block_on(crate::globe::assemble(
            Arc::new(start.fixture.clone()),
            Some(Arc::new(start.store.clone())),
            &fixture::terrain(),
            fixture::layers(),
            0,
            Arc::new(tuile_core::offload::Inline),
        ))
        .map_err(|e| Refused::Session(format!("{e:#}")))?;
        let pinned = Some(profile.pinned_level);
        let (started, server) = crate::native::launch::in_lockstep(
            globe,
            profile.traversal(pinned),
            runtime.handle().clone(),
        );
        let (clock, hand) = Clock::stepped(web_time::Instant::now());
        let (requests, queue) = std::sync::mpsc::channel();
        let config = ViewerConfig {
            controller: tuile_camera::CameraController::new(
                crate::start::StartView::from(start.view).camera(),
            )
            .with_min_altitude(1.0),
            title: "Tuile (headless)".into(),
            layers: fixture::layers(),
            layer: 0,
            here: None,
            requests: queue,
            remembers: false,
            lit_at_unix_seconds: start.lit_at,
            pinned_level: pinned,
            profile,
            retries: None,
            window: winit::window::Window::default_attributes(),
            controls: true,
            backends: None,
            clock,
        };
        let mut app = App::new(config, crate::launch::Launch::finished(Ok(started)));
        app.speak_through(said);
        app.start_offscreen(gpu, start.size, start.scale, start.eyes.rig());
        let log = match &start.log {
            Some(path) => Some(
                log::SessionLog::open(path, start.log_pictures, start.eyes.count())
                    .map_err(|e| Refused::Session(e.to_string()))?,
            ),
            None => None,
        };
        let m = tuile_core::metrics::metrics();
        // An eye this run has no reading for yet reads nothing, not the last
        // run's.
        m.eye_ground_pixels.clear();
        m.eye_black_pixels.clear();
        let server: ServerFuture = Box::pin(server.run());
        Ok(Self {
            app,
            hand,
            server: Some(server),
            woken: Arc::new(Woken::default()),
            requests,
            runtime: Some(runtime),
            dt: Duration::from_secs_f64(1.0 / start.refresh_hz.max(1.0)),
            elapsed: Duration::ZERO,
            frame: 0,
            baseline: m.snapshot(),
            network: start.fixture.asked(),
            limits,
            start,
            history: Vec::new(),
            observers: Vec::new(),
            log,
            socket: None,
            polls: 0,
            _claim: claim,
        })
    }

    /// Ends this run and starts another over the same store and the same
    /// fixture: the application killed and opened again, warm.
    pub fn relaunch(self) -> Result<Self, Refused> {
        let start = Start {
            log: None,
            ..self.start.clone()
        };
        drop(self);
        Self::start(start)
    }

    /// The numbers the run's profile holds it to.
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// What the run started from.
    pub fn started_from(&self) -> &Start {
        &self.start
    }

    /// One frame: the clock moves a frame on, the geometry server answers
    /// what the last frame asked, the loop turns, and the frame it asks for
    /// is drawn, read back and observed.
    pub fn step(&mut self) -> &State {
        self.app.land();
        self.hand.advance(self.dt);
        self.elapsed += self.dt;
        // What the network owes by now lands before the server is polled.
        self.start.fixture.advance_to(self.elapsed);
        self.polls = self.run_the_server();
        let drew = match self.app.turn() {
            Turn::End => false,
            Turn::Flow(_) => self.app.render_if_asked(),
        };
        self.frame += 1;
        let mut state = self.read(drew);
        for observer in &mut self.observers {
            let reading = observer.observe(&state);
            state.observed.insert(observer.topic().to_owned(), reading);
        }
        if let Some(log) = self.log.as_mut() {
            log.write(&state, &self.app);
        }
        self.history.push(state);
        self.answer_the_socket();
        self.history
            .last()
            .unwrap_or_else(|| unreachable!("a step was just recorded"))
    }

    /// `n` frames.
    pub fn steps(&mut self, n: u64) -> &State {
        for _ in 1..n {
            self.step();
        }
        self.step()
    }

    /// Steps until the picture is complete and has held still for a few
    /// frames, or `bound` frames have passed.
    pub fn settle(&mut self, bound: u64) -> Settled {
        for frames in 1..=bound {
            self.step();
            if self.still() {
                return Settled {
                    frames,
                    settled: true,
                };
            }
        }
        Settled {
            frames: bound,
            settled: false,
        }
    }

    /// Steps until `holds` of the state, or `bound` frames have passed;
    /// returns how many it took, or the bound.
    pub fn wait_until(&mut self, bound: u64, holds: impl Fn(&State) -> bool) -> Result<u64, u64> {
        for frames in 1..=bound {
            if holds(self.step()) {
                return Ok(frames);
            }
        }
        Err(bound)
    }

    /// Whether the last few frames drawn were complete and drew the same
    /// tiles: converged, as the specification has it (I3). Frames, not
    /// steps: a phone at rest draws four times a second, and the steps
    /// between draw nothing. Loads may still be in flight — what is
    /// prefetched beside the view is not the picture; [`State::idle`] says
    /// whether they are.
    fn still(&self) -> bool {
        let tail: Vec<&State> = self
            .history
            .iter()
            .rev()
            .filter(|s| s.drew)
            .take(STILL_FOR)
            .collect();
        tail.len() == STILL_FOR
            && tail.iter().all(|s| s.complete())
            && tail
                .windows(2)
                .all(|w| w[0].metrics.frame_digest == w[1].metrics.frame_digest)
    }

    /// Does `action` now, before the next step.
    pub fn act(&mut self, action: &Action) {
        series::carry_out(self, action);
    }

    /// Plays `series`: each step's actions, then the step. Returns the states
    /// it produced.
    pub fn play(&mut self, series: &Series) -> &[State] {
        let first = self.history.len();
        for actions in series.frames() {
            for action in actions {
                self.act(action);
            }
            self.step();
        }
        &self.history[first..]
    }

    /// Flies a recorded camera path, one camera a frame, as `TUILE_REPLAY`
    /// does; the run goes on from where it ends.
    pub fn fly(&mut self, tape: tuile_tape::Tape) {
        self.app.fly(tape);
    }

    /// The last step's state, or the state before the first.
    pub fn state(&self) -> State {
        self.history
            .last()
            .cloned()
            .unwrap_or_else(|| self.read(false))
    }

    /// Every step's state, in order.
    pub fn history(&self) -> &[State] {
        &self.history
    }

    /// The picture eye `eye` last drew: tightly packed RGBA8, sRGB, and its
    /// size.
    pub fn picture(&self, eye: usize) -> Option<(&[u8], (u32, u32))> {
        self.app.eye_pixels(eye)
    }

    /// Each eye's camera, as the last frame drew it — what its picture is
    /// scanned against.
    pub fn eye_cameras(&self) -> Vec<tuile_tape::Frame> {
        self.app
            .eye_cameras()
            .iter()
            .map(crate::app::offscreen::frame_of)
            .collect()
    }

    /// Frames that reached the eyes since the run began.
    pub fn frames_drawn(&self) -> u64 {
        self.app.frames_presented()
    }

    /// Whether a tape given to [`Headless::fly`] still has cameras to fly.
    pub fn flying(&self) -> bool {
        self.app.flying()
    }

    /// Adds a source of readings: a topic in the session log, a key in every
    /// [`State::observed`] from the next step on.
    pub fn observe_with(&mut self, observer: Box<dyn Observer>) {
        self.observers.push(observer);
    }

    /// The run's store, to start another from.
    pub fn store(&self) -> &Store {
        &self.start.store
    }

    /// The server's polls the last step took.
    pub fn server_polls(&self) -> u64 {
        self.polls
    }

    /// Why the session ended, if it has: a replay that reached its end with
    /// nobody landing it, say.
    pub fn ended(&self) -> Option<&'static str> {
        self.app.ending()
    }

    /// Closes the session log, and says where it is.
    pub fn finish(mut self) -> Result<Option<PathBuf>, String> {
        let log = self.log.take();
        drop(self);
        match log {
            Some(log) => log.finish().map(Some),
            None => Ok(None),
        }
    }

    pub(crate) fn queue(&self, request: Request) {
        // The receiver is the application's, which lives as long as `self`.
        let _ = self.requests.send(request);
    }

    pub(crate) fn app_mut(&mut self) -> &mut App {
        &mut self.app
    }

    /// Polls the geometry server until it has nothing left to do: every
    /// message the last frame sent taken, every load it could finish
    /// finished — all inline, on this thread, in an order that is the same
    /// every run.
    fn run_the_server(&mut self) -> u64 {
        let Some(server) = self.server.as_mut() else {
            return 0;
        };
        let waker = Waker::from(Arc::clone(&self.woken));
        let mut cx = Context::from_waker(&waker);
        let mut polls = 0;
        loop {
            self.woken.0.store(false, Ordering::SeqCst);
            polls += 1;
            if let Poll::Ready(()) = server.as_mut().poll(&mut cx) {
                tracing::info!("the geometry server ended");
                self.server = None;
                break;
            }
            if !self.woken.0.load(Ordering::SeqCst) {
                break;
            }
            if polls >= SERVER_POLLS {
                tracing::warn!(polls, "the geometry server is still busy; stepping on");
                break;
            }
        }
        polls
    }

    fn read(&self, drew: bool) -> State {
        let metrics = tuile_core::metrics::metrics()
            .snapshot()
            .since(&self.baseline);
        let view = crate::snapshot::current()
            .map_or_else(|| crate::steer::view_of(&self.app.camera()), |s| s.view)
            .into();
        let (size, scale) = self.app.screen_size().unwrap_or((self.start.size, 1.0));
        let eyes = (0..self.start.eyes.count())
            .map(|eye| EyeReading {
                ground: metrics.eye_ground_pixels[eye.min(tuile_core::metrics::EYES - 1)],
                black: metrics.eye_black_pixels[eye.min(tuile_core::metrics::EYES - 1)],
            })
            .collect();
        let asked = self.start.fixture.asked();
        State {
            frame: self.frame,
            drew,
            view,
            size,
            scale,
            eyes,
            metrics,
            network: (asked.0 - self.network.0, asked.1 - self.network.1),
            observed: BTreeMap::new(),
        }
    }

    fn answer_the_socket(&mut self) {
        let Some(requests) = self.socket.as_ref().map(socket::Listener::pending) else {
            return;
        };
        for (request, reply) in requests {
            let answer = socket::answer(self, &request);
            let _ = reply.send(answer);
        }
    }
}

impl Drop for Headless {
    fn drop(&mut self) {
        // A log nobody finished is finished here: an MCAP file without its
        // footer is one most readers refuse.
        if let Some(log) = self.log.take() {
            if let Err(e) = log.finish() {
                tracing::error!("the session log was not closed: {e}");
            }
        }
        self.socket = None;
        self.app.shut_down();
        // The server goes before the runtime its switcher spawns on.
        self.server = None;
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

/// What a headless run says to the person: a line in the log, since there is
/// nobody to dismiss an alert.
fn said(title: &str, text: &str) -> bool {
    tracing::warn!(title, text, "said to the person");
    true
}
