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

//! Unit tests for the guard-scoped views ([`TableRead`]/[`TableWrite`]/
//! [`TableTerm`]).
//!
//! Four things are pinned here:
//!
//! 1. **The hazard is real** — a bare `Eterm` read out of table storage (the raw
//!    [`Table`] API) silently re-reads as a *different* object once the store's
//!    per-size free lists recycle its words
//!    (`a_stale_raw_term_reads_as_a_different_object_after_arena_churn`).
//! 2. **The fix works at runtime** — a `copy_into` copy taken under a guard still
//!    holds the original value after the same churn
//!    (`copy_out_survives_guard_drop_and_arena_churn`).
//! 3. **The fix works at compile time** — the `compile_fail` doctests on
//!    [`TableTerm`] and [`SharedTable::with_read`] show a tagged term cannot escape
//!    its guard.
//! 4. **The views are faithful** — the reads/mutations they expose behave exactly
//!    like the raw [`Table`] methods they forward to (types, arities, navigation
//!    order, copy-out shapes).

use beam_alloc::Heap as OutHeap;
use beam_term::prelude::*;

use super::{TableRead, TableTerm, TableWrite};
use crate::kind::{Access, TableKind, TableOptions};
use crate::select::compile;
use crate::store::TermStore;
use crate::table::{SharedTable, Table, TableId};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A fresh, publicly-accessible table of `kind`, wrapped for sharing.
fn shared_table(kind: TableKind) -> SharedTable {
    let mut opts = TableOptions::new();
    opts.set_kind(kind);
    opts.set_access(Access::Public);
    SharedTable::new(Table::new(TableId::next(), opts).expect("a fresh table is constructible"))
}

/// The term of `name` (interned in `atoms`).
fn atom(atoms: &mut AtomTable, name: &str) -> Eterm {
    make_atom(atoms.intern(name))
}

/// `{Key, Value}` on `heap`.
fn pair(heap: &mut Heap, key: i64, value: i64) -> Eterm {
    heap.tuple(&[make_small(key), make_small(value)])
        .expect("the test arena has room")
}

/// A heap-allocated flonum (`[HEADER_FLONUM, bits]`), leaked for the test's life
/// (`beam_term` has no heap-based float constructor).
fn flonum(value: f64) -> Eterm {
    let words: Box<[Eterm; 2]> = Box::new([HEADER_FLONUM, Eterm::from_raw(value.to_bits() as usize)]);
    let leaked: &'static [Eterm; 2] = Box::leak(words);
    make_boxed(leaked.as_ptr())
}

/// `{Key, Flonum}` on `heap`.
fn float_pair(heap: &mut Heap, key: i64, value: f64) -> Eterm {
    heap.tuple(&[make_small(key), flonum(value)])
        .expect("the test arena has room")
}

/// Insert `object` into `shared` as the owner would.
fn put(shared: &SharedTable, object: Eterm, atoms: &AtomTable) {
    shared
        .with_write(|write| write.insert(object, atoms))
        .expect("no poisoned lock")
        .expect("a supported object inserts");
}

/// The single object stored under `key`, checked out of the table as an owned
/// term in `dest` — the idiom the tagged reads force.
fn copy_lookup(
    shared: &SharedTable,
    key: Eterm,
    atoms: &AtomTable,
    dest: &mut TermStore,
) -> Option<Eterm> {
    shared
        .with_read(|read| {
            let hits = read.lookup(key, atoms);
            hits.first().map(|hit| hit.copy_into(dest))
        })
        .expect("no poisoned lock")
        .transpose()
        .expect("a table-owned object is always copyable")
}

/// A one-clause spec `[{'$1', [], [Body]}]` compiled against `atoms`.
fn spec(heap: &mut Heap, atoms: &mut AtomTable, body: Eterm) -> crate::select::MatchSpec {
    let head = make_atom(atoms.intern("$1"));
    let body_list = heap.cons(body, NIL).expect("the test arena has room");
    let clause = heap
        .tuple(&[head, NIL, body_list])
        .expect("the test arena has room");
    let spec = heap.cons(clause, NIL).expect("the test arena has room");
    compile(spec, atoms).expect("the quoted spec compiles")
}

// ---------------------------------------------------------------------------
// 1 + 2. The hazard, and the fix
// ---------------------------------------------------------------------------

