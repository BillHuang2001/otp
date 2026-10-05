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
        // A `set` owns its bucket array, so an empty table is not zero words.
        // (`ordered_set`'s B+-tree does not report its node storage yet — a
        // documented gap — so its empty count may be zero.)
        if kind == TableKind::Set {
            assert!(empty > 0, "a hash interior owns its buckets");
        }

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
