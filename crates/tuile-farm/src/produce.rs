// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A whole film on one machine: baked in packs, rendered in parts, joined.
//!
//! ```text
//! frames  1 ───────────────────────────────────────────────── 18000
//! packs   [   1–450   ][  451–900  ][  901–1350 ] …   baked, each once
//! parts   [   1–450   ][  451–900  ][  901–1350 ] …   rendered, each to a segment
//!         (a pack may be cut in several parts; by default it is one)
//! film    ═══════════════════════════════════════     the segments, joined
//! ```
//!
//! Each pack is a lane: its bake, then its parts one after the other.
//! Several lanes run at once ([`Order::at_once`]), each step on a thread of
//! the runtime's blocking pool, in this process: a bake and a render are
//! functions of the [`Host`], called, not programs run.
//!
//! # Where it all is
//!
//! In the run store — a bucket, unless the environment names a directory —
//! and nowhere else: packs, segments, receipts and the film. A step writes
//! into a scratch directory that is gone when the production ends; what it
//! wrote is put in the store and removed.
//!
//! The [`Host`] says under which keys: [`Host::packs`] names the bake's own
//! parameters and [`Host::film`] the render's, so a pack or a segment found
//! there was made with the parameters asked for now, and one made with
//! others is somewhere else.
//!
//! # What counts as done, and is not done again
//!
//! A part is done when its **receipt** says so: put after the segment was
//! opened and found to hold exactly the part's frames, naming its size. Not
//! the segment's presence — a render that stopped mid-way leaves a file
//! behind — and not the render's own word alone. A pack is done when it is at
//! its key, where it is put only once its bake has ended well.
//!
//! What is done is not done again unless the order says so
//! ([`Order::render_again`], [`Order::bake_again`]). So a production can be
//! stopped at any moment and asked for again: it goes on from there. And the
//! film is joined only when every part is there, and must hold every frame:
//! a film with a hole in it is not written, the holes are named.
//!
//! # What a thread cannot do
//!
//! A step that never returns cannot be taken back: a thread is not killed.
//! Past [`Order::step_timeout`] the production stops and says so — its
//! process is then the thing to end — and asked again, goes on from what was
//! done.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;

use crate::{concat, RunStore, StoreError};

/// Frames `first..=last`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct Stretch {
    pub first: u32,
    pub last: u32,
}

impl Stretch {
    pub fn frames(&self) -> u64 {
        u64::from(self.last - self.first) + 1
    }
}

impl std::fmt::Display for Stretch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}-{}", self.first, self.last)
    }
}

/// What to produce.
#[derive(Debug, Clone)]
pub struct Order {
    pub frames: Stretch,
    /// Frames to a pack, and to a part. A part never straddles two packs.
    pub pack_frames: u32,
    pub part_frames: u32,
    /// Packs worked on at once: so many bakes or renders run together.
    pub at_once: usize,
    /// How many times a bake or a render is tried.
    pub tries: u32,
    /// How long a bake or a render may take. `None`: as long as it does.
    pub step_timeout: Option<Duration>,
    /// The film's cadence, checked against what was joined. `None`: not checked.
    pub fps: Option<f64>,
    /// Bakes the packs its parts need even where they are baked already.
    pub bake_again: bool,
    /// Renders every part even where it is rendered already.
    pub render_again: bool,
    /// This process's share of a production several run at once: task
    /// `index` of `count` takes the packs `index`, `index + count`, … in
    /// order. `None`: every pack.
    pub task: Option<(u32, u32)>,
}

impl Order {
    /// `frames`, cut and tried as a film usually is, nothing done again:
    /// packs and parts of 450 frames, one at a time, three tries.
    pub fn of(frames: Stretch) -> Self {
        Self {
            frames,
            pack_frames: 450,
            part_frames: 450,
            at_once: 1,
            tries: 3,
            step_timeout: None,
            fps: None,
            bake_again: false,
            render_again: false,
            task: None,
        }
    }
}

/// One pack and the parts rendered from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lane {
    pub pack: Stretch,
    pub parts: Vec<Stretch>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProduceError {
    #[error("nothing to produce: {0}")]
    Order(String),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("{} of {} parts are missing, the film is not written: frames {}", .missing.len(), .parts, named(.missing))]
    Missing { missing: Vec<Stretch>, parts: usize },
    #[error("stopped: {0} is there")]
    Stopped(String),
    #[error("the {step} of frames {frames} has not returned in {seconds} s: it cannot be taken back, end this process and ask again")]
    Stalled {
        step: &'static str,
        frames: Stretch,
        seconds: u64,
    },
    #[error("the film: {0}")]
    Film(String),
    /// This task's packs are done and others' are not yet: the last of the
    /// tasks to finish joins the film.
    #[error("this task's share is done; {done} of {of} parts are, and the last task to finish joins the film")]
    Waiting { done: usize, of: usize },
}

