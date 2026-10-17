// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Bringing the session up without holding the window's thread.
//!
//! Everything between "the application started" and "the geometry server is
//! running" is network: the host's credentials and service, the terrain's
//! layer description, the imagery's metadata, the tile store opening on disk.
//! Seconds on a good connection, and unbounded on a bad one.
//!
//! A desktop can afford to wait for it before opening the window. A phone
//! cannot: its system ends an application whose main thread does not return
//! from launch promptly, or stops answering for long, and a person looking at
//! a frozen launch screen draws the same conclusion. So the whole sequence is
//! one task on the session's async runtime, started by [`Launch::start`], and
//! the window collects the outcome with [`Launch::poll`] — a `try_recv`, which
//! never waits — on whichever turn of its loop it arrives, drawing the globe's
//! shell and the loading indicator meanwhile. The desktop takes the same task
//! and [`Launch::wait`]s for it, which is what it always did.

use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::Arc;

use tuile_core::protocol::InProcessStream;
use tuile_core::source::TileLoader;
use tuile_planetary::{ImageryDetail, LayerBudget};
use tuile_storage_foyer::FoyerStore;
use tuile_terrain::TerrainHeights;

use crate::embed::{ImageryChoice, TerrainChoice, ViewerHost};
use crate::gate::{Gate, Gated};
use crate::globe::Switcher;

/// A session that is up: its stream, and everything the window adopts from
/// the globe.
pub(crate) struct Started {
    pub stream: InProcessStream,
    /// The thread the geometry server runs on, to be joined on the way out.
    pub server: Option<std::thread::JoinHandle<()>>,
    pub detail: ImageryDetail,
    pub heights: Arc<TerrainHeights>,
    pub layer_budget: LayerBudget,
    pub switcher: Switcher,
    pub store: Option<Arc<FoyerStore>>,
    /// What the server's loads wait on while the application is away.
    pub gate: Gate,
}

/// What the launch needs to know, all of it known before the window exists.
pub(crate) struct Plan {
    pub host: Arc<dyn ViewerHost>,
    pub terrain: TerrainChoice,
    pub layers: &'static [ImageryChoice],
    pub opening: usize,
    pub store: String,
    pub config: tuile_core::traversal::Config,
    pub profile: crate::profile::Profile,
}

/// A launch under way, or over.
pub(crate) struct Launch {
    outcome: Receiver<Result<Started, String>>,
}

impl Launch {
    /// Starts the launch on `runtime` and returns at once.
    pub(crate) fn start(runtime: &tokio::runtime::Handle, plan: Plan) -> Self {
        let (tx, rx) = channel();
        let server_runtime = runtime.clone();
        runtime.spawn(async move {
            let outcome = bring_up(plan, server_runtime).await;
            // The window is gone; so is the reader.
            let _ = tx.send(outcome);
        });
        Self { outcome: rx }
    }

    /// A launch whose outcome is already known.
    pub(crate) fn finished(outcome: Result<Started, String>) -> Self {
        let (tx, rx) = channel();
        let _ = tx.send(outcome);
        Self { outcome: rx }
    }

    /// The outcome, if it has arrived. Never waits.
    pub(crate) fn poll(&mut self) -> Option<Result<Started, String>> {
        match self.outcome.try_recv() {
            Ok(outcome) => Some(outcome),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err(
                "the session's start-up stopped without an answer".into(),
            )),
        }
    }

    /// Waits for the outcome. For a desktop, before its window opens.
    pub(crate) fn wait(self) -> Result<Started, String> {
        self.outcome
            .recv()
            .unwrap_or_else(|_| Err("the session's start-up stopped without an answer".into()))
    }
}

async fn bring_up(plan: Plan, runtime: tokio::runtime::Handle) -> Result<Started, String> {
    let sources = plan.host.connect().await?;
    let store = crate::globe::open_the_store(
        &plan.store,
        plan.profile.store_memory_bytes,
        plan.profile.store_disk_bytes,
    )
    .await;
    let globe = crate::globe::assemble(
        sources,
        store,
        &plan.terrain,
        plan.layers,
        plan.opening,
        plan.profile.decode_threads,
    )
    .await
    .map_err(|e| format!("{e:#}"))?;
    let gate = Gate::default();
    let loader: Arc<dyn TileLoader> = Arc::new(Gated {
        inner: globe.loader,
        gate: gate.clone(),
    });
    let (stream, server) = tuile_core::runtime::in_process_with(globe.tree, loader, plan.config);
    // Its own thread, blocking on the runtime: the server's session is one
    // long future that the window talks to over channels.
    let server = std::thread::Builder::new()
        .name("geometry-server".into())
        .spawn(move || runtime.block_on(server.run()))
        .map_err(|e| format!("the geometry server could not start: {e}"))?;
    Ok(Started {
        stream,
        server: Some(server),
        detail: globe.detail,
        heights: globe.heights,
        layer_budget: globe.budget,
        switcher: globe.switcher,
        store: globe.store,
        gate,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

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
            store: "launch-test".into(),
            config: tuile_core::traversal::Config::default(),
            profile: crate::profile::Profile::desktop(),
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
        let mut launch = Launch::start(runtime.handle(), plan(Arc::new(Stalled)));
        let began = Instant::now();
        let mut slowest = Duration::ZERO;
        while began.elapsed() < Duration::from_millis(500) {
            let t = Instant::now();
            assert!(launch.poll().is_none(), "a stalled launch answered");
            slowest = slowest.max(t.elapsed());
            std::thread::sleep(Duration::from_millis(16));
        }
        assert!(
            slowest < Duration::from_millis(5),
            "a poll took {slowest:?}"
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
        let mut launch = Launch::start(runtime.handle(), plan(Arc::new(Refusing)));
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
            Launch::start(runtime.handle(), plan(Arc::new(Refusing)))
                .wait()
                .err()
                .as_deref(),
            Some("there is no key")
        );
    }
}
