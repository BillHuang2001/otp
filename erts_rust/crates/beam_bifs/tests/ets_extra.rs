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

//! Integration tests for the `ets:*` verbs added on top of the existing bridge —
//! `ets:safe_fixtable/2`, `ets:whereis/1`, `ets:update_element/4` and
//! `ets:is_compiled_ms/1` — plus the `named_table` name registry `ets:new/2` now
//! keeps in sync (`ets:whereis/1`) and the no-registry `unsupported_bif` contract.
//!
//! Every behaviour asserted here was probed against the system `erl`
//! (OTP 29.1.1 / erts-17.1) first; the probes are quoted beside the assertions.
//! Terms are compared **structurally** ([`beam_term::compare::eq`]) and list/tuple
//! destructuring goes through the registered `erlang`/`lists` BIFs, so the suite
//! uses only the public API.
//!
//! The handle convention the tests rely on is the oracle's: `ets:new(Name,
//! [named_table])` hands back the **name atom**, every other table a handle
//! **reference** (`erlang:is_reference/1` true). A table argument may be either
//! shape — every `ets:*` BIF routes it through the single resolver — so a name
//! atom is accepted wherever the handle ref is. Everything else is the oracle's:
//! the tid reference `ets:whereis/1` answers for a registered name (the `undefined`
//! of an unregistered one or of a non-`named_table` name field), the duplicate-name
//! `badarg`, the freeing of the name on `ets:delete/1`, the `update_element/4`
//! semantics and the `is_compiled_ms/1` predicate.

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry, TableRegistry};
use beam_common::{BeamError, ExitReason};
use beam_process::ProcessId;
use beam_term::{atom_val, AtomTable, Eterm};

/// The atom names these tests need (the `ets` options/values, the probe keys).
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed([
        "true",
        "false",
        "undefined",
        "set",
        "ordered_set",
        "bag",
        "duplicate_bag",
        "keypos",
        "public",
        "named_table",
        "$end_of_table",
        "id",
        // The probe table names and object keys.
        "ntab",
        "justname",
        "my_tab",
        "ignored",
        "k",
        "a",
        "b",
        "c",
        "x",
        "z",
        // The match-spec variables `ets:match_spec_compile/1` needs.
        "$1",
        "$2",
    ]);
    atoms
}

/// `ets:new(Name, Options)` through the registry (`Options` a list of atoms).
fn new_table(
    bifs: &BifRegistry,
    ctx: &mut BifContext<'_>,
    name: &str,
    options: &[&str],
) -> Result<Eterm, BeamError> {
    let name = ctx.atom(name)?;
    let mut opts = Vec::with_capacity(options.len());
    for option in options {
        opts.push(ctx.atom(option)?);
    }
    let opts = ctx.list(&opts)?;
    bifs.call("ets", "new", 2, ctx, &[name, opts])
}

/// `ets:new(Name, Options)`, which must succeed.
fn new_table_ok(
    bifs: &BifRegistry,
    ctx: &mut BifContext<'_>,
    name: &str,
    options: &[&str],
) -> Eterm {
    new_table(bifs, ctx, name, options).expect("ets:new")
}

/// The `{key, value}` object stored under `key` (a 2-tuple on the context heap).
fn pair(ctx: &mut BifContext<'_>, key: &str, value: i64) -> Eterm {
    let k = ctx.atom(key).expect("seeded");
    let v = ctx.int(value).expect("fits");
    ctx.tuple(&[k, v]).expect("fits")
}

/// One `{Position, Value}` entry of an `ets:update_element/3,4` update spec.
fn slot(ctx: &mut BifContext<'_>, position: i64, value: i64) -> Eterm {
    let pos = ctx.int(position).expect("fits");
    let value = ctx.int(value).expect("fits");
    ctx.tuple(&[pos, value]).expect("fits")
}
fn is_atom_named(atoms: &AtomTable, term: Eterm, name: &str) -> bool {
    atoms.name(atom_val(term)) == Some(name)
}

fn is_true(atoms: &AtomTable, term: Eterm) -> bool {
    is_atom_named(atoms, term, "true")
}

fn is_false(atoms: &AtomTable, term: Eterm) -> bool {
    is_atom_named(atoms, term, "false")
}

fn is_raise_atom(err: &BeamError, reason: &'static str) -> bool {
    matches!(err, BeamError::Raise(raise) if raise.reason == ExitReason::atom(reason))
}

fn is_badarg(err: &BeamError) -> bool {
    matches!(err, BeamError::BadArg)
}

