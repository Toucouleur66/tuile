// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! The CPU mip chains of a frame's textures, built off the render thread.
//!
//! An RGBA8 texture is uploaded with its whole chain, built on the CPU
//! ([`tuile_core::raster::mip_chain`], see `upload_texture` for why not on the GPU) —
//! and measured, that chain was the largest single cost of an upload: about
//! two thirds of the render thread's time in `prepare` under the aggressive
//! sequence. It is the same chain wherever it is built, so it is built
//! elsewhere:
//!
//! - **ahead**, on background threads, for what is queued and not uploaded
//!   this frame — a deferred delivery finds its chain ready when its frame
//!   comes;
//! - **across threads at once** for what this frame uploads and whose chain
//!   is not ready yet, the render thread taking its share.
//!
//! A texture whose chain is not here is built where it was, inline: the
//! chain is the same bytes whichever path built it, so nothing drawn
//! changes. On wasm there are no threads to give it to and every chain is
//! built inline, as before.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tuile_core::content::{DecodedTexture, DecodedTileContent};
use tuile_core::protocol::ResourceContent;
#[cfg(not(target_arch = "wasm32"))]
use tuile_core::raster;

/// How many queued deliveries ahead of the frame have their chains built.
pub(crate) const AHEAD: usize = 64;

/// What keeps a texture alive while its chain is built: the content it is
/// part of, so its address cannot be reused by another texture meanwhile.
#[derive(Clone)]
enum Keep {
    Imagery(Arc<DecodedTexture>),
    Owned(Arc<DecodedTileContent>, usize),
}

impl Keep {
    fn texture(&self) -> &DecodedTexture {
        match self {
            Keep::Imagery(t) => t,
            Keep::Owned(content, i) => &content.textures[*i],
        }
    }
}

/// A texture's address: what its chain is found by.
fn key(t: &DecodedTexture) -> usize {
    t as *const DecodedTexture as usize
}

/// A chain asked for, and built once it is.
struct Slot {
    // Read by the background threads, which wasm has none of.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    keep: Keep,
    chain: Option<Vec<DecodedTexture>>,
}

/// The chains built, or being built, by texture.
#[derive(Default)]
pub(crate) struct Chains {
    slots: Arc<Mutex<HashMap<usize, Slot>>>,
    #[cfg(not(target_arch = "wasm32"))]
    workers: std::sync::OnceLock<Option<std::sync::mpsc::Sender<usize>>>,
}

/// The textures of `content` whose chain an upload builds on the CPU.
fn needing(content: &ResourceContent) -> Vec<Keep> {
    let wants = |t: &DecodedTexture| t.rgba8().is_some();
    match content {
        ResourceContent::Mesh(m) => m
            .textures
            .iter()
            .enumerate()
            .filter(|(_, t)| wants(t))
            .map(|(i, _)| Keep::Owned(Arc::clone(m), i))
            .chain(
                m.imagery
                    .iter()
                    .filter(|l| l.is_visible() && wants(&l.texture))
                    .map(|l| Keep::Imagery(Arc::clone(&l.texture))),
            )
            .collect(),
        ResourceContent::Drape(d) => d
            .tiles
            .iter()
            .filter(|l| l.is_visible() && wants(&l.texture))
            .map(|l| Keep::Imagery(Arc::clone(&l.texture)))
            .collect(),
    }
}

impl Chains {
    /// The chain of `t`, if it was built: taken, so it is used once.
    pub(crate) fn take(&self, t: &DecodedTexture) -> Option<Vec<DecodedTexture>> {
        let mut slots = self.slots.lock().ok()?;
        let k = key(t);
        if slots.get(&k)?.chain.is_some() {
            slots.remove(&k)?.chain
        } else {
            None
        }
    }