/// **The fix, at runtime.** A `copy_into` copy taken under a read guard still
/// holds the original value after the guard is dropped *and* after the table churns
/// hard enough to recycle the arena words the original occupied.
#[test]
fn copy_out_survives_guard_drop_and_arena_churn() {
    let atoms = AtomTable::new();
    let mut source = Heap::new(64);
    let shared = shared_table(TableKind::Set);

    let original = pair(&mut source, 1, 10);
    put(&shared, original, &atoms);

    // Copy the object out while the guard is alive.
    let mut owned = TermStore::with_default_allocator();
    let copy = copy_lookup(&shared, make_small(1), &atoms, &mut owned)
        .expect("the key is present");
    // The guard is gone by now (it lived inside `with_read`).

    // Churn: delete the entry, then insert a *same-word-size* object. The store
    // frees the deleted object's runs and hands them straight back to this copy.
    shared
        .with_write(|write| write.delete(make_small(1), &atoms))
        .expect("no poisoned lock")
        .expect("deleting a present key succeeds");
    let replacement = pair(&mut source, 1, 999);
    put(&shared, replacement, &atoms);
    shared
        .with_write(|write| write.insert(pair(&mut source, 2, 2), &atoms))
        .expect("no poisoned lock")
        .expect("a supported object inserts");

    // The table really does hold the replacement...
    assert!(
        shared
            .with_read(|read| {
                let hits = read.lookup(make_small(1), &atoms);
                hits.len() == 1 && hits[0].eq_term(replacement, &atoms)
            })
            .expect("no poisoned lock")
    );
    // ...and the copy taken before the churn still holds the *original*: it lives
    // in the caller-owned store, which the table never touches.
    assert!(
        eq(copy, original, &atoms),
        "the copied-out term must still be the original {{1,10}}"
    );
    assert!(
        !eq(copy, replacement, &atoms),
        "the copied-out term must not have become the replacement"
    );
}

/// **The hazard, demonstrated deterministically through the raw [`Table`] API.**
///
/// Because `Table` hands out bare `Eterm`s that alias table storage, a term read
/// before a delete + same-size insert silently re-reads as the *new* object —
/// a well-formed wrong result, exactly what the guard-scoped views prevent.
#[test]
fn a_stale_raw_term_reads_as_a_different_object_after_arena_churn() {
    let atoms = AtomTable::new();
    let mut source = Heap::new(64);
    let mut table = Table::new(TableId::next(), TableOptions::new()).expect("a fresh table");

    let original = pair(&mut source, 1, 10);
    table.insert(original, &atoms).expect("insert into a fresh table");

    // A bare term read out of the table: no guard, nothing ties it to the table.
    let stale = table.lookup(make_small(1), &atoms)[0];
    assert!(eq(stale, original, &atoms), "the read backs the inserted object");

    // Delete it and insert a *same-word-size* object (3 words: header + 2).
    table.delete(make_small(1), &atoms).expect("the key is present");
    let replacement = pair(&mut source, 1, 999);
    table.insert(replacement, &atoms).expect("insert into an empty table");

    let reread = table.lookup(make_small(1), &atoms)[0];
    assert_eq!(
        stale, reread,
        "the store must have recycled the freed words (this is what makes the hazard real)"
    );
    assert!(
        eq(stale, replacement, &atoms),
        "the stale term now reads as the replacement object"
    );
    assert!(
        !eq(stale, original, &atoms),
        "the stale term no longer reads as the object it was read from"
    );
}

// ---------------------------------------------------------------------------
// 3. The views' reads and owned data
// ---------------------------------------------------------------------------

/// Owned metadata escapes the guard freely; tagged terms do not (they are only
/// reachable inside the closure, or as an explicit copy-out).
#[test]
fn owned_metadata_escapes_the_guard_while_terms_do_not() {
    let atoms = AtomTable::new();
    let mut source = Heap::new(64);
    let shared = shared_table(TableKind::OrderedSet);
    put(&shared, pair(&mut source, 1, 10), &atoms);
    put(&shared, pair(&mut source, 2, 20), &atoms);

    let owner = crate::table::OwnerId::new(3);
    shared
        .with_write(|write| write.set_owner(Some(owner)))
        .expect("no poisoned lock");

    // Everything here is `Copy`/owned, so it may outlive the guard.
    let snapshot = shared
        .with_read(|read| {
            let owned_keys: Vec<i64> = read
                .tab2list()
                .iter()
                .filter_map(|object| {
                    object
                        .tuple_element(0)
                        .and_then(|key| key.as_small_int())
                })
                .collect();
            (
                read.id(),
                read.kind(),
                read.keypos(),
                read.access(),
                read.size(),
                read.is_empty(),
                read.memory(),
                read.named_table(),
                read.name(),
                read.owner(),
                read.is_fixed(),
                read.info(),
                owned_keys,
            )
        })
        .expect("no poisoned lock");

    assert_eq!(snapshot.1, TableKind::OrderedSet);
    assert_eq!(snapshot.2, 1);
    assert_eq!(snapshot.3, Access::Public);
    assert_eq!(snapshot.4, 2);
    assert!(!snapshot.5);
    assert!(snapshot.6 >= 2, "two objects need at least two words");
    assert!(!snapshot.7);
    assert_eq!(snapshot.8, None);
    assert_eq!(snapshot.9, Some(owner));
    assert!(!snapshot.10);
    assert_eq!(snapshot.11.size, 2);
    assert_eq!(snapshot.12, vec![1, 2]);
    assert_eq!(snapshot.0, shared.id());
}

