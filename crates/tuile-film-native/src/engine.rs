// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The engine as a film's host: its own bake and its own render, called by
//! [`tuile_farm::produce`], with the keys their parameters give.

use std::path::Path;
use std::sync::Arc;

use tuile_bake::tape::TapeBake;
use tuile_bake::tiles::Tiles;
use tuile_farm::produce::{Failed, Host, Stretch};
use tuile_repository::Objects;

use crate::render::{render, AbsentTiles, Order};
use crate::setup::{film_sink, sun_at};
use crate::source::{Film, Sources};
use crate::Error;

/// How a film is rendered, beyond what its packs hold.
#[derive(Debug, Clone)]
pub struct Rendering {
    /// Everything of the picture: its scale against the bake's, its
    /// cadence, its tone, its look. Its `frames` are the production's.
    pub order: Order,
    /// The encoder, by name; `None` for the best this build has.
    pub codec: Option<String>,
    /// Bits a second.
    pub bitrate: u32,
    /// The sun's azimuth and elevation, in degrees, where the film begins.
    /// `None`: the look's own direction.
    pub sun: Option<(f64, f64)>,
}

/// The prefix of a film's keys: `films/<its packs' name>/<look>`, the look
/// naming everything a rendered picture depends on beside its pack — and
/// not which frames are rendered, so that every part of a film agrees.
pub fn film_prefix(packs: &str, rendering: &Rendering) -> String {
    let mut rendering = rendering.clone();
    rendering.order.frames = None;
    let look = tuile_pack::scene_digest(&[format!("{rendering:?}").as_bytes()]);
    format!(
        "films/{}/{look}",
        packs.trim_matches('/').trim_start_matches("packs/")
    )
}

/// What owns a tokio runtime of its own — the shared tile store does, to
/// flush from a bake's thread — made and let go off the runtime a production
/// runs on: a runtime may be neither started nor dropped from within another.
pub struct OffRuntime<T: Send + 'static> {
    inner: Option<T>,
}

impl<T: Send + 'static> OffRuntime<T> {
    /// `open` run on a thread of the blocking pool.
    pub async fn open(
        open: impl FnOnce() -> Result<T, String> + Send + 'static,
    ) -> Result<Self, String> {
        let inner = tokio::task::spawn_blocking(open)
            .await
            .map_err(|e| e.to_string())??;
        Ok(Self { inner: Some(inner) })
    }

    pub fn get(&self) -> Option<&T> {
        self.inner.as_ref()
    }
}

impl<T: Send + 'static> Drop for OffRuntime<T> {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            // On a thread of its own, waited for: whatever it flushes on its
            // way out is done before this returns.
            let _ = std::thread::spawn(move || drop(inner)).join();
        }
    }
}

/// The engine, set for one film.
pub struct Engine {
    bake: TapeBake,
    /// The shared tile store a bake reads and fills, when there is one.
    tiles: OffRuntime<Option<Tiles>>,
    sources: Sources,
    rendering: Rendering,
    /// The names its parameters give, taken once.
    packs: String,
    film: String,
    /// The runtime a render's reads run on.
    runtime: tokio::runtime::Handle,
}

impl Engine {
    /// The engine for `bake` rendered as `rendering` says. `runs` holds the
    /// packs and `tiles` the tile store the render reads; `cache` is where
    /// chunks of both are kept while the film is made. `first` is the
    /// film's first frame: the sun is placed where its camera is.
    pub async fn open(
        bake: TapeBake,
        mut rendering: Rendering,
        first: u32,
        runs: Arc<dyn Objects>,
        tiles: Arc<dyn Objects>,
        cache: &Path,
    ) -> Result<Self, Error> {
        // One sun for the whole film: placed anew at each part's first
        // frame, it would step between two parts.
        if let Some((azimuth, elevation)) = rendering.sun {
            let eye = first
                .checked_sub(1)
                .and_then(|i| bake.poses.get(i as usize))
                .ok_or_else(|| {
                    format!(
                        "frame {first} is not one of the tape's {}",
                        bake.poses.len()
                    )
                })?
                .position;
            rendering.order.look.to_sun = sun_at(eye, azimuth, elevation);
        }
        let packs = bake.packs();
        let film = film_prefix(&packs, &rendering);
        Ok(Self {
            tiles: OffRuntime::open(Tiles::from_env).await?,
            sources: Sources::open(runs, tiles, cache).await?,
            bake,
            rendering,
            packs,
            film,
            runtime: tokio::runtime::Handle::current(),
        })
    }

