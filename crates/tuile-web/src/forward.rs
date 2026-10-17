// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! What the worker forwards to the page, and what it refuses to.
//!
//! The worker sits between the geometry server and a page that renders, and it
//! is the only party that sees every message in order. So the rules a native
//! consumer applies on receipt — `tuile-wgpu`'s `ContentPump::on_message` —
//! are applied **here**, once, before anything crosses `postMessage`:
//!
//! - **`Select`** is forwarded with its whole `ancestry` closure and its
//!   `generation`. A selection that answers an *older* camera than one already
//!   forwarded is dropped: the page would otherwise draw the ground for a view
//!   it has left. Its ancestry links are still facts about the tree, but the
//!   page gets them again with the next selection, so nothing is lost.
//! - **`Content`** is forwarded as real content with its own `ancestry`, and
//!   acknowledged to the server — the page applies a batch on receipt, in one
//!   task, so forwarding is the upload as far as the server can tell. Without
//!   the ack the server believes the consumer holds nothing and keeps sending
//!   stand-ins over ground that is already real.
//! - **`Fill`** is refused when the page already holds the tile's real
//!   content. Both are filed under the same tile, so a late stand-in would
//!   overwrite real geometry with an approximation.
//! - **`Retire`** is narrowed to the tiles the page holds *as stand-ins*. A
//!   tile whose real content has since arrived is not the message's to touch.
//! - **`Evict`** is forwarded whole and forgets everything about its tiles.
//!
//! Pure Rust with no browser in it, so the rules are tested on the host.

use std::collections::HashMap;

use tuile_core::content::{DecodedTileContent, TileContent};
use tuile_core::protocol::{Ancestry, Priming, ServerMessage};
use tuile_core::source::TileId;
use tuile_core::traversal::TraversalStats;

/// What the page holds for one tile, as far as the worker has forwarded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Held {
    Real,
    Fill,
}

/// One message as the page will receive it.
#[derive(Debug)]
pub enum Forward {
    Select {
        tiles: Vec<(TileId, f64)>,
        /// Every selected tile and every ancestor of one, as the server sent
        /// it: the chain a page climbs when a tile has not arrived.
        ancestry: Vec<(TileId, Ancestry)>,
        stats: TraversalStats,
        generation: u64,
    },
    Add {
        tile: TileId,
        ancestry: Ancestry,
        /// A stand-in rather than the tile's own content.
        fill: bool,
        content: DecodedTileContent,
    },
    Evict {
        tiles: Vec<TileId>,
    },
    /// Stand-ins only, and only those the page holds as stand-ins.
    Retire {
        tiles: Vec<TileId>,
    },
    Error {
        tile: Option<TileId>,
        message: String,
    },
    Priming(Priming),
}

/// The outcome of routing one server message.
#[derive(Debug, Default)]
pub struct Routed {
    /// What to post to the page, if anything.
    pub forward: Option<Forward>,
    /// A tile to acknowledge to the server.
    pub ack: Option<TileId>,
}

/// The worker's record of what it has forwarded. See the module note.
#[derive(Debug, Default)]
pub struct Forwarder {
    held: HashMap<TileId, Held>,
    generation_sent: u64,
    generation_forwarded: Option<u64>,
}

impl Forwarder {
    pub fn new() -> Self {
        Self::default()
    }

    /// The generation to stamp on the next `ViewerState`. Monotone.
    pub fn next_generation(&mut self) -> u64 {
        self.generation_sent += 1;
        self.generation_sent
    }

    /// The generation the last forwarded selection answers.
    pub fn generation_forwarded(&self) -> Option<u64> {
        self.generation_forwarded
    }

    /// Whether the page holds `tile`, and as a stand-in.
    pub fn holds(&self, tile: TileId) -> Option<bool> {
        self.held.get(&tile).map(|h| *h == Held::Fill)
    }

