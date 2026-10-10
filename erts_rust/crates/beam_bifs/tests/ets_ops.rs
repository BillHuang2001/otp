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

//! Integration tests for the expanded `ets:*` bridge, driven through the public
//! [`BifRegistry::call`] over a real [`TableRegistry`].
//!
//! Every BIF added on top of the original `new/2`/`insert/2`/`lookup/2`/`delete/1`
//! is exercised here — `insert_new/2`, `delete/2`, `delete_object/2`,
//! `delete_all_objects/1`, `member/2`, `lookup_element/3,4`, `take/2`,
//! `first/1`, `last/1`, `next/2`, `prev/2`, `info/1,2`, `tab2list/1`,
//! `update_counter/3,4`, `update_element/3`, the `select/1,2,3` family and the
//! `match/1,2,3` / `match_object/1,2,3` / `select_count/2` / `select_delete/2` /
//! `match_delete/2` families — including the `badarg` cases and the no-registry
//! `unsupported_bif` contract.
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
        // The match-spec variables the `ets:select` specs use.
        "$1",
        "$2",
        // The wildcard and the remaining names the match-family tests use.
        "_",
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
        ("select", 1),
        ("select", 2),
        ("select", 3),
        // The match family and the count/delete verbs.
        ("match", 1),
        ("match", 2),
        ("match", 3),
        ("match_object", 1),
        ("match_object", 2),
        ("match_object", 3),
        ("select_count", 2),
        ("select_delete", 2),
        ("select_replace", 2),
        ("match_delete", 2),
        // The compiled match-spec family.
        ("match_spec_compile", 1),
        ("match_spec_run", 2),
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

