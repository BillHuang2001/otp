// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%

//! Unit tests for the NIF-created threads' **private atom space** ([`super::ForeignAtomHub`]).
//!
//! Each test names the invariant it pins, in the vocabulary of the module docs: the space is
//! append-only and its indices are private (never the live table's), `reserve` is idempotent,
//! `lookup`/`name_of` never fabricate, the scratch table names every private index so an image can
//! be encoded **by name**, and the one handoff to the runtime — a decoded image — is what carries a
//! name into the live table. No live-table coupling is exercised because there is none left to
//! exercise: nothing in this module reaches a `CodeServer`.

use super::*;
use beam_alloc::Heap;
use beam_codec::Encoder;
use beam_term::prelude::{atom_val, make_atom};
use beam_term::Atom;

/// The node name the by-name handoff test stamps into its image.
const NODE: &str = "nonode@nohost";

/// A reservation is a **private** index: reserving the same name twice answers the same index, a new
/// name takes the space's next one, and the space never renumbers what it already handed out.
#[test]
fn reserve_is_idempotent_and_append_only() {
    let hub = ForeignAtomHub::new();

    assert_eq!(hub.reserve("a"), 0, "the first name takes index 0");
    assert_eq!(hub.reserve("b"), 1);
    assert_eq!(hub.reserve("a"), 0, "a known name keeps its index");
    assert_eq!(hub.reserve("c"), 2, "and the next new name appends after it");
    assert_eq!(hub.reserve("a"), 0, "reserving again changes nothing");
    assert_eq!(hub.name_of(0).as_deref(), Some("a"));
    assert_eq!(hub.name_of(2).as_deref(), Some("c"));
}

/// The space is **private**: it owns no reference to the live table, and an index it hands out is not
/// a live index — a live table holding the same names spells them at its own indices.
#[test]
fn a_private_index_is_never_a_live_index() {
    let mut live = AtomTable::new();
    live.seed(["live_zero", "live_one", "live_two"]);

    let hub = ForeignAtomHub::new();
    let private = hub.reserve("live_two");

    assert_eq!(private, 0, "the private space starts at its own index 0");
    assert_eq!(
        live.lookup("live_two"),
        Some(Atom::from_raw(2)),
        "the live table spells that name at ITS index, which differs from the private one"
    );
    assert_eq!(
        live.name(Atom::from_raw(private)),
        Some("live_zero"),
        "so reading the live table at the private index names a different atom"
    );
    assert_eq!(
        live.len(),
        3,
        "and the private reservation appended nothing to the live table"
    );
    assert_eq!(
        hub.lookup("live_zero"),
        None,
        "a name only the live table knows is not in the private space"
    );
}

/// `name_of`/`lookup` round-trip over what the space holds, and both answer `None` — never a
/// fabricated index or name — for what it does not reach.
#[test]
fn name_of_and_lookup_round_trip_and_never_fabricate() {
    let hub = ForeignAtomHub::new();
    let names = ["alpha", "beta", "gamma"];
    for (index, name) in names.iter().enumerate() {
        assert_eq!(hub.reserve(name), index as u32);
    }

    for (index, name) in names.iter().enumerate() {
        assert_eq!(hub.name_of(index as u32).as_deref(), Some(*name));
        assert_eq!(hub.lookup(name), Some(index as u32));
    }

    assert_eq!(hub.name_of(3), None, "beyond the space's reach");
    assert_eq!(hub.lookup("nope"), None, "lookup never creates");
    assert_eq!(
        hub.name_of(u32::MAX),
        None,
        "an arbitrary index answers None rather than panicking"
    );
    assert_eq!(
        hub.reserve("delta"),
        3,
        "a refused lookup left no gap behind it"
    );
}

