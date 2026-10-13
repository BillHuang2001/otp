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

//! End-to-end **reachability** tests for the `ets:*` keys that used to be reachable
//! only through a `RuntimeHooks` seam stub: `src/ets/{nav,reverse,extra,specs}.rs`
//! gave eleven of them a real implementation while `src/runtime/other.rs` registered
//! a stub that — because `crate::runtime::register` runs **last** in `with_core_bifs`
//! and `BifRegistry::register` overwrites — shadowed the real one in
//! `default_registry()`. `ets:slot/2` was a stub-only key until it, too, got a real
//! implementation in `src/ets.rs`.
//!
//! Now that the stubs are gone, each of these keys must dispatch **through the
//! public registry** (`beam_bifs::default_registry()` + `BifRegistry::call`) to its
//! real implementation and return the oracle's real result, never the typed
//! `unsupported_bif` capability gap:
//!
//! * `ets:first_lookup/1`, `ets:last_lookup/1`, `ets:next_lookup/2`,
//!   `ets:prev_lookup/2` (`src/ets/nav.rs`);
//! * `ets:rename/2`, `ets:internal_delete_all/2`, `ets:internal_select_delete/2`
//!   (`src/ets/extra.rs`);
//! * `ets:match_spec_run_r/3` (`src/ets/specs.rs`);
//! * `ets:select_reverse/1,2,3` (`src/ets/reverse.rs`);
//! * `ets:slot/2` (`src/ets.rs`), whose `RuntimeHooks` stub was removed when it got
//!   a real implementation over `beam_ets::Table::slot`.
//!
//! The one key that **stays** a `RuntimeHooks` gap — the scheduler fan-in
//! `ets:internal_request_all/0` — is still asserted to answer the typed gap, so the
//! split is pinned as well.
//!
//! Every golden here was probed against the system `erl` (OTP 29.1.1 / erts-17.1);
//! the probes are quoted beside the assertions. Terms are compared
//! **structurally** ([`beam_term::compare::eq`]) and destructuring goes through the
//! registered `erlang` BIFs, so the suite uses only the public API. One live
//! cross-check shells out to the system `erl` and **skips loudly** when it is
//! absent (the `tests/packet.rs` pattern).

use std::process::Command;

use beam_alloc::Heap;
use beam_bifs::prelude::*;
use beam_common::{BeamError, ExitReason, Result};
use beam_process::ProcessId;
use beam_term::{atom_val, signed_value, AtomTable, Eterm};

/// The pid `ets:new/2` records as the owner of every table this suite creates.
fn self_pid() -> ProcessId {
    ProcessId::new(1, 0)
}

/// The atom names these tests resolve by name (the `ets` options/values, the probe
/// keys, the match-spec variables and the object payloads).
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed([
        "true",
        "false",
        "undefined",
        "set",
        "ordered_set",
        "public",
        "private",
        "named_table",
        "$end_of_table",
        // Table names.
        "nav_t",
        "d",
        "rename_me",
        "new_name",
        // Match-spec variables.
        "$1",
        "$2",
        // Object payloads.
        "a",
        "b",
        "c",
    ]);
    atoms
}

/// Calls `module:function/arity` through the full public registry.
fn call(
    bifs: &BifRegistry,
    ctx: &mut BifContext<'_>,
    module: &str,
    function: &str,
    arity: u32,
    args: &[Eterm],
) -> Result<Eterm> {
    bifs.call(module, function, arity, ctx, args)
}

/// A small integer term on the context heap.
fn int(ctx: &mut BifContext<'_>, value: i64) -> Eterm {
    ctx.int(value).expect("a small int fits")
}

/// The small-integer value of `term`.
fn as_int(term: Eterm) -> i64 {
    signed_value(term)
}

/// Whether `term` is the atom named `name`.
fn atom_is(atoms: &AtomTable, term: Eterm, name: &str) -> bool {
    atoms.name(atom_val(term)) == Some(name)
}

/// Whether `err` is the typed `unsupported_bif` capability gap.
fn is_unsupported(err: &BeamError) -> bool {
    matches!(err, BeamError::Raise(r) if r.reason == ExitReason::atom("unsupported_bif"))
}

/// Structural equality of two terms.
fn assert_same(atoms: &AtomTable, got: Eterm, expected: Eterm) {
    assert!(
        beam_term::compare::eq(got, expected, atoms),
        "the registry result does not match the expected term"
    );
}

/// `ets:new(Name, Options)` through the registry (`Options` a list of atoms).
fn new_table(bifs: &BifRegistry, ctx: &mut BifContext<'_>, name: &str, options: &[&str]) -> Eterm {
    let name = ctx.atom(name).expect("a seeded table name");
    let mut opts = Vec::with_capacity(options.len());
    for option in options {
        opts.push(ctx.atom(option).expect("a seeded option"));
    }
    let opts = ctx.list(&opts).expect("the options fit");
    call(bifs, ctx, "ets", "new", 2, &[name, opts]).expect("ets:new")
}

