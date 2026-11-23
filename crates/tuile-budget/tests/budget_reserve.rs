// SPDX-License-Identifier: MIT OR Apache-2.0
// Copyright (c) lapoule.dev

//! A reservation changed under a running session: the consumer's encoder
//! scratch taken off the drapes' limits when it starts encoding, given back
//! when it stops.

#![allow(clippy::unwrap_used, clippy::panic)]

mod budget_common;

use budget_common::*;
use tuile_budget::{Bytes, Class};

/// **A reservation taken under a session is off the limits from the next
/// step, and let go of, the limits are exactly what they were**: no stale
/// scratch left counted once a consumer stops encoding. One past the hard
/// limit is refused and changes nothing.
#[test]
fn a_reservation_changed_under_a_session_moves_the_limits_and_back() {
    let need = drawn(&[t(3, 0, 0)]);
    let mut rig = Rig::interactive();
    let plain = rig.step(&need).stats;
    let r = Bytes::mib(3);
    rig.b
        .reserve(Class::Drape, r)
        .expect("within the hard limit");
    assert_eq!(rig.b.config().reserved[Class::Drape], r);
    let held = rig.step(&need).stats;
    assert_eq!(
        held.hard_limit[Class::Drape],
        plain.hard_limit[Class::Drape].minus(r),
        "taken off the drapes' hard limit"
    );
    assert!(held.target[Class::Drape] < plain.target[Class::Drape]);
    assert_eq!(
        held.hard_limit[Class::TerrainMesh],
        plain.hard_limit[Class::TerrainMesh],
        "another class keeps its own"
    );

    rig.b.reserve(Class::Drape, Bytes::ZERO).expect("nothing");
    let back = rig.step(&need).stats;
    assert_eq!(
        back.hard_limit[Class::Drape],
        plain.hard_limit[Class::Drape]
    );
    assert_eq!(back.target[Class::Drape], plain.target[Class::Drape]);
    assert_eq!(back.overflow[Class::Drape], plain.overflow[Class::Drape]);

    let past = Rig::interactive_config().hard_limit[Class::Drape].plus(Bytes(1));
    assert!(rig.b.reserve(Class::Drape, past).is_err());
    assert_eq!(rig.b.config().reserved[Class::Drape], Bytes::ZERO);
    assert_eq!(
        rig.step(&need).stats.hard_limit[Class::Drape],
        plain.hard_limit[Class::Drape],
        "a refused reservation changes nothing"
    );
}
