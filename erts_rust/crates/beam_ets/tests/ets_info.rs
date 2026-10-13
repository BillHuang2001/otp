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

//! Metadata, identifiers, allocation and the `info`/`memory` snapshot.

mod common;

use beam_alloc::{AllocatorConfig, CarrierAllocator};
use beam_ets::{Access, OwnerId, Table, TableId, TableKind, TableOptions};
use beam_term::prelude::*;
use common::*;

#[test]
fn metadata_getters_and_info_entries() {
    let mut atoms = AtomTable::new();
    let name = atoms.intern("my_table");

    let mut opts = TableOptions::new();
    opts.set_kind(TableKind::OrderedSet)
        .set_keypos(2)
        .unwrap()
        .set_access(Access::Public)
        .set_name(name)
        .set_compressed(true)
        .set_write_concurrency(true)
        .set_read_concurrency(true);

    let id = TableId::new(42);
    let mut t = Table::new(id, opts).unwrap();

    assert_eq!(t.id(), id);
    assert_eq!(t.id().raw(), 42);
    assert_eq!(t.kind(), TableKind::OrderedSet);
    assert_eq!(t.name(), Some(name));
    assert_eq!(t.keypos(), 2);
    assert_eq!(t.access(), Access::Public);
    assert_eq!(t.owner(), None);

    let owner = OwnerId::new(7);
    t.set_owner(Some(owner));
    assert_eq!(t.owner(), Some(owner));
    assert_eq!(owner.raw(), 7);
    t.set_owner(None);
    assert_eq!(t.owner(), None);

    let info = t.info();
    assert_eq!(info.id, id);
    assert_eq!(info.kind, TableKind::OrderedSet);
    assert_eq!(info.keypos, 2);
    assert_eq!(info.access, Access::Public);
    assert_eq!(info.size, 0);
    assert_eq!(info.owner, None);
    assert!(info.compressed);
    assert!(info.write_concurrency);
    assert!(info.read_concurrency);

    let entries = info.entries(&mut atoms);
    assert_eq!(info_entry(&entries, &atoms, "id"), Some(make_small(42)));
    assert_eq!(info_entry(&entries, &atoms, "keypos"), Some(make_small(2)));
    assert_eq!(info_entry(&entries, &atoms, "size"), Some(make_small(0)));
    assert_eq!(
        info_entry(&entries, &atoms, "type"),
        Some(make_atom(atoms.lookup("ordered_set").unwrap()))
    );
    assert_eq!(
        info_entry(&entries, &atoms, "protection"),
        Some(make_atom(atoms.lookup("public").unwrap()))
    );
    assert_eq!(info_entry(&entries, &atoms, "name"), Some(make_atom(name)));
    assert_eq!(
        info_entry(&entries, &atoms, "owner"),
        Some(make_atom(atoms.lookup("undefined").unwrap()))
    );
    assert_eq!(
        info_entry(&entries, &atoms, "compressed"),
        Some(make_atom(atoms.lookup("true").unwrap()))
    );
    assert_eq!(
        info_entry(&entries, &atoms, "write_concurrency"),
        Some(make_atom(atoms.lookup("true").unwrap()))
    );
    assert_eq!(
        info_entry(&entries, &atoms, "read_concurrency"),
        Some(make_atom(atoms.lookup("true").unwrap()))
    );

    // An unnamed table reports the atom `undefined` for `name`.
    let plain = Table::new(TableId::next(), TableOptions::new()).unwrap();
    let plain_entries = plain.info().entries(&mut atoms);
    assert_eq!(
        info_entry(&plain_entries, &atoms, "name"),
        Some(make_atom(atoms.lookup("undefined").unwrap()))
    );
}