/// `erlang:element/2` of the tuple `term`.
fn element(bifs: &BifRegistry, ctx: &mut BifContext<'_>, pos: i64, term: Eterm) -> Eterm {
    let pos = ctx.int(pos).expect("fits");
    bifs.call("erlang", "element", 2, ctx, &[pos, term])
        .expect("erlang:element")
}

/// `lists:nth/2` — the `N`th (1-based) element of the proper list `term`.
fn nth(bifs: &BifRegistry, ctx: &mut BifContext<'_>, pos: i64, term: Eterm) -> Eterm {
    let pos = ctx.int(pos).expect("fits");
    bifs.call("lists", "nth", 2, ctx, &[pos, term])
        .expect("lists:nth")
}

/// The `ets:lookup/2` result of `key` in `handle`.
fn lookup(bifs: &BifRegistry, ctx: &mut BifContext<'_>, handle: Eterm, key: &str) -> Eterm {
    let key = ctx.atom(key).expect("seeded");
    bifs.call("ets", "lookup", 2, ctx, &[handle, key])
        .expect("ets:lookup")
}

/// The first (in `set` the only) object `ets:lookup/2` finds under `key`, or the
/// `NIL` of an empty lookup.
fn found(bifs: &BifRegistry, ctx: &mut BifContext<'_>, handle: Eterm, key: &str) -> Eterm {
    let objects = lookup(bifs, ctx, handle, key);
    nth(bifs, ctx, 1, objects)
}

/// The `ets:whereis/1` answer for the (name-atom) argument `name`.
fn whereis(bifs: &BifRegistry, ctx: &mut BifContext<'_>, name: Eterm) -> Eterm {
    bifs.call("ets", "whereis", 1, ctx, &[name])
        .expect("ets:whereis")
}

/// The raw match spec `[{{'$1','$2'}, [], [{{'$1','$2'}}]}]` (the whole object).
fn whole_object_spec(ctx: &mut BifContext<'_>) -> Eterm {
    let v1 = ctx.atom("$1").expect("seeded");
    let v2 = ctx.atom("$2").expect("seeded");
    let head = ctx.tuple(&[v1, v2]).expect("fits");
    let guards = ctx.list(&[]).expect("fits");
    let projected = ctx.tuple(&[v1, v2]).expect("fits");
    let body_item = ctx.tuple(&[projected]).expect("fits");
    let body = ctx.list(&[body_item]).expect("fits");
    let clause = ctx.tuple(&[head, guards, body]).expect("fits");
    ctx.list(&[clause]).expect("fits")
}

// ------------------------------------------------------------------- whereis

/// The oracle:
/// ```text
/// 1> N = ets:new(ntab,[named_table,set]), {N, ets:whereis(ntab) =:= ets:info(N,id)}.
/// {ntab,true}
/// 2> ets:whereis(justname).              % the name field of a plain table
/// undefined
/// 3> ets:new(justname,[set]), ets:whereis(justname).
/// undefined
/// 4> ets:whereis(123).
/// ** exception error: bad argument
/// ```
#[test]
fn whereis_answers_the_named_tables_tid_and_undefined_otherwise() {
    let bifs = default_registry();
    let mut heap = Heap::new(65536).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);

    let handle = new_table_ok(&bifs, &mut ctx, "ntab", &["named_table", "set"]);
    let name = ctx.atom("ntab").expect("seeded");
    // `ets:new(Name, [named_table])` returns the NAME ATOM (the oracle's `{ntab,…}`),
    // and that name atom is itself a table argument (the single resolver accepts it
    // wherever the handle ref is accepted).
    assert!(beam_term::compare::eq(handle, name, &atoms));
    let object = pair(&mut ctx, "k", 1);
    let out = bifs
        .call("ets", "insert", 2, &mut ctx, &[name, object])
        .expect("ets:insert/2 by name");
    assert!(is_true(&atoms, out));

    let id_item = ctx.atom("id").expect("seeded");
    let id = bifs
        .call("ets", "info", 2, &mut ctx, &[handle, id_item])
        .expect("ets:info/2");
    // `whereis/1` answers the table's **tid**: the very reference `ets:info(T, id)`
    // returns — not the name atom.
    let tid = whereis(&bifs, &mut ctx, name);
    assert!(beam_term::compare::eq(tid, id, &atoms));
    assert!(!beam_term::compare::eq(tid, name, &atoms));
    let out = bifs
        .call("erlang", "is_reference", 1, &mut ctx, &[tid])
        .expect("erlang:is_reference/1");
    assert!(is_true(&atoms, out));
    // An unregistered name is `undefined` — including the name *field* of a table
    // created without `named_table`.
    let _plain = new_table_ok(&bifs, &mut ctx, "justname", &["set"]);
    let justname = ctx.atom("justname").expect("seeded");
    let missing = whereis(&bifs, &mut ctx, justname);
    assert!(is_atom_named(&atoms, missing, "undefined"));
    // A non-atom is `badarg`.
    let seven = ctx.int(7).expect("fits");
    let err = bifs
        .call("ets", "whereis", 1, &mut ctx, &[seven])
        .expect_err("non-atom");
    assert!(is_badarg(&err), "{err:?}");
}

