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

//! Regression tests for `binary_to_term/1,2`'s **memoized decode scratch**.
//!
//! The decode path does not clone the whole shared `AtomTable` per call: it decodes
//! into a per-thread *image* of that table (`DECODE_SCRATCH` in `src/binconv.rs`),
//! revalidated index by index against the live table before each reuse and extended
//! name-for-name when the live table has grown. The correctness of that memo rests on
//! one property: **when the decoder writes into the scratch, the scratch must name the
//! same atom as the live table at every index the live table holds.** If it ever does
//! not — a stale image from an earlier table, a shorter table, or a scratch left ahead
//! of the table by an earlier decode — a name the decoder *interns* would be recorded
//! at the wrong global index and the decoded term would silently refer to the wrong
//! atom.
//!
//! These tests reuse one thread deliberately (each `#[test]` runs on one thread, and
//! every case below issues several decodes inside one function), so the memo is
//! carried across calls and every stale-image shape is exercised:
//!
//! * the same table twice (the memo is reused as-is);
//! * a table that is the old one plus new names appended (the memo is extended);
//! * a table that moves a name to a different index (the memo must be rebuilt — the
//!   case a blind reuse would corrupt);
//! * a shorter table after a longer one (the memo must be rebuilt);
//! * a `safe` decode and a failed decode, both of which leave the scratch ahead of
//!   the table with nothing committed.
//!
//! The name-bearing wire format makes the failure mode crisp: an ETF atom carries its
//! *name*, so a stale memo shows up as the decoded term resolving to a **different
//! name** under the live table.

use std::cell::RefCell;
use std::rc::Rc;

use beam_alloc::Heap;
use beam_bifs::{default_registry, AtomInterner, BifContext, BifRegistry};
use beam_codec::term::make_binary;
use beam_common::BeamError;
use beam_process::ProcessId;
use beam_term::{atom_val, make_small, Atom, AtomTable, Eterm};

/// `binary_to_term(<<131,119,3,102,111,111>>)` — the atom `foo`.
const FOO_BLOB: &[u8] = &[131, 119, 3, 102, 111, 111];

/// `binary_to_term(<<131,116,0,0,0,1,119,1,97,97,1>>)` — the map `#{a => 1}`
/// (the key is the already-interned atom `a`, so this image interns nothing).
const MAP_A_BLOB: &[u8] = &[131, 116, 0, 0, 0, 1, 119, 1, 97, 97, 1];

/// A truncated `SMALL_ATOM_UTF8_EXT` (`foo` declared as 3 bytes, only 1 present)
/// — a decode that fails after the decoder has already touched the scratch.
const TRUNCATED_BLOB: &[u8] = &[131, 119, 3, 102];

/// An [`AtomInterner`] over a **clone** of the table the context borrows — the
/// shape `beam_rt`'s `PendingInterner` takes (it predicts the context table's
/// next indices and commits them in order).
struct Interner {
    table: Rc<RefCell<AtomTable>>,
}

impl AtomInterner for Interner {
    fn intern(&mut self, name: &str) -> Atom {
        self.table.borrow_mut().intern(name)
    }
}

/// Decodes `blob` under `atoms` and returns the decoded term, the heap it lives on,
/// and the interner's table (a clone of `atoms` that the call may have extended with
/// fresh names) — so the term stays readable and resolvable.
///
/// `atoms` is what the context borrows; the interner's table is a clone of it (they
/// name the same atoms at the same indices, which is the runtime's own invariant).
fn decode(
    reg: &BifRegistry,
    atoms: &AtomTable,
    blob: &[u8],
) -> Result<(Heap, Eterm, Rc<RefCell<AtomTable>>), BeamError> {
    let mut heap = Heap::new(4096).expect("test heap");
    let shared = Rc::new(RefCell::new(atoms.clone()));
    let mut interner = Interner {
        table: Rc::clone(&shared),
    };
    let mut ctx =
        BifContext::new(&mut heap, atoms, ProcessId::new(1, 0)).with_atom_interner(&mut interner);
    let bin = make_binary(ctx.heap(), blob).expect("the blob fits");
    let term = reg.call("erlang", "binary_to_term", 1, &mut ctx, &[bin])?;
    Ok((heap, term, shared))
}