/// The tagged reads expose the object's structure without ever handing out a bare
/// term.
#[test]
fn tagged_reads_expose_structure_but_never_a_bare_term() {
    let mut atoms = AtomTable::new();
    let mut source = Heap::new(64);
    let shared = shared_table(TableKind::Set);

    let inner = source
        .tuple(&[make_small(7), atom(&mut atoms, "seven")])
        .expect("the test arena has room");
    let object = source
        .tuple(&[make_small(1), inner])
        .expect("the test arena has room");
    put(&shared, object, &atoms);

    // Inside the guard: structural access, plus an owned extraction.
    let extracted = shared
        .with_read(|read| {
            let hits = read.lookup(make_small(1), &atoms);
            assert_eq!(hits.len(), 1);
            let hit: &TableTerm<'_> = &hits[0];
            assert!(hit.is_tuple());
            assert!(!hit.is_list() && !hit.is_binary() && !hit.is_map());
            assert!(!hit.is_pid() && !hit.is_port() && !hit.is_reference());
            assert!(!hit.is_function() && !hit.is_float());
            assert_eq!(hit.tuple_arity(), Some(2));
            assert_eq!(hit.tuple_element(0).and_then(|k| k.as_small_int()), Some(1));
            assert!(hit.tuple_element(2).is_none(), "out-of-range element");
            let nested = hit.tuple_element(1).expect("a nested tuple");
            assert!(nested.is_tuple());
            assert!(!nested.is_integer() && !nested.is_atom());
            let third = nested.tuple_element(1).expect("the nested atom");
            assert!(third.is_atom());
            assert_eq!(third.as_atom(), atoms.lookup("seven"));
            assert_eq!(
                nested.tuple_element(0).and_then(|n| n.as_small_int()),
                Some(7)
            );
            assert!(hit.eq_term(object, &atoms), "eq_term against the source term");
            assert_eq!(hit.cmp_term(make_small(0), &atoms), core::cmp::Ordering::Greater);
            assert!(hit.size() >= 3, "a 2-tuple whose second element is a tuple");
            // Everything owned here escapes; nothing tagged does.
            (
                hit.tuple_arity(),
                third.as_atom(),
                third.as_atom().and_then(|a| atoms.name(a)).map(str::to_owned),
            )
        })
        .expect("no poisoned lock");

    assert_eq!(extracted.0, Some(2));
    assert_eq!(extracted.1, atoms.lookup("seven"));
    assert_eq!(extracted.2.as_deref(), Some("seven"));
}

/// `TableTerm`'s negative cases: every typed accessor answers `None` for a term
/// that is not of that type, and a non-tuple has no arity/elements.
#[test]
fn accessors_answer_none_for_the_wrong_term_class() {
    let mut atoms = AtomTable::new();
    let mut source = Heap::new(64);
    let shared = shared_table(TableKind::Set);

    let an_atom = atom(&mut atoms, "marker");
    put(&shared, pair(&mut source, 1, 10), &atoms);
    put(
        &shared,
        source.tuple(&[make_small(2), an_atom]).expect("room"),
        &atoms,
    );
    put(
        &shared,
        source
            .tuple(&[make_small(3), NIL])
            .expect("room"),
        &atoms,
    );
    put(&shared, float_pair(&mut source, 4, 1.5), &atoms);

    shared
        .with_read(|read| {
            let object = &read.lookup(make_small(1), &atoms)[0];
            assert!(object.is_integer() || object.is_tuple());
            assert_eq!(object.as_small_int(), None, "a tuple is not a small int");
            assert_eq!(object.as_atom(), None);
            assert_eq!(object.as_float(), None);
            assert_eq!(
                object.tuple_element(0).and_then(|k| k.as_small_int()),
                Some(1),
                "the key element *is* a small int"
            );

            let object = &read.lookup(make_small(2), &atoms)[0];
            let element = object.tuple_element(1).expect("the atom element");
            assert!(element.is_atom());
            assert_eq!(element.as_small_int(), None);
            assert_eq!(element.as_float(), None);
            assert_eq!(element.tuple_arity(), None);
            assert!(element.tuple_element(0).is_none());
            assert!(element.list_to_terms().len() == 1, "a non-list is one term");
            assert_eq!(element.size(), 0, "an immediate occupies no heap words");

            let object = &read.lookup(make_small(3), &atoms)[0];
            let empty = object.tuple_element(1).expect("the empty list");
            assert!(empty.is_list());
            assert!(empty.list_to_terms().is_empty());
            assert!(!empty.is_binary() && !empty.is_integer());

            let object = &read.lookup(make_small(4), &atoms)[0];
            let float = object.tuple_element(1).expect("the float");
            assert!(float.is_float());
            assert_eq!(float.as_float(), Some(1.5));
            assert!(!float.is_integer());
        })
        .expect("no poisoned lock");
}