/// The `{Key, Value}` object tuple (both small ints) on the context heap.
fn pair_obj(ctx: &mut BifContext<'_>, key: i64, value: i64) -> Eterm {
    let k = int(ctx, key);
    let v = int(ctx, value);
    ctx.tuple(&[k, v]).expect("the object fits")
}

/// The `{Key, Value}` object tuple with an **atom** value on the context heap.
fn keyed_obj(ctx: &mut BifContext<'_>, key: i64, value: &str) -> Eterm {
    let k = int(ctx, key);
    let v = ctx.atom(value).expect("a seeded payload atom");
    ctx.tuple(&[k, v]).expect("the object fits")
}

/// Inserts the `{K, K * 10}` objects for `rows`, in order.
fn insert_pairs(bifs: &BifRegistry, ctx: &mut BifContext<'_>, handle: Eterm, rows: &[(i64, i64)]) {
    for (k, v) in rows {
        let obj = pair_obj(ctx, *k, *v);
        let inserted = call(bifs, ctx, "ets", "insert", 2, &[handle, obj]).expect("ets:insert");
        assert!(atom_is(ctx.atoms(), inserted, "true"), "insert returns true");
    }
}

/// The list of small integers `values` on the context heap.
fn int_list(ctx: &mut BifContext<'_>, values: &[i64]) -> Eterm {
    let mut terms = Vec::with_capacity(values.len());
    for value in values {
        terms.push(int(ctx, *value));
    }
    ctx.list(&terms).expect("the list fits")
}

/// The list of `{K, V}` object tuples for `rows` on the context heap.
fn obj_list(ctx: &mut BifContext<'_>, rows: &[(i64, i64)]) -> Eterm {
    let mut objs = Vec::with_capacity(rows.len());
    for (k, v) in rows {
        objs.push(pair_obj(ctx, *k, *v));
    }
    ctx.list(&objs).expect("the list fits")
}

/// The `{Key, [Objects]}` a `*_lookup` navigation step returns, built as the
/// expected term for `key`/`rows`.
fn expected_key_objects(ctx: &mut BifContext<'_>, key: i64, rows: &[(i64, i64)]) -> Eterm {
    let mut objs = Vec::new();
    for (k, v) in rows {
        objs.push(pair_obj(ctx, *k, *v));
    }
    let list = ctx.list(&objs).expect("the object list fits");
    let key_term = int(ctx, key);
    ctx.tuple(&[key_term, list]).expect("the 2-tuple fits")
}

/// The raw match spec `[{{'$1','$2'},[],['$1']}]` — the body is the key.
fn key_spec(ctx: &mut BifContext<'_>) -> Eterm {
    let v1 = ctx.atom("$1").expect("seeded");
    let v2 = ctx.atom("$2").expect("seeded");
    let head = ctx.tuple(&[v1, v2]).expect("fits");
    let guards = ctx.list(&[]).expect("fits");
    let body = ctx.list(&[v1]).expect("fits");
    let clause = ctx.tuple(&[head, guards, body]).expect("fits");
    ctx.list(&[clause]).expect("fits")
}

/// The raw match spec `[{{1,'$1'},[],['$1']}]` — matches every `{1, Value}` and
/// yields `Value`.
fn pick_first_spec(ctx: &mut BifContext<'_>) -> Eterm {
    let one = int(ctx, 1);
    let v1 = ctx.atom("$1").expect("seeded");
    let head = ctx.tuple(&[one, v1]).expect("fits");
    let guards = ctx.list(&[]).expect("fits");
    let body = ctx.list(&[v1]).expect("fits");
    let clause = ctx.tuple(&[head, guards, body]).expect("fits");
    ctx.list(&[clause]).expect("fits")
}

/// The raw match spec `[{{1,'$1'},[],[true]}]` — the delete-selecting clause the
/// oracle probe for `ets:internal_select_delete/2` uses.
fn delete_key1_spec(ctx: &mut BifContext<'_>) -> Eterm {
    let one = int(ctx, 1);
    let v1 = ctx.atom("$1").expect("seeded");
    let head = ctx.tuple(&[one, v1]).expect("fits");
    let guards = ctx.list(&[]).expect("fits");
    let yes = ctx.atom("true").expect("seeded");
    let body = ctx.list(&[yes]).expect("fits");
    let clause = ctx.tuple(&[head, guards, body]).expect("fits");
    ctx.list(&[clause]).expect("fits")
}

// -------------------------------------------------------------- the hook gate

/// The twelve unshadowed keys left `RUNTIME_HOOKS_BIFS` (they are no longer
/// `RuntimeHooks` seam keys); the one kept key is still there.
#[test]
fn the_unshadowed_keys_are_no_longer_runtime_hook_keys() {
    for (function, arity) in [
        ("first_lookup", 1),
        ("last_lookup", 1),
        ("next_lookup", 2),
        ("prev_lookup", 2),
        ("rename", 2),
        ("internal_delete_all", 2),
        ("internal_select_delete", 2),
        ("match_spec_run_r", 3),
        ("select_reverse", 1),
        ("select_reverse", 2),
        ("select_reverse", 3),
        ("slot", 2),
    ] {
        assert!(
            !needs_runtime_hooks("ets", function, arity),
            "ets:{function}/{arity} must no longer be a RuntimeHooks key"
        );
    }
    assert!(
        needs_runtime_hooks("ets", "internal_request_all", 0),
        "ets:internal_request_all/0 stays a RuntimeHooks key"
    );
}

