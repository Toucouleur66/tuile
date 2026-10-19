// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The geometry server: traversal + scheduling + fetch + decode,
//! orchestrated around the pure [`traverse`](crate::traversal::traverse) function.
//!
//! This is a LOGICAL server, not a network server: [`in_process`] binds it
//! to a consumer over channels (the fully decoded liaison). Network
//! middleware (M2, `tuile-server`) drives the same loop and serializes the
//! protocol — glb on the wire.
//!
//! Runtime-agnostic by default: no tokio, no spawning. The whole server is
//! one future ([`GeometryServer::run`]) multiplexing client messages and
//! in-flight fetches through `FuturesUnordered`; the caller decides where it
//! runs. On native, with the optional `rt-tokio` feature, the host can hand
//! it a runtime handle instead and every load runs as a task of its own —
//! see [`Spawn`].

use crate::cache::ResidentCache;
use crate::content::TileContent;
use crate::fetch::TileFetcher;
use crate::protocol::{
    in_process_pair, ClientMessage, InProcessStream, Priming, ServerEndpoint, ServerMessage,
};
use crate::source::{LoadError, Loaded, TileId, TileLoader, TileTree};
use crate::tiles3d::{TilesetLoader, TilesetTree};
use crate::tileset::Tileset;
use crate::traversal::{
    traverse_with, Config, NearestContent, ResidencyView, TraversalOutput, ViewState,
};
use futures_channel::mpsc::UnboundedSender;
use futures_core::Stream;
use futures_util::future::{poll_fn, AbortHandle, Abortable, Aborted};
use futures_util::stream::FuturesUnordered;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::task::Poll;

type LoadMsg = (TileId, Result<Loaded, LoadError>);
/// A load's result and the moment its own future resolved — what the loop
/// measures [`arrival_wait_seconds`](crate::metrics::Metrics::arrival_wait_seconds)
/// from when it gets round to handling it.
type Arrival = (LoadMsg, crate::metrics::Stamp);
// The boxed load future is `Send` on native (driven by a multi-thread executor
// like tokio) but `?Send` on wasm, where it holds JS futures (`!Send`) and runs
// single-threaded under `spawn_local`. Same server loop, two host models.
#[cfg(not(target_arch = "wasm32"))]
type BoxLoadFut = Pin<Box<dyn Future<Output = Arrival> + Send>>;
#[cfg(target_arch = "wasm32")]
type BoxLoadFut = Pin<Box<dyn Future<Output = Arrival>>>;
type LoadFuture = Abortable<BoxLoadFut>;

/// A load future that notes when it was woken.
///
/// The loads live in one `FuturesUnordered` that only the loop polls, so a
/// load whose data has arrived — woken by its socket, its decode pool, its
/// store — makes no progress until the loop comes round to it, which may be
/// after a whole traversal. Stamping when the future *resolves* would hide
/// exactly that, since it resolves only once it is polled. So the waker is
/// wrapped: every wake is stamped, the time from a wake to the poll that
/// answers it goes to `waits`, and the future's output carries the last wake
/// before it completed — what [`crate::metrics::Metrics::arrival_wait_seconds`]
/// is measured from. A load that completes on a poll nothing woke (its first,
/// or one the loop made on its own) carries the instant of that poll.
///
/// Wakes pass straight through to the loop's waker, and the same waker is
/// handed over on every poll so `will_wake` keeps answering yes: nothing about
/// when or how often the load is polled changes.
struct Woken<F: Future> {
    inner: Pin<Box<F>>,
    hook: Arc<WakeHook>,
    waits: &'static crate::metrics::Timer,
}

#[derive(Default)]
struct WakeHook {
    /// The waker of whoever polls us: the `FuturesUnordered`'s.
    waker: std::sync::Mutex<Option<std::task::Waker>>,
    /// The first wake since the last poll.
    woke: std::sync::Mutex<Option<crate::metrics::Stamp>>,
}

impl std::task::Wake for WakeHook {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        if let Ok(mut woke) = self.woke.lock() {
            woke.get_or_insert_with(crate::metrics::stamp);
        }
        if let Ok(waker) = self.waker.lock() {
            if let Some(waker) = waker.as_ref() {
                waker.wake_by_ref();
            }
        }
    }
}

impl<F: Future> Woken<F> {
    fn new(inner: F, waits: &'static crate::metrics::Timer) -> Self {
        Self {
            inner: Box::pin(inner),
            hook: Arc::new(WakeHook::default()),
            waits,
        }
    }
}

impl<F: Future> Future for Woken<F> {
    type Output = (F::Output, crate::metrics::Stamp);

    fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        if let Ok(mut waker) = self.hook.waker.lock() {
            match waker.as_mut() {
                Some(w) if w.will_wake(cx.waker()) => {}
                _ => *waker = Some(cx.waker().clone()),
            }
        }
        let woke = self.hook.woke.lock().ok().and_then(|mut w| w.take());
        if let Some(woke) = woke {
            self.waits.record(woke.elapsed());
        }
        let waker = std::task::Waker::from(Arc::clone(&self.hook));
        let mut inner_cx = std::task::Context::from_waker(&waker);
        match self.inner.as_mut().poll(&mut inner_cx) {
            Poll::Ready(out) => Poll::Ready((out, woke.unwrap_or_else(crate::metrics::stamp))),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Where a session's loads run.
///
/// # Inline — the default, and the only mode on wasm
///
/// Every load is a future inside the server's own, polled by the loop
/// between two traversals. Nothing advances while a pass runs: no body is
/// read, no next request is sent, no decode is submitted. It is also the
/// mode in which a session can be stepped by hand, one poll at a time, which
/// is what every test of the loop does.
///
/// A browser worker gains nothing from anything else: it has one thread, so
/// a load spawned beside the server still waits for the server to yield.
///
/// # Tokio — native, behind the `rt-tokio` feature
///
/// Every load is spawned on the host's runtime — a handle passed in, never a
/// runtime built here — and reports back through a channel the loop drains
/// in batches. Loads then advance on the other workers while a traversal
/// runs, which is where an arrival used to wait. Cancelling still goes
/// through the load's abort handle, and a result that lands after its load
/// was cancelled is recognised as such and dropped.
///
/// A load that changes the tree's shape (a reveal, a graft) could then land
/// in the middle of a pass; the server therefore asks the tree to
/// [defer its edits](TileTree::defer_edits) and applies them at the top of
/// every pass.
#[derive(Clone, Default)]
pub enum Spawn {
    /// Loads polled by the server's own future.
    #[default]
    Inline,
    /// Loads spawned as tasks on this runtime.
    #[cfg(all(feature = "rt-tokio", not(target_arch = "wasm32")))]
    Tokio(tokio::runtime::Handle),
}

impl std::fmt::Debug for Spawn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Inline => f.write_str("Inline"),
            #[cfg(all(feature = "rt-tokio", not(target_arch = "wasm32")))]
            Self::Tokio(_) => f.write_str("Tokio"),
        }
    }
}

/// A load's result, or the news that it was cancelled.
type Done = Result<Arrival, Aborted>;

/// The loads in flight, wherever they run. See [`Spawn`].
enum Loads {
    Inline(FuturesUnordered<LoadFuture>),
    #[cfg(all(feature = "rt-tokio", not(target_arch = "wasm32")))]
    Tasks(tasks::Tasks),
}

impl Loads {
    fn new(spawn: &Spawn) -> Self {
        match spawn {
            Spawn::Inline => Self::Inline(FuturesUnordered::new()),
            #[cfg(all(feature = "rt-tokio", not(target_arch = "wasm32")))]
            Spawn::Tokio(handle) => Self::Tasks(tasks::Tasks::new(handle.clone())),
        }
    }

    /// Whether loads run beside the server rather than inside it.
    fn spawns(&self) -> bool {
        !matches!(self, Self::Inline(_))
    }

    fn push(&mut self, load: LoadFuture, _handle: &AbortHandle) {
        match self {
            Self::Inline(loads) => loads.push(load),
            #[cfg(all(feature = "rt-tokio", not(target_arch = "wasm32")))]
            Self::Tasks(tasks) => tasks.spawn(load, _handle.clone()),
        }
    }

    /// The next result, waiting for it. `Pending` when there is nothing in
    /// flight as well as when nothing has landed yet.
    fn poll_next(&mut self, cx: &mut std::task::Context<'_>) -> Poll<Done> {
        match self {
            Self::Inline(loads) => match Pin::new(loads).poll_next(cx) {
                Poll::Ready(Some(done)) => Poll::Ready(done),
                Poll::Ready(None) | Poll::Pending => Poll::Pending,
            },
            #[cfg(all(feature = "rt-tokio", not(target_arch = "wasm32")))]
            Self::Tasks(tasks) => tasks.poll_next(cx),
        }
    }

    /// The next result that has already landed, without waiting.
    fn try_next(&mut self) -> Option<Done> {
        use futures_util::{FutureExt, StreamExt};
        match self {
            Self::Inline(loads) => loads.next().now_or_never().flatten(),
            #[cfg(all(feature = "rt-tokio", not(target_arch = "wasm32")))]
            Self::Tasks(tasks) => tasks.try_next(),
        }
    }
}

/// Loads as tokio tasks. See [`Spawn::Tokio`].
#[cfg(all(feature = "rt-tokio", not(target_arch = "wasm32")))]
mod tasks {
    use super::{AbortHandle, Aborted, Arrival, Done, LoadFuture};
    use std::task::{Context, Poll};
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

    /// A result, with the handle of the load that produced it.
    type Landed = (Arrival, AbortHandle);

    pub(super) struct Tasks {
        runtime: tokio::runtime::Handle,
        tx: UnboundedSender<Landed>,
        pub(super) rx: UnboundedReceiver<Landed>,
    }

    impl Tasks {
        pub(super) fn new(runtime: tokio::runtime::Handle) -> Self {
            let (tx, rx) = unbounded_channel();
            Self { runtime, tx, rx }
        }

        /// Runs `load` as a task of its own. Its abort handle travels with
        /// the result, because aborting a task that has already sent cannot
        /// take the message back.
        pub(super) fn spawn(&self, load: LoadFuture, handle: AbortHandle) {
            let tx = self.tx.clone();
            // Detached: the abort handle is the only way to stop it, and a
            // cancelled load wakes, sees it, and ends.
            drop(self.runtime.spawn(async move {
                if let Ok(msg) = load.await {
                    // The server may have gone: nobody left to tell.
                    let _ = tx.send((msg, handle));
                }
            }));
        }

        /// A result whose load was cancelled after it finished — the loop
        /// cancels only from its own thread, before it reads the next
        /// result, so the flag is never late — is the news of that cancel,
        /// exactly as an inline load would have reported it.
        pub(super) fn screen((msg, handle): Landed) -> Done {
            if handle.is_aborted() {
                Err(Aborted)
            } else {
                Ok(msg)
            }
        }

        pub(super) fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Done> {
            match self.rx.poll_recv(cx) {
                Poll::Ready(Some(landed)) => Poll::Ready(Self::screen(landed)),
                // Never: the sender lives as long as this.
                Poll::Ready(None) | Poll::Pending => Poll::Pending,
            }
        }

        pub(super) fn try_next(&mut self) -> Option<Done> {
            self.rx.try_recv().ok().map(Self::screen)
        }
    }
}

/// A pause the host's event loop gets to run in: see
/// [`GeometryServer::with_breather`]. `Send` on native, like the loads, and
/// not on wasm, where it is a JS timer.
#[cfg(not(target_arch = "wasm32"))]
pub type Pause = Pin<Box<dyn Future<Output = ()> + Send>>;
#[cfg(target_arch = "wasm32")]
pub type Pause = Pin<Box<dyn Future<Output = ()>>>;

/// Makes a [`Pause`].
pub type Breather = Arc<dyn Fn() -> Pause + Send + Sync>;

/// Creates a geometry server for a **3D Tiles** tileset, bound in-process to
/// one consumer. Convenience over [`in_process_with`]: it wraps the tileset in
/// a shared arena and builds the [`TilesetTree`]/[`TilesetLoader`] pair.
///
/// Drive [`GeometryServer::run`] on whatever executor fits your host
/// (a tokio task, a wasm spawn_local, `futures_executor` in tests) and hand
/// the [`InProcessStream`] to the renderer.
pub fn in_process<F: TileFetcher + 'static>(
    tileset: Tileset,
    fetcher: Arc<F>,
    config: Config,
) -> (InProcessStream, GeometryServer) {
    let arena = Arc::new(RwLock::new(tileset));
    let tree: Box<dyn TileTree> = Box::new(TilesetTree::new(Arc::clone(&arena)));
    let loader: Arc<dyn TileLoader> = Arc::new(TilesetLoader::new(arena, fetcher));
    in_process_with(tree, loader, config)
}

/// Creates a geometry server over **any** tile source: a [`TileTree`] for the
/// traversal and a [`TileLoader`] for async content. This is the seam the
/// globe (terrain × imagery) and compositions plug into — the server knows
/// neither glb nor terrain nor grafting.
pub fn in_process_with(
    tree: Box<dyn TileTree>,
    loader: Arc<dyn TileLoader>,
    config: Config,
) -> (InProcessStream, GeometryServer) {
    let (client, endpoint) = in_process_pair();
    let cache = ResidentCache::pinning(
        config.resident_budget_bytes,
        config.resident_tile_limit,
        config.pinned_level,
    );
    (
        client,
        GeometryServer {
            tree,
            loader,
            config,
            endpoint,
            cache,
            residency: ResidencyView::default(),
            in_flight: HashMap::new(),
            started: HashMap::new(),
            epochs: HashMap::new(),
            refreshing: HashSet::new(),
            settled_epoch: 0,
            acked: HashSet::new(),
            filled: HashSet::new(),
            priming: Vec::new(),
            priming_outstanding: HashSet::new(),
            priming_state: Priming::default(),
            priming_reported: None,
            views: Vec::new(),
            view_generation: 0,
            out: TraversalOutput::default(),
            selected: HashSet::new(),
            rendered_last: HashSet::new(),
            chosen_before: HashSet::new(),
            near: NearestContent::default(),
            view_moved: false,
            frame: 0,
            primed: false,
            breather: None,
            spawn: Spawn::Inline,
        },
    )
}

/// One streaming session: owns the tile tree, loader, residency and in-flight
/// state for one consumer. N sessions = N servers sharing nothing mutable.
pub struct GeometryServer {
    tree: Box<dyn TileTree>,
    loader: Arc<dyn TileLoader>,
    config: Config,
    endpoint: ServerEndpoint,
    cache: ResidentCache,
    residency: ResidencyView,
    in_flight: HashMap<TileId, AbortHandle>,
    /// The loader's [`epoch`](TileLoader::epoch) when each in-flight load was
    /// started — what its content will be recorded as, whatever the loader
    /// has moved on to by the time it lands.
    started: HashMap<TileId, u64>,
    /// The epoch each resident tile's content was loaded under. A tile whose
    /// entry is not the loader's present epoch is out of date and due a
    /// refresh — see [`Session::refresh`].
    epochs: HashMap<TileId, u64>,
    /// In-flight loads that are refreshes of a tile already resident, rather
    /// than first loads. They are not cancelled by a camera move, and one
    /// that fails costs the new picture, not the session.
    refreshing: HashSet<TileId>,
    /// The epoch every resident tile is known to be at, so a settled session
    /// pays one comparison a pass for the mechanism and nothing else.
    settled_epoch: u64,
    /// Tiles the consumer has confirmed it holds, from `ClientMessage::Ack`.
    ///
    /// The server's residency is not the consumer's: content is sent, then
    /// queued, then uploaded within a per-frame budget, and every step of that
    /// is a frame where the tile is resident here and absent there. That gap is
    /// what a stand-in covers, and an ack is the only report of it.
    acked: HashSet<TileId>,
    /// Tiles a stand-in has been sent for, so it is sent once and taken back
    /// when its ground stops being looked at.
    filled: HashSet<TileId>,
    priming: Vec<TileId>,
    priming_outstanding: HashSet<TileId>,
    priming_state: Priming,
    priming_reported: Option<Priming>,
    views: Vec<ViewState>,
    view_generation: u64,
    out: TraversalOutput,
    selected: HashSet<TileId>,
    rendered_last: HashSet<TileId>,
    chosen_before: HashSet<TileId>,
    /// The scene-wide distance a uniform-detail pass is priced at, kept from
    /// one pass to the next while the camera and the tree's shape hold still.
    near: NearestContent,
    view_moved: bool,
    frame: u64,
    /// Whether the coarse pyramid has been asked for yet. See
    /// [`Session::prime`].
    primed: bool,
    /// Handing the host its turn: after this many events in a row with no
    /// wait between them, the pause. See [`GeometryServer::with_breather`].
    breather: Option<(usize, Breather)>,
    /// Where the loads run. See [`Spawn`].
    spawn: Spawn,
}

/// Takes back what a session said of itself, once it has gone.
///
/// A session's gauges are set as it works and were left as they last read
/// when it went: a second session in one process — a page that mounts
/// another viewer, a headless run after another — then read the first one's
/// residency and selection as its own until its first pass. A session that
/// has gone holds nothing and selects nothing. Held by [`GeometryServer::run`]
/// for as long as the session runs, so it is let go of however the future
/// ends: finished, or dropped.
struct Gone;

