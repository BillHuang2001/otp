//! Crate-level integration tests over the **public** `beam_ets` API.
//!
//! These exercise `Table` (through `TableId`/`TableOptions`/`TableKind`/`Access`/
//! `OwnerId`/`TableInfo`), the interiors' observable semantics for `set` and
//! `ordered_set` (plus a small `bag` check), `select_all` and the `compile`d
//! match-spec `select`, and the table identifiers.
//!
//! Terms are built with `beam_term` (the `AtomTable` and the `heap::Heap` test
//! arena) and compared structurally with `beam_term::compare::eq`/`cmp` — the
//! single authority on term equality/order (the crate never re-implements it).

use beam_alloc::{AllocatorConfig, CarrierAllocator};
use beam_common::BeamError;
use beam_ets::{compile, Access, OwnerId, Table, TableId, TableKind, TableOptions};
use beam_term::prelude::*;
use core::cmp::Ordering;

/// A `{Key, Value}` ETS object on `heap` (keypos 1).
fn object(heap: &mut Heap, key: i64, value: i64) -> Eterm {
    heap.tuple(&[make_small(key), make_small(value)])
        .expect("test heap has room")
}

/// An empty table of `kind`.
fn new_table(kind: TableKind) -> Table {
    let mut opts = TableOptions::new();
    opts.set_kind(kind);
    Table::new(TableId::next(), opts).expect("a fresh table is constructible")
}

/// A heap-allocated flonum term (`[HEADER_FLONUM, bits]`). `beam_term` has no
/// heap-based float constructor, so the two words are leaked for the test's life.
fn flonum(value: f64) -> Eterm {
    let words: Box<[Eterm; 2]> =
        Box::new([HEADER_FLONUM, Eterm::from_raw(value.to_bits() as usize)]);
    let leaked: &'static [Eterm; 2] = Box::leak(words);
    make_boxed(leaked.as_ptr())
}

/// The term of the atom `name`, interned in `atoms`.
fn interned_atom(atoms: &mut AtomTable, name: &str) -> Eterm {
    make_atom(atoms.intern(name))
}

/// A proper list of `elements` on `heap`.
fn proper_list(heap: &mut Heap, elements: &[Eterm]) -> Eterm {
    let mut acc = NIL;
    for element in elements.iter().rev() {
        acc = heap.cons(*element, acc).expect("test heap has room");
    }
    acc
}

/// The value of the `field` atom within an `ets:info/1` entry list.
fn info_entry(entries: &[(Atom, Eterm)], atoms: &AtomTable, field: &str) -> Option<Eterm> {
    let want = atoms.lookup(field)?;
    entries.iter().find(|(a, _)| *a == want).map(|(_, v)| *v)
}

// ---------------------------------------------------------------------------
// Core `set` / `ordered_set` semantics
// ---------------------------------------------------------------------------

#[test]
fn insert_lookup_member_delete_size_tab2list() {
    for kind in [TableKind::Set, TableKind::OrderedSet] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(256);
        let mut t = new_table(kind);

        assert!(t.is_empty());
        assert_eq!(t.size(), 0);
        assert!(t.tab2list().is_empty());

        let o1 = object(&mut heap, 1, 10);
        t.insert(o1, &atoms).unwrap();
        assert!(t.member(make_small(1), &atoms));
        assert!(!t.member(make_small(2), &atoms));
        assert_eq!(t.size(), 1);
        assert!(!t.is_empty());

        let looked = t.lookup(make_small(1), &atoms);
        assert_eq!(looked.len(), 1);
        assert!(eq(looked[0], o1, &atoms));
        assert!(t.lookup(make_small(2), &atoms).is_empty());

        let all = t.tab2list();
        assert_eq!(all.len(), 1);
        assert!(eq(all[0], o1, &atoms));

        // `delete` drops the entry; deleting an absent key is a no-op.
        t.delete(make_small(1), &atoms).unwrap();
        assert!(!t.member(make_small(1), &atoms));
        assert_eq!(t.size(), 0);
        assert!(t.is_empty());
        t.delete(make_small(99), &atoms).unwrap();
        assert_eq!(t.size(), 0);
    }
}