/// The one key that stays seam-backed still answers the typed capability gap
/// through the registry (with no `RuntimeHooks` installed).
#[test]
fn the_kept_ets_key_still_answers_the_typed_gap() {
    let bifs = default_registry();
    let mut heap = Heap::new(1 << 14).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx = BifContext::new(&mut heap, &atoms, self_pid()).with_tables(&mut tables);

    let err = call(&bifs, &mut ctx, "ets", "internal_request_all", 0, &[])
        .expect_err("internal_request_all/0");
    assert!(
        is_unsupported(&err),
        "ets:internal_request_all/0 must be the typed gap: {err:?}"
    );
}

// ----------------------------------------------------------------- nav: slot

/// `ets:slot/2` reaches its real implementation and walks an `ordered_set`'s exact
/// slots.
///
/// Oracle probe (`erl -noshell`, OTP 29.1.1 / erts-17.1), an `ordered_set` holding
/// `{6,60},{3,30},{4,40},{8,80},{1,10}` (insertion order), read as
/// `[ets:slot(T, N) || N <- lists:seq(0, 5)]`:
/// `[[{1,10}],[{3,30}],[{4,40}],[{6,60}],[{8,80}],'$end_of_table']` — slot `n` is the
/// object of the `(n+1)`-th smallest key, and the first slot past the end is the
/// `'$end_of_table'` atom (never a `badarg`). The live oracle is re-queried at the
/// end of the test; it **skips loudly** (never passes vacuously — the pinned
/// assertions above always run) when `erl` is absent.
#[test]
fn slot_walks_the_ordered_set_exactly() {
    let bifs = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx = BifContext::new(&mut heap, &atoms, self_pid()).with_tables(&mut tables);

    let handle = new_table(&bifs, &mut ctx, "nav_t", &["ordered_set"]);
    let rows = [(6, 60), (3, 30), (4, 40), (8, 80), (1, 10)];
    insert_pairs(&bifs, &mut ctx, handle, &rows);

    for (slot, row) in [
        (0i64, (1, 10)),
        (1, (3, 30)),
        (2, (4, 40)),
        (3, (6, 60)),
        (4, (8, 80)),
    ] {
        let s = int(&mut ctx, slot);
        let chunk = call(&bifs, &mut ctx, "ets", "slot", 2, &[handle, s]).expect("ets:slot");
        let expected = obj_list(&mut ctx, &[row]);
        assert_same(&atoms, chunk, expected);
    }

    // The first slot past the end is `'$end_of_table'`, not a `badarg`.
    let five = int(&mut ctx, 5);
    let end = call(&bifs, &mut ctx, "ets", "slot", 2, &[handle, five]).expect("ets:slot");
    assert!(
        atom_is(&atoms, end, "$end_of_table"),
        "slot 5 past the end is the end-of-table atom"
    );

    // Live oracle cross-check (skips loudly when `erl` is absent).
    let statement = concat!(
        "T = ets:new(nav_t, [ordered_set]), ",
        "[ets:insert(T, O) || O <- [{6,60},{3,30},{4,40},{8,80},{1,10}]], ",
        "io:format(\"~w~n\", [[ets:slot(T, N) || N <- lists:seq(0, 5)]])"
    );
    let Some(actual) = oracle_eval(statement) else {
        return;
    };
    assert_eq!(
        actual,
        "[[{1,10}],[{3,30}],[{4,40}],[{6,60}],[{8,80}],'$end_of_table']",
        "the live oracle disagrees with the pinned `ets:slot/2` goldens"
    );
}

/// An **empty** `ordered_set`'s slot 0 is `'$end_of_table'` (oracle-verified):
/// there is no object to return, but the slot is still in range.
#[test]
fn slot_of_an_empty_ordered_set_is_the_end() {
    let bifs = default_registry();
    let mut heap = Heap::new(1 << 14).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx = BifContext::new(&mut heap, &atoms, self_pid()).with_tables(&mut tables);

    let handle = new_table(&bifs, &mut ctx, "nav_t", &["ordered_set"]);
    let zero = int(&mut ctx, 0);
    let out = call(&bifs, &mut ctx, "ets", "slot", 2, &[handle, zero]).expect("ets:slot");
    assert!(
        atom_is(&atoms, out, "$end_of_table"),
        "an empty ordered_set has no slot 0 object"
    );
}

