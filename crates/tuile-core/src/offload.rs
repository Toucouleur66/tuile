// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Where a loader's blocking, CPU-bound work runs.
//!
//! The server is deliberately one future — `runtime.rs` spawns nothing, so it
//! can be driven by tokio, by `spawn_local` on wasm, or by `futures_executor`
//! in a test. That is the right shape for the *orchestration*, and the wrong
//! one for the work a load actually does: decoding a JPEG, resampling it onto
//! geographic spacing, decoding a quantized mesh. None of it awaits anything.
//! An `async fn` that runs it inline holds the polling thread for its whole
//! duration, and every other load in the `FuturesUnordered` waits its turn.
//!
//! Measured before this existed: 4060 samples in the resampling loop, **all of
//! them on the single `geometry-server` thread**, not one on a tokio worker,
//! while `loads_in_flight` sat pinned at its ceiling of 64. Sixty-four loads in
//! flight, one core doing them, the rest of the machine asleep.
//!
//! So the seam. [`decode`](crate::content::decode) and friends stay free,
//! blocking, I/O-free functions; this says where the caller puts them. The
//! default is [`Inline`], which is what wasm needs (no threads to hand work to)
//! and what a test wants (deterministic, no runtime). A native host installs
//! one backed by its own pool — `tokio::task::spawn_blocking`, rayon, a thread
//! of its own — and the core never learns which.
//!
//! Not compiled, and it cannot be: naming `tokio` is the whole point of the
//! example, and `tuile-core` depending on a runtime to typecheck its own
//! documentation is exactly the thing this seam exists to prevent.
//!
//! ```text
//! struct Blocking;
//! impl Offload for Blocking {
//!     fn submit(&self, job: Job) {
//!         tokio::task::spawn_blocking(job);
//!     }
//! }
//! ```

use std::cell::Cell;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use crate::budget::ResourceId;

// Const already; clippy on Android's target reads the expansion otherwise.
#[allow(clippy::missing_const_for_thread_local)]
mod loading_slot {
    use super::*;
    thread_local! {
        pub(super) static LOADING: Cell<Option<ResourceId>> = const { Cell::new(None) };
    }
}
use loading_slot::LOADING;

/// The resource whose load is being polled on this thread, if any: what a
/// job submitted from inside a load is for. A pool that orders its queue
/// reads it at submission, and asks [`LoadRanks`] where that load stands.
pub fn loading() -> Option<ResourceId> {
    LOADING.with(Cell::get)
}

/// A load's future, naming its resource to [`loading`] while it is polled.
pub struct ForLoad<F> {
    resource: ResourceId,
    inner: Pin<Box<F>>,
}

impl<F: Future> ForLoad<F> {
    pub fn new(resource: ResourceId, inner: F) -> Self {
        Self {
            resource,
            inner: Box::pin(inner),
        }
    }
}

impl<F: Future> Future for ForLoad<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let resource = self.resource;
        let before = LOADING.with(|l| l.replace(Some(resource)));
        let out = self.inner.as_mut().poll(cx);
        LOADING.with(|l| l.set(before));
        out
    }
}

/// Where each load in flight stands in the budget's priority order, as of
/// its latest step: 0 first. Shared between the session that ranks and a
/// pool that orders the work those loads wait on — a decode pool, which
/// would otherwise take its jobs in the order they came, and keep a load the
/// camera has left ahead of the one it now looks at.
#[derive(Clone, Debug, Default)]
pub struct LoadRanks(Arc<Mutex<HashMap<ResourceId, u32>>>);

impl LoadRanks {
    /// Where `resource`'s load stands, if it is in flight and ranked.
    pub fn rank(&self, resource: ResourceId) -> Option<u32> {
        self.0.lock().ok()?.get(&resource).copied()
    }

    /// Replaces the ranks: the loads in flight, in priority order.
    pub fn publish(&self, in_order: impl IntoIterator<Item = ResourceId>) {
        if let Ok(mut ranks) = self.0.lock() {
            ranks.clear();
            ranks.extend(in_order.into_iter().zip(0u32..));
        }
    }
}

/// A unit of blocking work. Owns everything it touches, so it can cross to
/// whatever thread the host chooses.
pub type Job = Box<dyn FnOnce() + Send + 'static>;

