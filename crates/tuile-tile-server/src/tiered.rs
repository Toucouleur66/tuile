// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Archives downloaded once, then kept in tiers in front of their origin.
//!
//! A store's archives are immutable: each is written once under a key of its
//! own (`<epoch>-<nanos>-<salt>.pmtiles`, [`is_archive_key`]) and never
//! rewritten, so a copy of one is never stale. [`Tiered`] is an object store
//! that answers for its origin and keeps such archives in tiers read before
//! it, nearest first — a disk on this machine, a bucket the machines of a
//! region share, any object store. Reads go tier by tier, then to the
//! origin; what the origin gives is written into every tier that missed it,
//! so the next reader anywhere behind those tiers does not go to the origin.
//!
//! - **Content keyed.** Only archive keys are tiered. A manifest, the
//!   catalog, an extract — keys that are rewritten — always go to the origin.
//! - **Kept in memory.** A range of an archive is read through the process's
//!   [`TierCache`]: its directories kept for the process, its tile data in
//!   aligned blocks kept for a while, each piece read once however many
//!   readers want it at once. A range read again costs no request.
//! - **Capped.** Every request to a tier, and to the origin, holds one of its
//!   permits: a reader past the cap waits, and each request is given a time
//!   scaled to its bytes ([`crate::tier_cache`]).
//! - **Single flight.** However many readers of this process miss the same
//!   archive at once, it is downloaded once.
//! - **Written once.** A tier is written with [`PutMode::Create`]: an archive
//!   another process put there first is left as it is (the same bytes).
//! - **Ranges.** A range of an archive no tier holds brings the whole archive
//!   down, up to [`DEFAULT_WHOLE_BELOW`], and is served from it: the next
//!   range is in a tier. A larger archive is read from the origin by ranges,
//!   as without tiers.
//! - **Never edited.** An object written into a tier is never written
//!   again, nor its metadata: a read is never a write. A tier that forgets
//!   forgets by age — a bucket's lifecycle deleting an object some days
//!   after its creation — and an archive it forgot is read from the origin
//!   by its next reader and written back, its age starting over. Data
//!   updated often does not belong in an object store: should the use of
//!   each archive ever be counted, it is counted in the host's register of
//!   tiles, not in object metadata.
//!
//! Without tiers it is its origin: every call passes through.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures_util::future::{BoxFuture, FutureExt, Shared};
use futures_util::stream::{BoxStream, StreamExt};
use object_store::path::Path;
use object_store::{
    Attributes, CopyOptions, Extensions, GetOptions, GetRange, GetResult, GetResultPayload,
    ListResult, MultipartUpload, ObjectMeta, ObjectStore, ObjectStoreExt, PutMode,
    PutMultipartOptions, PutOptions, PutPayload, PutResult, Result,
};

use crate::tier_cache::{CacheClass, DirPiece, InFlight, TierCache, TierCacheStats, LAYOUT, ORIGIN};

/// Archives up to this size are brought down whole on a range miss.
pub const DEFAULT_WHOLE_BELOW: u64 = 256 << 20;

/// What an archive's first read asks for: its header and, within these
/// bytes, its root directory (the specification's own bound).
pub const HEAD_BYTES: u64 = 16_384;

/// Whether `key` names a store archive: `…/<epoch>-<24 digits>-<16 hex>.pmtiles`,
/// a key written once and never again.
pub fn is_archive_key(key: &str) -> bool {
    let name = key.rsplit('/').next().unwrap_or(key);
    let Some(stem) = name.strip_suffix(".pmtiles") else {
        return false;
    };
    let mut parts = stem.rsplitn(3, '-');
    let (Some(salt), Some(nanos), Some(epoch)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !epoch.is_empty()
        && nanos.len() == 24
        && nanos.bytes().all(|b| b.is_ascii_digit())
        && salt.len() == 16
        && salt.bytes().all(|b| b.is_ascii_hexdigit())
}

/// One tier: a name for the counters and the permits, and where it keeps
/// its copies.
#[derive(Debug, Clone)]
pub struct Tier {
    pub name: String,
    pub objects: Arc<dyn ObjectStore>,
}

impl Tier {
    pub fn new(name: impl Into<String>, objects: Arc<dyn ObjectStore>) -> Self {
        Self {
            name: name.into(),
            objects,
        }
    }
}

/// What the tiers answered, since they were made.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TieredStats {
    /// Per tier, nearest first: `(name, reads answered, bytes answered)`.
    pub tiers: Vec<(String, u64, u64)>,
    /// Archive reads that went to the origin, and the bytes it gave.
    pub origin_requests: u64,
    pub origin_bytes: u64,
    /// Archives written into a tier, and found there already.
    pub written: u64,
    pub already_there: u64,
    /// Ranges of archives too large to bring down whole, read by range.
    pub ranges_passed: u64,
    /// The process's cache and caps, which every [`Tiered`] sharing them
    /// counts in.
    pub cache: TierCacheStats,
}

/// What a [`Tiered::prefetch`] brought in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Prefetched {
    /// Ranges read in (most from what was kept already), and their bytes.
    pub ranges: u64,
    pub bytes: u64,
    /// Ranges that could not be read: their readers will try again.
    pub failed: u64,
}

#[derive(Debug, Default)]
struct Counters {
    hits: Vec<AtomicU64>,
    hit_bytes: Vec<AtomicU64>,
    origin_requests: AtomicU64,
    origin_bytes: AtomicU64,
    written: AtomicU64,
    already_there: AtomicU64,
    ranges_passed: AtomicU64,
}

/// A download's failure, shared by everyone waiting on it.
#[derive(Debug, Clone)]
enum Failed {
    NotFound(String),
    Other(String),
}

impl Failed {
    fn of(path: &Path, e: object_store::Error) -> Self {
        match e {
            object_store::Error::NotFound { .. } => Self::NotFound(path.to_string()),
            e => Self::Other(e.to_string()),
        }
    }

    fn error(self) -> object_store::Error {
        match self {
            Self::NotFound(path) => object_store::Error::NotFound {
                path,
                source: "not in the origin".into(),
            },
            Self::Other(said) => object_store::Error::Generic {
                store: "tiered",
                source: said.into(),
            },
        }
    }
}

/// A failure of a cache's load, shared by every reader waiting on it.
fn shared_error(e: Arc<object_store::Error>) -> object_store::Error {
    match &*e {
        object_store::Error::NotFound { path, .. } => object_store::Error::NotFound {
            path: path.clone(),
            source: e.to_string().into(),
        },
        _ => object_store::Error::Generic {
            store: "tiered",
            source: e.to_string().into(),
        },
    }
}

fn invalid(said: impl fmt::Display) -> object_store::Error {
    object_store::Error::Generic {
        store: "tiered",
        source: said.to_string().into(),
    }
}