/// The `'$end_of_table'` **anchor** raises `badarg` for the `ordered_set` walkers
/// `ets:next/2` and `ets:prev/2` — through the public registry — even when the
/// atom is itself a **stored** key; a hash kind is unaffected.
///
/// The C `db_next_tree_common`/`db_prev_tree_common`
/// (`../../erts/emulator/beam/erl_db_tree.c:654,729`) return `DB_ERROR_BADKEY`
/// unconditionally on `am_EOT`, so an `ordered_set` never reads the atom as a
/// range bound (`db_next_hash_common` has no such special-case). Oracle probe
/// (`erl -noshell`, OTP 29.1.1 / erts-17.1), an `ordered_set` of
/// `{1,1},{2,2},{3,3}`: both walkers raise `badarg` on the anchor, stored or not,
/// and on an empty table; a `set` with `'$end_of_table'` stored answers normally.
#[test]
fn ordered_set_walkers_reject_the_end_of_table_anchor() {
    let reg = default_registry();
    let mut heap = Heap::new(65536).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);

    let ordered = new_table(&reg, &mut ctx, "my_tab", "ordered_set");
    let eot = ctx.atom("$end_of_table").expect("seeded");

    // An empty `ordered_set` rejects the anchor for both directions.
    for name in ["next", "prev"] {
        assert!(
            matches!(
                reg.call("ets", name, 2, &mut ctx, &[ordered, eot]),
                Err(BeamError::BadArg)
            ),
            "ets:{name}/2 on an empty ordered_set must be badarg"
        );
    }

    // Three integer-keyed objects, plus `'$end_of_table'` stored as a key.
    for key in 1..=3i64 {
        let k = ctx.int(key).expect("fits");
        let obj = ctx.tuple(&[k, k]).expect("fits");
        reg.call("ets", "insert", 2, &mut ctx, &[ordered, obj])
            .expect("insert");
    }
    let eot_obj = ctx.tuple(&[eot, eot]).expect("fits");
    reg.call("ets", "insert", 2, &mut ctx, &[ordered, eot_obj])
        .expect("insert");

    let one = ctx.int(1).expect("fits");
    let two = ctx.int(2).expect("fits");
    let three = ctx.int(3).expect("fits");

    // The ordinary walks are unaffected.
    assert_eq!(
        reg.call("ets", "next", 2, &mut ctx, &[ordered, one])
            .expect("next(1)"),
        two
    );
    assert_eq!(
        reg.call("ets", "prev", 2, &mut ctx, &[ordered, three])
            .expect("prev(3)"),
        two
    );

    // The anchor is rejected even though it is stored.
    for name in ["next", "prev"] {
        assert!(
            matches!(
                reg.call("ets", name, 2, &mut ctx, &[ordered, eot]),
                Err(BeamError::BadArg)
            ),
            "ets:{name}/2 on the stored '$end_of_table' anchor must be badarg"
        );
    }

    // A hash kind (`set`) is unaffected: with the atom stored it is an ordinary
    // key, so `ets:next/2` walks from it and `ets:prev/2` aliases that step.
    let hashed = new_table(&reg, &mut ctx, "tab2", "set");
    for obj in [
        ctx.tuple(&[one, one]).expect("fits"),
        ctx.tuple(&[eot, eot]).expect("fits"),
    ] {
        reg.call("ets", "insert", 2, &mut ctx, &[hashed, obj])
            .expect("insert");
    }
    let next = reg
        .call("ets", "next", 2, &mut ctx, &[hashed, eot])
        .expect("next(eot) on a set is not badarg");
    let prev = reg
        .call("ets", "prev", 2, &mut ctx, &[hashed, eot])
        .expect("prev(eot) on a set is not badarg");
    assert!(
        beam_term::compare::eq(next, prev, &atoms),
        "a hash kind aliases prev to next"
    );
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

    // `info/2` `id` is the table's **tid reference** — the very handle ref thing
    // `ets:new/2` handed out, so `is_reference(ets:info(T, id))` is `true` and
    // `ets:info(T, id) =:= T` holds (both verified against the system `erl`).
    let id_item = ctx.atom("id").expect("seeded");
    let id = reg
        .call("ets", "info", 2, &mut ctx, &[handle, id_item])
        .expect("info/2 id");
    let is_ref = reg
        .call("erlang", "is_reference", 1, &mut ctx, &[id])
        .expect("erlang:is_reference");
    assert!(is_true(&atoms, is_ref), "info/2 `id` is not a reference");
    assert!(
        beam_term::compare::eq(id, handle, &atoms),
        "ets:info(T, id) must be =:= T"
    );
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

    // The `{'$1','$2'}` pattern the match-family handle checks use.
    let pattern = both_vars_pattern(&mut ctx);

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
        ("match", 1, vec![one]),
        ("match", 2, vec![one, pattern]),
        ("match", 3, vec![one, pattern, two]),
        ("match_object", 1, vec![one]),
        ("match_object", 2, vec![one, pattern]),
        ("match_object", 3, vec![one, pattern, two]),
        ("select_count", 2, vec![one, true_spec(&mut ctx)]),
        ("select_delete", 2, vec![one, true_spec(&mut ctx)]),
        ("select_replace", 2, vec![one, true_spec(&mut ctx)]),
        ("match_delete", 2, vec![one, pattern]),
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
    // A compiled-spec handle is built the same way (it too needs the registry).
    let compiled = {
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);
        let spec = replace_spec(&mut ctx);
        reg.call("ets", "match_spec_compile", 1, &mut ctx, &[spec])
            .expect("ets:match_spec_compile/1")
    };

    // No `.with_tables(...)`: a table operation on the valid handle reports a
    // typed `unsupported_bif` raise rather than a fabricated table.
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    let key = ctx.atom("a").expect("seeded");
    // The arguments the match-family entries need; none of them touches the
    // registry (they are built on the context heap).
    let pattern = both_vars_pattern(&mut ctx);
    let spec = true_spec(&mut ctx);
    let two = ctx.int(2).expect("fits");
    let list = ctx.list(&[]).expect("fits");
    for (name, arity, args) in [
        ("member", 2u32, vec![handle, key]),
        ("info", 1, vec![handle]),
        ("first", 1, vec![handle]),
        ("tab2list", 1, vec![handle]),
        ("delete", 1, vec![handle]),
        // The match family and the count/delete/replace verbs reach the registry
        // only after their handle decodes, so they report the same raise.
        ("match", 1, vec![handle]),
        ("match", 2, vec![handle, pattern]),
        ("match", 3, vec![handle, pattern, two]),
        ("match_object", 1, vec![handle]),
        ("match_object", 2, vec![handle, pattern]),
        ("match_object", 3, vec![handle, pattern, two]),
        ("select_count", 2, vec![handle, spec]),
        ("select_delete", 2, vec![handle, spec]),
        ("select_replace", 2, vec![handle, spec]),
        ("match_delete", 2, vec![handle, pattern]),
        // The compiled match-spec family: `match_spec_compile/1` compiles (it needs
        // no handle) and then reaches the registry; `match_spec_run/2` decodes its
        // valid compiled handle first.
        ("match_spec_compile", 1, vec![spec]),
        ("match_spec_run", 2, vec![list, compiled]),
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

/// The `ets:select/2` spec `[{{'$1','$2'}, [], [{{'$1','$2'}}]}]` — the whole
/// matched object per match — built on `ctx`.
///
/// This is the raw (uncompiled) spec list every `ets:select` entry point takes;
/// `beam_ets::compile` lowers it inside the BIF.
fn whole_object_spec(ctx: &mut BifContext<'_>) -> Eterm {
    let v1 = ctx.atom("$1").expect("seeded");
    let v2 = ctx.atom("$2").expect("seeded");
    let head = ctx.tuple(&[v1, v2]).expect("fits");
    let guards = ctx.list(&[]).expect("fits");
    // The `{{ … }}` construct form is a 1-tuple holding the tuple to build.
    let projected = ctx.tuple(&[v1, v2]).expect("fits");
    let body_item = ctx.tuple(&[projected]).expect("fits");
    let body = ctx.list(&[body_item]).expect("fits");
    let clause = ctx.tuple(&[head, guards, body]).expect("fits");
    ctx.list(&[clause]).expect("fits")
}

/// The 1-based key/value-carrying ordered objects `{a,1} … ` for `keys`, inserted
/// into `handle` (they insert in ascending key order).
fn insert_ordered(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    handle: Eterm,
    keys: &[&str],
) {
    for (i, key) in keys.iter().enumerate() {
        let obj = object(ctx, key, i as i64 + 1);
        reg.call("ets", "insert", 2, ctx, &[handle, obj])
            .expect("insert");
    }
}

#[test]
fn select_2_projects_every_matching_object_in_iteration_order() {
    let reg = default_registry();
    let mut heap = Heap::new(65536).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);
    // An `ordered_set` iterates in key order, so the projection is deterministic
    // (a `set`'s order is unspecified).
    let handle = new_table(&reg, &mut ctx, "tab2", "ordered_set");
    insert_ordered(&reg, &mut ctx, handle, &["a", "b", "c"]);

    let spec = whole_object_spec(&mut ctx);
    let got = reg
        .call("ets", "select", 2, &mut ctx, &[handle, spec])
        .expect("ets:select/2");

    // Verified against the system `erl`: the body `{{'$1','$2'}}` yields the whole
    // object per match, in iteration order.
    let a = object(&mut ctx, "a", 1);
    let b = object(&mut ctx, "b", 2);
    let c = object(&mut ctx, "c", 3);
    let expected = ctx.list(&[a, b, c]).expect("fits");
    assert!(
        beam_term::compare::eq(got, expected, &atoms),
        "ets:select/2 projection mismatch"
    );

    // No matches ⇒ the empty list (the oracle returns `[]`, not `'$end_of_table'`).
    let no_match = ctx.int(99).expect("fits");
    let no_match_head = ctx.tuple(&[no_match, no_match]).expect("fits");
    let guards = ctx.list(&[]).expect("fits");
    let body = ctx.list(&[no_match]).expect("fits");
    let clause = ctx.tuple(&[no_match_head, guards, body]).expect("fits");
    let spec = ctx.list(&[clause]).expect("fits");
    let empty = reg
        .call("ets", "select", 2, &mut ctx, &[handle, spec])
        .expect("ets:select/2 no match");
    assert_eq!(length(&reg, &mut ctx, empty), 0);

    // A malformed spec is `badarg`, as is a non-ref handle.
    let one = ctx.int(1).expect("fits");
    assert!(matches!(
        reg.call("ets", "select", 2, &mut ctx, &[handle, one]),
        Err(BeamError::BadArg)
    ));
    let spec = whole_object_spec(&mut ctx);
    assert!(matches!(
        reg.call("ets", "select", 2, &mut ctx, &[one, spec]),
        Err(BeamError::BadArg)
    ));
}