fn named(stretches: &[Stretch]) -> String {
    stretches
        .iter()
        .map(Stretch::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The lanes of an order: its packs in order, each with its parts in order.
pub fn plan(order: &Order) -> Result<Vec<Lane>, ProduceError> {
    if let Some((index, count)) = order.task {
        if index >= count {
            return Err(ProduceError::Order(format!("task {index} of {count}")));
        }
    }
    let Stretch { first, last } = order.frames;
    if first > last {
        return Err(ProduceError::Order(format!("frames {first}-{last}")));
    }
    if order.pack_frames == 0 || order.part_frames == 0 {
        return Err(ProduceError::Order("a pack or a part of no frame".into()));
    }
    let cut = |within: Stretch, by: u32| -> Vec<Stretch> {
        let mut out = Vec::new();
        let mut at = within.first;
        loop {
            let end = at.saturating_add(by - 1).min(within.last);
            out.push(Stretch {
                first: at,
                last: end,
            });
            if end == within.last {
                return out;
            }
            at = end + 1;
        }
    };
    Ok(cut(order.frames, order.pack_frames)
        .into_iter()
        .map(|pack| Lane {
            pack,
            parts: cut(pack, order.part_frames),
        })
        .collect())
}

/// A step of the production.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Bake,
    Render,
}

impl Step {
    fn name(self) -> &'static str {
        match self {
            Step::Bake => "bake",
            Step::Render => "render",
        }
    }
}

/// Why a step did not end well.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failed {
    pub why: String,
    /// Whether trying again can end otherwise. A part that names what is not
    /// there to be read fails the same way every time.
    pub again: bool,
}

impl Failed {
    /// A failure worth another try.
    pub fn once(why: impl Into<String>) -> Self {
        Self {
            why: why.into(),
            again: true,
        }
    }

    /// A failure another try would only repeat.
    pub fn for_good(why: impl Into<String>) -> Self {
        Self {
            why: why.into(),
            again: false,
        }
    }
}

/// What bakes a pack and renders a part, and says where they are kept.
/// [`Host::bake`] and [`Host::render`] are called on a thread of their own
/// and block it for as long as they take; several are called at once when
/// the order says so.
pub trait Host: Send + Sync + 'static {
    /// The prefix of the packs' keys. It names everything a pack depends
    /// on — the cameras, the picture's size, the selection's settings, the
    /// sources: two bakes that differ in any of them have two prefixes.
    fn packs(&self) -> String;
    /// The prefix of the film's keys: its parts, their receipts, the film.
    /// It names the packs and everything a rendered picture depends on.
    fn film(&self) -> String;
    /// Bakes `frames` into a pack written at `out`, a scratch file.
    fn bake(&self, frames: Stretch, out: &Path) -> Result<(), Failed>;
    /// Renders `frames`, whose pack is in the store at the key `pack`, to a
    /// segment written at `out`, a scratch file.
    fn render(&self, frames: Stretch, pack: &str, out: &Path) -> Result<(), Failed>;
}

/// What is said as the production goes: on standard output as it is said,
/// and kept, to be put beside the film.
#[derive(Default)]
pub struct Journal {
    said: Mutex<Vec<String>>,
}

impl Journal {
    pub fn say(&self, line: &str) {
        let line = format!(
            "{} {line}",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
        );
        println!("{line}");
        if let Ok(mut said) = self.said.lock() {
            said.push(line);
        }
    }

    /// Everything said so far, a line each.
    pub fn text(&self) -> String {
        self.said
            .lock()
            .map(|said| said.join("\n") + "\n")
            .unwrap_or_default()
    }
}

/// A part's receipt: beside its segment, written once the segment was found
/// whole.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Receipt {
    pub frames: Stretch,
    /// The segment's size: a segment of another size is not the one vouched for.
    pub bytes: u64,
    pub tries: u32,
    pub seconds: f64,
}

/// The keys of a production.
struct Keys {
    packs: String,
    film: String,
}

impl Keys {
    fn of(host: &dyn Host) -> Self {
        let trimmed = |prefix: String| prefix.trim_matches('/').to_string();
        Self {
            packs: trimmed(host.packs()),
            film: trimmed(host.film()),
        }
    }
    fn pack(&self, frames: Stretch) -> String {
        format!("{}/{frames}.tuilepack", self.packs)
    }
    fn parts(&self) -> String {
        format!("{}/parts", self.film)
    }
    fn segment(&self, frames: Stretch) -> String {
        format!("{}/parts/{frames}.mp4", self.film)
    }
    fn receipt(&self, frames: Stretch) -> String {
        format!("{}/parts/{frames}.json", self.film)
    }
    fn stop(&self) -> String {
        format!("{}/STOP", self.film)
    }
    fn journal(&self, task: Option<(u32, u32)>) -> String {
        match task {
            Some((index, _)) => format!("{}/journal-t{index}.log", self.film),
            None => format!("{}/journal.log", self.film),
        }
    }
    fn film(&self) -> String {
        format!("{}/film.mp4", self.film)
    }
}

/// The parts done: each one's receipt, and the segment it vouches for — of
/// the frames it says, of the size it says.
async fn done(
    store: &dyn RunStore,
    keys: &Keys,
    parts: &[Stretch],
) -> Result<Vec<Stretch>, StoreError> {
    let listed: HashMap<String, u64> = store
        .list(&keys.parts())
        .await?
        .into_iter()
        .map(|e| (e.key, e.size))
        .collect();
    let mut done = Vec::new();
    for part in parts {
        let (Some(size), Some(segment)) = (
            listed.get(&keys.receipt(*part)),
            listed.get(&keys.segment(*part)),
        ) else {
            continue;
        };
        let bytes = store.get_range(&keys.receipt(*part), 0..*size).await?;
        // A receipt that cannot be read vouches for nothing.
        let Ok(receipt) = serde_json::from_slice::<Receipt>(&bytes) else {
            continue;
        };
        if receipt.frames == *part && receipt.bytes == *segment {
            done.push(*part);
        }
    }
    Ok(done)
}