type Download = Shared<BoxFuture<'static, std::result::Result<(Bytes, ObjectMeta), Failed>>>;

/// Where an archive's directories lie, read from its head: what its first
/// read brings in, kept for the process.
#[derive(Debug)]
pub(crate) struct Layout {
    pub(crate) meta: ObjectMeta,
    /// The archive's first [`HEAD_BYTES`]: its header and root directory.
    pub(crate) head: Bytes,
    /// Its directory sections past the head — root, metadata, leaves — as
    /// its header places them; none when it has no PMTiles header.
    pub(crate) sections: Vec<Range<u64>>,
}

/// The directory sections of an archive of `size` bytes whose first bytes
/// are `head`, past the head: none when `head` is no PMTiles v3 header.
fn sections_of(head: &[u8], size: u64) -> Vec<Range<u64>> {
    if head.len() < crate::archive::HEADER_LEN || &head[..7] != b"PMTiles" || head[7] != 3 {
        return Vec::new();
    }
    let field = |at: usize| {
        let mut b = [0u8; 8];
        b.copy_from_slice(&head[at..at + 8]);
        u64::from_le_bytes(b)
    };
    // Root directory, metadata, leaf directories: (offset, length) each.
    [(8, 16), (24, 32), (40, 48)]
        .into_iter()
        .filter_map(|(offset, length)| {
            let (offset, length) = (field(offset), field(length));
            let start = offset.max(head.len() as u64);
            let end = offset.saturating_add(length).min(size);
            (length > 0 && start < end).then_some(start..end)
        })
        .collect()
}

/// `bytes[from..to]`, offsets within `bytes` itself; a read that came back
/// short is an error, never a shorter answer.
fn cut(bytes: &Bytes, from: u64, to: u64) -> Result<Bytes> {
    if to > bytes.len() as u64 || from > to {
        return Err(invalid(format!(
            "a read of {} bytes asked for {from}..{to}",
            bytes.len()
        )));
    }
    Ok(bytes.slice(from as usize..to as usize))
}

/// One piece of a range: from the archive's head, one of its directory
/// sections, or one of its data blocks; the bytes taken from it, in the
/// archive's offsets.
enum Piece {
    Head(Range<u64>),
    Section(Range<u64>, Range<u64>),
    Block(u64, Range<u64>),
}

struct Inner {
    origin: Arc<dyn ObjectStore>,
    tiers: Vec<Tier>,
    whole_below: u64,
    downloads: Mutex<HashMap<String, Download>>,
    counters: Counters,
    cache: Arc<TierCache>,
}

/// An origin with tiers in front of it: see the [module](self).
#[derive(Clone)]
pub struct Tiered {
    inner: Arc<Inner>,
}

impl fmt::Debug for Tiered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tiered")
            .field("origin", &self.inner.origin)
            .field("tiers", &self.inner.tiers)
            .finish()
    }
}

impl fmt::Display for Tiered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Tiered({}", self.inner.origin)?;
        for t in &self.inner.tiers {
            write!(f, " < {}", t.name)?;
        }
        write!(f, ")")
    }
}

impl Tiered {
    /// `origin` behind `tiers`, nearest first, with a cache of its own at
    /// the defaults ([`Self::with_cache`] shares the process's).
    pub fn new(origin: Arc<dyn ObjectStore>, tiers: Vec<Tier>) -> Self {
        Self::with_whole_below(origin, tiers, DEFAULT_WHOLE_BELOW)
    }

    /// The same, archives up to `whole_below` bytes brought down whole on a
    /// range miss.
    pub fn with_whole_below(
        origin: Arc<dyn ObjectStore>,
        tiers: Vec<Tier>,
        whole_below: u64,
    ) -> Self {
        let n = tiers.len();
        Self {
            inner: Arc::new(Inner {
                origin,
                tiers,
                whole_below,
                downloads: Mutex::default(),
                counters: Counters {
                    hits: (0..n).map(|_| AtomicU64::new(0)).collect(),
                    hit_bytes: (0..n).map(|_| AtomicU64::new(0)).collect(),
                    ..Counters::default()
                },
                cache: Arc::new(TierCache::default()),
            }),
        }
    }

    /// The same, reading through `cache` and asking its tiers under its
    /// permits: the process's one, shared by every store it opens.
    pub fn with_cache(mut self, cache: Arc<TierCache>) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.cache = cache,
            None => tracing::warn!("a tiered store already shared keeps its cache"),
        }
        self
    }

    pub fn origin(&self) -> &Arc<dyn ObjectStore> {
        &self.inner.origin
    }

    pub fn cache(&self) -> &Arc<TierCache> {
        &self.inner.cache
    }

    /// An object the tiers keep and the origin never holds — one written by
    /// [`Self::keep_in_tiers`] — from the nearest tier that has it; `None`
    /// when no tier has it. The origin is never asked: it would only answer
    /// that it has none.
    pub async fn get_kept(&self, location: &Path) -> Result<Option<GetResult>> {
        self.get_kept_opts(location, GetOptions::default()).await
    }

    /// The same, read with `options`: a range of it, for one.
    pub async fn get_kept_opts(
        &self,
        location: &Path,
        options: GetOptions,
    ) -> Result<Option<GetResult>> {
        let inner = &self.inner;
        for (i, tier) in inner.tiers.iter().enumerate() {
            match inner.get_from(i, location, options.clone()).await {
                Ok(got) => {
                    inner.hit(i, got.range.end - got.range.start);
                    return Ok(Some(got));
                }
                Err(object_store::Error::NotFound { .. }) => continue,
                Err(error) => {
                    tracing::warn!(tier = %tier.name, key = %location, %error, "TIER-READ-FAILED");
                    continue;
                }
            }
        }
        Ok(None)
    }

    /// `bytes` kept under `location` in every tier that does not hold it
    /// yet, never in the origin: for an object made from archives — named by
    /// what it was made from, so that the same key is the same content —
    /// that the next reader behind these tiers then reads whole instead of
    /// making it again. Written once ([`PutMode::Create`]); a tier that holds it already is not sent the
    /// bytes again. A tier that fails is told in the log.
    pub async fn keep_in_tiers(&self, location: &Path, bytes: Bytes) {
        let inner = &self.inner;
        for i in 0..inner.tiers.len() {
            let tier = &inner.tiers[i];
            if inner
                .cache
                .request(&tier.name, 0, tier.objects.head(location))
                .await
                .is_ok()
            {
                inner.counters.already_there.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            inner.keep_one(i, location, &bytes).await;
        }
    }

    /// The archive ranges `reads` brought into the cache ahead of their
    /// readers, in their order, at most a tier's cap of them at once: for
    /// a caller who knows what it will read — a bake knows its film — to
    /// read it while it does something else. What a range needs that is
    /// kept already costs nothing; a range of a key that is no archive is
    /// left alone (it is never kept). A range that fails is counted and
    /// left to its reader.
    pub async fn prefetch(&self, reads: impl IntoIterator<Item = (Path, Range<u64>)>) -> Prefetched {
        let inner = &self.inner;
        if inner.tiers.is_empty() {
            return Prefetched::default();
        }
        let at_once = inner.cache.config().in_flight.max(1);
        futures_util::stream::iter(reads)
            .filter(|(at, _)| std::future::ready(is_archive_key(at.as_ref())))
            .map(|(at, range)| async move {
                inner
                    .read_range(&at, &GetRange::Bounded(range))
                    .await
                    .map(|got| got.range.end - got.range.start)
            })
            .buffered(at_once)
            .fold(Prefetched::default(), |mut done, read| async move {
                match read {
                    Ok(bytes) => {
                        done.ranges += 1;
                        done.bytes += bytes;
                    }
                    Err(error) => {
                        tracing::debug!(%error, "TIER-PREFETCH-FAILED");
                        done.failed += 1;
                    }
                }
                done
            })
            .await
    }

    pub fn stats(&self) -> TieredStats {
        let c = &self.inner.counters;
        let g = |a: &AtomicU64| a.load(Ordering::Relaxed);
        TieredStats {
            tiers: self
                .inner
                .tiers
                .iter()
                .enumerate()
                .map(|(i, t)| (t.name.clone(), g(&c.hits[i]), g(&c.hit_bytes[i])))
                .collect(),
            origin_requests: g(&c.origin_requests),
            origin_bytes: g(&c.origin_bytes),
            written: g(&c.written),
            already_there: g(&c.already_there),
            ranges_passed: g(&c.ranges_passed),
            cache: self.inner.cache.stats(),
        }
    }
}

