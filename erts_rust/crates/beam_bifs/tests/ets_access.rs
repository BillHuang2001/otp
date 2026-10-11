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

//! Integration tests pinning the `ets` **owner / access-control** model, driven
//! through the public [`BifRegistry::call`] over a real [`TableRegistry`].
//!
//! `ets:new/2` records the **calling** process as the table's owner
//! (`crate::ets::resolve::caller_owner`) and every table-taking BIF enforces the
//! oracle's access matrix as the calling process. This suite builds two
//! [`BifContext`]s over the *same* [`TableRegistry`] — an **owner**
//! (`ProcessId::new(1, 0)`, the context each table is created in) and a
//! **non-owner** (`ProcessId::new(1, 1)`) — and runs every gated operation from
//! both, under each of `public` / `protected` / `private`.
//!
//! # The model
//!
//! | requester | `public` | `protected` | `private` |
//! |---|---|---|---|
//! | owner | read + write | read + write | read + write |
//! | non-owner | read + write | **read only** | **neither** |
//!
//! A denial is `badarg` (`beam_ets::Table::check_read`/`check_write`, surfaced by
//! `crate::ets::take_table_gated`). `ets:info/1,2` is **not** gated at all
//! (`crate::ets::take_table`), so a non-owner may introspect a `private` table.
//!
//! * **read-gated:** `lookup/2`, `lookup_element/3,4`, `member/2`, `tab2list/1`,
//!   `first/1`, `last/1`, `next/2`, `prev/2`, `safe_fixtable/2`, `select/1,2,3`,
//!   `match/1,2,3`, `match_object/1,2,3`, `select_count/2` and the `/1`
//!   continuation resumes;
//! * **write-gated:** `insert/2`, `insert_new/2`, `delete/1,2`, `delete_object/2`,
//!   `delete_all_objects/1`, `take/2`, `update_counter/3,4`, `update_element/3,4`,
//!   `select_delete/2`, `select_replace/2`, `match_delete/2`;
//! * **ungated:** `info/1,2`.
//!
//! # The oracle probe
//!
//! Every row of the matrix (and the `owner`/`heir` `info` facts) was verified
//! against the system `erl` (OTP 29.1.1 / erts-17.1). The probe creates each table
//! in the **owner** process (`self()`) and runs the operation in a spawned
//! **non-owner** whose result is mailed back:
//!
//! ```erlang
//! run_as(F) ->
//!     Owner = self(), Ref = make_ref(),
//!     spawn(fun() -> Owner ! {Ref, try _ = F(), ok
//!                                  catch error:badarg -> badarg end} end),
//!     receive {Ref, R} -> R end.
//! newtab(Prot) ->
//!     T = ets:new(t, [ordered_set, Prot]),
//!     ets:insert(T, {1,10}), ets:insert(T, {2,20}), T.
//! ```
//!
//! with (one call per op, e.g.)
//!
//! ```erlang
//! T = newtab(protected), run_as(fun() -> ets:lookup(T, 1) end).
//! ```
//!
//! The observed matrix (owner always `ok`; the non-owner row below) was:
//!
//! ```text
//! public    reads:  all ok      writes: all ok
//! protected reads:  all ok      writes: all badarg
//! private   reads:  all badarg  writes: all badarg
//! continuations (select/1, match/1, match_object/1):
//! public ok, protected ok, private badarg   (owner: ok everywhere)
//! info/1 and info/2: a non-owner of a private table -> ok
//! ```
//!
//! The `info` facts were probed with:
//!
//! ```erlang
//! T = ets:new(t, [set]),              ets:info(T, owner) =:= self().  %% true
//! T1 = ets:new(t1, [set]),            ets:info(T1, heir).             %% none
//! H = spawn(fun() -> ok end),
//! T2 = ets:new(t2, [set, {heir, H}]), ets:info(T2, heir) =:= H.       %% true
//! T3 = ets:new(t3, [set, {heir, none}]), ets:info(T3, heir).          %% none
//! ```

use std::process::Command;

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry, TableRegistry};
use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::{atom_val, is_internal_pid, AtomTable, Eterm};