#[test]
fn insert_replaces_the_object_on_an_existing_key() {
    for kind in [TableKind::Set, TableKind::OrderedSet] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut t = new_table(kind);

        t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
        t.insert(object(&mut heap, 1, 20), &atoms).unwrap();

        assert_eq!(t.size(), 1);
        let looked = t.lookup(make_small(1), &atoms);
        assert_eq!(looked.len(), 1);
        assert!(eq(looked[0], object(&mut heap, 1, 20), &atoms));
    }
}

#[test]
fn insert_new_is_false_when_the_key_exists() {
    for kind in [TableKind::Set, TableKind::OrderedSet] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut t = new_table(kind);

        assert!(t.insert_new(object(&mut heap, 1, 10), &atoms).unwrap());
        // The key now exists: a second `insert_new` is a no-op returning `false`.
        assert!(!t.insert_new(object(&mut heap, 1, 20), &atoms).unwrap());
        assert_eq!(t.size(), 1);
        let looked = t.lookup(make_small(1), &atoms);
        assert!(eq(looked[0], object(&mut heap, 1, 10), &atoms));

        // A fresh key inserts and returns `true`.
        assert!(t.insert_new(object(&mut heap, 2, 20), &atoms).unwrap());
        assert_eq!(t.size(), 2);
    }
}

#[test]
fn lookup_element_is_one_based_and_badarg_out_of_range() {
    for kind in [TableKind::Set, TableKind::OrderedSet] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut t = new_table(kind);

        let o = heap
            .tuple(&[make_small(1), make_small(10), make_small(20)])
            .unwrap();
        t.insert(o, &atoms).unwrap();

        assert_eq!(
            t.lookup_element(make_small(1), 1, &atoms).unwrap(),
            make_small(1)
        );
        assert_eq!(
            t.lookup_element(make_small(1), 2, &atoms).unwrap(),
            make_small(10)
        );
        assert_eq!(
            t.lookup_element(make_small(1), 3, &atoms).unwrap(),
            make_small(20)
        );

        // Out of range, 0 and an absent key are all `badarg`.
        assert!(matches!(
            t.lookup_element(make_small(1), 4, &atoms),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            t.lookup_element(make_small(1), 0, &atoms),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            t.lookup_element(make_small(99), 1, &atoms),
            Err(BeamError::BadArg)
        ));
    }
}

#[test]
fn delete_object_removes_only_a_matching_object() {
    for kind in [TableKind::Set, TableKind::OrderedSet] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(256);
        let mut t = new_table(kind);

        t.insert(object(&mut heap, 1, 10), &atoms).unwrap();

        // A non-matching object leaves the entry alone.
        t.delete_object(object(&mut heap, 1, 99), &atoms).unwrap();
        assert!(t.member(make_small(1), &atoms));

        // A structurally equal (freshly built) object removes the stored one.
        t.delete_object(object(&mut heap, 1, 10), &atoms).unwrap();
        assert!(!t.member(make_small(1), &atoms));
        assert_eq!(t.size(), 0);
    }
}

#[test]
fn delete_all_objects_empties_the_table() {
    for kind in [TableKind::Set, TableKind::OrderedSet] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(256);
        let mut t = new_table(kind);

        for k in 0..5 {
            t.insert(object(&mut heap, k, k * 10), &atoms).unwrap();
        }
        assert_eq!(t.size(), 5);

        t.delete_all_objects();
        assert_eq!(t.size(), 0);
        assert!(t.is_empty());
        assert!(t.tab2list().is_empty());
        assert!(!t.member(make_small(0), &atoms));
    }
}

#[test]
fn insert_list_inserts_every_object() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);

    let objects = [
        object(&mut heap, 1, 10),
        object(&mut heap, 2, 20),
        object(&mut heap, 3, 30),
    ];
    t.insert_list(&objects, &atoms).unwrap();
    assert_eq!(t.size(), 3);
    for k in 1..=3 {
        assert!(t.member(make_small(k), &atoms));
    }

    // An empty list is a no-op.
    t.insert_list(&[], &atoms).unwrap();
    assert_eq!(t.size(), 3);
}