/// `list_to_terms` walks a proper list with `as_small_int`, and keeps an improper
/// tail as its last element.
#[test]
fn tagged_list_reads_walk_proper_and_improper_lists() {
    let atoms = AtomTable::new();
    let mut source = Heap::new(64);
    let shared = shared_table(TableKind::Set);

    let tail = source.cons(make_small(3), NIL).expect("room");
    let middle = source.cons(make_small(2), tail).expect("room");
    let proper = source.cons(make_small(1), middle).expect("room");
    // `[1, 2 | 4]` — an improper list whose tail is the non-list `4`.
    let two = source.cons(make_small(2), make_small(4)).expect("room");
    let improper = source.cons(make_small(1), two).expect("room");
    let first_object = source.tuple(&[make_small(1), proper]).expect("room");
    let second_object = source.tuple(&[make_small(2), improper]).expect("room");
    put(&shared, first_object, &atoms);
    put(&shared, second_object, &atoms);

    shared
        .with_read(|read| {
            let object = &read.lookup(make_small(1), &atoms)[0];
            let list = object.tuple_element(1).expect("the list");
            assert!(list.is_list());
            let values: Vec<i64> = list
                .list_to_terms()
                .iter()
                .map(|term| term.as_small_int().expect("a small int"))
                .collect();
            assert_eq!(values, vec![1, 2, 3]);

            // `[1, 2 | 4]`: the improper tail is kept as the last element rather
            // than being dropped.
            let object = &read.lookup(make_small(2), &atoms)[0];
            let list = object.tuple_element(1).expect("the list");
            let values: Vec<i64> = list
                .list_to_terms()
                .iter()
                .map(|term| term.as_small_int().expect("a small int"))
                .collect();
            assert_eq!(values, vec![1, 2, 4]);
        })
        .expect("no poisoned lock");
}

/// Navigation returns tagged keys (and the absent key is `badarg`, an owned
/// error — not a term).
#[test]
fn navigation_yields_tagged_keys() {
    let atoms = AtomTable::new();
    let mut source = Heap::new(64);
    let shared = shared_table(TableKind::OrderedSet);
    for key in 1..=4 {
        put(&shared, pair(&mut source, key, key * 10), &atoms);
    }

    let walk = shared
        .with_read(|read| {
            let mut keys = Vec::new();
            let mut cursor = read.first(&atoms).map(|key| key.as_small_int());
            while let Some(Some(key)) = cursor {
                keys.push(key);
                let next = read
                    .next(make_small(key), &atoms)
                    .expect("an ordered table treats any key as a range bound");
                cursor = next.map(|term| term.as_small_int());
            }
            let backwards = read
                .prev(make_small(4), &atoms)
                .expect("an ordered table treats any key as a range bound")
                .expect("a key below 4 exists")
                .as_small_int();
            (
                keys,
                read.last(&atoms).and_then(|key| key.as_small_int()),
                backwards,
                read.first(&atoms).and_then(|key| key.as_small_int()),
            )
        })
        .expect("no poisoned lock");

    assert_eq!(walk.0, vec![1, 2, 3, 4]);
    assert_eq!(walk.1, Some(4));
    assert_eq!(walk.2, Some(3));
    assert_eq!(walk.3, Some(1));

    // An absent anchor on a hash kind is `badarg` — a typed error, owned, so it
    // escapes the guard freely.
    let hash = shared_table(TableKind::Set);
    put(&hash, pair(&mut source, 1, 10), &atoms);
    let absent_anchor_is_badarg = hash
        .with_read(|read| {
            matches!(
                read.next(make_small(42), &atoms),
                Err(beam_common::BeamError::BadArg)
            )
        })
        .expect("no poisoned lock");
    assert!(absent_anchor_is_badarg, "a hash kind needs a present anchor");
}

/// `lookup_element*` works through the view, including the `/4` default form for
/// an absent key and the multi-kind list form.
#[test]
fn tagged_element_reads_cover_the_lookup_element_family() {
    let atoms = AtomTable::new();
    let mut source = Heap::new(64);
    let shared = shared_table(TableKind::Set);
    put(&shared, pair(&mut source, 1, 10), &atoms);

    let mut owned = TermStore::with_default_allocator();
    let (present, defaulted, absent_all) = shared
        .with_read(|read| {
            let present = read
                .lookup_element(make_small(1), 2, &atoms)
                .expect("a present key with a valid position")
                .copy_into(&mut owned)
                .expect("copyable");
            let defaulted = read
                .lookup_element_or(make_small(9), 2, make_small(-1), &atoms)
                .expect("an absent key yields the default")
                .copy_into(&mut owned)
                .expect("copyable");
            let absent_all = read
                .lookup_element_all_or(make_small(9), 2, make_small(-1), &atoms)
                .expect("an absent key is Ok(None)")
                .is_none();
            (present, defaulted, absent_all)
        })
        .expect("no poisoned lock");

    assert!(eq(present, make_small(10), &atoms));
    assert!(eq(defaulted, make_small(-1), &atoms));
    assert!(absent_all);

    // The multi-kind list form.
    let bag = shared_table(TableKind::Bag);
    put(&bag, pair(&mut source, 1, 10), &atoms);
    put(&bag, pair(&mut source, 1, 20), &atoms);
    let mut owned = TermStore::with_default_allocator();
    let values = bag
        .with_read(|read| {
            read.lookup_element_all(make_small(1), 2, &atoms)
                .expect("the multi-kind list form")
                .iter()
                .map(|term| term.copy_into(&mut owned))
                .collect::<beam_common::Result<Vec<_>>>()
        })
        .expect("no poisoned lock")
        .expect("copyable");
    assert_eq!(values.len(), 2);
    assert!(values.iter().all(|value| eq(*value, make_small(10), &atoms) || eq(*value, make_small(20), &atoms)));
}

