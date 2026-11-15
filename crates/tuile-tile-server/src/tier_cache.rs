// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What a process keeps of the archives it reads through its tiers, and how
//! hard it may ask each tier at once.
//!
//! A store's archives are immutable ([`crate::tiered::is_archive_key`]): a
//! byte read once is the same byte for as long as the key exists, so what
//! is read is kept with nothing ever to invalidate, bounded by memory only.
//! It is kept in two classes:
//!
//! - **Directories** — an archive's header, its root directory, its
//!   metadata and its leaf directories, the bytes every reader of the
//!   archive walks before any tile — are kept for the process's lifetime,
//!   in a byte budget of their own ([`TierCacheConfig::directory_bytes`]).
//! - **Tile data** is read in aligned blocks of
//!   [`TierCacheConfig::block_bytes`], each kept
//!   [`TierCacheConfig::data_ttl`] in a byte budget of its own, the least
//!   recently used block evicted first.
//!
//! Each class is a [`moka`] cache, whose loads are single flight: however
//! many readers want one block at once, it is asked for once, and ranges of
//! one archive that fall in the same block share it.
//!
//! Every request a [`crate::Tiered`] makes of a tier — or of its origin —
//! holds one of that tier's permits ([`TierCacheConfig::in_flight`],
//! [`TierCacheConfig::origin_in_flight`] for the origin) for as long as it
//! runs: a reader beyond the cap waits for a permit rather than opening one
//! more connection that the others' bandwidth would time out. A request is
//! given a time scaled to the bytes it asks for
//! ([`TierCacheConfig::deadline`]), not one fixed time that a large read
//! over a busy link never meets and retries in a loop.
//!
//! One [`TierCache`] is meant per process, shared by every store it opens:
//! its budgets are the process's, and the host counts them in its memory
//! ([`TierCacheConfig::memory_bytes`]).
//!
//! | Variable | Default | What |
//! |---|---|---|
//! | `TUILE_TILES_BLOCK_KIB` | 512 | tile data read in blocks of this size |
//! | `TUILE_TILES_DATA_CACHE_MIB` | 1024 | tile data blocks kept, at most |
//! | `TUILE_TILES_DATA_TTL_S` | 1800 | a block kept this long after it is read in |
//! | `TUILE_TILES_DIRECTORY_CACHE_MIB` | 256 | directories kept, at most, for the process |
//! | `TUILE_TILES_IN_FLIGHT` | 32 | requests to one tier at once |
//! | `TUILE_TILES_ORIGIN_IN_FLIGHT` | 8 | requests to the origin at once |
//! | `TUILE_TILES_REQUEST_TIMEOUT_S` | 10 | a request's time, before its bytes |
//! | `TUILE_TILES_MIN_KIB_PER_S` | 2048 | the slowest a request's bytes may come |
//! | `TUILE_TILES_ATTEMPTS` | 3 | attempts at an origin read, each given more time |
//! | `TUILE_TILES_CLIENT_RETRIES` | 3 | retries an object-store client makes of one request |

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const KIB: u64 = 1 << 10;
const MIB: u64 = 1 << 20;

/// Default for [`TierCacheConfig::block_bytes`].
pub const DEFAULT_BLOCK_BYTES: u64 = 512 * KIB;
/// Default for [`TierCacheConfig::data_bytes`].
pub const DEFAULT_DATA_CACHE_BYTES: u64 = 1024 * MIB;
/// Default for [`TierCacheConfig::data_ttl`].
pub const DEFAULT_DATA_TTL: Duration = Duration::from_secs(30 * 60);
/// Default for [`TierCacheConfig::directory_bytes`].
pub const DEFAULT_DIRECTORY_CACHE_BYTES: u64 = 256 * MIB;
/// Default for [`TierCacheConfig::in_flight`].
pub const DEFAULT_IN_FLIGHT: usize = 32;
/// Default for [`TierCacheConfig::origin_in_flight`].
pub const DEFAULT_ORIGIN_IN_FLIGHT: usize = 8;
/// Default for [`TierCacheConfig::request_timeout`].
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Default for [`TierCacheConfig::min_bytes_per_s`].
pub const DEFAULT_MIN_BYTES_PER_S: u64 = 2 * MIB;
/// Default for [`TierCacheConfig::attempts`].
pub const DEFAULT_ATTEMPTS: u32 = 3;
/// Default for [`TierCacheConfig::client_retries`].
pub const DEFAULT_CLIENT_RETRIES: usize = 3;

/// The name the origin's permits and counters go by.
pub const ORIGIN: &str = "origin";

