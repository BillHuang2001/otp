//! Integration tests for the expanded `ets:*` bridge, driven through the public
//! [`BifRegistry::call`] over a real [`TableRegistry`].
//!
//! Every BIF added on top of the original `new/2`/`insert/2`/`lookup/2`/`delete/1`
//! is exercised here — `insert_new/2`, `delete/2`, `delete_object/2`,
//! `delete_all_objects/1`, `member/2`, `lookup_element/3,4`, `take/2`,
//! `first/1`, `last/1`, `next/2`, `prev/2`, `info/1,2`, `tab2list/1`,
//! `update_counter/3,4` and `update_element/3` — including the `badarg` cases and
//! the no-registry `unsupported_bif` contract.
//!
//! Terms are asserted **structurally** with [`beam_term::compare::eq`]; list and
//! tuple destructuring goes through the registered `erlang`/`lists` BIFs
//! (`length/1`, `element/2`, `lists:nth/2`) rather than the crate-private
//! term helpers, so this suite uses only the public API. The behaviours asserted
//! are the ones verified against the system `erl` (OTP 29 / erts-17.1).

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry, TableRegistry};
use beam_common::BeamError;
use beam_process::ProcessId;
use beam_term::{atom_val, signed_value, AtomTable, Eterm};

/// The atom names the `ets` BIFs and the destructuring helpers need.
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed([
        "true",
        "false",
        "ok",
        "undefined",
        "set",
        "ordered_set",
        "bag",
        "duplicate_bag",
        "keypos",
        "public",
        "protected",
        "private",
        "named_table",
        "$end_of_table",
        "id",
        "name",
        "type",
        "protection",
        "size",
        "memory",
        "owner",
        "compressed",
        "read_concurrency",
        "write_concurrency",
        "decentralized_counters",
        "heir",
        "none",
        "node",
        "nonode@nohost",
        "my_tab",
        "tab2",
        "a",
        "b",
        "c",
        "p",
        "q",
        "z",
    ]);
    atoms
}

/// An `ets` object `{key, value}` built on the context heap.
fn object(ctx: &mut BifContext<'_>, key: &str, value: i64) -> Eterm {
    let k = ctx.atom(key).expect("seeded");
    let v = ctx.int(value).expect("fits");
    ctx.tuple(&[k, v]).expect("fits")
}

/// `ets:new(Name, [Kind])` through the registry, returning the handle.
fn new_table(bifs: &BifRegistry, ctx: &mut BifContext<'_>, name: &str, kind: &str) -> Eterm {
    let name = ctx.atom(name).expect("seeded");
    let kind = ctx.atom(kind).expect("seeded");
    let options = ctx.list(&[kind]).expect("fits");
    bifs.call("ets", "new", 2, ctx, &[name, options])
        .expect("ets:new")
}

fn is_true(atoms: &AtomTable, term: Eterm) -> bool {
    atoms.name(atom_val(term)) == Some("true")
}

fn is_false(atoms: &AtomTable, term: Eterm) -> bool {
    atoms.name(atom_val(term)) == Some("false")
}

fn is_raise_atom(err: &BeamError, reason: &'static str) -> bool {
    matches!(err, BeamError::Raise(raise) if raise.reason == beam_common::ExitReason::atom(reason))
}

/// `erlang:length/1` of the list `term`.
fn length(bifs: &BifRegistry, ctx: &mut BifContext<'_>, term: Eterm) -> i64 {
    let len = bifs
        .call("erlang", "length", 1, ctx, &[term])
        .expect("erlang:length");
    signed_value(len)
}

/// `lists:nth/2` — the `N`th (1-based) element of the proper list `term`.
///
/// The `ets:lookup/2`, `ets:take/2` and `ets:info/1` results are *lists* of
/// objects, so indexing them goes through `lists:nth/2`; [`element`] below is
/// `erlang:element/2`, which accepts only a single `{Key, Value}` tuple.
fn nth(bifs: &BifRegistry, ctx: &mut BifContext<'_>, pos: i64, term: Eterm) -> Eterm {
    let pos = ctx.int(pos).expect("fits");
    bifs.call("lists", "nth", 2, ctx, &[pos, term])
        .expect("lists:nth")
}