/// The scratch table is a **dense** image of the space: private index `i` names the space's atom `i`,
/// so `beam_codec`'s encoder can spell every atom by name. It is cached while nothing was appended and
/// rebuilt once the space grew.
#[test]
fn the_scratch_table_names_every_private_index() {
    let hub = ForeignAtomHub::new();
    hub.reserve("a");
    hub.reserve("b");

    let scratch = hub.scratch_table();
    assert_eq!(scratch.len(), 2, "one atom per private index");
    for index in 0..2u32 {
        assert_eq!(
            scratch.name(Atom::from_raw(index)).map(str::to_string),
            hub.name_of(index),
            "index {index} disagrees between the scratch table and the space"
        );
    }
    assert_eq!(
        scratch.lookup("b"),
        Some(Atom::from_raw(1)),
        "the scratch table's lookup is the inverse of the private index"
    );

    assert!(
        Arc::ptr_eq(&scratch, &hub.scratch_table()),
        "an unchanged space reuses the image"
    );
    hub.reserve("c");
    assert!(
        !Arc::ptr_eq(&scratch, &hub.scratch_table()),
        "a grown space invalidates the image"
    );
    assert_eq!(
        hub.scratch_table().lookup("c"),
        Some(Atom::from_raw(2)),
        "and the rebuilt one names the appended atom"
    );
}

/// The one handoff: an atom built on the space encodes to an image that spells it **by name**, and
/// decoding that image into a live table interns the name there — at the live table's own index, which
/// need not be the private one. This is the property that makes the live-table coupling unnecessary.
#[test]
fn a_decoded_image_carries_a_name_into_the_live_table() {
    let hub = ForeignAtomHub::new();
    let private = hub.reserve("thread_made_atom");

    let table = hub.scratch_table();
    let mut encoder = Encoder::new(table.as_ref());
    encoder.encode(make_atom(Atom::from_raw(private))).expect("the atom encodes");
    let image = encoder.finish();
    assert_eq!(image.first(), Some(&131), "a complete ETF image");

    let mut live = AtomTable::new();
    live.seed(["a_live_atom_first"]);
    let node = live.intern(NODE);
    let mut heap = Heap::new(1024).expect("a decode heap");
    let decoded = beam_codec::decode_term_with_local_node(&image, &mut heap, &mut live, node, 7, None)
        .expect("the image decodes against the live table");

    let live_index = live
        .lookup("thread_made_atom")
        .expect("the decoder interned the name into the live table");
    assert_eq!(
        live_index,
        Atom::from_raw(1),
        "at the live table's own next index"
    );
    assert_ne!(
        live_index,
        Atom::from_raw(private),
        "which is NOT the private index the foreign thread used"
    );
    assert_eq!(
        name_of(&live, live_index),
        "thread_made_atom",
        "the live table spells the name the foreign thread created"
    );
    assert_eq!(
        atom_val(decoded),
        live_index,
        "the decoded term names the live atom"
    );
}

/// Two hosts of one factory share the space, and it is genuinely usable from several threads at once:
/// every thread reserves the same names and all of them agree on each name's index.
#[test]
fn the_space_is_shared_across_threads() {
    let hub = ForeignAtomHub::new();
    let names = ["one", "two", "three", "four"];
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let hub = Arc::clone(&hub);
            std::thread::spawn(move || {
                names
                    .iter()
                    .map(|name| (*name, hub.reserve(name)))
                    .collect::<Vec<_>>()
            })
        })
        .collect();

    let mut answers: Vec<Vec<(&str, u32)>> = handles
        .into_iter()
        .map(|handle| handle.join().expect("the worker does not panic"))
        .collect();
    // The first thread to reach a name fixes its index; every later one must see the same. Sort so the
    // comparison does not depend on which thread won.
    let expected = answers.remove(0);
    for other in &answers {
        for ((name, index), (other_name, other_index)) in expected.iter().zip(other) {
            assert_eq!(name, other_name);
            assert_eq!(
                index, other_index,
                "every thread agrees on {name}'s private index"
            );
        }
    }
    assert_eq!(
        hub.scratch_table().len(),
        names.len(),
        "four distinct names, four indices — the races did not duplicate any"
    );
}

/// `name_at(table, index)`, with a clear failure when the index names nothing.
fn name_of(table: &AtomTable, index: Atom) -> String {
    table
        .name(index)
        .unwrap_or_else(|| panic!("index {index:?} names no atom"))
        .to_string()
}
