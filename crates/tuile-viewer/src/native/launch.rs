// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The desktop's and the phone's session: the geometry server on a thread of
//! its own, blocking on the async runtime, its loads behind the tile store on
//! disk.
//!
//! The whole sequence is one task on the runtime, started by [`start`]: the
//! host's credentials and sources, the store, the globe, the server. A
//! desktop waits for it before opening its window ([`Launch::wait`]); a phone
//! opens at once and collects it from its loop.
//!
//! [`Launch::wait`]: crate::launch::Launch::wait

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

use tuile_core::protocol::InProcessStream;
use tuile_core::source::TileLoader;
use tuile_core::storage::ContentStore;
use tuile_planetary::{ImageryDetail, LayerBudget};
use tuile_storage_foyer::SuspendableStore as FoyerStore;
use tuile_terrain::TerrainHeights;

use crate::embed::{ImageryChoice, TerrainChoice, ViewerHost};
use crate::gate::Gated;
use crate::globe::{Layers, Switched};
use crate::launch::{Launch, Outcome};

/// What the window sets the imagery detail through: the loader's own handle.
pub(crate) type Detail = ImageryDetail;
/// What the window declares the device's imagery slots through.
pub(crate) type Budget = LayerBudget;
/// What the session's loads wait on while the application is away.
pub(crate) type Gate = crate::gate::Gate;

/// A session that is up: its stream, and everything the window adopts from
/// the globe.
pub(crate) struct Started {
    pub stream: InProcessStream,
    /// The thread the geometry server runs on, to be joined on the way out.
    pub server: Option<std::thread::JoinHandle<()>>,
    pub detail: Detail,
    pub heights: Arc<TerrainHeights>,
    pub layer_budget: Budget,
    pub switcher: Switcher,
    /// Kept so it can be flushed on the way out: a store that is merely
    /// dropped leaves the disk as cold as it found it.
    pub store: Option<Arc<FoyerStore>>,
    pub gate: Gate,
    /// The runtime the store is flushed on.
    pub runtime: tokio::runtime::Handle,
}

impl Started {
    /// Closes the session down: the stream, then the store, then the server.
    ///
    /// The store is closed *before* the server is joined: the server thread
    /// blocks on the runtime, and the runtime waits for the store's blocking
    /// workers — so waiting for the thread first and only then asking the
    /// store to close is a deadlock by construction. `join_the_server` is a
    /// desktop's: a phone on its way out is given seconds for all of it, and
    /// does not wait for the server.
    pub(crate) fn close(self, patience: std::time::Duration, join_the_server: bool) {
        let Started {
            stream,
            server,
            store,
            runtime,
            ..
        } = self;
        drop(stream);
        crate::native::store::close(store, &runtime, patience);
        if join_the_server {
            if let Some(server) = server {
                if server.join().is_err() {
                    tracing::error!("the geometry server panicked; its last work is lost");
                }
            }
        }
    }

    /// Writes the tile store down and closes it, off the window's thread.
    pub(crate) fn suspend_store(&self) {
        if let Some(store) = self.store.clone() {
            self.runtime.spawn(async move {
                match store.suspend().await {
                    Ok(()) => tracing::info!("tile store written down and closed"),
                    Err(e) => tracing::warn!("the tile store did not close ({e})"),
                }
            });
        }
    }

    /// Opens it again.
    pub(crate) fn resume_store(&self) {
        if let Some(store) = self.store.clone() {
            self.runtime.spawn(async move {
                match store.resume().await {
                    Ok(()) => tracing::info!("tile store open again"),
                    Err(e) => tracing::warn!("the tile store did not open again ({e})"),
                }
            });
        }
    }
}

/// Changes the imagery a running globe drapes.
///
/// Asking is one call that returns at once: resolving a layer is a round trip
/// or two to the host's service, and the window must not wait on it. The
/// resolution runs on the session's async runtime and the answer is collected
/// by the render loop ([`Switcher::poll`]) — the same shape as the location
/// request, for the same reason.
pub(crate) struct Switcher {
    layers: Layers,
    runtime: tokio::runtime::Handle,
    done: (Sender<Switched>, Receiver<Switched>),
}