#[test]
fn insert_rejects_malformed_objects_with_badarg() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(64);
    let mut t = new_table(TableKind::Set);

    // Non-tuples are not valid ETS objects.
    assert!(matches!(t.insert(make_small(1), &atoms), Err(BeamError::BadArg)));
    assert!(matches!(t.insert(NIL, &atoms), Err(BeamError::BadArg)));
    let list = proper_list(&mut heap, &[make_small(1)]);
    assert!(matches!(t.insert(list, &atoms), Err(BeamError::BadArg)));
    assert_eq!(t.size(), 0);

    // A keypos-2 table rejects a 1-tuple (arity < keypos).
    let mut opts = TableOptions::new();
    opts.set_keypos(2).unwrap();
    let mut t2 = Table::new(TableId::next(), opts).unwrap();
    let one = heap.tuple(&[make_small(1)]).unwrap();
    assert!(matches!(t2.insert(one, &atoms), Err(BeamError::BadArg)));
    assert_eq!(t2.size(), 0);
}

// ---------------------------------------------------------------------------
// Key equality / ordering
// ---------------------------------------------------------------------------

#[test]
fn integer_and_float_keys_are_distinct() {
    for kind in [TableKind::Set, TableKind::OrderedSet] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut t = new_table(kind);

        let one = make_small(1);
        let one_float = flonum(1.0);
        let o_int = heap.tuple(&[one, make_small(10)]).unwrap();
        let o_float = heap.tuple(&[one_float, make_small(20)]).unwrap();

        t.insert(o_int, &atoms).unwrap();
        t.insert(o_float, &atoms).unwrap();

        assert_eq!(t.size(), 2);
        assert!(t.member(one, &atoms));
        assert!(t.member(one_float, &atoms));

        let int_obj = t.lookup(one, &atoms);
        let float_obj = t.lookup(one_float, &atoms);
        assert_eq!(int_obj.len(), 1);
        assert_eq!(float_obj.len(), 1);
        assert!(eq(int_obj[0], o_int, &atoms));
        assert!(eq(float_obj[0], o_float, &atoms));
    }
}

#[test]
fn ordered_set_iterates_in_ascending_term_order() {
    let mut atoms = AtomTable::new();
    let a = make_atom(atoms.intern("a"));
    let mut heap = Heap::new(256);

    let small = make_small(5);
    let tuple = heap.tuple(&[make_small(1)]).unwrap();
    let list = heap.cons(make_small(1), NIL).unwrap();

    let mut t = new_table(TableKind::OrderedSet);
    // Insert out of term order; the objects are 1-tuples keyed by the term.
    for key in [list, tuple, a, small] {
        let o = heap.tuple(&[key]).unwrap();
        t.insert(o, &atoms).unwrap();
    }
    assert_eq!(t.size(), 4);

    let ordered = t.tab2list();
    assert_eq!(ordered.len(), 4);
    // Ascending `cmp` order between consecutive objects ({k_i} < {k_i+1}).
    for window in ordered.windows(2) {
        assert_eq!(cmp(window[0], window[1], &atoms), Ordering::Less);
    }
    // The concrete key order: number < atom < tuple < list.
    for (object, key) in ordered.iter().zip([small, a, tuple, list]) {
        let want = heap.tuple(&[key]).unwrap();
        assert!(eq(*object, want, &atoms), "unexpected object at this position");
    }
}

// ---------------------------------------------------------------------------
// `select_all` and the compiled match-spec `select`
// ---------------------------------------------------------------------------

#[test]
fn select_all_keeps_objects_accepted_by_the_predicate() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);
    for k in 0..5 {
        t.insert(object(&mut heap, k, k * 10), &atoms).unwrap();
    }

    // Keep objects whose first element (the key) is a small int > 2.
    let mut pred = |elements: &[Eterm]| is_small(elements[0]) && signed_value(elements[0]) > 2;
    let kept = t.select_all(&mut pred);

    assert_eq!(kept.len(), 2);
    for k in [3i64, 4] {
        let want = object(&mut heap, k, k * 10);
        assert!(
            kept.iter().any(|g| eq(*g, want, &atoms)),
            "key {k} should have been kept"
        );
    }
    for k in [0i64, 1, 2] {
        let not_want = object(&mut heap, k, k * 10);
        assert!(
            !kept.iter().any(|g| eq(*g, not_want, &atoms)),
            "key {k} should not have been kept"
        );
    }
}

#[test]
fn select_wildcard_head_and_whole_object_body() {
    let mut atoms = AtomTable::new();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);
    for k in 0..4 {
        t.insert(object(&mut heap, k, k * 10), &atoms).unwrap();
    }

    // Spec: [ {'_', [], '$_'} ] — every object, returned whole.
    let clause = heap
        .tuple(&[interned_atom(&mut atoms, "_"), NIL, interned_atom(&mut atoms, "$_")])
        .unwrap();
    let spec = proper_list(&mut heap, &[clause]);
    let compiled = compile(spec, &atoms).unwrap();

    let selected = t.select(&compiled, &atoms).unwrap();
    assert_eq!(selected.len(), 4);
    for k in 0..4 {
        let want = object(&mut heap, k, k * 10);
        assert!(
            selected.iter().any(|g| eq(*g, want, &atoms)),
            "key {k} should have been selected"
        );
    }
}

