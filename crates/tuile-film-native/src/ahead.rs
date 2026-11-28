// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! Reading ahead: what the next frames need of the store, made before they
//! need it.
//!
//! The pack is the plan: it says which tiles each frame enters, a whole
//! film ahead, for the price of reading its table. Each entering tile needs
//! its mesh — built from its terrain tile's bytes, or cut out of an
//! ancestor's — and the imagery tiles of its drape, decoded and laid on the
//! terrain's spacing. Those are pure functions of the store's bytes
//! ([`tuile_film::from_store`]), and they are what a frame waits on before
//! it can be drawn: so they are started, in the order the render will take
//! them — frame by frame, a tile's drape together with its mesh — on
//! threads of their own, a few frames before the render reaches them.
//!
//! What is made ahead is exactly what the render would have made when it
//! needed it: the same functions of the same bytes. Only when it is made
//! changes. The GPU is handed it on the render's thread, in the render's
//! order, as before; anything not ready in time is waited for, and anything
//! that failed, or was never started, is made the old way, there and then.
//!
//! Bounded twice: by frames ([`ReadAhead::frames`]) and by the bytes of
//! what was made and not yet taken ([`ReadAhead::bytes`]).

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Instant;

use futures_util::StreamExt;
use tuile_core::content::DecodedTexture;
use tuile_core::raster::TilingScheme;
use tuile_film::from_store::{imagery_texture, is_baked, terrain_mesh};
use tuile_film::{fb, refs_of, Mesh, Pack, StoreTile, TileKey, TileRefs};
use tuile_repository::{ArchivedTiles, TileRepository};

use crate::observe::{Ahead as Took, Timings};
use crate::Error;

/// How far ahead of the frame being drawn the render makes what the next
/// frames need. Numbers to tune, never a way to turn it off: a window of no
/// frame still makes the frame's own tiles side by side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadAhead {
    /// Frames of the plan after the one being drawn whose tiles are started.
    pub frames: u32,
    /// What was made ahead and not taken yet, at most, in bytes: each
    /// mesh's and decoded imagery tile's as it is started (an estimate) and
    /// then as it came out.
    pub bytes: u64,
    /// Threads that make it; `0`, as many as the machine has cores but one.
    pub workers: u32,
}

impl Default for ReadAhead {
    /// A few frames: a frame of a film is drawn in tens of milliseconds, a
    /// terrain tile is made in about one and an imagery tile decoded in two
    /// or three, so a frame's entering tiles are made, across the cores,
    /// well within the frames before it.
    fn default() -> Self {
        Self {
            frames: 4,
            bytes: 512 << 20,
            workers: 0,
        }
    }
}

type Job = Box<dyn FnOnce() + Send>;

