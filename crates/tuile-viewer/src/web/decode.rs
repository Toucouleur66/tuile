// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The browser's decode pool: Web Workers that decode imagery and terrain for
//! the geometry server's worker, exchanging transferred buffers.
//!
//! ```text
//!   geometry worker                                  decode workers (N)
//!   loader ── run_portable ── WebDecode ─┐           ┌─ runDecodeWorker
//!     ▲                                  │  {id, kind, input: ArrayBuffer}
//!     │                       Scheduler ─┼──────────▶│  jobs::serve(kind, bytes)
//!     └──── Reply (oneshot) ◀────────────┘◀──────────┘  {id, output: ArrayBuffer}
//! ```
//!
//! No `SharedArrayBuffer`, so no atomics, no nightly `build-std`, and no
//! COOP/COEP headers asked of whoever serves the page: each worker is its own
//! instance of the same module, handed the geometry worker's compiled
//! `WebAssembly.Module` so it compiles nothing and fetches nothing. A job
//! crosses as one transferred buffer each way, which is what the jobs'
//! [`Portable`](tuile_core::offload::Portable) form is for.
//!
//! Never a hole: a worker that fails is replaced, and its job — like every job
//! once no worker can take one — fails back to its caller, which decodes it on
//! the geometry worker's own thread, as before the pool existed.

use std::cell::RefCell;

use js_sys::{Array, ArrayBuffer, Object, Reflect, Uint8Array};
use tuile_core::offload::{Job, Offload, Remote, Reply};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_time::{Duration, Instant};

use super::schedule::{Dispatch, Queued, Scheduler};

/// How long a decode worker may take to load before its jobs are decoded
/// here instead. Generous: a phone compiling nothing still has to instantiate
/// a module of several megabytes.
const START_TIMEOUT_MS: i32 = 15_000;

/// How many failed workers are replaced before the pool is left to shrink.
/// A worker that fails is a decode that panicked or a browser that took the
/// worker away; one that keeps failing is a bug, and replacing it for ever
/// would only hide it.
const REPLACEMENTS: usize = 8;

/// How often the decode counters are written to the console, when anything
/// was decoded.
const SUMMARY_EVERY: Duration = Duration::from_secs(10);

thread_local! {
    static POOL: RefCell<Option<Pool>> = const { RefCell::new(None) };
    static RANKS: RefCell<Option<tuile_core::offload::LoadRanks>> = const { RefCell::new(None) };
    static STATS: RefCell<Stats> = RefCell::new(Stats::new());
}

/// Where the loader's decoding goes in a browser: the pool when one was
/// started and still has a worker, this thread otherwise.
///
/// A handle on this thread's pool, not the pool: the workers are JavaScript
/// objects, which stay on the thread that made them.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct WebDecode;

impl Offload for WebDecode {
    /// A job that cannot leave — a closure, or a portable job whose remote
    /// failed — runs here, timed, so the counters say what the pool spared.
    fn submit(&self, job: Job) {
        let started = Instant::now();
        job();
        let spent = started.elapsed();
        STATS.with(|s| {
            let mut s = s.borrow_mut();
            s.here += 1;
            s.here_time += spent;
        });
        summarise();
    }

    fn remote(&self) -> Option<&dyn Remote> {
        let alive = POOL.with(|p| p.borrow().as_ref().is_some_and(|p| p.schedule.alive()));
        alive.then_some(self as &dyn Remote)
    }
}

impl Remote for WebDecode {
    fn submit_bytes(&self, kind: &'static str, input: Vec<u8>, reply: Reply) {
        let started = Instant::now();
        let refused = POOL.with(|p| {
            let mut p = p.borrow_mut();
            let Some(pool) = p.as_mut() else {
                return Some(reply);
            };
            // Here, never in a worker's own listener: see `Pool::retired`.
            pool.retired.clear();
            let job = Pending {
                kind,
                input,
                reply,
                asked: started,
                load: tuile_core::offload::loading(),
            };
            match pool.schedule.submit(job) {
                Ok(Some(dispatch)) => pool.dispatch(dispatch),
                Ok(None) => {}
                Err(job) => return Some(job.reply),
            }
            None
        });
        if let Some(reply) = refused {
            reply.send(Err("no decode worker is left".into()));
        }
        STATS.with(|s| s.borrow_mut().handing += started.elapsed());
    }
}