#[test]
fn select_tuple_head_with_bind_and_single_element_body() {
    let mut atoms = AtomTable::new();
    let mut heap = Heap::new(256);
    let mut t = new_table(TableKind::Set);
    for k in 0..5 {
        t.insert(object(&mut heap, k, k * 10), &atoms).unwrap();
    }

    // Spec: [ {{'$1', '_'}, [], {'$1'}} ] — the key of every object.
    let head = heap
        .tuple(&[interned_atom(&mut atoms, "$1"), interned_atom(&mut atoms, "_")])
        .unwrap();
    let body = heap.tuple(&[interned_atom(&mut atoms, "$1")]).unwrap();
    let clause = heap.tuple(&[head, NIL, body]).unwrap();
    let spec = proper_list(&mut heap, &[clause]);
    let compiled = compile(spec, &atoms).unwrap();

    let mut keys = t.select(&compiled, &atoms).unwrap();
    keys.sort_by(|a, b| cmp(*a, *b, &atoms));
    assert_eq!(keys, (0..5).map(make_small).collect::<Vec<_>>());
}

#[test]
fn select_multi_element_body_is_badarg() {
    let mut atoms = AtomTable::new();
    let mut heap = Heap::new(64);
    let mut t = new_table(TableKind::Set);
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();

    // Body {'$1', '_'} has two elements: M1 cannot build a fresh tuple, so the
    // match spec is outside the supported subset.
    let body = heap
        .tuple(&[interned_atom(&mut atoms, "$1"), interned_atom(&mut atoms, "_")])
        .unwrap();
    let clause = heap
        .tuple(&[interned_atom(&mut atoms, "_"), NIL, body])
        .unwrap();
    let spec = proper_list(&mut heap, &[clause]);
    let compiled = compile(spec, &atoms).unwrap();

    assert!(matches!(t.select(&compiled, &atoms), Err(BeamError::BadArg)));
}

// ---------------------------------------------------------------------------
// Multi kinds (shared hash interior)
// ---------------------------------------------------------------------------

#[test]
fn bag_stores_distinct_objects_per_key() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(64);
    let mut t = new_table(TableKind::Bag);

    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    t.insert(object(&mut heap, 1, 20), &atoms).unwrap();
    // An identical object is a no-op for a bag.
    t.insert(object(&mut heap, 1, 10), &atoms).unwrap();

    assert_eq!(t.size(), 2);
    let looked = t.lookup(make_small(1), &atoms);
    assert_eq!(looked.len(), 2);
    assert!(looked.iter().any(|o| eq(*o, object(&mut heap, 1, 10), &atoms)));
    assert!(looked.iter().any(|o| eq(*o, object(&mut heap, 1, 20), &atoms)));

    // `delete` removes the whole key (all its objects).
    t.delete(make_small(1), &atoms).unwrap();
    assert_eq!(t.size(), 0);
    assert!(t.is_empty());
}

// ---------------------------------------------------------------------------
// Metadata, identifiers and allocation
// ---------------------------------------------------------------------------

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
    assert_eq!(info_entry(&entries, &atoms, "type"), Some(make_atom(atoms.lookup("ordered_set").unwrap())));
    assert_eq!(info_entry(&entries, &atoms, "protection"), Some(make_atom(atoms.lookup("public").unwrap())));
    assert_eq!(info_entry(&entries, &atoms, "name"), Some(make_atom(name)));
    assert_eq!(info_entry(&entries, &atoms, "owner"), Some(make_atom(atoms.lookup("undefined").unwrap())));
    assert_eq!(info_entry(&entries, &atoms, "compressed"), Some(make_atom(atoms.lookup("true").unwrap())));
    assert_eq!(info_entry(&entries, &atoms, "write_concurrency"), Some(make_atom(atoms.lookup("true").unwrap())));
    assert_eq!(info_entry(&entries, &atoms, "read_concurrency"), Some(make_atom(atoms.lookup("true").unwrap())));

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