/// `erlang:element/2` of the tuple `term`.
fn element(
    bifs: &BifRegistry,
    ctx: &mut BifContext<'_>,
    pos: i64,
    term: Eterm,
) -> Eterm {
    let pos = ctx.int(pos).expect("fits");
    bifs.call("erlang", "element", 2, ctx, &[pos, term])
        .expect("erlang:element")
}

#[test]
fn registration_covers_every_new_ets_bif() {
    let reg = default_registry();
    for (name, arity) in [
        ("new", 2),
        ("insert", 2),
        ("insert_new", 2),
        ("lookup", 2),
        ("lookup_element", 3),
        ("lookup_element", 4),
        ("member", 2),
        ("take", 2),
        ("first", 1),
        ("last", 1),
        ("next", 2),
        ("prev", 2),
        ("size", 0), // never registered: `ets:size/1` is not an OTP export.
        ("info", 1),
        ("info", 2),
        ("tab2list", 1),
        ("delete", 1),
        ("delete", 2),
        ("delete_object", 2),
        ("delete_all_objects", 1),
        ("update_counter", 3),
        ("update_counter", 4),
        ("update_element", 3),
    ] {
        let expected = name != "size";
        assert_eq!(
            reg.contains("ets", name, arity),
            expected,
            "ets:{name}/{arity}"
        );
    }
}

#[test]
fn insert_new_is_atomic_across_a_list() {
    let reg = default_registry();
    let mut heap = Heap::new(65536).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);
    let handle = new_table(&reg, &mut ctx, "my_tab", "set");

    let p1 = object(&mut ctx, "p", 1);
    let p2 = object(&mut ctx, "p", 2);

    // A single new object is `true`; the same key again is `false`.
    let result = reg
        .call("ets", "insert_new", 2, &mut ctx, &[handle, p1])
        .expect("insert_new");
    assert!(is_true(&atoms, result));
    let result = reg
        .call("ets", "insert_new", 2, &mut ctx, &[handle, p2])
        .expect("insert_new");
    assert!(is_false(&atoms, result));

    // The stored object is unchanged.
    let p = ctx.atom("p").expect("seeded");
    let stored = reg
        .call("ets", "lookup", 2, &mut ctx, &[handle, p])
        .expect("lookup");
    let first_object = nth(&reg, &mut ctx, 1, stored);
    assert_eq!(element(&reg, &mut ctx, 2, first_object), ctx.int(1).unwrap());

    // A list with one already-present key inserts nothing.
    let q1 = object(&mut ctx, "q", 1);
    let p5 = object(&mut ctx, "p", 5);
    let mixed = ctx.list(&[q1, p5]).expect("fits");
    let result = reg
        .call("ets", "insert_new", 2, &mut ctx, &[handle, mixed])
        .expect("insert_new");
    assert!(is_false(&atoms, result));
    let q = ctx.atom("q").expect("seeded");
    let absent = reg
        .call("ets", "lookup", 2, &mut ctx, &[handle, q])
        .expect("lookup");
    assert_eq!(length(&reg, &mut ctx, absent), 0);

    // An empty list is `true`.
    let nil = ctx.list(&[]).expect("fits");
    let result = reg
        .call("ets", "insert_new", 2, &mut ctx, &[handle, nil])
        .expect("insert_new []");
    assert!(is_true(&atoms, result));
}

#[test]
fn insert_accepts_the_empty_list() {
    // `ets:insert(T, [])` is `true` in the oracle (the empty list is no objects).
    let reg = default_registry();
    let mut heap = Heap::new(65536).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);
    let handle = new_table(&reg, &mut ctx, "my_tab", "set");

    let nil = ctx.list(&[]).expect("fits");
    let result = reg
        .call("ets", "insert", 2, &mut ctx, &[handle, nil])
        .expect("ets:insert []");
    assert!(is_true(&atoms, result));
}