/// The pid every table in these tests is created by.
const OWNER: ProcessId = ProcessId::new(1, 0);
/// A pid that is never a table's owner.
const NON_OWNER: ProcessId = ProcessId::new(1, 1);
/// The pid installed as a heir where the tests need one.
const HEIR: ProcessId = ProcessId::new(1, 2);

/// The atom names the `ets` BIFs and the term builders here need.
///
/// `ets:info/1` renders all 15 field names through `BifContext::intern_atom`,
/// which needs each name already interned (no `AtomInterner` hook is installed),
/// so the whole field-name set is seeded.
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed([
        // option atoms
        "set",
        "ordered_set",
        "public",
        "protected",
        "private",
        "named_table",
        "heir",
        "none",
        // the 15 `ets:info/1` field names
        "id",
        "decentralized_counters",
        "read_concurrency",
        "write_concurrency",
        "compressed",
        "memory",
        "owner",
        "name",
        "size",
        "node",
        "type",
        "keypos",
        "protection",
        // values / navigation
        "true",
        "false",
        "undefined",
        "nonode@nohost",
        "$end_of_table",
        // test data + match-spec names
        "t",
        "a",
        "b",
        "c",
        "m",
        "def",
        "_",
        "$_",
        "$1",
        "$2",
    ]);
    atoms
}

// ------------------------------------------------------------- term builders

/// Runs `f` over a builder [`BifContext`] (heap + atoms; no table registry
/// needed to construct argument terms).
fn build<T>(heap: &mut Heap, atoms: &AtomTable, f: impl FnOnce(&mut BifContext<'_>) -> T) -> T {
    let mut ctx = BifContext::new(heap, atoms, OWNER);
    f(&mut ctx)
}

/// Dispatches `module:function/arity` as `pid` over `tables`, returning the BIF
/// result (so a denial surfaces as `Err(BeamError::BadArg)`).
fn call(
    reg: &BifRegistry,
    heap: &mut Heap,
    atoms: &AtomTable,
    tables: &mut TableRegistry,
    pid: ProcessId,
    module: &str,
    function: &str,
    arity: u32,
    args: &[Eterm],
) -> Result<Eterm> {
    let mut ctx = BifContext::new(heap, atoms, pid).with_tables(tables);
    reg.call(module, function, arity, &mut ctx, args)
}

/// The guaranteed-interned atom `name`.
fn atom(ctx: &mut BifContext<'_>, name: &str) -> Eterm {
    ctx.atom(name).expect("the name is seeded in `seeded_atoms`")
}

/// The small integer `n`.
fn small(ctx: &mut BifContext<'_>, n: i64) -> Eterm {
    ctx.int(n).expect("fits a small integer")
}

/// The `ets` object `{key, value}`.
fn object(ctx: &mut BifContext<'_>, key: &str, value: i64) -> Eterm {
    let k = atom(ctx, key);
    let v = small(ctx, value);
    ctx.tuple(&[k, v]).expect("fits")
}

/// The `{'_','_'}` tuple — `ets:match`/`ets:match_object`'s wildcard pattern.
fn wildcard_object(ctx: &mut BifContext<'_>) -> Eterm {
    let u = atom(ctx, "_");
    ctx.tuple(&[u, u]).expect("fits")
}

/// `[{{'_','_'}, [], ['$_']}]` — select every object, whole.
fn whole_spec(ctx: &mut BifContext<'_>) -> Eterm {
    let head = wildcard_object(ctx);
    let guards = ctx.list(&[]).expect("fits");
    let whole = atom(ctx, "$_");
    let body = ctx.list(&[whole]).expect("fits");
    let clause = ctx.tuple(&[head, guards, body]).expect("fits");
    ctx.list(&[clause]).expect("fits")
}

/// `[{{'_','_'}, [], [true]}]` — count/delete every object.
fn true_spec(ctx: &mut BifContext<'_>) -> Eterm {
    let head = wildcard_object(ctx);
    let guards = ctx.list(&[]).expect("fits");
    let truth = atom(ctx, "true");
    let body = ctx.list(&[truth]).expect("fits");
    let clause = ctx.tuple(&[head, guards, body]).expect("fits");
    ctx.list(&[clause]).expect("fits")
}

/// `[{{'$1','$2'}, [], [{{'$1', b}}]}]` — replace every 2-tuple with `{Key, b}`.
fn replace_spec(ctx: &mut BifContext<'_>) -> Eterm {
    let v1 = atom(ctx, "$1");
    let v2 = atom(ctx, "$2");
    let b = atom(ctx, "b");
    let head = ctx.tuple(&[v1, v2]).expect("fits");
    let guards = ctx.list(&[]).expect("fits");
    let constructed = ctx.tuple(&[v1, b]).expect("fits");
    let body_item = ctx.tuple(&[constructed]).expect("fits");
    let body = ctx.list(&[body_item]).expect("fits");
    let clause = ctx.tuple(&[head, guards, body]).expect("fits");
    ctx.list(&[clause]).expect("fits")
}

// ------------------------------------------------------- the gated operations

/// The access an `ets:*` operation requires of its caller.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Gate {
    /// Allowed for any process on `public`/`protected`, the owner only on `private`.
    Read,
    /// Allowed for any process on `public`, the owner only on `protected`/`private`.
    Write,
}

