// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The page's session: a worker running the geometry server, and the handles
//! the application drives it through — under the names the desktop's have.
//!
//! On a desktop the window and the loader share memory: the imagery detail is
//! an atomic both read, the gate a notification both see. A page and its
//! worker share nothing, so each handle here is a small proxy that posts what
//! the application did to the worker, which does it to the loader's own
//! handle (see [`super::control`]). The application cannot tell the
//! difference, which is the point.
//!
//! Relief is the one thing that travels the other way. The camera stands on
//! the terrain's heights, which the desktop's loader records as it decodes;
//! here the page records them from the content it is handed — each terrain
//! tile's highest vertex — before the content goes on to the GPU.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll};

use tuile_core::content::TileContent;
use tuile_core::protocol::{ClientMessage, GeometryStream, ServerMessage, StreamError};
use tuile_planetary::LayerBudget;
use tuile_terrain::{GeographicTilingScheme, TerrainHeights, TileCoord};
use tuile_web::post::{Connection, WorkerStream};
use wasm_bindgen::{JsCast, JsValue};
use web_time::Instant;

use super::control::{ToPage, ToWorker};
use super::options::{ProfileChoice, SourcesSpec};
use crate::embed::ImageryChoice;
use crate::globe::Switched;
use crate::launch::{Launch, Outcome, START_TIMEOUT};

/// Everything a start-up needs, kept so a failed one can be tried again.
pub(crate) struct Plan {
    /// Makes the worker: the package's own function, written so a bundler
    /// sees the worker script and ships it.
    pub make_worker: js_sys::Function,
    pub sources: SourcesSpec,
    /// The catalog's bearer. Held for the page's lifetime and posted to each
    /// worker it starts; never logged, never in a URL.
    pub token: Option<String>,
    pub profile: ProfileChoice,
    pub device_memory: Option<f64>,
}

/// The worker, and the page's ear on it. Dropping the last handle ends both:
/// the listener goes, and the worker is terminated with everything it held.
struct Link {
    worker: web_sys::Worker,
    _connection: Connection,
}

impl Link {
    fn post(&self, message: &ToWorker) {
        match serde_wasm_bindgen::to_value(message) {
            Ok(value) => {
                if let Err(e) = self.worker.post_message(&value) {
                    tracing::error!("the worker could not be told: {e:?}");
                }
            }
            Err(e) => tracing::error!("a message to the worker could not be written: {e}"),
        }
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.worker.terminate();
    }
}

/// A session that is up: its stream, and the handles the window adopts.
pub(crate) struct Started {
    pub stream: PageStream,
    pub detail: Detail,
    pub heights: Arc<TerrainHeights>,
    pub layer_budget: Budget,
    pub switcher: Switcher,
    pub gate: Gate,
}

impl Started {
    /// Closes the session down. A page has no store to flush and no thread to
    /// join: the worker is terminated, and that is all of it.
    pub(crate) fn close(self, _patience: std::time::Duration, _join_the_server: bool) {
        tracing::info!("session closed; the worker is terminated");
    }

    /// The browser keeps no tile store yet: nothing to write down.
    pub(crate) fn suspend_store(&self) {}

    /// Nor anything to open again.
    pub(crate) fn resume_store(&self) {}
}

/// The stream, with the relief recorded on the way past.
pub(crate) struct PageStream {
    inner: WorkerStream,
    heights: Arc<TerrainHeights>,
}

impl GeometryStream for PageStream {
    fn send(&self, msg: ClientMessage) -> Result<(), StreamError> {
        self.inner.send(msg)
    }

    fn poll_message(&mut self, cx: &mut Context<'_>) -> Poll<Option<ServerMessage>> {
        let polled = self.inner.poll_message(cx);
        if let Poll::Ready(Some(ServerMessage::Content {
            tile,
            content: TileContent::Decoded(decoded),
            ..
        })) = &polled
        {
            if let Some(highest) = highest_point(decoded) {
                let (z, x, y) = tile.terrain_coord();
                self.heights.record(TileCoord::new(z, x, y), highest);
            }
        }
        polled
    }
}