/// Every ill-typed or out-of-range slot is the oracle's `badarg`: a **negative**
/// slot, a slot past the end, a **float**, a **bignum** and a non-integer
/// (oracle-verified: a bignum and a float are `badarg` because the C reads only a
/// small integer, and a negative slot is rejected before the interior).
#[test]
fn slot_argument_errors_are_badarg() {
    let bifs = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx = BifContext::new(&mut heap, &atoms, self_pid()).with_tables(&mut tables);

    let handle = new_table(&bifs, &mut ctx, "nav_t", &["ordered_set"]);
    insert_pairs(&bifs, &mut ctx, handle, &[(1, 10), (2, 20), (3, 30)]);

    let negative = int(&mut ctx, -1);
    let past_end = int(&mut ctx, 99);
    let float = ctx.make_float(1.0).expect("the float fits");
    let non_integer = ctx.atom("undefined").expect("seeded");
    let bignum = beam_codec::term::make_big(ctx.heap(), false, &[0, 1]).expect("2^64 fits");

    for (name, slot) in [
        ("negative", negative),
        ("past_end", past_end),
        ("float", float),
        ("non_integer", non_integer),
        ("bignum", bignum),
    ] {
        let err = call(&bifs, &mut ctx, "ets", "slot", 2, &[handle, slot])
            .expect_err("a bad slot is badarg");
        assert!(
            matches!(err, BeamError::BadArg),
            "a {name} slot must be badarg, got {err:?}"
        );
    }
}

/// `ets:slot/2` is read-gated: a **non-owner** of a `private` table is `badarg`,
/// while the owner reads it (the oracle's `DB_READ` status mask).
#[test]
fn slot_is_read_gated_for_a_non_owner_of_a_private_table() {
    let bifs = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();

    // The owner creates the private table and inserts two objects.
    let handle = {
        let mut ctx = BifContext::new(&mut heap, &atoms, self_pid()).with_tables(&mut tables);
        let handle = new_table(&bifs, &mut ctx, "nav_t", &["ordered_set", "private"]);
        insert_pairs(&bifs, &mut ctx, handle, &[(1, 10), (2, 20)]);
        let zero = int(&mut ctx, 0);
        let out = call(&bifs, &mut ctx, "ets", "slot", 2, &[handle, zero]).expect("owner slot");
        assert_same(&atoms, out, obj_list(&mut ctx, &[(1, 10)]));
        handle
    };

    // A different process is refused.
    let other = ProcessId::new(2, 0);
    let mut ctx = BifContext::new(&mut heap, &atoms, other).with_tables(&mut tables);
    let zero = int(&mut ctx, 0);
    let err = call(&bifs, &mut ctx, "ets", "slot", 2, &[handle, zero])
        .expect_err("a non-owner read of a private table is badarg");
    assert!(
        matches!(err, BeamError::BadArg),
        "a non-owner `ets:slot/2` on a private table must be badarg, got {err:?}"
    );
}
// ------------------------------------------------------------- nav: lookups

/// `ets:first_lookup/1` and `ets:last_lookup/1` reach their real implementations.
///
/// Oracle probe (`erl -noshell`, erts-17.1), an `ordered_set` holding
/// `{1,10},{2,20},{3,30}`:
/// `ets:first_lookup(T)` ⇒ `{1,[{1,10}]}`, `ets:last_lookup(T)` ⇒ `{3,[{3,30}]}`.
#[test]
fn first_and_last_lookup_are_reachable_and_ordered() {
    let bifs = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx = BifContext::new(&mut heap, &atoms, self_pid()).with_tables(&mut tables);

    let handle = new_table(&bifs, &mut ctx, "nav_t", &["ordered_set"]);
    insert_pairs(&bifs, &mut ctx, handle, &[(1, 10), (2, 20), (3, 30)]);

    let first =
        call(&bifs, &mut ctx, "ets", "first_lookup", 1, &[handle]).expect("ets:first_lookup/1");
    let expected = expected_key_objects(&mut ctx, 1, &[(1, 10)]);
    assert_same(&atoms, first, expected);

    let last =
        call(&bifs, &mut ctx, "ets", "last_lookup", 1, &[handle]).expect("ets:last_lookup/1");
    let expected = expected_key_objects(&mut ctx, 3, &[(3, 30)]);
    assert_same(&atoms, last, expected);
}