impl Switcher {
    /// Starts the switch to `layers[layer]`.
    pub(crate) fn ask(&self, layer: usize, choice: &ImageryChoice) {
        let asked = web_time::Instant::now();
        let (layers, done, choice) = (self.layers.clone(), self.done.0.clone(), choice.clone());
        self.runtime.spawn(async move {
            let outcome = layers.switch_to(layer, &choice).await;
            // A closed channel means the window is gone, and with it the
            // point of saying how it went.
            let _ = done.send(Switched {
                layer,
                outcome,
                asked,
            });
        });
    }

    /// The outcome of a switch that has finished resolving, if one has.
    pub(crate) fn poll(&self) -> Option<Switched> {
        self.done.1.try_recv().ok()
    }
}

/// What the launch needs to know, all of it known before the window exists.
#[derive(Clone)]
pub(crate) struct Plan {
    pub host: Arc<dyn ViewerHost>,
    pub terrain: TerrainChoice,
    pub layers: &'static [ImageryChoice],
    pub opening: usize,
    /// Where the tile store lives on disk.
    pub store: std::path::PathBuf,
    pub config: tuile_core::traversal::Config,
    pub profile: crate::profile::Profile,
    /// See [`crate::launch::START_TIMEOUT`].
    pub timeout: std::time::Duration,
}

/// Starts the launch on `runtime` and returns at once.
pub(crate) fn start(runtime: &tokio::runtime::Handle, plan: Plan) -> Launch {
    let (tx, launch) = Launch::pending();
    let server_runtime = runtime.clone();
    runtime.spawn(async move {
        let outcome = bring_up(plan, server_runtime).await;
        // The window is gone; so is the reader.
        let _ = tx.send(outcome);
    });
    launch
}

async fn bring_up(plan: Plan, runtime: tokio::runtime::Handle) -> Outcome {
    let timeout = plan.timeout;
    tokio::time::timeout(timeout, bring_up_now(plan, runtime))
        .await
        .unwrap_or_else(|_| {
            Err(format!(
                "the tile service did not answer within {} s",
                timeout.as_secs()
            ))
        })
}