/// The highest point of a tile's surface above the ellipsoid, metres.
fn highest_point(content: &tuile_core::content::DecodedTileContent) -> Option<f32> {
    content
        .meshes
        .iter()
        .flat_map(|mesh| mesh.positions.iter())
        .map(|p| {
            let ecef = content.local_origin_ecef + glam::DVec3::from(glam::Vec3::from(*p));
            tuile_core::geo::ecef_to_geodetic(ecef).height as f32
        })
        .reduce(f32::max)
}

/// The imagery detail the loader drapes at, forwarded when it changes.
#[derive(Clone, Default)]
pub(crate) struct Detail {
    link: Option<Rc<Link>>,
    sent: Rc<Cell<u64>>,
}

impl Detail {
    /// See `tuile_planetary::ImageryDetail::set_target_texel_spacing`. Called
    /// every frame; posted only when the number moved.
    pub(crate) fn set_target_texel_spacing(&self, metres: f64) {
        let Some(link) = self.link.as_ref() else {
            return;
        };
        if self.sent.replace(metres.to_bits()) != metres.to_bits() {
            link.post(&ToWorker::Detail { metres });
        }
    }
}

/// How many imagery layers a drape may carry, as the device says.
#[derive(Clone, Default)]
pub(crate) struct Budget {
    link: Option<Rc<Link>>,
    local: LayerBudget,
}

impl Budget {
    /// See `tuile_planetary::LayerBudget::set_from_device`.
    pub(crate) fn set_from_device(&self, max_sampled_textures_per_stage: u32) {
        self.local.set_from_device(max_sampled_textures_per_stage);
        if let Some(link) = self.link.as_ref() {
            link.post(&ToWorker::Budget {
                reported: max_sampled_textures_per_stage,
            });
        }
    }

    /// The slots a drape carries.
    pub(crate) fn get(&self) -> u32 {
        self.local.get()
    }
}

/// The worker's loads, held while the tab is hidden.
pub(crate) struct Gate {
    link: Rc<Link>,
}

impl Gate {
    /// Holds every load not yet started.
    pub(crate) fn shut(&self) {
        self.link.post(&ToWorker::Pause { paused: true });
    }

    /// Lets them go.
    pub(crate) fn open(&self) {
        self.link.post(&ToWorker::Pause { paused: false });
    }
}

/// A layer resolved, or refused, by the worker.
type Answer = (usize, Result<u64, String>);

/// Changes the imagery the worker's globe drapes. The same two calls as the
/// desktop's; the worker resolves the layer, which it already knows by
/// index.
pub(crate) struct Switcher {
    link: Rc<Link>,
    asked: RefCell<HashMap<usize, Instant>>,
    answers: Rc<RefCell<VecDeque<Answer>>>,
}

impl Switcher {
    /// Starts the switch to the host's layer `layer`.
    pub(crate) fn ask(&self, layer: usize, _choice: &ImageryChoice) {
        self.asked.borrow_mut().insert(layer, Instant::now());
        self.link.post(&ToWorker::Imagery { layer });
    }

    /// The outcome of a switch that has finished resolving, if one has.
    pub(crate) fn poll(&self) -> Option<Switched> {
        let (layer, outcome) = self.answers.borrow_mut().pop_front()?;
        let asked = self
            .asked
            .borrow_mut()
            .remove(&layer)
            .unwrap_or_else(Instant::now);
        Some(Switched {
            layer,
            outcome,
            asked,
        })
    }
}

/// What a start-up holds until the worker says it is up.
struct Waiting {
    answer: std::sync::mpsc::Sender<Outcome>,
    stream: WorkerStream,
    link: Rc<Link>,
}