/// `TableRead::member` reports key presence — a plain `bool`, so nothing tagged
/// escapes — for both an ordered and a hash kind.
#[test]
fn member_reports_key_presence_across_kinds() {
    let atoms = AtomTable::new();
    let mut source = Heap::new(64);

    for kind in [TableKind::Set, TableKind::OrderedSet] {
        let shared = shared_table(kind);
        put(&shared, pair(&mut source, 1, 10), &atoms);
        put(&shared, pair(&mut source, 2, 20), &atoms);

        let (first, second, absent) = shared
            .with_read(|read| {
                (
                    read.member(make_small(1), &atoms),
                    read.member(make_small(2), &atoms),
                    read.member(make_small(9), &atoms),
                )
            })
            .expect("no poisoned lock");

        assert!(first, "{kind:?}: a stored key is a member");
        assert!(second, "{kind:?}: every stored key is a member");
        assert!(!absent, "{kind:?}: an absent key is not a member");
    }
}

/// `TableTerm::is_bitstring` distinguishes a **byte-aligned** binary from a
/// **non-byte-aligned** bitstring. Both are read back through the table, which
/// deep-copies them into table storage (the A.1 copy boundary).
#[test]
fn bitstring_reads_distinguish_aligned_binaries_from_unaligned_bits() {
    use beam_term::bitstring::{heap_bits_size_words, write_binary, write_heap_bits};

    let atoms = AtomTable::new();
    let shared = shared_table(TableKind::Set);

    // The bitstring writers need `Eterm`-aligned words and hand back a term that
    // points into them, so the arena must outlive the insert (which deep-copies).
    let binary_words = heap_bits_size_words(24);
    let bits_words = heap_bits_size_words(13);
    let mut source = Heap::new(binary_words + bits_words + 32);

    // A byte-aligned binary `<<0,1,2>>` (24 bits).
    // SAFETY: `binary_words == heap_bits_size_words(24)` words are reserved, which
    // is exactly what `write_binary` fills for a 3-byte binary.
    let binary = unsafe {
        let hp = source.alloc_words(binary_words).expect("arena room");
        write_binary(hp, &[0u8, 1, 2]).expect("3 bytes is a 24-bit binary")
    };
    // A 13-bit string `<<16#146B:13>>` (not a whole number of bytes).
    // SAFETY: `bits_words == heap_bits_size_words(13)` words are reserved, which is
    // exactly what `write_heap_bits` fills for a 13-bit string.
    let bits = unsafe {
        let hp = source.alloc_words(bits_words).expect("arena room");
        write_heap_bits(hp, &[0xA3, 0x5C], 13).expect("a bitstring is writable")
    };

    // Wrap each in a tuple: an inserted object must be a tuple (its key element is
    // what the table indexes on), and the store deep-copies it in.
    put(
        &shared,
        source.tuple(&[make_small(1), binary]).expect("arena room"),
        &atoms,
    );
    put(
        &shared,
        source.tuple(&[make_small(2), bits]).expect("arena room"),
        &atoms,
    );

    let (aligned, unaligned) = shared
        .with_read(|read| {
            let aligned = read.lookup(make_small(1), &atoms)[0]
                .tuple_element(1)
                .expect("the binary element");
            let unaligned = read.lookup(make_small(2), &atoms)[0]
                .tuple_element(1)
                .expect("the bitstring element");
            (
                (aligned.is_bitstring(), aligned.is_binary()),
                (unaligned.is_bitstring(), unaligned.is_binary()),
            )
        })
        .expect("no poisoned lock");

    assert_eq!(
        aligned,
        (true, true),
        "a 24-bit binary is both a bitstring and a binary"
    );
    assert_eq!(
        unaligned,
        (true, false),
        "a 13-bit string is a bitstring but not a binary"
    );
}

// ---------------------------------------------------------------------------
// 4. The write view
// ---------------------------------------------------------------------------