#[test]
fn select_3_limit_and_select_1_continuations_round_trip() {
    let reg = default_registry();
    let mut heap = Heap::new(65536).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);
    let handle = new_table(&reg, &mut ctx, "tab2", "ordered_set");
    insert_ordered(&reg, &mut ctx, handle, &["a", "b", "c", "p"]);
    let spec = whole_object_spec(&mut ctx);
    let two = ctx.int(2).expect("fits");
    let end = ctx.atom("$end_of_table").expect("seeded");

    // Chunk 1: `{Results, Continuation}` — two results and a continuation ref
    // thing (never the `'$end_of_table'` atom).
    let chunk1 = reg
        .call("ets", "select", 3, &mut ctx, &[handle, spec, two])
        .expect("ets:select/3");
    let results1 = element(&reg, &mut ctx, 1, chunk1);
    let cont1 = element(&reg, &mut ctx, 2, chunk1);
    assert_eq!(length(&reg, &mut ctx, results1), 2);
    let objects = [object(&mut ctx, "a", 1), object(&mut ctx, "b", 2)];
    let expected1 = ctx.list(&objects).expect("fits");
    assert!(beam_term::compare::eq(results1, expected1, &atoms));
    assert!(!beam_term::compare::eq(cont1, end, &atoms), "chunk 1 produced no continuation");

    // Chunk 2: the second `limit`-sized chunk, off the continuation.
    let chunk2 = reg
        .call("ets", "select", 1, &mut ctx, &[cont1])
        .expect("ets:select/1");
    let results2 = element(&reg, &mut ctx, 1, chunk2);
    let cont2 = element(&reg, &mut ctx, 2, chunk2);
    assert_eq!(length(&reg, &mut ctx, results2), 2);
    let objects = [object(&mut ctx, "c", 3), object(&mut ctx, "p", 4)];
    let expected2 = ctx.list(&objects).expect("fits");
    assert!(beam_term::compare::eq(results2, expected2, &atoms));

    // Chunk 3: the traversal finished after the limit landed on the last object, so
    // the results are empty and the oracle renders the **bare** `'$end_of_table'`
    // atom (verified against erts-17.1).
    let chunk3 = reg
        .call("ets", "select", 1, &mut ctx, &[cont2])
        .expect("ets:select/1");
    assert_eq!(
        atoms.name(atom_val(chunk3)),
        Some("$end_of_table"),
        "an exhausted continuation must render as the bare '$end_of_table'"
    );

    // An unknown continuation handle is `badarg` (a plain table handle names no
    // continuation).
    assert!(matches!(
        reg.call("ets", "select", 1, &mut ctx, &[handle]),
        Err(BeamError::BadArg)
    ));

    // `limit == 0` is `badarg` (the oracle raises it).
    let zero = ctx.int(0).expect("fits");
    let spec = whole_object_spec(&mut ctx);
    assert!(matches!(
        reg.call("ets", "select", 3, &mut ctx, &[handle, spec, zero]),
        Err(BeamError::BadArg)
    ));
    // A non-integer limit is `badarg` too.
    let spec = whole_object_spec(&mut ctx);
    let bad_limit = ctx.atom("a").expect("seeded");
    assert!(matches!(
        reg.call("ets", "select", 3, &mut ctx, &[handle, spec, bad_limit]),
        Err(BeamError::BadArg)
    ));
}

#[test]
fn select_3_final_chunk_is_a_tuple_when_it_carries_results() {
    let reg = default_registry();
    let mut heap = Heap::new(65536).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);
    let handle = new_table(&reg, &mut ctx, "tab2", "ordered_set");
    // Five objects: `limit` 2 gives 2+cont, 2+cont, then a **tuple** chunk
    // `{[Last], '$end_of_table'}` (the oracle renders the trailing results inside a
    // tuple, unlike the empty case which is the bare atom).
    insert_ordered(&reg, &mut ctx, handle, &["a", "b", "c", "p", "q"]);
    let spec = whole_object_spec(&mut ctx);
    let two = ctx.int(2).expect("fits");

    let chunk1 = reg
        .call("ets", "select", 3, &mut ctx, &[handle, spec, two])
        .expect("ets:select/3");
    let cont1 = element(&reg, &mut ctx, 2, chunk1);
    let chunk2 = reg
        .call("ets", "select", 1, &mut ctx, &[cont1])
        .expect("ets:select/1");
    let cont2 = element(&reg, &mut ctx, 2, chunk2);

    let chunk3 = reg
        .call("ets", "select", 1, &mut ctx, &[cont2])
        .expect("ets:select/1");
    let results3 = element(&reg, &mut ctx, 1, chunk3);
    let terminator = element(&reg, &mut ctx, 2, chunk3);
    assert_eq!(length(&reg, &mut ctx, results3), 1);
    let q = object(&mut ctx, "q", 5);
    let expected = ctx.list(&[q]).expect("fits");
    assert!(beam_term::compare::eq(results3, expected, &atoms));
    let end = ctx.atom("$end_of_table").expect("seeded");
    assert!(beam_term::compare::eq(terminator, end, &atoms));
}