/// A `private` table reports both concurrency flags as `false`, whatever was
/// requested; `public`/`protected` report the requested values, and `compressed`
/// is **not** forced off.
///
/// Verified against the system `erl` (OTP 29 / erts-17.1), which clears the
/// fine-locked flag for `DB_PRIVATE` tables:
/// * `ets:new(t, [private, {write_concurrency,true}, {read_concurrency,true}])` →
///   `write_concurrency => false`, `read_concurrency => false`;
/// * the same options on a `protected`/`public` table → `true`/`true`;
/// * `ets:new(t, [private, compressed])` → `compressed => true`.
#[test]
fn private_tables_report_the_concurrency_flags_as_false() {
    let mut atoms = AtomTable::new();
    let true_atom = make_atom(atoms.intern("true"));
    let false_atom = make_atom(atoms.intern("false"));

    for access in [Access::Public, Access::Protected, Access::Private] {
        let mut opts = TableOptions::new();
        opts.set_access(access)
            .set_write_concurrency(true)
            .set_read_concurrency(true)
            .set_compressed(true);
        let concurrency = opts.concurrency();
        let t = Table::new(TableId::next(), opts).unwrap();

        let expected = access != Access::Private;
        let flag_atom = if expected { true_atom } else { false_atom };

        let info = t.info();
        assert_eq!(info.write_concurrency, expected, "{access:?} write_concurrency");
        assert_eq!(info.read_concurrency, expected, "{access:?} read_concurrency");
        // `private` does not suppress compression.
        assert!(info.compressed, "{access:?} keeps compressed");
        assert_eq!(
            concurrency == beam_ets::Concurrency::None,
            !expected,
            "{access:?} concurrency mode follows the normalized flags"
        );

        let entries = info.entries(&mut atoms);
        for (field, want) in [
            ("write_concurrency", flag_atom),
            ("read_concurrency", flag_atom),
            // The oracle reports `compressed => true` for a private table.
            ("compressed", true_atom),
        ] {
            assert_eq!(
                info_entry(&entries, &atoms, field),
                Some(want),
                "{access:?} `{field}`"
            );
        }
    }

    // The order of the options does not matter: the rule is applied on read.
    let mut flag_first = TableOptions::new();
    flag_first
        .set_write_concurrency(true)
        .set_read_concurrency(true)
        .set_access(Access::Private);
    let t = Table::new(TableId::next(), flag_first).unwrap();
    assert!(!t.info().write_concurrency);
    assert!(!t.info().read_concurrency);
}

/// The `ets:info/1` field set is the oracle's: **15** fields, in the oracle's
/// order (verified against the system `erl`, OTP 29 / erts-17.1, which returns
/// the reverse of the C `fields[]` array in `erl_db.c`), for every kind.
///
/// Three of them are fixed by the runtime shape rather than the table: `heir` is
/// always `none`, `node` is always `nonode@nohost`, and `decentralized_counters`
/// is `true` iff the table is an `ordered_set` with `write_concurrency` on (the
/// CATREE path) and `false` otherwise.
#[test]
fn entries_emits_the_oracle_field_set_in_order() {
    /// The 15 `ets:info/1` fields, in the oracle's order.
    const ORACLE_FIELDS: [&str; 15] = [
        "id",
        "decentralized_counters",
        "read_concurrency",
        "write_concurrency",
        "compressed",
        "memory",
        "owner",
        "heir",
        "name",
        "size",
        "node",
        "named_table",
        "type",
        "keypos",
        "protection",
    ];

    let mut atoms = AtomTable::new();
    let set = new_table(TableKind::Set);
    let entries = set.info().entries(&mut atoms);

    assert_eq!(entries.len(), ORACLE_FIELDS.len(), "the field count");
    let names: Vec<&str> = entries
        .iter()
        .map(|(atom, _)| atoms.name(*atom).expect("every field atom is interned"))
        .collect();
    assert_eq!(names, ORACLE_FIELDS, "the ets:info/1 field order");

    // The three fields this crate fixes from the runtime shape, on a plain `set`.
    assert_eq!(
        info_entry(&entries, &atoms, "heir"),
        Some(make_atom(atoms.lookup("none").unwrap()))
    );
    assert_eq!(
        info_entry(&entries, &atoms, "node"),
        Some(make_atom(atoms.lookup("nonode@nohost").unwrap()))
    );
    assert_eq!(
        info_entry(&entries, &atoms, "decentralized_counters"),
        Some(make_atom(atoms.lookup("false").unwrap()))
    );

    // `decentralized_counters` is the CATREE path: a public `ordered_set` with
    // `write_concurrency` reports `true` …
    let mut opts = TableOptions::new();
    opts.set_kind(TableKind::OrderedSet)
        .set_access(Access::Public)
        .set_write_concurrency(true);
    let ordered_wc = Table::new(TableId::next(), opts).unwrap();
    let ordered_wc_entries = ordered_wc.info().entries(&mut atoms);
    assert_eq!(
        info_entry(&ordered_wc_entries, &atoms, "decentralized_counters"),
        Some(make_atom(atoms.lookup("true").unwrap()))
    );

    // … while an `ordered_set` without it reports `false`.
    let plain_ordered = new_table(TableKind::OrderedSet);
    let plain_ordered_entries = plain_ordered.info().entries(&mut atoms);
    assert_eq!(
        info_entry(&plain_ordered_entries, &atoms, "decentralized_counters"),
        Some(make_atom(atoms.lookup("false").unwrap()))
    );
}