/// The threads that make what is read ahead, taking it in the order it was
/// asked for.
pub(crate) struct Workers {
    queue: Option<mpsc::Sender<Job>>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Workers {
    pub(crate) fn new(asked: u32) -> Result<Self, Error> {
        let count = match asked {
            0 => std::thread::available_parallelism()
                .map_or(1, |n| n.get().saturating_sub(1))
                .max(1),
            n => n as usize,
        };
        let (queue, taken) = mpsc::channel::<Job>();
        let taken = Arc::new(Mutex::new(taken));
        let mut threads = Vec::with_capacity(count);
        for n in 0..count {
            let taken = taken.clone();
            threads.push(
                std::thread::Builder::new()
                    .name(format!("film-ahead-{n}"))
                    .spawn(move || loop {
                        let next = match taken.lock() {
                            Ok(taken) => taken.recv(),
                            Err(_) => return,
                        };
                        match next {
                            Ok(job) => job(),
                            Err(_) => return,
                        }
                    })?,
            );
        }
        Ok(Self {
            queue: Some(queue),
            threads,
        })
    }

    fn run(&self, job: Job) {
        if let Some(queue) = &self.queue {
            // Every thread gone: the receiver of the job's result hears
            // nothing, and the render makes it itself.
            let _ = queue.send(job);
        }
    }
}

impl Drop for Workers {
    fn drop(&mut self) {
        self.queue = None;
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

/// A tile's mesh, made ahead, and whether its terrain tile is not the one
/// baked.
pub(crate) struct MeshReady {
    pub mesh: Mesh,
    pub renewed: bool,
}

/// An imagery tile, read and decoded ahead.
pub(crate) struct ImageryReady {
    /// The reference it was decoded for.
    pub tile: StoreTile,
    /// As the store holds it.
    pub bytes: Vec<u8>,
    pub decoded: DecodedTexture,
    /// Every texel opaque.
    pub opaque: bool,
}

/// What a job sends: what it made, and the bytes it holds.
type Made<T> = (T, i64);

/// Something started ahead.
struct Pending<T> {
    /// The frame it was started for, and the one being drawn when it was.
    due: u32,
    started: u32,
    made: tokio::sync::oneshot::Receiver<Made<T>>,
}

/// A tile the plan says enters, and what it is built from.
struct Need {
    frame: u32,
    key: TileKey,
    id: u64,
    refs: TileRefs,
    factor: [f32; 4],
}

/// The colour factor of a tile as the pack gives it.
pub(crate) fn factor_of(tile: &fb::Tile<'_>) -> [f32; 4] {
    match tile.base_color_factor() {
        Some(v) if v.len() == 4 => [v.get(0), v.get(1), v.get(2), v.get(3)],
        _ => [1.0; 4],
    }
}

/// What a decoded imagery tile and a mesh hold, in bytes, before they are
/// made: an imagery tile of the usual 256 texels a side, and a mesh a
/// few times the bytes it is decoded from.
const IMAGERY_GUESS: i64 = 256 * 256 * 4;
const MESH_PER_BYTE: i64 = 8;

/// Store tiles read at once.
const AT_ONCE: usize = 48;

fn mesh_bytes(mesh: &Mesh) -> i64 {
    (mesh.positions.len() + mesh.normals.len() + mesh.uvs.len() + mesh.indices.len()) as i64
}

/// The store's tiles of one pack, made ahead of its frames.
pub(crate) struct Lookahead<'w> {
    workers: &'w Workers,
    settings: ReadAhead,
    /// The pack's layers and the imagery's grid; `None` for a pack that
    /// carries its tiles.
    layers: Option<(String, String, TilingScheme)>,
    /// The frames rendered, in order; those whose needs are known; and
    /// where the frame being drawn is among them.
    plan: Vec<u32>,
    planned: usize,
    drawing: usize,
    /// What the plan says is drawn by the last frame planned.
    drawn: HashSet<TileKey>,
    /// Needs known and not yet started, in the order of need.
    needs: VecDeque<Need>,
    meshes: HashMap<TileKey, VecDeque<Pending<MeshReady>>>,
    imagery: HashMap<(u8, u32, u32), VecDeque<Pending<ImageryReady>>>,
    /// The bytes of what was started and not taken.
    held: Arc<AtomicI64>,
}

impl<'w> Lookahead<'w> {
    /// Ahead of `plan`, the frames of `pack` to be rendered in that order,
    /// from what is resident before the first of them.
    pub(crate) fn new(
        workers: &'w Workers,
        settings: ReadAhead,
        pack: &Pack<'_>,
        scheme: Option<TilingScheme>,
        plan: Vec<u32>,
        resident: impl IntoIterator<Item = TileKey>,
    ) -> Self {
        let layers = match (pack.content(), pack.store_layers(), scheme) {
            (tuile_film::Content::Embedded, _, _) => None,
            (_, Some((terrain, imagery)), Some(scheme)) => {
                Some((terrain.to_string(), imagery.to_string(), scheme))
            }
            _ => None,
        };
        Self {
            workers,
            settings,
            layers,
            plan,
            planned: 0,
            drawing: 0,
            drawn: resident.into_iter().collect(),
            needs: VecDeque::new(),
            meshes: HashMap::new(),
            imagery: HashMap::new(),
            held: Arc::new(AtomicI64::new(0)),
        }
    }

    /// The frame about to be drawn: the window moves to it.
    pub(crate) fn begin(&mut self, frame: u32) {
        if let Some(at) = self.plan[self.drawing..].iter().position(|f| *f == frame) {
            self.drawing += at;
        }
    }

    fn frame_now(&self) -> u32 {
        self.plan.get(self.drawing).copied().unwrap_or(u32::MAX)
    }