/// What a process keeps of its archives, and how hard it asks its tiers:
/// see the [module](self).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierCacheConfig {
    /// Tile data is read and kept in aligned blocks of this many bytes.
    pub block_bytes: u64,
    /// Tile data blocks kept, in bytes at most.
    pub data_bytes: u64,
    /// How long a tile data block is kept after it is read in.
    pub data_ttl: Duration,
    /// Directories kept, in bytes at most, for the process's lifetime.
    pub directory_bytes: u64,
    /// Requests to one tier at once, at most.
    pub in_flight: usize,
    /// Requests to the origin at once, at most.
    pub origin_in_flight: usize,
    /// A request's time before its first byte; its bytes are given more
    /// ([`Self::deadline`]).
    pub request_timeout: Duration,
    /// The slowest a request's bytes may come before it is given up.
    pub min_bytes_per_s: u64,
    /// Attempts at a read of the origin — the last place to read from —
    /// each given one more deadline than the one before.
    pub attempts: u32,
    /// Retries an object-store client of a tier makes of one request
    /// (`object_store`'s `RetryConfig::max_retries`), for a host that builds
    /// the client.
    pub client_retries: usize,
}

impl Default for TierCacheConfig {
    fn default() -> Self {
        Self {
            block_bytes: DEFAULT_BLOCK_BYTES,
            data_bytes: DEFAULT_DATA_CACHE_BYTES,
            data_ttl: DEFAULT_DATA_TTL,
            directory_bytes: DEFAULT_DIRECTORY_CACHE_BYTES,
            in_flight: DEFAULT_IN_FLIGHT,
            origin_in_flight: DEFAULT_ORIGIN_IN_FLIGHT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            min_bytes_per_s: DEFAULT_MIN_BYTES_PER_S,
            attempts: DEFAULT_ATTEMPTS,
            client_retries: DEFAULT_CLIENT_RETRIES,
        }
    }
}

impl TierCacheConfig {
    /// The configuration the environment says: see the [module](self).
    pub fn from_env() -> Self {
        Self::from_vars(|name| std::env::var(name).ok())
    }

    /// The configuration `var` says, a variable it does not say — or says
    /// as no number — at its default.
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Self {
        let number = |name: &str| var(name).and_then(|v| v.trim().parse::<u64>().ok());
        let d = Self::default();
        Self {
            block_bytes: number("TUILE_TILES_BLOCK_KIB")
                .filter(|&k| k > 0)
                .map_or(d.block_bytes, |k| k * KIB),
            data_bytes: number("TUILE_TILES_DATA_CACHE_MIB").map_or(d.data_bytes, |m| m * MIB),
            data_ttl: number("TUILE_TILES_DATA_TTL_S").map_or(d.data_ttl, Duration::from_secs),
            directory_bytes: number("TUILE_TILES_DIRECTORY_CACHE_MIB")
                .map_or(d.directory_bytes, |m| m * MIB),
            in_flight: number("TUILE_TILES_IN_FLIGHT")
                .filter(|&n| n > 0)
                .map_or(d.in_flight, |n| n as usize),
            origin_in_flight: number("TUILE_TILES_ORIGIN_IN_FLIGHT")
                .filter(|&n| n > 0)
                .map_or(d.origin_in_flight, |n| n as usize),
            request_timeout: number("TUILE_TILES_REQUEST_TIMEOUT_S")
                .filter(|&s| s > 0)
                .map_or(d.request_timeout, Duration::from_secs),
            min_bytes_per_s: number("TUILE_TILES_MIN_KIB_PER_S")
                .filter(|&k| k > 0)
                .map_or(d.min_bytes_per_s, |k| k * KIB),
            attempts: number("TUILE_TILES_ATTEMPTS")
                .filter(|&n| n > 0)
                .map_or(d.attempts, |n| n.min(u64::from(u32::MAX)) as u32),
            client_retries: number("TUILE_TILES_CLIENT_RETRIES")
                .map_or(d.client_retries, |n| n as usize),
        }
    }

    /// What the caches may hold at most, in bytes: what a host counts in
    /// the memory it plans for, once per process.
    pub fn memory_bytes(&self) -> u64 {
        self.data_bytes + self.directory_bytes
    }

    /// The time a request for `bytes` is given: [`Self::request_timeout`],
    /// and the time its bytes take at [`Self::min_bytes_per_s`].
    pub fn deadline(&self, bytes: u64) -> Duration {
        self.request_timeout
            + Duration::from_secs_f64(bytes as f64 / self.min_bytes_per_s.max(1) as f64)
    }
}

/// Which cache a read was answered from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CacheClass {
    /// Headers, root, metadata and leaf directories.
    Directory,
    /// Tile data blocks.
    Data,
}

impl CacheClass {
    fn label(self) -> &'static str {
        match self {
            Self::Directory => "directory",
            Self::Data => "data",
        }
    }
}

/// Reads of one cache class since the cache was made.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClassStats {
    /// Answered from what was kept.
    pub hits: u64,
    /// Read from a tier or the origin by this reader.
    pub loads: u64,
    /// Answered by another reader's read of the same piece, in flight.
    pub joined: u64,
    /// Bytes the class holds now.
    pub bytes: u64,
}