#[test]
fn table_id_next_returns_distinct_increasing_ids() {
    let first = TableId::next();
    let second = TableId::next();
    assert!(first.raw() < second.raw());
    assert_ne!(first, second);
    // A wrapped id round-trips and `Display` renders its raw value.
    assert_eq!(TableId::new(123).raw(), 123);
    assert_eq!(TableId::new(123).to_string(), "123");
}

#[test]
fn with_allocator_builds_a_working_table() {
    let alloc = CarrierAllocator::new(
        AllocatorConfig::builder()
            .name("beam_ets_integration_test")
            .carrier_size(64 * 1024)
            .build(),
    )
    .expect("a fresh carrier allocator must be constructible")
    .share();

    let atoms = AtomTable::new();
    let mut heap = Heap::new(16);
    let mut t = Table::with_allocator(TableId::next(), TableOptions::new(), alloc)
        .expect("a fresh table is constructible");

    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    assert!(t.member(make_small(1), &atoms));
    assert_eq!(t.size(), 1);
}

#[test]
fn memory_tracks_the_table_owned_storage() {
    for kind in [TableKind::Set, TableKind::OrderedSet] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(4096);
        let mut t = new_table(kind);

        let empty = t.memory();
        // Both interiors report their index storage, so an empty table is not zero
        // words: a `set` owns its bucket array and an `ordered_set` owns its root
        // leaf (plus the B+-tree slot table).
        assert!(empty > 0, "{kind:?} interior owns its index storage");

        for k in 0..50i64 {
            t.insert(object(&mut heap, k, k * 2), &atoms).unwrap();
        }
        let populated = t.memory();
        assert!(populated > empty, "{kind:?} memory grows on insert");
        assert_eq!(t.info().memory, populated, "{kind:?} info agrees");

        // Replacing one key's object reclaims the replaced copy, so the count does
        // not grow forever.
        let before_replace = t.memory();
        for _ in 0..20 {
            t.insert(object(&mut heap, 0, 7), &atoms).unwrap();
        }
        assert!(
            t.memory() <= before_replace + 8,
            "{kind:?} repeated replacement must not accumulate ({before_replace} -> {})",
            t.memory()
        );

        // `delete` reclaims the removed object's storage.
        let before_delete = t.memory();
        t.delete(make_small(0), &atoms).unwrap();
        assert!(
            t.memory() < before_delete,
            "{kind:?} memory drops on delete"
        );

        // `delete_all_objects` releases the term storage …
        t.delete_all_objects();
        assert_eq!(t.size(), 0);
        assert!(
            t.memory() < populated,
            "{kind:?} memory drops on delete_all"
        );

        // … and leaves the table usable for further inserts.
        t.insert(object(&mut heap, 1, 1), &atoms).unwrap();
        assert!(t.memory() > 0, "{kind:?} memory after reuse");
        assert_eq!(t.size(), 1);
    }
}

#[test]
fn ordered_set_memory_includes_the_node_arena() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(64 * 1024);
    let mut t = new_table(TableKind::OrderedSet);

    // The empty table owns its B+-tree root leaf and slot table, so it is not zero
    // words — the node arena is counted even before any key exists.
    let empty = t.memory();
    assert!(empty > 0, "an empty ordered_set owns its B+-tree root leaf");
    assert_eq!(t.info().memory, empty, "info agrees with memory()");

    // A handful of keys: still a single root leaf, negligible node storage.
    for k in 0..8i64 {
        t.insert(object(&mut heap, k, k), &atoms).unwrap();
    }
    let few = t.memory();
    assert!(few > empty, "inserting keys grows the memory count");

    // Enough keys to force many node splits (branches + a level of leaves). The
    // B+-tree node arena is now a large part of the table's memory.
    for k in 8..4000i64 {
        t.insert(object(&mut heap, k, k), &atoms).unwrap();
    }
    let many = t.memory();
    assert_eq!(t.size(), 4000);

    // `memory` strictly exceeds the term-store-only figure: each object is a
    // `{Key, Value}` tuple of three store words (header + two elements), so the
    // term store alone is `3 * size`; the node arena pushes the total above it.
    assert!(
        many > 3 * t.size(),
        "node arena counted: {many} > 3 * {} (term store only)",
        t.size()
    );
    // Never under-reports: at least one word per stored object.
    assert!(many >= t.size(), "words ≥ objects: {many} >= {}", t.size());
    assert_eq!(t.info().memory, many, "info agrees after the churn");
}