/// A production under way: what was asked, of whom.
struct Run<'a> {
    order: &'a Order,
    store: &'a dyn RunStore,
    host: &'a Arc<dyn Host>,
    journal: &'a Journal,
    keys: &'a Keys,
    /// The parts already done, as the store had them when it began.
    done: &'a [Stretch],
    /// Where steps write, gone with the production.
    scratch: &'a Path,
}

/// Calls a step on a thread of its own until it ends well and `whole` finds
/// its output so, at most [`Order::tries`] times. `None` when it never did.
async fn attempt(
    run: &Run<'_>,
    step: Step,
    frames: Stretch,
    call: impl Fn(Arc<dyn Host>) -> Result<(), Failed> + Clone + Send + 'static,
    whole: impl Fn() -> Result<(), String>,
) -> Result<Option<(u32, f64)>, ProduceError> {
    let Run {
        order,
        host,
        journal,
        ..
    } = *run;
    for tried in 1..=order.tries.max(1) {
        let began = Instant::now();
        let (call, host) = (call.clone(), host.clone());
        let called = tokio::task::spawn_blocking(move || call(host));
        let joined = match order.step_timeout {
            Some(within) => match tokio::time::timeout(within, called).await {
                Ok(joined) => joined,
                Err(_) => {
                    let stalled = ProduceError::Stalled {
                        step: step.name(),
                        frames,
                        seconds: within.as_secs(),
                    };
                    journal.say(&stalled.to_string());
                    return Err(stalled);
                }
            },
            None => called.await,
        };
        let seconds = began.elapsed().as_secs_f64();
        let ended = match joined {
            Ok(Ok(())) => whole().map_err(|why| {
                Failed::once(format!("it ended well, and its output is not whole: {why}"))
            }),
            Ok(Err(failed)) => Err(failed),
            // It panicked: said, and the lanes beside it go on.
            Err(e) => Err(Failed::once(e.to_string())),
        };
        match ended {
            Ok(()) => {
                journal.say(&format!(
                    "{} {frames} try {tried}: done in {seconds:.0} s",
                    step.name()
                ));
                return Ok(Some((tried, seconds)));
            }
            Err(failed) => {
                journal.say(&format!(
                    "{} {frames} try {tried}: failed after {seconds:.0} s — {}{}",
                    step.name(),
                    failed.why,
                    if failed.again {
                        ""
                    } else {
                        " — not tried again"
                    }
                ));
                if !failed.again {
                    break;
                }
            }
        }
    }
    Ok(None)
}

/// One lane: the pack if a part needs it, then the parts not yet done. Gives
/// back the parts still missing.
async fn work(run: &Run<'_>, lane: &Lane) -> Result<Vec<Stretch>, ProduceError> {
    let Run {
        order,
        store,
        keys,
        scratch,
        ..
    } = *run;
    let wanted: Vec<Stretch> = lane
        .parts
        .iter()
        .copied()
        .filter(|part| order.render_again || !run.done.contains(part))
        .collect();
    if wanted.is_empty() {
        return Ok(wanted);
    }
    let stopped = || async {
        match store.exists(&keys.stop()).await? {
            true => Err(ProduceError::Stopped(keys.stop())),
            false => Ok(()),
        }
    };
    stopped().await?;
    // Written here, put in the store, and gone.
    let gone = |path: &Path| {
        let _ = std::fs::remove_file(path);
    };
    let pack = keys.pack(lane.pack);
    if order.bake_again || !store.exists(&pack).await? {
        let baking = scratch.join(format!("{}.tuilepack", lane.pack));
        let (frames, out, found) = (lane.pack, baking.clone(), baking.clone());
        let baked = attempt(
            run,
            Step::Bake,
            lane.pack,
            move |host| {
                let _ = std::fs::remove_file(&out);
                host.bake(frames, &out)
            },
            move || match std::fs::metadata(&found) {
                Ok(m) if m.len() > 0 => Ok(()),
                Ok(_) => Err("an empty pack".into()),
                Err(e) => Err(format!("no pack: {e}")),
            },
        )
        .await;
        if !matches!(baked, Ok(Some(_))) {
            gone(&baking);
            return baked.map(|_| wanted);
        }
        // At its key only now: a pack found there is a pack baked whole.
        let put = store.put(&baking, &pack).await;
        gone(&baking);
        put?;
    }
    let mut missing = Vec::new();
    for part in wanted {
        stopped().await?;
        let segment = scratch.join(format!("{part}.mp4"));
        let (from, out, found) = (pack.clone(), segment.clone(), segment.clone());
        let rendered = attempt(
            run,
            Step::Render,
            part,
            move |host| {
                let _ = std::fs::remove_file(&out);
                host.render(part, &from, &out)
            },
            move || match concat::count_frames(&found) {
                Ok(frames) if frames == part.frames() => Ok(()),
                Ok(frames) => Err(format!("{frames} frames of {}", part.frames())),
                Err(e) => Err(e.to_string()),
            },
        )
        .await;
        let Ok(Some((tries, seconds))) = rendered else {
            gone(&segment);
            rendered?;
            missing.push(part);
            continue;
        };
        // The segment, then the receipt that vouches for it: a receipt is
        // never there before what it names.
        let put = store.put(&segment, &keys.segment(part)).await;
        gone(&segment);
        let receipt = Receipt {
            frames: part,
            bytes: put?,
            tries,
            seconds,
        };
        let json =
            serde_json::to_vec_pretty(&receipt).map_err(|e| ProduceError::Film(e.to_string()))?;
        put_bytes(
            store,
            scratch,
            &format!("{part}.json"),
            &json,
            &keys.receipt(part),
        )
        .await?;
    }
    Ok(missing)
}