/// The oracle:
/// ```text
/// 1> ets:new(ntab,[named_table,set]), ets:new(ntab,[named_table,set]).
/// ** exception error: bad argument
/// 2> ets:new(ntab,[named_table,set]), ets:new(ntab,[set]).     % unnamed: allowed
/// #Ref<...>
/// 3> N = ets:new(ntab,[named_table,set]), ets:delete(N),
///    {ets:whereis(ntab), is_reference(ets:new(ntab,[named_table,set]))}.
/// {undefined,false}     % the name is free again; `new` returns the NAME ATOM
/// ```
#[test]
fn new_rejects_a_duplicate_name_and_delete_frees_it() {
    let bifs = default_registry();
    let mut heap = Heap::new(65536).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);

    let name = ctx.atom("ntab").expect("seeded");
    // `ets:new(Name, [named_table])` returns the NAME ATOM (the oracle's shape).
    let handle = new_table_ok(&bifs, &mut ctx, "ntab", &["named_table", "set"]);
    assert!(beam_term::compare::eq(handle, name, &atoms));

    // A second `named_table` with the same name is `badarg`; an *unnamed* table
    // reusing the name is fine (it is not registered) and is handed back as a plain
    // handle **reference**, not the name atom.
    let err = new_table(&bifs, &mut ctx, "ntab", &["named_table", "set"]).expect_err("taken");
    assert!(is_badarg(&err), "{err:?}");
    let unnamed = new_table_ok(&bifs, &mut ctx, "ntab", &["set"]);
    assert!(!beam_term::compare::eq(unnamed, name, &atoms));
    let out = bifs
        .call("erlang", "is_reference", 1, &mut ctx, &[unnamed])
        .expect("erlang:is_reference/1");
    assert!(is_true(&atoms, out));
    // The registered name still resolves to the *named* table's tid.
    let id_item = ctx.atom("id").expect("seeded");
    let tid = bifs
        .call("ets", "info", 2, &mut ctx, &[handle, id_item])
        .expect("ets:info/2");
    assert!(beam_term::compare::eq(
        whereis(&bifs, &mut ctx, name),
        tid,
        &atoms
    ));

    // Deleting the named table (by name) clears its name...
    assert!(is_true(
        &atoms,
        bifs.call("ets", "delete", 1, &mut ctx, &[name])
            .expect("ets:delete/1")
    ));
    let gone = whereis(&bifs, &mut ctx, name);
    assert!(is_atom_named(&atoms, gone, "undefined"));

    // ...and the tid captured before the delete is `badarg` afterwards.
    let err = bifs
        .call("ets", "delete", 1, &mut ctx, &[tid])
        .expect_err("deleted tid");
    assert!(is_badarg(&err), "{err:?}");

    // ...so the name can be taken again, and `new` hands back the name atom again
    // (a *fresh* table, so a new tid).
    let again = new_table_ok(&bifs, &mut ctx, "ntab", &["named_table", "set"]);
    assert!(beam_term::compare::eq(again, name, &atoms));
    assert!(!beam_term::compare::eq(
        whereis(&bifs, &mut ctx, name),
        tid,
        &atoms
    ));
}

// ------------------------------------------------------------- safe_fixtable

