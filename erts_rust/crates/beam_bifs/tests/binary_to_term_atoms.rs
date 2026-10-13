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

//! Integration tests for `erlang:binary_to_term/1,2`'s **atom interning**.
//!
//! `binary_to_term` decodes an ETF image (contract A.5) into a *live* term. An
//! image that carries an atom names it by its global index, so decoding a name
//! the runtime has never seen must **extend the runtime's atom table**. The
//! context only borrows that table immutably ([`BifContext::atoms`]), so the
//! create path is the [`AtomInterner`] seam: the decoder interns into a
//! memoized, per-thread *image* of the context's table and then hands every
//! appended name to the hook, in order — so the hook must assign exactly the
//! index the decoder wrote into the term. (The memo's reuse correctness is
//! pinned by `tests/decode_scratch.rs`.)
//!
//! These tests drive the registry exactly as `beam_rt` must: a real
//! `beam_alloc::Heap`, a real seeded `beam_term::AtomTable`, and an
//! [`AtomInterner`] over a clone of that table. They pin the property the
//! `wire/decode_reference_atom_blob` and `wire/decode_reference_map_blob`
//! differential cases depend on — a decoded fresh atom resolves back to its name
//! against the table the interner extended.
//!
//! Without an installed hook, an image carrying a fresh atom is the documented
//! `TermNotFound` gap (never a fabricated index); that half is pinned by the
//! unit tests in `src/binconv.rs`.

use std::cell::RefCell;
use std::rc::Rc;

use beam_alloc::Heap;
use beam_bifs::{default_registry, AtomInterner, BifContext};
use beam_codec::term::make_binary;
use beam_common::BeamError;
use beam_process::ProcessId;
use beam_term::{atom_val, make_small, Atom, AtomTable};

/// `term_to_binary(foo)` — `SMALL_ATOM_UTF8_EXT` `foo`
/// (`binary_to_term(<<131,119,3,102,111,111>>)`).
const FOO_BLOB: &[u8] = &[131, 119, 3, 102, 111, 111];

/// The reference `MAP_EXT` image of `#{a => 1}`
/// (`binary_to_term(<<131,116,0,0,0,1,119,1,97,97,1>>)`).
const MAP_BLOB: &[u8] = &[131, 116, 0, 0, 0, 1, 119, 1, 97, 97, 1];

/// The same map with a **fresh** key atom, `#{foo => 1}` — exercises the hook
/// for an atom nested inside a container.
const MAP_FRESH_BLOB: &[u8] = &[131, 116, 0, 0, 0, 1, 119, 3, 102, 111, 111, 97, 1];

/// The seed used by these tests: `a` is present (`MAP_BLOB`'s key); `foo` is
/// deliberately absent so the interning path is actually exercised.
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(["true", "false", "ok", "a"]);
    atoms
}

/// An [`AtomInterner`] over a **clone** of the table the context borrows.
///
/// This reproduces the index coupling `binary_to_term` relies on: the clone has
/// the context's indices for every already-interned name, and a name the hook
/// creates gets exactly the index the context's own table would have assigned —
/// which is the index the decoder wrote into the decoded term.
struct Interner {
    table: Rc<RefCell<AtomTable>>,
}

impl AtomInterner for Interner {
    fn intern(&mut self, name: &str) -> Atom {
        self.table.borrow_mut().intern(name)
    }
}

#[test]
fn binary_to_term_interns_a_fresh_atom_through_the_hook() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    assert!(atoms.lookup("foo").is_none(), "the premise: foo is fresh");

    let shared = Rc::new(RefCell::new(atoms.clone()));
    let term = {
        let mut interner = Interner {
            table: Rc::clone(&shared),
        };
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
            .with_atom_interner(&mut interner);
        let blob = make_binary(ctx.heap(), FOO_BLOB).expect("the blob fits");
        reg.call("erlang", "binary_to_term", 1, &mut ctx, &[blob])
            .expect("an ATOM_UTF8_EXT image decodes")
    };

    assert!(beam_term::is_atom(term), "the decode yields the atom");

    // The hook created `foo` (it was not in the seed)...
    let table = shared.borrow();
    assert!(table.lookup("foo").is_some(), "the hook interned the name");
    // ...and the decoded term's atom word names it.
    assert_eq!(table.name(atom_val(term)), Some("foo"));

    // The decoded term resolves against a context built over the very table the
    // interner extended — exactly how `beam_rt` reads a decoded term back. The
    // term *is* the atom `foo`, not merely an index that happens to look right.
    let mut heap2 = Heap::new(1024).expect("test heap");
    let ctx = BifContext::new(&mut heap2, &table, ProcessId::new(1, 0));
    assert_eq!(ctx.atom("foo").expect("interned by the hook"), term);
}

#[test]
fn binary_to_term_interns_atoms_inside_a_map() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    assert!(
        atoms.lookup("a").is_some(),
        "the premise: the map key is interned"
    );

    let shared = Rc::new(RefCell::new(atoms.clone()));
    let term = {
        let mut interner = Interner {
            table: Rc::clone(&shared),
        };
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
            .with_atom_interner(&mut interner);
        let blob = make_binary(ctx.heap(), MAP_BLOB).expect("the blob fits");
        reg.call("erlang", "binary_to_term", 1, &mut ctx, &[blob])
            .expect("a MAP_EXT image decodes")
    };

    let table = shared.borrow();
    // `#{a => 1}`: one pair, whose key is the already-interned atom `a` — so the
    // hook was not needed and the table did not grow.
    assert_eq!(table.len(), atoms.len(), "no new atom was created");
    let pairs = beam_term::map::map_pairs(term, &table).expect("a flat map");
    assert_eq!(pairs.len(), 1);
    let (key, value) = pairs[0];
    assert!(beam_term::is_atom(key), "the key is an atom");
    assert_eq!(table.name(atom_val(key)), Some("a"));
    assert_eq!(value, make_small(1));
}

#[test]
fn binary_to_term_interns_a_fresh_atom_inside_a_map() {
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();

    let shared = Rc::new(RefCell::new(atoms.clone()));
    let term = {
        let mut interner = Interner {
            table: Rc::clone(&shared),
        };
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
            .with_atom_interner(&mut interner);
        let blob = make_binary(ctx.heap(), MAP_FRESH_BLOB).expect("the blob fits");
        reg.call("erlang", "binary_to_term", 1, &mut ctx, &[blob])
            .expect("a MAP_EXT image decodes")
    };

    let table = shared.borrow();
    // `#{foo => 1}`: the nested fresh key went through the hook.
    let pairs = beam_term::map::map_pairs(term, &table).expect("a flat map");
    assert_eq!(pairs.len(), 1);
    let (key, value) = pairs[0];
    assert!(beam_term::is_atom(key), "the key is an atom");
    assert_eq!(table.name(atom_val(key)), Some("foo"));
    assert_eq!(value, make_small(1));
}

#[test]
fn binary_to_term_without_an_interner_reports_term_not_found() {
    // The documented gap (a fresh name with no hook is `TermNotFound`, never a
    // fabricated index) — pinned here at the seam level, next to the happy path.
    let reg = default_registry();
    let mut heap = Heap::new(4096).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    let blob = make_binary(ctx.heap(), FOO_BLOB).expect("the blob fits");
    assert!(matches!(
        reg.call("erlang", "binary_to_term", 1, &mut ctx, &[blob]),
        Err(BeamError::TermNotFound)
    ));
}
