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

//! The outbound-payload suite: the gate over every walkable shape, and the round trip
//! that turns one of this runtime's own identifiers into the external term the wire
//! carries.

use beam_alloc::Heap;
use beam_codec::make_internal_ref;
use beam_codec::term::{
    external_pid_parts, external_port_parts, external_ref_parts, make_external_pid, make_flatmap,
    make_map, tuple_arity, tuple_element,
};
use beam_term::prelude::{
    is_list, make_atom, make_internal_pid, make_internal_port, make_pid_data, make_port_data,
    make_small, Eterm, NIL,
};
use beam_term::AtomTable;

use super::{holds_local_id, PayloadArena};

/// This runtime's own node, as the atom term an external identifier's node slot carries.
const LOCAL_NODE: &str = "rustus@billmini";

/// This runtime's creation (the other half of the identity an external id carries).
const CREATION: u32 = 7;

/// A term heap for the fixtures' own terms.
fn heap() -> Heap {
    Heap::new(1 << 12).expect("a small term heap")
}

/// A fresh payload arena.
fn arena() -> PayloadArena {
    PayloadArena::new().expect("a payload arena")
}

/// The local node's atom term in `atoms`.
fn local_node(atoms: &mut AtomTable) -> Eterm {
    make_atom(atoms.intern(LOCAL_NODE))
}

/// An internal pid with serial `3` and process number `42` — the identity an external
/// pid of this node must carry after the rewrite.
fn local_pid() -> Eterm {
    make_internal_pid(make_pid_data(3, 42))
}

/// A pid belongs to the sender's node, so a payload carrying it has to reach the wire as
/// an **external** pid: `beam_dist`'s encoder knows no local node identity, so an
/// internal pid has no wire form at all (it refuses with `external_term`).
#[test]
fn an_internal_pid_in_a_payload_becomes_an_external_pid() {
    let mut atoms = AtomTable::new();
    let node = local_node(&mut atoms);
    let mut heap = heap();
    let hello = make_atom(atoms.intern("hello"));
    let payload = heap
        .tuple(&[hello, local_pid()])
        .expect("a 2-tuple payload");

    let mut arena = arena();
    let out = arena
        .externalize(payload, &mut atoms, node, CREATION)
        .expect("the payload externalises");

    assert_ne!(
        out.raw(),
        payload.raw(),
        "a payload carrying a local pid is re-materialised, not passed through"
    );
    assert_eq!(tuple_arity(out).expect("a tuple"), 2);
    let head = tuple_element(out, 0).expect("element 0");
    assert_eq!(head.raw(), hello.raw(), "the other subterms are untouched");
    let (out_node, number, serial, creation) = external_pid_parts(
        tuple_element(out, 1).expect("element 1"),
    )
    .expect("the pid subterm is an external pid");
    assert_eq!(
        out_node.raw(),
        node.raw(),
        "the pid's node slot is this runtime's own node"
    );
    assert_eq!(
        (number, serial, creation),
        (42, 3, CREATION),
        "the process number, serial and creation are the original pid's"
    );
    assert!(
        !holds_local_id(out),
        "no internal identifier is left anywhere in the rewritten payload"
    );
}

/// The gate: a payload with no local identifier is handed back **as the same term**, so
/// today's send path (no copy, no allocation) is untouched.
#[test]
fn a_payload_without_a_local_pid_is_returned_unchanged() {
    let mut atoms = AtomTable::new();
    let node = local_node(&mut atoms);
    let mut heap = heap();
    let hello = make_atom(atoms.intern("hello"));
    let payload = heap
        .tuple(&[hello, make_small(1)])
        .expect("a 2-tuple payload");

    assert!(
        !holds_local_id(payload),
        "an atom and an integer hold no internal pid, port or reference"
    );

    let mut arena = arena();
    let out = arena
        .externalize(payload, &mut atoms, node, CREATION)
        .expect("nothing to rewrite");
    assert_eq!(
        out.raw(),
        payload.raw(),
        "the gate answered false, so the rewrite never ran and the payload was not copied"
    );
}

/// Another node's pid is already external: it must not be rewritten (its node slot names
/// the peer, not this runtime).
#[test]
fn an_external_pid_in_a_payload_is_left_alone() {
    let mut atoms = AtomTable::new();
    let node = local_node(&mut atoms);
    let peer = make_atom(atoms.intern("oracle@billmini"));
    let mut heap = heap();
    let hello = make_atom(atoms.intern("hello"));
    let foreign = make_external_pid(&mut heap, peer, 5, 1, 3).expect("an external pid");
    let payload = heap
        .tuple(&[hello, foreign])
        .expect("a 2-tuple payload");

    assert!(
        !holds_local_id(payload),
        "a peer's pid is not one of this runtime's own identifiers"
    );

    let mut arena = arena();
    let out = arena
        .externalize(payload, &mut atoms, node, CREATION)
        .expect("nothing to rewrite");
    assert_eq!(out.raw(), payload.raw(), "the payload is handed back as-is");
    assert_eq!(
        tuple_element(out, 1).expect("element 1").raw(),
        foreign.raw(),
        "the peer's pid term itself is untouched"
    );
}