/// `ets:next_lookup/2` and `ets:prev_lookup/2` reach their real implementations,
/// including the ordered-set **range-bound** rule for a key past the end.
///
/// Oracle probe (erts-17.1): `next_lookup(T,1)` ⇒ `{2,[{2,20}]}`,
/// `prev_lookup(T,3)` ⇒ `{2,[{2,20}]}`, `prev_lookup(T,99)` ⇒ `{3,[{3,30}]}`,
/// `next_lookup(T,99)` ⇒ `'$end_of_table'`.
#[test]
fn next_and_prev_lookup_are_reachable_with_the_range_bound_rule() {
    let bifs = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx = BifContext::new(&mut heap, &atoms, self_pid()).with_tables(&mut tables);

    let handle = new_table(&bifs, &mut ctx, "nav_t", &["ordered_set"]);
    insert_pairs(&bifs, &mut ctx, handle, &[(1, 10), (2, 20), (3, 30)]);

    let one = int(&mut ctx, 1);
    let three = int(&mut ctx, 3);
    let past = int(&mut ctx, 99);

    let next = call(&bifs, &mut ctx, "ets", "next_lookup", 2, &[handle, one]).expect("next_lookup");
    let expected = expected_key_objects(&mut ctx, 2, &[(2, 20)]);
    assert_same(&atoms, next, expected);

    let prev = call(&bifs, &mut ctx, "ets", "prev_lookup", 2, &[handle, three]).expect("prev_lookup");
    let expected = expected_key_objects(&mut ctx, 2, &[(2, 20)]);
    assert_same(&atoms, prev, expected);

    // The range-bound rule: an absent anchor is answered by the nearest stored key.
    let prev_past =
        call(&bifs, &mut ctx, "ets", "prev_lookup", 2, &[handle, past]).expect("prev_lookup(99)");
    let expected = expected_key_objects(&mut ctx, 3, &[(3, 30)]);
    assert_same(&atoms, prev_past, expected);

    let next_past =
        call(&bifs, &mut ctx, "ets", "next_lookup", 2, &[handle, past]).expect("next_lookup(99)");
    assert!(
        atom_is(&atoms, next_past, "$end_of_table"),
        "a walk past the last key is '$end_of_table'"
    );
}

/// The `'$end_of_table'` **anchor** raises `badarg` for the ordered
/// `ets:next_lookup/2` and `ets:prev_lookup/2` — through the public registry —
/// even when the atom is itself a **stored** key, and on an empty table; a hash
/// kind is unaffected.
///
/// The C `db_next_tree_common`/`db_prev_tree_common`
/// (`../../erts/emulator/beam/erl_db_tree.c:654,729`) return `DB_ERROR_BADKEY`
/// unconditionally on `am_EOT` (`db_next_hash_common` has no such special-case).
/// Oracle probe (erts-17.1): an `ordered_set` holding `{1,10},{2,20},{3,30}`
/// answers `badarg` for `ets:next_lookup(T,'$end_of_table')` and
/// `ets:prev_lookup(T,'$end_of_table')`, stored or not; a `set` with the atom
/// stored walks it as an ordinary key.
#[test]
fn ordered_lookups_reject_the_end_of_table_anchor() {
    let bifs = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx = BifContext::new(&mut heap, &atoms, self_pid()).with_tables(&mut tables);

    let handle = new_table(&bifs, &mut ctx, "nav_t", &["ordered_set"]);
    let eot = ctx.atom("$end_of_table").expect("seeded");

    // An empty `ordered_set` rejects the anchor for both lookup walkers.
    for name in ["next_lookup", "prev_lookup"] {
        assert!(
            matches!(
                call(&bifs, &mut ctx, "ets", name, 2, &[handle, eot]),
                Err(BeamError::BadArg)
            ),
            "ets:{name}/2 on an empty ordered_set must be badarg"
        );
    }

    insert_pairs(&bifs, &mut ctx, handle, &[(1, 10), (2, 20), (3, 30)]);

    // The ordinary lookup walk still answers; an *absent* anchor that is not the
    // walk's terminator is `badarg` nonetheless.
    let one = int(&mut ctx, 1);
    let next = call(&bifs, &mut ctx, "ets", "next_lookup", 2, &[handle, one])
        .expect("ets:next_lookup(T,1)");
    let expected = expected_key_objects(&mut ctx, 2, &[(2, 20)]);
    assert_same(&atoms, next, expected);
    for name in ["next_lookup", "prev_lookup"] {
        assert!(
            matches!(
                call(&bifs, &mut ctx, "ets", name, 2, &[handle, eot]),
                Err(BeamError::BadArg)
            ),
            "ets:{name}/2 on the '$end_of_table' anchor must be badarg"
        );
    }

    // Storing the atom as a key leaves it rejected.
    let eot_obj = ctx.tuple(&[eot, eot]).expect("the object fits");
    call(&bifs, &mut ctx, "ets", "insert", 2, &[handle, eot_obj]).expect("ets:insert");
    for name in ["next_lookup", "prev_lookup"] {
        assert!(
            matches!(
                call(&bifs, &mut ctx, "ets", name, 2, &[handle, eot]),
                Err(BeamError::BadArg)
            ),
            "ets:{name}/2 on the stored '$end_of_table' anchor must be badarg"
        );
    }

    // A hash kind (`set`) is unaffected: with the atom stored it is an ordinary
    // key, so the forward step walks from it and the backward one aliases it.
    let hashed = new_table(&bifs, &mut ctx, "d", &["set"]);
    for obj in [
        ctx.tuple(&[one, one]).expect("fits"),
        ctx.tuple(&[eot, eot]).expect("fits"),
    ] {
        call(&bifs, &mut ctx, "ets", "insert", 2, &[hashed, obj]).expect("ets:insert");
    }
    let next = call(&bifs, &mut ctx, "ets", "next_lookup", 2, &[hashed, eot])
        .expect("next_lookup(eot) on a set is not badarg");
    let prev = call(&bifs, &mut ctx, "ets", "prev_lookup", 2, &[hashed, eot])
        .expect("prev_lookup(eot) on a set is not badarg");
    assert_same(&atoms, next, prev);
}