#[test]
fn member_lookup_element_and_take() {
    let reg = default_registry();
    let mut heap = Heap::new(65536).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);
    let handle = new_table(&reg, &mut ctx, "my_tab", "set");

    let a = ctx.atom("a").expect("seeded");
    let b = ctx.atom("b").expect("seeded");
    let obj_a = object(&mut ctx, "a", 7);
    reg.call("ets", "insert", 2, &mut ctx, &[handle, obj_a])
        .expect("insert");

    // `member/2` for a present and an absent key.
    let present = reg
        .call("ets", "member", 2, &mut ctx, &[handle, a])
        .expect("member");
    assert!(is_true(&atoms, present));
    let absent = reg
        .call("ets", "member", 2, &mut ctx, &[handle, b])
        .expect("member");
    assert!(is_false(&atoms, absent));

    // `lookup_element/3` returns element 2 of `{a, 7}`.
    let two = ctx.int(2).expect("fits");
    let value = reg
        .call("ets", "lookup_element", 3, &mut ctx, &[handle, a, two])
        .expect("lookup_element/3");
    assert_eq!(value, ctx.int(7).unwrap());

    // `lookup_element/4` returns the default for an absent key.
    let default = ctx.int(99).expect("fits");
    let value = reg
        .call(
            "ets",
            "lookup_element",
            4,
            &mut ctx,
            &[handle, b, two, default],
        )
        .expect("lookup_element/4");
    assert_eq!(value, default);

    // `take/2` removes the entry and hands back its objects.
    let taken = reg
        .call("ets", "take", 2, &mut ctx, &[handle, a])
        .expect("take");
    assert_eq!(length(&reg, &mut ctx, taken), 1);
    let taken_object = nth(&reg, &mut ctx, 1, taken);
    assert_eq!(element(&reg, &mut ctx, 2, taken_object), ctx.int(7).unwrap());
    // The key is gone afterwards.
    let present = reg
        .call("ets", "member", 2, &mut ctx, &[handle, a])
        .expect("member");
    assert!(is_false(&atoms, present));

    // Taking an absent key yields the empty list, not an error.
    let taken = reg
        .call("ets", "take", 2, &mut ctx, &[handle, b])
        .expect("take absent");
    assert_eq!(length(&reg, &mut ctx, taken), 0);
}

#[test]
fn delete_key_object_and_all_objects() {
    let reg = default_registry();
    let mut heap = Heap::new(65536).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);
    let handle = new_table(&reg, &mut ctx, "my_tab", "set");

    let a = ctx.atom("a").expect("seeded");
    let obj_a = object(&mut ctx, "a", 1);
    reg.call("ets", "insert", 2, &mut ctx, &[handle, obj_a])
        .expect("insert");

    // `delete/2` removes the key and reports `true` (also when absent).
    let result = reg
        .call("ets", "delete", 2, &mut ctx, &[handle, a])
        .expect("delete/2");
    assert!(is_true(&atoms, result));
    let result = reg
        .call("ets", "delete", 2, &mut ctx, &[handle, a])
        .expect("delete absent");
    assert!(is_true(&atoms, result));

    // `delete_object/2` removes one matching object.
    let obj_again = object(&mut ctx, "a", 1);
    reg.call("ets", "insert", 2, &mut ctx, &[handle, obj_again])
        .expect("insert");
    let result = reg
        .call("ets", "delete_object", 2, &mut ctx, &[handle, obj_again])
        .expect("delete_object");
    assert!(is_true(&atoms, result));
    let absent = reg
        .call("ets", "lookup", 2, &mut ctx, &[handle, a])
        .expect("lookup");
    assert_eq!(length(&reg, &mut ctx, absent), 0);

    // `delete_all_objects/1` empties the table.
    let b = object(&mut ctx, "b", 2);
    let objects = ctx.list(&[b]).expect("fits");
    reg.call("ets", "insert", 2, &mut ctx, &[handle, objects])
        .expect("insert");
    let result = reg
        .call("ets", "delete_all_objects", 1, &mut ctx, &[handle])
        .expect("delete_all_objects");
    assert!(is_true(&atoms, result));
    let listed = reg
        .call("ets", "tab2list", 1, &mut ctx, &[handle])
        .expect("tab2list");
    assert_eq!(length(&reg, &mut ctx, listed), 0);
}