    fn tiles(&self) -> Option<&Tiles> {
        self.tiles.get().and_then(Option::as_ref)
    }

    async fn rendered(&self, frames: Stretch, pack: &str, out: &Path) -> Result<(), Error> {
        let film = Film::of(self.sources.packs.objects.as_ref(), &[pack]).await?;
        let mut order = self.rendering.order.clone();
        order.frames = Some((frames.first, frames.last));
        let mut sink = film_sink(
            out.to_string_lossy().into_owned(),
            self.rendering.bitrate,
            self.rendering.codec.as_deref(),
        )?;
        render(
            &self.sources,
            &film,
            &order,
            &mut (),
            sink.as_mut(),
            &mut (),
        )
        .await?;
        Ok(())
    }
}

impl Host for Engine {
    fn packs(&self) -> String {
        self.packs.clone()
    }

    fn film(&self) -> String {
        self.film.clone()
    }

    fn bake(&self, frames: Stretch, out: &Path) -> Result<(), Failed> {
        if self.bake.token.is_empty() {
            return Err(Failed::for_good(
                "a pack is to be baked and TUILE_ION_TOKEN is not set",
            ));
        }
        let baked = self.bake.bake(frames.first, frames.last, out, self.tiles());
        // Whatever the bake fetched is published, even if it failed.
        let flushed = self.tiles().map(Tiles::flush).transpose();
        baked.map_err(Failed::once)?;
        flushed.map_err(Failed::once)?;
        Ok(())
    }

    fn render(&self, frames: Stretch, pack: &str, out: &Path) -> Result<(), Failed> {
        // Called on a thread of its own: the render's reads are waited for
        // here, on the runtime that made the engine.
        self.runtime
            .block_on(self.rendered(frames, pack, out))
            .map_err(|e| {
                match e.downcast_ref::<AbsentTiles>() {
                    // The same tiles would be absent on the next try.
                    Some(_) => Failed::for_good(e.to_string()),
                    None => Failed::once(e.to_string()),
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendering() -> Rendering {
        Rendering {
            order: Order::default(),
            codec: None,
            bitrate: 12_000_000,
            sun: None,
        }
    }

    /// What holds a runtime of its own, as the shared tile store does: made
    /// and let go on the async runtime that drives a production, either one
    /// panics ("Cannot start a runtime from within a runtime").
    struct OwnRuntime(tokio::runtime::Runtime);

    impl OwnRuntime {
        fn open() -> Result<Self, String> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .map_err(|e| e.to_string())?;
            runtime.block_on(async {});
            Ok(Self(runtime))
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn what_owns_a_runtime_is_made_and_let_go_off_the_production_s_own() {
        let held = OffRuntime::open(OwnRuntime::open).await.expect("opened");
        assert!(held.get().is_some());
        drop(held);
        // Nothing to hold is nothing.
        let none = OffRuntime::<OwnRuntime>::open(|| Err("not configured".into())).await;
        assert!(none.is_err());
    }

    #[test]
    fn a_film_is_kept_under_its_packs_and_its_look_and_not_under_its_frames() {
        let film = film_prefix("packs/abc.references", &rendering());
        assert!(film.starts_with("films/abc.references/"), "{film}");
        assert_eq!(film_prefix("packs/abc.references/", &rendering()), film);
        // Whichever frames are rendered, it is the same film.
        let mut part = rendering();
        part.order.frames = Some((51, 100));
        assert_eq!(film_prefix("packs/abc.references", &part), film);

        // Anything of the picture is another film; another bake too.
        let mut others = vec![film_prefix("packs/abd.references", &rendering())];
        for change in [
            (|r: &mut Rendering| r.order.fps = 60) as fn(&mut Rendering),
            |r| r.order.scale = 0.5,
            |r| r.order.supersample = 1,
            |r| r.order.look.exposure_ev = 1.6,
            |r| r.order.look.shadow = 1.0,
            |r| r.order.look.haze = Some(tuile_film::Haze::default()),
            |r| r.order.tone = crate::Tone::Off,
            |r| r.codec = Some("h264".into()),
            |r| r.bitrate = 45_000_000,
            |r| r.sun = Some((255.0, 10.0)),
        ] {
            let mut changed = rendering();
            change(&mut changed);
            others.push(film_prefix("packs/abc.references", &changed));
        }
        others.push(film);
        let distinct: std::collections::HashSet<&String> = others.iter().collect();
        assert_eq!(distinct.len(), others.len(), "{others:?}");
    }
}