/// One gated `ets:*` operation and the argument builder for it (the builder
/// receives the table's handle and returns the BIF argument list).
struct Op {
    function: &'static str,
    arity: u32,
    /// The gate this operation must enforce (self-checked by
    /// [`the_op_tables_classify_every_gate`]).
    gate: Gate,
    args: ArgsFn,
}

/// Builds a BIF argument list from the table handle.
type ArgsFn = fn(&mut BifContext<'_>, Eterm) -> Vec<Eterm>;

/// `[Tab]`.
fn args_handle(_ctx: &mut BifContext<'_>, h: Eterm) -> Vec<Eterm> {
    vec![h]
}

/// `[Tab, Key]` (key `a`, which [`seed`] inserts).
fn args_key(ctx: &mut BifContext<'_>, h: Eterm) -> Vec<Eterm> {
    vec![h, atom(ctx, "a")]
}

/// `[Tab, Key, Pos]`.
fn args_key_pos(ctx: &mut BifContext<'_>, h: Eterm) -> Vec<Eterm> {
    let (key, pos) = (atom(ctx, "a"), small(ctx, 2));
    vec![h, key, pos]
}

/// `[Tab, Key, Pos, Default]`.
fn args_key_pos_default(ctx: &mut BifContext<'_>, h: Eterm) -> Vec<Eterm> {
    let (key, pos, default) = (atom(ctx, "a"), small(ctx, 2), small(ctx, 99));
    vec![h, key, pos, default]
}

/// `[Tab, true]` — `ets:safe_fixtable/2`.
fn args_true(ctx: &mut BifContext<'_>, h: Eterm) -> Vec<Eterm> {
    vec![h, atom(ctx, "true")]
}

/// `[Tab, Spec]` — the whole-object select spec.
fn args_whole_spec(ctx: &mut BifContext<'_>, h: Eterm) -> Vec<Eterm> {
    vec![h, whole_spec(ctx)]
}

/// `[Tab, Spec, 1]`.
fn args_whole_spec_limit(ctx: &mut BifContext<'_>, h: Eterm) -> Vec<Eterm> {
    let (spec, limit) = (whole_spec(ctx), small(ctx, 1));
    vec![h, spec, limit]
}

/// `[Tab, Pattern]` — the `{'_','_'}` wildcard pattern.
fn args_wildcard(ctx: &mut BifContext<'_>, h: Eterm) -> Vec<Eterm> {
    vec![h, wildcard_object(ctx)]
}

/// `[Tab, Pattern, 1]`.
fn args_wildcard_limit(ctx: &mut BifContext<'_>, h: Eterm) -> Vec<Eterm> {
    let (pattern, limit) = (wildcard_object(ctx), small(ctx, 1));
    vec![h, pattern, limit]
}

/// `[Tab, Spec]` — the count/delete-everything spec.
fn args_true_spec(ctx: &mut BifContext<'_>, h: Eterm) -> Vec<Eterm> {
    vec![h, true_spec(ctx)]
}

/// `[Tab, Spec]` — the replace-every-object spec.
fn args_replace_spec(ctx: &mut BifContext<'_>, h: Eterm) -> Vec<Eterm> {
    vec![h, replace_spec(ctx)]
}

/// `[Tab, {a,1}]`.
fn args_object_a1(ctx: &mut BifContext<'_>, h: Eterm) -> Vec<Eterm> {
    let obj = object(ctx, "a", 1);
    vec![h, obj]
}

/// `[Tab, {c,1}]`.
fn args_object_c1(ctx: &mut BifContext<'_>, h: Eterm) -> Vec<Eterm> {
    let obj = object(ctx, "c", 1);
    vec![h, obj]
}

/// `[Tab, Key, Incr]` — `ets:update_counter/3`.
fn args_incr(ctx: &mut BifContext<'_>, h: Eterm) -> Vec<Eterm> {
    let (key, incr) = (atom(ctx, "a"), small(ctx, 1));
    vec![h, key, incr]
}

/// `[Tab, Key, Incr, Default]` — `ets:update_counter/4` on an absent key `m`.
fn args_incr_default(ctx: &mut BifContext<'_>, h: Eterm) -> Vec<Eterm> {
    let key = atom(ctx, "m");
    let (incr, default) = (small(ctx, 1), object(ctx, "m", 0));
    vec![h, key, incr, default]
}

/// `[Tab, Key, {Pos, Value}]` — `ets:update_element/3`.
fn args_update_element(ctx: &mut BifContext<'_>, h: Eterm) -> Vec<Eterm> {
    let (key, spec) = (atom(ctx, "a"), pos_value(ctx));
    vec![h, key, spec]
}

/// `[Tab, Key, [{Pos, Value}], Default]` — `ets:update_element/4` on an absent
/// key `m`.
fn args_update_element_default(ctx: &mut BifContext<'_>, h: Eterm) -> Vec<Eterm> {
    let key = atom(ctx, "m");
    let (spec, default) = (pos_value(ctx), object(ctx, "m", 0));
    let updates = ctx.list(&[spec]).expect("fits");
    vec![h, key, updates, default]
}

/// The `{Pos, Value}` element-update tuple `{2, 42}`.
fn pos_value(ctx: &mut BifContext<'_>) -> Eterm {
    let (pos, value) = (small(ctx, 2), small(ctx, 42));
    ctx.tuple(&[pos, value]).expect("fits")
}

/// The read-gated operations (see the module docs).
fn read_ops() -> Vec<Op> {
    vec![
        Op { function: "lookup", arity: 2, gate: Gate::Read, args: args_key },
        Op { function: "lookup_element", arity: 3, gate: Gate::Read, args: args_key_pos },
        Op { function: "lookup_element", arity: 4, gate: Gate::Read, args: args_key_pos_default },
        Op { function: "member", arity: 2, gate: Gate::Read, args: args_key },
        Op { function: "tab2list", arity: 1, gate: Gate::Read, args: args_handle },
        Op { function: "first", arity: 1, gate: Gate::Read, args: args_handle },
        Op { function: "last", arity: 1, gate: Gate::Read, args: args_handle },
        Op { function: "next", arity: 2, gate: Gate::Read, args: args_key },
        Op { function: "prev", arity: 2, gate: Gate::Read, args: args_key },
        Op { function: "safe_fixtable", arity: 2, gate: Gate::Read, args: args_true },
        Op { function: "select", arity: 2, gate: Gate::Read, args: args_whole_spec },
        Op { function: "select", arity: 3, gate: Gate::Read, args: args_whole_spec_limit },
        Op { function: "match", arity: 2, gate: Gate::Read, args: args_wildcard },
        Op { function: "match", arity: 3, gate: Gate::Read, args: args_wildcard_limit },
        Op { function: "match_object", arity: 2, gate: Gate::Read, args: args_wildcard },
        Op { function: "match_object", arity: 3, gate: Gate::Read, args: args_wildcard_limit },
        Op { function: "select_count", arity: 2, gate: Gate::Read, args: args_true_spec },
    ]
}

/// The write-gated operations (see the module docs).
fn write_ops() -> Vec<Op> {
    vec![
        Op { function: "insert", arity: 2, gate: Gate::Write, args: args_object_c1 },
        Op { function: "insert_new", arity: 2, gate: Gate::Write, args: args_object_a1 },
        Op { function: "delete", arity: 1, gate: Gate::Write, args: args_handle },
        Op { function: "delete", arity: 2, gate: Gate::Write, args: args_key },
        Op { function: "delete_object", arity: 2, gate: Gate::Write, args: args_object_a1 },
        Op { function: "delete_all_objects", arity: 1, gate: Gate::Write, args: args_handle },
        Op { function: "take", arity: 2, gate: Gate::Write, args: args_key },
        Op { function: "update_counter", arity: 3, gate: Gate::Write, args: args_incr },
        Op { function: "update_counter", arity: 4, gate: Gate::Write, args: args_incr_default },
        Op { function: "update_element", arity: 3, gate: Gate::Write, args: args_update_element },
        Op { function: "update_element", arity: 4, gate: Gate::Write, args: args_update_element_default },
        Op { function: "select_delete", arity: 2, gate: Gate::Write, args: args_true_spec },
        Op { function: "select_replace", arity: 2, gate: Gate::Write, args: args_replace_spec },
        Op { function: "match_delete", arity: 2, gate: Gate::Write, args: args_wildcard },
    ]
}

// ------------------------------------------------------------- table helpers

/// `ets:new(t, [set, Protection])` **in the owner context**, returning the handle.
fn new_protected_table(
    reg: &BifRegistry,
    heap: &mut Heap,
    atoms: &AtomTable,
    tables: &mut TableRegistry,
    protection: &str,
) -> Eterm {
    let args = build(heap, atoms, |ctx| {
        let name = atom(ctx, "t");
        let (kind, prot) = (atom(ctx, "set"), atom(ctx, protection));
        let opts = ctx.list(&[kind, prot]).expect("fits");
        vec![name, opts]
    });
    call(reg, heap, atoms, tables, OWNER, "ets", "new", 2, &args).expect("ets:new/2")
}

/// `ets:new(t, [set, {heir, Heir}])` **in the owner context**, returning the
/// handle.
fn new_table_with_heir(
    reg: &BifRegistry,
    heap: &mut Heap,
    atoms: &AtomTable,
    tables: &mut TableRegistry,
    heir: Eterm,
) -> Eterm {
    let args = build(heap, atoms, |ctx| {
        let name = atom(ctx, "t");
        let kind = atom(ctx, "set");
        let tag = atom(ctx, "heir");
        let opt = ctx.tuple(&[tag, heir]).expect("fits");
        let opts = ctx.list(&[kind, opt]).expect("fits");
        vec![name, opts]
    });
    call(reg, heap, atoms, tables, OWNER, "ets", "new", 2, &args).expect("ets:new/2")
}

/// Seeds `{a,1}` and `{b,2}` into `handle` (as the owner — `insert/2` is
/// write-gated, and the owner is always allowed). Two objects are needed so a
/// `select|match|match_object/3` with `limit` 1 yields a continuation.
fn seed(
    reg: &BifRegistry,
    heap: &mut Heap,
    atoms: &AtomTable,
    tables: &mut TableRegistry,
    handle: Eterm,
) {
    for (key, value) in [("a", 1), ("b", 2)] {
        let args = build(heap, atoms, |ctx| {
            let obj = object(ctx, key, value);
            vec![handle, obj]
        });
        call(reg, heap, atoms, tables, OWNER, "ets", "insert", 2, &args).expect("ets:insert/2");
    }
}

/// Runs `op` as `pid` against `handle`.
fn run_op(
    reg: &BifRegistry,
    heap: &mut Heap,
    atoms: &AtomTable,
    tables: &mut TableRegistry,
    pid: ProcessId,
    handle: Eterm,
    op: &Op,
) -> Result<Eterm> {
    let args = build(heap, atoms, |ctx| (op.args)(ctx, handle));
    call(reg, heap, atoms, tables, pid, "ets", op.function, op.arity, &args)
}

/// `ets:info(Tab, Item)` as the owner.
fn info_field(
    reg: &BifRegistry,
    heap: &mut Heap,
    atoms: &AtomTable,
    tables: &mut TableRegistry,
    handle: Eterm,
    item: &str,
) -> Eterm {
    let args = build(heap, atoms, |ctx| {
        let item = atom(ctx, item);
        vec![handle, item]
    });
    call(reg, heap, atoms, tables, OWNER, "ets", "info", 2, &args).expect("ets:info/2")
}

/// `erlang:element/2` of the tuple `term`.
fn element(
    reg: &BifRegistry,
    heap: &mut Heap,
    atoms: &AtomTable,
    tables: &mut TableRegistry,
    pos: i64,
    term: Eterm,
) -> Eterm {
    let args = build(heap, atoms, |ctx| {
        let pos = small(ctx, pos);
        vec![pos, term]
    });
    call(reg, heap, atoms, tables, OWNER, "erlang", "element", 2, &args)
        .expect("erlang:element/2")
}

// --------------------------------------------------------------------- tests

/// A sanity self-check of the op tables: the read list is entirely read-gated and
/// the write list entirely write-gated, and every key is registered (so a typo
/// cannot silently turn into an `undefined_bif`).
#[test]
fn the_op_tables_classify_every_gate() {
    let reg = default_registry();
    for op in read_ops() {
        assert_eq!(op.gate, Gate::Read, "ets:{}/{} is misfiled", op.function, op.arity);
        assert!(
            reg.contains("ets", op.function, op.arity),
            "ets:{}/{} is not registered",
            op.function,
            op.arity
        );
    }
    for op in write_ops() {
        assert_eq!(op.gate, Gate::Write, "ets:{}/{} is misfiled", op.function, op.arity);
        assert!(
            reg.contains("ets", op.function, op.arity),
            "ets:{}/{} is not registered",
            op.function,
            op.arity
        );
    }
}

/// The full **owner × protection × {read, write}** matrix over every gated
/// operation. Each run gets a fresh table, so a mutation can never perturb a
/// later observation.
#[test]
fn the_owner_x_protection_x_access_matrix_is_enforced() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();

    let reads = read_ops();
    let writes = write_ops();

    // (protection, non-owner may read, non-owner may write) — the oracle matrix.
    let modes: [(&str, bool, bool); 3] = [
        ("public", true, true),
        ("protected", true, false),
        ("private", false, false),
    ];

    for (protection, read_ok, write_ok) in modes {
        for (ops, non_owner_allowed) in [(&reads, read_ok), (&writes, write_ok)] {
            for op in ops {
                let name = format!("ets:{}/{}", op.function, op.arity);

                // The owner of a table is always allowed, whatever its protection.
                let handle =
                    new_protected_table(&reg, &mut heap, &atoms, &mut tables, protection);
                seed(&reg, &mut heap, &atoms, &mut tables, handle);
                assert!(
                    run_op(&reg, &mut heap, &atoms, &mut tables, OWNER, handle, op).is_ok(),
                    "{name} must be allowed for the owner of a {protection} table"
                );

                // A non-owner only where the protection admits it.
                let handle =
                    new_protected_table(&reg, &mut heap, &atoms, &mut tables, protection);
                seed(&reg, &mut heap, &atoms, &mut tables, handle);
                let result = run_op(&reg, &mut heap, &atoms, &mut tables, NON_OWNER, handle, op);
                if non_owner_allowed {
                    assert!(
                        result.is_ok(),
                        "{name} must be allowed for a non-owner of a {protection} table"
                    );
                } else {
                    assert!(
                        matches!(result, Err(BeamError::BadArg)),
                        "{name} must be `badarg` for a non-owner of a {protection} table"
                    );
                }
            }
        }
    }
}