#[test]
fn tab2list_returns_every_object() {
    let reg = default_registry();
    let mut heap = Heap::new(65536).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);
    // `ordered_set` iterates in key order, so the result is deterministic.
    let handle = new_table(&reg, &mut ctx, "my_tab", "ordered_set");

    let obj_a = object(&mut ctx, "a", 1);
    let obj_b = object(&mut ctx, "b", 2);
    let objects = ctx.list(&[obj_b, obj_a]).expect("fits");
    reg.call("ets", "insert", 2, &mut ctx, &[handle, objects])
        .expect("insert");

    let listed = reg
        .call("ets", "tab2list", 1, &mut ctx, &[handle])
        .expect("tab2list");
    assert_eq!(length(&reg, &mut ctx, listed), 2);
    let expected = ctx.list(&[obj_a, obj_b]).expect("fits");
    assert!(beam_term::compare::eq(listed, expected, ctx.atoms()));
}

#[test]
fn navigation_walks_a_set_and_an_ordered_set() {
    let reg = default_registry();
    let mut heap = Heap::new(65536).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);

    // An empty set: `first`/`last` are `'$end_of_table'`.
    let empty = new_table(&reg, &mut ctx, "my_tab", "set");
    let first = reg
        .call("ets", "first", 1, &mut ctx, &[empty])
        .expect("first");
    assert_eq!(atoms.name(atom_val(first)), Some("$end_of_table"));
    let last = reg.call("ets", "last", 1, &mut ctx, &[empty]).expect("last");
    assert_eq!(atoms.name(atom_val(last)), Some("$end_of_table"));

    // A one-key set: `first`/`last` agree; `next`/`prev` are the terminator.
    let obj_a = object(&mut ctx, "a", 1);
    reg.call("ets", "insert", 2, &mut ctx, &[empty, obj_a])
        .expect("insert");
    let a = ctx.atom("a").expect("seeded");
    let first = reg
        .call("ets", "first", 1, &mut ctx, &[empty])
        .expect("first");
    assert!(beam_term::compare::eq(first, a, &atoms));
    let last = reg.call("ets", "last", 1, &mut ctx, &[empty]).expect("last");
    assert!(beam_term::compare::eq(last, a, &atoms));
    let next = reg
        .call("ets", "next", 2, &mut ctx, &[empty, a])
        .expect("next");
    assert_eq!(atoms.name(atom_val(next)), Some("$end_of_table"));
    let prev = reg
        .call("ets", "prev", 2, &mut ctx, &[empty, a])
        .expect("prev");
    assert_eq!(atoms.name(atom_val(prev)), Some("$end_of_table"));

    // An `ordered_set` over integer keys: range-bound navigation.
    let ordered = new_table(&reg, &mut ctx, "tab2", "ordered_set");
    for key in 1..=3i64 {
        let k = ctx.int(key).expect("fits");
        let v = ctx.atom("a").expect("seeded");
        let obj = ctx.tuple(&[k, v]).expect("fits");
        reg.call("ets", "insert", 2, &mut ctx, &[ordered, obj])
            .expect("insert");
    }
    let one = ctx.int(1).expect("fits");
    let two = ctx.int(2).expect("fits");
    let three = ctx.int(3).expect("fits");
    let zero = ctx.int(0).expect("fits");
    let first = reg
        .call("ets", "first", 1, &mut ctx, &[ordered])
        .expect("first");
    assert_eq!(first, one);
    let last = reg
        .call("ets", "last", 1, &mut ctx, &[ordered])
        .expect("last");
    assert_eq!(last, three);
    let next = reg
        .call("ets", "next", 2, &mut ctx, &[ordered, one])
        .expect("next");
    assert_eq!(next, two);
    let next = reg
        .call("ets", "next", 2, &mut ctx, &[ordered, three])
        .expect("next");
    assert_eq!(atoms.name(atom_val(next)), Some("$end_of_table"));
    let prev = reg
        .call("ets", "prev", 2, &mut ctx, &[ordered, three])
        .expect("prev");
    assert_eq!(prev, two);
    // An absent key is a *range bound* for `ordered_set`: `next(T, 0)` is the
    // smallest stored key.
    let next = reg
        .call("ets", "next", 2, &mut ctx, &[ordered, zero])
        .expect("next range bound");
    assert_eq!(next, one);
}