/// Every mutation the write view exposes behaves like the raw `Table` method, and
/// the one operation that hands back table storage (`take`) hands it back tagged.
#[test]
fn write_view_mutations_match_the_raw_table() {
    let atoms = AtomTable::new();
    let mut source = Heap::new(64);
    let shared = shared_table(TableKind::OrderedSet);

    let inserted = shared
        .with_write(|write| {
            write.insert(pair(&mut source, 1, 10), &atoms)?;
            write.insert(pair(&mut source, 2, 20), &atoms)?;
            let fresh = write.insert_new(pair(&mut source, 3, 30), &atoms)?;
            let duplicate = write.insert_new(pair(&mut source, 1, 99), &atoms)?;
            let batch = [
                pair(&mut source, 4, 40),
                pair(&mut source, 5, 50),
                pair(&mut source, 6, 60),
            ];
            write.insert_list(&batch, &atoms)?;
            Ok::<_, beam_common::BeamError>((fresh, duplicate, write.size()))
        })
        .expect("no poisoned lock")
        .expect("every object is supported");
    assert_eq!(inserted, (true, false, 6));

    // `update_counter` builds its result on the *caller's* heap, never in table
    // storage, so the result is owned.
    let mut out = OutHeap::new(256).expect("a caller heap");
    let counted = shared
        .with_write(|write| {
            let op = make_small(5);
            write.update_counter(make_small(1), op, &atoms, &mut out)
        })
        .expect("no poisoned lock")
        .expect("a present key updates");
    assert!(eq(counted, make_small(15), &atoms));

    let element = shared
        .with_write(|write| write.update_element(make_small(2), &[(2, make_small(200))], &atoms, &mut out))
        .expect("no poisoned lock")
        .expect("a present key updates");
    assert!(element);

    // `delete_object` compares whole objects; `delete` compares keys.
    shared
        .with_write(|write| {
            write.delete_object(pair(&mut source, 2, 200), &atoms)?;
            write.delete(make_small(3), &atoms)
        })
        .expect("no poisoned lock")
        .expect("delete_object/delete succeed");
    assert_eq!(shared.with_read(|read| read.size()).expect("no poisoned lock"), 4);

    // Fixation is a write-view operation.
    shared
        .with_write(|write| write.set_fixation(true))
        .expect("no poisoned lock");
    assert!(shared.with_read(|read| read.is_fixed()).expect("no poisoned lock"));

    // `take` hands back table storage: tagged, and `copy_into`able.
    let mut owned = TermStore::with_default_allocator();
    let taken = shared
        .with_write(|write| {
            let objects = write.take(make_small(1), &atoms)?;
            assert_eq!(objects.len(), 1);
            let copy = objects[0].copy_into(&mut owned)?;
            Ok::<_, beam_common::BeamError>(copy)
        })
        .expect("no poisoned lock")
        .expect("take succeeds");
    assert!(eq(taken, pair(&mut source, 1, 15), &atoms));
    assert_eq!(shared.with_read(|read| read.size()).expect("no poisoned lock"), 3);

    // The copy survives the table being emptied and refilled.
    shared
        .with_write(|write| {
            write.delete_all_objects();
            write.insert(pair(&mut source, 1, 15), &atoms)
        })
        .expect("no poisoned lock")
        .expect("a supported object inserts");
    assert!(
        eq(taken, pair(&mut source, 1, 15), &atoms),
        "the taken copy is caller-owned storage"
    );

    // The mutating select family.
    let deleted = shared
        .with_write(|write| write.match_delete(pair(&mut source, 1, 15), &atoms, &mut out))
        .expect("no poisoned lock")
        .expect("match_delete succeeds");
    assert_eq!(deleted, 1);
    assert_eq!(shared.with_read(|read| read.size()).expect("no poisoned lock"), 0);
}

/// The ownership operations are reachable from the write view.
#[test]
fn write_view_exposes_the_ownership_operations() {
    use crate::table::{Heir, OwnerId, SetOpt};

    let shared = shared_table(TableKind::Set);
    let first = OwnerId::new(1);
    let second = OwnerId::new(2);

    shared
        .with_write(|write| {
            write.set_owner(Some(first));
            write.set_heir(Some(Heir {
                owner: second,
                data: Some(make_small(7)),
            }))
        })
        .expect("no poisoned lock");
    assert_eq!(
        shared.with_read(|read| read.owner()).expect("no poisoned lock"),
        Some(first)
    );
    let heir_owner = shared
        .with_write(|write| {
            // The gift data is a *tagged* term read under the guard, so the heir view
            // is confined to this block: only its owned fields leave it.
            let (data, owner) = {
                let heir = write.heir().expect("the heir is installed");
                (
                    heir.data().and_then(|data| data.as_small_int()),
                    heir.owner(),
                )
            };
            assert_eq!(data, Some(7));
            assert!(write.give_away(Some(first), Some(second)).is_ok());
            owner
        })
        .expect("no poisoned lock");
    assert_eq!(heir_owner, second);
    assert_eq!(
        shared.with_read(|read| read.owner()).expect("no poisoned lock"),
        Some(second)
    );
    shared
        .with_write(|write| write.setopts(Some(second), SetOpt::ClearHeir))
        .expect("no poisoned lock")
        .expect("the owner may clear the heir");
    assert!(shared
        .with_write(|write| write.heir().is_none())
        .expect("no poisoned lock"));
}

/// The **gift-carrying** transfer is reachable from the write view: it moves
/// ownership and returns the `{'ETS-TRANSFER', …}` obligation
/// ([`Table::give_away_with_gift`]).
#[test]
fn write_view_exposes_the_gift_carrying_transfer() {
    use crate::table::OwnerId;

    let shared = shared_table(TableKind::Set);
    let giver = OwnerId::new(1);
    let recipient = OwnerId::new(2);
    shared
        .with_write(|write| write.set_owner(Some(giver)))
        .expect("no poisoned lock");

    let transfer = shared
        .with_write(|write| {
            write.give_away_with_gift(Some(giver), Some(recipient), make_small(9))
        })
        .expect("no poisoned lock")
        .expect("the owner may transfer with a gift");
    assert_eq!(transfer.to, recipient);
    assert_eq!(transfer.from, Some(giver));
    assert_eq!(transfer.data, make_small(9));
    assert_eq!(
        shared.with_read(|read| read.owner()).expect("no poisoned lock"),
        Some(recipient)
    );
}