    /// Starts what the next frames need, in the order they need it, as far
    /// as the window and the bytes allow. `on_gpu` says which imagery tiles
    /// are resident already, and are not decoded again.
    pub(crate) async fn top_up(
        &mut self,
        pack: &Pack<'_>,
        store: &ArchivedTiles,
        on_gpu: &dyn Fn(&(u8, u32, u32)) -> bool,
        timings: &mut Timings,
    ) -> Result<(), Error> {
        if self.layers.is_none() {
            return Ok(());
        }
        loop {
            if self.held.load(Ordering::Relaxed) >= self.settings.bytes as i64 {
                return Ok(());
            }
            let Some(need) = self.needs.pop_front() else {
                if self.planned >= self.plan.len()
                    || self.planned > self.drawing + self.settings.frames as usize
                {
                    return Ok(());
                }
                self.plan_next(pack)?;
                continue;
            };
            self.start(need, store, on_gpu, timings).await;
        }
    }

    /// The needs of the next frame of the plan: the tiles it draws that the
    /// frame before it did not, in the order the render enters them.
    fn plan_next(&mut self, pack: &Pack<'_>) -> Result<(), Error> {
        let frame = self.plan[self.planned];
        self.planned += 1;
        let mut now = HashSet::new();
        for tile in pack.frame(frame)? {
            let key = TileKey::of(&tile);
            if !now.insert(key) || self.drawn.contains(&key) {
                continue;
            }
            if let Some(refs) = refs_of(&tile) {
                self.needs.push_back(Need {
                    frame,
                    key,
                    id: tile.id(),
                    refs,
                    factor: factor_of(&tile),
                });
            }
        }
        self.drawn = now;
        Ok(())
    }

    /// Reads what `need` is built from and hands its making to the workers:
    /// its mesh, and each imagery tile of its drape not resident or started
    /// already. A tile the store does not answer is not started: the render
    /// asks for it again when it needs it, and says what is wrong.
    async fn start(
        &mut self,
        need: Need,
        store: &ArchivedTiles,
        on_gpu: &dyn Fn(&(u8, u32, u32)) -> bool,
        timings: &mut Timings,
    ) {
        let Some((terrain_layer, imagery_layer, scheme)) = self.layers.clone() else {
            return;
        };
        let started = self.frame_now();
        let source = need.refs.terrain;
        let t = Instant::now();
        let terrain = store
            .tile(&terrain_layer, source.level, source.x, source.y)
            .await;
        timings.read_terrain += ms(t);
        if let Ok(Some(terrain)) = terrain {
            let bytes = terrain.bytes;
            let guess = bytes.len() as i64 * MESH_PER_BYTE;
            let refs = need.refs.clone();
            let (id, factor) = (need.id, need.factor);
            let made = self.spawn(guess, move || {
                let mesh = terrain_mesh(id, &refs, &bytes, factor).ok()?;
                let held = mesh_bytes(&mesh);
                Some((
                    MeshReady {
                        mesh,
                        renewed: !is_baked(&refs.terrain, &bytes),
                    },
                    held,
                ))
            });
            self.meshes.entry(need.key).or_default().push_back(Pending {
                due: need.frame,
                started,
                made,
            });
        }

        let mut wanted = Vec::new();
        for placed in &need.refs.imagery {
            let at = (placed.tile.level, placed.tile.x, placed.tile.y);
            let started_already = self.imagery.get(&at).is_some_and(|p| !p.is_empty())
                || wanted.iter().any(|t: &StoreTile| (t.level, t.x, t.y) == at);
            if !started_already && !on_gpu(&at) {
                wanted.push(placed.tile);
            }
        }
        let t = Instant::now();
        let read: Vec<_> = futures_util::stream::iter(
            wanted
                .iter()
                .map(|tile| store.tile(&imagery_layer, tile.level, tile.x, tile.y)),
        )
        .buffered(AT_ONCE)
        .collect()
        .await;
        timings.read_imagery += ms(t);
        for (tile, found) in wanted.into_iter().zip(read) {
            let Ok(Some(found)) = found else { continue };
            let bytes = found.bytes;
            let guess = IMAGERY_GUESS + bytes.len() as i64;
            let made = self.spawn(guess, move || {
                let decoded = imagery_texture(&tile, &scheme, &bytes).ok()?;
                let rgba8 = decoded.rgba8()?;
                let opaque = rgba8.as_chunks::<4>().0.iter().all(|p| p[3] == 255);
                let held = (rgba8.len() + bytes.len()) as i64;
                Some((
                    ImageryReady {
                        tile,
                        bytes,
                        decoded,
                        opaque,
                    },
                    held,
                ))
            });
            self.imagery
                .entry((tile.level, tile.x, tile.y))
                .or_default()
                .push_back(Pending {
                    due: need.frame,
                    started,
                    made,
                });
        }
    }