#[test]
fn info_reports_the_table_fields() {
    let reg = default_registry();
    let mut heap = Heap::new(65536).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);
    let handle = new_table(&reg, &mut ctx, "my_tab", "set");
    let obj_a = object(&mut ctx, "a", 1);
    reg.call("ets", "insert", 2, &mut ctx, &[handle, obj_a])
        .expect("insert");

    // A representative sample of the `info/2` items.
    for (item, expected) in [
        ("size", ctx.int(1).unwrap()),
        ("type", ctx.atom("set").unwrap()),
        ("name", ctx.atom("my_tab").unwrap()),
        // The table was created without the `named_table` option, so it reports
        // `named_table => false` (`ets:info(ets:new(t1, [set]), named_table)` ⇒
        // `false` in the oracle).
        ("named_table", ctx.atom("false").unwrap()),
        // The `ets:new(T, [set])` default protection is `protected`, not
        // `public` (verified: `ets:info(ets:new(t, [set]), protection)` ⇒
        // `protected`).
        ("protection", ctx.atom("protected").unwrap()),
        ("keypos", ctx.int(1).unwrap()),
    ] {
        let item = ctx.atom(item).expect("seeded");
        let value = reg
            .call("ets", "info", 2, &mut ctx, &[handle, item])
            .expect("ets:info/2");
        assert!(
            beam_term::compare::eq(value, expected, &atoms),
            "info/2 item mismatch"
        );
    }

    // `info/2` `id` reproduces `TableInfo::entries`: the table's numeric id as a
    // small integer (the oracle reports a tid reference here instead — see the
    // `ets.rs` `info_field` docs).
    let id_item = ctx.atom("id").expect("seeded");
    let id = reg
        .call("ets", "info", 2, &mut ctx, &[handle, id_item])
        .expect("info/2 id");
    let is_int = reg
        .call("erlang", "is_integer", 1, &mut ctx, &[id])
        .expect("erlang:is_integer");
    assert!(is_true(&atoms, is_int), "info/2 `id` is not an integer");

    // `heir` is `none` on a fresh table; `node` is `nonode@nohost`; a `set` reports
    // `decentralized_counters => false` (verified against the system `erl`).
    for (item, expected) in [
        ("heir", ctx.atom("none").unwrap()),
        ("node", ctx.atom("nonode@nohost").unwrap()),
        ("decentralized_counters", ctx.atom("false").unwrap()),
    ] {
        let item = ctx.atom(item).expect("seeded");
        let value = reg
            .call("ets", "info", 2, &mut ctx, &[handle, item])
            .expect("ets:info/2");
        assert!(
            beam_term::compare::eq(value, expected, &atoms),
            "info/2 item mismatch"
        );
    }

    // `info/1` is the oracle's 15-field proplist, in the oracle's order, with one
    // `{Item, Value}` pair per field.
    let proplist = reg
        .call("ets", "info", 1, &mut ctx, &[handle])
        .expect("ets:info/1");
    assert_eq!(length(&reg, &mut ctx, proplist), 15);
    let first_pair = nth(&reg, &mut ctx, 1, proplist);
    assert_eq!(element(&reg, &mut ctx, 1, first_pair), id_item);

    // An unknown item is `badarg` (the verified `erl` behaviour), as is a
    // non-atom item.
    let bogus = ctx.atom("z").expect("seeded");
    assert!(matches!(
        reg.call("ets", "info", 2, &mut ctx, &[handle, bogus]),
        Err(BeamError::BadArg)
    ));
    let one = ctx.int(1).unwrap();
    assert!(matches!(
        reg.call("ets", "info", 2, &mut ctx, &[handle, one]),
        Err(BeamError::BadArg)
    ));
}