#[test]
fn info_reports_memory_and_named_table() {
    let mut atoms = AtomTable::new();
    let mut heap = Heap::new(256);
    let name = atoms.intern("mem_table");
    let mut opts = TableOptions::new();
    opts.set_name(name);
    let mut t = Table::new(TableId::next(), opts).unwrap();
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();

    let info = t.info();
    // The registered name and the `named_table` flag are independent. Oracle:
    // `ets:info(ets:new(mem_table, []), named_table) =:= false` (and `name` is
    // still `mem_table`) — only the `named_table` option sets the flag.
    assert!(!info.named_table);
    assert_eq!(info.name, Some(name));
    assert!(info.memory > 0);
    assert_eq!(info.memory, t.memory());

    let entries = info.entries(&mut atoms);
    assert_eq!(
        info_entry(&entries, &atoms, "memory"),
        Some(make_small(info.memory as i64))
    );
    assert_eq!(info_entry(&entries, &atoms, "name"), Some(make_atom(name)));
    assert_eq!(
        info_entry(&entries, &atoms, "named_table"),
        Some(make_atom(atoms.lookup("false").unwrap()))
    );

    // Setting the `named_table` option (independently of the name) flips it.
    let mut opts = TableOptions::new();
    opts.set_name(name).set_named(true);
    let named = Table::new(TableId::next(), opts).unwrap();
    let named_info = named.info();
    assert!(named_info.named_table);
    assert_eq!(named_info.name, Some(name));
    let named_entries = named_info.entries(&mut atoms);
    assert_eq!(
        info_entry(&named_entries, &atoms, "named_table"),
        Some(make_atom(atoms.lookup("true").unwrap()))
    );

    // An unnamed table reports `named_table` = false.
    let plain = Table::new(TableId::next(), TableOptions::new()).unwrap();
    let plain_info = plain.info();
    assert!(!plain_info.named_table);
    let plain_entries = plain_info.entries(&mut atoms);
    assert_eq!(
        info_entry(&plain_entries, &atoms, "named_table"),
        Some(make_atom(atoms.lookup("false").unwrap()))
    );
}

/// While a table is fixed (`ets:safe_fixtable/2`), a deletion still empties it —
/// `ets:info(T, size)` is `0` — but the deleted objects' storage is held, so
/// `ets:info(T, memory)` stays above the unfixed twin and only falls at unfix.
///
/// The oracle shape (system `erl`, OTP 29 / erts-17.1): `ets:info(Tab, memory)`
/// does not drop until `ets:safe_fixtable(Tab, false)`; `ets:info(Tab, size)`
/// drops immediately. The comparison is against an *identical* table that deleted
/// the same objects unfixed, so the two differ only in the fix flag.
#[test]
fn fixed_tables_report_size_zero_while_info_memory_is_held() {
    for kind in [
        TableKind::Set,
        TableKind::OrderedSet,
        TableKind::Bag,
        TableKind::DuplicateBag,
    ] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(1 << 16);
        let mut fixed = new_table(kind);
        let mut released = new_table(kind);

        let objects: Vec<Eterm> = (0..64).map(|i| int_tuple(&mut heap, &[i, i])).collect();
        for &object in &objects {
            fixed.insert(object, &atoms).unwrap();
            released.insert(object, &atoms).unwrap();
        }
        let loaded = fixed.memory();
        assert_eq!(fixed.info().memory, loaded, "{kind:?}: info agrees while loaded");
        assert_eq!(fixed.info().size, 64, "{kind:?}: the loaded object count");

        fixed.set_fixation(true);
        fixed.delete_all_objects();
        released.delete_all_objects();

        assert_eq!(fixed.info().size, 0, "{kind:?}: a fixed table still empties");
        assert!(fixed.is_empty(), "{kind:?}: a fixed table reports empty");
        assert_eq!(
            fixed.deferred_objects(),
            64,
            "{kind:?}: every removed object's storage is held"
        );
        assert_eq!(released.deferred_objects(), 0, "{kind:?}");
        assert_eq!(
            fixed.info().memory,
            fixed.memory(),
            "{kind:?}: `TableInfo::memory` agrees with `Table::memory` while fixed"
        );
        assert!(
            fixed.info().memory > released.info().memory,
            "{kind:?}: a fixed table holds the deleted storage ({} !> {})",
            fixed.info().memory,
            released.info().memory
        );

        fixed.set_fixation(false);
        assert_eq!(
            fixed.deferred_objects(),
            0,
            "{kind:?}: unfixing releases everything held"
        );
        assert_eq!(
            fixed.info().memory,
            fixed.memory(),
            "{kind:?}: `TableInfo::memory` agrees after unfix"
        );
        assert_eq!(
            fixed.info().memory,
            released.info().memory,
            "{kind:?}: after unfixing, both tables account for the same storage"
        );
        assert_eq!(fixed.info().size, 0, "{kind:?}: still empty after unfix");
    }
}