/// One live cross-check that the system ERTS really rejects the
/// `'$end_of_table'` anchor in all four ordered walkers (the goldens this suite
/// pins for the crate's own BIFs). `~w` is used so the oracle does not line-wrap;
/// the test **skips loudly** when no `erl` is on `PATH`.
#[test]
fn the_live_oracle_rejects_the_end_of_table_anchor() {
    let statement = concat!(
        "T = ets:new(nav_t, [ordered_set]), ",
        "[ets:insert(T, O) || O <- [{1,10},{2,20},{3,30}]], ",
        "E = '$end_of_table', ",
        "F = fun(G) -> try G() of V -> {ok, V} catch C:R -> {C, R} end end, ",
        "io:format(\"~w~n\", [[F(fun() -> ets:next(T, E) end), ",
        "F(fun() -> ets:prev(T, E) end), ",
        "F(fun() -> ets:next_lookup(T, E) end), ",
        "F(fun() -> ets:prev_lookup(T, E) end)]])"
    );
    let Some(actual) = oracle_eval(statement) else {
        return;
    };
    assert_eq!(
        actual, "[{error,badarg},{error,badarg},{error,badarg},{error,badarg}]",
        "the live oracle no longer rejects the '$end_of_table' anchor"
    );
}

// ---------------------------------------------------------------- rename/2

/// `ets:rename/2` reaches its real implementation and rekeys the name registry.
///
/// Oracle probe (erts-17.1): for `T = ets:new(old, [set, named_table])`,
/// `ets:rename(T, new)` ⇒ `new`, `ets:whereis(old)` ⇒ `undefined`, and the new
/// name resolves to the *same* table with its objects intact.
#[test]
fn rename_2_is_reachable_and_rekeys_a_named_table() {
    let bifs = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx = BifContext::new(&mut heap, &atoms, self_pid()).with_tables(&mut tables);

    let old = ctx.atom("rename_me").expect("seeded");
    let handle = new_table(&bifs, &mut ctx, "rename_me", &["named_table", "set"]);
    assert_eq!(handle, old, "a named_table's handle is its name atom");
    insert_pairs(&bifs, &mut ctx, handle, &[(1, 10), (2, 20)]);

    let new = ctx.atom("new_name").expect("seeded");
    let renamed = call(&bifs, &mut ctx, "ets", "rename", 2, &[handle, new]).expect("ets:rename/2");
    assert_eq!(renamed, new, "a named rename answers the new name atom");

    // The old name left the registry.
    let whereis_old = call(&bifs, &mut ctx, "ets", "whereis", 1, &[old]).expect("ets:whereis/1");
    assert!(atom_is(&atoms, whereis_old, "undefined"));

    // The new name resolves to the very same table, objects intact.
    let one = int(&mut ctx, 1);
    let found = call(&bifs, &mut ctx, "ets", "lookup", 2, &[new, one]).expect("ets:lookup/2");
    let expected = obj_list(&mut ctx, &[(1, 10)]);
    assert_same(&atoms, found, expected);
}

/// `ets:rename/2` on a table created **without** `named_table` returns the
/// unchanged handle (`R =:= Tab`), the oracle's own asymmetry.
#[test]
fn rename_2_on_an_unnamed_table_returns_the_handle() {
    let bifs = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx = BifContext::new(&mut heap, &atoms, self_pid()).with_tables(&mut tables);

    let handle = new_table(&bifs, &mut ctx, "nav_t", &["set"]);
    let new = ctx.atom("new_name").expect("seeded");
    let renamed = call(&bifs, &mut ctx, "ets", "rename", 2, &[handle, new]).expect("ets:rename/2");
    assert_eq!(renamed, handle, "an unnamed rename answers the unchanged handle");

    // The name was not registered.
    let whereis = call(&bifs, &mut ctx, "ets", "whereis", 1, &[new]).expect("ets:whereis/1");
    assert!(atom_is(&atoms, whereis, "undefined"));
}

// --------------------------------------------------- internal_* delete verbs