/// The oracle:
/// ```text
/// 1> T = ets:new(t,[set]),
///    {ets:safe_fixtable(T,true), ets:safe_fixtable(T,false)}.
/// {true,true}
/// 2> ets:safe_fixtable(T,bad).          ets:safe_fixtable(T,1).
/// ** exception error: bad argument     ** exception error: bad argument
/// 3> ets:safe_fixtable(notatable,true).
/// ** exception error: bad argument
/// 4> N = ets:new(ntab,[named_table,set]), ets:safe_fixtable(ntab,true).
/// true
/// 5> ets:new(justname,[set]), ets:safe_fixtable(justname,true).
/// ** exception error: bad argument
/// ```
#[test]
fn safe_fixtable_answers_true_for_both_booleans_and_validates_its_arguments() {
    let bifs = default_registry();
    let mut heap = Heap::new(65536).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);

    let handle = new_table_ok(&bifs, &mut ctx, "my_tab", &["set"]);
    let on = ctx.atom("true").expect("seeded");
    let off = ctx.atom("false").expect("seeded");
    for flag in [on, off] {
        let out = bifs
            .call("ets", "safe_fixtable", 2, &mut ctx, &[handle, flag])
            .expect("ets:safe_fixtable/2");
        assert!(is_true(&atoms, out));
    }

    // A `named_table`'s name atom is a table argument (the oracle: `true`).
    let named = new_table_ok(&bifs, &mut ctx, "ntab", &["named_table", "set"]);
    let name = ctx.atom("ntab").expect("seeded");
    let out = bifs
        .call("ets", "safe_fixtable", 2, &mut ctx, &[name, on])
        .expect("ets:safe_fixtable/2 by name");
    assert!(is_true(&atoms, out));
    // ...and reading the name back shows the name-atom path found the same table.
    let id_item = ctx.atom("id").expect("seeded");
    let id = bifs
        .call("ets", "info", 2, &mut ctx, &[named, id_item])
        .expect("ets:info/2");
    let tid = bifs
        .call("ets", "whereis", 1, &mut ctx, &[name])
        .expect("ets:whereis/1");
    assert!(beam_term::compare::eq(tid, id, &atoms));

    // Any non-boolean second argument is `badarg` (the oracle compares by atom
    // identity).
    let bad = ctx.atom("z").expect("seeded");
    let one = ctx.int(1).expect("fits");
    let nil = ctx.list(&[]).expect("fits");
    for flag in [bad, one, nil] {
        let err = bifs
            .call("ets", "safe_fixtable", 2, &mut ctx, &[handle, flag])
            .expect_err("non-boolean");
        assert!(is_badarg(&err), "{err:?}");
    }

    // A deleted handle, and the name field of an unregistered table, are `badarg`.
    let justname = ctx.atom("justname").expect("seeded");
    let gone = {
        // A handle of a deleted table.
        let temp = new_table_ok(&bifs, &mut ctx, "my_tab", &["set"]);
        bifs.call("ets", "delete", 1, &mut ctx, &[temp])
            .expect("ets:delete/1");
        temp
    };
    for table in [gone, justname] {
        let err = bifs
            .call("ets", "safe_fixtable", 2, &mut ctx, &[table, on])
            .expect_err("not a table");
        assert!(is_badarg(&err), "{err:?}");
    }
}

// ------------------------------------------------------- update_element /4

/// The oracle:
/// ```text
/// 1> T = ets:new(t,[set]), ets:update_element(T, k, {2,99}, {k,0}).
/// true
/// 2> ets:lookup(T, k).
/// [{k,99}]
/// 3> ets:update_element(T, k, [], {ignored,5}), ets:lookup(T, k).
/// [{k,99}]                       % the default is not used for a present key
/// 4> ets:update_element(T, k, {2,9}, {a,b,c}), ets:lookup(T, k).
/// [{k,9}]
/// ```
#[test]
fn update_element_4_deposits_the_default_with_its_key_replaced() {
    let bifs = default_registry();
    let mut heap = Heap::new(65536).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);

    let handle = new_table_ok(&bifs, &mut ctx, "my_tab", &["set"]);
    let key = ctx.atom("k").expect("seeded");
    let spec = ctx
        .tuple(&[
            ctx.int(2).expect("fits"),
            ctx.int(99).expect("fits"),
        ])
        .expect("fits");
    let default = pair(&mut ctx, "k", 0);

    let out = bifs
        .call("ets", "update_element", 4, &mut ctx, &[handle, key, spec, default])
        .expect("ets:update_element/4");
    assert!(is_true(&atoms, out));
    // The default's key slot was replaced by `k`, its element 2 by the spec.
    let stored = found(&bifs, &mut ctx, handle, "k");
    assert!(beam_term::compare::eq(
        element(&bifs, &mut ctx, 2, stored),
        ctx.int(99).expect("fits"),
        &atoms
    ));

    // A present key ignores the default's contents (it is only shape-checked): an
    // empty spec updates nothing, and a 3-arity default is still accepted.
    let empty = ctx.list(&[]).expect("fits");
    let ignored = pair(&mut ctx, "ignored", 5);
    let out = bifs
        .call("ets", "update_element", 4, &mut ctx, &[handle, key, empty, ignored])
        .expect("ets:update_element/4");
    assert!(is_true(&atoms, out));
    let wide = {
        let a = ctx.atom("a").expect("seeded");
        let b = ctx.atom("b").expect("seeded");
        let c = ctx.atom("c").expect("seeded");
        ctx.tuple(&[a, b, c]).expect("fits")
    };
    let two9 = ctx
        .tuple(&[ctx.int(2).expect("fits"), ctx.int(9).expect("fits")])
        .expect("fits");
    let out = bifs
        .call("ets", "update_element", 4, &mut ctx, &[handle, key, two9, wide])
        .expect("ets:update_element/4");
    assert!(is_true(&atoms, out));
    let stored = found(&bifs, &mut ctx, handle, "k");
    assert!(beam_term::compare::eq(
        element(&bifs, &mut ctx, 2, stored),
        ctx.int(9).expect("fits"),
        &atoms
    ));
}