    pub fn route(&mut self, msg: ServerMessage) -> Routed {
        match msg {
            ServerMessage::Select {
                tiles,
                ancestry,
                stats,
                generation,
            } => {
                if self.generation_forwarded.is_some_and(|g| generation < g) {
                    return Routed::default();
                }
                self.generation_forwarded = Some(generation);
                Routed {
                    forward: Some(Forward::Select {
                        tiles,
                        ancestry,
                        stats,
                        generation,
                    }),
                    ack: None,
                }
            }
            ServerMessage::Content {
                tile,
                ancestry,
                content,
            } => match content {
                TileContent::Decoded(content) => {
                    self.held.insert(tile, Held::Real);
                    Routed {
                        forward: Some(Forward::Add {
                            tile,
                            ancestry,
                            fill: false,
                            content,
                        }),
                        ack: Some(tile),
                    }
                }
                // Never produced by the in-process binding; said rather than
                // dropped silently if it ever is.
                TileContent::Raw { .. } => Routed {
                    forward: Some(Forward::Error {
                        tile: Some(tile),
                        message: "raw content reached the worker".into(),
                    }),
                    ack: None,
                },
            },
            ServerMessage::Fill {
                tile,
                ancestry,
                content,
            } => {
                if self.held.get(&tile) == Some(&Held::Real) {
                    return Routed::default();
                }
                self.held.insert(tile, Held::Fill);
                Routed {
                    forward: Some(Forward::Add {
                        tile,
                        ancestry,
                        fill: true,
                        content,
                    }),
                    ack: None,
                }
            }
            ServerMessage::Retire { tiles } => {
                let tiles: Vec<TileId> = tiles
                    .into_iter()
                    .filter(|t| self.held.get(t) == Some(&Held::Fill))
                    .collect();
                for t in &tiles {
                    self.held.remove(t);
                }
                Routed {
                    forward: (!tiles.is_empty()).then_some(Forward::Retire { tiles }),
                    ack: None,
                }
            }
            ServerMessage::Evict { tiles } => {
                for t in &tiles {
                    self.held.remove(t);
                }
                Routed {
                    forward: Some(Forward::Evict { tiles }),
                    ack: None,
                }
            }
            ServerMessage::Error { tile, message } => Routed {
                forward: Some(Forward::Error { tile, message }),
                ack: None,
            },
            ServerMessage::Priming(p) => Routed {
                forward: Some(Forward::Priming(p)),
                ack: None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty() -> DecodedTileContent {
        DecodedTileContent {
            withheld_drape: None,
            meshes: Vec::new(),
            textures: Vec::new(),
            imagery: Vec::new(),
            local_origin_ecef: glam::DVec3::ZERO,
            transform_local: glam::Mat4::IDENTITY,
        }
    }

    fn anc(level: u32, parent: Option<TileId>) -> Ancestry {
        Ancestry { level, parent }
    }

    fn content(tile: TileId) -> ServerMessage {
        ServerMessage::Content {
            tile,
            ancestry: anc(3, None),
            content: TileContent::Decoded(empty()),
        }
    }

    fn fill(tile: TileId) -> ServerMessage {
        ServerMessage::Fill {
            tile,
            ancestry: anc(3, None),
            content: empty(),
        }
    }

    fn select(
        tiles: &[TileId],
        ancestry: Vec<(TileId, Ancestry)>,
        generation: u64,
    ) -> ServerMessage {
        ServerMessage::Select {
            tiles: tiles.iter().map(|t| (*t, 1.0)).collect(),
            ancestry,
            stats: TraversalStats::default(),
            generation,
        }
    }

    #[test]
    fn a_late_fill_never_replaces_real_content() {
        let mut f = Forwarder::new();
        let t = TileId(7);
        let routed = f.route(content(t));
        assert_eq!(routed.ack, Some(t), "real content is acknowledged");
        let late = f.route(fill(t));
        assert!(
            late.forward.is_none(),
            "a stand-in arriving after the real tile would overwrite it"
        );
        assert_eq!(f.holds(t), Some(false));
    }

    #[test]
    fn a_fill_is_forwarded_flagged_and_never_acknowledged() {
        let mut f = Forwarder::new();
        let t = TileId(8);
        let routed = f.route(fill(t));
        assert!(matches!(
            routed.forward,
            Some(Forward::Add { fill: true, .. })
        ));
        assert_eq!(routed.ack, None, "acking a stand-in stops the real tile");
        // And real content then replaces it.
        let real = f.route(content(t));
        assert!(matches!(
            real.forward,
            Some(Forward::Add { fill: false, .. })
        ));
        assert_eq!(f.holds(t), Some(false));
    }

    #[test]
    fn retire_takes_back_stand_ins_and_spares_real_content() {
        let mut f = Forwarder::new();
        let (real, filled, unknown) = (TileId(1), TileId(2), TileId(3));
        f.route(fill(real));
        f.route(content(real));
        f.route(fill(filled));
        let routed = f.route(ServerMessage::Retire {
            tiles: vec![real, filled, unknown],
        });
        match routed.forward {
            Some(Forward::Retire { tiles }) => assert_eq!(tiles, vec![filled]),
            other => panic!("expected a narrowed retire, got {other:?}"),
        }
        assert_eq!(f.holds(real), Some(false), "real content untouched");
        assert_eq!(f.holds(filled), None);
        // Nothing left to take back: nothing crosses.
        let again = f.route(ServerMessage::Retire { tiles: vec![real] });
        assert!(again.forward.is_none());
    }

    #[test]
    fn evict_forgets_the_tile_so_a_new_fill_is_accepted() {
        let mut f = Forwarder::new();
        let t = TileId(4);
        f.route(content(t));
        assert!(matches!(
            f.route(ServerMessage::Evict { tiles: vec![t] }).forward,
            Some(Forward::Evict { .. })
        ));
        assert_eq!(f.holds(t), None);
        assert!(f.route(fill(t)).forward.is_some());
    }

    #[test]
    fn a_selection_for_an_older_camera_is_dropped() {
        let mut f = Forwarder::new();
        let (a, b) = (TileId(10), TileId(11));
        assert!(f.route(select(&[a], Vec::new(), 5)).forward.is_some());
        assert!(
            f.route(select(&[b], Vec::new(), 4)).forward.is_none(),
            "generation 4 answers a camera the page has left"
        );
        assert_eq!(f.generation_forwarded(), Some(5));
        assert!(
            f.route(select(&[b], Vec::new(), 5)).forward.is_some(),
            "the same generation is re-selected as tiles arrive"
        );
    }

    #[test]
    fn the_whole_ancestor_chain_is_forwarded() {
        let mut f = Forwarder::new();
        let (root, mid, leaf) = (TileId(20), TileId(21), TileId(22));
        let chain = vec![
            (leaf, anc(2, Some(mid))),
            (mid, anc(1, Some(root))),
            (root, anc(0, None)),
        ];
        match f.route(select(&[leaf], chain.clone(), 1)).forward {
            Some(Forward::Select {
                ancestry,
                generation,
                ..
            }) => {
                assert_eq!(ancestry, chain, "a broken chain is a black square");
                assert_eq!(generation, 1);
            }
            other => panic!("expected a selection, got {other:?}"),
        }
        let routed = f.route(ServerMessage::Content {
            tile: mid,
            ancestry: anc(1, Some(root)),
            content: TileContent::Decoded(empty()),
        });
        match routed.forward {
            Some(Forward::Add { ancestry, .. }) => assert_eq!(ancestry, anc(1, Some(root))),
            other => panic!("expected content, got {other:?}"),
        }
    }

    #[test]
    fn generations_sent_are_monotone() {
        let mut f = Forwarder::new();
        let a = f.next_generation();
        let b = f.next_generation();
        assert!(b > a);
    }
}