/// The `/1` continuation resumes of the select/match/match_object family are
/// read-gated (the continuation is built by the owner's `/3` call, then resumed
/// by both requesters).
#[test]
fn continuation_resumes_are_read_gated() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();

    // The `/3` maker (args `[Tab, Subject, 1]`), per family. The `/1` resume of a
    // family is the same function name at arity 1.
    let families: [(&str, ArgsFn); 3] = [
        ("select", args_whole_spec_limit),
        ("match", args_wildcard_limit),
        ("match_object", args_wildcard_limit),
    ];
    let modes: [(&str, bool); 3] = [("public", true), ("protected", true), ("private", false)];

    // Builds `{Results, Continuation}` for the family as the owner and returns the
    // continuation term.
    let make_continuation = |reg: &BifRegistry,
                             heap: &mut Heap,
                             tables: &mut TableRegistry,
                             protection: &str,
                             function: &str,
                             make_args: ArgsFn|
     -> Eterm {
        let handle = new_protected_table(reg, heap, &atoms, tables, protection);
        seed(reg, heap, &atoms, tables, handle);
        let args = build(heap, &atoms, |ctx| make_args(ctx, handle));
        let chunk = call(reg, heap, &atoms, tables, OWNER, "ets", function, 3, &args)
            .expect("the chunking /3 call");
        element(reg, heap, &atoms, tables, 2, chunk)
    };

    for (protection, non_owner_allowed) in modes {
        for (function, make_args) in families {
            // The owner may always resume.
            let cont = make_continuation(&reg, &mut heap, &mut tables, protection, function, make_args);
            let resumed = call(&reg, &mut heap, &atoms, &mut tables, OWNER, "ets", function, 1, &[cont]);
            assert!(
                resumed.is_ok(),
                "ets:{function}/1 must succeed for the owner of a {protection} table"
            );

            // A non-owner only where the protection admits a read.
            let cont = make_continuation(&reg, &mut heap, &mut tables, protection, function, make_args);
            let resumed =
                call(&reg, &mut heap, &atoms, &mut tables, NON_OWNER, "ets", function, 1, &[cont]);
            if non_owner_allowed {
                assert!(
                    resumed.is_ok(),
                    "ets:{function}/1 must succeed for a non-owner of a {protection} table"
                );
            } else {
                assert!(
                    matches!(resumed, Err(BeamError::BadArg)),
                    "ets:{function}/1 must be `badarg` for a non-owner of a {protection} table"
                );
            }
        }
    }
}