/// Starts `count` decode workers, each made by `make` — the package's own
/// function, written so a bundler sees the decode worker's script.
pub(crate) fn start(make: js_sys::Function, count: usize) {
    let mut pool = Pool {
        make,
        slots: (0..count).map(|_| None).collect(),
        schedule: Scheduler::new(count),
        next_id: 0,
        replacements: REPLACEMENTS,
        retired: Vec::new(),
    };
    let mut failed = Vec::new();
    for index in 0..count {
        if let Err(why) = pool.spawn(index) {
            failed.push(why);
            let _ = pool.schedule.lost(index);
        }
    }
    if let Some(why) = failed.first() {
        tracing::warn!(
            failed = failed.len(),
            count,
            "decode workers could not start, decoding here instead: {why}"
        );
    }
    tracing::info!(workers = count - failed.len(), "decode pool");
    POOL.with(|p| *p.borrow_mut() = Some(pool));
}

/// The decode worker's entry point, called by the package's decode worker
/// script once the module is instantiated: answers jobs until it is
/// terminated.
#[wasm_bindgen(js_name = runDecodeWorker)]
pub fn run_decode_worker() {
    super::log::install();
    let on_job =
        Closure::<dyn FnMut(web_sys::MessageEvent)>::new(|event: web_sys::MessageEvent| {
            answer(&event.data());
        });
    scope().set_onmessage(Some(on_job.as_ref().unchecked_ref()));
    // The handler lives as long as the worker.
    on_job.forget();
    let ready = Object::new();
    let _ = Reflect::set(&ready, &"ready".into(), &JsValue::TRUE);
    if let Err(e) = scope().post_message(&ready) {
        tracing::error!("the decode worker could not say it is ready: {e:?}");
    }
}

/// Runs one job and posts its answer, the output's buffer transferred.
fn answer(message: &JsValue) {
    let id = Reflect::get(message, &"id".into()).unwrap_or(JsValue::NULL);
    let kind = Reflect::get(message, &"kind".into())
        .ok()
        .and_then(|k| k.as_string());
    let input = Reflect::get(message, &"input".into())
        .ok()
        .and_then(|i| i.dyn_into::<ArrayBuffer>().ok());
    let outcome = match (kind, input) {
        (Some(kind), Some(input)) => {
            tuile_planetary::jobs::serve(&kind, Uint8Array::new(&input).to_vec())
        }
        _ => Err("a decode job without its kind or its bytes".into()),
    };
    let reply = Object::new();
    let _ = Reflect::set(&reply, &"id".into(), &id);
    let posted = match outcome {
        Ok(output) => {
            let buffer = Uint8Array::from(output.as_slice()).buffer();
            let _ = Reflect::set(&reply, &"output".into(), &buffer);
            scope().post_message_with_transfer(&reply, &Array::of1(&buffer))
        }
        Err(why) => {
            let _ = Reflect::set(&reply, &"error".into(), &why.into());
            scope().post_message(&reply)
        }
    };
    if let Err(e) = posted {
        tracing::error!("a decode answer could not be posted: {e:?}");
    }
}

/// A job waiting for, or running on, a worker.
struct Pending {
    kind: &'static str,
    input: Vec<u8>,
    reply: Reply,
    asked: Instant,
    /// The load waiting on it, named when it was submitted
    /// (`tuile_core::offload::loading`).
    load: Option<tuile_core::budget::ResourceId>,
}

impl Queued for Pending {
    fn wanted(&self) -> bool {
        self.reply.wanted()
    }

    /// The load's rank in the session's latest step: see [`follow`].
    fn rank(&self) -> u32 {
        let Some(load) = self.load else {
            return u32::MAX;
        };
        RANKS.with(|r| {
            r.borrow()
                .as_ref()
                .and_then(|ranks| ranks.rank(load))
                .unwrap_or(u32::MAX)
        })
    }
}

/// Orders the pool's queue by the session's ranks of its loads in flight:
/// the decoding a load the camera now looks at waits on goes before the
/// decoding of one it has left.
pub(crate) fn follow(ranks: tuile_core::offload::LoadRanks) {
    RANKS.with(|r| *r.borrow_mut() = Some(ranks));
}

/// What a worker is running.
struct InFlight {
    id: u64,
    reply: Reply,
    asked: Instant,
}

/// One worker, and the listeners that must live as long as it does.
struct Slot {
    worker: web_sys::Worker,
    /// Which start of this slot it is: a timeout or a message from a worker
    /// since replaced is ignored.
    generation: u64,
    running: Option<InFlight>,
    /// The start timeout, cleared when the slot is retired so it never fires
    /// on a listener that is gone.
    timeout: i32,
    _heard: Closure<dyn FnMut(web_sys::MessageEvent)>,
    _failed: Closure<dyn FnMut(web_sys::Event)>,
    _late: Closure<dyn FnMut()>,
}