/// Accepts blocking work and runs it somewhere. Implementors decide where.
///
/// `submit` returns immediately; the value comes back through [`run`], which
/// is the only way callers are expected to use this.
pub trait Offload: Send + Sync {
    /// Run `job`, eventually, wherever this implementor runs things.
    ///
    /// Must not block the caller for the job's duration — an implementation
    /// that does is [`Inline`], and should say so by being it.
    fn submit(&self, job: Job);

    /// Where [`Portable`] work goes when it has to cross to something that
    /// shares no memory with the caller — a Web Worker, which can be handed
    /// bytes and nothing else.
    ///
    /// `None`, the default, is every native host: a closure crosses to a thread
    /// for free, and turning the job into bytes would only cost a copy.
    /// [`run_portable`] then takes the closure path through [`submit`](Self::submit).
    fn remote(&self) -> Option<&dyn Remote> {
        None
    }
}

/// Work stated as data: a job written to bytes, run wherever the same code is
/// loaded, and its value read back from bytes.
///
/// The form a job has to take to leave a browser's worker. A [`Job`] is a
/// pointer into the caller's linear memory, and a second worker only shares
/// that memory under `SharedArrayBuffer` — atomics, a nightly `build-std`, and
/// COOP/COEP headers on whoever serves the page. Bytes cost a `postMessage`
/// with a transferred buffer, on every browser, behind any server.
///
/// The far side finds the job by [`KIND`](Self::KIND) and runs it with
/// [`serve`]. A job's own failure (a corrupt JPEG) belongs in its `Output`, so
/// it travels as an answer like any other; the transport's `Err` is kept for
/// the transport failing, which [`run_portable`] recovers from by running the
/// job here.
pub trait Portable: Sized + Send + 'static {
    /// The name the far side dispatches on. Unique among a host's jobs.
    const KIND: &'static str;
    /// What the job computes.
    type Output: Send + 'static;

    /// Does the work: blocking, I/O-free, like [`Job`].
    fn run(self) -> Self::Output;
    /// The job as bytes.
    fn encode(&self) -> Vec<u8>;
    /// The job back from [`encode`](Self::encode)'s bytes.
    fn decode(bytes: Vec<u8>) -> Result<Self, String>;
    /// The value as bytes.
    fn encode_output(output: Self::Output) -> Vec<u8>;
    /// The value back from [`encode_output`](Self::encode_output)'s bytes.
    fn decode_output(bytes: Vec<u8>) -> Result<Self::Output, String>;
}

/// Runs a [`Portable`] job from its bytes and answers in bytes: what the far
/// side of a [`Remote`] calls, once it has found the job by its kind.
pub fn serve<P: Portable>(input: Vec<u8>) -> Result<Vec<u8>, String> {
    Ok(P::encode_output(P::decode(input)?.run()))
}

/// Accepts [`Portable`] work as bytes and answers through a [`Reply`].
///
/// Not `Send`: a browser's worker handle is not, and a remote is only ever
/// reached through the [`Offload`] that lends it, on the thread that holds it.
pub trait Remote {
    /// Run the job of this kind from these bytes, somewhere; answer `reply`.
    ///
    /// Must return at once, like [`Offload::submit`].
    fn submit_bytes(&self, kind: &'static str, input: Vec<u8>, reply: Reply);
}

/// Where a [`Remote`] puts its answer: the bytes of the value, or why the
/// transport could not produce them.
pub struct Reply(futures_channel::oneshot::Sender<Result<Vec<u8>, String>>);

impl Reply {
    /// Whether anyone still waits for the answer. A load the camera left is
    /// dropped with its receiver, and a job still queued for it is worth
    /// skipping rather than running for nobody.
    pub fn wanted(&self) -> bool {
        !self.0.is_canceled()
    }

    /// Answers. Silently nothing when the asker has gone.
    pub fn send(self, answer: Result<Vec<u8>, String>) {
        let _ = self.0.send(answer);
    }
}

impl std::fmt::Debug for Reply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reply")
            .field("wanted", &self.wanted())
            .finish()
    }
}

/// Runs the job on the calling thread, before returning.
///
/// The default, and correct wherever there is nowhere else to put the work:
/// `wasm32-unknown-unknown` has no threads to hand it to, and a test that
/// wants a deterministic order does not want any. Behaviour is exactly what
/// the code did before the seam existed.
#[derive(Debug, Clone, Copy, Default)]
pub struct Inline;

impl Offload for Inline {
    fn submit(&self, job: Job) {
        job();
    }
}