async fn bring_up_now(plan: Plan, runtime: tokio::runtime::Handle) -> Outcome {
    let sources = plan.host.connect().await?;
    let store = crate::native::store::open_at(
        &plan.store,
        plan.profile.store_memory_bytes,
        plan.profile.store_disk_bytes,
        plan.profile.clear_damaged_store,
    )
    .await;
    // Decoding and resampling go to their own threads. Left on the server's
    // thread they run one at a time no matter how many loads are in flight —
    // measured at sixty-four outstanding and a single core busy.
    // As many as the profile allows: every core on a desktop, a few on a
    // phone, where the window and the runtime need theirs.
    let decode = Arc::new(tuile_core::offload::ThreadPool::with_threads(
        plan.profile.decode_threads,
    ));
    let globe = crate::globe::assemble(
        sources,
        store.clone().map(|s| s as Arc<dyn ContentStore>),
        &plan.terrain,
        plan.layers,
        plan.opening,
        decode,
    )
    .await
    .map_err(|e| format!("{e:#}"))?;
    let gate = Gate::default();
    let loader: Arc<dyn TileLoader> = Arc::new(Gated {
        inner: globe.loader,
        gate: gate.clone(),
    });
    let (stream, server) = tuile_core::runtime::in_process_with(globe.tree, loader, plan.config);
    // Every load a task on the runtime's workers, so tiles keep arriving
    // while the server traverses.
    let server = server.with_spawn(tuile_core::runtime::Spawn::Tokio(runtime.clone()));
    // Its own thread, blocking on the runtime: the server's session is one
    // long future that the window talks to over channels.
    let server_runtime = runtime.clone();
    let server = std::thread::Builder::new()
        .name("geometry-server".into())
        .spawn(move || server_runtime.block_on(server.run()))
        .map_err(|e| format!("the geometry server could not start: {e}"))?;
    Ok(Started {
        stream,
        server: Some(server),
        detail: globe.detail,
        heights: globe.heights,
        layer_budget: globe.budget,
        switcher: Switcher {
            layers: globe.layers,
            runtime: runtime.clone(),
            done: channel(),
        },
        store,
        gate,
        runtime,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use web_time::Instant;

    /// A host whose service never answers — the network a phone has in a
    /// lift, or a captive portal.
    struct Stalled;

    #[async_trait::async_trait]
    impl ViewerHost for Stalled {
        fn identity(&self) -> crate::embed::Identity {
            crate::embed::Identity::default()
        }
        fn terrain(&self) -> TerrainChoice {
            TerrainChoice {
                asset: 1,
                cache: "t".into(),
            }
        }
        fn imagery(&self) -> Vec<ImageryChoice> {
            vec![ImageryChoice::new("a", "A", 2, "©")]
        }
        async fn connect(&self) -> Result<Arc<dyn crate::embed::Sources>, String> {
            std::future::pending().await
        }
    }

    /// One that refuses at once.
    struct Refusing;

    #[async_trait::async_trait]
    impl ViewerHost for Refusing {
        fn identity(&self) -> crate::embed::Identity {
            crate::embed::Identity::default()
        }
        fn terrain(&self) -> TerrainChoice {
            Stalled.terrain()
        }
        fn imagery(&self) -> Vec<ImageryChoice> {
            Stalled.imagery()
        }
        async fn connect(&self) -> Result<Arc<dyn crate::embed::Sources>, String> {
            Err("there is no key".into())
        }
    }

    fn plan(host: Arc<dyn ViewerHost>) -> Plan {
        static LAYERS: std::sync::OnceLock<Vec<ImageryChoice>> = std::sync::OnceLock::new();
        Plan {
            host,
            terrain: Stalled.terrain(),
            layers: LAYERS.get_or_init(|| Stalled.imagery()),
            opening: 0,
            store: std::env::temp_dir().join("launch-test"),
            config: tuile_core::traversal::Config::default(),
            profile: crate::profile::Profile::desktop(),
            timeout: crate::launch::START_TIMEOUT,
        }
    }

    /// The window's side of a launch that never finishes: every poll returns
    /// at once, with nothing, for as long as anyone asks. This is the
    /// property the phone's watchdog holds the application to — the main
    /// thread is never waiting on the network.
    #[test]
    fn polling_a_stalled_launch_never_waits() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("a runtime");
        let mut launch = start(runtime.handle(), plan(Arc::new(Stalled)));
        let began = Instant::now();
        let mut slowest = Duration::ZERO;
        while began.elapsed() < Duration::from_millis(500) {
            let t = Instant::now();
            assert!(launch.poll().is_none(), "a stalled launch answered");
            slowest = slowest.max(t.elapsed());
            std::thread::sleep(Duration::from_millis(16));
        }
        // A liveness bound, not a speed one: a poll that waited on the
        // network would wait the whole start-up; a slow runner does not.
        assert!(
            slowest < Duration::from_millis(250),
            "a poll took {slowest:?}"
        );
        runtime.shutdown_background();
    }

    /// A network that never answers is given up on after the timeout, with a
    /// sentence that says so — not a spinner turning for ever.
    #[test]
    fn a_stalled_launch_times_out() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("a runtime");
        let mut stalled = plan(Arc::new(Stalled));
        stalled.timeout = Duration::from_millis(100);
        let outcome = start(runtime.handle(), stalled).wait();
        assert!(
            outcome
                .err()
                .is_some_and(|why| why.contains("did not answer")),
            "a stalled start-up was not called off"
        );
        runtime.shutdown_background();
    }

    /// A refusal arrives as the host's own sentence, on a later poll.
    #[test]
    fn a_refused_launch_says_why() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("a runtime");
        let mut launch = start(runtime.handle(), plan(Arc::new(Refusing)));
        let deadline = Instant::now() + Duration::from_secs(5);
        let outcome = loop {
            if let Some(outcome) = launch.poll() {
                break outcome;
            }
            assert!(Instant::now() < deadline, "no answer");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(outcome.err().as_deref(), Some("there is no key"));
        assert_eq!(
            start(runtime.handle(), plan(Arc::new(Refusing)))
                .wait()
                .err()
                .as_deref(),
            Some("there is no key")
        );
    }
}