/// `ets:internal_delete_all/2` reaches its real implementation: with the
/// `undefined` holder it empties the table and answers the deleted count, and any
/// other holder is returned verbatim with the table untouched.
///
/// Oracle probe (erts-17.1): three objects ⇒ `3`, then `0`; a holder `42` ⇒ `42`.
#[test]
fn internal_delete_all_2_is_reachable() {
    let bifs = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx = BifContext::new(&mut heap, &atoms, self_pid()).with_tables(&mut tables);

    let handle = new_table(&bifs, &mut ctx, "nav_t", &["set"]);
    insert_pairs(&bifs, &mut ctx, handle, &[(1, 10), (2, 20), (3, 30)]);

    let undefined = ctx.atom("undefined").expect("seeded");
    let deleted = call(&bifs, &mut ctx, "ets", "internal_delete_all", 2, &[handle, undefined])
        .expect("ets:internal_delete_all/2");
    assert_eq!(as_int(deleted), 3, "the number of deleted objects is answered");

    // The table is now empty…
    let one = int(&mut ctx, 1);
    let lookup = call(&bifs, &mut ctx, "ets", "lookup", 2, &[handle, one]).expect("ets:lookup/2");
    let empty = ctx.list(&[]).expect("fits");
    assert_same(&atoms, lookup, empty);

    // …and a second pass reports 0.
    let again = call(&bifs, &mut ctx, "ets", "internal_delete_all", 2, &[handle, undefined])
        .expect("ets:internal_delete_all/2");
    assert_eq!(as_int(again), 0);

    // The trap protocol: a non-`undefined` holder is returned verbatim and the
    // table is not touched.
    let marker = int(&mut ctx, 42);
    let trapped = call(&bifs, &mut ctx, "ets", "internal_delete_all", 2, &[handle, marker])
        .expect("ets:internal_delete_all/2");
    assert_eq!(trapped, marker, "a non-undefined holder is the answer verbatim");
}

/// `ets:internal_select_delete/2` reaches its real implementation and answers the
/// count the raw match spec deleted.
///
/// Oracle probe (erts-17.1): a `set` holding `{1,a},{2,a},{3,a}` and
/// `[{{1,'$1'},[],[true]}]` ⇒ `1`, and re-running it ⇒ `0`.
#[test]
fn internal_select_delete_2_is_reachable() {
    let bifs = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx = BifContext::new(&mut heap, &atoms, self_pid()).with_tables(&mut tables);

    // The objects are `{Key, a}`; the key is the first element, so a `set` keyed on
    // position 1 holds keys 1, 2 and 3.
    let handle = new_table(&bifs, &mut ctx, "d", &["set"]);
    for key in 1..=3 {
        let obj = keyed_obj(&mut ctx, key, "a");
        call(&bifs, &mut ctx, "ets", "insert", 2, &[handle, obj]).expect("ets:insert");
    }

    let spec = delete_key1_spec(&mut ctx);
    let deleted = call(&bifs, &mut ctx, "ets", "internal_select_delete", 2, &[handle, spec])
        .expect("ets:internal_select_delete/2");
    assert_eq!(as_int(deleted), 1, "one object was selected and deleted");

    let again = call(&bifs, &mut ctx, "ets", "internal_select_delete", 2, &[handle, spec])
        .expect("ets:internal_select_delete/2");
    assert_eq!(as_int(again), 0, "the second pass deletes nothing");
}

// ------------------------------------------------------- match_spec_run_r/3

/// `ets:match_spec_run_r/3` reaches its real implementation and **prepends** each
/// match onto the accumulator.
///
/// Oracle probe (erts-17.1): with
/// `MS = ets:match_spec_compile([{{1,'$1'},[],['$1']}])`,
/// `ets:match_spec_run_r([{1,a},{2,b},{1,c}], MS, [])` ⇒ `[c,a]` (the matching
/// prefix reversed), while the forward `ets:match_spec_run/2` is `[a,c]`.
#[test]
fn match_spec_run_r_3_is_reachable() {
    let bifs = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx = BifContext::new(&mut heap, &atoms, self_pid()).with_tables(&mut tables);

    let spec = pick_first_spec(&mut ctx);
    let compiled = call(&bifs, &mut ctx, "ets", "match_spec_compile", 1, &[spec])
        .expect("ets:match_spec_compile/1");

    let t_a = keyed_obj(&mut ctx, 1, "a");
    let t_b = keyed_obj(&mut ctx, 2, "b");
    let t_c = keyed_obj(&mut ctx, 1, "c");
    let input = ctx.list(&[t_a, t_b, t_c]).expect("fits");
    let empty_acc = ctx.list(&[]).expect("fits");

    let got = call(
        &bifs,
        &mut ctx,
        "ets",
        "match_spec_run_r",
        3,
        &[input, compiled, empty_acc],
    )
    .expect("ets:match_spec_run_r/3");

    let c = ctx.atom("c").expect("seeded");
    let a = ctx.atom("a").expect("seeded");
    let expected = ctx.list(&[c, a]).expect("fits");
    assert_same(&atoms, got, expected);
}

// -------------------------------------------------------- select_reverse/2,3