// ------------------------------------------------------------- match family

/// The `{'$1','$2'}` match pattern: both variables bound, so `ets:match/2`
/// yields `[Key, Value]` per object and `ets:match_object/2` yields `{Key, Value}`.
///
/// Unlike a *match spec* (where a clause's second element is the guard list), a
/// `ets:match/2` pattern is the bare head term — the assertions below are the
/// oracle's (`ets:match(T, {'$1','$2'})` on `{1,a}` ⇒ `[[1,a]]`).
fn both_vars_pattern(ctx: &mut BifContext<'_>) -> Eterm {
    let v1 = ctx.atom("$1").expect("seeded");
    let v2 = ctx.atom("$2").expect("seeded");
    ctx.tuple(&[v1, v2]).expect("fits")
}

/// The counting/deleting spec `[{{'$1','$2'}, [], [true]}]` — one clause whose body
/// is the boolean `true`, so `ets:select_count/2` counts every 2-tuple and
/// `ets:select_delete/2` deletes them.
fn true_spec(ctx: &mut BifContext<'_>) -> Eterm {
    let v1 = ctx.atom("$1").expect("seeded");
    let v2 = ctx.atom("$2").expect("seeded");
    let head = ctx.tuple(&[v1, v2]).expect("fits");
    let guards = ctx.list(&[]).expect("fits");
    let body = ctx.list(&[ctx.atom("true").expect("seeded")]).expect("fits");
    let clause = ctx.tuple(&[head, guards, body]).expect("fits");
    ctx.list(&[clause]).expect("fits")
}

/// `[{{z}, [], [true]}]` — a spec whose head matches no object of a 2-tuple table.
fn no_match_spec(ctx: &mut BifContext<'_>) -> Eterm {
    let z = ctx.atom("z").expect("seeded");
    let head = ctx.tuple(&[z]).expect("fits");
    let guards = ctx.list(&[]).expect("fits");
    let body = ctx.list(&[ctx.atom("true").expect("seeded")]).expect("fits");
    let clause = ctx.tuple(&[head, guards, body]).expect("fits");
    ctx.list(&[clause]).expect("fits")
}

/// A literal `{Key, '_'}` match pattern — binds nothing, matches one key.
fn key_pattern(ctx: &mut BifContext<'_>, key: &str) -> Eterm {
    let key = ctx.atom(key).expect("seeded");
    let any = ctx.atom("_").expect("seeded");
    ctx.tuple(&[key, any]).expect("fits")
}

/// The replacing spec `[{{'$1','$2'}, [], [{{'$1',b}}]}]` — matches every 2-tuple
/// and replaces it with `{Key, b}`, keeping the key at position 1.
///
/// This is the oracle's `ets:select_replace/2` idiom (the `{{ … }}` construction
/// form is a 1-tuple holding the tuple to build); it is the raw spec term
/// `ets:match_spec_compile/1` and `ets:select_replace/2` take.
fn replace_spec(ctx: &mut BifContext<'_>) -> Eterm {
    let v1 = ctx.atom("$1").expect("seeded");
    let v2 = ctx.atom("$2").expect("seeded");
    let b = ctx.atom("b").expect("seeded");
    let head = ctx.tuple(&[v1, v2]).expect("fits");
    let guards = ctx.list(&[]).expect("fits");
    let constructed = ctx.tuple(&[v1, b]).expect("fits");
    let body_item = ctx.tuple(&[constructed]).expect("fits");
    let body = ctx.list(&[body_item]).expect("fits");
    let clause = ctx.tuple(&[head, guards, body]).expect("fits");
    ctx.list(&[clause]).expect("fits")
}