struct Pool {
    make: js_sys::Function,
    slots: Vec<Option<Slot>>,
    schedule: Scheduler<Pending>,
    next_id: u64,
    replacements: usize,
    /// Slots whose worker has gone, kept until no listener of theirs can be
    /// running: a worker is lost from inside one of its own listeners, and a
    /// listener must not be dropped while it runs.
    retired: Vec<Slot>,
}

impl Pool {
    /// Makes worker `index`, listens to it, hands it the module, and gives it
    /// [`START_TIMEOUT_MS`] to say it is ready.
    fn spawn(&mut self, index: usize) -> Result<(), String> {
        let generation = self.next_id;
        self.next_id += 1;
        let worker: web_sys::Worker = self
            .make
            .call0(&JsValue::NULL)
            .map_err(|e| format!("{e:?}"))?
            .dyn_into()
            .map_err(|_| "the decode worker function returned no worker".to_owned())?;
        let heard =
            Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
                heard(index, generation, &e.data());
            });
        worker.set_onmessage(Some(heard.as_ref().unchecked_ref()));
        let failed = Closure::<dyn FnMut(web_sys::Event)>::new(move |e: web_sys::Event| {
            // Said here, once, rather than also as an uncaught error.
            e.prevent_default();
            let why = Reflect::get(&e, &"message".into())
                .ok()
                .and_then(|m| m.as_string())
                .unwrap_or_else(|| "the decode worker failed".into());
            lose(index, generation, &why);
        });
        worker.set_onerror(Some(failed.as_ref().unchecked_ref()));
        let late = Closure::<dyn FnMut()>::new(move || {
            let starting = POOL.with(|p| {
                p.borrow().as_ref().is_some_and(|p| {
                    p.slots[index]
                        .as_ref()
                        .is_some_and(|s| s.generation == generation)
                        && p.schedule.starting(index)
                })
            });
            if starting {
                lose(index, generation, "the decode worker did not load in time");
            }
        });
        let timeout = scope()
            .set_timeout_with_callback_and_timeout_and_arguments_0(
                late.as_ref().unchecked_ref(),
                START_TIMEOUT_MS,
            )
            .unwrap_or(0);
        // The module this worker runs, compiled once: the decode worker
        // instantiates it rather than fetching and compiling it again.
        let init = Object::new();
        let _ = Reflect::set(&init, &"module".into(), &wasm_bindgen::module());
        worker
            .post_message(&init)
            .map_err(|e| format!("the decode worker could not be handed its module: {e:?}"))?;
        self.slots[index] = Some(Slot {
            worker,
            generation,
            running: None,
            timeout,
            _heard: heard,
            _failed: failed,
            _late: late,
        });
        Ok(())
    }

    /// Posts a job to its worker, its bytes transferred. A job that cannot be
    /// posted fails back to its caller at once.
    fn dispatch(&mut self, Dispatch { worker, job }: Dispatch<Pending>) {
        let id = self.next_id;
        self.next_id += 1;
        let Some(slot) = self.slots[worker].as_mut() else {
            job.reply.send(Err("the decode worker is gone".into()));
            return;
        };
        let buffer = Uint8Array::from(job.input.as_slice()).buffer();
        drop(job.input);
        let message = Object::new();
        let _ = Reflect::set(&message, &"id".into(), &JsValue::from(id as f64));
        let _ = Reflect::set(&message, &"kind".into(), &job.kind.into());
        let _ = Reflect::set(&message, &"input".into(), &buffer);
        match slot
            .worker
            .post_message_with_transfer(&message, &Array::of1(&buffer))
        {
            Ok(()) => {
                slot.running = Some(InFlight {
                    id,
                    reply: job.reply,
                    asked: job.asked,
                })
            }
            Err(e) => job
                .reply
                .send(Err(format!("the job could not be posted: {e:?}"))),
        }
    }
}