/// A ranged read's answer, its body read: what a cap holds a permit for.
async fn buffered(got: GetResult) -> Result<GetResult> {
    let (meta, range, attributes) = (got.meta.clone(), got.range.clone(), got.attributes.clone());
    let body = got.bytes().await?;
    Ok(GetResult {
        payload: GetResultPayload::Stream(
            futures_util::stream::once(async move { Ok(body) }).boxed(),
        ),
        meta,
        range,
        attributes,
        extensions: Extensions::default(),
    })
}

/// A whole object's answer, its tier's permit held until its body is read
/// or let go.
fn held(got: GetResult, in_flight: InFlight) -> GetResult {
    let GetResult {
        payload,
        meta,
        range,
        attributes,
        extensions,
    } = got;
    let payload = match payload {
        GetResultPayload::Stream(body) => GetResultPayload::Stream(
            body.map(move |chunk| {
                let _held = &in_flight;
                chunk
            })
            .boxed(),
        ),
        // A local file: read here, nothing of the tier's held.
        #[allow(unreachable_patterns)]
        local => local,
    };
    GetResult {
        payload,
        meta,
        range,
        attributes,
        extensions,
    }
}

impl Inner {
    /// A read of tier `i`, under its cap: a range read whole within its
    /// deadline, a head as it is, a whole object streamed with its permit
    /// held until the stream is done.
    async fn get_from(&self, i: usize, at: &Path, options: GetOptions) -> Result<GetResult> {
        let tier = &self.tiers[i];
        if options.head {
            return self
                .cache
                .request(&tier.name, 0, tier.objects.get_opts(at, options))
                .await;
        }
        if let Some(range) = &options.range {
            let asked = match range {
                GetRange::Bounded(r) => r.end.saturating_sub(r.start),
                GetRange::Offset(_) => self.whole_below,
                GetRange::Suffix(n) => *n,
            };
            return self
                .cache
                .request(&tier.name, asked, async {
                    buffered(tier.objects.get_opts(at, options).await?).await
                })
                .await;
        }
        let in_flight = self.cache.permit(&tier.name).await;
        let deadline = self.cache.config().request_timeout;
        match tokio::time::timeout(deadline, tier.objects.get_opts(at, options)).await {
            Ok(got) => Ok(held(got?, in_flight)),
            Err(_) => Err(self.cache.timed_out(&tier.name, deadline)),
        }
    }

    /// `at` — or `range` of it — from the origin, under its cap: given its
    /// deadline, and a longer one each time it is not met, up to
    /// [`crate::TierCacheConfig::attempts`]. A failure the origin's client
    /// answered (after its own retries) is not asked again.
    async fn read_origin(
        &self,
        at: &Path,
        range: Option<Range<u64>>,
        asked: u64,
    ) -> Result<(Bytes, ObjectMeta)> {
        let config = self.cache.config();
        let mut deadline = config.deadline(asked);
        let mut attempt = 1;
        loop {
            let options = GetOptions {
                range: range.clone().map(GetRange::Bounded),
                ..GetOptions::default()
            };
            let in_flight = self.cache.permit(ORIGIN).await;
            let read = async {
                let got = self.origin.get_opts(at, options).await?;
                let meta = got.meta.clone();
                Ok::<_, object_store::Error>((got.bytes().await?, meta))
            };
            let answer = tokio::time::timeout(deadline, read).await;
            drop(in_flight);
            match answer {
                Ok(Ok((bytes, meta))) => {
                    TierCache::read_bytes(ORIGIN, bytes.len() as u64);
                    return Ok((bytes, meta));
                }
                Ok(Err(error)) => return Err(error),
                Err(_) => {
                    let error = self.cache.timed_out(ORIGIN, deadline);
                    if attempt >= config.attempts {
                        return Err(error);
                    }
                    tracing::warn!(key = %at, %error, attempt, "TIER-ORIGIN-SLOW");
                    attempt += 1;
                    deadline += config.deadline(asked);
                }
            }
        }
    }

    /// `range` of the archive `at`, from the nearest tier that has it,
    /// else from the origin: an archive up to `whole_below` brought down
    /// whole — and kept in the tiers — then cut, a larger one read by
    /// range. `size` when it is known.
    async fn fetch(
        self: &Arc<Self>,
        at: &Path,
        range: Range<u64>,
        size: Option<u64>,
    ) -> Result<(Bytes, ObjectMeta)> {
        let asked = range.end - range.start;
        for (i, tier) in self.tiers.iter().enumerate() {
            let options = GetOptions {
                range: Some(GetRange::Bounded(range.clone())),
                ..GetOptions::default()
            };
            let read = async {
                let got = tier.objects.get_opts(at, options).await?;
                let meta = got.meta.clone();
                Ok::<_, object_store::Error>((got.bytes().await?, meta))
            };
            match self.cache.request(&tier.name, asked, read).await {
                Ok((bytes, meta)) => {
                    self.hit(i, bytes.len() as u64);
                    return Ok((bytes, meta));
                }
                Err(object_store::Error::NotFound { .. }) => continue,
                Err(error) => {
                    tracing::warn!(tier = %tier.name, key = %at, %error, "TIER-READ-FAILED");
                    continue;
                }
            }
        }
        let size = match size {
            Some(size) => size,
            None => self.cache.request(ORIGIN, 0, self.origin.head(at)).await?.size,
        };
        if size <= self.whole_below {
            let (whole, meta) = self.download(at).await?;
            let r = GetRange::Bounded(range)
                .as_range(whole.len() as u64)
                .map_err(invalid)?;
            // Copied: a slice would keep the whole archive alive in the cache.
            return Ok((Bytes::copy_from_slice(&cut(&whole, r.start, r.end)?), meta));
        }
        self.counters.ranges_passed.fetch_add(1, Ordering::Relaxed);
        self.read_origin(at, Some(range), asked).await
    }