/// Decodes `blob` to an atom and returns the name it resolves to.
fn decoded_atom_name(reg: &BifRegistry, atoms: &AtomTable, blob: &[u8]) -> String {
    let (_heap, term, table) = decode(reg, atoms, blob).expect("the image decodes");
    assert!(beam_term::is_atom(term), "the decode yields an atom");
    let table = table.borrow();
    table
        .name(atom_val(term))
        .expect("the decoded index names an atom")
        .to_string()
}

#[test]
fn the_same_table_twice_reuses_the_memo_without_moving_the_atom() {
    let reg = default_registry();
    let mut atoms = AtomTable::new();
    atoms.seed(["a", "b", "c", "foo"]); // `foo` present at index 3
    let len = atoms.len();

    assert_eq!(decoded_atom_name(&reg, &atoms, FOO_BLOB), "foo");
    // Second call on the same thread: the memo is reused as-is — still `foo`.
    let (_heap, term, table) = decode(&reg, &atoms, FOO_BLOB).expect("decodes");
    assert_eq!(
        table.borrow().name(atom_val(term)),
        Some("foo"),
        "the memo was reused and still resolves `foo`"
    );
    // No atom was created (both calls found `foo` already interned).
    assert_eq!(table.borrow().len(), len, "no atom was created");
}

#[test]
fn a_growing_table_extends_the_memo_and_keeps_identity() {
    let reg = default_registry();
    let mut base = AtomTable::new();
    base.seed(["a", "b", "c", "foo"]); // foo @3
    assert_eq!(decoded_atom_name(&reg, &base, FOO_BLOB), "foo");

    // The live table grows by appending — `foo` stays at index 3, so the memo is
    // extended rather than rebuilt and must still resolve `foo` to index 3.
    let mut grown = base.clone();
    grown.seed(["g1", "g2"]);
    assert_eq!(decoded_atom_name(&reg, &grown, FOO_BLOB), "foo");
}

#[test]
fn a_table_that_moves_a_name_rebuilds_the_memo() {
    // The case a blind memo reuse would corrupt: `foo` sits at index 3 in the first
    // table and at index 5 in the second, so a memo carried over from the first must
    // be discarded. A stale index would make the decoded term resolve to `d`
    // (index 3 of the second table) instead of `foo`.
    let reg = default_registry();
    let mut first = AtomTable::new();
    first.seed(["a", "b", "c", "foo"]); // foo @3
    assert_eq!(decoded_atom_name(&reg, &first, FOO_BLOB), "foo");

    let mut second = AtomTable::new();
    second.seed(["a", "b", "c", "d", "e", "foo"]); // foo @5, `d` @3
    let (_heap, term, table) = decode(&reg, &second, FOO_BLOB).expect("decodes");
    let table = table.borrow();
    assert_eq!(
        table.name(atom_val(term)),
        Some("foo"),
        "the memo was not rebuilt and the term aliased the wrong atom"
    );
    assert_ne!(table.name(Atom::from_raw(3)), Some("foo"));
}

#[test]
fn a_shorter_table_after_a_longer_one_rebuilds_the_memo() {
    let reg = default_registry();
    let mut long = AtomTable::new();
    long.seed(["a", "b", "c", "d", "e", "foo"]);
    assert_eq!(decoded_atom_name(&reg, &long, FOO_BLOB), "foo");

    let mut short = AtomTable::new();
    short.seed(["a", "b", "c"]); // no `foo`
    let name = decoded_atom_name(&reg, &short, FOO_BLOB);
    assert_eq!(name, "foo", "the memo was not rebuilt to the shorter table");
}