/// Puts `bytes` at `key`, by way of a scratch file that does not outlive it.
async fn put_bytes(
    store: &dyn RunStore,
    scratch: &Path,
    name: &str,
    bytes: &[u8],
    key: &str,
) -> Result<(), ProduceError> {
    let path = scratch.join(name);
    std::fs::write(&path, bytes).map_err(|source| ProduceError::Io {
        path: path.clone(),
        source,
    })?;
    let put = store.put(&path, key).await;
    let _ = std::fs::remove_file(&path);
    put?;
    Ok(())
}

/// The film an order made.
#[derive(Debug, Clone, PartialEq)]
pub struct Film {
    /// Its key in the store.
    pub key: String,
    pub frames: u64,
    pub bytes: u64,
    pub parts: usize,
    pub seconds: f64,
}

/// Produces the film of `order` in `store`: bakes what is not baked, renders
/// what is not rendered, and joins the parts once all are there.
///
/// Stopped by an object named `STOP` under the film's prefix: nothing new is
/// begun, what is running ends, and the same order asked again goes on from
/// there.
pub async fn produce(
    order: &Order,
    store: &dyn RunStore,
    host: Arc<dyn Host>,
    journal: &Journal,
) -> Result<Film, ProduceError> {
    let lanes = plan(order)?;
    let keys = Keys::of(host.as_ref());
    let parts: Vec<Stretch> = lanes.iter().flat_map(|l| l.parts.iter().copied()).collect();
    let done = done(store, &keys, &parts).await?;
    journal.say(&format!(
        "frames {}: {} packs under {}, {} parts under {} ({}), {} at once, {} tries each",
        order.frames,
        lanes.len(),
        keys.packs,
        parts.len(),
        keys.film,
        match order.render_again {
            true => format!("{} already done, rendered again", done.len()),
            false => format!("{} already done", done.len()),
        },
        order.at_once.max(1),
        order.tries.max(1)
    ));
    let scratch = tempfile::tempdir().map_err(|source| ProduceError::Io {
        path: std::env::temp_dir(),
        source,
    })?;
    let made = lanes_then_film(
        order,
        store,
        &host,
        journal,
        &keys,
        &done,
        scratch.path(),
        &lanes,
    )
    .await;
    // Whatever came of it, what was said is beside the film.
    let said = journal.text();
    if let Err(e) = put_bytes(
        store,
        scratch.path(),
        "journal.log",
        said.as_bytes(),
        &keys.journal(order.task),
    )
    .await
    {
        eprintln!("the journal was not put in the store: {e}");
    }
    made
}