    /// `load` of `key` in `cache`, once however many ask at once, counted
    /// in `class`.
    async fn kept<V>(
        &self,
        cache: &moka::future::Cache<(String, u64), V>,
        key: (String, u64),
        class: CacheClass,
        load: impl Future<Output = Result<V>>,
    ) -> Result<V>
    where
        V: Clone + Send + Sync + 'static,
    {
        if let Some(v) = cache.get(&key).await {
            self.cache.counted(class, true, false);
            return Ok(v);
        }
        let loaded = AtomicBool::new(false);
        let got = cache
            .try_get_with(key, async {
                loaded.store(true, Ordering::Relaxed);
                load.await
            })
            .await
            .map_err(shared_error);
        self.cache
            .counted(class, false, loaded.load(Ordering::Relaxed));
        if loaded.load(Ordering::Relaxed) {
            self.cache.gauge_sizes();
        }
        got
    }

    /// The archive's layout, read once for the process.
    async fn layout(self: &Arc<Self>, at: &Path) -> Result<Arc<Layout>> {
        let load = async {
            let (head, meta) = self.fetch(at, 0..HEAD_BYTES, None).await?;
            let sections = sections_of(&head, meta.size);
            Ok(DirPiece::Layout(Arc::new(Layout {
                meta,
                head,
                sections,
            })))
        };
        let key = (at.to_string(), LAYOUT);
        match self
            .kept(&self.cache.directories, key, CacheClass::Directory, load)
            .await?
        {
            DirPiece::Layout(layout) => Ok(layout),
            DirPiece::Section(_) => Err(invalid("a layout's key holds a section")),
        }
    }

    /// The directory section `section` of the archive, read once for the
    /// process.
    async fn section(self: &Arc<Self>, at: &Path, section: &Range<u64>, size: u64) -> Result<Bytes> {
        let load = async {
            let (bytes, _) = self.fetch(at, section.clone(), Some(size)).await?;
            Ok(DirPiece::Section(bytes))
        };
        let key = (at.to_string(), section.start);
        match self
            .kept(&self.cache.directories, key, CacheClass::Directory, load)
            .await?
        {
            DirPiece::Section(bytes) => Ok(bytes),
            DirPiece::Layout(_) => Err(invalid("a section's key holds a layout")),
        }
    }

    /// The `k`th data block of the archive, kept a while.
    async fn block(self: &Arc<Self>, at: &Path, k: u64, size: u64) -> Result<Bytes> {
        let block = self.cache.config().block_bytes;
        let range = k * block..((k + 1) * block).min(size);
        let load = async {
            let (bytes, _) = self.fetch(at, range, Some(size)).await?;
            Ok(bytes)
        };
        self.kept(&self.cache.data, (at.to_string(), k), CacheClass::Data, load)
            .await
    }

    /// `range` of the archive `at`, read through the cache: its head, its
    /// directory sections and its data blocks, each read once.
    async fn read_range(self: &Arc<Self>, at: &Path, range: &GetRange) -> Result<GetResult> {
        let layout = self.layout(at).await?;
        let size = layout.meta.size;
        let r = range.as_range(size).map_err(invalid)?;
        metrics::histogram!("tuile_tiles_range_bytes").record((r.end - r.start) as f64);
        let head = layout.head.len() as u64;
        let block = self.cache.config().block_bytes;
        let mut pieces = Vec::new();
        let mut pos = r.start;
        while pos < r.end {
            let piece = if pos < head {
                Piece::Head(pos..r.end.min(head))
            } else if let Some(s) = layout.sections.iter().find(|s| s.contains(&pos)) {
                Piece::Section(s.clone(), pos..r.end.min(s.end))
            } else {
                let k = pos / block;
                Piece::Block(k, pos..r.end.min((k + 1) * block).min(size))
            };
            pos = match &piece {
                Piece::Head(t) | Piece::Section(_, t) | Piece::Block(_, t) => t.end,
            };
            pieces.push(piece);
        }
        let layout = &layout;
        let parts = futures_util::future::try_join_all(pieces.into_iter().map(|piece| async move {
            match piece {
                Piece::Head(t) => cut(&layout.head, t.start, t.end),
                Piece::Section(s, t) => {
                    let bytes = self.section(at, &s, size).await?;
                    cut(&bytes, t.start - s.start, t.end - s.start)
                }
                Piece::Block(k, t) => {
                    let bytes = self.block(at, k, size).await?;
                    cut(&bytes, t.start - k * block, t.end - k * block)
                }
            }
        }))
        .await?;
        let body = match parts.len() {
            1 => parts.into_iter().next().unwrap_or_default(),
            _ => {
                let mut all = BytesMut::with_capacity((r.end - r.start) as usize);
                for part in parts {
                    all.extend_from_slice(&part);
                }
                all.freeze()
            }
        };
        Ok(GetResult {
            payload: GetResultPayload::Stream(
                futures_util::stream::once(async move { Ok(body) }).boxed(),
            ),
            meta: layout.meta.clone(),
            range: r,
            attributes: Attributes::default(),
            extensions: Extensions::default(),
        })
    }

    /// The archive from the origin, written into every tier: once however
    /// many ask at the same time.
    async fn download(self: &Arc<Self>, location: &Path) -> Result<(Bytes, ObjectMeta)> {
        let key = location.to_string();
        let flight = {
            let mut downloads = self
                .downloads
                .lock()
                .map_err(|_| invalid("a lock was poisoned"))?;
            match downloads.get(&key) {
                Some(d) => d.clone(),
                None => {
                    let inner = self.clone();
                    let at = location.clone();
                    let d: Download = async move {
                        let got = inner.fetch_and_keep(&at).await;
                        if let Ok(mut d) = inner.downloads.lock() {
                            d.remove(at.as_ref());
                        }
                        got
                    }
                    .boxed()
                    .shared();
                    downloads.insert(key, d.clone());
                    d
                }
            }
        };
        flight.await.map_err(Failed::error)
    }