/// `ets:info/1,2` is **not** access-gated: a non-owner may introspect a `private`
/// table (the discriminating fact).
#[test]
fn info_is_not_access_gated_for_a_non_owner_of_a_private_table() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();

    let handle = new_protected_table(&reg, &mut heap, &atoms, &mut tables, "private");

    // `ets:info/1` — the whole proplist.
    let info_1 = call(&reg, &mut heap, &atoms, &mut tables, NON_OWNER, "ets", "info", 1, &[handle]);
    assert!(info_1.is_ok(), "a non-owner must be able to `ets:info/1` a private table");

    // `ets:info/2` — a single field.
    let args = build(&mut heap, &atoms, |ctx| {
        let size = atom(ctx, "size");
        vec![handle, size]
    });
    let info_2 = call(&reg, &mut heap, &atoms, &mut tables, NON_OWNER, "ets", "info", 2, &args);
    assert!(info_2.is_ok(), "a non-owner must be able to `ets:info/2` a private table");
}

/// `ets:info(T, owner)` is the creating (owner) pid as an internal pid term, and
/// `ets:info(T, heir)` is `none` when unset and the recorded pid when the table
/// was created with `{heir, Pid}`.
#[test]
fn info_reports_the_owner_pid_and_the_heir() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();

    // A fresh table records its creator as the owner and no heir.
    let handle = new_protected_table(&reg, &mut heap, &atoms, &mut tables, "public");
    let owner = info_field(&reg, &mut heap, &atoms, &mut tables, handle, "owner");
    assert!(is_internal_pid(owner), "ets:info(T, owner) must be an internal pid term");
    assert_eq!(owner, OWNER.to_eterm(), "ets:info(T, owner) must be the creating pid");
    let heir = info_field(&reg, &mut heap, &atoms, &mut tables, handle, "heir");
    assert_eq!(atoms.name(atom_val(heir)), Some("none"), "an unset heir is `none`");

    // `{heir, Pid}` records the heir pid.
    let handle = new_table_with_heir(&reg, &mut heap, &atoms, &mut tables, HEIR.to_eterm());
    let heir = info_field(&reg, &mut heap, &atoms, &mut tables, handle, "heir");
    assert!(is_internal_pid(heir), "ets:info(T, heir) must be an internal pid term");
    assert_eq!(heir, HEIR.to_eterm(), "ets:info(T, heir) must be the installed heir");

    // `{heir, none}` leaves it unset.
    let none = build(&mut heap, &atoms, |ctx| atom(ctx, "none"));
    let handle = new_table_with_heir(&reg, &mut heap, &atoms, &mut tables, none);
    let heir = info_field(&reg, &mut heap, &atoms, &mut tables, handle, "heir");
    assert_eq!(atoms.name(atom_val(heir)), Some("none"), "a `{{heir, none}}` option leaves the heir unset");
}