#[test]
fn a_fresh_atom_lands_at_the_index_the_interner_predicts() {
    // `foo` is absent, so the decoder appends it: it must be written at exactly
    // `atoms.len()`, the index `beam_rt`'s interner predicts and commits.
    let reg = default_registry();
    let mut atoms = AtomTable::new();
    atoms.seed(["a", "b", "c"]); // len 3, no `foo`
    let base = atoms.len();

    let (_heap, term, table) = decode(&reg, &atoms, FOO_BLOB).expect("decodes");
    assert_eq!(
        atom_val(term),
        Atom::from_raw(base as u32),
        "the fresh atom must be written at atoms.len()"
    );
    assert_eq!(table.borrow().name(Atom::from_raw(base as u32)), Some("foo"));

    // A second fresh decode on the same thread (the memo is now ahead of `atoms`)
    // must still predict the same base.
    let (_heap, term2, _table) = decode(&reg, &atoms, FOO_BLOB).expect("decodes");
    assert_eq!(atom_val(term2), Atom::from_raw(base as u32));
}

#[test]
fn a_failed_decode_leaves_the_memo_safe_for_the_next_call() {
    // A truncated image fails *after* the decoder has written into the scratch.
    // Nothing is committed, the memo is now ahead of the table, and the next call
    // must rebuild rather than trust it.
    let reg = default_registry();
    let mut atoms = AtomTable::new();
    atoms.seed(["a", "b", "c", "foo"]);
    let len = atoms.len();

    assert!(decode(&reg, &atoms, TRUNCATED_BLOB).is_err());
    assert_eq!(atoms.len(), len, "the context's table is untouched");

    assert_eq!(
        decoded_atom_name(&reg, &atoms, FOO_BLOB),
        "foo",
        "the decode after a failure must still resolve names"
    );
}

#[test]
fn a_safe_decode_of_a_fresh_atom_commits_nothing_and_keeps_the_memo_sound() {
    let reg = default_registry();
    let mut atoms = AtomTable::new();
    atoms.seed(["a", "b", "c", "safe"]); // no `foo`

    // `binary_to_term/2` with `[safe]`: creating an atom is `badarg`, and nothing is
    // committed — but the scratch grew while decoding.
    let mut heap = Heap::new(4096).expect("test heap");
    let shared = Rc::new(RefCell::new(atoms.clone()));
    {
        let mut interner = Interner {
            table: Rc::clone(&shared),
        };
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
            .with_atom_interner(&mut interner);
        let bin = make_binary(ctx.heap(), FOO_BLOB).expect("the blob fits");
        let safe = ctx.atom("safe").expect("seeded");
        let opts = ctx.list(&[safe]).expect("fits");
        assert!(matches!(
            reg.call("erlang", "binary_to_term", 2, &mut ctx, &[bin, opts]),
            Err(BeamError::BadArg)
        ));
    }
    assert_eq!(shared.borrow().len(), atoms.len(), "nothing committed");

    // A later normal decode on the same thread must resolve `foo` correctly.
    assert_eq!(decoded_atom_name(&reg, &atoms, FOO_BLOB), "foo");
}

#[test]
fn an_image_with_no_fresh_atom_is_unaffected_by_the_memo() {
    // `#{a => 1}` interns nothing; decoding it repeatedly (the memo warm) leaves the
    // table untouched and the key correct.
    let reg = default_registry();
    let mut atoms = AtomTable::new();
    atoms.seed(["a", "b", "c", "foo"]);
    let len = atoms.len();

    for _ in 0..3 {
        let (_heap, term, table) = decode(&reg, &atoms, MAP_A_BLOB).expect("decodes");
        let table = table.borrow();
        let pairs = beam_term::map::map_pairs(term, &table).expect("a flat map");
        assert_eq!(pairs.len(), 1);
        assert_eq!(table.name(atom_val(pairs[0].0)), Some("a"));
        assert_eq!(pairs[0].1, make_small(1));
        assert_eq!(table.len(), len, "the map created no atom");
    }
}