#[test]
fn update_counter_and_update_element() {
    let reg = default_registry();
    let mut heap = Heap::new(65536).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);
    let handle = new_table(&reg, &mut ctx, "my_tab", "set");

    let a = ctx.atom("a").expect("seeded");
    let b = ctx.atom("b").expect("seeded");
    let obj_a = object(&mut ctx, "a", 10);
    reg.call("ets", "insert", 2, &mut ctx, &[handle, obj_a])
        .expect("insert");

    // `update_counter/3` with a bare increment bumps element `keypos + 1`.
    let incr = ctx.int(5).expect("fits");
    let value = reg
        .call("ets", "update_counter", 3, &mut ctx, &[handle, a, incr])
        .expect("update_counter/3");
    assert_eq!(value, ctx.int(15).unwrap());

    // A `{Pos, Incr}` spec addresses element 2 explicitly.
    let two = ctx.int(2).expect("fits");
    let spec = ctx.tuple(&[two, incr]).expect("fits");
    let value = reg
        .call("ets", "update_counter", 3, &mut ctx, &[handle, a, spec])
        .expect("update_counter/3 spec");
    assert_eq!(value, ctx.int(20).unwrap());

    // `update_counter/4` deposits the default for an absent key.
    let default = ctx.tuple(&[a, ctx.int(0).unwrap()]).expect("fits");
    let value = reg
        .call(
            "ets",
            "update_counter",
            4,
            &mut ctx,
            &[handle, b, incr, default],
        )
        .expect("update_counter/4");
    assert_eq!(value, ctx.int(5).unwrap());
    let present = reg
        .call("ets", "member", 2, &mut ctx, &[handle, b])
        .expect("member");
    assert!(is_true(&atoms, present));

    // `update_element/3` replaces element 2 and reports `true`.
    let nine = ctx.int(9).expect("fits");
    let pair_spec = ctx.tuple(&[two, nine]).expect("fits");
    let spec_list = ctx.list(&[pair_spec]).expect("fits");
    let result = reg
        .call("ets", "update_element", 3, &mut ctx, &[handle, a, spec_list])
        .expect("update_element");
    assert!(is_true(&atoms, result));
    let stored = reg
        .call("ets", "lookup", 2, &mut ctx, &[handle, a])
        .expect("lookup");
    let stored_object = nth(&reg, &mut ctx, 1, stored);
    assert_eq!(element(&reg, &mut ctx, 2, stored_object), nine);

    // An absent key is `false` regardless of the spec — the oracle validates the
    // spec only for a *present* key (verified: `ets:update_element(T, Absent,
    // BadSpec)` ⇒ `false`; `ets:update_element(T, Present, BadSpec)` ⇒ badarg).
    // `b` is present here (the `update_counter/4` above created it), so use a
    // key that was never inserted. The present-key `badarg` case is covered in
    // `badarg_cases_are_typed`.
    let z = ctx.atom("z").expect("seeded");
    let result = reg
        .call("ets", "update_element", 3, &mut ctx, &[handle, z, nine])
        .expect("update_element absent");
    assert!(is_false(&atoms, result));
}