    /// Hands `make` to the workers, `guess` bytes counted held until what
    /// it made is known. What it made is counted until it is taken; what
    /// nobody takes any more is let go and uncounted by the job itself.
    fn spawn<T: Send + 'static>(
        &self,
        guess: i64,
        make: impl FnOnce() -> Option<Made<T>> + Send + 'static,
    ) -> tokio::sync::oneshot::Receiver<Made<T>> {
        let (send, made) = tokio::sync::oneshot::channel();
        let held = self.held.clone();
        held.fetch_add(guess, Ordering::Relaxed);
        self.workers.run(Box::new(move || {
            let out = make();
            held.fetch_sub(guess, Ordering::Relaxed);
            if let Some((made, bytes)) = out {
                held.fetch_add(bytes, Ordering::Relaxed);
                if send.send((made, bytes)).is_err() {
                    held.fetch_sub(bytes, Ordering::Relaxed);
                }
            }
        }));
        made
    }

    /// Whether `key`'s mesh was started ahead.
    pub(crate) fn has_mesh(&self, key: &TileKey) -> bool {
        self.meshes.get(key).is_some_and(|p| !p.is_empty())
    }

    /// Whether imagery tile `at` was started ahead.
    pub(crate) fn has_imagery(&self, at: &(u8, u32, u32)) -> bool {
        self.imagery.get(at).is_some_and(|p| !p.is_empty())
    }

    /// `key`'s mesh as made ahead for `frame`, waited for if it is not made
    /// yet (the wait counted in `waited` milliseconds); `None` if it was not
    /// started, or could not be made.
    pub(crate) async fn mesh(
        &mut self,
        key: &TileKey,
        frame: u32,
        waited: &mut f64,
        took: &mut Took,
    ) -> Option<MeshReady> {
        let pending = self.meshes.get_mut(key)?.pop_front()?;
        self.take(pending, frame, waited, took).await
    }

    /// Imagery tile `tile` as decoded ahead, likewise.
    pub(crate) async fn imagery(
        &mut self,
        tile: &StoreTile,
        frame: u32,
        waited: &mut f64,
        took: &mut Took,
    ) -> Option<ImageryReady> {
        let at = (tile.level, tile.x, tile.y);
        let pending = self.imagery.get_mut(&at)?.pop_front()?;
        let ready = self.take(pending, frame, waited, took).await?;
        // Decoded for another reference to the same tile: not this one's.
        (ready.tile == *tile).then_some(ready)
    }

    async fn take<T>(
        &self,
        mut pending: Pending<T>,
        frame: u32,
        waited: &mut f64,
        took: &mut Took,
    ) -> Option<T> {
        let made = match pending.made.try_recv() {
            Ok(made) => {
                took.ready += 1;
                made
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                let t = Instant::now();
                let made = pending.made.await.ok();
                *waited += ms(t);
                // Not made: the render makes it, and counts it so.
                let made = made?;
                took.waited += 1;
                made
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => return None,
        };
        took.early += usize::from(pending.started < frame);
        self.held.fetch_sub(made.1, Ordering::Relaxed);
        Some(made.0)
    }

    /// `frame` is drawn: what was started for it, or before it, and not
    /// taken, the plan was wrong about — a tile resident already — and is
    /// let go.
    pub(crate) fn end(&mut self, frame: u32) {
        let held = &self.held;
        self.meshes.retain(|_, p| let_go(p, frame, held));
        self.imagery.retain(|_, p| let_go(p, frame, held));
    }
}

/// Lets go of what was started for `frame` or before it; whether anything
/// is left.
fn let_go<T>(pending: &mut VecDeque<Pending<T>>, frame: u32, held: &AtomicI64) -> bool {
    pending.retain_mut(|p| {
        if p.due > frame {
            return true;
        }
        // Closed first: a job that finishes after this cannot send, and
        // uncounts what it made itself.
        p.made.close();
        if let Ok((_, bytes)) = p.made.try_recv() {
            held.fetch_sub(bytes, Ordering::Relaxed);
        }
        false
    });
    !pending.is_empty()
}

fn ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1000.0
}