impl Drop for Gone {
    fn drop(&mut self) {
        let m = crate::metrics::metrics();
        for gauge in [
            &m.tiles_visited,
            &m.tiles_culled,
            &m.tiles_selected,
            &m.gaps,
            &m.resident_bytes,
            &m.imagery_bytes,
            &m.imagery_textures,
            &m.loads_in_flight,
            &m.tiles_filled,
            &m.priming_total,
            &m.priming_pending,
            &m.priming_unavailable,
            &m.priming_done,
        ] {
            gauge.set(0);
        }
    }
}

impl GeometryServer {
    /// Runs this session's loads as given. Default: [`Spawn::Inline`].
    #[must_use]
    pub fn with_spawn(mut self, spawn: Spawn) -> Self {
        self.spawn = spawn;
        self
    }

    /// Hands the host its turn after every `every` events handled back to
    /// back, by awaiting `pause()`.
    ///
    /// A load answered from memory completes on its first poll, and so does
    /// the traversal it triggers, and the next load: a burst can run for
    /// seconds without the loop ever waiting. On a desktop's thread pool that
    /// costs a core. In a browser's worker, which has one thread and receives
    /// the camera as messages, it is a server that hears nothing — no camera,
    /// no acks — while it works, and a page that stops refining under a
    /// moving view. The worker's pause is a timer, which lets its messages in.
    #[must_use]
    pub fn with_breather(mut self, every: usize, pause: Breather) -> Self {
        self.breather = Some((every.max(1), pause));
        self
    }

    /// Runs the session until the consumer goes away.
    pub async fn run(mut self) {
        let _gone = Gone;
        let mut loads = Loads::new(&self.spawn);
        if loads.spawns() {
            // A load landing on another thread must not reshape the tree under
            // a pass: its edits wait for the top of the next one.
            self.tree.defer_edits();
        }
        let breather = self.breather.take().or_else(|| default_breather(&loads));
        let mut busy = 0usize;
        let mut rx = self.endpoint.rx;
        let tx = self.endpoint.tx.clone();
        let mut session = Session {
            tree: self.tree.as_ref(),
            loader: &self.loader,
            config: &self.config,
            cache: &mut self.cache,
            residency: &mut self.residency,
            in_flight: &mut self.in_flight,
            started: &mut self.started,
            epochs: &mut self.epochs,
            refreshing: &mut self.refreshing,
            settled_epoch: &mut self.settled_epoch,
            priming: &mut self.priming,
            priming_outstanding: &mut self.priming_outstanding,
            priming_state: &mut self.priming_state,
            priming_reported: &mut self.priming_reported,
            views: &mut self.views,
            view_generation: &mut self.view_generation,
            out: &mut self.out,
            selected: &mut self.selected,
            rendered_last: &mut self.rendered_last,
            chosen_before: &mut self.chosen_before,
            near: &mut self.near,
            acked: &mut self.acked,
            filled: &mut self.filled,
            view_moved: &mut self.view_moved,
            frame: &mut self.frame,
            primed: &mut self.primed,
        };

        enum Event {
            Load(Result<Arrival, Aborted>),
            Client(ClientMessage),
            Closed,
        }

        // Which source gets looked at first, flipped every iteration.
        //
        // A fixed order starves one side or the other. Loads first — the
        // original — meant that while completions kept arriving the camera was
        // never read at all, and every decision was taken on a view it had
        // already left: measured with the altitude climbing from 235 km to
        // 4274 km while `selected`, `visited` and `culled` did not move a
        // single count. Client first starves the mirror image, since a viewer
        // sends a state every frame and completions would go unread.
        //
        // Alternating costs a branch. Within one side, what is already waiting
        // is taken together: the camera's burst keeps only its last state, and
        // the loads that have landed are delivered before the one traversal
        // that answers them all (`drain_ready`).
        let mut client_first = true;

        loop {
            // Whether this turn had to wait: a turn that waited has given the
            // host its turn already.
            let mut waited = false;
            let event = poll_fn(|cx| {
                let mut poll_client =
                    |cx: &mut std::task::Context<'_>| match Pin::new(&mut rx).poll_next(cx) {
                        Poll::Ready(Some(msg)) => Poll::Ready(Some(Event::Client(msg))),
                        Poll::Ready(None) => Poll::Ready(Some(Event::Closed)),
                        Poll::Pending => Poll::Ready(None),
                    };
                let polled = if client_first {
                    if let Poll::Ready(Some(event)) = poll_client(cx) {
                        return Poll::Ready(event);
                    }
                    loads.poll_next(cx).map(Event::Load)
                } else {
                    if let Poll::Ready(done) = loads.poll_next(cx) {
                        return Poll::Ready(Event::Load(done));
                    }
                    match poll_client(cx) {
                        Poll::Ready(Some(event)) => Poll::Ready(event),
                        _ => Poll::Pending,
                    }
                };
                if polled.is_pending() {
                    waited = true;
                }
                polled
            })
            .await;
            client_first = !client_first;
            busy = if waited { 0 } else { busy + 1 };
            if let Some((every, pause)) = &breather {
                if busy >= *every {
                    pause().await;
                    busy = 0;
                }
            }

            // Why this session ended, named rather than implied.
            //
            // Every one of these used to be a bare `break`. A server that has
            // stopped is then indistinguishable from a server with nothing to
            // do: same silence, same last log line, and a consumer that keeps
            // sending into a channel nobody reads (`let _ = stream.send(..)`).
            // A whole afternoon went into deciding, from the outside, whether
            // this loop was still turning. It says so now.
            let ended: Option<String> = match event {
                Event::Client(first) => {
                    let mut dirty = session.on_client(first);
                    // Coalesce the burst: a viewer sends a state per frame, and
                    // only the last one is worth traversing for.
                    while let Ok(more) = rx.try_recv() {
                        dirty |= session.on_client(more);
                    }
                    if dirty {
                        session
                            .retraverse(&tx, &mut loads)
                            .err()
                            .map(|stop| stop.why("answering a camera move"))
                    } else {
                        None
                    }
                }
                Event::Load(Ok((msg, landed))) => {
                    let m = crate::metrics::metrics();
                    m.arrival_wait_seconds.record(landed.elapsed());
                    // Coalesce the arrivals, as the camera's burst is above:
                    // every completion that is already ready is delivered
                    // before the one traversal that answers them all.
                    match session
                        .on_load_done(msg, &tx)
                        .and_then(|()| drain_ready(&mut session, &mut loads, &tx))
                    {
                        Err(stop) => Some(stop.why("delivering a tile")),
                        Ok(more) => {
                            m.arrivals_per_pass.record(1 + more);
                            session
                                .retraverse(&tx, &mut loads)
                                .err()
                                .map(|stop| stop.why("answering an arrival"))
                        }
                    }
                }
                // Cancelled load: cleaned up at abort time.
                Event::Load(Err(Aborted)) => None,
                Event::Closed => Some("the client hung up".to_string()),
            };
            if let Some(why) = ended {
                tracing::error!(
                    reason = why,
                    traversals = crate::metrics::metrics().traversals.get(),
                    "geometry server stopping"
                );
                break;
            }
        }
        // Spawned loads outlive the loop otherwise: nobody would read them.
        for handle in session.in_flight.values() {
            handle.abort();
        }
    }
}

/// Delivers every load completion that is already ready, without waiting.
///
/// A traversal ran on every single arrival before this: a burst of tiles
/// answered from memory or from a warm store cost one full walk from the
/// roots each, and the walk is what the loop spends its time in. Each of
/// those passes but the last was superseded before anything could act on
/// it — a consumer drains every queued message before it draws, so it only
/// ever saw the last `Select` of the burst anyway.
///
/// Bounded by the fetch cap: at most that many loads can be in flight, so
/// a drain cannot outlast the burst it was called for, and the camera is
/// read again after it. A batch counts as one event for the breather.
/// Returns how many it delivered, for `arrivals_per_pass`.
fn drain_ready(
    session: &mut Session<'_>,
    loads: &mut Loads,
    tx: &UnboundedSender<ServerMessage>,
) -> Result<u64, Stop> {
    let m = crate::metrics::metrics();
    let mut delivered = 0;
    for _ in 0..session.config.maximum_simultaneous_fetches {
        match loads.try_next() {
            Some(Ok((msg, landed))) => {
                m.arrival_wait_seconds.record(landed.elapsed());
                session.on_load_done(msg, tx)?;
                delivered += 1;
            }
            // Cancelled: cleaned up at abort time, as in the loop.
            Some(Err(Aborted)) => {}
            // Nothing in flight, or nothing ready yet.
            None => break,
        }
    }
    Ok(delivered)
}

/// The pause a session takes when its host gave none: for spawned loads, a
/// turn handed back to the runtime every so often, so a server answering a
/// long burst from memory does not keep its worker from the tasks queued on
/// it. Inline, none — the host decides, as it always has.
fn default_breather(_loads: &Loads) -> Option<(usize, Breather)> {
    #[cfg(all(feature = "rt-tokio", not(target_arch = "wasm32")))]
    if _loads.spawns() {
        let pause: Breather = Arc::new(|| Box::pin(tokio::task::yield_now()));
        return Some((16, pause));
    }
    None
}

/// Borrowed view of the server state for the message handlers.
struct Session<'a> {
    tree: &'a dyn TileTree,
    loader: &'a Arc<dyn TileLoader>,
    config: &'a Config,
    cache: &'a mut ResidentCache,
    residency: &'a mut ResidencyView,
    in_flight: &'a mut HashMap<TileId, AbortHandle>,
    started: &'a mut HashMap<TileId, u64>,
    epochs: &'a mut HashMap<TileId, u64>,
    refreshing: &'a mut HashSet<TileId>,
    settled_epoch: &'a mut u64,
    /// Coarse tiles asked for once at startup; drained as fetch slots free up.
    priming: &'a mut Vec<TileId>,
    /// Every primed tile that has not yet been answered — still queued above,
    /// or in flight. A tile leaves this set exactly once, when its content is
    /// delivered or when the session gives up on it, which is what makes
    /// [`Priming::settled`] a condition a host can wait on. Kept apart from the
    /// queue because a tile popped from the queue is not yet resolved, and it
    /// was that gap the window used to hang in.
    priming_outstanding: &'a mut HashSet<TileId>,
    /// What the coarse pyramid looks like right now, and what was last sent —
    /// so the report goes out on a change rather than on every pass.
    priming_state: &'a mut Priming,
    priming_reported: &'a mut Option<Priming>,
    views: &'a mut Vec<ViewState>,
    view_generation: &'a mut u64,
    out: &'a mut TraversalOutput,
    /// What this pass touched: the frontier, every ancestor of it, and
    /// everything it asked for. Never a victim — the reference implementation's
    /// "used this frame", which it will go over its cap rather than reclaim.
    selected: &'a mut HashSet<TileId>,
    /// What the previous pass drew. Consulted by the descendant limit — see
    /// [`crate::traversal::Config::loading_descendant_limit`].
    rendered_last: &'a mut HashSet<TileId>,
    /// What the last pass under the previous views chose at its own level:
    /// the refine hysteresis's memory (`Config::refine_hysteresis`). Moved on
    /// only when the camera moves, so every pass of one frame refines against
    /// the same memory and the frame it settles on does not depend on how
    /// its tiles arrived.
    chosen_before: &'a mut HashSet<TileId>,
    near: &'a mut NearestContent,
    acked: &'a mut HashSet<TileId>,
    filled: &'a mut HashSet<TileId>,
    /// Set when the camera moved, which is the only time in-flight loads may be
    /// cancelled. Nothing else reads it: residency is ordered by use, not by
    /// which view asked.
    view_moved: &'a mut bool,
    frame: &'a mut u64,
    primed: &'a mut bool,
}

/// Why the run loop unwinds. Both end the session; only the reason differs,
/// and only the reason is worth a log line.
enum Stop {
    /// The consumer went away: nothing left to send to.
    Gone,
    /// A tile could not be loaded, after the transport had already retried it.
    LoadFailed { tile: TileId, message: String },
}

impl Stop {
    /// What to say about it, given what the session was doing at the time.
    fn why(&self, during: &str) -> String {
        match self {
            Self::Gone => format!("the consumer went away while {during}"),
            Self::LoadFailed { tile, message } => format!(
                "tile {tile:?} could not be loaded and the transport had \
                 already retried it, so the session ends here rather than \
                 serving ground with a hole in it: {message}"
            ),
        }
    }
}