/// The oracle:
/// ```text
/// 1> T = ets:new(t,[set]),
///    {ets:update_element(T,k,{3,9},{k,0}), ets:lookup(T,k)}.
/// {badarg,[]}                     % pos > default arity: nothing deposited
/// 2> ets:update_element(T,k,{2,9},notatuple).   ets:update_element(T,k,{2,9},{}).
/// {badarg} {badarg}
/// 3> ets:update_element(T,k,{2,9},{a,b,c}), ets:lookup(T,k).
/// {true,[{k,9,c}]}                % arity 3 default: DEPOSITED (probe 1 aborted first)
/// 4> ets:insert(T,{k,1}), ets:update_element(T,k,{2,9},notatuple).
/// {badarg}                        % an invalid default fails even for a present key
/// 5> ets:update_element(T,k,{2,9},{zzz,7}), ets:lookup(T,k).
/// {true,[{k,9}]}
/// ```
#[test]
fn update_element_4_validates_the_default_upfront_and_the_spec_afterwards() {
    let bifs = default_registry();
    let mut heap = Heap::new(65536).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);

    let handle = new_table_ok(&bifs, &mut ctx, "my_tab", &["set"]);
    let key = ctx.atom("k").expect("seeded");
    let default = pair(&mut ctx, "k", 0);
    let some = |ctx: &mut BifContext<'_>, pos: i64, value: i64| {
        ctx.tuple(&[ctx.int(pos).expect("fits"), ctx.int(value).expect("fits")])
            .expect("fits")
    };

    // `pos` beyond the default's arity: `badarg` with nothing deposited.
    let three = some(&mut ctx, 3, 9);
    let err = bifs
        .call("ets", "update_element", 4, &mut ctx, &[handle, key, three, default])
        .expect_err("pos > arity");
    assert!(is_badarg(&err), "{err:?}");
    let stored = lookup(&bifs, &mut ctx, handle, "k");
    assert!(beam_term::compare::eq(
        stored,
        ctx.list(&[]).expect("fits"),
        &atoms
    ));

    // An invalid default is `badarg` — a non-tuple, an arity-0 tuple, and (below) an
    // arity-0..keypos-1 tuple.
    let two9 = some(&mut ctx, 2, 9);
    let not_a_tuple = ctx.int(5).expect("fits");
    let empty_tuple = ctx.tuple(&[]).expect("fits");
    for bad_default in [not_a_tuple, empty_tuple] {
        let err = bifs
            .call(
                "ets",
                "update_element",
                4,
                &mut ctx,
                &[handle, key, two9, bad_default],
            )
            .expect_err("bad default");
        assert!(is_badarg(&err), "{err:?}");
    }

    // A 3-arity default is accepted and deposited with the key at `keypos`.
    let wide = {
        let a = ctx.atom("a").expect("seeded");
        let b = ctx.atom("b").expect("seeded");
        let c = ctx.atom("c").expect("seeded");
        ctx.tuple(&[a, b, c]).expect("fits")
    };
    let out = bifs
        .call("ets", "update_element", 4, &mut ctx, &[handle, key, two9, wide])
        .expect("ets:update_element/4");
    assert!(is_true(&atoms, out));
    let stored = found(&bifs, &mut ctx, handle, "k");
    assert!(beam_term::compare::eq(
        element(&bifs, &mut ctx, 2, stored),
        ctx.int(9).expect("fits"),
        &atoms
    ));
    assert!(beam_term::compare::eq(
        element(&bifs, &mut ctx, 3, stored),
        ctx.atom("c").expect("seeded"),
        &atoms
    ));

    // An invalid default fails even when the key exists (nothing is written)...
    let err = bifs
        .call(
            "ets",
            "update_element",
            4,
            &mut ctx,
            &[handle, key, two9, not_a_tuple],
        )
        .expect_err("bad default, present key");
    assert!(is_badarg(&err), "{err:?}");
    // ...while a *valid* unused default does not stop the update.
    let other = pair(&mut ctx, "z", 7);
    let out = bifs
        .call("ets", "update_element", 4, &mut ctx, &[handle, key, two9, other])
        .expect("ets:update_element/4");
    assert!(is_true(&atoms, out));

    // The spec is validated against the *object's* arity: `pos` 0, `pos` beyond it
    // and a `pos` naming the key element are all `badarg`.
    let zero = some(&mut ctx, 0, 9);
    let beyond = some(&mut ctx, 4, 9);
    let key_elem = some(&mut ctx, 1, 9);
    for bad_spec in [zero, beyond, key_elem] {
        let err = bifs
            .call("ets", "update_element", 4, &mut ctx, &[handle, key, bad_spec, other])
            .expect_err("bad position");
        assert!(is_badarg(&err), "{err:?}");
    }

    // Malformed spec shapes: a bare term, an improper list, a 3-tuple/1-tuple
    // entry, a non-tuple element, a non-integer position.
    let a = ctx.atom("a").expect("seeded");
    let improper = ctx.cons(two9, a).expect("fits");
    let three_tuple = ctx
        .tuple(&[
            ctx.int(2).expect("fits"),
            ctx.int(3).expect("fits"),
            ctx.int(4).expect("fits"),
        ])
        .expect("fits");
    let one_tuple = ctx.tuple(&[ctx.int(2).expect("fits")]).expect("fits");
    let atom_pos = ctx
        .tuple(&[a, ctx.int(3).expect("fits")])
        .expect("fits");
    for bad_spec in [
        a,
        improper,
        ctx.list(&[three_tuple]).expect("fits"),
        ctx.list(&[one_tuple]).expect("fits"),
        ctx.list(&[a]).expect("fits"),
        ctx.list(&[atom_pos]).expect("fits"),
    ] {
        let err = bifs
            .call("ets", "update_element", 4, &mut ctx, &[handle, key, bad_spec, other])
            .expect_err("malformed spec");
        assert!(is_badarg(&err), "{err:?}");
    }
}