/// The offload every caller gets when nothing is chosen.
pub fn inline() -> Arc<dyn Offload> {
    Arc::new(Inline)
}

/// A fixed set of OS threads that decode and resample.
///
/// Std only — `std::thread` and `std::sync::mpsc`, no runtime, no backend, and
/// the whole thing is compiled out on wasm where neither exists. That keeps the
/// no-backend rule intact while sparing every native host from writing the same
/// twenty lines.
///
/// **Fixed, and deliberately small.** The obvious alternative — hand each job to
/// `tokio::task::spawn_blocking` — has a pool of 512, so sixty-four loads in
/// flight become sixty-four threads resampling at once on a machine with eight
/// cores. That is not eight times the throughput; it is eight times the
/// throughput minus what the L2 cache loses to fifty-six other working sets. A
/// count near the core count runs the same work with the cache intact, and the
/// queue behind it costs nothing.
///
/// One core is left for the thread polling the server and the one presenting
/// frames. They are latency-bound and the jobs here are not: a resample that
/// starts a millisecond later is invisible, a frame that does is not.
#[cfg(not(target_arch = "wasm32"))]
pub struct ThreadPool {
    /// `None` only while dropping, which is what tells the workers to stop.
    jobs: std::sync::Mutex<Option<std::sync::mpsc::Sender<Job>>>,
    workers: Vec<std::thread::JoinHandle<()>>,
}

#[cfg(not(target_arch = "wasm32"))]
impl ThreadPool {
    /// Sized from the machine: one thread per core, less one, at least one.
    pub fn sized_to_machine() -> Self {
        Self::with_threads(Self::machine_threads())
    }

    /// The thread count [`sized_to_machine`](Self::sized_to_machine) uses: one
    /// per core, less one, at least one. Public so a host that lets its
    /// operator choose the count (from its own configuration — the core reads
    /// none) keeps this as the default.
    pub fn machine_threads() -> usize {
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        cores.saturating_sub(1).max(1)
    }