#[test]
fn badarg_cases_are_typed() {
    let reg = default_registry();
    let mut heap = Heap::new(65536).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);
    let handle = new_table(&reg, &mut ctx, "my_tab", "set");

    let a = ctx.atom("a").expect("seeded");
    let b = ctx.atom("b").expect("seeded");
    let one = ctx.int(1).expect("fits");
    let two = ctx.int(2).expect("fits");
    let obj_a = object(&mut ctx, "a", 1);
    reg.call("ets", "insert", 2, &mut ctx, &[handle, obj_a])
        .expect("insert");

    // A non-table handle (a small int) is `badarg` for every BIF.
    for (name, arity, args) in [
        ("insert_new", 2u32, vec![one, obj_a]),
        ("delete", 2, vec![one, a]),
        ("delete_object", 2, vec![one, obj_a]),
        ("delete_all_objects", 1, vec![one]),
        ("member", 2, vec![one, a]),
        ("lookup_element", 3, vec![one, a, two]),
        ("take", 2, vec![one, a]),
        ("first", 1, vec![one]),
        ("last", 1, vec![one]),
        ("next", 2, vec![one, a]),
        ("prev", 2, vec![one, a]),
        ("info", 1, vec![one]),
        ("tab2list", 1, vec![one]),
        ("update_counter", 3, vec![one, a, one]),
        ("update_element", 3, vec![one, a, two]),
    ] {
        assert!(
            matches!(
                reg.call("ets", name, arity, &mut ctx, &args),
                Err(BeamError::BadArg)
            ),
            "ets:{name}/{arity} on a non-table handle"
        );
    }

    // An absent key is `badarg` for `lookup_element/3` and `update_counter/3`,
    // and an out-of-range position is `badarg` too.
    assert!(matches!(
        reg.call("ets", "lookup_element", 3, &mut ctx, &[handle, b, two]),
        Err(BeamError::BadArg)
    ));
    let nine = ctx.int(9).expect("fits");
    assert!(matches!(
        reg.call("ets", "lookup_element", 3, &mut ctx, &[handle, a, nine]),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        reg.call("ets", "update_counter", 3, &mut ctx, &[handle, b, one]),
        Err(BeamError::BadArg)
    ));
    // `next/2` on a hash table requires a present key.
    assert!(matches!(
        reg.call("ets", "next", 2, &mut ctx, &[handle, b]),
        Err(BeamError::BadArg)
    ));

    // An improper object list and a non-tuple object are `badarg`.
    let improper = ctx.cons(obj_a, obj_a).expect("fits");
    assert!(matches!(
        reg.call("ets", "insert_new", 2, &mut ctx, &[handle, improper]),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        reg.call("ets", "insert", 2, &mut ctx, &[handle, a]),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        reg.call("ets", "delete_object", 2, &mut ctx, &[handle, one]),
        Err(BeamError::BadArg)
    ));

    // A position that is not a small integer is `badarg` for `lookup_element`.
    assert!(matches!(
        reg.call("ets", "lookup_element", 3, &mut ctx, &[handle, a, a]),
        Err(BeamError::BadArg)
    ));

    // A malformed `update_element/3` spec is `badarg` for a *present* key.
    assert!(matches!(
        reg.call("ets", "update_element", 3, &mut ctx, &[handle, a, a]),
        Err(BeamError::BadArg)
    ));
}

#[test]
fn ets_bifs_without_a_registry_are_unsupported() {
    let reg = default_registry();
    let mut heap = Heap::new(65536).expect("test heap");
    let atoms = seeded_atoms();

    // A *valid* handle has to be built first: every handle-taking `ets` BIF
    // decodes its first argument (`ref_thing_id`) *before* it consults the
    // registry, so a non-ref handle is `badarg` (see `badarg_cases_are_typed`)
    // and the missing-registry path is only reached with a real ref-thing
    // handle. The handle is an ordinary on-heap term, so it stays valid once
    // the registry-bound context that produced it is gone.
    let mut tables = TableRegistry::new();
    let handle = {
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);
        new_table(&reg, &mut ctx, "my_tab", "set")
    };

    // No `.with_tables(...)`: a table operation on the valid handle reports a
    // typed `unsupported_bif` raise rather than a fabricated table.
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    let key = ctx.atom("a").expect("seeded");

    for (name, arity, args) in [
        ("member", 2u32, vec![handle, key]),
        ("info", 1, vec![handle]),
        ("first", 1, vec![handle]),
        ("tab2list", 1, vec![handle]),
        ("delete", 1, vec![handle]),
    ] {
        let err = reg
            .call("ets", name, arity, &mut ctx, &args)
            .expect_err("no registry installed");
        assert!(
            is_raise_atom(&err, "unsupported_bif"),
            "ets:{name}/{arity} without a registry: {err:?}"
        );
    }

    // `ets:new/2` needs no handle and reports the same missing-registry raise.
    let name = ctx.atom("tab2").expect("seeded");
    let kind = ctx.atom("set").expect("seeded");
    let options = ctx.list(&[kind]).expect("fits");
    let err = reg
        .call("ets", "new", 2, &mut ctx, &[name, options])
        .expect_err("no registry installed");
    assert!(
        is_raise_atom(&err, "unsupported_bif"),
        "ets:new/2 without a registry: {err:?}"
    );
}