/// The oracle:
/// ```text
/// 1> T = ets:new(t,[set]), ets:update_element(T,k,[{2,3},{2,4}]), ets:lookup(T,k).
/// {true,[{k,4}]}                  % a repeated position: the last entry wins
/// 2> B = ets:new(b,[bag]), ets:update_element(B,k,{2,9},{k,0}).
/// ** exception error: bad argument
/// 3> O = ets:new(o,[ordered_set]), ets:update_element(O,k,{2,9},{k,0}).
/// {true,[{k,9}]}
/// 4> K = ets:new(k2,[set,{keypos,2}]), ets:update_element(K,k,{1,9},{x,k}).
/// {true,[{9,k}]}                  % `keypos` 2: the deposited key lands in slot 2
/// 5> ets:update_element(K,k,{2,9},{x,k}).
/// ** exception error: bad argument   % `Pos == keypos`
/// ```
#[test]
fn update_element_4_covers_the_kinds_keypos_and_a_repeated_position() {
    let bifs = default_registry();
    let mut heap = Heap::new(65536).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);

    let key = ctx.atom("k").expect("seeded");
    let default = pair(&mut ctx, "k", 0);

    // A repeated position: the later entry wins.
    let handle = new_table_ok(&bifs, &mut ctx, "my_tab", &["set"]);
    let first = slot(&mut ctx, 2, 3);
    let second = slot(&mut ctx, 2, 4);
    let repeated = ctx.list(&[first, second]).expect("fits");
    let out = bifs
        .call(
            "ets",
            "update_element",
            4,
            &mut ctx,
            &[handle, key, repeated, default],
        )
        .expect("ets:update_element/4");
    assert!(is_true(&atoms, out));
    let stored = found(&bifs, &mut ctx, handle, "k");
    assert!(beam_term::compare::eq(
        element(&bifs, &mut ctx, 2, stored),
        ctx.int(4).expect("fits"),
        &atoms
    ));

    // `bag`/`duplicate_bag` have no updatable objects.
    let two9 = ctx
        .tuple(&[ctx.int(2).expect("fits"), ctx.int(9).expect("fits")])
        .expect("fits");
    for kind in ["bag", "duplicate_bag"] {
        let bag = new_table_ok(&bifs, &mut ctx, "my_tab", &[kind]);
        let err = bifs
            .call("ets", "update_element", 4, &mut ctx, &[bag, key, two9, default])
            .expect_err("multi kind");
        assert!(is_badarg(&err), "{kind}: {err:?}");
    }

    // An `ordered_set` takes the default path too.
    let ordered = new_table_ok(&bifs, &mut ctx, "my_tab", &["ordered_set"]);
    let out = bifs
        .call(
            "ets",
            "update_element",
            4,
            &mut ctx,
            &[ordered, key, two9, default],
        )
        .expect("ets:update_element/4");
    assert!(is_true(&atoms, out));
    assert!(beam_term::compare::eq(
        found(&bifs, &mut ctx, ordered, "k"),
        pair(&mut ctx, "k", 9),
        &atoms
    ));

    // `{keypos, 2}`: the deposited object is the default with `k` in slot 2, and
    // updating the key element (slot 2) is `badarg`.
    let keypos2 = {
        let name = ctx.atom("my_tab").expect("seeded");
        let kind = ctx.atom("set").expect("seeded");
        let kp = ctx
            .tuple(&[ctx.atom("keypos").expect("seeded"), ctx.int(2).expect("fits")])
            .expect("fits");
        let opts = ctx.list(&[kind, kp]).expect("fits");
        bifs.call("ets", "new", 2, &mut ctx, &[name, opts])
            .expect("ets:new")
    };
    let x = ctx.atom("x").expect("seeded");
    let default_kp2 = ctx.tuple(&[x, key]).expect("fits");
    let slot1 = ctx
        .tuple(&[ctx.int(1).expect("fits"), ctx.int(9).expect("fits")])
        .expect("fits");
    let out = bifs
        .call(
            "ets",
            "update_element",
            4,
            &mut ctx,
            &[keypos2, key, slot1, default_kp2],
        )
        .expect("ets:update_element/4 keypos 2");
    assert!(is_true(&atoms, out));
    let stored = found(&bifs, &mut ctx, keypos2, "k");
    assert!(beam_term::compare::eq(
        stored,
        ctx.tuple(&[ctx.int(9).expect("fits"), key]).expect("fits"),
        &atoms
    ));
    let slot2 = ctx
        .tuple(&[ctx.int(2).expect("fits"), ctx.int(9).expect("fits")])
        .expect("fits");
    let err = bifs
        .call(
            "ets",
            "update_element",
            4,
            &mut ctx,
            &[keypos2, key, slot2, default_kp2],
        )
        .expect_err("pos == keypos");
    assert!(is_badarg(&err), "{err:?}");
}