    async fn fetch_and_keep(&self, at: &Path) -> std::result::Result<(Bytes, ObjectMeta), Failed> {
        // Kept by a download that ended since this reader missed the tiers.
        for (i, tier) in self.tiers.iter().enumerate() {
            let read = async {
                let got = tier.objects.get(at).await?;
                let meta = got.meta.clone();
                Ok::<_, object_store::Error>((got.bytes().await?, meta))
            };
            if let Ok((bytes, meta)) = self.cache.request(&tier.name, self.whole_below, read).await {
                self.hit(i, bytes.len() as u64);
                return Ok((bytes, meta));
            }
        }
        let (bytes, meta) = self
            .read_origin(at, None, self.whole_below)
            .await
            .map_err(|e| Failed::of(at, e))?;
        self.counters
            .origin_requests
            .fetch_add(1, Ordering::Relaxed);
        self.counters
            .origin_bytes
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        metrics::counter!("tuile_tiles_tier_origin_bytes_total").increment(bytes.len() as u64);
        self.keep(self.tiers.len(), at, &bytes).await;
        Ok((bytes, meta))
    }

    /// `bytes` written into the `nearest` tiers, unless one holds them
    /// already. A tier that fails to keep them is told in the log: the
    /// read goes on.
    async fn keep(&self, nearest: usize, at: &Path, bytes: &Bytes) {
        for i in 0..nearest {
            self.keep_one(i, at, bytes).await;
        }
    }

    /// `bytes` written into tier `i`, unless it holds them already.
    async fn keep_one(&self, i: usize, at: &Path, bytes: &Bytes) {
        let tier = &self.tiers[i];
        let opts = PutOptions {
            mode: PutMode::Create,
            ..PutOptions::default()
        };
        let put = tier
            .objects
            .put_opts(at, PutPayload::from_bytes(bytes.clone()), opts);
        match self.cache.request(&tier.name, bytes.len() as u64, put).await {
            Ok(_) => {
                self.counters.written.fetch_add(1, Ordering::Relaxed);
                metrics::counter!("tuile_tiles_tier_writes_total", "tier" => tier.name.clone())
                    .increment(1);
            }
            Err(object_store::Error::AlreadyExists { .. }) => {
                // Written by whoever put it there: the same bytes.
                self.counters.already_there.fetch_add(1, Ordering::Relaxed);
            }
            Err(error) => {
                tracing::warn!(tier = %tier.name, key = %at, %error, "TIER-WRITE-FAILED");
                metrics::counter!("tuile_tiles_tier_write_failures_total", "tier" => tier.name.clone())
                    .increment(1);
            }
        }
    }

    fn hit(&self, i: usize, bytes: u64) {
        self.counters.hits[i].fetch_add(1, Ordering::Relaxed);
        self.counters.hit_bytes[i].fetch_add(bytes, Ordering::Relaxed);
        TierCache::read_bytes(&self.tiers[i].name, bytes);
        metrics::counter!("tuile_tiles_tier_hits_total", "tier" => self.tiers[i].name.clone())
            .increment(1);
        metrics::counter!("tuile_tiles_tier_hit_bytes_total", "tier" => self.tiers[i].name.clone())
            .increment(bytes);
    }
}

/// Whether a read asks for anything but the object as it is.
fn conditional(o: &GetOptions) -> bool {
    o.if_match.is_some()
        || o.if_none_match.is_some()
        || o.if_modified_since.is_some()
        || o.if_unmodified_since.is_some()
        || o.version.is_some()
}

/// `bytes`, the whole object, answered as a read of `range` of it.
fn served(meta: ObjectMeta, bytes: Bytes, range: Option<&GetRange>) -> Result<GetResult> {
    let whole = bytes.len() as u64;
    let r: Range<u64> = match range {
        Some(r) => r.as_range(whole).map_err(invalid)?,
        None => 0..whole,
    };
    let body = bytes.slice(r.start as usize..r.end as usize);
    Ok(GetResult {
        payload: GetResultPayload::Stream(
            futures_util::stream::once(async move { Ok(body) }).boxed(),
        ),
        meta,
        range: r,
        attributes: Attributes::default(),
        extensions: Extensions::default(),
    })
}

#[async_trait]
impl ObjectStore for Tiered {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> Result<PutResult> {
        self.inner.origin.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        opts: PutMultipartOptions,
    ) -> Result<Box<dyn MultipartUpload>> {
        self.inner.origin.put_multipart_opts(location, opts).await
    }

