// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What a bake by reference tells, as it goes, of the tiles a render of its
//! frames will read from the tile store.
//!
//! The render of a stretch begins by reading every store tile its frames name
//! — minutes, for a 4K stretch. Told as the bake finds them, a render service
//! reads them while the bake goes on, and is ready when the pack is.

use std::collections::{BTreeSet, HashSet};

use tuile_pack::{BakedTile, StoreTile};

/// A tile of the tile store: its level, x and y.
pub type StoreAt = (u8, u32, u32);

/// The store tiles a stretch of baked frames names, new since the last time
/// they were told.
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Announced {
    /// The frames they were found in, first and last.
    pub frames: (u32, u32),
    /// The tile store's layers they are of.
    pub terrain_layer: String,
    pub imagery_layer: String,
    pub terrain: Vec<StoreAt>,
    pub imagery: Vec<StoreAt>,
}

/// Whoever is told: a host writes them where its render service looks.
pub trait Announce: Send + Sync {
    /// The store tiles of `announced`, which no earlier call named.
    fn announce(&self, announced: &Announced) -> Result<(), String>;
}

/// Frames gathered before what they name is told.
pub const ANNOUNCE_EVERY: u32 = 25;

/// Gathers what the tiles of each frame name and tells it every
/// [`ANNOUNCE_EVERY`] frames, each store tile once.
pub struct Herald<'a> {
    to: &'a dyn Announce,
    layers: (String, String),
    told: HashSet<(bool, StoreAt)>,
    terrain: BTreeSet<StoreAt>,
    imagery: BTreeSet<StoreAt>,
    since: Option<u32>,
}

fn at(tile: &StoreTile) -> StoreAt {
    (tile.level, tile.x, tile.y)
}

impl<'a> Herald<'a> {
    /// `terrain_layer` and `imagery_layer`: the store's layers the pack
    /// refers to.
    pub fn new(to: &'a dyn Announce, terrain_layer: &str, imagery_layer: &str) -> Self {
        Self {
            to,
            layers: (terrain_layer.to_string(), imagery_layer.to_string()),
            told: HashSet::new(),
            terrain: BTreeSet::new(),
            imagery: BTreeSet::new(),
            since: None,
        }
    }

    /// A tile the pack takes at `frame`.
    pub fn tile(&mut self, frame: u32, tile: &BakedTile) {
        self.since.get_or_insert(frame);
        let Some(refs) = &tile.refs else { return };
        if self.told.insert((false, at(&refs.terrain))) {
            self.terrain.insert(at(&refs.terrain));
        }
        for placed in &refs.imagery {
            if self.told.insert((true, at(&placed.tile))) {
                self.imagery.insert(at(&placed.tile));
            }
        }
    }

    /// `frame` is baked: told if it closes a stretch, or if it is the last.
    pub fn frame_done(&mut self, frame: u32, last: bool) -> Result<(), String> {
        let first = *self.since.get_or_insert(frame);
        if !last && frame + 1 - first < ANNOUNCE_EVERY {
            return Ok(());
        }
        self.since = None;
        if self.terrain.is_empty() && self.imagery.is_empty() {
            return Ok(());
        }
        let announced = Announced {
            frames: (first, frame),
            terrain_layer: self.layers.0.clone(),
            imagery_layer: self.layers.1.clone(),
            terrain: std::mem::take(&mut self.terrain).into_iter().collect(),
            imagery: std::mem::take(&mut self.imagery).into_iter().collect(),
        };
        self.to.announce(&announced)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tuile_pack::{ImageryPlacement, TileRefs};

    use super::*;

    #[derive(Default)]
    struct Heard(Mutex<Vec<Announced>>);

    impl Announce for Heard {
        fn announce(&self, announced: &Announced) -> Result<(), String> {
            self.0.lock().expect("lock").push(announced.clone());
            Ok(())
        }
    }

    fn store(level: u8, x: u32, y: u32) -> StoreTile {
        StoreTile {
            level,
            x,
            y,
            digest: 0,
        }
    }

    fn tile(terrain: StoreTile, imagery: &[StoreTile]) -> BakedTile {
        BakedTile {
            id: 0,
            drape: 0,
            origin_ecef: [0.0; 3],
            positions: vec![],
            normals: vec![],
            uvs: vec![],
            indices: vec![],
            vertex_count: 0,
            index_count: 0,
            base_color_factor: [1.0; 4],
            texture: None,
            texture_format: tuile_pack::TextureFormat::None,
            refs: Some(TileRefs {
                terrain,
                skirt_height: 0.0,
                imagery: imagery
                    .iter()
                    .map(|t| ImageryPlacement {
                        tile: t.clone(),
                        coverage: [0.0, 0.0, 1.0, 1.0],
                        translation: [0.0; 2],
                        scale: [1.0; 2],
                    })
                    .collect(),
                composed_side: 256,
            }),
        }
    }

    #[test]
    fn each_store_tile_is_told_once_a_stretch_of_frames_at_a_time_and_the_last_stretch_too() {
        let heard = Heard::default();
        let mut herald = Herald::new(&heard, "asset-1", "asset-2");
        let last = ANNOUNCE_EVERY + 10;
        for frame in 1..=last {
            // The same terrain every frame; a new imagery tile every frame,
            // and one already told.
            herald.tile(
                frame,
                &tile(store(12, 1, 2), &[store(15, frame, 7), store(15, 1, 7)]),
            );
            herald.frame_done(frame, frame == last).expect("told");
        }
        let heard = heard.0.into_inner().expect("lock");
        assert_eq!(heard.len(), 2);
        assert_eq!(heard[0].frames, (1, ANNOUNCE_EVERY));
        assert_eq!(
            (
                heard[0].terrain_layer.as_str(),
                heard[0].imagery_layer.as_str()
            ),
            ("asset-1", "asset-2")
        );
        assert_eq!(heard[0].terrain, [(12, 1, 2)]);
        assert_eq!(heard[0].imagery.len(), ANNOUNCE_EVERY as usize);
        assert_eq!(heard[1].frames, (ANNOUNCE_EVERY + 1, last));
        assert!(heard[1].terrain.is_empty(), "told already");
        assert_eq!(heard[1].imagery.len(), 10);
        let all: HashSet<StoreAt> = heard
            .iter()
            .flat_map(|h| h.imagery.iter().copied())
            .collect();
        assert_eq!(all.len(), last as usize, "each once");
    }

    #[test]
    fn a_tile_with_nothing_of_the_store_and_a_stretch_with_nothing_new_tell_nothing() {
        let heard = Heard::default();
        let mut herald = Herald::new(&heard, "asset-1", "asset-2");
        herald.tile(
            1,
            &BakedTile {
                refs: None,
                ..tile(store(1, 0, 0), &[])
            },
        );
        herald.frame_done(1, true).expect("nothing");
        assert!(heard.0.lock().expect("lock").is_empty());
    }
}