/// The un-defaulted `/3` form is unchanged: an absent key is `false` (the default
/// deposit is `/4`'s), and a `named_table`'s name atom works for the new `/4`.
#[test]
fn update_element_3_is_unchanged_and_the_name_atom_reaches_the_slash_4_form() {
    let bifs = default_registry();
    let mut heap = Heap::new(65536).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);

    let handle = new_table_ok(&bifs, &mut ctx, "my_tab", &["set"]);
    let key = ctx.atom("k").expect("seeded");
    let two9 = ctx
        .tuple(&[ctx.int(2).expect("fits"), ctx.int(9).expect("fits")])
        .expect("fits");
    let out = bifs
        .call("ets", "update_element", 3, &mut ctx, &[handle, key, two9])
        .expect("ets:update_element/3");
    assert!(is_false(&atoms, out));
    let stored = lookup(&bifs, &mut ctx, handle, "k");
    assert!(beam_term::compare::eq(
        stored,
        ctx.list(&[]).expect("fits"),
        &atoms
    ));

    // The `named_table` name atom is a table argument of `/4` (the oracle accepts
    // it), and the table it names is the registered one.
    let named = new_table_ok(&bifs, &mut ctx, "ntab", &["named_table", "set"]);
    let name = ctx.atom("ntab").expect("seeded");
    let default = pair(&mut ctx, "k", 0);
    let out = bifs
        .call(
            "ets",
            "update_element",
            4,
            &mut ctx,
            &[name, key, two9, default],
        )
        .expect("ets:update_element/4 by name");
    assert!(is_true(&atoms, out));
    let stored = found(&bifs, &mut ctx, named, "k");
    assert!(beam_term::compare::eq(stored, pair(&mut ctx, "k", 9), &atoms));
}