/// What the caches and the caps did, since they were made.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TierCacheStats {
    pub directory: ClassStats,
    pub data: ClassStats,
    /// Requests that waited for a permit of their tier.
    pub cap_waits: u64,
    /// Requests given up at their deadline.
    pub timeouts: u64,
}

#[derive(Debug, Default)]
struct ClassCounters {
    hits: AtomicU64,
    loads: AtomicU64,
    joined: AtomicU64,
}

/// A piece an archive keeps in the directory class: its layout (the head,
/// read first), or one of its directory sections.
#[derive(Debug, Clone)]
pub(crate) enum DirPiece {
    Layout(Arc<crate::tiered::Layout>),
    Section(Bytes),
}

impl DirPiece {
    fn weight(&self) -> u64 {
        match self {
            Self::Layout(l) => l.head.len() as u64 + 256,
            Self::Section(b) => b.len() as u64,
        }
    }
}

/// The key a layout is kept under, among an archive's sections.
pub(crate) const LAYOUT: u64 = u64::MAX;

/// What a process keeps of its archives and the permits of its tiers: see
/// the [module](self). Shared by every [`crate::Tiered`] of the process
/// ([`crate::Tiered::with_cache`]).
pub struct TierCache {
    config: TierCacheConfig,
    pub(crate) directories: moka::future::Cache<(String, u64), DirPiece>,
    pub(crate) data: moka::future::Cache<(String, u64), Bytes>,
    caps: Mutex<HashMap<String, Arc<Semaphore>>>,
    directory_counters: ClassCounters,
    data_counters: ClassCounters,
    cap_waits: AtomicU64,
    timeouts: AtomicU64,
}

impl std::fmt::Debug for TierCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TierCache")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl Default for TierCache {
    fn default() -> Self {
        Self::new(TierCacheConfig::default())
    }
}

/// An entry's weight, as a cache counts it: its bytes and its key's.
fn weight(key: &(String, u64), bytes: u64) -> u32 {
    (bytes + key.0.len() as u64 + 64).min(u64::from(u32::MAX)) as u32
}

impl TierCache {
    pub fn new(config: TierCacheConfig) -> Self {
        let directories = moka::future::Cache::builder()
            .max_capacity(config.directory_bytes)
            .weigher(|k: &(String, u64), v: &DirPiece| weight(k, v.weight()))
            .build();
        let data = moka::future::Cache::builder()
            .max_capacity(config.data_bytes)
            .time_to_live(config.data_ttl)
            .weigher(|k: &(String, u64), v: &Bytes| weight(k, v.len() as u64))
            .build();
        Self {
            config,
            directories,
            data,
            caps: Mutex::default(),
            directory_counters: ClassCounters::default(),
            data_counters: ClassCounters::default(),
            cap_waits: AtomicU64::new(0),
            timeouts: AtomicU64::new(0),
        }
    }

    pub fn config(&self) -> &TierCacheConfig {
        &self.config
    }

    pub fn stats(&self) -> TierCacheStats {
        let class = |c: &ClassCounters, bytes: u64| ClassStats {
            hits: c.hits.load(Ordering::Relaxed),
            loads: c.loads.load(Ordering::Relaxed),
            joined: c.joined.load(Ordering::Relaxed),
            bytes,
        };
        TierCacheStats {
            directory: class(&self.directory_counters, self.directories.weighted_size()),
            data: class(&self.data_counters, self.data.weighted_size()),
            cap_waits: self.cap_waits.load(Ordering::Relaxed),
            timeouts: self.timeouts.load(Ordering::Relaxed),
        }
    }

    /// A read of `class` answered: `hit` from what was kept, else `loaded`
    /// by this reader or joined to another's.
    pub(crate) fn counted(&self, class: CacheClass, hit: bool, loaded: bool) {
        let c = match class {
            CacheClass::Directory => &self.directory_counters,
            CacheClass::Data => &self.data_counters,
        };
        let outcome = match (hit, loaded) {
            (true, _) => {
                c.hits.fetch_add(1, Ordering::Relaxed);
                "hit"
            }
            (false, true) => {
                c.loads.fetch_add(1, Ordering::Relaxed);
                "load"
            }
            (false, false) => {
                c.joined.fetch_add(1, Ordering::Relaxed);
                "joined"
            }
        };
        metrics::counter!("tuile_tiles_cache_reads_total", "class" => class.label(), "outcome" => outcome)
            .increment(1);
    }

    /// The bytes each class holds, on the gauges.
    pub(crate) fn gauge_sizes(&self) {
        metrics::gauge!("tuile_tiles_cache_bytes", "class" => CacheClass::Directory.label())
            .set(self.directories.weighted_size() as f64);
        metrics::gauge!("tuile_tiles_cache_bytes", "class" => CacheClass::Data.label())
            .set(self.data.weighted_size() as f64);
    }