/// The other two identifier families: a local `make_ref/0` reference and a local port.
#[test]
fn an_internal_reference_and_a_local_port_in_a_payload_are_externalised() {
    let mut atoms = AtomTable::new();
    let node = local_node(&mut atoms);
    let mut heap = heap();
    let reference = make_internal_ref(&mut heap, 42).expect("an internal reference");
    let port = make_internal_port(make_port_data(7));
    let payload = heap
        .tuple(&[reference, port])
        .expect("a 2-tuple payload");

    assert!(
        holds_local_id(payload),
        "an internal reference and a local port are both local identifiers"
    );

    let mut arena = arena();
    let out = arena
        .externalize(payload, &mut atoms, node, CREATION)
        .expect("the payload externalises");

    let (ref_node, ids, _ref_creation) =
        external_ref_parts(tuple_element(out, 0).expect("element 0"))
            .expect("the reference subterm is an external reference");
    assert_eq!(
        ref_node.raw(),
        node.raw(),
        "the reference's node slot is this runtime's own node"
    );
    assert_eq!(
        ids,
        &[42u32, 0, 0][..],
        "the id packs into the wire numbers (`ref_wire::ref_id_to_numbers`)"
    );

    let (port_node, number, port_creation) =
        external_port_parts(tuple_element(out, 1).expect("element 1"))
            .expect("the port subterm is an external port");
    assert_eq!(
        port_node.raw(),
        node.raw(),
        "the port's node slot is this runtime's own node"
    );
    assert_eq!(
        (number, port_creation),
        (7, CREATION),
        "the port id and creation are the original's"
    );
    assert!(!holds_local_id(out), "neither identifier is local any more");
}

/// The gate descends the shapes a message payload is built from: a list's spine (walked
/// iteratively) and a flat map's keys and values.
#[test]
fn a_local_pid_nested_in_a_list_and_a_flat_map_is_found_and_externalised() {
    let mut atoms = AtomTable::new();
    let node = local_node(&mut atoms);
    let mut heap = heap();
    let key = make_atom(atoms.intern("from"));
    let two = make_small(2);

    let tail = heap.cons(local_pid(), NIL).expect("a one-element list");
    let inner = heap.cons(two, tail).expect("a two-element list");
    let list = heap.cons(inner, NIL).expect("the outer list");
    let map = make_flatmap(&mut heap, &[(key, local_pid())]).expect("a flat map");

    assert!(holds_local_id(list), "a pid two cons cells deep is found");
    assert!(holds_local_id(map), "a pid in a flat map's value is found");

    let mut arena = arena();
    let out_list = arena
        .externalize(list, &mut atoms, node, CREATION)
        .expect("the list externalises");
    let out_map = arena
        .externalize(map, &mut atoms, node, CREATION)
        .expect("the map externalises");

    for out in [out_list, out_map] {
        assert!(!holds_local_id(out), "no internal identifier is left");
        let found = find_external_pid(out).expect("the pid is still there, now external");
        let (out_node, number, serial, creation) =
            external_pid_parts(found).expect("an external pid");
        assert_eq!(out_node.raw(), node.raw());
        assert_eq!((number, serial, creation), (42, 3, CREATION));
    }
}

/// The documented residual: a **hash map** (> 32 pairs) is a leaf to the gate — reading
/// its pairs needs an allocated pair list — so a local identifier inside one keeps the
/// encoder's own honest `external_term` refusal rather than being rewritten.
#[test]
fn a_hash_map_payload_is_outside_the_walk_and_stays_unrewritten() {
    let mut atoms = AtomTable::new();
    let node = local_node(&mut atoms);
    let mut heap = heap();
    let pairs: Vec<(Eterm, Eterm)> = (0..33).map(|i| (make_small(i), local_pid())).collect();
    let map = make_map(&mut heap, &pairs, &atoms).expect("a 33-pair map (a HAMT)");

    assert!(
        !holds_local_id(map),
        "a hash map is outside the walk, so the gate answers false rather than guessing"
    );

    let mut arena = arena();
    let out = arena
        .externalize(map, &mut atoms, node, CREATION)
        .expect("nothing to rewrite");
    assert_eq!(out.raw(), map.raw(), "the payload is handed back as-is");
}

/// The first external pid found anywhere the gate walks, for the nesting test's
/// assertions. `None` when the term holds none.
fn find_external_pid(term: Eterm) -> Option<Eterm> {
    if external_pid_parts(term).is_ok() {
        return Some(term);
    }
    if let Ok(arity) = tuple_arity(term) {
        return (0..arity).find_map(|index| {
            tuple_element(term, index)
                .ok()
                .and_then(find_external_pid)
        });
    }
    if let Ok((keys, values)) = beam_codec::term::flatmap_parts(term) {
        return keys
            .iter()
            .chain(values)
            .find_map(|&elem| find_external_pid(elem));
    }
    let mut cell = term;
    while is_list(cell) {
        let Ok(head) = beam_codec::term::cons_head(cell) else {
            return None;
        };
        if let Some(found) = find_external_pid(head) {
            return Some(found);
        }
        let Ok(tail) = beam_codec::term::cons_tail(cell) else {
            return None;
        };
        if !is_list(tail) {
            return find_external_pid(tail);
        }
        cell = tail;
    }
    None
}