/// **The residual hazard of `as_requester`, pinned.** Its reads are bare `Eterm`s
/// that alias table storage (by design — it is the raw access-enforcement seam),
/// so a term read there escapes the guard and silently re-reads as a *different,
/// well-formed object* once the store recycles its words. The recycling assertion
/// makes this fail loudly if the allocator ever stops reusing free runs.
#[test]
fn as_requester_reads_are_a_documented_residual_hazard() {
    use crate::table::OwnerId;

    let atoms = AtomTable::new();
    let mut source = Heap::new(64);
    let shared = shared_table(TableKind::Set);
    let owner = OwnerId::new(7);
    shared
        .with_write(|write| write.set_owner(Some(owner)))
        .expect("no poisoned lock");

    let original = pair(&mut source, 1, 10);
    put(&shared, original, &atoms);

    // A bare term read through the access-checked raw view, taken *out* of the
    // closure — nothing tags it to the guard.
    let escaped = shared
        .with_write(|write| {
            let hits = write
                .as_requester(Some(owner))
                .lookup(make_small(1), &atoms)
                .expect("the owner may read");
            hits[0]
        })
        .expect("no poisoned lock");
    assert!(eq(escaped, original, &atoms), "the read backs the inserted object");

    // Churn: delete the entry and insert a *same-word-size* object; the store frees
    // the deleted object's runs and hands them straight back to the replacement.
    shared
        .with_write(|write| write.delete(make_small(1), &atoms))
        .expect("no poisoned lock")
        .expect("the key is present");
    let replacement = pair(&mut source, 1, 999);
    put(&shared, replacement, &atoms);

    let reread = shared
        .with_write(|write| {
            write
                .as_requester(Some(owner))
                .lookup(make_small(1), &atoms)
                .expect("the owner may read")[0]
        })
        .expect("no poisoned lock");
    assert_eq!(
        escaped, reread,
        "the store must have recycled the freed words — without recycling this hazard test is vacuous"
    );
    assert!(
        eq(escaped, replacement, &atoms),
        "the escaped term now reads as the replacement object"
    );
    assert!(
        !eq(escaped, original, &atoms),
        "the escaped term no longer reads as the object it was read from"
    );
}

// ---------------------------------------------------------------------------
// 5. The select family: copy-out only
// ---------------------------------------------------------------------------

/// `match_object`'s `'$_'` bodies return the **stored** object, so its results are
/// the strongest case of aliasing. The copy-out form hands back owned terms that
/// survive the table being emptied and refilled.
#[test]
fn match_object_copy_out_owns_terms_that_survive_churn() {
    let mut atoms = AtomTable::new();
    let mut source = Heap::new(128);
    let shared = shared_table(TableKind::OrderedSet);
    for key in 1..=3 {
        put(&shared, pair(&mut source, key, key * 10), &atoms);
    }

    // `{'_', '_'}` matches every object; the body is `'$_'` (the object itself).
    let pattern = source
        .tuple(&[atom(&mut atoms, "_"), atom(&mut atoms, "_")])
        .expect("the test arena has room");
    let mut owned = TermStore::with_default_allocator();
    let results = shared
        .with_read(|read| read.match_object(pattern, &atoms, &mut owned))
        .expect("no poisoned lock")
        .expect("the pattern compiles and matches");
    assert_eq!(results.len(), 3);

    // Empty the table and insert same-size replacements: the arena words the
    // results used to alias are now recycled.
    shared
        .with_write(|write| {
            write.delete_all_objects();
            for key in 1..=3 {
                write.insert(pair(&mut source, key, key * 100), &atoms)?;
            }
            Ok::<_, beam_common::BeamError>(())
        })
        .expect("no poisoned lock")
        .expect("a supported object inserts");

    for (index, result) in results.iter().enumerate() {
        let key = index as i64 + 1;
        assert!(
            eq(*result, pair(&mut source, key, key * 10), &atoms),
            "result {index} must still be the object read under the guard"
        );
    }
}

/// A body that *builds* a term allocates on the view's scratch heap; a result set
/// larger than the starting scratch heap must grow it and still return every
/// result (the retry path).
#[test]
fn select_copy_out_grows_the_scratch_heap_when_needed() {
    let mut atoms = AtomTable::new();
    let mut source = Heap::new(1 << 16);
    let shared = shared_table(TableKind::OrderedSet);
    const ROWS: i64 = 2_000;
    for key in 1..=ROWS {
        put(&shared, pair(&mut source, key, key), &atoms);
    }

    // Body `{{'$1','$1'}}` constructs a fresh 2-tuple per matched object (3 words
    // each): 6 000 words, far past the 1 024-word starting scratch heap. (The
    // `{{ … }}` one-element-tuple form is the C's tuple-construct syntax.)
    let spec = {
        let var = atom(&mut atoms, "$1");
        let template = source.tuple(&[var, var]).expect("the test arena has room");
        let built = source.tuple(&[template]).expect("the test arena has room");
        spec(&mut source, &mut atoms, built)
    };

    let mut owned = TermStore::with_default_allocator();
    let results = shared
        .with_read(|read| read.select(&spec, &atoms, &mut owned))
        .expect("no poisoned lock")
        .expect("the built tuple is supported");
    assert_eq!(results.len(), ROWS as usize);
    for (index, result) in results.iter().enumerate() {
        let key = index as i64 + 1;
        // The head binds the whole object, so the body builds `{'{'k,k'}', '{'k,k'}'}`.
        let object = pair(&mut source, key, key);
        let expected = source
            .tuple(&[object, object])
            .expect("the test arena has room");
        assert!(
            eq(*result, expected, &atoms),
            "the built tuple for row {key} must be a 2-tuple of the row object"
        );
    }

    // `select_count` uses the same scratch heap; the body must be `true`.
    let counted = shared
        .with_read(|read| read.select_count(&spec, &atoms))
        .expect("no poisoned lock")
        .expect("countable");
    assert_eq!(counted, 0, "a tuple body is never `true`");
}