// ------------------------------------------------------------ is_compiled_ms

/// The oracle:
/// ```text
/// 1> ets:is_compiled_ms(ets:match_spec_compile([{'_',[],[true]}])).
/// true
/// 2> ets:is_compiled_ms([{'_',[],[true]}]).
/// false
/// 3> T = ets:new(t,[set]), ets:is_compiled_ms(T).      ets:is_compiled_ms(42).
/// false                                                 false
/// 4> ets:is_compiled_ms(x).                            ets:is_compiled_ms(make_ref()).
/// false                                                 false
/// ```
#[test]
fn is_compiled_ms_recognizes_only_a_compiled_spec() {
    let bifs = default_registry();
    let mut heap = Heap::new(65536).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);

    let spec = whole_object_spec(&mut ctx);
    let compiled = bifs
        .call("ets", "match_spec_compile", 1, &mut ctx, &[spec])
        .expect("ets:match_spec_compile/1");
    let out = bifs
        .call("ets", "is_compiled_ms", 1, &mut ctx, &[compiled])
        .expect("ets:is_compiled_ms/1");
    assert!(is_true(&atoms, out));

    // Everything else is `false`: the raw spec list, a table handle (a reference),
    // a *continuation* reference, an atom and an integer.
    let handle = new_table_ok(&bifs, &mut ctx, "my_tab", &["set"]);
    let object = pair(&mut ctx, "a", 1);
    bifs.call("ets", "insert", 2, &mut ctx, &[handle, object])
        .expect("ets:insert/2");
    let object = pair(&mut ctx, "b", 2);
    bifs.call("ets", "insert", 2, &mut ctx, &[handle, object])
        .expect("ets:insert/2");
    let spec = whole_object_spec(&mut ctx);
    let one = ctx.int(1).expect("fits");
    let chunk = bifs
        .call("ets", "select", 3, &mut ctx, &[handle, spec, one])
        .expect("ets:select/3");
    let continuation = element(&bifs, &mut ctx, 2, chunk);
    let raw = whole_object_spec(&mut ctx);
    for term in [
        raw,
        handle,
        continuation,
        ctx.atom("x").expect("seeded"),
        one,
    ] {
        let out = bifs
            .call("ets", "is_compiled_ms", 1, &mut ctx, &[term])
            .expect("ets:is_compiled_ms/1");
        assert!(is_false(&atoms, out));
    }
}

// ------------------------------------------------------------ missing registry

/// Every verb added here reports the module's typed `unsupported_bif` raise when no
/// [`TableRegistry`] is installed (the same contract the pre-existing keys have); a
/// non-reference argument of `is_compiled_ms/1` is answered `false` without one,
/// because it cannot be a compiled spec.
#[test]
fn the_new_verbs_require_a_registry() {
    let bifs = default_registry();
    let mut heap = Heap::new(65536).expect("test heap");
    let atoms = seeded_atoms();

    // A handle from a *previous* registry (dropped): decoding it needs the registry.
    let handle = {
        let mut tables = TableRegistry::new();
        let mut with_tables =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);
        new_table_ok(&bifs, &mut with_tables, "my_tab", &["set"])
    };
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    let key = ctx.atom("k").expect("seeded");
    let on = ctx.atom("true").expect("seeded");
    let name = ctx.atom("ntab").expect("seeded");
    let default = pair(&mut ctx, "k", 0);
    for (bif, args) in [
        ("safe_fixtable", vec![handle, on]),
        ("whereis", vec![name]),
        ("update_element", vec![handle, key, default, default]),
        ("is_compiled_ms", vec![handle]),
    ] {
        let err = bifs
            .call("ets", bif, args.len() as u32, &mut ctx, &args)
            .expect_err("no registry installed");
        assert!(
            is_raise_atom(&err, "unsupported_bif"),
            "ets:{bif}: {err:?}"
        );
    }

    // A non-reference is not a compiled spec, so it needs no registry at all.
    let seven = ctx.int(7).expect("fits");
    let out = bifs
        .call("ets", "is_compiled_ms", 1, &mut ctx, &[seven])
        .expect("ets:is_compiled_ms/1");
    assert!(is_false(&atoms, out));
}