    fn cap(&self, tier: &str) -> Arc<Semaphore> {
        let mut caps = self.caps.lock().unwrap_or_else(|p| p.into_inner());
        caps.entry(tier.to_string())
            .or_insert_with(|| {
                Arc::new(Semaphore::new(if tier == ORIGIN {
                    self.config.origin_in_flight
                } else {
                    self.config.in_flight
                }))
            })
            .clone()
    }

    /// One of `tier`'s permits, waited for when they are all out: held, it
    /// counts in the tier's requests in flight.
    pub(crate) async fn permit(&self, tier: &str) -> InFlight {
        let cap = self.cap(tier);
        let permit = match cap.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                self.cap_waits.fetch_add(1, Ordering::Relaxed);
                metrics::counter!("tuile_tiles_tier_cap_waits_total", "tier" => tier.to_string())
                    .increment(1);
                let began = Instant::now();
                let p = cap
                    .acquire_owned()
                    .await
                    .expect("a tier's permits are never closed");
                metrics::histogram!("tuile_tiles_tier_cap_wait_seconds", "tier" => tier.to_string())
                    .record(began.elapsed().as_secs_f64());
                p
            }
        };
        metrics::gauge!("tuile_tiles_tier_in_flight", "tier" => tier.to_string()).increment(1.0);
        metrics::counter!("tuile_tiles_tier_requests_total", "tier" => tier.to_string())
            .increment(1);
        InFlight {
            tier: tier.to_string(),
            _permit: permit,
        }
    }

    /// `request` to `tier`, for `bytes`, under one of its permits and given
    /// its [deadline](TierCacheConfig::deadline).
    pub(crate) async fn request<T>(
        &self,
        tier: &str,
        bytes: u64,
        request: impl Future<Output = object_store::Result<T>>,
    ) -> object_store::Result<T> {
        let _in_flight = self.permit(tier).await;
        let deadline = self.config.deadline(bytes);
        match tokio::time::timeout(deadline, request).await {
            Ok(answer) => answer,
            Err(_) => Err(self.timed_out(tier, deadline)),
        }
    }

    pub(crate) fn timed_out(&self, tier: &str, deadline: Duration) -> object_store::Error {
        self.timeouts.fetch_add(1, Ordering::Relaxed);
        metrics::counter!("tuile_tiles_tier_timeouts_total", "tier" => tier.to_string())
            .increment(1);
        object_store::Error::Generic {
            store: "tiered",
            source: format!("{tier}: no answer within {deadline:?}").into(),
        }
    }

    /// `bytes` read from `tier`, on its counter.
    pub(crate) fn read_bytes(tier: &str, bytes: u64) {
        metrics::counter!("tuile_tiles_tier_bytes_total", "tier" => tier.to_string())
            .increment(bytes);
    }
}

/// A request of a tier in flight: its permit, given back when dropped.
pub(crate) struct InFlight {
    tier: String,
    _permit: OwnedSemaphorePermit,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        metrics::gauge!("tuile_tiles_tier_in_flight", "tier" => self.tier.clone()).decrement(1.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_environment_sets_numbers_and_leaves_the_rest_at_their_defaults() {
        let vars: HashMap<&str, &str> = [
            ("TUILE_TILES_BLOCK_KIB", "256"),
            ("TUILE_TILES_DATA_TTL_S", "60"),
            ("TUILE_TILES_IN_FLIGHT", "4"),
            ("TUILE_TILES_ORIGIN_IN_FLIGHT", "zero"),
            ("TUILE_TILES_DATA_CACHE_MIB", "2048"),
        ]
        .into_iter()
        .collect();
        let c = TierCacheConfig::from_vars(|n| vars.get(n).map(|v| v.to_string()));
        assert_eq!(c.block_bytes, 256 * KIB);
        assert_eq!(c.data_ttl, Duration::from_secs(60));
        assert_eq!(c.in_flight, 4);
        assert_eq!(c.origin_in_flight, DEFAULT_ORIGIN_IN_FLIGHT, "not a number");
        assert_eq!(c.memory_bytes(), 2048 * MIB + DEFAULT_DIRECTORY_CACHE_BYTES);
        assert_eq!(TierCacheConfig::from_vars(|_| None), TierCacheConfig::default());
    }

    #[test]
    fn a_request_is_given_time_for_its_bytes() {
        let c = TierCacheConfig::default();
        assert_eq!(c.deadline(0), DEFAULT_REQUEST_TIMEOUT);
        assert_eq!(
            c.deadline(256 * MIB),
            DEFAULT_REQUEST_TIMEOUT + Duration::from_secs(128)
        );
    }
}