    /// `threads` workers, named so they are legible in a profiler — which is
    /// how the imbalance this fixes was found in the first place.
    pub fn with_threads(threads: usize) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        let rx = Arc::new(std::sync::Mutex::new(rx));
        let workers = (0..threads.max(1))
            .map(|i| {
                let rx = Arc::clone(&rx);
                std::thread::Builder::new()
                    .name(format!("tuile-decode-{i}"))
                    .spawn(move || loop {
                        // The lock is held only to take a job, never to run one,
                        // or this pool would be one thread wearing a hat.
                        let job = match rx.lock() {
                            Ok(q) => q.recv(),
                            // A worker panicked mid-job and poisoned the queue.
                            // The rest carry on rather than unwinding in turn.
                            Err(poisoned) => poisoned.into_inner().recv(),
                        };
                        match job {
                            Ok(job) => job(),
                            // Every sender dropped: the pool is going away.
                            Err(_) => break,
                        }
                    })
                    .expect("spawn decode worker")
            })
            .collect();
        tracing::debug!(threads, "decode pool");
        Self {
            jobs: std::sync::Mutex::new(Some(tx)),
            workers,
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Offload for ThreadPool {
    fn submit(&self, job: Job) {
        let sent = match self.jobs.lock() {
            Ok(tx) => tx.as_ref().map(|tx| tx.send(job)),
            Err(poisoned) => poisoned.into_inner().as_ref().map(|tx| tx.send(job)),
        };
        // Submitted during shutdown. The caller's `run` will see its channel
        // dropped, which is the same thing a cancelled load sees.
        if !matches!(sent, Some(Ok(()))) {
            tracing::debug!("decode pool is closed; job dropped");
        }
    }
}

/// Closes the queue and waits, so a job in flight finishes rather than being
/// torn down with the process. Cheap: the queue is drained, not abandoned.
#[cfg(not(target_arch = "wasm32"))]
impl Drop for ThreadPool {
    fn drop(&mut self) {
        if let Ok(mut tx) = self.jobs.lock() {
            tx.take();
        }
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
    }
}

/// A pool sized to this machine, ready to hand to a loader.
#[cfg(not(target_arch = "wasm32"))]
pub fn threaded() -> Arc<dyn Offload> {
    Arc::new(ThreadPool::sized_to_machine())
}

/// Runs `job` on `off` and awaits its value.
///
/// This is the whole ergonomic point: a caller writes `run(&off, move || …)`
/// where it wrote the body inline, and the `.await` is the only visible
/// difference. `T` is unconstrained because the closure carries its own
/// channel — no boxing of the result, no `Any`, no downcast.
///
/// # Panics
///
/// If the host drops the job without running it. That is a broken [`Offload`],
/// not a condition to recover from: the load would otherwise hang for ever,
/// holding a slot that nothing will ever free.
pub async fn run<T, F>(off: &dyn Offload, job: F) -> T
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = futures_channel::oneshot::channel();
    off.submit(Box::new(move || {
        // The receiver is gone when the load was cancelled mid-flight — an
        // ordinary event (the camera moved), and nothing to report.
        let _ = tx.send(job());
    }));
    rx.await
        .expect("the offload dropped a job without running it")
}

/// Runs a [`Portable`] job on `off` and awaits its value.
///
/// Through `off`'s [`Remote`] as bytes when it has one, and otherwise exactly
/// as [`run`] — the closure, no serialisation — which is every native host and
/// [`Inline`]. A remote that fails (its worker died, its answer is unreadable)
/// costs the job a second run, here, through [`run`]: slower, never a missing
/// tile. The job's own failure is not a transport failure and is not retried;
/// it is in the `Output`.
pub async fn run_portable<P: Portable>(off: &dyn Offload, job: P) -> P::Output {
    // The remote is let go before the await: it is not `Send`, and a native
    // load future that held it would stop being so.
    let asked = off.remote().map(|remote| {
        let (tx, rx) = futures_channel::oneshot::channel();
        remote.submit_bytes(P::KIND, job.encode(), Reply(tx));
        rx
    });
    if let Some(rx) = asked {
        match rx.await {
            Ok(Ok(bytes)) => match P::decode_output(bytes) {
                Ok(output) => return output,
                Err(why) => tracing::warn!(kind = P::KIND, "unreadable answer, run here: {why}"),
            },
            Ok(Err(why)) => tracing::warn!(kind = P::KIND, "the remote failed, run here: {why}"),
            Err(futures_channel::oneshot::Canceled) => {
                tracing::warn!(kind = P::KIND, "the remote dropped the job, run here");
            }
        }
    }
    run(off, move || job.run()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn inline_runs_the_job_and_returns_its_value() {
        let answer = futures_executor::block_on(run(&Inline, || 6 * 7));
        assert_eq!(answer, 42);
    }

    /// The point of the trait: work reaches the host's pool rather than the
    /// caller's thread. Recorded here by a thread id, which is the only thing
    /// that actually distinguishes the two.
    #[test]
    fn a_host_offload_runs_the_job_off_the_calling_thread() {
        struct OnItsOwnThread;
        impl Offload for OnItsOwnThread {
            fn submit(&self, job: Job) {
                std::thread::spawn(job);
            }
        }
        let here = std::thread::current().id();
        let there =
            futures_executor::block_on(run(&OnItsOwnThread, move || std::thread::current().id()));
        assert_ne!(there, here, "the job ran on the calling thread");
    }

    /// Two jobs submitted before either is awaited must both be accepted:
    /// `submit` returning only after the work is done would serialise the very
    /// loads this exists to overlap.
    #[test]
    fn submit_does_not_wait_for_the_job() {
        struct Collect(std::sync::Mutex<Vec<Job>>);
        impl Offload for Collect {
            fn submit(&self, job: Job) {
                self.0.lock().expect("jobs").push(job);
            }
        }
        static RAN: AtomicU32 = AtomicU32::new(0);
        let off = Collect(std::sync::Mutex::new(Vec::new()));
        off.submit(Box::new(|| {
            RAN.fetch_add(1, Ordering::SeqCst);
        }));
        off.submit(Box::new(|| {
            RAN.fetch_add(1, Ordering::SeqCst);
        }));
        assert_eq!(RAN.load(Ordering::SeqCst), 0, "submit ran the job itself");
        assert_eq!(off.0.lock().expect("jobs").len(), 2);
    }

    /// The whole point, stated as a measurement rather than a hope: watch how
    /// many jobs are inside the pool at once and require it to exceed one.
    ///
    /// Deliberately *not* written with a `Barrier`, which is the obvious way
    /// and the wrong one: a barrier of four under a serial pool blocks the
    /// first job for ever, so the regression this guards would hang the suite
    /// instead of failing it — and a hang is exactly what cannot be told apart
    /// from a slow machine. Overlap is observed instead, so [`Inline`] scores a
    /// peak of one and fails on the assertion, in bounded time.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn a_pool_runs_jobs_at_the_same_time() {
        fn peak_overlap(off: &dyn Offload) -> u32 {
            let now = Arc::new(AtomicU32::new(0));
            let peak = Arc::new(AtomicU32::new(0));
            let (tx, rx) = std::sync::mpsc::channel();
            for _ in 0..4 {
                let (now, peak, tx) = (Arc::clone(&now), Arc::clone(&peak), tx.clone());
                off.submit(Box::new(move || {
                    let inside = now.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(inside, Ordering::SeqCst);
                    // Long enough that four threads overlap, short enough that
                    // the serial case still answers promptly.
                    std::thread::sleep(std::time::Duration::from_millis(40));
                    now.fetch_sub(1, Ordering::SeqCst);
                    let _ = tx.send(());
                }));
            }
            drop(tx);
            while rx.recv().is_ok() {}
            peak.load(Ordering::SeqCst)
        }

        let pooled = peak_overlap(&ThreadPool::with_threads(4));
        assert!(pooled > 1, "the pool ran {pooled} job(s) at a time");

        // The behaviour being replaced, measured by the same ruler, so the
        // number above has something to be better than.
        assert_eq!(peak_overlap(&Inline), 1, "Inline overlapped");
    }

    /// A pool with fewer threads than jobs must still finish all of them: the
    /// queue is what makes a small pool safe to use with sixty-four loads in
    /// flight.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn a_small_pool_still_finishes_every_job() {
        let pool = ThreadPool::with_threads(2);
        let (tx, rx) = std::sync::mpsc::channel();
        for i in 0..32 {
            let tx = tx.clone();
            pool.submit(Box::new(move || {
                let _ = tx.send(i);
            }));
        }
        drop(tx);
        let mut seen: Vec<u32> = rx.iter().collect();
        seen.sort_unstable();
        assert_eq!(seen, (0..32).collect::<Vec<_>>());
    }