#[test]
fn match_2_binds_every_variable_and_match_object_2_returns_whole_objects() {
    let reg = default_registry();
    let mut heap = Heap::new(65536).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);
    // An `ordered_set` iterates in key order, so the results are deterministic.
    let handle = new_table(&reg, &mut ctx, "tab2", "ordered_set");
    insert_ordered(&reg, &mut ctx, handle, &["a", "b", "c"]);

    // `ets:match(T, {'$1','$2'})` ⇒ `[[Key, Value], …]` (verified against erts-17.1:
    // `[[1,a]]` for the single object `{1,a}`).
    let pattern = both_vars_pattern(&mut ctx);
    let got = reg
        .call("ets", "match", 2, &mut ctx, &[handle, pattern])
        .expect("ets:match/2");
    let pairs = [
        ctx.list(&[ctx.atom("a").unwrap(), ctx.int(1).unwrap()])
            .expect("fits"),
        ctx.list(&[ctx.atom("b").unwrap(), ctx.int(2).unwrap()])
            .expect("fits"),
        ctx.list(&[ctx.atom("c").unwrap(), ctx.int(3).unwrap()])
            .expect("fits"),
    ];
    let expected = ctx.list(&pairs).expect("fits");
    assert!(
        beam_term::compare::eq(got, expected, &atoms),
        "ets:match/2 binding lists mismatch"
    );

    // `ets:match_object/2` ⇒ the whole objects.
    let pattern = both_vars_pattern(&mut ctx);
    let got = reg
        .call("ets", "match_object", 2, &mut ctx, &[handle, pattern])
        .expect("ets:match_object/2");
    let objects = [
        object(&mut ctx, "a", 1),
        object(&mut ctx, "b", 2),
        object(&mut ctx, "c", 3),
    ];
    let expected = ctx.list(&objects).expect("fits");
    assert!(
        beam_term::compare::eq(got, expected, &atoms),
        "ets:match_object/2 whole-object mismatch"
    );

    // A pattern that matches nothing yields the **empty list** for both — never
    // `'$end_of_table'` (verified: `ets:match(T, {x,y})` ⇒ `[]`).
    let literal = {
        let z = ctx.atom("z").expect("seeded");
        ctx.tuple(&[z, z]).expect("fits")
    };
    let got = reg
        .call("ets", "match", 2, &mut ctx, &[handle, literal])
        .expect("ets:match/2 no match");
    assert_eq!(length(&reg, &mut ctx, got), 0);
    let got = reg
        .call("ets", "match_object", 2, &mut ctx, &[handle, literal])
        .expect("ets:match_object/2 no match");
    assert_eq!(length(&reg, &mut ctx, got), 0);
}

#[test]
fn match_3_and_match_1_chunk_through_the_shared_continuation_renderer() {
    let reg = default_registry();
    let mut heap = Heap::new(65536).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);
    let handle = new_table(&reg, &mut ctx, "tab2", "ordered_set");
    insert_ordered(&reg, &mut ctx, handle, &["a", "b", "c", "p"]);
    let two = ctx.int(2).expect("fits");
    let end = ctx.atom("$end_of_table").expect("seeded");

    // Chunk 1 of `ets:match/3`: two binding lists plus a continuation.
    let pattern = both_vars_pattern(&mut ctx);
    let chunk1 = reg
        .call("ets", "match", 3, &mut ctx, &[handle, pattern, two])
        .expect("ets:match/3");
    let results1 = element(&reg, &mut ctx, 1, chunk1);
    let cont1 = element(&reg, &mut ctx, 2, chunk1);
    let pairs = [
        ctx.list(&[ctx.atom("a").unwrap(), ctx.int(1).unwrap()])
            .expect("fits"),
        ctx.list(&[ctx.atom("b").unwrap(), ctx.int(2).unwrap()])
            .expect("fits"),
    ];
    let expected1 = ctx.list(&pairs).expect("fits");
    assert!(beam_term::compare::eq(results1, expected1, &atoms));
    assert!(
        !beam_term::compare::eq(cont1, end, &atoms),
        "ets:match/3 produced no continuation"
    );

    // Chunk 2 off `ets:match/1`.
    let chunk2 = reg
        .call("ets", "match", 1, &mut ctx, &[cont1])
        .expect("ets:match/1");
    let results2 = element(&reg, &mut ctx, 1, chunk2);
    let cont2 = element(&reg, &mut ctx, 2, chunk2);
    let pairs = [
        ctx.list(&[ctx.atom("c").unwrap(), ctx.int(3).unwrap()])
            .expect("fits"),
        ctx.list(&[ctx.atom("p").unwrap(), ctx.int(4).unwrap()])
            .expect("fits"),
    ];
    let expected2 = ctx.list(&pairs).expect("fits");
    assert!(beam_term::compare::eq(results2, expected2, &atoms));

    // Chunk 3: the limit landed on the last object, so the resume finds nothing and
    // the oracle renders the **bare** `'$end_of_table'` atom.
    let chunk3 = reg
        .call("ets", "match", 1, &mut ctx, &[cont2])
        .expect("ets:match/1");
    assert_eq!(
        atoms.name(atom_val(chunk3)),
        Some("$end_of_table"),
        "an exhausted `ets:match/1` must render as the bare '$end_of_table'"
    );

    // The same round trip through `ets:match_object/3`+`/1`: whole objects, and the
    // final chunk is again the bare atom.
    let pattern = both_vars_pattern(&mut ctx);
    let chunk1 = reg
        .call("ets", "match_object", 3, &mut ctx, &[handle, pattern, two])
        .expect("ets:match_object/3");
    let objects = [object(&mut ctx, "a", 1), object(&mut ctx, "b", 2)];
    let expected = ctx.list(&objects).expect("fits");
    assert!(beam_term::compare::eq(
        element(&reg, &mut ctx, 1, chunk1),
        expected,
        &atoms
    ));
    let cont1 = element(&reg, &mut ctx, 2, chunk1);
    let chunk2 = reg
        .call("ets", "match_object", 1, &mut ctx, &[cont1])
        .expect("ets:match_object/1");
    let objects = [object(&mut ctx, "c", 3), object(&mut ctx, "p", 4)];
    let expected = ctx.list(&objects).expect("fits");
    assert!(beam_term::compare::eq(
        element(&reg, &mut ctx, 1, chunk2),
        expected,
        &atoms
    ));
    let cont2 = element(&reg, &mut ctx, 2, chunk2);
    let chunk3 = reg
        .call("ets", "match_object", 1, &mut ctx, &[cont2])
        .expect("ets:match_object/1");
    assert_eq!(atoms.name(atom_val(chunk3)), Some("$end_of_table"));
}