impl Session<'_> {
    /// Returns true when a re-traversal is needed.
    fn on_client(&mut self, msg: ClientMessage) -> bool {
        match msg {
            ClientMessage::ViewerState { views, generation } => {
                *self.views = views;
                *self.view_generation = generation;
                // A new camera position: the traversal it triggers starts a new
                // generation, and the one it displaces becomes history rather
                // than being forgotten.
                *self.view_moved = true;
                true
            }
            // Recorded, not acted on: it changes nothing about what to load —
            // residency is server-side — and it is the only evidence of what
            // the consumer actually holds, which is what decides whether a
            // stand-in is needed. It used to be dropped here.
            ClientMessage::Ack { tile } => {
                self.acked.insert(tile);
                // The ack is the proof the stand-in is gone: real geometry is
                // filed under the same id, so uploading it replaced the fill
                // on the consumer's side. Nothing left to take back.
                self.filled.remove(&tile);
                false
            }
            ClientMessage::Cancel { tile } => {
                if let Some(handle) = self.in_flight.remove(&tile) {
                    handle.abort();
                    self.started.remove(&tile);
                    self.refreshing.remove(&tile);
                }
                false
            }
            // Swept now, against what the last pass protected — the frame
            // the consumer just saw converge — and not at the next pass,
            // whose views depend on whether the next camera has arrived
            // yet. Forgotten here as the pass forgets what it reclaims;
            // announced with the next pass's `Evict`.
            ClientMessage::Trim { budget_bytes } => {
                let released = self
                    .cache
                    .release_beyond(budget_bytes, self.selected)
                    .to_vec();
                for tile in &released {
                    self.residency.remove(*tile);
                    self.epochs.remove(tile);
                    self.acked.remove(tile);
                    self.filled.remove(tile);
                }
                tracing::info!(
                    budget_mib = budget_bytes >> 20,
                    released = released.len(),
                    kept = self.residency.len(),
                    "trimmed the residency"
                );
                false
            }
        }
    }

    /// Sends a stand-in for every selected tile the consumer does not hold, and
    /// takes back the ones whose ground it has stopped looking at.
    ///
    /// # The gap
    ///
    /// A tile is selected only once it is resident *here*. It is drawn only once
    /// it is uploaded *there*, and between the two lie a channel, a queue and a
    /// per-frame upload budget. In those frames the consumer falls back to the
    /// nearest ancestor it holds — which covers the missing tile's siblings as
    /// well, so two approximations of one hillside end up over the same ground
    /// and the depth test picks a winner per pixel. That is the shimmer.
    ///
    /// An ack is the only report of that gap, which is why one is now kept.
    ///
    /// # Bookkeeping
    ///
    /// Sent once per tile, and taken back when the tile leaves the selection.
    /// A stand-in is *not* evicted when the real content arrives: the consumer
    /// files both under the same id, so the real one simply replaces it. The set
    /// is bounded by the selection, so a session cannot accumulate them.
    fn fill_the_gaps(&mut self, tx: &UnboundedSender<ServerMessage>) -> Result<(), Stop> {
        let mut sent = Vec::new();
        for (tile, _) in self.out.selected.iter() {
            if self.acked.contains(tile) || self.filled.contains(tile) {
                continue;
            }
            // Synchronous and cache-only by contract: this runs inside the
            // traversal, and a stand-in that waited on the network would arrive
            // together with the tile it was standing in for.
            if let Some(content) = self.loader.fill(*tile) {
                self.filled.insert(*tile);
                sent.push((*tile, content));
            }
        }
        let just_sent = sent.len();
        for (tile, content) in sent {
            tx.unbounded_send(ServerMessage::Fill {
                tile,
                ancestry: self.ancestry(tile),
                content,
            })
            .map_err(|_| Stop::Gone)?;
        }

        // Ground the camera has left: take those stand-ins back, so the
        // consumer does not keep a surface for every tile ever briefly
        // selected — a leak the server cannot see, because it never charged
        // itself for any of them.
        //
        // **As a `Retire`, never as an `Evict`.** `Content` is sent once per
        // residency, so an `Evict` reaching a consumer whose real copy is
        // still in its upload queue destroys the only delivery there will
        // ever be — the ground then wears its stand-in for as long as the
        // cache holds the tile. Two guards were tried before this message
        // existed and both leaked: clearing `filled` when the content is sent
        // races the traversal in between, and sparing acked tiles turns
        // `filled` into a set nothing ever prunes (measured at 3912 and
        // climbing). `Retire` removes the race by construction: the consumer
        // drops only what it holds as a stand-in, so sending it on stale
        // knowledge costs nothing.
        let stale: Vec<TileId> = self
            .filled
            .iter()
            .copied()
            .filter(|t| !self.selected.contains(t))
            .collect();
        for tile in &stale {
            self.filled.remove(tile);
        }
        if !stale.is_empty() {
            tracing::debug!(
                taken = stale.len(),
                held = self.filled.len(),
                "stand-ins retired"
            );
            tx.unbounded_send(ServerMessage::Retire { tiles: stale })
                .map_err(|_| Stop::Gone)?;
        }
        if just_sent > 0 {
            tracing::debug!(just_sent, held = self.filled.len(), "stand-in surfaces");
        }
        crate::metrics::metrics()
            .tiles_filled
            .set(self.filled.len() as u64);
        Ok(())
    }

    /// What the tree knows about a tile, for the wire.
    ///
    /// One place, so no emission site is tempted to derive it from the handle —
    /// which is the mistake this whole change exists to remove.
    fn ancestry(&self, tile: TileId) -> crate::protocol::Ancestry {
        crate::protocol::Ancestry {
            level: self.tree.level(tile),
            parent: self.tree.parent(tile),
        }
    }

    fn retraverse(
        &mut self,
        tx: &UnboundedSender<ServerMessage>,
        loads: &mut Loads,
    ) -> Result<(), Stop> {
        // Whether this pass was provoked by the camera or by a tile arriving.
        // It decides one thing only: whether in-flight loads may be cancelled
        // below. Consumed here, so a pass provoked by an arrival does not
        // inherit the last camera move's licence to cancel.
        let camera_moved = std::mem::take(self.view_moved);
        // What loads changed in the tree's shape since the last pass, applied
        // before anything reads it — so the whole pass walks one tree. Nothing
        // is held unless loads run as tasks (`Spawn`); inline, the edits have
        // already landed, between passes, as they always did.
        if let Some((tile, why)) = self.tree.apply_deferred_edits().into_iter().next() {
            return self.fail(tile, why, tx);
        }
        *self.frame += 1;
        self.prime();
        // Open a new pass before anything is touched: from here on, every tile
        // this traversal reaches is off limits to the sweep at the end of it.
        self.cache.start_pass();
        if camera_moved {
            // `out` still holds the last pass under the views just left.
            self.chosen_before.clear();
            self.chosen_before.extend(self.out.chosen.iter().copied());
        }
        let started = crate::metrics::stamp();
        traverse_with(
            self.tree,
            self.residency,
            self.views,
            self.config,
            *self.frame,
            self.rendered_last,
            self.chosen_before,
            self.near,
            self.out,
        );
        let walked = started.elapsed();
        // Every pass of every session these tests drive is checked against
        // the reference pass, which works everything out from scratch.
        #[cfg(test)]
        tests::matches_the_reference_pass(self);
        // What this pass actually put on screen, for the next one to consult.
        // A held REPLACE only cuts its subtree loose when it has nothing of its
        // own drawn yet — once it is on screen there is no black to avoid, and
        // cutting off then would stall refinement rather than accelerate it.
        self.rendered_last.clear();
        self.rendered_last
            .extend(self.out.selected.iter().map(|(tile, _)| *tile));
        // Every pass, with everything that decided it.
        //
        // A selection is meant to be a function of (stage, time). It is not,
        // and this is the trace that shows why: the passes of one render are
        // printed in order, so two runs of the SAME frame can be diffed pass
        // by pass and the exact pass where they part company can be named.
        // Without it the only observable was the final tile count, which says
        // that two runs disagreed but never where.
        crate::det!(
            "pass",
            n = *self.frame,
            selected = self.out.selected.len(),
            sel_digest = crate::determinism::digest(self.out.selected.iter().map(|(t, _)| t.0)),
            requested = self.out.requests.len(),
            req_digest = crate::determinism::digest(self.out.requests.iter().map(|r| r.tile.0)),
            visited = self.out.stats.visited,
            culled = self.out.stats.culled,
            gaps = self.out.stats.gaps,
            // The two counters that say the traversal answered the CLOCK
            // rather than the scene. `deferred_subtrees` above zero means a
            // hold cancelled its subtree because the network was slow, which
            // is how one frame selected 106 tiles on five runs and 7 on the
            // sixth. Untraced, it took an afternoon to find; traced, it is one
            // line of the diff.
            deferred = self.out.stats.deferred_subtrees,
            held_but_drawn = self.out.stats.held_but_drawn,
            resident = self.residency.len(),
            in_flight = self.in_flight.len(),
            priming_left = self.priming.len(),
            camera_moved = camera_moved,
        );
        let m = crate::metrics::metrics();
        m.traversals.inc();
        m.traversal_seconds.record(walked);
        m.tiles_visited.set(u64::from(self.out.stats.visited));
        m.tiles_culled.set(u64::from(self.out.stats.culled));
        m.tiles_selected.set(u64::from(self.out.stats.selected));
        m.gaps.set(u64::from(self.out.stats.gaps));
        m.selected_by_level.clear();
        for (tile, _) in &self.out.selected {
            m.selected_by_level.inc(self.tree.level(*tile));
        }
        m.queued_by_level.clear();
        for req in &self.out.requests {
            m.queued_by_level.inc(self.tree.level(req.tile));
        }

        // `selected` is the *protected* set: what the budget may not evict.
        // The frontier is always in it — never drop what is on screen.
        self.selected.clear();
        for (t, _) in &self.out.selected {
            self.selected.insert(*t);
            self.cache.touch(*t);
        }
        // Pin the path from each selected tile up to the root: keep ancestors
        // resident (protected from eviction, kept LRU-warm) so the consumer
        // always has a coarser fallback to draw while finer tiles stream in —
        // no holes. Pinned, but NOT sent as selection (they're not the
        // frontier; the consumer renders them only where finer tiles are not
        // ready yet).
        //
        // The whole chain, not a band near the frontier. A traversal asks for
        // every ancestor it does not have, so an unpinned one is evicted and
        // re-requested at once: the session then spends itself reloading the
        // same tiles — 44k loads for 1.2k tiles, in the run that proved it —
        // and never converges. When the working set genuinely exceeds the
        // budget the cache goes over it instead, which is degraded but
        // progressing.
        let frontier: Vec<TileId> = self.out.selected.iter().map(|(t, _)| *t).collect();
        for tile in frontier {
            let mut ancestor = self.tree.parent(tile);
            while let Some(a) = ancestor {
                if !self.selected.insert(a) {
                    break; // this ancestor (and its chain) is already pinned
                }
                self.cache.touch(a);
                ancestor = self.tree.parent(a);
            }
        }
        // Whatever this traversal still asks for is protected too. Evicting a
        // tile the very next traversal re-requests is thrash: it would load,
        // evict something else, be requested again, and the session would spin
        // forever without converging. When the working set genuinely does not
        // fit, the cache goes over budget instead (its documented fallback) —
        // degraded, but progressing.
        for req in &self.out.requests {
            self.selected.insert(req.tile);
        }
        // And so is what a held REPLACE is waiting on. Those are resident, not
        // drawn and not requested — the one category a residency built from
        // "selected plus requested" cannot see — so without this they are swept
        // and asked for again on the very next pass, for ever.
        for tile in &self.out.awaiting {
            self.selected.insert(*tile);
            self.cache.touch(*tile);
        }
        // The protected set just changed, so content held only by the view the
        // camera has left is now the most depletable thing in the cache. Doing
        // this here — and not only when a load lands — is what lets a settled
        // camera give memory back at all.
        let mut reclaimed = self.cache.take_released();
        reclaimed.extend(self.cache.trim(self.selected));
        for tile in &reclaimed {
            self.residency.remove(*tile);
            self.epochs.remove(tile);
            // Same hygiene as the insert-path eviction: the `Evict` below takes
            // the consumer's copy and its stand-in alike, so both records stop
            // being true here. `acked` left behind is the worse of the two — it
            // suppresses the stand-in the next time this ground is looked at,
            // and grows for the life of the session.
            self.acked.remove(tile);
            self.filled.remove(tile);
        }
        // Counted here as well as on insert. This is now the path that reclaims
        // most of what a session gives back — expiry fires while there is still
        // budget headroom, so an insert-only count would have kept reading zero
        // and said nothing had been freed.
        let m = crate::metrics::metrics();
        m.tiles_evicted.add(reclaimed.len() as u64);
        m.resident_bytes.set(self.cache.used_bytes() as u64);
        let (textures, imagery_bytes) = self.cache.imagery_bytes();
        m.imagery_textures.set(textures as u64);
        m.imagery_bytes.set(imagery_bytes as u64);
        // Selection first, reclaim second — and the order is load-bearing.
        //
        // A consumer drains every queued message in one go before it draws, so
        // it applies both in the same frame whatever the order. But it walks up
        // from each selected tile to the nearest ancestor it actually holds, and
        // draws that when the fine tile has not landed. Reclaim first and that
        // ancestor can be gone before the selection naming it is even read: the
        // ground it was covering has nothing left to fall back to, and goes
        // black. Announcing what is wanted before taking anything away costs a
        // line and removes the window entirely.
        let selecting = crate::metrics::stamp();
        let sent = tx.unbounded_send(ServerMessage::Select {
            tiles: self.out.selected.clone(),
            // `self.selected` is the protected closure — the frontier, the chain
            // from each of its tiles up to the root, what is requested and what
            // a held REPLACE waits on. That is exactly the set a consumer's walk
            // can pass through, so it is exactly the set whose shape it needs.
            ancestry: self
                .selected
                .iter()
                .map(|tile| (*tile, self.ancestry(*tile)))
                .collect(),
            stats: self.out.stats,
            generation: *self.view_generation,
        });
        m.select_seconds.record(selecting.elapsed());
        sent.map_err(|_| Stop::Gone)?;
        if !reclaimed.is_empty() {
            tx.unbounded_send(ServerMessage::Evict { tiles: reclaimed })
                .map_err(|_| Stop::Gone)?;
        }
        if self.config.stand_ins {
            self.fill_the_gaps(tx)?;
        }

        // Cancel in-flight loads the camera has moved away from — and only
        // then.
        //
        // A traversal runs on **every load completion**, not only on a camera
        // move, and cancelling on all of them livelocks. One tile arrives, the
        // pass re-runs, `forbid_holes` rolls back a different branch, other
        // tiles fall out of the request set and are killed; the next pass asks
        // for them again, they are spawned again, and the pass after that kills
        // them again. Nothing ever finishes. The symptom is a request count
        // frozen at some number, a selection that stops following the camera,
        // and — the part that cost the most time — *no log output at all*, since
        // every load dies before it can say anything.
        //
        // A load provoked by a tile arriving is still wanted; the pass simply
        // reordered what it needs first. Only a camera move can make one
        // pointless, and even then finishing is often cheaper than restarting.
        // A primed tile is never stale, whatever the camera does. It is not
        // wanted for this view — it is the floor every view falls back to — so
        // it appears in no request set, and killing it would drop it out of the
        // pyramid for good: popped from the queue, aborted, never re-asked, and
        // a host counting arrivals waits on it for ever.
        if camera_moved {
            let wanted: HashSet<TileId> = self.out.requests.iter().map(|r| r.tile).collect();
            let stale: Vec<TileId> = self
                .in_flight
                .keys()
                // Nor is a refresh: it is wanted by no view in particular and
                // by every view that will ever look at its tile, and the old
                // picture is on screen until it lands.
                .filter(|t| {
                    !wanted.contains(t)
                        && !self.priming_outstanding.contains(t)
                        && !self.refreshing.contains(t)
                })
                .copied()
                .collect();
            for t in stale {
                if let Some(handle) = self.in_flight.remove(&t) {
                    handle.abort();
                    self.started.remove(&t);
                    crate::metrics::metrics().loads_cancelled.inc();
                }
            }
        }

        // The coarse pyramid first, for as long as there is any of it left.
        //
        // Nothing it competes with is on screen: the host holds its window back
        // until this is on the GPU, so a frontier tile fetched now is a tile
        // fetched for nobody. It is finite — a few thousand tiles — and drains
        // once, at the start of a session, after which this costs a branch.
        while let Some(&tile) = self.priming.last() {
            if self.in_flight.len() >= self.config.maximum_simultaneous_fetches {
                break;
            }
            self.priming.pop();
            // Answered before its turn came round: the traversal asked for
            // it first. Resolved here rather than left outstanding for a load
            // that will never be started.
            if self.residency.is_resident(tile) {
                self.resolve_primed(tile, true);
                continue;
            }
            if self.in_flight.contains_key(&tile) {
                continue;
            }
            // Protected like anything else asked for, so a sweep between now
            // and its arrival cannot make the work pointless.
            self.selected.insert(tile);
            self.start(tile, loads);
        }
        // Spawn new loads, highest priority first, within the cap. The
        // traversal only requests tiles that have content, so the loader is
        // never asked to load a structural-empty tile.
        let wanted: Vec<TileId> = self.out.requests.iter().map(|r| r.tile).collect();
        for tile in wanted {
            if self.in_flight.len() >= self.config.maximum_simultaneous_fetches {
                break;
            }
            if self.residency.is_resident(tile) || self.in_flight.contains_key(&tile) {
                continue;
            }
            self.start(tile, loads);
        }

        // Last, with whatever slots the view itself left free.
        self.refresh(loads);

        if *self.primed && self.priming_outstanding.is_empty() {
            crate::metrics::metrics().priming_done.set(1);
        }
        self.report_priming(tx)?;
        Ok(())
    }

    /// Starts loading `tile`, and notes the epoch it was started under.
    fn start(&mut self, tile: TileId, loads: &mut Loads) {
        let loader = Arc::clone(self.loader);
        // Read here and not when the load lands: the loader may move on while
        // the load is in flight, and content begun under the old epoch must be
        // recorded as old — recorded as new, it would never be refreshed, and
        // the ground would keep the previous picture for good. The other error
        // is harmless: content that turns out newer than its record is
        // refreshed once more than it needed.
        self.started.insert(tile, loader.epoch());
        let (handle, registration) = AbortHandle::new_pair();
        let fut: BoxLoadFut = Box::pin(Woken::new(
            async move { (tile, loader.load(tile).await) },
            &crate::metrics::metrics().load_wake_wait_seconds,
        ));
        loads.push(Abortable::new(fut, registration), &handle);
        self.in_flight.insert(tile, handle);
        let m = crate::metrics::metrics();
        m.loads_started.inc();
        m.loads_by_level.inc(self.tree.level(tile));
        m.loads_in_flight.set(self.in_flight.len() as u64);
    }

    /// Loads again every resident tile whose content is from an earlier
    /// [`epoch`](TileLoader::epoch) of the loader — a globe told to drape
    /// other imagery — **without ever letting go of the content it has.**
    ///
    /// # The order is the whole point
    ///
    /// The obvious way to change what every tile shows is to drop the tiles
    /// and let the traversal ask for them again. That is a globe with nothing
    /// on it for as long as the network takes, and nothing on it is black.
    /// So nothing is dropped. An out-of-date tile stays resident, stays in
    /// the selection and stays with the consumer; its replacement is loaded
    /// beside it and sent as a second [`ServerMessage::Content`] under the
    /// same id, which a consumer applies by building the new surface and only
    /// then releasing the old — one filing, replaced in place. At every
    /// instant each tile has exactly one picture: the old one, then the new.
    ///
    /// # Coarse first, what is looked at first
    ///
    /// The protected set — the frontier, its ancestors, what a held `REPLACE`
    /// waits on — goes before everything else, and within each group the
    /// coarsest tiles go first: one coarse tile repaints a whole region, and
    /// it is also the fallback every finer tile climbs to, so the change
    /// reads as the globe turning over and then sharpening, not as confetti.
    /// The rest of the residency follows; it is not on screen, but it is what
    /// the camera returns to.
    ///
    /// Refreshes take only the fetch slots the view's own requests left free:
    /// ground that has no picture at all outranks ground that has last
    /// epoch's.
    fn refresh(&mut self, loads: &mut Loads) {
        let now = self.loader.epoch();
        if *self.settled_epoch == now {
            return;
        }
        let mut stale: Vec<(bool, u32, TileId)> = self
            .epochs
            .iter()
            .filter(|(tile, epoch)| **epoch != now && !self.in_flight.contains_key(*tile))
            .map(|(tile, _)| (!self.selected.contains(tile), self.tree.level(*tile), *tile))
            .collect();
        // Nothing out of date, and nothing in flight that will be recorded as
        // out of date when it lands: the turn-over is complete.
        if stale.is_empty() && self.started.values().all(|epoch| *epoch == now) {
            *self.settled_epoch = now;
            tracing::info!(epoch = now, "every resident tile is at the loader's epoch");
            return;
        }
        // Protected before the rest, coarse before fine, and the id last so
        // that two runs refresh in the same order.
        stale.sort_unstable_by_key(|(unprotected, level, tile)| (*unprotected, *level, tile.0));
        for (_, _, tile) in stale {
            if self.in_flight.len() >= self.config.maximum_simultaneous_fetches {
                break;
            }
            self.refreshing.insert(tile);
            self.start(tile, loads);
        }
    }

    /// Asks for the whole coarse pyramid, once, at the start of a session.
    ///
    /// These tiles are pinned in the residency and are what every fallback
    /// walks up to, so they should be on the GPU before anything needs them —
    /// not discovered by a camera that happens to fly over them. Measured
    /// without this, on ground already flown several times: level 5 held 291 of
    /// its 1024 tiles and level 3 held none at all, so a fast movement fell
    /// through the safety net onto bare ground.
    ///
    /// They are **requested, not selected**. `prepared` and `selection` are
    /// separate sets in the consumer, so a tile whose content arrives is
    /// uploaded and held without being drawn — which is exactly what a fallback
    /// is: ready, and invisible until something needs it.
    ///
    /// The count is bounded by the tree, not by the view: `4^level` tiles, so
    /// 1365 of them through level 5, about 480 MiB of imagery on the GPU
    /// against a budget measured in gigabytes.
    fn prime(&mut self) {
        if *self.primed {
            return;
        }
        let m = crate::metrics::metrics();
        let Some(level) = self.config.pinned_level else {
            *self.primed = true;
            // Nothing to wait for, so anyone gating on this may start at once.
            m.priming_done.set(1);
            return;
        };
        *self.primed = true;

        let mut wanted = Vec::new();
        let mut frontier = self.tree.roots();
        for _ in 0..=level {
            let mut next = Vec::new();
            for tile in frontier {
                if self.tree.properties(tile).has_content {
                    wanted.push(tile);
                }
                next.extend(self.tree.children(tile));
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        // The coarse pyramid is the floor the whole selection stands on, and
        // its membership depends on `tree.children()` — which is exactly the
        // thing that changes shape as availability arrives. A pyramid that
        // differs between two runs is a selection that will differ too.
        crate::det!(
            "prime",
            through_level = level,
            tiles = wanted.len(),
            digest = crate::determinism::digest(wanted.iter().map(|t| t.0)),
        );
        tracing::info!(
            through_level = level,
            tiles = wanted.len(),
            "priming the coarse pyramid onto the GPU"
        );
        m.priming_total.set(wanted.len() as u64);
        self.priming_state.total = wanted.len() as u32;
        self.priming_outstanding.extend(wanted.iter().copied());
        *self.priming = wanted;
    }

    /// Marks a primed tile as answered — delivered, or given up on.
    ///
    /// Called on **every** terminal outcome of a load, because the count a host
    /// waits on has to fall to zero however the tile ended. A tile the source
    /// does not serve is resolved, not pending: it will never be on the GPU, and
    /// a gate that keeps waiting for it is a gate that never opens.
    fn resolve_primed(&mut self, tile: TileId, arrived: bool) {
        if !self.priming_outstanding.remove(&tile) {
            return;
        }
        if !arrived {
            self.priming_state.unavailable += 1;
        }
    }

    /// Sends the coarse-pyramid count when it has moved.
    fn report_priming(&mut self, tx: &UnboundedSender<ServerMessage>) -> Result<(), Stop> {
        self.priming_state.outstanding = self.priming_outstanding.len() as u32;
        let m = crate::metrics::metrics();
        m.priming_pending
            .set(u64::from(self.priming_state.outstanding));
        m.priming_unavailable
            .set(u64::from(self.priming_state.unavailable));
        if *self.priming_reported == Some(*self.priming_state) {
            return Ok(());
        }
        *self.priming_reported = Some(*self.priming_state);
        tx.unbounded_send(ServerMessage::Priming(*self.priming_state))
            .map_err(|_| Stop::Gone)
    }

    fn on_load_done(
        &mut self,
        (tile, result): LoadMsg,
        tx: &UnboundedSender<ServerMessage>,
    ) -> Result<(), Stop> {
        self.in_flight.remove(&tile);
        let epoch = self.started.remove(&tile).unwrap_or(0);
        let refresh = self.refreshing.remove(&tile);
        let m = crate::metrics::metrics();
        m.loads_in_flight.set(self.in_flight.len() as u64);
        // A refresh whose tile was evicted while it was in flight has nothing
        // left to replace. Admitting it would bring back, unasked, a tile the
        // budget had just let go of.
        if refresh && !self.residency.is_resident(tile) {
            return Ok(());
        }
        match result {
            // A refresh that fails costs the new picture and nothing else:
            // the tile keeps the content it has, which is the whole reason it
            // was kept. Ending the session over it — the rule for a first
            // load, where the alternative is a hole — would trade ground that
            // is merely out of date for no ground at all. It is not retried:
            // recorded at the epoch it was attempted under, it waits for the
            // next change or for its own eviction.
            Err(e) if refresh => {
                m.loads_failed.inc();
                self.epochs.insert(tile, epoch);
                tracing::error!(?tile, "refresh failed, the tile keeps its content: {e}");
                tx.unbounded_send(ServerMessage::Error {
                    tile: Some(tile),
                    message: format!("refresh failed, the tile keeps its content: {e}"),
                })
                .map_err(|_| Stop::Gone)
            }
            Err(e) => {
                m.loads_failed.inc();
                crate::det!("load_err", tile = tile.0, level = self.tree.level(tile));
                self.fail(tile, e.to_string(), tx)
            }
            // Topology grew in place (external tileset grafted): the graft
            // cleared the host's content, so the next traversal won't
            // re-request it; the revealed children load on their own. Nothing
            // will ever be uploaded for this tile, so a pyramid waiting on it
            // is waiting on nothing.
            Ok(Loaded::Expanded) => {
                self.resolve_primed(tile, false);
                Ok(())
            }
            Ok(Loaded::Content(decoded)) => {
                let size = decoded.byte_size();
                // Imagery is charged separately because it is shared: what this
                // tile costs the budget is its geometry plus whatever share of
                // the draped textures no other resident tile is already paying.
                let imagery: Vec<_> = decoded
                    .imagery
                    .iter()
                    .map(|l| (l.coord, l.texture.resident_bytes()))
                    .collect();
                // The level comes from the tree, which is the only thing that
                // can read an opaque handle — see `TileTree::level`.
                let evicted =
                    self.cache
                        .insert(tile, self.tree.level(tile), size, &imagery, self.selected);
                m.loads_completed.inc();
                m.tiles_evicted.add(evicted.len() as u64);
                m.resident_bytes.set(self.cache.used_bytes() as u64);
                let (textures, bytes) = self.cache.imagery_bytes();
                m.imagery_textures.set(textures as u64);
                m.imagery_bytes.set(bytes as u64);
                for e in &evicted {
                    self.residency.remove(*e);
                    self.epochs.remove(e);
                    // The consumer is about to drop it too, so its ack stops
                    // being true. Left behind, it would suppress the stand-in
                    // the next time this ground is looked at. The same goes for
                    // `filled`: an `Evict` takes the stand-in with everything
                    // else, so tracking it past this point is fiction.
                    self.acked.remove(e);
                    self.filled.remove(e);
                }
                if !evicted.is_empty() {
                    crate::det!(
                        "evict",
                        count = evicted.len(),
                        digest = crate::determinism::digest(evicted.iter().map(|t| t.0)),
                    );
                    tx.unbounded_send(ServerMessage::Evict { tiles: evicted })
                        .map_err(|_| Stop::Gone)?;
                }
                self.residency.insert(tile);
                self.epochs.insert(tile, epoch);
                self.resolve_primed(tile, true);
                // Arrivals are genuinely unordered — the network decides. On
                // its own event name so a comparison can drop it and still
                // compare every decision that was taken.
                crate::det!(
                    "load_ok",
                    tile = tile.0,
                    level = self.tree.level(tile),
                    resident = self.residency.len(),
                );
                tx.unbounded_send(ServerMessage::Content {
                    tile,
                    ancestry: self.ancestry(tile),
                    content: TileContent::Decoded(decoded),
                })
                .map_err(|_| Stop::Gone)?;
                Ok(())
            }
        }
    }

    /// A tile that could not be loaded ends the session.
    ///
    /// There used to be a set of given-up-on tiles here instead, and it was the
    /// most expensive twelve lines in the repository. It was never emptied, so
    /// a tile lost to one dropped socket was lost for as long as the process
    /// lived; it was consulted by the traversal, so the shape of the ground
    /// silently depended on which requests had happened to fail; and it let the
    /// session carry on pretending, which is how a render shipped forty-eight
    /// black frames instead of stopping on the first one. Two nodes rendering
    /// the same frame would not even agree with each other — the reproducibility
    /// the whole design is for, traded away for a hidden mutable set.
    ///
    /// So the tile is not remembered: the transport has already retried (see
    /// `is_retryable_transport`), and a failure that survives that is not a
    /// hiccup. The consumer is told which tile and why, and then the stream
    /// closes under it, which is a thing a caller cannot ignore.
    fn fail(
        &mut self,
        tile: TileId,
        message: String,
        tx: &UnboundedSender<ServerMessage>,
    ) -> Result<(), Stop> {
        // Sent before unwinding: a bare closed channel says a tile is missing
        // without ever saying which, and that was a whole afternoon once.
        tx.unbounded_send(ServerMessage::Error {
            tile: Some(tile),
            message: message.clone(),
        })
        .map_err(|_| Stop::Gone)?;
        Err(Stop::LoadFailed { tile, message })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::FsFetcher;
    use crate::protocol::GeometryStream;
    use futures_util::task::LocalSpawnExt;
    use glam::{dvec2, dvec3};
    use std::sync::Mutex;
    use url::Url;

    thread_local! {
        /// Uniform-detail passes checked against the reference on this
        /// thread, and the descents the session's memo had run by the last.
        static CHECKED: std::cell::Cell<(u64, u64)> = const { std::cell::Cell::new((0, 0)) };
    }

    /// The pass the session just ran, run again from scratch with
    /// [`crate::traversal::traverse`], and the two compared. Called on every
    /// pass of every session the tests in this module drive.
    pub(super) fn matches_the_reference_pass(s: &Session<'_>) {
        let mut fresh = TraversalOutput::default();
        crate::traversal::traverse(
            s.tree,
            s.residency,
            s.views,
            s.config,
            *s.frame,
            s.rendered_last,
            s.chosen_before,
            &mut fresh,
        );
        let at = format!("pass {}", *s.frame);
        assert_eq!(s.out.selected, fresh.selected, "{at}: selection");
        assert_eq!(s.out.requests, fresh.requests, "{at}: requests");
        assert_eq!(s.out.awaiting, fresh.awaiting, "{at}: awaiting");
        assert_eq!(s.out.chosen, fresh.chosen, "{at}: chosen");
        assert_eq!(s.out.stats, fresh.stats, "{at}: stats");
        if s.config.uniform_detail {
            CHECKED.with(|c| c.set((c.get().0 + 1, s.near.descents())));
        }
    }

    /// **A session prices a still camera once, and selects as a fresh pass
    /// would on every pass.**
    ///
    /// Every arrival is answered by a pass, and under uniform detail every
    /// pass used to repeat the descent along the line of sight. Here the
    /// session runs to convergence twice — one camera, then another — with
    /// each of its passes checked against the reference by
    /// `matches_the_reference_pass`, and it may descend once per camera.
    #[test]
    fn a_still_camera_is_priced_once_per_camera() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = write_deep_fixture(dir.path());
        let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
        let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");
        let config = Config {
            uniform_detail: true,
            pinned_level: None,
            ..Config::default()
        };
        let (mut stream, server) = in_process(tileset, Arc::new(FsFetcher), config);
        let mut server = Box::pin(server.run());
        CHECKED.with(|c| c.set((0, 0)));

        stream
            .send(ClientMessage::ViewerState {
                views: vec![near_view()],
                generation: 0,
            })
            .expect("send");
        let (loaded, _) = settle(&mut server, &mut stream).expect("first view settles");
        let (first, descents) = CHECKED.with(|c| c.get());
        assert!(
            loaded >= 3,
            "only {loaded} tiles: nothing arrived to answer"
        );
        // Siblings answered from disk land together and share a pass, so
        // two is the least: the camera's, and the arrivals'.
        assert!(first >= 2, "{first} passes: the arrivals were not answered");
        assert_eq!(descents, 1, "{first} passes under one camera");

        let farther = ViewState::perspective(
            dvec3(0.0, 0.0, 400.0),
            dvec3(0.0, 0.0, -1.0),
            dvec3(0.0, 1.0, 0.0),
            dvec2(1024.0, 768.0),
            std::f64::consts::FRAC_PI_3,
        );
        stream
            .send(ClientMessage::ViewerState {
                views: vec![farther],
                generation: 1,
            })
            .expect("send");
        settle(&mut server, &mut stream).expect("second view settles");
        let (total, descents) = CHECKED.with(|c| c.get());
        assert!(total > first, "the second camera was never traversed");
        assert_eq!(descents, 2, "{total} passes under two cameras");
    }

    /// **A load that is woken carries the wake, not the poll.** The loop polls
    /// the loads only when it gets round to them, so a load whose data arrived
    /// during a traversal resolves after it — and the arrival wait is the time
    /// in between, which a stamp taken on resolving would always read as zero.
    #[test]
    fn a_woken_load_carries_its_wake_not_its_poll() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        /// Pending until told otherwise, with its waker kept for the test.
        struct Later {
            ready: Arc<AtomicBool>,
            waker: Arc<Mutex<Option<std::task::Waker>>>,
        }
        impl Future for Later {
            type Output = u8;
            fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<u8> {
                if self.ready.load(Ordering::SeqCst) {
                    return Poll::Ready(7);
                }
                *self.waker.lock().expect("lock") = Some(cx.waker().clone());
                Poll::Pending
            }
        }
        /// The loop's side: counts the wakes passed through to it.
        struct Count(AtomicUsize);
        impl std::task::Wake for Count {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let waits: &'static crate::metrics::Timer = Box::leak(Box::default());
        let ready = Arc::new(AtomicBool::new(false));
        let slot = Arc::new(Mutex::new(None));
        let mut load = Box::pin(Woken::new(
            Later {
                ready: Arc::clone(&ready),
                waker: Arc::clone(&slot),
            },
            waits,
        ));
        let count = Arc::new(Count(AtomicUsize::new(0)));
        let waker = std::task::Waker::from(Arc::clone(&count));
        let mut cx = std::task::Context::from_waker(&waker);

        assert!(load.as_mut().poll(&mut cx).is_pending());
        assert_eq!(waits.count(), 0, "nothing woke it yet");

        ready.store(true, Ordering::SeqCst);
        slot.lock().expect("lock").take().expect("a waker").wake();
        assert_eq!(
            count.0.load(Ordering::SeqCst),
            1,
            "the wake reaches the loop"
        );
        // The loop is busy elsewhere.
        std::thread::sleep(std::time::Duration::from_millis(30));

        let Poll::Ready((out, woke)) = load.as_mut().poll(&mut cx) else {
            unreachable!("ready once woken");
        };
        assert_eq!(out, 7);
        assert!(
            woke.elapsed() >= std::time::Duration::from_millis(30),
            "the stamp is the wake's, 30 ms ago, not the poll's: {:?}",
            woke.elapsed()
        );
        assert_eq!(waits.count(), 1, "one wake, one wait");
        assert!(waits.peak_millis() >= 30.0, "{}", waits.peak_millis());
    }

    fn write_fixture(dir: &std::path::Path) -> Url {
        let glb = crate::content::tests::test_glb();
        std::fs::write(dir.join("root.glb"), &glb).expect("write root");
        std::fs::write(dir.join("a.glb"), &glb).expect("write a");
        std::fs::write(dir.join("b.glb"), &glb).expect("write b");
        let tileset = r#"{
          "asset": { "version": "1.1" },
          "geometricError": 200,
          "root": {
            "boundingVolume": { "sphere": [0, 0, 0, 100] },
            "geometricError": 50,
            "refine": "REPLACE",
            "content": { "uri": "root.glb" },
            "children": [
              { "boundingVolume": { "sphere": [-50, 0, 0, 50] },
                "geometricError": 0, "content": { "uri": "a.glb" } },
              { "boundingVolume": { "sphere": [50, 0, 0, 50] },
                "geometricError": 0, "content": { "uri": "b.glb" } }
            ]
          }
        }"#;
        let path = dir.join("tileset.json");
        std::fs::write(&path, tileset).expect("write tileset");
        Url::from_file_path(&path).expect("url")
    }

    /// The same fixture with one child's content missing from disk — a tile the
    /// source will never serve, which over a globe is most of an ocean.
    fn write_fixture_missing_one_child(dir: &std::path::Path) -> Url {
        let url = write_fixture(dir);
        std::fs::remove_file(dir.join("b.glb")).expect("remove b");
        url
    }

    /// A tile the source cannot serve ends the session, loudly.
    ///
    /// This asserted the opposite until 2026-09-08, and the opposite was a
    /// disaster. The session used to write the tile down in a `failed` set,
    /// stop asking for it, and carry on: the pyramid settled, the gate opened,
    /// and the ground simply had a hole in it that nothing downstream could
    /// see. On the farm that hole reached the procedural as "the frame did not
    /// converge", which emits nothing at all — forty-eight black frames, shipped
    /// as a video, because one endpoint dropped three connections.
    ///
    /// The transport retries a transient failure before we ever hear about it
    /// (`is_retryable_transport`), so a failure that reaches here is real. The
    /// contract is now: say which tile, say why, and stop. A caller that is
    /// waiting on a stream gets a closed stream, which it cannot mistake for
    /// success.
    #[test]
    fn a_tile_the_source_cannot_serve_ends_the_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = write_fixture_missing_one_child(dir.path());
        let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
        let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");

        // Level 0 and 1: the root and its two children, one of which has no
        // content on disk.
        let config = Config {
            pinned_level: Some(1),
            ..Config::default()
        };
        let (mut stream, server) = in_process(tileset, Arc::new(FsFetcher), config);
        let mut server = Box::pin(server.run());

        stream
            .send(ClientMessage::ViewerState {
                views: vec![near_view()],
                generation: 0,
            })
            .expect("send");

        const MAX_STEPS: usize = 2_000;
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let mut reported: Option<(Option<TileId>, String)> = None;
        let mut stopped = false;
        for _ in 0..MAX_STEPS {
            if !stopped && server.as_mut().poll(&mut cx).is_ready() {
                stopped = true;
            }
            while let Poll::Ready(Some(msg)) = stream.poll_message(&mut cx) {
                if let ServerMessage::Error { tile, message } = msg {
                    reported.get_or_insert((tile, message));
                }
            }
            if stopped {
                break;
            }
        }

        assert!(
            stopped,
            "the session kept running after a tile it can never serve"
        );
        let (tile, message) =
            reported.expect("the session stopped without ever saying which tile it could not load");
        assert!(tile.is_some(), "an unattributed failure is not actionable");
        assert!(
            !message.is_empty(),
            "the reason has to travel with the failure"
        );
        // And the stream is closed under the consumer, so nothing downstream
        // can read the truncated selection as a finished one.
        assert!(
            matches!(stream.poll_message(&mut cx), Poll::Ready(None)),
            "the stream stayed open after the session ended"
        );
    }

    /// A three-level REPLACE tileset: root → two mids → four leaves. Deep
    /// enough that selecting the leaves leaves an ancestor (the root) outside
    /// the pinned band.
    fn write_deep_fixture(dir: &std::path::Path) -> Url {
        let glb = crate::content::tests::test_glb();
        for name in ["root", "m0", "m1", "l0", "l1", "l2", "l3"] {
            std::fs::write(dir.join(format!("{name}.glb")), &glb).expect("write glb");
        }
        let leaf = |x: f64, uri: &str| {
            format!(
                r#"{{ "boundingVolume": {{ "sphere": [{x}, 0, 0, 25] }},
                     "geometricError": 0, "content": {{ "uri": "{uri}" }} }}"#
            )
        };
        let tileset = format!(
            r#"{{
              "asset": {{ "version": "1.1" }},
              "geometricError": 400,
              "root": {{
                "boundingVolume": {{ "sphere": [0, 0, 0, 100] }},
                "geometricError": 100,
                "refine": "REPLACE",
                "content": {{ "uri": "root.glb" }},
                "children": [
                  {{ "boundingVolume": {{ "sphere": [-50, 0, 0, 50] }},
                     "geometricError": 50, "refine": "REPLACE",
                     "content": {{ "uri": "m0.glb" }},
                     "children": [{}, {}] }},
                  {{ "boundingVolume": {{ "sphere": [50, 0, 0, 50] }},
                     "geometricError": 50, "refine": "REPLACE",
                     "content": {{ "uri": "m1.glb" }},
                     "children": [{}, {}] }}
                ]
              }}
            }}"#,
            leaf(-75.0, "l0.glb"),
            leaf(-25.0, "l1.glb"),
            leaf(25.0, "l2.glb"),
            leaf(75.0, "l3.glb"),
        );
        let path = dir.join("tileset.json");
        std::fs::write(&path, &tileset).expect("write tileset");
        Url::from_file_path(&path).expect("url")
    }

    /// Steps a session by hand until it goes quiet, returning
    /// `(loads, evictions)` — or `None` if it never settled.
    ///
    /// Hand-stepping rather than running to quiescence because a session can
    /// fail to converge (evicting a tile the next traversal re-requests), and a
    /// test must report that as a failure rather than hang. `FsFetcher`
    /// resolves inline, so every poll makes progress: the bound is a step
    /// count, not a timeout.
    fn settle(
        server: &mut Pin<Box<impl Future<Output = ()>>>,
        stream: &mut InProcessStream,
    ) -> Option<(usize, Vec<TileId>)> {
        const MAX_STEPS: usize = 2_000;
        const QUIET_STEPS: usize = 8;
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let (mut loaded, mut quiet) = (0usize, 0usize);
        let mut evicted: Vec<TileId> = Vec::new();
        for _ in 0..MAX_STEPS {
            let _ = server.as_mut().poll(&mut cx);
            let mut got = false;
            while let Poll::Ready(Some(msg)) = stream.poll_message(&mut cx) {
                got = true;
                match msg {
                    ServerMessage::Content { .. } => loaded += 1,
                    ServerMessage::Evict { tiles } => evicted.extend(tiles),
                    _ => {}
                }
            }
            quiet = if got { 0 } else { quiet + 1 };
            if quiet > QUIET_STEPS {
                return Some((loaded, evicted));
            }
        }
        None
    }

    /// **A stand-in that has been replaced is not evicted.**
    ///
    /// `Fill` and `Content` are filed under the same id by the consumer, so the
    /// real thing simply replaces the stand-in when it lands. The server has to
    /// notice: it keeps a `filled` set so it can take a stand-in back when the
    /// camera leaves that ground, and if the tile is still in that set once its
    /// real content has been sent, the take-back names real geometry.
    ///
    /// What that costs, measured in the viewer before this test existed: the
    /// consumer drops the tile, the server still holds it resident and still has
    /// its ack, so it sends neither `Content` (resident) nor `Fill` (acked)
    /// when the ground is looked at again. The walk climbs to an ancestor and
    /// stays there — **28 tiles drawn coarse with nothing loading and no error
    /// reported**, stable, for as long as the session ran. On screen that is
    /// blurry rectangles with straight tile-aligned edges, which is a different
    /// artefact from the ragged organic outlines of two surfaces fighting, and
    /// was mistaken for it.
    #[test]
    fn a_stand_in_that_has_been_replaced_is_not_evicted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = write_deep_fixture(dir.path());
        let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
        let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");

        /// Real content for everything, and a stand-in on demand.
        struct Standing(Arc<dyn TileLoader>);

        #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
        #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
        impl TileLoader for Standing {
            async fn load(&self, id: TileId) -> Result<Loaded, LoadError> {
                self.0.load(id).await
            }
            fn fill(&self, _id: TileId) -> Option<crate::content::DecodedTileContent> {
                Some(crate::content::DecodedTileContent {
                    withheld_drape: None,
                    meshes: Vec::new(),
                    textures: Vec::new(),
                    imagery: Vec::new(),
                    local_origin_ecef: glam::DVec3::ZERO,
                    transform_local: glam::Mat4::IDENTITY,
                })
            }
        }

        let arena = Arc::new(RwLock::new(tileset));
        let tree: Box<dyn TileTree> = Box::new(TilesetTree::new(Arc::clone(&arena)));
        let inner: Arc<dyn TileLoader> = Arc::new(TilesetLoader::new(arena, Arc::new(FsFetcher)));
        let (mut stream, server) = in_process_with(
            tree,
            Arc::new(Standing(inner)) as Arc<dyn TileLoader>,
            Config {
                pinned_level: None,
                ..Config::default()
            },
        );
        let mut server = Box::pin(server.run());

        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let mut delivered: HashSet<TileId> = HashSet::new();
        let mut wrongly_taken: Vec<TileId> = Vec::new();

        // Look somewhere, then look elsewhere: the second view is what makes the
        // first view's ground leave the selection, which is when a stand-in is
        // taken back.
        for view in [near_view(), far_view()] {
            stream
                .send(ClientMessage::ViewerState {
                    views: vec![view],
                    generation: 0,
                })
                .expect("send");
            for _ in 0..256 {
                let _ = server.as_mut().poll(&mut cx);
                while let Poll::Ready(Some(msg)) = stream.poll_message(&mut cx) {
                    match msg {
                        ServerMessage::Content { tile, .. } => {
                            delivered.insert(tile);
                            // A real consumer acks what it uploads; a stand-in
                            // is never acked, which is what the server relies on.
                            let _ = stream.send(ClientMessage::Ack { tile });
                        }
                        ServerMessage::Evict { tiles } => {
                            for t in tiles {
                                if delivered.contains(&t) {
                                    wrongly_taken.push(t);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        assert!(
            wrongly_taken.is_empty(),
            "the server took back {} tiles whose real content it had already \
             sent: {wrongly_taken:?}. The consumer drops them, the server still \
             holds them resident and acked, and that ground is coarse for the \
             rest of the session",
            wrongly_taken.len()
        );
    }

    /// **A server answered from memory still hands its host a turn.** Every
    /// load here completes on its first poll, so the loop never has to wait;
    /// with a breather every two events it pauses all the same, and the
    /// session goes on to deliver everything after each pause.
    ///
    /// Every two, not four: arrivals that are ready together are one event
    /// (`drain_ready`), so this seven-tile session is a handful of events
    /// in a row — the camera, the root, its children, their children — and
    /// four of them back to back no longer happens without a wait between.
    #[test]
    fn a_busy_server_hands_its_host_a_turn() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// Ready on the second poll: a turn given back to the executor.
        struct Yield(bool);
        impl Future for Yield {
            type Output = ();
            fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<()> {
                if std::mem::replace(&mut self.0, true) {
                    Poll::Ready(())
                } else {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }
        }

        let run = |every: usize| -> (usize, usize) {
            let dir = tempfile::tempdir().expect("tempdir");
            let url = write_deep_fixture(dir.path());
            let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
            let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");
            let pauses = Arc::new(AtomicUsize::new(0));
            let counted = Arc::clone(&pauses);
            let (mut stream, server) = in_process(
                tileset,
                Arc::new(FsFetcher),
                Config {
                    pinned_level: None,
                    ..Config::default()
                },
            );
            let server = server.with_breather(
                every,
                Arc::new(move || {
                    counted.fetch_add(1, Ordering::SeqCst);
                    Box::pin(Yield(false))
                }),
            );
            let mut server = Box::pin(server.run());
            let waker = futures_util::task::noop_waker();
            let mut cx = std::task::Context::from_waker(&waker);
            stream
                .send(ClientMessage::ViewerState {
                    views: vec![near_view()],
                    generation: 0,
                })
                .expect("send");
            let mut delivered = 0usize;
            for _ in 0..256 {
                let _ = server.as_mut().poll(&mut cx);
                while let Poll::Ready(Some(msg)) = stream.poll_message(&mut cx) {
                    if let ServerMessage::Content { tile, .. } = msg {
                        delivered += 1;
                        let _ = stream.send(ClientMessage::Ack { tile });
                    }
                }
            }
            (pauses.load(Ordering::SeqCst), delivered)
        };
        let (pauses, delivered) = run(2);
        assert!(pauses > 0, "the host never had a turn");
        assert!(
            delivered >= 7,
            "the session stopped after a pause: {delivered}"
        );
        // A breather that never comes due takes no pause: the count is of
        // events back to back, and this session has fewer than a thousand.
        assert_eq!(run(1000).0, 0, "paused before its count was reached");
    }

    /// A load that waits behind a gate the test opens, for the tiles it holds.
    struct Gate {
        open: std::sync::atomic::AtomicBool,
        wakers: Mutex<Vec<std::task::Waker>>,
        waiting: std::sync::atomic::AtomicUsize,
    }

    impl Gate {
        fn closed() -> Arc<Self> {
            Arc::new(Self {
                open: std::sync::atomic::AtomicBool::new(false),
                wakers: Mutex::new(Vec::new()),
                waiting: std::sync::atomic::AtomicUsize::new(0),
            })
        }

        fn open(&self) {
            self.open.store(true, std::sync::atomic::Ordering::SeqCst);
            for waker in self.wakers.lock().expect("wakers").drain(..) {
                waker.wake();
            }
        }

        async fn pass(&self) {
            use std::sync::atomic::Ordering;
            self.waiting.fetch_add(1, Ordering::SeqCst);
            poll_fn(|cx| {
                if self.open.load(Ordering::SeqCst) {
                    Poll::Ready(())
                } else {
                    self.wakers.lock().expect("wakers").push(cx.waker().clone());
                    Poll::Pending
                }
            })
            .await;
            self.waiting.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// **Loads that land together are answered by one traversal.**
    ///
    /// Four leaves are asked for at once and held behind a gate; when it opens
    /// all four are ready on the same turn. One pass per arrival — what the
    /// loop did before it drained them — walked the tree four times, and three
    /// of those selections were superseded before a consumer, which drains
    /// every queued message before it draws, could ever see them. In
    /// production that was ~110 000 traversals per pod in ten minutes.
    ///
    /// What this does not cover: whether fewer intermediate passes change the
    /// final selection of a real flight (the hysteresis reads the previous
    /// pass). That is the bake goldens' job.
    #[test]
    fn loads_that_land_together_are_answered_by_one_traversal() {
        use std::sync::atomic::Ordering;

        let dir = tempfile::tempdir().expect("tempdir");
        let url = write_deep_fixture(dir.path());
        let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
        let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");
        let root = tileset.root();
        let leaves: HashSet<TileId> = tileset
            .tile(root)
            .children
            .iter()
            .flat_map(|m| tileset.tile(*m).children.clone())
            .collect();
        assert_eq!(leaves.len(), 4);

        /// Real content, the leaves only once the gate is open.
        struct Held(Arc<dyn TileLoader>, HashSet<TileId>, Arc<Gate>);

        #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
        #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
        impl TileLoader for Held {
            async fn load(&self, id: TileId) -> Result<Loaded, LoadError> {
                if self.1.contains(&id) {
                    self.2.pass().await;
                }
                self.0.load(id).await
            }
        }

        let gate = Gate::closed();
        let arena = Arc::new(RwLock::new(tileset));
        let tree: Box<dyn TileTree> = Box::new(TilesetTree::new(Arc::clone(&arena)));
        let inner: Arc<dyn TileLoader> = Arc::new(TilesetLoader::new(arena, Arc::new(FsFetcher)));
        let (mut stream, server) = in_process_with(
            tree,
            Arc::new(Held(inner, leaves.clone(), Arc::clone(&gate))) as Arc<dyn TileLoader>,
            Config {
                pinned_level: None,
                ..Config::default()
            },
        );
        let mut server = Box::pin(server.run());
        stream
            .send(ClientMessage::ViewerState {
                views: vec![near_view()],
                generation: 0,
            })
            .expect("send");

        // Steps the session until it goes quiet, counting what it sent.
        fn step(
            server: &mut Pin<Box<impl Future<Output = ()>>>,
            stream: &mut InProcessStream,
        ) -> (usize, HashSet<TileId>) {
            let waker = futures_util::task::noop_waker();
            let mut cx = std::task::Context::from_waker(&waker);
            let (mut selections, mut arrived) = (0usize, HashSet::new());
            for _ in 0..32 {
                assert!(server.as_mut().poll(&mut cx).is_pending(), "session ended");
                while let Poll::Ready(Some(msg)) = stream.poll_message(&mut cx) {
                    match msg {
                        ServerMessage::Select { .. } => selections += 1,
                        ServerMessage::Content { tile, .. } => {
                            arrived.insert(tile);
                        }
                        _ => {}
                    }
                }
            }
            (selections, arrived)
        }

        let (_, before) = step(&mut server, &mut stream);
        assert_eq!(
            gate.waiting.load(Ordering::SeqCst),
            4,
            "every leaf is asked for before the gate opens (arrived: {before:?})"
        );
        assert!(before.is_disjoint(&leaves));

        gate.open();
        let (selections, arrived) = step(&mut server, &mut stream);
        assert_eq!(arrived, leaves, "all four leaves landed");
        assert_eq!(
            selections, 1,
            "four arrivals ready on the same turn are answered by one traversal"
        );
    }

    /// Records the tiles whose load was dropped before it finished — what a
    /// cancel does to a load.
    struct Unfinished {
        tile: TileId,
        dropped: Arc<Mutex<Vec<TileId>>>,
        finished: bool,
    }

    impl Drop for Unfinished {
        fn drop(&mut self) {
            if !self.finished {
                self.dropped.lock().expect("dropped").push(self.tile);
            }
        }
    }

    /// Real content; the tiles in `held` only once the gate opens, and a
    /// note of every load dropped unfinished.
    struct Holding {
        inner: Arc<dyn TileLoader>,
        held: HashSet<TileId>,
        gate: Arc<Gate>,
        dropped: Arc<Mutex<Vec<TileId>>>,
    }

    #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
    #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
    impl TileLoader for Holding {
        async fn load(&self, id: TileId) -> Result<Loaded, LoadError> {
            let mut guard = Unfinished {
                tile: id,
                dropped: Arc::clone(&self.dropped),
                finished: false,
            };
            if self.held.contains(&id) {
                self.gate.pass().await;
            }
            let loaded = self.inner.load(id).await;
            guard.finished = true;
            loaded
        }
    }

    /// A session over `tileset` whose loader holds `held` behind a gate.
    #[allow(clippy::type_complexity)]
    fn holding_session(
        tileset: Tileset,
        held: HashSet<TileId>,
        config: Config,
    ) -> (
        InProcessStream,
        Pin<Box<impl Future<Output = ()>>>,
        Arc<Gate>,
        Arc<Mutex<Vec<TileId>>>,
    ) {
        let gate = Gate::closed();
        let dropped = Arc::new(Mutex::new(Vec::new()));
        let arena = Arc::new(RwLock::new(tileset));
        let tree: Box<dyn TileTree> = Box::new(TilesetTree::new(Arc::clone(&arena)));
        let inner: Arc<dyn TileLoader> = Arc::new(TilesetLoader::new(arena, Arc::new(FsFetcher)));
        let loader = Holding {
            inner,
            held,
            gate: Arc::clone(&gate),
            dropped: Arc::clone(&dropped),
        };
        let (stream, server) = in_process_with(tree, Arc::new(loader), config);
        (stream, Box::pin(server.run()), gate, dropped)
    }

    /// What one stretch of polling produced, in order.
    #[derive(Debug, Default)]
    struct Heard {
        /// The generation of every `Select`, in order.
        selections: Vec<u64>,
        /// Content delivered, and how many selections had gone out before it.
        contents: Vec<(TileId, usize)>,
    }

    fn listen(
        server: &mut Pin<Box<impl Future<Output = ()>>>,
        stream: &mut InProcessStream,
        polls: usize,
    ) -> Heard {
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let mut heard = Heard::default();
        for _ in 0..polls {
            assert!(server.as_mut().poll(&mut cx).is_pending(), "session ended");
            while let Poll::Ready(Some(msg)) = stream.poll_message(&mut cx) {
                match msg {
                    ServerMessage::Select { generation, .. } => heard.selections.push(generation),
                    ServerMessage::Content { tile, .. } => {
                        heard.contents.push((tile, heard.selections.len()));
                    }
                    ServerMessage::Error { message, .. } => unreachable!("error: {message}"),
                    _ => {}
                }
            }
        }
        heard
    }

    fn look(stream: &InProcessStream, view: ViewState, generation: u64) {
        stream
            .send(ClientMessage::ViewerState {
                views: vec![view],
                generation,
            })
            .expect("send");
    }

    /// The two-leaf fixture, its root and its leaves.
    fn two_leaves(dir: &std::path::Path) -> (Tileset, TileId, HashSet<TileId>) {
        let url = write_fixture(dir);
        let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
        let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");
        let root = tileset.root();
        let leaves = tileset.tile(root).children.iter().copied().collect();
        (tileset, root, leaves)
    }

    fn unpinned() -> Config {
        Config {
            pinned_level: None,
            ..Config::default()
        }
    }

    /// **The camera and the loads take turns.** A camera and two arrivals
    /// are ready on the same turn; which is answered first follows the turn
    /// order, which alternates. The same session started twice, the second
    /// with one extra event before (an ack) — the order is flipped, so
    /// across the two the arrivals are answered first once and the camera
    /// once. A loop that always read the camera first never answers the
    /// arrivals first; one that always read the loads never the camera.
    #[test]
    fn the_camera_and_the_arrivals_take_turns() {
        let first_answered = |extra_event: bool| -> u64 {
            let dir = tempfile::tempdir().expect("tempdir");
            let (tileset, root, leaves) = two_leaves(dir.path());
            let (mut stream, mut server, gate, _) = holding_session(tileset, leaves, unpinned());
            look(&stream, near_view(), 1);
            let before = listen(&mut server, &mut stream, 16);
            assert!(before.contents.iter().any(|(t, _)| *t == root));
            assert_eq!(gate.waiting.load(std::sync::atomic::Ordering::SeqCst), 2);
            if extra_event {
                stream.send(ClientMessage::Ack { tile: root }).expect("ack");
                listen(&mut server, &mut stream, 4);
            }
            // Both sides ready on the same turn.
            gate.open();
            look(&stream, near_view(), 2);
            let after = listen(&mut server, &mut stream, 16);
            assert_eq!(after.contents.len(), 2, "both leaves landed: {after:?}");
            // Answered by a pass still on generation 1: the arrivals went
            // first. On generation 2: the camera did.
            after.selections[0]
        };
        let firsts: HashSet<u64> = [first_answered(false), first_answered(true)].into();
        assert_eq!(
            firsts,
            HashSet::from([1, 2]),
            "the camera and the arrivals each went first once"
        );
    }

    /// **A camera state and an ack in one burst still move the picture.** The
    /// burst is coalesced; the ack asks for nothing, the camera for a pass,
    /// and the pass is owed if any message in it asked.
    #[test]
    fn a_camera_coalesced_with_an_ack_is_still_answered() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (tileset, root, leaves) = two_leaves(dir.path());
        let (mut stream, mut server, _gate, _) = holding_session(tileset, leaves, unpinned());
        look(&stream, near_view(), 1);
        listen(&mut server, &mut stream, 16);
        look(&stream, far_view(), 2);
        stream.send(ClientMessage::Ack { tile: root }).expect("ack");
        let heard = listen(&mut server, &mut stream, 16);
        assert_eq!(heard.selections, vec![2], "one pass, for the new camera");
    }

    /// **A camera move cancels the loads it no longer wants, and only
    /// those.** Both leaves are in flight behind a gate. The same view again
    /// still wants them: nothing is dropped. A view far enough that the root
    /// alone suffices wants neither: both are.
    #[test]
    fn a_camera_move_cancels_only_what_it_stopped_wanting() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (tileset, _root, leaves) = two_leaves(dir.path());
        let (mut stream, mut server, gate, dropped) =
            holding_session(tileset, leaves.clone(), unpinned());
        look(&stream, near_view(), 1);
        listen(&mut server, &mut stream, 16);
        assert_eq!(gate.waiting.load(std::sync::atomic::Ordering::SeqCst), 2);

        look(&stream, near_view(), 2);
        listen(&mut server, &mut stream, 16);
        assert!(
            dropped.lock().expect("dropped").is_empty(),
            "a camera that still wants the leaves kept them"
        );

        look(&stream, far_view(), 3);
        listen(&mut server, &mut stream, 16);
        let gone: HashSet<TileId> = dropped.lock().expect("dropped").iter().copied().collect();
        assert_eq!(gone, leaves, "a camera that wants neither dropped both");
    }

    /// **No more loads in flight than the cap, and up to it.** Every tile
    /// is held, so nothing in flight ever finishes. With room for two, two
    /// wait — not none, not more; with room for plenty, more than two do,
    /// so it was the cap that stopped them. With the coarse pyramid and
    /// without: its loads and the view's are capped alike.
    #[test]
    fn loads_in_flight_fill_the_cap_and_stop_there() {
        let waiting = |pinned_level: Option<u32>, cap: usize, view: ViewState| {
            let dir = tempfile::tempdir().expect("tempdir");
            let url = write_deep_fixture(dir.path());
            let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
            let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");
            let every: HashSet<TileId> = tileset.tiles().map(|(id, _)| id).collect();
            let (mut stream, mut server, gate, _) = holding_session(
                tileset,
                every,
                Config {
                    pinned_level,
                    maximum_simultaneous_fetches: cap,
                    ..Config::default()
                },
            );
            look(&stream, view, 1);
            listen(&mut server, &mut stream, 16);
            gate.waiting.load(std::sync::atomic::Ordering::SeqCst)
        };
        for pinned_level in [Some(1), None] {
            assert_eq!(
                waiting(pinned_level, 2, near_view()),
                2,
                "pinned {pinned_level:?}"
            );
            assert!(
                waiting(pinned_level, 64, near_view()) > 2,
                "pinned {pinned_level:?}"
            );
        }
        // From far off the view wants the root alone, so what fills the cap
        // is the pyramid's own loads: the root and its two children.
        assert_eq!(waiting(None, 64, far_view()), 1, "the view alone");
        assert_eq!(waiting(Some(1), 2, far_view()), 2, "the pyramid, capped");
        assert_eq!(waiting(Some(1), 64, far_view()), 3, "the whole pyramid");
    }

    /// A tileset whose root is an external tileset: the load grafts, and the
    /// grafted root is the content.
    #[cfg(all(feature = "rt-tokio", not(target_arch = "wasm32")))]
    fn write_external_fixture(dir: &std::path::Path) -> Url {
        let glb = crate::content::tests::test_glb();
        std::fs::write(dir.join("a.glb"), &glb).expect("write a");
        std::fs::write(
            dir.join("sub.json"),
            r#"{ "asset": { "version": "1.1" }, "geometricError": 50,
                 "root": { "boundingVolume": { "sphere": [0, 0, 0, 100] },
                           "geometricError": 0, "content": { "uri": "a.glb" } } }"#,
        )
        .expect("write sub");
        let path = dir.join("tileset.json");
        std::fs::write(
            &path,
            r#"{ "asset": { "version": "1.1" }, "geometricError": 200,
                 "root": { "boundingVolume": { "sphere": [0, 0, 0, 100] },
                           "geometricError": 50, "refine": "REPLACE",
                           "content": { "uri": "sub.json" } } }"#,
        )
        .expect("write tileset");
        Url::from_file_path(&path).expect("url")
    }

    /// **Loads as tasks: a graft waits for the top of a pass.** The server
    /// asks its tree to hold edits once, before anything is spawned, and
    /// applies them at the top of every pass; the external tileset grafted by
    /// a load running on another worker is then walked by the next pass whole,
    /// and its content arrives, on a multi-thread runtime, with the server
    /// itself a task on it.
    #[cfg(all(feature = "rt-tokio", not(target_arch = "wasm32")))]
    #[test]
    fn loads_as_tasks_graft_between_passes_and_converge() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// Counts what the server asks of the tree's edits.
        struct Recording {
            inner: TilesetTree,
            deferred: Arc<AtomicUsize>,
            applied: Arc<AtomicUsize>,
        }
        impl TileTree for Recording {
            fn roots(&self) -> Vec<TileId> {
                self.inner.roots()
            }
            fn children(&self, id: TileId) -> Vec<TileId> {
                self.inner.children(id)
            }
            fn properties(&self, id: TileId) -> crate::source::TileProperties {
                self.inner.properties(id)
            }
            fn parent(&self, id: TileId) -> Option<TileId> {
                self.inner.parent(id)
            }
            fn level(&self, id: TileId) -> u32 {
                self.inner.level(id)
            }
            fn defer_edits(&self) {
                self.deferred.fetch_add(1, Ordering::SeqCst);
                self.inner.defer_edits();
            }
            fn apply_deferred_edits(&self) -> Vec<(TileId, String)> {
                self.applied.fetch_add(1, Ordering::SeqCst);
                self.inner.apply_deferred_edits()
            }
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let url = write_external_fixture(dir.path());
        let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
        let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");
        let root = tileset.root();
        let arena = Arc::new(RwLock::new(tileset));
        let (deferred, applied) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let tree: Box<dyn TileTree> = Box::new(Recording {
            inner: TilesetTree::new(Arc::clone(&arena)),
            deferred: Arc::clone(&deferred),
            applied: Arc::clone(&applied),
        });
        let loader: Arc<dyn TileLoader> =
            Arc::new(TilesetLoader::new(Arc::clone(&arena), Arc::new(FsFetcher)));

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .build()
            .expect("runtime");
        let (mut stream, server) = in_process_with(
            tree,
            loader,
            Config {
                pinned_level: None,
                ..Config::default()
            },
        );
        let server = server.with_spawn(Spawn::Tokio(runtime.handle().clone()));
        let running = runtime.spawn(server.run());

        // A session that never walks the graft goes quiet rather than failing:
        // a watchdog turns that into a failure instead of a hang.
        let watchdog =
            runtime.spawn_blocking(|| std::thread::sleep(std::time::Duration::from_secs(20)));
        let (grafted, passes) = runtime.block_on(async {
            let session = async {
                stream
                    .send(ClientMessage::ViewerState {
                        views: vec![near_view()],
                        generation: 0,
                    })
                    .expect("send");
                let mut passes = 0usize;
                let mut grafted = None;
                while let Some(msg) = stream.next_message().await {
                    match msg {
                        ServerMessage::Content { tile, .. } => grafted = Some(tile),
                        ServerMessage::Select { tiles, .. } => {
                            passes += 1;
                            if grafted.is_some_and(|g| tiles.iter().any(|(t, _)| *t == g)) {
                                break;
                            }
                        }
                        ServerMessage::Error { message, .. } => {
                            unreachable!("unexpected error: {message}")
                        }
                        _ => {}
                    }
                }
                (grafted, passes)
            };
            tokio::select! {
                outcome = session => outcome,
                _ = watchdog => panic!("the grafted tileset was never walked"),
            }
        });
        drop(stream);
        let grafted = grafted.expect("the grafted root's content arrived");
        assert_eq!(
            arena.read().expect("arena").parent(grafted),
            Some(root),
            "the content is the grafted root's"
        );
        assert_eq!(
            deferred.load(Ordering::SeqCst),
            1,
            "edits held once, up front"
        );
        assert!(
            applied.load(Ordering::SeqCst) >= passes,
            "held edits applied at the top of every pass: {} for {passes}",
            applied.load(Ordering::SeqCst)
        );
        runtime
            .block_on(running)
            .expect("the server ends when the client goes");
        // Not waiting on the watchdog's sleep.
        runtime.shutdown_background();
    }

    /// **A result that lands after its load was cancelled is the news of the
    /// cancel.** A task cannot take back what it has sent; the abort handle
    /// that travels with the result says whether the loop still wants it.
    #[cfg(all(feature = "rt-tokio", not(target_arch = "wasm32")))]
    #[test]
    fn a_task_cancelled_after_it_finished_reports_a_cancel() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .build()
            .expect("runtime");
        let mut loads = Loads::new(&Spawn::Tokio(runtime.handle().clone()));
        let mut start = |tile: TileId| {
            let (handle, registration) = AbortHandle::new_pair();
            let fut: BoxLoadFut =
                Box::pin(async move { ((tile, Ok(Loaded::Expanded)), crate::metrics::stamp()) });
            loads.push(Abortable::new(fut, registration), &handle);
            handle
        };
        let kept = start(TileId(1));
        let cancelled = start(TileId(2));
        // Both have finished and sent: wait until both are in the channel.
        let mut landed = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let Loads::Tasks(tasks) = &mut loads else {
            unreachable!("tasks")
        };
        while landed.len() < 2 {
            assert!(std::time::Instant::now() < deadline, "the tasks never sent");
            if let Ok(item) = tasks.rx.try_recv() {
                landed.push(item);
            }
            std::thread::yield_now();
        }
        cancelled.abort();
        let mut outcomes: Vec<(u64, bool)> = landed
            .into_iter()
            .map(|(msg, handle)| (msg.0 .0 .0, tasks::Tasks::screen((msg, handle)).is_ok()))
            .collect();
        outcomes.sort_unstable();
        assert_eq!(outcomes, vec![(1, true), (2, false)]);
        drop(kept);
    }

    /// The mode names itself, for the log line a host writes.
    #[test]
    fn a_spawn_says_which_it_is() {
        assert_eq!(format!("{:?}", Spawn::default()), "Inline");
        #[cfg(all(feature = "rt-tokio", not(target_arch = "wasm32")))]
        {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("runtime");
            assert_eq!(
                format!("{:?}", Spawn::Tokio(runtime.handle().clone())),
                "Tokio"
            );
        }
    }

    /// **Inline, nothing is deferred and no pause is added.** The edits of a
    /// load have landed between two passes by construction, as they always
    /// did, and the host keeps the only say over pauses.
    #[test]
    fn inline_loads_neither_defer_edits_nor_pause() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Counting(Arc<AtomicUsize>);
        impl TileTree for Counting {
            fn roots(&self) -> Vec<TileId> {
                vec![TileId(0)]
            }
            fn children(&self, _id: TileId) -> Vec<TileId> {
                Vec::new()
            }
            fn properties(&self, _id: TileId) -> crate::source::TileProperties {
                crate::source::TileProperties {
                    bounding_volume: crate::math::BoundingVolume::Sphere(crate::math::Sphere {
                        center: glam::DVec3::ZERO,
                        radius: 1.0,
                    }),
                    geometric_error: 1.0,
                    refine: crate::tileset::Refine::Replace,
                    has_content: false,
                }
            }
            fn defer_edits(&self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        struct Never;
        #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
        #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
        impl TileLoader for Never {
            async fn load(&self, _id: TileId) -> Result<Loaded, LoadError> {
                futures_util::future::pending().await
            }
        }
        let deferred = Arc::new(AtomicUsize::new(0));
        let (mut stream, server) = in_process_with(
            Box::new(Counting(Arc::clone(&deferred))),
            Arc::new(Never),
            unpinned(),
        );
        let mut server = Box::pin(server.run());
        look(&stream, near_view(), 1);
        listen(&mut server, &mut stream, 4);
        assert_eq!(deferred.load(Ordering::SeqCst), 0, "inline defers nothing");
        assert!(default_breather(&Loads::new(&Spawn::Inline)).is_none());
    }

    /// Spawned loads defer, and pause for the runtime by default; a result
    /// that has landed is taken without waiting.
    #[cfg(all(feature = "rt-tokio", not(target_arch = "wasm32")))]
    #[test]
    fn spawned_loads_land_without_waiting_and_pause_by_default() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .build()
            .expect("runtime");
        let mut loads = Loads::new(&Spawn::Tokio(runtime.handle().clone()));
        assert!(loads.spawns());
        assert!(default_breather(&loads).is_some());
        for tile in [TileId(1), TileId(2)] {
            let (handle, registration) = AbortHandle::new_pair();
            let fut: BoxLoadFut =
                Box::pin(async move { ((tile, Ok(Loaded::Expanded)), crate::metrics::stamp()) });
            loads.push(Abortable::new(fut, registration), &handle);
        }
        let mut landed = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while landed.len() < 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "try_next never handed over what had landed"
            );
            if let Some(Ok(((tile, _), _))) = loads.try_next() {
                landed.push(tile.0);
            }
            std::thread::yield_now();
        }
        landed.sort_unstable();
        assert_eq!(landed, vec![1, 2]);
        assert!(loads.try_next().is_none(), "nothing more");
    }

    /// **A request the network never answers costs one slot, not the
    /// session.** One leaf's load never completes — a browser `fetch` with no
    /// answer, before transports bounded their requests. With room for two
    /// loads at once, every other tile still arrives through the other slot,
    /// the leaf's parent stays selected over its ground — the ancestor drawn,
    /// not a hole — and the server keeps answering views.
    #[test]
    fn a_load_that_never_answers_holds_one_slot_and_the_ancestor_stays() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = write_deep_fixture(dir.path());
        let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
        let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");
        let root = tileset.root();
        let mids = tileset.tile(root).children.clone();
        let stalled = tileset.tile(mids[0]).children[0];
        let others: Vec<TileId> = mids
            .iter()
            .flat_map(|m| tileset.tile(*m).children.clone())
            .filter(|t| *t != stalled)
            .collect();

        /// Real content for every tile but one, which never answers.
        struct Silent(Arc<dyn TileLoader>, TileId);

        #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
        #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
        impl TileLoader for Silent {
            async fn load(&self, id: TileId) -> Result<Loaded, LoadError> {
                if id == self.1 {
                    return futures_util::future::pending().await;
                }
                self.0.load(id).await
            }
        }

        let arena = Arc::new(RwLock::new(tileset));
        let tree: Box<dyn TileTree> = Box::new(TilesetTree::new(Arc::clone(&arena)));
        let inner: Arc<dyn TileLoader> = Arc::new(TilesetLoader::new(arena, Arc::new(FsFetcher)));
        let (mut stream, server) = in_process_with(
            tree,
            Arc::new(Silent(inner, stalled)) as Arc<dyn TileLoader>,
            Config {
                pinned_level: None,
                maximum_simultaneous_fetches: 2,
                ..Config::default()
            },
        );
        let mut server = Box::pin(server.run());
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let mut delivered: HashSet<TileId> = HashSet::new();
        let mut selected: HashSet<TileId> = HashSet::new();
        let mut selections = 0usize;
        // The camera keeps sending views, as a frame loop does.
        for _ in 0..64 {
            stream
                .send(ClientMessage::ViewerState {
                    views: vec![near_view()],
                    generation: 0,
                })
                .expect("the server still listens");
            for _ in 0..16 {
                assert!(
                    server.as_mut().poll(&mut cx).is_pending(),
                    "the session ended over one silent request"
                );
                while let Poll::Ready(Some(msg)) = stream.poll_message(&mut cx) {
                    match msg {
                        ServerMessage::Content { tile, .. } => {
                            delivered.insert(tile);
                            let _ = stream.send(ClientMessage::Ack { tile });
                        }
                        ServerMessage::Select { tiles, .. } => {
                            selections += 1;
                            selected = tiles.into_iter().map(|(t, _)| t).collect();
                        }
                        ServerMessage::Error { message, .. } => {
                            unreachable!("unexpected error: {message}")
                        }
                        _ => {}
                    }
                }
            }
        }
        for tile in &others {
            assert!(
                delivered.contains(tile),
                "{tile:?} never arrived past the silent load"
            );
        }
        assert!(!delivered.contains(&stalled));
        assert!(
            selected.contains(&mids[0]),
            "the silent leaf's parent stands over its ground: {selected:?}"
        );
        assert!(selections > 1, "selections kept coming");
    }

    /// **Taking back a stand-in must never cost the consumer real content.**
    ///
    /// `Content` is sent exactly once per residency: the server marks the tile
    /// resident and never re-sends it. So anything that makes the consumer
    /// discard that one delivery — including its copy still waiting in the
    /// upload queue, which is the state `Evict` explicitly purges — loses the
    /// tile for as long as the server's cache holds it. Measured in the viewer:
    /// 1853 tiles taken back in one flight, 232 of them re-issued a stand-in
    /// right after, coarse for the rest of the session with nothing loading.
    ///
    /// The consumer that matters here is the honest one: it uploads on a
    /// budget, so its ack arrives *later* than the content. This test holds
    /// that gap open by never acking, and asserts the server still never sends
    /// an `Evict` for a tile whose content it has delivered — a take-back of
    /// the stand-in must travel as [`ServerMessage::Retire`], which the
    /// consumer applies only to surfaces it holds *as stand-ins*.
    #[test]
    fn a_stand_in_take_back_never_travels_as_evict() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = write_deep_fixture(dir.path());
        let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
        let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");

        struct Standing(Arc<dyn TileLoader>);
        #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
        #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
        impl TileLoader for Standing {
            async fn load(&self, id: TileId) -> Result<Loaded, LoadError> {
                self.0.load(id).await
            }
            fn fill(&self, _id: TileId) -> Option<crate::content::DecodedTileContent> {
                Some(crate::content::DecodedTileContent {
                    withheld_drape: None,
                    meshes: Vec::new(),
                    textures: Vec::new(),
                    imagery: Vec::new(),
                    local_origin_ecef: glam::DVec3::ZERO,
                    transform_local: glam::Mat4::IDENTITY,
                })
            }
        }

        let arena = Arc::new(RwLock::new(tileset));
        let tree: Box<dyn TileTree> = Box::new(TilesetTree::new(Arc::clone(&arena)));
        let inner: Arc<dyn TileLoader> = Arc::new(TilesetLoader::new(arena, Arc::new(FsFetcher)));
        let (mut stream, server) = in_process_with(
            tree,
            Arc::new(Standing(inner)) as Arc<dyn TileLoader>,
            Config {
                pinned_level: None,
                ..Config::default()
            },
        );
        let mut server = Box::pin(server.run());

        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let mut delivered: HashSet<TileId> = HashSet::new();
        let mut evicted_after_delivery: Vec<TileId> = Vec::new();

        // Look somewhere, then elsewhere — the move is what makes the first
        // view's stand-ins leave the selection. **Never ack**: the upload queue
        // of a real consumer is exactly this window, held open.
        for view in [near_view(), far_view()] {
            stream
                .send(ClientMessage::ViewerState {
                    views: vec![view],
                    generation: 0,
                })
                .expect("send");
            for _ in 0..256 {
                let _ = server.as_mut().poll(&mut cx);
                while let Poll::Ready(Some(msg)) = stream.poll_message(&mut cx) {
                    match msg {
                        ServerMessage::Content { tile, .. } => {
                            delivered.insert(tile);
                        }
                        ServerMessage::Evict { tiles } => {
                            for t in tiles {
                                if delivered.contains(&t) {
                                    evicted_after_delivery.push(t);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        assert!(
            evicted_after_delivery.is_empty(),
            "{} tiles were named by an Evict after their one Content delivery, \
             with no cache pressure to justify it: the consumer purges them from \
             its upload queue and the server never re-sends — that ground is a \
             stand-in for the rest of the session. A stand-in take-back must be \
             a Retire. Tiles: {evicted_after_delivery:?}",
            evicted_after_delivery.len()
        );
    }

    /// A turn must not re-stream its own wake.
    ///
    /// The tiles the camera has just left are the ones it is most likely to want
    /// back, and evicting them the instant they leave the frustum was measured at
    /// 150 reloads over the second half of an orbit. What keeps them is headroom
    /// and nothing else: an LRU only evicts when it is over its cap, so with room
    /// for both working sets the previous view's tiles simply stay, ordered
    /// behind the current one's and ahead of anything older.
    ///
    /// This is where a ring of "recent camera positions" used to be — a second
    /// mechanism protecting what ordering already protects, and one that, when
    /// it was also allowed to *evict*, threw away 93 185 tiles in a session.
    /// A consumer that has not acknowledged a selected tile is sent a stand-in.
    ///
    /// The trigger is the *consumer's* gap, not the server's: a tile is selected
    /// only once it is resident here, and drawn only once it is uploaded there.
    /// Between the two lie a channel, a queue and an upload budget, and in those
    /// frames the consumer falls back to an ancestor that also covers the
    /// missing tile's siblings — two surfaces over one patch of ground.
    ///
    /// This test never acks, which is the extreme of that gap and the only shape
    /// a test can hold still. Written first against "selected but not resident",
    /// where it failed with "the view selected nothing to stand in for" — a
    /// condition that cannot occur, because selection implies residency.
    #[test]
    fn a_tile_the_consumer_has_not_acknowledged_gets_a_stand_in() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = write_deep_fixture(dir.path());
        let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
        let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");

        /// Real content for everything, and a stand-in on demand. Both halves
        /// matter: without content nothing is ever selected, and without a fill
        /// there is nothing to observe.
        struct Standing(Arc<dyn TileLoader>);

        #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
        #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
        impl TileLoader for Standing {
            async fn load(&self, id: TileId) -> Result<Loaded, LoadError> {
                self.0.load(id).await
            }
            fn fill(&self, _id: TileId) -> Option<crate::content::DecodedTileContent> {
                Some(crate::content::DecodedTileContent {
                    withheld_drape: None,
                    meshes: Vec::new(),
                    textures: Vec::new(),
                    imagery: Vec::new(),
                    local_origin_ecef: glam::DVec3::ZERO,
                    transform_local: glam::Mat4::IDENTITY,
                })
            }
        }

        let arena = Arc::new(RwLock::new(tileset));
        let tree: Box<dyn TileTree> = Box::new(TilesetTree::new(Arc::clone(&arena)));
        let inner: Arc<dyn TileLoader> = Arc::new(TilesetLoader::new(arena, Arc::new(FsFetcher)));
        let (mut stream, server) = in_process_with(
            tree,
            Arc::new(Standing(inner)) as Arc<dyn TileLoader>,
            // The default configuration, deliberately: stand-ins are on by
            // default, and a test that switched them on itself would keep
            // passing after the default was quietly turned back off.
            Config {
                pinned_level: None,
                ..Config::default()
            },
        );
        let mut server = Box::pin(server.run());
        stream
            .send(ClientMessage::ViewerState {
                views: vec![near_view()],
                generation: 0,
            })
            .expect("send");

        // Drive to quiescence without ever acking, which is what a consumer
        // whose uploads never finish looks like from here.
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let mut selected: Vec<TileId> = Vec::new();
        let mut filled: HashSet<TileId> = HashSet::new();
        for _ in 0..64 {
            let _ = server.as_mut().poll(&mut cx);
            while let Poll::Ready(Some(msg)) = stream.poll_message(&mut cx) {
                match msg {
                    ServerMessage::Select { tiles, .. } => {
                        selected = tiles.iter().map(|(t, _)| *t).collect();
                    }
                    ServerMessage::Fill { tile, .. } => {
                        filled.insert(tile);
                    }
                    ServerMessage::Evict { tiles } => {
                        for t in tiles {
                            filled.remove(&t);
                        }
                    }
                    _ => {}
                }
            }
        }

        assert!(!selected.is_empty(), "the view selected nothing");
        for tile in &selected {
            assert!(
                filled.contains(tile),
                "{tile:?} is selected and unacknowledged, and got no stand-in \
                 (stand-ins: {filled:?})"
            );
        }
    }

    /// A loader whose content changes when it is told to, as a globe's does
    /// when its imagery is switched. Each tile's content carries the epoch it
    /// was loaded under — in its origin, which nothing here reads — so a
    /// consumer can tell the old picture from the new.
    struct Turning {
        inner: Arc<dyn TileLoader>,
        epoch: Arc<std::sync::atomic::AtomicU64>,
    }

    #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
    #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
    impl TileLoader for Turning {
        async fn load(&self, id: TileId) -> Result<Loaded, LoadError> {
            let epoch = self.epoch.load(std::sync::atomic::Ordering::SeqCst);
            match self.inner.load(id).await? {
                Loaded::Content(mut content) => {
                    content.local_origin_ecef = dvec3(epoch as f64, 0.0, 0.0);
                    Ok(Loaded::Content(content))
                }
                other => Ok(other),
            }
        }

        fn epoch(&self) -> u64 {
            self.epoch.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    /// **Changing what every tile shows never leaves a tile showing nothing.**
    ///
    /// The consumer here is the simplest honest one: it files content under
    /// its tile, replaces what was there, and drops a tile only on `Evict`.
    /// After every step of the server — every frame, in a viewer — each tile
    /// of the selection must be held. The loader's epoch is then moved on,
    /// as a switch of imagery does, and the same must stay true at every step
    /// until every selected tile holds the new content.
    ///
    /// Seen failing with the old content dropped first — an `Evict` ahead of
    /// each refresh, the "drop and re-request" implementation: 'selected and
    /// held by nobody' on the first step after the change.
    ///
    /// Also held: the turn-over is coarse first, the new content replaces the
    /// old under the same id with no eviction in between, and a tile is
    /// refreshed once.
    #[test]
    fn a_change_of_epoch_replaces_every_tile_without_ever_dropping_one() {
        use std::sync::atomic::{AtomicU64, Ordering};

        let dir = tempfile::tempdir().expect("tempdir");
        let url = write_deep_fixture(dir.path());
        let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
        let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");
        let arena = Arc::new(RwLock::new(tileset));
        let tree: Box<dyn TileTree> = Box::new(TilesetTree::new(Arc::clone(&arena)));
        let levels = TilesetTree::new(Arc::clone(&arena));
        let inner: Arc<dyn TileLoader> = Arc::new(TilesetLoader::new(arena, Arc::new(FsFetcher)));
        let epoch = Arc::new(AtomicU64::new(0));
        let (mut stream, server) = in_process_with(
            tree,
            Arc::new(Turning {
                inner,
                epoch: Arc::clone(&epoch),
            }) as Arc<dyn TileLoader>,
            Config {
                pinned_level: None,
                stand_ins: false,
                ..Config::default()
            },
        );
        let mut server: Pin<Box<dyn Future<Output = ()>>> = Box::pin(server.run());
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);

        // What the consumer holds: tile → the epoch of the content filed there.
        let mut held: HashMap<TileId, u64> = HashMap::new();
        let mut selected: Vec<TileId> = Vec::new();
        // Every delivery after the change, in order: (level, tile).
        let mut turned: Vec<(u32, TileId)> = Vec::new();
        let mut evicted_after_the_change = 0usize;

        let mut step = |server: &mut Pin<Box<dyn Future<Output = ()>>>,
                        stream: &mut InProcessStream,
                        held: &mut HashMap<TileId, u64>,
                        selected: &mut Vec<TileId>,
                        turned: &mut Vec<(u32, TileId)>,
                        evicted: &mut usize,
                        changed: bool| {
            stream
                .send(ClientMessage::ViewerState {
                    views: vec![near_view()],
                    generation: 0,
                })
                .expect("send");
            let _ = server.as_mut().poll(&mut cx);
            while let Poll::Ready(Some(msg)) = stream.poll_message(&mut cx) {
                match msg {
                    ServerMessage::Select { tiles, .. } => {
                        *selected = tiles.iter().map(|(t, _)| *t).collect();
                    }
                    ServerMessage::Content { tile, content, .. } => {
                        let TileContent::Decoded(decoded) = content else {
                            unreachable!("in-process content is decoded")
                        };
                        let at = decoded.local_origin_ecef.x as u64;
                        if changed {
                            turned.push((levels.level(tile), tile));
                        }
                        held.insert(tile, at);
                        stream.send(ClientMessage::Ack { tile }).expect("ack");
                    }
                    ServerMessage::Evict { tiles } => {
                        for t in tiles {
                            held.remove(&t);
                            if changed {
                                *evicted += 1;
                            }
                        }
                    }
                    _ => {}
                }
            }
            // The frame: everything selected has something to draw.
            for tile in selected.iter() {
                assert!(
                    held.contains_key(tile),
                    "{tile:?} is selected and held by nobody (changed: {changed})"
                );
            }
        };

        for _ in 0..64 {
            step(
                &mut server,
                &mut stream,
                &mut held,
                &mut selected,
                &mut turned,
                &mut evicted_after_the_change,
                false,
            );
        }
        assert!(selected.len() > 1, "the view settled on {selected:?}");
        assert!(held.values().all(|at| *at == 0));
        let before: HashSet<TileId> = held.keys().copied().collect();

        // The switch.
        epoch.store(1, Ordering::SeqCst);
        for _ in 0..64 {
            step(
                &mut server,
                &mut stream,
                &mut held,
                &mut selected,
                &mut turned,
                &mut evicted_after_the_change,
                true,
            );
        }

        // Every tile held is now the new content — the selection, and the
        // ancestors a consumer falls back to.
        for (tile, at) in &held {
            assert_eq!(*at, 1, "{tile:?} still shows the old content");
        }
        assert_eq!(
            held.keys().copied().collect::<HashSet<_>>(),
            before,
            "the change added or lost tiles"
        );
        assert_eq!(evicted_after_the_change, 0, "a refresh evicts nothing");
        // Coarse first, and once each.
        let order: Vec<u32> = turned.iter().map(|(level, _)| *level).collect();
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(order, sorted, "the turn-over was not coarse first");
        assert_eq!(turned.len(), before.len(), "refreshed {turned:?}");
    }

    #[test]
    fn tiles_the_camera_just_left_survive_the_next_move() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = write_deep_fixture(dir.path());
        let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
        let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");

        // Room for the whole fixture, which is the point: nothing is over any
        // ceiling, so nothing may be reclaimed.
        let config = Config::default();
        let (mut stream, server) = in_process(tileset, Arc::new(FsFetcher), config);
        let mut server = Box::pin(server.run());

        stream
            .send(ClientMessage::ViewerState {
                views: vec![near_view()],
                generation: 0,
            })
            .expect("send");
        let near_selection = selection_after(&mut server, &mut stream);
        assert!(
            near_selection.len() > 1,
            "the near view selected several tiles"
        );

        stream
            .send(ClientMessage::ViewerState {
                views: vec![far_view()],
                generation: 0,
            })
            .expect("send");
        let (_, evicted) = settle(&mut server, &mut stream).expect("far view settles");

        let lost: Vec<_> = evicted
            .iter()
            .filter(|t| near_selection.contains(t))
            .collect();
        assert!(
            lost.is_empty(),
            "the camera moved once and already lost {lost:?} from where it came"
        );
    }

    /// Drives one view to quiescence and reports what it settled on.
    fn selection_after(
        server: &mut Pin<Box<impl Future<Output = ()>>>,
        stream: &mut InProcessStream,
    ) -> HashSet<TileId> {
        const MAX_STEPS: usize = 2_000;
        let waker = futures_util::task::noop_waker();
        let mut cx = std::task::Context::from_waker(&waker);
        let mut selection = HashSet::new();
        let mut quiet = 0;
        for _ in 0..MAX_STEPS {
            let _ = server.as_mut().poll(&mut cx);
            let mut got = false;
            while let Poll::Ready(Some(msg)) = stream.poll_message(&mut cx) {
                got = true;
                if let ServerMessage::Select { tiles, .. } = msg {
                    selection = tiles.iter().map(|(t, _)| *t).collect();
                }
            }
            quiet = if got { 0 } else { quiet + 1 };
            if quiet > 8 {
                break;
            }
        }
        selection
    }

    /// A loader that refuses to serve the same tile past a sane number of
    /// times.
    ///
    /// The failure this guards against is a livelock, and a livelock here does
    /// not fail a test on its own. `settle` gives up after a fixed number of
    /// steps and returns whatever it saw, so a server reloading the same tile
    /// for ever looks, from the outside, exactly like one that finished: the
    /// selection is right, the eviction counts are plausible, and nothing
    /// asserts. It is only visible against the network, which a test does not
    /// have. Counting loads is what turns it into an assertion — and the count
    /// has to be per tile, since the totals alone stay unremarkable.
    struct CountingLoader {
        inner: Arc<dyn TileLoader>,
        counts: Arc<Mutex<HashMap<TileId, usize>>>,
    }

    /// Generous: a tile may legitimately be loaded again after the camera has
    /// genuinely left and come back. Nothing converging comes near this.
    const SANE_RELOADS: usize = 20;

    #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
    #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
    impl TileLoader for CountingLoader {
        async fn load(&self, tile: TileId) -> Result<Loaded, LoadError> {
            let seen = {
                let mut counts = self.counts.lock().expect("counts");
                let seen = counts.entry(tile).or_insert(0);
                *seen += 1;
                *seen
            };
            assert!(
                seen <= SANE_RELOADS,
                "{tile:?} has been loaded {seen} times: the server is expiring \
                 content it is about to ask for again"
            );
            self.inner.load(tile).await
        }
    }

    /// Expiry must not reclaim what the current camera position still needs.
    ///
    /// One camera position takes many passes to settle, and a tile that has just
    /// arrived is often selected by none of them — `forbid_holes` draws its
    /// parent until all four siblings are ready. It is then resident, not
    /// selected, and no longer requested. Recording only the newest pass's needs
    /// drops it from the sole generation that ever named it; expiry takes it;
    /// the next pass asks for it again. Nothing converges, and because the loads
    /// resolve inside the same poll, nothing yields either.
    #[test]
    fn a_still_camera_does_not_expire_what_it_is_about_to_ask_for() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = write_deep_fixture(dir.path());
        let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
        let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");

        let arena = Arc::new(RwLock::new(tileset));
        let tree: Box<dyn TileTree> = Box::new(TilesetTree::new(Arc::clone(&arena)));
        let counts = Arc::new(Mutex::new(HashMap::new()));
        let loader: Arc<dyn TileLoader> = Arc::new(CountingLoader {
            inner: Arc::new(TilesetLoader::new(arena, Arc::new(FsFetcher))),
            counts: Arc::clone(&counts),
        });

        // A cap low enough that the LRU must evict while the view is still
        // filling in — the configuration in which a reload cycle is easiest to
        // close, since eviction and loading are then happening at once.
        let config = Config {
            resident_tile_limit: 3,
            // The fixture is three levels deep, so the default pinned floor
            // would cover all of it and nothing could ever be reclaimed. The
            // pin is a production guarantee, not a property under test here.
            pinned_level: None,
            ..Config::default()
        };
        let (mut stream, server) = in_process_with(tree, loader, config);
        let mut server = Box::pin(server.run());

        stream
            .send(ClientMessage::ViewerState {
                views: vec![near_view()],
                generation: 0,
            })
            .expect("send");
        settle(&mut server, &mut stream).expect("a still camera settles");

        let counts = counts.lock().expect("counts");
        assert!(!counts.is_empty(), "the near view loaded something");
        for (tile, seen) in counts.iter() {
            assert_eq!(
                *seen, 1,
                "{tile:?} was loaded {seen} times for one camera position"
            );
        }
    }

    /// A camera going back and forth over a cache too small for both views has
    /// to converge, not oscillate. Each move evicts what the other view wanted;
    /// the guarantee is that every pass still reaches quiescence rather than
    /// evicting and reloading inside a single traversal.
    #[test]
    fn a_camera_moving_back_and_forth_converges() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = write_deep_fixture(dir.path());
        let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
        let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");

        let config = Config {
            resident_tile_limit: 2,
            // The fixture is three levels deep, so the default pinned floor
            // would cover all of it and nothing could ever be reclaimed. The
            // pin is a production guarantee, not a property under test here.
            pinned_level: None,
            ..Config::default()
        };
        let (mut stream, server) = in_process(tileset, Arc::new(FsFetcher), config);
        let mut server = Box::pin(server.run());

        for view in [near_view(), far_view(), near_view(), far_view()] {
            stream
                .send(ClientMessage::ViewerState {
                    views: vec![view],
                    generation: 0,
                })
                .expect("send");
            settle(&mut server, &mut stream).expect("settles");
        }
    }

    /// The cap is what bounds a session: park the camera somewhere that needs
    /// fewer tiles than it holds, and the ones it left must be reclaimed.
    #[test]
    fn tiles_left_behind_by_the_camera_are_evicted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = write_deep_fixture(dir.path());
        let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
        let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");

        // Room for a working set, not for the whole tree: the tiles the camera
        // leaves behind are what has to go. The count is the ceiling that
        // matters — the fixture's tiles are 3-vertex triangles, so no plausible
        // byte budget would ever separate them.
        let config = Config {
            resident_tile_limit: 3,
            // The fixture is three levels deep, so the default pinned floor
            // would cover all of it and nothing could ever be reclaimed. The
            // pin is a production guarantee, not a property under test here.
            pinned_level: None,
            ..Config::default()
        };
        let (mut stream, server) = in_process(tileset, Arc::new(FsFetcher), config);
        let mut server = Box::pin(server.run());

        stream
            .send(ClientMessage::ViewerState {
                views: vec![near_view()],
                generation: 0,
            })
            .expect("send");
        let (loaded, _) = settle(&mut server, &mut stream).expect("first view settles");
        assert!(
            loaded > 1,
            "the near view loaded several tiles (got {loaded})"
        );

        // Pull far back and stay there. The far view needs fewer tiles than the
        // near one did, so what it stops selecting stops being protected, and
        // scores lowest for the camera's new distance.
        let moves = 3;
        let mut evicted = Vec::new();
        for _ in 0..moves {
            stream
                .send(ClientMessage::ViewerState {
                    views: vec![far_view()],
                    generation: 0,
                })
                .expect("send");
            let (_, dropped) = settle(&mut server, &mut stream).expect("far view settles");
            evicted.extend(dropped);
        }

        assert!(
            !evicted.is_empty(),
            "after {moves} moves away, the tiles left behind were still not reclaimed"
        );
    }

    /// Depletion order is by what a tile is worth from where the camera *is*,
    /// which is both its depth and its distance in one number — its
    /// screen-space error.
    ///
    /// Pull back until the root alone satisfies the error, and the leaves the
    /// near view pulled in are worth almost nothing while the coarse tiles
    /// covering the same ground are worth a great deal. So the leaves must go
    /// first and the coarse tiles last, whatever order they arrived in. Age
    /// alone gets this backwards on a zoom-out: the root is the *oldest* tile
    /// in the cache and the leaves are the newest.
    #[test]
    fn depletion_takes_the_deepest_tiles_before_the_coarse_ones() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = write_deep_fixture(dir.path());
        let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
        let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");

        // A second view of the same tileset, to read each tile's geometric error
        // back out — the test must not hard-code which id is a leaf.
        let reference = Tileset::from_json_bytes(&bytes, &url).expect("tileset");
        let tree = TilesetTree::new(Arc::new(RwLock::new(reference)));
        let error_of = |t: TileId| tree.properties(t).geometric_error;

        // Tight enough that the sweep has to cross a level. A cap that only
        // forces it to drop a couple of leaves proves nothing about order —
        // every candidate has the same geometric error, and any sweep at all
        // looks correct.
        let config = Config {
            resident_tile_limit: 2,
            // The fixture is three levels deep, so the default pinned floor
            // would cover all of it and nothing could ever be reclaimed. The
            // pin is a production guarantee, not a property under test here.
            pinned_level: None,
            ..Config::default()
        };
        let (mut stream, server) = in_process(tileset, Arc::new(FsFetcher), config);
        let mut server = Box::pin(server.run());

        stream
            .send(ClientMessage::ViewerState {
                views: vec![near_view()],
                generation: 0,
            })
            .expect("send");
        settle(&mut server, &mut stream).expect("the near view settles");

        stream
            .send(ClientMessage::ViewerState {
                views: vec![far_view()],
                generation: 0,
            })
            .expect("send");
        let (_, evicted) = settle(&mut server, &mut stream).expect("the far view settles");
        assert!(!evicted.is_empty(), "pulling back reclaimed nothing at all");

        // Deepest first: a smaller geometric error is a finer tile, and the
        // sweep must work its way up the pyramid, never down.
        //
        // This fixture has three levels and drops a whole level at a time, so
        // every tile in one sweep tends to share a geometric error and the
        // ordering here cannot, on its own, tell the weight apart from plain
        // recency — checked, and it passes either way. It stands as a regression
        // guard; what proves the weight is
        // [`the_depletion_weight_rises_with_depth_and_with_distance`].
        let errors: Vec<f64> = evicted.iter().map(|t| error_of(*t)).collect();
        for pair in errors.windows(2) {
            assert!(
                pair[0] <= pair[1],
                "the sweep took a coarser tile before a finer one: {errors:?}"
            );
        }
    }

    fn near_view() -> ViewState {
        ViewState::perspective(
            dvec3(0.0, 0.0, 120.0),
            dvec3(0.0, 0.0, -1.0),
            dvec3(0.0, 1.0, 0.0),
            dvec2(1024.0, 768.0),
            std::f64::consts::FRAC_PI_3,
        )
    }

    /// Far enough that the root alone satisfies the SSE — the leaves the near
    /// view pulled in become dead weight.
    fn far_view() -> ViewState {
        ViewState::perspective(
            dvec3(0.0, 0.0, 100_000.0),
            dvec3(0.0, 0.0, -1.0),
            dvec3(0.0, 1.0, 0.0),
            dvec2(1024.0, 768.0),
            std::f64::consts::FRAC_PI_3,
        )
    }

    #[test]
    fn end_to_end_session_converges_to_leaves() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = write_fixture(dir.path());
        let bytes = std::fs::read(url.to_file_path().expect("path")).expect("read");
        let tileset = Tileset::from_json_bytes(&bytes, &url).expect("tileset");
        let root = tileset.root();
        let leaves = tileset.tile(root).children.clone();

        let (mut stream, server) = in_process(tileset, Arc::new(FsFetcher), Config::default());

        let mut pool = futures_executor::LocalPool::new();
        pool.spawner().spawn_local(server.run()).expect("spawn");

        let expected: HashSet<TileId> = leaves.iter().copied().collect();
        let final_selection = pool.run_until(async move {
            stream
                .send(ClientMessage::ViewerState {
                    views: vec![near_view()],
                    generation: 0,
                })
                .expect("send");

            let mut contents: Vec<TileId> = Vec::new();
            let mut last_select: Vec<(TileId, f64)> = Vec::new();
            while let Some(msg) = stream.next_message().await {
                match msg {
                    ServerMessage::Content { tile, content, .. } => {
                        assert!(
                            matches!(content, TileContent::Decoded(_)),
                            "in-process content is always decoded"
                        );
                        contents.push(tile);
                    }
                    ServerMessage::Retire { .. } => {}
                    // Stand-ins are not content: this test counts what actually
                    // arrived, and folding them in would let it pass on
                    // approximations.
                    ServerMessage::Fill { .. } => {}
                    ServerMessage::Select { tiles, .. } => {
                        last_select = tiles;
                        // Steady state: both leaves selected, nothing pending.
                        let sel: HashSet<TileId> = last_select.iter().map(|(t, _)| *t).collect();
                        if sel == expected {
                            break;
                        }
                    }
                    ServerMessage::Error { message, .. } => {
                        unreachable!("unexpected error: {message}")
                    }
                    ServerMessage::Evict { .. } | ServerMessage::Priming(_) => {}
                }
            }
            (contents, last_select)
        });

        let (contents, selection) = final_selection;
        let sel: HashSet<TileId> = selection.iter().map(|(t, _)| *t).collect();
        assert_eq!(sel, leaves.iter().copied().collect::<HashSet<_>>());
        assert!(!sel.contains(&root), "REPLACE: leaves displaced the root");
        assert!(
            contents.len() >= 2,
            "both leaves must have been decoded (got {contents:?})"
        );
    }

    #[test]
    fn external_tileset_is_grafted_and_its_leaf_rendered() {
        let dir = tempfile::tempdir().expect("tempdir");
        let glb = crate::content::tests::test_glb();
        std::fs::create_dir(dir.path().join("sub")).expect("mkdir");
        std::fs::write(dir.path().join("sub/leaf.glb"), &glb).expect("write");
        // The external root resolves its content against ITS json, in sub/.
        std::fs::write(
            dir.path().join("sub/external.json"),
            r#"{ "asset": { "version": "1.1" }, "geometricError": 4,
                 "root": { "boundingVolume": { "sphere": [0,0,0,80] },
                           "geometricError": 0, "content": { "uri": "leaf.glb" } } }"#,
        )
        .expect("write external");
        let root_json = r#"{
          "asset": { "version": "1.1" }, "geometricError": 200,
          "root": { "boundingVolume": { "sphere": [0,0,0,100] },
                    "geometricError": 0, "refine": "REPLACE",
                    "content": { "uri": "sub/external.json" } }
        }"#;
        let path = dir.path().join("tileset.json");
        std::fs::write(&path, root_json).expect("write root");
        let url = Url::from_file_path(&path).expect("url");
        let tileset = Tileset::from_json_bytes(root_json.as_bytes(), &url).expect("ts");

        let (mut stream, server) = in_process(tileset, Arc::new(FsFetcher), Config::default());
        let mut pool = futures_executor::LocalPool::new();
        pool.spawner().spawn_local(server.run()).expect("spawn");

        let saw_content = pool.run_until(async move {
            stream
                .send(ClientMessage::ViewerState {
                    views: vec![near_view()],
                    generation: 0,
                })
                .expect("send");
            while let Some(msg) = stream.next_message().await {
                match msg {
                    // The grafted external leaf must arrive decoded.
                    ServerMessage::Content { content, .. } => {
                        return matches!(content, TileContent::Decoded(_));
                    }
                    ServerMessage::Error { message, .. } => {
                        unreachable!("unexpected error: {message}")
                    }
                    _ => {}
                }
            }
            false
        });
        assert!(saw_content, "external tileset leaf decoded end-to-end");
    }

    #[test]
    fn missing_content_reports_error_not_panic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tileset_json = r#"{
          "asset": { "version": "1.1" },
          "geometricError": 200,
          "root": {
            "boundingVolume": { "sphere": [0, 0, 0, 100] },
            "geometricError": 0,
            "refine": "REPLACE",
            "content": { "uri": "missing.glb" }
          }
        }"#;
        let path = dir.path().join("tileset.json");
        std::fs::write(&path, tileset_json).expect("write");
        let url = Url::from_file_path(&path).expect("url");
        let tileset = Tileset::from_json_bytes(tileset_json.as_bytes(), &url).expect("tileset");

        let (mut stream, server) = in_process(tileset, Arc::new(FsFetcher), Config::default());
        let mut pool = futures_executor::LocalPool::new();
        pool.spawner().spawn_local(server.run()).expect("spawn");

        let got_error = pool.run_until(async move {
            stream
                .send(ClientMessage::ViewerState {
                    views: vec![near_view()],
                    generation: 0,
                })
                .expect("send");
            while let Some(msg) = stream.next_message().await {
                if let ServerMessage::Error { tile, .. } = msg {
                    return tile.is_some();
                }
            }
            false
        });
        assert!(got_error, "fetch failure must surface as a typed Error");
    }
}