/// What worker `index` said: that it is ready, or the answer to its job.
fn heard(index: usize, generation: u64, message: &JsValue) {
    if let Some(why) = Reflect::get(message, &"failed".into())
        .ok()
        .and_then(|f| f.as_string())
    {
        lose(index, generation, &why);
        return;
    }
    let started = Instant::now();
    let answered = POOL.with(|p| {
        let mut p = p.borrow_mut();
        let pool = p.as_mut()?;
        let slot = pool.slots[index].as_mut()?;
        if slot.generation != generation {
            return None;
        }
        if Reflect::has(message, &"ready".into()).unwrap_or(false) {
            if let Some(next) = pool.schedule.ready(index) {
                pool.dispatch(next);
            }
            return None;
        }
        let id = Reflect::get(message, &"id".into())
            .ok()
            .and_then(|i| i.as_f64());
        let running = slot.running.take()?;
        if id != Some(running.id as f64) {
            tracing::error!(
                index,
                "a decode answer for a job this worker was not running"
            );
        }
        let outcome = match Reflect::get(message, &"output".into())
            .ok()
            .and_then(|o| o.dyn_into::<ArrayBuffer>().ok())
        {
            Some(output) => Ok(Uint8Array::new(&output).to_vec()),
            None => Err(Reflect::get(message, &"error".into())
                .ok()
                .and_then(|e| e.as_string())
                .unwrap_or_else(|| "a decode answer with neither output nor error".into())),
        };
        if let Some(next) = pool.schedule.finished(index) {
            pool.dispatch(next);
        }
        Some((running, outcome))
    });
    let Some((running, outcome)) = answered else {
        return;
    };
    let wait = running.asked.elapsed();
    running.reply.send(outcome);
    STATS.with(|s| {
        let mut s = s.borrow_mut();
        s.away += 1;
        s.away_wait += wait;
        s.away_peak = s.away_peak.max(wait);
        s.handing += started.elapsed();
    });
    summarise();
}

/// Worker `index` has failed: its job fails back to its caller, and it is
/// replaced while replacements last. Once none is left that can work, every
/// queued job fails back too.
fn lose(index: usize, generation: u64, why: &str) {
    let (running, handed_back) = POOL.with(|p| {
        let mut p = p.borrow_mut();
        let Some(pool) = p.as_mut() else {
            return (None, Vec::new());
        };
        let Some(mut slot) = pool.slots[index].take_if(|s| s.generation == generation) else {
            return (None, Vec::new());
        };
        slot.worker.terminate();
        scope().clear_timeout_with_handle(slot.timeout);
        tracing::warn!(index, "a decode worker failed: {why}");
        let running = slot.running.take();
        pool.retired.push(slot);
        if pool.replacements > 0 && pool.spawn(index).is_ok() {
            pool.replacements -= 1;
            pool.schedule.restarted(index);
            return (running, Vec::new());
        }
        let handed_back = pool.schedule.lost(index);
        if !pool.schedule.alive() {
            tracing::warn!("no decode worker is left: decoding here");
        }
        (running, handed_back)
    });
    if let Some(running) = running {
        running
            .reply
            .send(Err(format!("the decode worker failed: {why}")));
    }
    for job in handed_back {
        job.reply.send(Err("no decode worker is left".into()));
    }
}

/// What the decoding cost this thread, and what the pool did instead.
struct Stats {
    since: Instant,
    /// Jobs run on this thread, and the time they held it.
    here: u32,
    here_time: Duration,
    /// Jobs answered by a worker, and the time from asking to the answer.
    away: u32,
    away_wait: Duration,
    away_peak: Duration,
    /// This thread's share of the jobs that went away: queueing, copying the
    /// bytes out and the answer back in.
    handing: Duration,
}

impl Stats {
    fn new() -> Self {
        Self {
            since: Instant::now(),
            here: 0,
            here_time: Duration::ZERO,
            away: 0,
            away_wait: Duration::ZERO,
            away_peak: Duration::ZERO,
            handing: Duration::ZERO,
        }
    }
}

/// One console line every [`SUMMARY_EVERY`] while anything is decoded: the
/// number to read before and after a change to where decoding runs.
fn summarise() {
    let line = STATS.with(|s| {
        let mut s = s.borrow_mut();
        let seconds = s.since.elapsed();
        if seconds < SUMMARY_EVERY {
            return None;
        }
        let done = std::mem::replace(&mut *s, Stats::new());
        Some((done, seconds))
    });
    let Some((s, seconds)) = line else { return };
    let (queued, peak, dropped) = POOL.with(|p| {
        p.borrow().as_ref().map_or((0, 0, 0), |p| {
            (p.schedule.queued(), p.schedule.peak(), p.schedule.dropped())
        })
    });
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    tracing::info!(
        seconds = format!("{:.1}", seconds.as_secs_f64()),
        here = s.here,
        here_ms = format!("{:.0}", ms(s.here_time)),
        away = s.away,
        away_wait_mean_ms = format!("{:.1}", ms(s.away_wait) / f64::from(s.away.max(1))),
        away_wait_peak_ms = format!("{:.0}", ms(s.away_peak)),
        handing_ms = format!("{:.0}", ms(s.handing)),
        queued,
        queue_peak = peak,
        dropped,
        "decode"
    );
}

/// How many cores the browser reports to this worker.
pub(crate) fn hardware_concurrency() -> Option<f64> {
    Some(scope().navigator().hardware_concurrency())
}

fn scope() -> web_sys::DedicatedWorkerGlobalScope {
    js_sys::global().unchecked_into()
}