    async fn get_opts(&self, location: &Path, options: GetOptions) -> Result<GetResult> {
        let inner = &self.inner;
        if inner.tiers.is_empty() || conditional(&options) || !is_archive_key(location.as_ref()) {
            return inner.origin.get_opts(location, options).await;
        }
        if !options.head {
            if let Some(range) = &options.range {
                return inner.read_range(location, range).await;
            }
        }
        for (i, tier) in inner.tiers.iter().enumerate() {
            match inner.get_from(i, location, options.clone()).await {
                Ok(got) => {
                    if i > 0 && !options.head {
                        // Found farther away: kept nearer too.
                        let meta = got.meta.clone();
                        let bytes = got.bytes().await?;
                        inner.hit(i, bytes.len() as u64);
                        inner.keep(i, location, &bytes).await;
                        return served(meta, bytes, None);
                    }
                    inner.hit(i, got.range.end - got.range.start);
                    return Ok(got);
                }
                Err(object_store::Error::NotFound { .. }) => continue,
                Err(error) => {
                    tracing::warn!(tier = %tier.name, key = %location, %error, "TIER-READ-FAILED");
                    continue;
                }
            }
        }
        if options.head {
            return inner
                .cache
                .request(ORIGIN, 0, inner.origin.get_opts(location, options))
                .await;
        }
        let (bytes, meta) = inner.download(location).await?;
        served(meta, bytes, None)
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, Result<Path>>,
    ) -> BoxStream<'static, Result<Path>> {
        // A tier's copies stay: their keys are never written again, and
        // tiers keep what they keep by their own rules.
        self.inner.origin.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta>> {
        self.inner.origin.list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> Result<ListResult> {
        self.inner.origin.list_with_delimiter(prefix).await
    }

    async fn copy_opts(&self, from: &Path, to: &Path, options: CopyOptions) -> Result<()> {
        self.inner.origin.copy_opts(from, to, options).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TierCache, TierCacheConfig};
    use std::time::Duration;
    use object_store::memory::InMemory;

    const ARCHIVE: &str =
        "imagery/zones/z4/1/2/202611-176279040000000000000000-00000000deadbeef.pmtiles";

    /// An object store that counts the reads it answers, by key.
    #[derive(Debug, Default)]
    struct Counted {
        inner: InMemory,
        gets: Mutex<HashMap<String, u64>>,
    }

    impl Counted {
        fn gets(&self, key: &str) -> u64 {
            self.gets.lock().unwrap().get(key).copied().unwrap_or(0)
        }
    }

    impl fmt::Display for Counted {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "Counted")
        }
    }

    #[async_trait]
    impl ObjectStore for Counted {
        async fn put_opts(&self, l: &Path, p: PutPayload, o: PutOptions) -> Result<PutResult> {
            self.inner.put_opts(l, p, o).await
        }
        async fn put_multipart_opts(
            &self,
            l: &Path,
            o: PutMultipartOptions,
        ) -> Result<Box<dyn MultipartUpload>> {
            self.inner.put_multipart_opts(l, o).await
        }
        async fn get_opts(&self, l: &Path, o: GetOptions) -> Result<GetResult> {
            if !o.head {
                *self.gets.lock().unwrap().entry(l.to_string()).or_default() += 1;
            }
            // A slow origin: concurrent readers overlap.
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            self.inner.get_opts(l, o).await
        }
        fn delete_stream(
            &self,
            l: BoxStream<'static, Result<Path>>,
        ) -> BoxStream<'static, Result<Path>> {
            self.inner.delete_stream(l)
        }
        fn list(&self, p: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta>> {
            self.inner.list(p)
        }
        async fn list_with_delimiter(&self, p: Option<&Path>) -> Result<ListResult> {
            self.inner.list_with_delimiter(p).await
        }
        async fn copy_opts(&self, f: &Path, t: &Path, o: CopyOptions) -> Result<()> {
            self.inner.copy_opts(f, t, o).await
        }
    }

    fn bytes() -> Bytes {
        Bytes::from((0..=255u8).cycle().take(10_000).collect::<Vec<u8>>())
    }

    async fn origin_with_archive() -> Arc<Counted> {
        let origin = Arc::new(Counted::default());
        origin
            .inner
            .put(&Path::from(ARCHIVE), bytes().into())
            .await
            .unwrap();
        origin
            .inner
            .put(
                &Path::from("imagery/zones/z4/1/2/manifest.json"),
                "{}".into(),
            )
            .await
            .unwrap();
        origin
    }

    #[test]
    fn only_archive_keys_are_tiered() {
        assert!(is_archive_key(ARCHIVE));
        assert!(is_archive_key(
            "terrain/top/1-2-176279040000000000000000-0123456789abcdef.pmtiles"
        ));
        assert!(!is_archive_key("imagery/zones/z4/1/2/manifest.json"));
        assert!(!is_archive_key("catalog.json"));
        assert!(!is_archive_key("_extracts/run/c0001.pmtiles"));
        assert!(!is_archive_key(
            "x/-176279040000000000000000-0123456789abcdef.pmtiles"
        ));
    }

    #[tokio::test]
    async fn a_miss_fills_every_tier_and_the_next_read_never_reaches_the_origin() {
        let origin = origin_with_archive().await;
        let (local, global) = (Arc::new(InMemory::new()), Arc::new(InMemory::new()));
        let tiered = Tiered::new(
            origin.clone(),
            vec![
                Tier::new("local", local.clone()),
                Tier::new("global", global.clone()),
            ],
        );
        let at = Path::from(ARCHIVE);
        assert_eq!(
            tiered.get(&at).await.unwrap().bytes().await.unwrap(),
            bytes()
        );
        assert_eq!(origin.gets(ARCHIVE), 1);
        assert_eq!(
            local.get(&at).await.unwrap().bytes().await.unwrap(),
            bytes()
        );
        assert_eq!(
            global.get(&at).await.unwrap().bytes().await.unwrap(),
            bytes()
        );
        // Again, whole and by range: the nearest tier answers.
        assert_eq!(
            tiered.get(&at).await.unwrap().bytes().await.unwrap(),
            bytes()
        );
        assert_eq!(
            tiered.get_range(&at, 100..200).await.unwrap(),
            bytes().slice(100..200)
        );
        assert_eq!(origin.gets(ARCHIVE), 1, "the origin was read once");
        let s = tiered.stats();
        assert_eq!((s.origin_requests, s.written), (1, 2));
        assert_eq!(s.tiers[0].1, 2);
    }

    #[tokio::test]
    async fn a_far_tier_hit_fills_the_near_one_and_spares_the_origin() {
        let origin = origin_with_archive().await;
        let (local, global) = (Arc::new(InMemory::new()), Arc::new(InMemory::new()));
        let at = Path::from(ARCHIVE);
        // Another machine of the region brought it down already.
        global.put(&at, bytes().into()).await.unwrap();
        let tiered = Tiered::new(
            origin.clone(),
            vec![
                Tier::new("local", local.clone()),
                Tier::new("global", global),
            ],
        );
        assert_eq!(
            tiered.get(&at).await.unwrap().bytes().await.unwrap(),
            bytes()
        );
        assert_eq!(origin.gets(ARCHIVE), 0);
        assert_eq!(
            local.get(&at).await.unwrap().bytes().await.unwrap(),
            bytes()
        );
    }

    #[tokio::test]
    async fn concurrent_readers_of_one_archive_download_it_once() {
        let origin = origin_with_archive().await;
        let global = Arc::new(InMemory::new());
        let tiered = Tiered::new(origin.clone(), vec![Tier::new("global", global)]);
        let at = Path::from(ARCHIVE);
        let reads = (0..8).map(|i| {
            let (t, at) = (tiered.clone(), at.clone());
            async move {
                if i % 2 == 0 {
                    t.get(&at).await.unwrap().bytes().await.unwrap()
                } else {
                    t.get_range(&at, 0..10_000).await.unwrap()
                }
            }
        });
        for got in futures_util::future::join_all(reads).await {
            assert_eq!(got, bytes());
        }
        assert_eq!(origin.gets(ARCHIVE), 1, "one download for eight readers");
        assert_eq!(tiered.stats().written, 1, "and one write");
    }

    #[tokio::test]
    async fn what_is_rewritten_always_goes_to_the_origin() {
        let origin = origin_with_archive().await;
        let global = Arc::new(InMemory::new());
        let tiered = Tiered::new(origin.clone(), vec![Tier::new("global", global.clone())]);
        let manifest = "imagery/zones/z4/1/2/manifest.json";
        for _ in 0..2 {
            tiered.get(&Path::from(manifest)).await.unwrap();
        }
        assert_eq!(origin.gets(manifest), 2);
        assert!(global.head(&Path::from(manifest)).await.is_err());
    }

    #[tokio::test]
    async fn an_archive_absent_everywhere_is_not_found() {
        let origin = Arc::new(Counted::default());
        let tiered = Tiered::new(origin, vec![Tier::new("global", Arc::new(InMemory::new()))]);
        let got = tiered.get(&Path::from(ARCHIVE)).await;
        assert!(
            matches!(got, Err(object_store::Error::NotFound { .. })),
            "{got:?}"
        );
    }

    #[tokio::test]
    async fn a_large_archive_is_read_by_ranges_from_the_origin() {
        let origin = origin_with_archive().await;
        let global = Arc::new(InMemory::new());
        let tiered = Tiered::with_whole_below(
            origin.clone(),
            vec![Tier::new("global", global.clone())],
            1000,
        );
        let at = Path::from(ARCHIVE);
        assert_eq!(
            tiered.get_range(&at, 0..10).await.unwrap(),
            bytes().slice(0..10)
        );
        assert!(global.head(&at).await.is_err(), "not brought down whole");
        assert_eq!(tiered.stats().ranges_passed, 1);
    }

    /// **An archive a tier forgot is read from the origin and kept again.**
    /// A bucket's lifecycle deletes by age: the next process that reads
    /// the archive finds it absent, reads the origin once, and writes it
    /// back — and a read of what a tier holds never writes to it.
    #[tokio::test]
    async fn an_archive_a_tier_forgot_is_read_from_the_origin_and_kept_again() {
        let origin = origin_with_archive().await;
        let global = Arc::new(InMemory::new());
        let at = Path::from(ARCHIVE);
        // One process per read: each with a cache of its own.
        let process = || Tiered::new(origin.clone(), vec![Tier::new("global", global.clone())]);
        process().get(&at).await.unwrap().bytes().await.unwrap();
        assert_eq!(origin.gets(ARCHIVE), 1, "the first reader reads the origin");
        let written = global.head(&at).await.expect("kept").last_modified;
        // Read whole and by range by other processes: no write, no edit.
        for _ in 0..3 {
            let p = process();
            p.get(&at).await.unwrap().bytes().await.unwrap();
            p.get_range(&at, 0..10).await.unwrap();
        }
        assert_eq!(origin.gets(ARCHIVE), 1, "kept: the origin is not asked");
        assert_eq!(global.head(&at).await.unwrap().last_modified, written, "a read writes nothing");
        // The lifecycle deletes it.
        global.delete(&at).await.unwrap();
        let p = process();
        assert_eq!(&p.get_range(&at, 100..110).await.unwrap()[..], &bytes()[100..110]);
        assert_eq!(origin.gets(ARCHIVE), 2, "absent from the tier: read from the origin");
        let again = global.head(&at).await.expect("kept again");
        assert!(again.last_modified >= written, "written back, its age starting over");
        assert_eq!(p.stats().written, 1);
        process().get(&at).await.unwrap().bytes().await.unwrap();
        assert_eq!(origin.gets(ARCHIVE), 2, "the next reader reads the tier again");
    }

    /// A slow tier, as a bucket across a busy network is: it counts the
    /// reads it answers and the most it was answering at once.
    #[derive(Debug, Default)]
    struct Slow {
        inner: InMemory,
        gets: AtomicU64,
        now: AtomicU64,
        most: AtomicU64,
    }

    impl Slow {
        fn gets(&self) -> u64 {
            self.gets.load(Ordering::SeqCst)
        }
        fn most(&self) -> u64 {
            self.most.load(Ordering::SeqCst)
        }
    }

    impl fmt::Display for Slow {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "Slow")
        }
    }

    #[async_trait]
    impl ObjectStore for Slow {
        async fn put_opts(&self, l: &Path, p: PutPayload, o: PutOptions) -> Result<PutResult> {
            self.inner.put_opts(l, p, o).await
        }
        async fn put_multipart_opts(
            &self,
            l: &Path,
            o: PutMultipartOptions,
        ) -> Result<Box<dyn MultipartUpload>> {
            self.inner.put_multipart_opts(l, o).await
        }
        async fn get_opts(&self, l: &Path, o: GetOptions) -> Result<GetResult> {
            if o.head {
                return self.inner.get_opts(l, o).await;
            }
            self.gets.fetch_add(1, Ordering::SeqCst);
            let now = self.now.fetch_add(1, Ordering::SeqCst) + 1;
            self.most.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(20)).await;
            let got = self.inner.get_opts(l, o).await;
            self.now.fetch_sub(1, Ordering::SeqCst);
            got
        }
        fn delete_stream(
            &self,
            l: BoxStream<'static, Result<Path>>,
        ) -> BoxStream<'static, Result<Path>> {
            self.inner.delete_stream(l)
        }
        fn list(&self, p: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta>> {
            self.inner.list(p)
        }
        async fn list_with_delimiter(&self, p: Option<&Path>) -> Result<ListResult> {
            self.inner.list_with_delimiter(p).await
        }
        async fn copy_opts(&self, f: &Path, t: &Path, o: CopyOptions) -> Result<()> {
            self.inner.copy_opts(f, t, o).await
        }
    }

    /// The `n`th archive key of a test.
    fn archive(n: u64) -> String {
        format!("imagery/zones/z4/1/2/202611-1762790400000000{n:08}-00000000deadbeef.pmtiles")
    }

    /// **However many readers, a tier is asked at most its cap at once.**
    /// Two hundred ranges of two hundred archives read together: without a
    /// cap, two hundred requests in flight, the storm a bake start made.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_tier_is_never_asked_more_than_its_cap_at_once() {
        let origin = Arc::new(Counted::default());
        let global = Arc::new(Slow::default());
        for n in 0..200 {
            global
                .inner
                .put(&Path::from(archive(n)), bytes().into())
                .await
                .unwrap();
        }
        let tiered = Tiered::new(origin.clone(), vec![Tier::new("global", global.clone())])
            .with_cache(Arc::new(TierCache::new(TierCacheConfig {
                in_flight: 16,
                ..TierCacheConfig::default()
            })));
        let reads = (0..200).map(|n| {
            let t = tiered.clone();
            async move { t.get_range(&Path::from(archive(n)), 100..200).await.unwrap() }
        });
        for got in futures_util::future::join_all(reads).await {
            assert_eq!(got, bytes().slice(100..200));
        }
        assert!(global.most() <= 16, "{} requests at once", global.most());
        assert!(tiered.stats().cache.cap_waits > 0, "readers waited");
        assert_eq!(global.gets(), 200, "each archive read once");
        assert_eq!(origin.gets.lock().unwrap().len(), 0);
    }

    /// **Two readers of one range make one request.**
    #[tokio::test]
    async fn two_readers_of_one_range_make_one_request() {
        let origin = Arc::new(Counted::default());
        let global = Arc::new(Slow::default());
        let at = Path::from(ARCHIVE);
        global.inner.put(&at, bytes().into()).await.unwrap();
        let tiered = Tiered::new(origin, vec![Tier::new("global", global.clone())]);
        let (a, b) = futures_util::join!(
            tiered.get_range(&at, 100..200),
            tiered.get_range(&at, 100..200)
        );
        assert_eq!(a.unwrap(), bytes().slice(100..200));
        assert_eq!(b.unwrap(), bytes().slice(100..200));
        assert_eq!(global.gets(), 1);
    }

    /// An archive's bytes, `len` of them, its header saying its leaf
    /// directories lie at `leaves` and its tiles at `data`: the layout of a
    /// PMTiles archive, the rest of it filler.
    fn laid_out(len: usize, leaves: Range<u64>, data: Range<u64>) -> Bytes {
        let mut b: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        b[..7].copy_from_slice(b"PMTiles");
        b[7] = 3;
        let mut field = |at: usize, v: u64| b[at..at + 8].copy_from_slice(&v.to_le_bytes());
        field(8, 127);
        field(16, 100);
        field(24, 227);
        field(32, 0);
        field(40, leaves.start);
        field(48, leaves.end - leaves.start);
        field(56, data.start);
        field(64, data.end - data.start);
        Bytes::from(b)
    }

    /// **A range read is kept: read again, it costs no request.**
    #[tokio::test]
    async fn a_range_read_again_costs_no_request() {
        let origin = Arc::new(Counted::default());
        let global = Arc::new(Slow::default());
        let at = Path::from(ARCHIVE);
        let archive = laid_out(1 << 20, 20_000..24_000, 24_000..1 << 20);
        global.inner.put(&at, archive.clone().into()).await.unwrap();
        let tiered = Tiered::new(origin, vec![Tier::new("global", global.clone())]);
        let range = 300_000..301_000;
        assert_eq!(
            tiered.get_range(&at, range.clone()).await.unwrap(),
            archive.slice(300_000..301_000)
        );
        let first = global.gets();
        assert_eq!(
            tiered.get_range(&at, range).await.unwrap(),
            archive.slice(300_000..301_000)
        );
        assert_eq!(global.gets(), first, "the second read is kept");
    }

    /// A tiered store over `global` alone, its data blocks kept `ttl`.
    fn kept_for(global: Arc<Slow>, ttl: Duration) -> Tiered {
        Tiered::new(Arc::new(Counted::default()), vec![Tier::new("global", global)]).with_cache(
            Arc::new(TierCache::new(TierCacheConfig {
                block_bytes: 64 << 10,
                data_ttl: ttl,
                ..TierCacheConfig::default()
            })),
        )
    }

    /// **Tile data is kept for its time, then read again once.**
    #[tokio::test]
    async fn tile_data_is_read_again_once_its_time_is_up() {
        let global = Arc::new(Slow::default());
        let at = Path::from(ARCHIVE);
        let archive = laid_out(1 << 20, 20_000..24_000, 24_000..1 << 20);
        global.inner.put(&at, archive.clone().into()).await.unwrap();
        let tiered = kept_for(global.clone(), Duration::from_millis(300));
        let range = 300_000..301_000;
        tiered.get_range(&at, range.clone()).await.unwrap();
        assert_eq!(global.gets(), 2, "its layout, then its block");
        tiered.get_range(&at, range.clone()).await.unwrap();
        assert_eq!(global.gets(), 2, "within its time: no request");
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            tiered.get_range(&at, range).await.unwrap(),
            archive.slice(300_000..301_000)
        );
        assert_eq!(global.gets(), 3, "past its time: the block once more");
        let s = tiered.stats().cache;
        assert_eq!((s.data.hits, s.data.loads), (1, 2), "{s:?}");
    }

    /// **Directories outlive tile data.** An archive's header and leaf
    /// directories are read once for the process; its tiles, once a while.
    #[tokio::test]
    async fn directories_stay_kept_past_the_data_s_time() {
        let global = Arc::new(Slow::default());
        let at = Path::from(ARCHIVE);
        let archive = laid_out(1 << 20, 200_000..210_000, 210_000..1 << 20);
        global.inner.put(&at, archive.clone().into()).await.unwrap();
        let tiered = kept_for(global.clone(), Duration::from_millis(300));
        let (leaves, tiles) = (201_000..202_000, 500_000..501_000);
        tiered.get_range(&at, 0..127).await.unwrap();
        tiered.get_range(&at, leaves.clone()).await.unwrap();
        tiered.get_range(&at, tiles.clone()).await.unwrap();
        assert_eq!(global.gets(), 3, "head, leaves, a block");
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            tiered.get_range(&at, leaves).await.unwrap(),
            archive.slice(201_000..202_000)
        );
        tiered.get_range(&at, 0..127).await.unwrap();
        assert_eq!(global.gets(), 3, "directories still kept");
        assert_eq!(
            tiered.get_range(&at, tiles).await.unwrap(),
            archive.slice(500_000..501_000)
        );
        assert_eq!(global.gets(), 4, "the tiles read again");
    }

    /// **A range across blocks, sections and the head is the archive's
    /// bytes, exactly.**
    #[tokio::test]
    async fn a_range_across_pieces_is_the_archive_s_bytes() {
        let global = Arc::new(Slow::default());
        let at = Path::from(ARCHIVE);
        let archive = laid_out(1 << 20, 20_000..24_000, 24_000..1 << 20);
        global.inner.put(&at, archive.clone().into()).await.unwrap();
        let tiered = kept_for(global.clone(), Duration::from_secs(60));
        for r in [0..1 << 20, 10..300_000, 16_000..25_000, 65_000..200_000, 1_000_000..1 << 20] {
            assert_eq!(
                tiered.get_range(&at, r.clone()).await.unwrap(),
                archive.slice(r.start as usize..r.end as usize),
                "{r:?}"
            );
        }
        // Past the end: clamped as an object store clamps it.
        assert_eq!(
            tiered.get_range(&at, 1_048_000..2_000_000).await.unwrap(),
            archive.slice(1_048_000..)
        );
    }

    /// **An archive no tier holds is brought down once, its blocks cut from
    /// it; a planned read brings them in ahead.**
    #[tokio::test]
    async fn a_prefetch_brings_the_blocks_in_ahead_of_their_readers() {
        let origin = Arc::new(Counted::default());
        let at = Path::from(ARCHIVE);
        let archive = laid_out(1 << 20, 20_000..24_000, 24_000..1 << 20);
        origin.inner.put(&at, archive.clone().into()).await.unwrap();
        let global = Arc::new(Slow::default());
        let tiered = Tiered::new(origin.clone(), vec![Tier::new("global", global.clone())]);
        let ranges = [100_000..110_000, 400_000..500_000, 900_000..901_000];
        let done = tiered
            .prefetch(ranges.iter().map(|r| (at.clone(), r.clone())))
            .await;
        assert_eq!((done.ranges, done.failed), (3, 0));
        assert_eq!(origin.gets(ARCHIVE), 1, "brought down whole, once");
        let before = global.gets();
        for r in ranges {
            assert_eq!(
                tiered.get_range(&at, r.clone()).await.unwrap(),
                archive.slice(r.start as usize..r.end as usize)
            );
        }
        assert_eq!(global.gets(), before, "every range kept");
        assert_eq!(origin.gets(ARCHIVE), 1);
    }
}