/// `ets:select_reverse/2` and `/3` reach their real implementations: `/2` is the
/// full reverse-ordered result list, `/3` the `{Results, Continuation}` chunk, and
/// `/1` resumes that continuation.
///
/// Oracle probe (erts-17.1): an `ordered_set` holding `{1,10},{2,20},{3,30},{4,40}`
/// and `[{{'$1','$2'},[],['$1']}]` — `ets:select_reverse(T, S)` ⇒ `[4,3,2,1]`,
/// `element(1, ets:select_reverse(T, S, 2))` ⇒ `[3,2]`, and resuming that
/// continuation ⇒ `[2,1]`.
#[test]
fn select_reverse_1_2_and_3_are_reachable() {
    let bifs = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx = BifContext::new(&mut heap, &atoms, self_pid()).with_tables(&mut tables);

    let handle = new_table(&bifs, &mut ctx, "nav_t", &["ordered_set"]);
    insert_pairs(
        &bifs,
        &mut ctx,
        handle,
        &[(1, 10), (2, 20), (3, 30), (4, 40)],
    );
    let spec = key_spec(&mut ctx);

    let all = call(&bifs, &mut ctx, "ets", "select_reverse", 2, &[handle, spec])
        .expect("ets:select_reverse/2");
    let expected = int_list(&mut ctx, &[4, 3, 2, 1]);
    assert_same(&atoms, all, expected);

    let two = int(&mut ctx, 2);
    let chunk = call(&bifs, &mut ctx, "ets", "select_reverse", 3, &[handle, spec, two])
        .expect("ets:select_reverse/3");

    // The chunk is the 2-tuple `{[4,3], Continuation}`.
    let one_pos = int(&mut ctx, 1);
    let size = call(&bifs, &mut ctx, "erlang", "tuple_size", 1, &[chunk]).expect("tuple_size");
    assert_eq!(as_int(size), 2, "a chunk is a 2-tuple");

    let results = call(&bifs, &mut ctx, "erlang", "element", 2, &[one_pos, chunk]).expect("element");
    let expected = int_list(&mut ctx, &[4, 3]);
    assert_same(&atoms, results, expected);

    let two_pos = int(&mut ctx, 2);
    let cont = call(&bifs, &mut ctx, "erlang", "element", 2, &[two_pos, chunk]).expect("element");
    let is_ref =
        call(&bifs, &mut ctx, "erlang", "is_reference", 1, &[cont]).expect("is_reference");
    assert!(
        atom_is(&atoms, is_ref, "true"),
        "a full chunk yields a continuation reference"
    );

    // `ets:select_reverse/1` resumes the continuation: the next chunk `{[2,1], _}`
    // (oracle: `element(1, ets:select_reverse(C1))` ⇒ `[2,1]`).
    let resumed =
        call(&bifs, &mut ctx, "ets", "select_reverse", 1, &[cont]).expect("ets:select_reverse/1");
    let size = call(&bifs, &mut ctx, "erlang", "tuple_size", 1, &[resumed]).expect("tuple_size");
    assert_eq!(as_int(size), 2, "a resumed chunk is a 2-tuple");
    let results =
        call(&bifs, &mut ctx, "erlang", "element", 2, &[one_pos, resumed]).expect("element");
    let expected = int_list(&mut ctx, &[2, 1]);
    assert_same(&atoms, results, expected);
}

// ------------------------------------------------------- live oracle check

/// Runs one `-eval` statement under the system `erl`; `None` when the oracle is
/// unavailable (the test then skips loudly).
fn oracle_eval(statement: &str) -> Option<String> {
    let output = Command::new("erl")
        .args(["-noshell", "-eval", statement, "-s", "init", "stop"])
        .output();
    match output {
        Ok(out) if out.status.success() => {
            Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
        }
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

/// One live cross-check that the goldens this suite pins are what the system ERTS
/// still produces. `~w` (not `~p`) is used so the oracle does not line-wrap.
#[test]
fn the_live_oracle_matches_the_pinned_goldens() {
    let statement = concat!(
        "T = ets:new(nav_t, [ordered_set]), ",
        "[ets:insert(T, O) || O <- [{1,10},{2,20},{3,30}]], ",
        "D = ets:new(d, [set]), ",
        "[ets:insert(D, O) || O <- [{1,a},{2,a},{3,a}]], ",
        "MS = ets:match_spec_compile([{{1,'$1'},[],['$1']}]), ",
        "Run = ets:match_spec_run_r([{1,a},{2,b},{1,c}], MS, []), ",
        "S = [{{'$1','$2'},[],['$1']}], ",
        "Del = ets:internal_select_delete(D, [{{1,'$1'},[],[true]}]), ",
        "Cnt = ets:internal_delete_all(D, undefined), ",
        "io:format(\"~w~n\", [{",
        "ets:first_lookup(T), ets:last_lookup(T), ",
        "ets:next_lookup(T,1), ets:prev_lookup(T,3), ",
        "ets:prev_lookup(T,99), ets:next_lookup(T,99), ",
        "Run, ets:select_reverse(T, S), element(1, ets:select_reverse(T, S, 2)), ",
        "Del, Cnt, ets:lookup(D, 1)}])"
    );
    let Some(actual) = oracle_eval(statement) else {
        return;
    };
    assert_eq!(
        actual,
        "{{1,[{1,10}]},{3,[{3,30}]},{2,[{2,20}]},{2,[{2,20}]},{3,[{3,30}]},\
         '$end_of_table',[c,a],[3,2,1],[3,2],1,2,[]}",
        "the live oracle disagrees with the pinned goldens"
    );
}