#[test]
fn match_3_final_chunk_is_a_tuple_when_it_carries_results() {
    let reg = default_registry();
    let mut heap = Heap::new(65536).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);
    let handle = new_table(&reg, &mut ctx, "tab2", "ordered_set");
    // Five objects: `limit` 2 gives 2+cont, 2+cont, then the **tuple** chunk
    // `{[Last], '$end_of_table'}` (verified: `ets:match/1` renders trailing results
    // inside a tuple, unlike the empty case).
    insert_ordered(&reg, &mut ctx, handle, &["a", "b", "c", "p", "q"]);
    let two = ctx.int(2).expect("fits");

    let pattern = both_vars_pattern(&mut ctx);
    let chunk1 = reg
        .call("ets", "match", 3, &mut ctx, &[handle, pattern, two])
        .expect("ets:match/3");
    let cont1 = element(&reg, &mut ctx, 2, chunk1);
    let chunk2 = reg
        .call("ets", "match", 1, &mut ctx, &[cont1])
        .expect("ets:match/1");
    let cont2 = element(&reg, &mut ctx, 2, chunk2);
    let chunk3 = reg
        .call("ets", "match", 1, &mut ctx, &[cont2])
        .expect("ets:match/1");
    let results3 = element(&reg, &mut ctx, 1, chunk3);
    let terminator = element(&reg, &mut ctx, 2, chunk3);
    let pairs = [ctx
        .list(&[ctx.atom("q").unwrap(), ctx.int(5).unwrap()])
        .expect("fits")];
    let expected = ctx.list(&pairs).expect("fits");
    assert!(beam_term::compare::eq(results3, expected, &atoms));
    let end = ctx.atom("$end_of_table").expect("seeded");
    assert!(beam_term::compare::eq(terminator, end, &atoms));
}

#[test]
fn match_family_limit_and_continuation_badarg_cases() {
    let reg = default_registry();
    let mut heap = Heap::new(65536).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);
    let handle = new_table(&reg, &mut ctx, "tab2", "set");
    insert_ordered(&reg, &mut ctx, handle, &["a", "b"]);

    let zero = ctx.int(0).expect("fits");
    let one = ctx.int(1).expect("fits");

    // `limit == 0` is `badarg` for both `/3` forms (verified against erts-17.1), as
    // is a negative or non-integer limit.
    for name in ["match", "match_object"] {
        let pattern = both_vars_pattern(&mut ctx);
        assert!(
            matches!(
                reg.call("ets", name, 3, &mut ctx, &[handle, pattern, zero]),
                Err(BeamError::BadArg)
            ),
            "ets:{name}/3 with limit 0"
        );
        let pattern = both_vars_pattern(&mut ctx);
        let minus_one = ctx.int(-1).expect("fits");
        assert!(matches!(
            reg.call("ets", name, 3, &mut ctx, &[handle, pattern, minus_one]),
            Err(BeamError::BadArg)
        ));
        let pattern = both_vars_pattern(&mut ctx);
        let bad_limit = ctx.atom("a").expect("seeded");
        assert!(matches!(
            reg.call("ets", name, 3, &mut ctx, &[handle, pattern, bad_limit]),
            Err(BeamError::BadArg)
        ));
    }

    // A `/1` form on something that is not a live continuation is `badarg`: the
    // table handle names no continuation (verified: `ets:match(T)` ⇒ badarg), and
    // neither does a non-ref term.
    assert!(matches!(
        reg.call("ets", "match", 1, &mut ctx, &[handle]),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        reg.call("ets", "match_object", 1, &mut ctx, &[handle]),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        reg.call("ets", "match", 1, &mut ctx, &[one]),
        Err(BeamError::BadArg)
    ));

    // A continuation of a **deleted** table is `badarg` (deleting the table drops
    // its continuations).
    let pattern = both_vars_pattern(&mut ctx);
    let chunk = reg
        .call("ets", "match", 3, &mut ctx, &[handle, pattern, one])
        .expect("ets:match/3");
    let stale = element(&reg, &mut ctx, 2, chunk);
    let deleted = reg
        .call("ets", "delete", 1, &mut ctx, &[handle])
        .expect("ets:delete/1");
    assert!(is_true(&atoms, deleted));
    assert!(matches!(
        reg.call("ets", "match", 1, &mut ctx, &[stale]),
        Err(BeamError::BadArg)
    ));
}