/// Every tagged term handed out by a read view is copyable, and the copy equals the
/// term it came from — the copy-out contract holds for all the read shapes.
#[test]
fn every_read_shape_copies_out_faithfully() {
    let atoms = AtomTable::new();
    let mut source = Heap::new(256);
    let shared = shared_table(TableKind::OrderedSet);
    for key in 1..=3 {
        put(&shared, pair(&mut source, key, key * 10), &atoms);
    }

    let mut owned = TermStore::with_default_allocator();
    let checked = shared
        .with_read(|read| {
            let mut seen = Vec::new();
            for term in read.tab2list() {
                seen.push(term.copy_into(&mut owned)?);
            }
            for key in [1, 2, 3] {
                seen.push(
                    read.lookup_element(make_small(key), 1, &atoms)?
                        .copy_into(&mut owned)?,
                );
            }
            if let Some(term) = read.first(&atoms) {
                seen.push(term.copy_into(&mut owned)?);
            }
            if let Some(term) = read.last(&atoms) {
                seen.push(term.copy_into(&mut owned)?);
            }
            if let Some(term) = read.next(make_small(1), &atoms)? {
                seen.push(term.copy_into(&mut owned)?);
            }
            if let Some(term) = read.prev(make_small(3), &atoms)? {
                seen.push(term.copy_into(&mut owned)?);
            }
            Ok::<_, beam_common::BeamError>(seen)
        })
        .expect("no poisoned lock")
        .expect("every read is copyable");
    assert_eq!(checked.len(), 3 + 3 + 4);
    // The `tab2list` copies are the three objects; the rest are keys/immediates.
    assert!(eq(checked[0], pair(&mut source, 1, 10), &atoms));
    assert!(eq(checked[1], pair(&mut source, 2, 20), &atoms));
    assert!(eq(checked[2], pair(&mut source, 3, 30), &atoms));
    assert!(eq(checked[3], make_small(1), &atoms));
    assert!(eq(checked[6], make_small(1), &atoms), "first");
    assert!(eq(checked[7], make_small(3), &atoms), "last");
    assert!(eq(checked[8], make_small(2), &atoms), "next of 1");
    assert!(eq(checked[9], make_small(2), &atoms), "prev of 3");
}

// ---------------------------------------------------------------------------
// 6. The views' function signatures (the compile-time contract)
// ---------------------------------------------------------------------------

/// The view types are used exactly as documented: closures over `&mut` views that
/// return owned data, and a scoped `read()`/`write()` pair.
#[test]
fn views_are_usable_through_the_documented_shapes() {
    let atoms = AtomTable::new();
    let mut source = Heap::new(64);
    let shared = shared_table(TableKind::Set);

    // Closure form, returning owned data.
    let inserted: bool = shared
        .with_write(|write: &mut TableWrite<'_>| {
            write.insert(pair(&mut source, 1, 10), &atoms).unwrap();
            write.size() == 1
        })
        .expect("no poisoned lock");
    assert!(inserted);

    // Scoped handle form; the terms stay inside the block.
    let value = {
        let read: TableRead<'_> = shared.read().expect("no poisoned lock");
        let hits = read.lookup(make_small(1), &atoms);
        assert_eq!(hits.len(), 1);
        hits[0].as_small_int()
    };
    assert_eq!(value, None, "the object is a tuple, not an integer");

    // The write handle is a first-class value too.
    let mut write: TableWrite<'_> = shared.write().expect("no poisoned lock");
    write.delete(make_small(1), &atoms).expect("the key is present");
    assert_eq!(write.size(), 0);
    drop(write);
}

/// The views borrow the handle, so the same `SharedTable` cannot be read and
/// written at once (a compile-time property asserted here by construction: the
/// write happens after the read guard is dropped).
#[test]
fn a_read_view_does_not_block_a_later_write() {
    let atoms = AtomTable::new();
    let mut source = Heap::new(64);
    let shared = shared_table(TableKind::Set);
    put(&shared, pair(&mut source, 1, 10), &atoms);

    let seen = {
        let read = shared.read().expect("no poisoned lock");
        read.size()
    };
    shared
        .with_write(|write| write.delete(make_small(1), &atoms))
        .expect("no poisoned lock")
        .expect("the key is present after the read guard is gone");
    assert_eq!(seen, 1);
    assert_eq!(shared.with_read(|read| read.size()).expect("no poisoned lock"), 0);
}