/// Starts a worker, asks it to bring the session up, and returns at once.
///
/// The outcome arrives through the worker's `started` or `failed` — or, if
/// it says nothing within [`START_TIMEOUT`], as a failure, and the worker is
/// terminated.
pub(crate) fn start(plan: &Plan) -> Launch {
    let (answer, launch) = Launch::pending();
    let worker = match new_worker(&plan.make_worker) {
        Ok(worker) => worker,
        Err(why) => {
            let _ = answer.send(Err(why));
            return launch;
        }
    };
    let answers: Rc<RefCell<VecDeque<Answer>>> = Rc::default();
    let waiting: Rc<RefCell<Option<Waiting>>> = Rc::default();
    let heard = {
        let (waiting, answers) = (Rc::clone(&waiting), Rc::clone(&answers));
        move |value: JsValue| match serde_wasm_bindgen::from_value::<ToPage>(value) {
            Ok(ToPage::Started) => {
                if let Some(w) = waiting.borrow_mut().take() {
                    let _ = w.answer.send(Ok(started(w.stream, w.link, &answers)));
                }
            }
            Ok(ToPage::Failed { reason }) => {
                if let Some(w) = waiting.borrow_mut().take() {
                    let _ = w.answer.send(Err(reason));
                }
            }
            Ok(ToPage::Switched { layer, outcome }) => {
                answers.borrow_mut().push_back((layer, outcome));
            }
            Err(e) => tracing::error!("the worker said something unreadable: {e}"),
        }
    };
    let (stream, connection) = WorkerStream::connect(&worker, heard);
    let link = Rc::new(Link {
        worker,
        _connection: connection,
    });
    link.post(&ToWorker::Start {
        sources: plan.sources.clone(),
        token: plan.token.clone(),
        opening: 0,
        profile: plan.profile,
        device_memory: plan.device_memory,
    });
    *waiting.borrow_mut() = Some(Waiting {
        answer,
        stream,
        link,
    });
    give_up_after(START_TIMEOUT, move || {
        if let Some(w) = waiting.borrow_mut().take() {
            let _ = w.answer.send(Err(format!(
                "the tile service did not answer within {} s",
                START_TIMEOUT.as_secs()
            )));
        }
    });
    launch
}

/// The session's handles, over a worker that has said it is up.
fn started(
    stream: WorkerStream,
    link: Rc<Link>,
    answers: &Rc<RefCell<VecDeque<Answer>>>,
) -> Started {
    let heights = Arc::new(TerrainHeights::new(GeographicTilingScheme::default()));
    Started {
        stream: PageStream {
            inner: stream,
            heights: Arc::clone(&heights),
        },
        detail: Detail {
            link: Some(Rc::clone(&link)),
            sent: Rc::default(),
        },
        heights,
        layer_budget: Budget {
            link: Some(Rc::clone(&link)),
            local: LayerBudget::default(),
        },
        switcher: Switcher {
            link: Rc::clone(&link),
            asked: RefCell::default(),
            answers: Rc::clone(answers),
        },
        gate: Gate { link },
    }
}

/// A worker running the package's worker script.
fn new_worker(make: &js_sys::Function) -> Result<web_sys::Worker, String> {
    make.call0(&JsValue::NULL)
        .map_err(|e| format!("the geometry worker could not start: {e:?}"))?
        .dyn_into::<web_sys::Worker>()
        .map_err(|_| "the worker factory did not return a Worker".to_owned())
}

/// Runs `then` once, after `wait`, on the page's own timer.
fn give_up_after(wait: std::time::Duration, then: impl FnOnce() + 'static) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let callback = wasm_bindgen::closure::Closure::once_into_js(then);
    let millis = i32::try_from(wait.as_millis()).unwrap_or(i32::MAX);
    if let Err(e) = window
        .set_timeout_with_callback_and_timeout_and_arguments_0(callback.unchecked_ref(), millis)
    {
        tracing::warn!("no start-up timeout: {e:?}");
    }
}