/// Runs one `-eval` statement under the system `erl`; `None` when the oracle is
/// unavailable (the test then skips loudly, the `tests/match_spec.rs` /
/// `tests/os_bif.rs` pattern — never a silent pass).
fn oracle_eval(statement: &str) -> Option<String> {
    let output = Command::new("erl")
        .args(["-noshell", "-eval", statement, "-s", "init", "stop"])
        .output();
    match output {
        Ok(out) if out.status.success() => Some(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(out) => {
            eprintln!(
                "SKIP: the oracle `erl` failed ({}); stderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            None
        }
        Err(err) => {
            eprintln!("SKIP: no `erl` oracle on PATH ({err})");
            None
        }
    }
}

/// The live-oracle leg of [`info_reports_the_owner_pid_and_the_heir`]: in the
/// **owning** process the oracle reports `ets:info(T, owner) =:= self()` as `true`
/// and an unset heir as `none`, and `{heir, Pid}` records the pid. The in-crate
/// leg asserts the same facts through the registry and then pins the oracle.
#[test]
fn the_live_oracle_agrees_that_the_table_owner_is_the_calling_process() {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 20).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();

    // In-crate: the owner is this context's pid, and an unset heir is `none`.
    let handle = new_protected_table(&reg, &mut heap, &atoms, &mut tables, "set");
    let owner = info_field(&reg, &mut heap, &atoms, &mut tables, handle, "owner");
    assert_eq!(owner, OWNER.to_eterm(), "the table owner is the creating process");
    let heir = info_field(&reg, &mut heap, &atoms, &mut tables, handle, "heir");
    assert_eq!(atoms.name(atom_val(heir)), Some("none"), "an unset heir is `none`");

    // In-crate: `{heir, Pid}` records the pid.
    let handle = new_table_with_heir(&reg, &mut heap, &atoms, &mut tables, HEIR.to_eterm());
    let heir = info_field(&reg, &mut heap, &atoms, &mut tables, handle, "heir");
    assert_eq!(heir, HEIR.to_eterm(), "the installed heir is the recorded pid");

    // The oracle side, probed with:
    //   T = ets:new(t, [set]),
    //   H = spawn(fun() -> receive stop -> ok end end),
    //   T2 = ets:new(t2, [set, {heir, H}]),
    //   io:format("~w ~w ~w~n",
    //       [ets:info(T, owner) =:= self(), ets:info(T, heir), ets:info(T2, heir) =:= H])
    let statement = "T = ets:new(t, [set]), \
         H = spawn(fun() -> receive stop -> ok end end), \
         T2 = ets:new(t2, [set, {heir, H}]), \
         io:format(\"~w ~w ~w~n\", \
             [ets:info(T, owner) =:= self(), ets:info(T, heir), ets:info(T2, heir) =:= H])";
    let Some(stdout) = oracle_eval(statement) else {
        eprintln!("SKIP: the oracle is unavailable for the ets:info owner/heir probe");
        return;
    };
    assert_eq!(
        stdout.trim(),
        "true none true",
        "the oracle must report the creating process as the owner, an unset heir as `none`"
    );
}