#[allow(clippy::too_many_arguments)]
async fn lanes_then_film(
    order: &Order,
    store: &dyn RunStore,
    host: &Arc<dyn Host>,
    journal: &Journal,
    keys: &Keys,
    done: &[Stretch],
    scratch: &Path,
    lanes: &[Lane],
) -> Result<Film, ProduceError> {
    let parts: Vec<Stretch> = lanes.iter().flat_map(|l| l.parts.iter().copied()).collect();
    let mut missing = Vec::new();
    let mut stopped = None;
    let run = Run {
        order,
        store,
        host,
        journal,
        keys,
        done,
        scratch,
    };
    // This task's share, in order; every lane when it is the only task.
    let mine = lanes.iter().enumerate().filter(|(at, _)| match order.task {
        Some((index, count)) => *at as u32 % count == index,
        None => true,
    });
    let mut running = futures_util::stream::iter(mine.map(|(_, lane)| lane))
        .map(|lane| work(&run, lane))
        .buffer_unordered(order.at_once.max(1));
    while let Some(ended) = running.next().await {
        match ended {
            Ok(some) => missing.extend(some),
            // The lanes already running end by themselves; none is begun.
            Err(ProduceError::Stopped(by)) => stopped = Some(by),
            Err(e) => return Err(e),
        }
    }
    drop(running);
    if let Some(by) = stopped {
        journal.say(&format!("stopped: {by} is there"));
        return Err(ProduceError::Stopped(by));
    }
    if !missing.is_empty() {
        missing.sort();
        let e = ProduceError::Missing {
            missing,
            parts: parts.len(),
        };
        journal.say(&e.to_string());
        return Err(e);
    }
    // One task of several: the film is joined by whichever finds every part
    // there — the parts' receipts, read again now, not what this task did.
    if order.task.is_some() {
        let now = self::done(store, keys, &parts).await?;
        if now.len() < parts.len() {
            let e = ProduceError::Waiting {
                done: now.len(),
                of: parts.len(),
            };
            journal.say(&e.to_string());
            return Err(e);
        }
    }

    // Every part is in the store: brought here, joined, and the film put.
    let fetched: Vec<PathBuf> = futures_util::stream::iter(parts.iter().copied())
        .map(|part| async move {
            let local = scratch.join(format!("join-{part}.mp4"));
            store.get(&keys.segment(part), &local).await?;
            Ok::<_, StoreError>(local)
        })
        .buffered(crate::assemble_segments())
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<_, _>>()?;
    let joining = scratch.join("film.mp4");
    let film = |e: String| ProduceError::Film(e);
    let (segments, joined) = (fetched, joining.clone());
    let probe = tokio::task::spawn_blocking(move || {
        concat::concat(&segments, &joined)?;
        concat::probe(&joined)
    })
    .await
    .map_err(|e| film(e.to_string()))?
    .map_err(|e| film(e.to_string()))?;
    if probe.frames != order.frames.frames() {
        return Err(film(format!(
            "{} frames joined of {}",
            probe.frames,
            order.frames.frames()
        )));
    }
    if let Some(fps) = order.fps {
        concat::check_cadence(probe, fps).map_err(film)?;
    }
    let key = keys.film();
    let bytes = store.put(&joining, &key).await?;
    journal.say(&format!(
        "film: {key} — {} frames, {:.1} s, {} parts, {:.1} MB",
        probe.frames,
        probe.seconds,
        parts.len(),
        bytes as f64 / 1e6
    ));
    Ok(Film {
        key,
        frames: probe.frames,
        bytes,
        parts: parts.len(),
        seconds: probe.seconds,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ObjectRunStore, Tuning};

    const SPS: &[u8] = &[0x67, 0x64, 0x00, 0x0a, 0xac, 0xd9, 0x5e];

    fn order(frames: (u32, u32)) -> Order {
        Order {
            pack_frames: 30,
            part_frames: 10,
            ..Order::of(Stretch {
                first: frames.0,
                last: frames.1,
            })
        }
    }

    type Bake = Box<dyn Fn(Stretch, &Path) -> Result<(), Failed> + Send + Sync>;
    type Render = Box<dyn Fn(Stretch, &Path) -> Result<(), Failed> + Send + Sync>;

    /// A host that writes a pack of four bytes and a segment of the part's
    /// frames, says what it was asked, and can be told to do otherwise.
    struct Told {
        asked: Mutex<Vec<String>>,
        packs: String,
        film: String,
        bake: Bake,
        render: Render,
    }

    fn segment(frames: u64, out: &Path) -> Result<(), Failed> {
        concat::write_test_segment(out, frames as u32, SPS, 1)
            .map_err(|e| Failed::once(e.to_string()))
    }

    impl Told {
        fn new() -> Self {
            Self {
                asked: Mutex::new(Vec::new()),
                packs: "packs/scene-a".into(),
                film: "films/scene-a/look-1".into(),
                bake: Box::new(|_, out| {
                    std::fs::write(out, b"pack").map_err(|e| Failed::once(e.to_string()))
                }),
                render: Box::new(|frames, out| segment(frames.frames(), out)),
            }
        }

        fn asked(&self, what: &str) -> Vec<String> {
            let asked = self.asked.lock().expect("lock");
            asked
                .iter()
                .filter_map(|a| a.strip_prefix(what))
                .map(String::from)
                .collect()
        }
    }

    impl Host for Told {
        fn packs(&self) -> String {
            self.packs.clone()
        }
        fn film(&self) -> String {
            self.film.clone()
        }
        fn bake(&self, frames: Stretch, out: &Path) -> Result<(), Failed> {
            self.asked
                .lock()
                .expect("lock")
                .push(format!("bake {frames}"));
            (self.bake)(frames, out)
        }
        fn render(&self, frames: Stretch, pack: &str, out: &Path) -> Result<(), Failed> {
            self.asked
                .lock()
                .expect("lock")
                .push(format!("render {frames} from {pack}"));
            (self.render)(frames, out)
        }
    }

    /// A store on a directory: the same reads and writes as a bucket's.
    struct World {
        root: tempfile::TempDir,
        store: ObjectRunStore,
        journal: Journal,
    }

    impl World {
        fn new() -> Self {
            let root = tempfile::tempdir().expect("dir");
            let store = ObjectRunStore::local(root.path(), Tuning::default()).expect("store");
            Self {
                root,
                store,
                journal: Journal::default(),
            }
        }

        async fn made(&self, order: &Order, host: &Arc<Told>) -> Result<Film, ProduceError> {
            produce(order, &self.store, host.clone(), &self.journal).await
        }

        fn at(&self, key: &str) -> PathBuf {
            self.root.path().join(key)
        }

        /// Every key in the store, sorted.
        async fn keys(&self) -> Vec<String> {
            self.store
                .list("")
                .await
                .expect("list")
                .into_iter()
                .map(|e| e.key)
                .collect()
        }
    }

    fn missing(failed: &ProduceError) -> Vec<(u32, u32)> {
        match failed {
            ProduceError::Missing { missing, .. } => {
                missing.iter().map(|m| (m.first, m.last)).collect()
            }
            _ => Vec::new(),
        }
    }

    #[test]
    fn parts_cover_the_frames_once_and_never_straddle_two_packs() {
        let lanes = plan(&Order {
            pack_frames: 150,
            part_frames: 50,
            ..order((1, 18000))
        })
        .expect("plan");
        assert_eq!(lanes.len(), 120);
        assert_eq!(
            lanes[0].pack,
            Stretch {
                first: 1,
                last: 150
            }
        );
        assert_eq!(
            lanes[0].parts,
            [(1, 50), (51, 100), (101, 150)].map(|(first, last)| Stretch { first, last })
        );
        assert_eq!(
            lanes[119].pack,
            Stretch {
                first: 17851,
                last: 18000
            }
        );

        // Sizes that divide nothing: every frame still once, in order, and
        // each part within its pack.
        let lanes = plan(&Order {
            pack_frames: 7,
            part_frames: 3,
            ..order((5, 30))
        })
        .expect("plan");
        let mut next = 5;
        for lane in &lanes {
            assert!(lane.pack.frames() <= 7);
            assert_eq!(lane.parts.first().map(|p| p.first), Some(lane.pack.first));
            assert_eq!(lane.parts.last().map(|p| p.last), Some(lane.pack.last));
            for part in &lane.parts {
                assert_eq!(part.first, next);
                assert!(part.frames() <= 3 && part.last <= lane.pack.last);
                next = part.last + 1;
            }
        }
        assert_eq!(next, 31);
        // A part larger than a pack is the pack.
        let lanes = plan(&Order {
            pack_frames: 4,
            part_frames: 9,
            ..order((1, 8))
        })
        .expect("plan");
        assert_eq!(lanes[1].parts, [Stretch { first: 5, last: 8 }]);
        // The last frame there is does not overflow.
        let top = u32::MAX;
        let lanes = plan(&order((top - 3, top))).expect("plan");
        assert_eq!(
            lanes[0].parts,
            [Stretch {
                first: top - 3,
                last: top
            }]
        );

        assert!(plan(&order((9, 3))).is_err());
        assert!(plan(&Order {
            part_frames: 0,
            ..order((1, 9))
        })
        .is_err());
    }

    #[test]
    fn an_order_left_as_it_is_cuts_a_pack_and_a_part_of_fifteen_seconds_each() {
        // Measured on a 4K film: a part pays some eighty seconds before its
        // first frame, then a seventh of a second a frame. At 450 frames
        // that start is two fifths of the part; at 50 it was six sevenths.
        let order = Order::of(Stretch {
            first: 1,
            last: 18000,
        });
        assert_eq!(
            (
                order.pack_frames,
                order.part_frames,
                order.at_once,
                order.tries
            ),
            (450, 450, 1, 3)
        );
        assert!(!order.bake_again && !order.render_again);
        let lanes = plan(&order).expect("plan");
        assert_eq!(lanes.len(), 40);
        assert!(lanes.iter().all(|lane| lane.parts == [lane.pack]));
    }

    #[tokio::test]
    async fn a_film_is_baked_rendered_and_joined_in_the_store_and_nowhere_else() {
        let world = World::new();
        let host = Arc::new(Told::new());
        // 55 frames: two packs (30, 25), parts of 10, 10, 10 | 10, 10, 5.
        let film = world.made(&order((101, 155)), &host).await.expect("film");
        assert_eq!((film.frames, film.parts), (55, 6));
        assert_eq!(film.key, "films/scene-a/look-1/film.mp4");
        assert_eq!(
            concat::count_frames(&world.at(&film.key)).expect("film"),
            55
        );
        assert_eq!(host.asked("bake "), ["101-130", "131-155"]);
        assert_eq!(
            host.asked("render "),
            [
                "101-110 from packs/scene-a/101-130.tuilepack",
                "111-120 from packs/scene-a/101-130.tuilepack",
                "121-130 from packs/scene-a/101-130.tuilepack",
                "131-140 from packs/scene-a/131-155.tuilepack",
                "141-150 from packs/scene-a/131-155.tuilepack",
                "151-155 from packs/scene-a/131-155.tuilepack",
            ]
        );
        // Two packs; six segments and their receipts, the film, the journal.
        let keys = world.keys().await;
        assert_eq!(
            keys.iter()
                .filter(|k| k.starts_with("packs/scene-a/"))
                .count(),
            2
        );
        assert_eq!(
            keys.iter()
                .filter(|k| k.starts_with("films/scene-a/look-1/parts/"))
                .count(),
            12
        );
        assert_eq!(keys.len(), 2 + 12 + 2, "{keys:?}");
        let said =
            std::fs::read_to_string(world.at("films/scene-a/look-1/journal.log")).expect("journal");
        assert!(
            said.contains("film: films/scene-a/look-1/film.mp4"),
            "{said}"
        );
    }

    #[tokio::test]
    async fn what_is_done_is_not_done_again_unless_the_order_says_so() {
        let world = World::new();
        let order = order((1, 40));
        world
            .made(&order, &Arc::new(Told::new()))
            .await
            .expect("film");

        // Asked again: the film is joined from what is there, and nothing is
        // asked of the host.
        let idle = Arc::new(Told::new());
        assert_eq!(world.made(&order, &idle).await.expect("film").frames, 40);
        assert!(idle.asked("").is_empty(), "{:?}", idle.asked(""));

        // One segment lost: that part alone is rendered again, from the pack
        // that is there.
        std::fs::remove_file(world.at("films/scene-a/look-1/parts/21-30.mp4")).expect("remove");
        world.made(&order, &idle).await.expect("film");
        assert_eq!(
            idle.asked(""),
            ["render 21-30 from packs/scene-a/1-30.tuilepack"]
        );

        // Told to render again: every part, and no pack.
        let again = Arc::new(Told::new());
        world
            .made(
                &Order {
                    render_again: true,
                    ..order.clone()
                },
                &again,
            )
            .await
            .expect("film");
        assert_eq!(again.asked("render ").len(), 4);
        assert!(again.asked("bake ").is_empty());

        // Told to bake again: nothing, since no part needs a pack…
        let again = Arc::new(Told::new());
        world
            .made(
                &Order {
                    bake_again: true,
                    ..order.clone()
                },
                &again,
            )
            .await
            .expect("film");
        assert!(again.asked("").is_empty());
        // …and both packs when the parts are rendered again too.
        let both = Order {
            bake_again: true,
            render_again: true,
            ..order.clone()
        };
        world.made(&both, &again).await.expect("film");
        assert_eq!(again.asked("bake "), ["1-30", "31-40"]);
    }

    #[tokio::test]
    async fn other_parameters_are_other_keys_and_share_nothing_they_should_not() {
        let world = World::new();
        let order = order((1, 30));
        world
            .made(&order, &Arc::new(Told::new()))
            .await
            .expect("film");

        // Another look over the same bake: its parts are rendered, from the
        // pack already there.
        let look = Arc::new(Told {
            film: "films/scene-a/look-2".into(),
            ..Told::new()
        });
        let film = world.made(&order, &look).await.expect("film");
        assert_eq!(film.key, "films/scene-a/look-2/film.mp4");
        assert!(look.asked("bake ").is_empty());
        assert_eq!(look.asked("render ").len(), 3);

        // Another bake: nothing of the first one is taken for it.
        let scene = Arc::new(Told {
            packs: "packs/scene-b".into(),
            film: "films/scene-b/look-1".into(),
            ..Told::new()
        });
        world.made(&order, &scene).await.expect("film");
        assert_eq!(scene.asked("bake "), ["1-30"]);
        assert_eq!(scene.asked("render ").len(), 3);
    }

    #[tokio::test]
    async fn a_part_that_fails_is_tried_again_and_one_that_never_ends_well_is_named() {
        let world = World::new();
        let once = Mutex::new(false);
        let host = Arc::new(Told {
            // Part 11-20 fails its first try; part 21-30 always does.
            render: Box::new(move |frames, out| match frames.first {
                21 => Err(Failed::once("no")),
                11 if !std::mem::replace(&mut *once.lock().expect("lock"), true) => {
                    Err(Failed::once("not yet"))
                }
                _ => segment(frames.frames(), out),
            }),
            ..Told::new()
        });
        let order = order((1, 40));

        let failed = world.made(&order, &host).await.expect_err("a hole");
        assert_eq!(missing(&failed), [(21, 30)], "{failed}");
        // No film with a hole in it; the other parts are done all the same,
        // the second lane's too.
        assert!(!world.at("films/scene-a/look-1/film.mp4").exists());
        let renders: Vec<String> = host
            .asked("render ")
            .iter()
            .map(|r| r.split(' ').next().unwrap_or_default().to_string())
            .collect();
        assert_eq!(renders.iter().filter(|r| *r == "11-20").count(), 2);
        assert_eq!(renders.iter().filter(|r| *r == "21-30").count(), 3);
        assert_eq!(renders.iter().filter(|r| *r == "31-40").count(), 1);
        // What was said is in the store all the same.
        let said =
            std::fs::read_to_string(world.at("films/scene-a/look-1/journal.log")).expect("journal");
        assert!(said.contains("1 of 4 parts are missing"), "{said}");

        // The fault gone, the same order renders the hole alone and joins.
        let mended = Arc::new(Told::new());
        assert_eq!(world.made(&order, &mended).await.expect("film").frames, 40);
        assert_eq!(
            mended.asked(""),
            ["render 21-30 from packs/scene-a/1-30.tuilepack"]
        );
    }

    #[tokio::test]
    async fn a_failure_another_try_would_repeat_is_tried_once() {
        let world = World::new();
        let host = Arc::new(Told {
            render: Box::new(|_, _| Err(Failed::for_good("tiles the store lacks"))),
            ..Told::new()
        });
        let failed = world
            .made(&order((1, 10)), &host)
            .await
            .expect_err("for good");
        assert_eq!(missing(&failed), [(1, 10)]);
        assert_eq!(host.asked("render ").len(), 1);
        assert!(world
            .journal
            .text()
            .contains("tiles the store lacks — not tried again"));
    }

    #[tokio::test]
    async fn a_segment_short_of_its_frames_is_not_a_part_done() {
        let world = World::new();
        // Ends well, with seven frames where ten were asked.
        let host = Arc::new(Told {
            render: Box::new(|_, out| segment(7, out)),
            ..Told::new()
        });
        let failed = world
            .made(
                &Order {
                    tries: 2,
                    ..order((1, 10))
                },
                &host,
            )
            .await
            .expect_err("short");
        assert_eq!(missing(&failed), [(1, 10)], "{failed}");
        assert_eq!(host.asked("render ").len(), 2);
        assert!(
            world.keys().await.iter().all(|k| !k.contains("/parts/")),
            "{:?}",
            world.keys().await
        );
        assert!(world.journal.text().contains("7 frames of 10"));
    }

    #[tokio::test]
    async fn a_bake_that_fails_leaves_no_pack_at_its_key_and_its_parts_missing() {
        let world = World::new();
        let host = Arc::new(Told {
            bake: Box::new(|_, out| {
                std::fs::write(out, b"half").map_err(|e| Failed::once(e.to_string()))?;
                Err(Failed::once("the source went away"))
            }),
            ..Told::new()
        });
        let failed = world
            .made(
                &Order {
                    tries: 2,
                    ..order((1, 30))
                },
                &host,
            )
            .await
            .expect_err("no pack");
        assert_eq!(missing(&failed), [(1, 10), (11, 20), (21, 30)], "{failed}");
        assert_eq!(host.asked("bake ").len(), 2);
        assert!(host.asked("render ").is_empty());
        assert_eq!(world.keys().await, ["films/scene-a/look-1/journal.log"]);
    }

    #[tokio::test]
    async fn a_step_that_panics_fails_its_part_and_not_the_production() {
        let world = World::new();
        let host = Arc::new(Told {
            render: Box::new(|frames, out| {
                assert!(frames.first != 11, "the renderer gave up");
                segment(frames.frames(), out)
            }),
            ..Told::new()
        });
        let failed = world
            .made(
                &Order {
                    tries: 1,
                    ..order((1, 30))
                },
                &host,
            )
            .await
            .expect_err("a hole");
        assert_eq!(missing(&failed), [(11, 20)], "{failed}");
        assert_eq!(host.asked("render ").len(), 3);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn lanes_at_once_run_together_and_give_the_same_film() {
        let world = World::new();
        // Each bake waits until three are in at once: with fewer lanes than
        // that, none would ever return.
        let gate = Arc::new(std::sync::Barrier::new(3));
        let host = Arc::new(Told {
            bake: Box::new(move |_, out| {
                gate.wait();
                std::fs::write(out, b"pack").map_err(|e| Failed::once(e.to_string()))
            }),
            ..Told::new()
        });
        let together = Order {
            at_once: 3,
            step_timeout: Some(Duration::from_secs(20)),
            ..order((1, 90))
        };
        let film = world.made(&together, &host).await.expect("film");
        assert_eq!((film.frames, film.parts), (90, 9));

        let alone = World::new();
        let one = alone
            .made(&order((1, 90)), &Arc::new(Told::new()))
            .await
            .expect("film");
        assert_eq!(
            std::fs::read(world.at(&film.key)).expect("film"),
            std::fs::read(alone.at(&one.key)).expect("film")
        );
    }

    #[tokio::test]
    async fn a_step_that_does_not_return_stops_the_production_and_says_which() {
        let world = World::new();
        let host = Arc::new(Told {
            render: Box::new(|frames, out| {
                if frames.first == 11 {
                    std::thread::sleep(Duration::from_secs(3));
                }
                segment(frames.frames(), out)
            }),
            ..Told::new()
        });
        let order = Order {
            step_timeout: Some(Duration::from_millis(300)),
            ..order((1, 30))
        };
        let stalled = world.made(&order, &host).await.expect_err("stalled");
        assert!(
            matches!(stalled, ProduceError::Stalled { step: "render", frames, .. } if frames.first == 11),
            "{stalled}"
        );
        // What was done before it is kept.
        assert!(world.at("films/scene-a/look-1/parts/1-10.json").exists());
        assert!(!world.at("films/scene-a/look-1/film.mp4").exists());
    }

    #[tokio::test]
    async fn tasks_of_one_production_share_its_packs_and_the_last_to_finish_joins() {
        let world = World::new();
        // 90 frames: three packs of 30, parts of 10; two tasks.
        let whole = order((1, 90));
        let first = Arc::new(Told::new());
        let second = Arc::new(Told::new());

        // Task 0 takes packs 0 and 2, task 1 pack 1.
        let waiting = world
            .made(
                &Order {
                    task: Some((0, 2)),
                    ..whole.clone()
                },
                &first,
            )
            .await
            .expect_err("not all parts are there yet");
        assert!(
            matches!(waiting, ProduceError::Waiting { done: 6, of: 9 }),
            "{waiting}"
        );
        assert_eq!(first.asked("bake "), ["1-30", "61-90"]);
        assert!(!world.at("films/scene-a/look-1/film.mp4").exists());

        let film = world
            .made(
                &Order {
                    task: Some((1, 2)),
                    ..whole.clone()
                },
                &second,
            )
            .await
            .expect("the last joins");
        assert_eq!((film.frames, film.parts), (90, 9));
        assert_eq!(second.asked("bake "), ["31-60"]);
        assert_eq!(second.asked("render ").len(), 3);
        // Each task said what it did in a journal of its own.
        for t in 0..2 {
            assert!(world
                .at(&format!("films/scene-a/look-1/journal-t{t}.log"))
                .exists());
        }

        // Whichever order they finish in: the first to end joins nothing.
        let other = World::new();
        other
            .made(
                &Order {
                    task: Some((1, 2)),
                    ..whole.clone()
                },
                &Arc::new(Told::new()),
            )
            .await
            .expect_err("waiting");
        let film = other
            .made(
                &Order {
                    task: Some((0, 2)),
                    ..whole.clone()
                },
                &Arc::new(Told::new()),
            )
            .await
            .expect("joined");
        assert_eq!(film.frames, 90);
        // More tasks than packs: those with none join if they find it all done.
        let idle = Arc::new(Told::new());
        let film = other
            .made(
                &Order {
                    task: Some((5, 6)),
                    ..whole
                },
                &idle,
            )
            .await
            .expect("all done already");
        assert_eq!(film.frames, 90);
        assert!(idle.asked("").is_empty());
    }

    #[tokio::test]
    async fn a_stop_object_begins_nothing_and_the_order_goes_on_once_it_is_gone() {
        let world = World::new();
        let host = Arc::new(Told::new());
        let stop = world.at("films/scene-a/look-1/STOP");
        std::fs::create_dir_all(stop.parent().expect("parent")).expect("dir");
        std::fs::write(&stop, "").expect("stop");
        let stopped = world
            .made(&order((1, 30)), &host)
            .await
            .expect_err("stopped");
        assert!(matches!(stopped, ProduceError::Stopped(_)), "{stopped}");
        assert!(host.asked("").is_empty());
        std::fs::remove_file(&stop).expect("remove");
        assert_eq!(
            world
                .made(&order((1, 30)), &host)
                .await
                .expect("film")
                .frames,
            30
        );
    }
}
