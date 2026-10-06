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
    assert!(info.named_table);
    assert_eq!(info.name, Some(name));
    assert!(info.memory > 0);
    assert_eq!(info.memory, t.memory());

    let entries = info.entries(&mut atoms);
    assert_eq!(
        info_entry(&entries, &atoms, "memory"),
        Some(make_small(info.memory as i64))
    );
    assert_eq!(
        info_entry(&entries, &atoms, "named_table"),
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