    /// Lets go of whatever was built or asked for `content`'s textures: it
    /// was uploaded, or will not be.
    pub(crate) fn forget(&self, content: &ResourceContent) {
        let keys: Vec<usize> = needing(content).iter().map(|k| key(k.texture())).collect();
        if keys.is_empty() {
            return;
        }
        if let Ok(mut slots) = self.slots.lock() {
            for k in keys {
                slots.remove(&k);
            }
        }
    }

    /// Keeps only the chains of `contents` — what goes this frame and what
    /// is asked ahead — so what is held stays bounded however long the
    /// queue grows: a delivery that fell back in the order is asked again
    /// when it comes forward.
    pub(crate) fn keep_only<'a>(&self, contents: impl IntoIterator<Item = &'a ResourceContent>) {
        let kept: std::collections::HashSet<usize> = contents
            .into_iter()
            .flat_map(needing)
            .map(|k| key(k.texture()))
            .collect();
        if let Ok(mut slots) = self.slots.lock() {
            slots.retain(|k, _| kept.contains(k));
        }
    }

    /// Builds now, across threads, the chains of `contents` not built yet:
    /// this frame's uploads. Returns how many were built.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn build_now(&self, contents: &[&ResourceContent]) -> usize {
        let missing: Vec<Keep> = {
            let Ok(slots) = self.slots.lock() else {
                return 0;
            };
            contents
                .iter()
                .flat_map(|c| needing(c))
                .filter(|k| {
                    slots
                        .get(&key(k.texture()))
                        .is_none_or(|s| s.chain.is_none())
                })
                .collect()
        };
        // One chain alone is built where it would have been.
        if missing.len() < 2 {
            return 0;
        }
        let threads = std::thread::available_parallelism()
            .map_or(1, |n| n.get())
            .clamp(1, 4)
            .min(missing.len());
        let shares: Vec<&[Keep]> = missing.chunks(missing.len().div_ceil(threads)).collect();
        let (mine, theirs) = shares.split_first().expect("at least one share");
        let built: Vec<(Keep, Vec<DecodedTexture>)> = std::thread::scope(|scope| {
            let handles: Vec<_> = theirs
                .iter()
                .map(|share| {
                    scope.spawn(move || {
                        share
                            .iter()
                            .map(|k| (k.clone(), raster::mip_chain(k.texture())))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            let mut built: Vec<(Keep, Vec<DecodedTexture>)> = mine
                .iter()
                .map(|k| (k.clone(), raster::mip_chain(k.texture())))
                .collect();
            for h in handles {
                built.extend(h.join().unwrap_or_default());
            }
            built
        });
        let n = built.len();
        if let Ok(mut slots) = self.slots.lock() {
            for (keep, chain) in built {
                slots.insert(
                    key(keep.texture()),
                    Slot {
                        keep,
                        chain: Some(chain),
                    },
                );
            }
        }
        n
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) fn build_now(&self, _contents: &[&ResourceContent]) -> usize {
        0
    }

    /// Asks the background threads for the chains of `contents`: queued, to
    /// be uploaded in a later frame. Each is asked once.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn ahead<'a>(&self, contents: impl IntoIterator<Item = &'a ResourceContent>) {
        let Some(workers) = self.workers.get_or_init(|| self.spawn()) else {
            return;
        };
        let Ok(mut slots) = self.slots.lock() else {
            return;
        };
        for keep in contents.into_iter().flat_map(needing) {
            let k = key(keep.texture());
            if slots.contains_key(&k) {
                continue;
            }
            slots.insert(k, Slot { keep, chain: None });
            let _ = workers.send(k);
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) fn ahead<'a>(&self, _contents: impl IntoIterator<Item = &'a ResourceContent>) {}

    /// The background threads: each takes a texture asked for, builds its
    /// chain without holding the lock, and leaves it in its slot — unless
    /// the slot was let go of meanwhile.
    #[cfg(not(target_arch = "wasm32"))]
    fn spawn(&self) -> Option<std::sync::mpsc::Sender<usize>> {
        let (send, receive) = std::sync::mpsc::channel::<usize>();
        let receive = Arc::new(Mutex::new(receive));
        let threads = std::thread::available_parallelism()
            .map_or(1, |n| n.get() / 4)
            .clamp(1, 2);
        for i in 0..threads {
            let receive = Arc::clone(&receive);
            let slots = Arc::downgrade(&self.slots);
            let spawned = std::thread::Builder::new()
                .name(format!("tuile mip chains {i}"))
                .spawn(move || loop {
                    let next = receive.lock().ok().and_then(|r| r.recv().ok());
                    let Some(k) = next else {
                        return;
                    };
                    let Some(live) = slots.upgrade() else {
                        return;
                    };
                    let keep = match live.lock() {
                        Ok(slots) => match slots.get(&k) {
                            Some(slot) if slot.chain.is_none() => slot.keep.clone(),
                            _ => continue,
                        },
                        Err(_) => return,
                    };
                    let chain = raster::mip_chain(keep.texture());
                    if let Ok(mut held) = live.lock() {
                        if let Some(slot) = held.get_mut(&k) {
                            slot.chain = Some(chain);
                        }
                    };
                });
            if spawned.is_err() {
                return None;
            }
        }
        Some(send)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuile_core::raster::{ImageryCoord, ImageryLayer};

    fn drape(seed: u8) -> ResourceContent {
        let pixels: Vec<u8> = (0..64 * 64 * 4)
            .map(|i| (i as u8).wrapping_mul(seed))
            .collect();
        ResourceContent::Drape(Arc::new(tuile_core::content::DrapeContent {
            layer: tuile_core::budget::LayerId(0),
            level: 3,
            tiles: vec![ImageryLayer {
                coord: ImageryCoord {
                    level: 3,
                    x: 1,
                    y: 1,
                },
                source: 0,
                texture: Arc::new(DecodedTexture::from_rgba8(64, 64, pixels)),
                coverage: [0.0, 0.0, 1.0, 1.0],
                translation: [0.0, 0.0],
                scale: [1.0, 1.0],
            }],
            floor_tiles: 0,
            identity: 1,
            withheld: false,
        }))
    }

    fn texture(content: &ResourceContent) -> &DecodedTexture {
        match content {
            ResourceContent::Drape(d) => &d.tiles[0].texture,
            ResourceContent::Mesh(_) => unreachable!(),
        }
    }

    /// **A chain built off the render thread is the chain built on it**,
    /// byte for byte, and it is used once.
    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn a_chain_built_across_threads_is_the_inline_chain() {
        let chains = Chains::default();
        let (a, b) = (drape(3), drape(7));
        assert_eq!(chains.build_now(&[&a, &b]), 2);
        for content in [&a, &b] {
            let t = texture(content);
            let built = chains.take(t).expect("built");
            let inline = raster::mip_chain(t);
            assert_eq!(built.len(), inline.len());
            for (x, y) in built.iter().zip(&inline) {
                assert_eq!((x.width, x.height), (y.width, y.height));
                assert_eq!(x.rgba8(), y.rgba8());
            }
            assert!(chains.take(t).is_none(), "a chain is taken once");
        }
    }

    /// **A chain asked ahead is built in the background** and found ready;
    /// one let go of is not kept.
    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn a_chain_asked_ahead_is_built_in_the_background() {
        let chains = Chains::default();
        let (a, b) = (drape(5), drape(9));
        chains.ahead([&a, &b]);
        chains.forget(&b);
        let t = texture(&a);
        let began = std::time::Instant::now();
        let built = loop {
            if let Some(built) = chains.take(t) {
                break built;
            }
            assert!(began.elapsed().as_secs() < 10, "never built");
            std::thread::yield_now();
        };
        assert_eq!(built.len(), raster::mip_chain(t).len());
        assert!(
            chains.take(texture(&b)).is_none(),
            "a forgotten chain is kept"
        );
        assert!(chains.slots.lock().expect("slots").is_empty());
    }
}