    /// Dropping the pool waits for what it accepted. A job torn down halfway
    /// would leave the caller's `run` awaiting a value that never comes.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn dropping_the_pool_lets_running_jobs_finish() {
        static DONE: AtomicU32 = AtomicU32::new(0);
        let pool = ThreadPool::with_threads(2);
        for _ in 0..8 {
            pool.submit(Box::new(|| {
                std::thread::sleep(std::time::Duration::from_millis(5));
                DONE.fetch_add(1, Ordering::SeqCst);
            }));
        }
        drop(pool);
        assert_eq!(DONE.load(Ordering::SeqCst), 8, "jobs were torn down");
    }

    /// A job small enough to check by hand: the sum of its bytes. Counts its
    /// encodings, so a test can tell the bytes path from the closure path.
    struct Sum(Vec<u8>);

    static ENCODED: AtomicU32 = AtomicU32::new(0);

    impl Portable for Sum {
        const KIND: &'static str = "test.sum";
        type Output = u64;
        fn run(self) -> u64 {
            self.0.iter().map(|b| u64::from(*b)).sum()
        }
        fn encode(&self) -> Vec<u8> {
            ENCODED.fetch_add(1, Ordering::SeqCst);
            self.0.clone()
        }
        fn decode(bytes: Vec<u8>) -> Result<Self, String> {
            Ok(Self(bytes))
        }
        fn encode_output(output: u64) -> Vec<u8> {
            output.to_le_bytes().to_vec()
        }
        fn decode_output(bytes: Vec<u8>) -> Result<u64, String> {
            let bytes: [u8; 8] = bytes.try_into().map_err(|_| "not eight bytes".to_owned())?;
            Ok(u64::from_le_bytes(bytes))
        }
    }

    /// An offload whose remote is a closure over what it was handed.
    struct WithRemote<F: Fn(&'static str, Vec<u8>, Reply) + Send + Sync>(F);

    impl<F: Fn(&'static str, Vec<u8>, Reply) + Send + Sync> Offload for WithRemote<F> {
        fn submit(&self, job: Job) {
            job();
        }
        fn remote(&self) -> Option<&dyn Remote> {
            Some(self)
        }
    }

    impl<F: Fn(&'static str, Vec<u8>, Reply) + Send + Sync> Remote for WithRemote<F> {
        fn submit_bytes(&self, kind: &'static str, input: Vec<u8>, reply: Reply) {
            (self.0)(kind, input, reply);
        }
    }

    /// The bytes path: the job leaves as bytes, is served by its kind on
    /// another thread, and its value comes back read from bytes.
    #[test]
    fn a_portable_job_crosses_as_bytes_and_comes_back_as_its_value() {
        let served = Arc::new(AtomicU32::new(0));
        let counted = Arc::clone(&served);
        let off = WithRemote(move |kind: &'static str, input: Vec<u8>, reply: Reply| {
            assert_eq!(kind, Sum::KIND);
            counted.fetch_add(1, Ordering::SeqCst);
            std::thread::spawn(move || reply.send(serve::<Sum>(input)));
        });
        let sum = futures_executor::block_on(run_portable(&off, Sum(vec![1, 2, 3, 250])));
        assert_eq!(sum, 256);
        assert_eq!(served.load(Ordering::SeqCst), 1, "the remote was not used");
    }

    /// No remote, no bytes: a native host pays nothing for the seam.
    #[test]
    fn without_a_remote_the_job_runs_as_a_closure_and_is_never_encoded() {
        let before = ENCODED.load(Ordering::SeqCst);
        let sum = futures_executor::block_on(run_portable(&Inline, Sum(vec![7; 6])));
        assert_eq!(sum, 42);
        assert_eq!(
            ENCODED.load(Ordering::SeqCst),
            before,
            "the job was encoded"
        );
    }

    /// A remote that fails, answers garbage or drops the job costs a second
    /// run here — never a value missing.
    #[test]
    fn a_failing_remote_falls_back_to_running_the_job_here() {
        let failing = WithRemote(|_: &'static str, _: Vec<u8>, reply: Reply| {
            reply.send(Err("the worker died".into()));
        });
        assert_eq!(
            futures_executor::block_on(run_portable(&failing, Sum(vec![5, 5]))),
            10
        );
        let garbled = WithRemote(|_: &'static str, _: Vec<u8>, reply: Reply| {
            reply.send(Ok(vec![1, 2, 3]));
        });
        assert_eq!(
            futures_executor::block_on(run_portable(&garbled, Sum(vec![5, 6]))),
            11
        );
        let dropping = WithRemote(|_: &'static str, _: Vec<u8>, reply: Reply| drop(reply));
        assert_eq!(
            futures_executor::block_on(run_portable(&dropping, Sum(vec![5, 7]))),
            12
        );
    }

    /// A reply knows when its asker has gone, which is what lets a queue skip
    /// work nobody waits for.
    #[test]
    fn a_reply_knows_when_nobody_waits() {
        let (tx, rx) = futures_channel::oneshot::channel();
        let reply = Reply(tx);
        assert!(reply.wanted());
        drop(rx);
        assert!(!reply.wanted());
        reply.send(Ok(Vec::new()));
    }

    /// **A load names itself while it is polled, and only then**: what a
    /// pool reads to know which load a job is for.
    #[test]
    fn a_load_names_itself_while_it_is_polled() {
        let r = ResourceId::Mesh(crate::source::TileId(7));
        assert_eq!(loading(), None);
        let seen = futures_executor::block_on(ForLoad::new(r, async { loading() }));
        assert_eq!(seen, Some(r));
        assert_eq!(loading(), None, "nothing named once the poll returns");
    }

    /// The ranks are the order they were published in, and only those.
    #[test]
    fn ranks_follow_the_published_order() {
        let ranks = LoadRanks::default();
        let (a, b) = (
            ResourceId::Mesh(crate::source::TileId(1)),
            ResourceId::Mesh(crate::source::TileId(2)),
        );
        ranks.publish([b, a]);
        assert_eq!((ranks.rank(b), ranks.rank(a)), (Some(0), Some(1)));
        ranks.publish([a]);
        assert_eq!((ranks.rank(a), ranks.rank(b)), (Some(0), None));
    }

    /// Native loaders are `Send` futures: the seam must not make them less.
    #[test]
    fn a_portable_run_is_a_send_future() {
        fn send<T: Send>(_: &T) {}
        let future = run_portable(&Inline, Sum(vec![1]));
        send(&future);
        assert_eq!(futures_executor::block_on(future), 1);
    }
}