#[test]
fn select_count_select_delete_and_match_delete_return_the_oracle_values() {
    let reg = default_registry();
    let mut heap = Heap::new(65536).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);
    let handle = new_table(&reg, &mut ctx, "tab2", "ordered_set");
    insert_ordered(&reg, &mut ctx, handle, &["a", "b", "c", "p"]);

    // `ets:select_count/2` ⇒ a small int (verified: 4 for the four 2-tuples).
    let spec = true_spec(&mut ctx);
    let count = reg
        .call("ets", "select_count", 2, &mut ctx, &[handle, spec])
        .expect("ets:select_count/2");
    assert_eq!(signed_value(count), 4);
    let is_int = reg
        .call("erlang", "is_integer", 1, &mut ctx, &[count])
        .expect("erlang:is_integer");
    assert!(is_true(&atoms, is_int), "ets:select_count/2 must return a small int");

    // A spec whose head matches nothing counts `0` (verified).
    let spec = no_match_spec(&mut ctx);
    let count = reg
        .call("ets", "select_count", 2, &mut ctx, &[handle, spec])
        .expect("ets:select_count/2 no match");
    assert_eq!(signed_value(count), 0);

    // `ets:select_delete/2` ⇒ the number deleted (verified: 0 when nothing matches,
    // then the table size, then 0 again).
    let spec = no_match_spec(&mut ctx);
    let deleted = reg
        .call("ets", "select_delete", 2, &mut ctx, &[handle, spec])
        .expect("ets:select_delete/2 no match");
    assert_eq!(signed_value(deleted), 0);
    let spec = true_spec(&mut ctx);
    let deleted = reg
        .call("ets", "select_delete", 2, &mut ctx, &[handle, spec])
        .expect("ets:select_delete/2");
    assert_eq!(signed_value(deleted), 4);
    let spec = true_spec(&mut ctx);
    let deleted = reg
        .call("ets", "select_delete", 2, &mut ctx, &[handle, spec])
        .expect("ets:select_delete/2 empty");
    assert_eq!(signed_value(deleted), 0);

    // `ets:match_delete/2` ⇒ the atom `true` (the oracle's stdlib wrapper discards
    // the deleted count) and **not** the count.
    insert_ordered(&reg, &mut ctx, handle, &["a", "b", "c", "p"]);
    let pattern = both_vars_pattern(&mut ctx);
    let result = reg
        .call("ets", "match_delete", 2, &mut ctx, &[handle, pattern])
        .expect("ets:match_delete/2");
    assert!(
        is_true(&atoms, result),
        "ets:match_delete/2 must return `true`, got {result:?}"
    );
    let spec = true_spec(&mut ctx);
    let count = reg
        .call("ets", "select_count", 2, &mut ctx, &[handle, spec])
        .expect("ets:select_count/2 after match_delete");
    assert_eq!(signed_value(count), 0);

    // A pattern that matches nothing still returns `true` and deletes nothing
    // (verified: `ets:match_delete(T, {x,'_'})` ⇒ `true` with the table intact).
    insert_ordered(&reg, &mut ctx, handle, &["a", "b"]);
    let literal = key_pattern(&mut ctx, "z");
    let result = reg
        .call("ets", "match_delete", 2, &mut ctx, &[handle, literal])
        .expect("ets:match_delete/2 no match");
    assert!(is_true(&atoms, result));
    let spec = true_spec(&mut ctx);
    let count = reg
        .call("ets", "select_count", 2, &mut ctx, &[handle, spec])
        .expect("ets:select_count/2 after no-op match_delete");
    assert_eq!(signed_value(count), 2);

    // A `{Key, '_'}` pattern removes exactly that key's entry: `q` goes, the
    // `{a,1}`/`{b,2}` entries stay (verified with `ets:match_delete(T, {1,'_'})`).
    insert_ordered(&reg, &mut ctx, handle, &["q"]);
    let only_q = key_pattern(&mut ctx, "q");
    let result = reg
        .call("ets", "match_delete", 2, &mut ctx, &[handle, only_q])
        .expect("ets:match_delete/2 by key");
    assert!(is_true(&atoms, result));
    let q = ctx.atom("q").expect("seeded");
    let lookup = reg
        .call("ets", "lookup", 2, &mut ctx, &[handle, q])
        .expect("ets:lookup/2");
    assert_eq!(length(&reg, &mut ctx, lookup), 0);
    let spec = true_spec(&mut ctx);
    let count = reg
        .call("ets", "select_count", 2, &mut ctx, &[handle, spec])
        .expect("ets:select_count/2 after keyed match_delete");
    assert_eq!(signed_value(count), 2);
}
#[test]
fn match_spec_compile_run_and_select_replace_behave_like_the_oracle() {
    let reg = default_registry();
    let mut heap = Heap::new(65536).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);

    // ---- ets:match_spec_compile/1 returns a `#Ref<…>` (verified: is_reference/1).
    let spec = replace_spec(&mut ctx);
    let compiled = reg
        .call("ets", "match_spec_compile", 1, &mut ctx, &[spec])
        .expect("ets:match_spec_compile/1");
    let is_ref = reg
        .call("erlang", "is_reference", 1, &mut ctx, &[compiled])
        .expect("erlang:is_reference/1");
    assert!(
        is_true(&atoms, is_ref),
        "ets:match_spec_compile/1 must return a reference"
    );

    // Every compile is a fresh handle.
    let spec = replace_spec(&mut ctx);
    let compiled2 = reg
        .call("ets", "match_spec_compile", 1, &mut ctx, &[spec])
        .expect("ets:match_spec_compile/1");
    let same = reg
        .call("erlang", "=:=", 2, &mut ctx, &[compiled, compiled2])
        .expect("erlang:=:=/2");
    assert!(
        is_false(&atoms, same),
        "each ets:match_spec_compile/1 must return a fresh handle"
    );

    // A malformed spec is `badarg` (verified: `ets:match_spec_compile(bad)` and
    // `ets:match_spec_compile([{bad}])`).
    let bad = ctx.int(1).expect("fits");
    assert!(matches!(
        reg.call("ets", "match_spec_compile", 1, &mut ctx, &[bad]),
        Err(BeamError::BadArg)
    ));
    let bad_clause = ctx.tuple(&[bad]).expect("fits");
    let bad_spec = ctx.list(&[bad_clause]).expect("fits");
    assert!(matches!(
        reg.call("ets", "match_spec_compile", 1, &mut ctx, &[bad_spec]),
        Err(BeamError::BadArg)
    ));

    // ---- ets:match_spec_run/2 runs the *compiled* handle over a list.
    let a1 = object(&mut ctx, "a", 1);
    let b2 = object(&mut ctx, "b", 2);
    let list = ctx.list(&[a1, b2]).expect("fits");
    let got = reg
        .call("ets", "match_spec_run", 2, &mut ctx, &[list, compiled])
        .expect("ets:match_spec_run/2");
    let a = ctx.atom("a").expect("seeded");
    let b = ctx.atom("b").expect("seeded");
    let a_b = ctx.tuple(&[a, b]).expect("fits");
    let b_b = ctx.tuple(&[b, b]).expect("fits");
    let expected = ctx.list(&[a_b, b_b]).expect("fits");
    assert!(
        beam_term::compare::eq(got, expected, &atoms),
        "ets:match_spec_run/2 projection mismatch"
    );

    // A non-matching element is dropped (an atom is not a 2-tuple), and an empty
    // list yields the empty list (verified: `[]` in both cases).
    let z = ctx.atom("z").expect("seeded");
    let nonmatching = ctx.list(&[z]).expect("fits");
    let got = reg
        .call("ets", "match_spec_run", 2, &mut ctx, &[nonmatching, compiled])
        .expect("ets:match_spec_run/2 no match");
    assert_eq!(length(&reg, &mut ctx, got), 0);
    let empty = ctx.list(&[]).expect("fits");
    let got = reg
        .call("ets", "match_spec_run", 2, &mut ctx, &[empty, compiled])
        .expect("ets:match_spec_run/2 empty");
    assert_eq!(length(&reg, &mut ctx, got), 0);

    // A **raw** clause list as the spec is `badarg` — the oracle's match_spec_run/2
    // takes only the compiled handle, the direct opposite of the select family.
    let raw = replace_spec(&mut ctx);
    assert!(matches!(
        reg.call("ets", "match_spec_run", 2, &mut ctx, &[list, raw]),
        Err(BeamError::BadArg)
    ));

    // A non-reference spec, a non-list first argument and an improper list are all
    // `badarg` (all verified).
    let one = ctx.int(1).expect("fits");
    assert!(matches!(
        reg.call("ets", "match_spec_run", 2, &mut ctx, &[list, one]),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        reg.call("ets", "match_spec_run", 2, &mut ctx, &[one, compiled]),
        Err(BeamError::BadArg)
    ));
    let improper = ctx.cons(a1, z).expect("fits");
    assert!(matches!(
        reg.call("ets", "match_spec_run", 2, &mut ctx, &[improper, compiled]),
        Err(BeamError::BadArg)
    ));

    // A *forged* compiled handle (a ref thing the specs registry does not know) is
    // `badarg`: the table handle built below is exactly such a reference.
    let table = new_table(&reg, &mut ctx, "my_tab", "set");
    assert!(matches!(
        reg.call("ets", "match_spec_run", 2, &mut ctx, &[list, table]),
        Err(BeamError::BadArg)
    ));

    // ---- The compiled handle is NOT accepted by the select family (badarg).
    for (name, arity, args) in [
        ("select", 2u32, vec![table, compiled]),
        ("select_count", 2, vec![table, compiled]),
        ("select_delete", 2, vec![table, compiled]),
        ("select_replace", 2, vec![table, compiled]),
    ] {
        assert!(
            matches!(
                reg.call("ets", name, arity, &mut ctx, &args),
                Err(BeamError::BadArg)
            ),
            "ets:{name}/{arity} must reject a compiled spec (badarg)"
        );
    }

    // ---- ets:select_replace/2 replaces and counts (verified counts).
    insert_ordered(&reg, &mut ctx, table, &["a", "b", "c"]);
    let spec = replace_spec(&mut ctx);
    let count = reg
        .call("ets", "select_replace", 2, &mut ctx, &[table, spec])
        .expect("ets:select_replace/2");
    assert_eq!(signed_value(count), 3, "every 2-tuple object is replaced");
    // The replacements landed: `{a,1}`/`{b,2}`/`{c,3}` ⇒ `{a,b}`/`{b,b}`/`{c,b}`.
    let stored = reg
        .call("ets", "lookup", 2, &mut ctx, &[table, a])
        .expect("ets:lookup/2");
    let stored = nth(&reg, &mut ctx, 1, stored);
    assert!(beam_term::compare::eq(stored, a_b, &atoms));
    let c = ctx.atom("c").expect("seeded");
    let c_b = ctx.tuple(&[c, b]).expect("fits");
    let stored = reg
        .call("ets", "lookup", 2, &mut ctx, &[table, c])
        .expect("ets:lookup/2");
    let stored = nth(&reg, &mut ctx, 1, stored);
    assert!(beam_term::compare::eq(stored, c_b, &atoms));

    // On a `bag` the whole call is `badarg` — even for a never-matching spec (the
    // oracle rejects the table kind before the contents).
    let bag = new_table(&reg, &mut ctx, "tab2", "bag");
    let spec = replace_spec(&mut ctx);
    assert!(matches!(
        reg.call("ets", "select_replace", 2, &mut ctx, &[bag, spec]),
        Err(BeamError::BadArg)
    ));
    let spec = no_match_spec(&mut ctx);
    assert!(matches!(
        reg.call("ets", "select_replace", 2, &mut ctx, &[bag, spec]),
        Err(BeamError::BadArg)
    ));

    // A `duplicate_bag` is supported.
    let dup = new_table(&reg, &mut ctx, "tab2", "duplicate_bag");
    let obj = object(&mut ctx, "a", 1);
    reg.call("ets", "insert", 2, &mut ctx, &[dup, obj])
        .expect("insert");
    let spec = replace_spec(&mut ctx);
    let count = reg
        .call("ets", "select_replace", 2, &mut ctx, &[dup, spec])
        .expect("ets:select_replace/2 duplicate_bag");
    assert_eq!(signed_value(count), 1);

    // A malformed spec and a non-ref handle are `badarg`.
    assert!(matches!(
        reg.call("ets", "select_replace", 2, &mut ctx, &[table, one]),
        Err(BeamError::BadArg)
    ));
    let spec = replace_spec(&mut ctx);
    assert!(matches!(
        reg.call("ets", "select_replace", 2, &mut ctx, &[one, spec]),
        Err(BeamError::BadArg)
    ));
}
