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

//! End-to-end tests for the match-spec compiler/executor and the `select`/`match`
//! family.
//!
//! Every expectation below was derived by running the same spec against the system
//! `erl` (OTP 29 / erts-17.1, `erl -noshell -eval '…' -s init stop`); the probe
//! results are quoted next to the assertion.
//!
//! This is deliberately one comprehensive suite: it is a long file (over the
//! workspace's ~1000-line guidance) because the match-spec language is covered as
//! a single coherent battery — heads, guards, bodies, arithmetic — and splitting it
//! by construct would duplicate the shared fixtures and oracle probes.

use std::rc::Rc;

use beam_alloc::{Heap, RefcBinary};
use beam_term::prelude::*;
use beam_common::BeamError;
use beam_ets::prelude::*;
use beam_ets::select::expr::set_arith_hook;

/// A table of the atoms the specs below reference. `true`/`false` must be
/// interned: guard results are the atoms they name.
fn atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    for name in [
        "_", "$_", "$$", "$0", "$x", "$1", "$2", "$3", "true", "false", "ok", "x", "a", "b", "c",
        "element", "abs", "size", "hd", "tl", "length", "self", "andalso", "orelse", "not", "and",
        "or", "const", "is_integer", "is_atom", "is_boolean", "is_list", "is_number",
        "is_tuple", "is_float", "is_map", "is_pid", "is_port", "is_reference", "is_function",
        "is_binary", "is_bitstring", "is_foo", "=:=", "=/=", "==", "/=", "<", "=<", ">", ">=",
        "+", "-", "*", "/", "div", "rem", "band", "bor", "bxor", "bsl", "bsr", "$", "$1x", "$01",
        "$5", "map_get", "is_map_key", "map_size", "tuple_size", "byte_size", "bit_size", "float",
        "trunc", "round", "list_to_tuple",
    ] {
        atoms.intern(name);
    }
    atoms
}

/// [`atoms`] plus the `EXIT` atom a failed match-spec *body* expression is replaced
/// by (`exec::exit_term` — the C `FAIL_TERM`).
///
/// `EXIT` is deliberately **not** in the shared [`atoms`]: the crate reports the
/// typed `badarg` capability gap when the caller's table has not interned it, and
/// the pre-existing `arithmetic_outside_the_small_integer_subset_is_a_typed_error`
/// pins that non-interned outcome for a body `{div,1,0}`/`{rem,1,0}` (see the note
/// there). A real runtime always interns `EXIT` (`beam_rt::boot::CORE_ATOMS`), so
/// the `EXIT`-body tests below use this table instead of perturbing the shared one.
fn atoms_with_exit() -> AtomTable {
    let mut atoms = atoms();
    let _ = atoms.intern("EXIT");
    atoms
}

fn at(atoms: &AtomTable, name: &str) -> Eterm {
    make_atom(atoms.lookup(name).expect("interned by `atoms`"))
}

fn tup(heap: &mut Heap, elements: &[Eterm]) -> Eterm {
    heap.tuple(elements).expect("test heap has room")
}

fn list(heap: &mut Heap, elements: &[Eterm]) -> Eterm {
    let mut acc = NIL;
    for element in elements.iter().rev() {
        acc = heap.cons(*element, acc).expect("test heap has room");
    }
    acc
}

fn new_table(kind: TableKind) -> Table {
    let mut opts = TableOptions::new();
    let _ = opts.set_kind(kind);
    Table::new(TableId::next(), opts).expect("table")
}

/// A `{K, V, W}` object.
fn object(heap: &mut Heap, k: i64, v: Eterm, w: i64) -> Eterm {
    tup(heap, &[make_small(k), v, make_small(w)])
}

/// `[ {Head, Guards, Body} ]` for a single clause.
fn spec1(heap: &mut Heap, head: Eterm, guards: &[Eterm], body: &[Eterm]) -> Eterm {
    let guard_list = list(heap, guards);
    let body_list = list(heap, body);
    let clause = tup(heap, &[head, guard_list, body_list]);
    list(heap, &[clause])
}

/// A match-spec call term `{Name, Args…}` (the guard/body function-call form).
fn call(heap: &mut Heap, name: Eterm, args: &[Eterm]) -> Eterm {
    let mut elements = Vec::with_capacity(args.len() + 1);
    elements.push(name);
    elements.extend_from_slice(args);
    tup(heap, &elements)
}

/// A float term on the test heap (the float-operand probe).
fn float(heap: &mut Heap, value: f64) -> Eterm {
    // SAFETY: `alloc_words(2)` hands back the two exclusively-owned, writable words
    // a float occupies (`[HEADER_FLONUM, bits]`); `write_float` writes both before
    // the term escapes, and the heap outlives it (it is never reset here).
    unsafe {
        let words = heap.alloc_words(2).expect("test heap has room for a float");
        write_float(words, value)
    }
}

/// Compile a one-clause `{Head, Guards, Body}` spec and run `select` with it.
fn select_one(
    table: &Table,
    head: Eterm,
    guards: &[Eterm],
    body: &[Eterm],
    atoms: &AtomTable,
    heap: &mut Heap,
) -> beam_common::Result<Vec<Eterm>> {
    let spec = compile(spec1(heap, head, guards, body), atoms).expect("the spec compiles");
    table.select(&spec, atoms, heap)
}

/// Assert that a refused arithmetic result is the crate's typed `unsupported`
/// raise naming match-spec arithmetic.
fn assert_unsupported_arithmetic(err: BeamError, what: &str) {
    let raise = err
        .into_raise()
        .expect("the arithmetic refusal is an explicit raise");
    assert_eq!(raise.class, beam_common::ExceptionClass::Error, "{what}");
    assert!(
        raise
            .reason
            .as_atom()
            .expect("the reason is an atom")
            .starts_with("unsupported_ets: match-spec arithmetic"),
        "the raise names the unsupported arithmetic ({what})"
    );
}

const KINDS: [TableKind; 4] = [
    TableKind::Set,
    TableKind::OrderedSet,
    TableKind::Bag,
    TableKind::DuplicateBag,
];

// ---------------------------------------------------------------------------
// Heads
// ---------------------------------------------------------------------------

#[test]
fn whole_object_body_selects_every_object_in_every_kind() {
    // `ets:select(T, [{'_',[],['$_']}])` returns every object (all 4 kinds).
    for kind in KINDS {
        let atoms = atoms();
        let mut heap = Heap::new(8192).expect("test heap");
        let mut table = new_table(kind);
        table
            .insert(object(&mut heap, 1, at(&atoms, "a"), 10), &atoms)
            .unwrap();
        table
            .insert(object(&mut heap, 2, at(&atoms, "b"), 20), &atoms)
            .unwrap();
        let spec = compile(spec1(&mut heap, at(&atoms, "_"), &[], &[at(&atoms, "$_")]), &atoms)
            .unwrap();
        let got = table.select(&spec, &atoms, &mut heap).unwrap();
        assert_eq!(got.len(), 2, "kind {kind:?}");
    }
}

#[test]
fn nested_list_head_binds_and_the_body_value_is_the_last_expression() {
    // erl: `ets:select(T,[{ {['$1','$2'],'_'}, [], ['$2','$1'] }])` on
    //     `{[1,2],5}` and `{[3,4],6}` -> `[1,3]`: a body is a *sequence* whose
    //     value is the value of the LAST expression (`'$1'`), so a two-element body
    //     yields one result per matched object, not two.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::OrderedSet);
    let first = list(&mut heap, &[make_small(1), make_small(2)]);
    let second = list(&mut heap, &[make_small(3), make_small(4)]);
    table
        .insert(tup(&mut heap, &[first, make_small(5)]), &atoms)
        .unwrap();
    table
        .insert(tup(&mut heap, &[second, make_small(6)]), &atoms)
        .unwrap();

    let inner = list(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let head = tup(&mut heap, &[inner, at(&atoms, "_")]);
    let spec = compile(
        spec1(
            &mut heap,
            head,
            &[],
            &[at(&atoms, "$2"), at(&atoms, "$1")],
        ),
        &atoms,
    )
    .unwrap();

    let got = table.select(&spec, &atoms, &mut heap).unwrap();
    assert_eq!(got, vec![make_small(1), make_small(3)]);
}

#[test]
fn repeated_variable_requires_the_elements_to_be_equal() {
    // erl: head `{'$1','$1'}` on `{1,1}`,`{2,3}`,`{4,4}` -> the first and third.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::OrderedSet);
    for (k, v) in [(1, 1), (2, 3), (4, 4)] {
        table
            .insert(tup(&mut heap, &[make_small(k), make_small(v)]), &atoms)
            .unwrap();
    }
    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$1")]);
    let spec = compile(spec1(&mut heap, head, &[], &[at(&atoms, "$_")]), &atoms).unwrap();
    let got = table.select(&spec, &atoms, &mut heap).unwrap();
    assert_eq!(got.len(), 2);
}

#[test]
fn head_dollar_underscore_and_dollar_dollar_are_literal_atoms() {
    // erl, with `D = '$_'`, `DD = '$$'` and `T = [{1,'$_'},{2,'$$'},{3,9}]`:
    //   ets:select(T, [{{1,D},[],[true]}])   -> [true]
    //   ets:match_object(T, {1,D})           -> [{1,'$_'}]
    //   ets:select(T, [{{1,DD},[],[true]}])  -> []
    //   ets:match_object(T, {1,9})           -> []
    //   ets:select(T, [{{3,9},[],[true]}])   -> [true]
    //   ets:match_spec_run([D,1], ets:match_spec_compile([{D,[],[true]}])) -> [true]
    // So in a HEAD `'$_'`/`'$$'` are ordinary literal atoms compared with exact
    // `=:=` — they are special only in a *body*; the C `dmc_one_term` emits
    // `matchEq` for any atom that is not `'_'`/`'$N'` (`erl_db_util.c:4117-4121`).
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::Set);
    table
        .insert(object(&mut heap, 1, at(&atoms, "$_"), 0), &atoms)
        .unwrap();
    table
        .insert(object(&mut heap, 2, at(&atoms, "$$"), 0), &atoms)
        .unwrap();
    table
        .insert(object(&mut heap, 3, make_small(9), 0), &atoms)
        .unwrap();

    // A nested head `{1,'$_'}` matches exactly the object whose corresponding
    // element IS that same atom (`{1,'$_',0}`), not the other rows.
    let dollar = at(&atoms, "$_");
    let nested = tup(&mut heap, &[make_small(1), dollar, at(&atoms, "_")]);
    let got = select_one(&table, nested, &[], &[at(&atoms, "$_")], &atoms, &mut heap).unwrap();
    assert_eq!(got.len(), 1, "head `{{1,'$_'}}` matches `{{1,'$_',0}}`");
    assert!(eq(got[0], object(&mut heap, 1, dollar, 0), &atoms));

    // …but NOT `{1,9}` (the literal is `=:=`, not a wildcard) nor `{1,'$$'}`.
    let other_literal = tup(&mut heap, &[make_small(1), make_small(9), at(&atoms, "_")]);
    let got = select_one(&table, other_literal, &[], &[at(&atoms, "$_")], &atoms, &mut heap)
        .unwrap();
    assert!(got.is_empty(), "head `{{1,9}}` must not match `{{1,'$_',0}}`");

    let other_atom = tup(&mut heap, &[make_small(1), at(&atoms, "$$"), at(&atoms, "_")]);
    let got = select_one(&table, other_atom, &[], &[at(&atoms, "$_")], &atoms, &mut heap).unwrap();
    assert!(
        got.is_empty(),
        "head `{{1,'$$'}}` must not match `{{1,'$_',0}}`"
    );

    // `'$$'` is likewise a literal: it matches `{2,'$$',0}` and nothing else.
    let nested = tup(&mut heap, &[make_small(2), at(&atoms, "$$"), at(&atoms, "_")]);
    let got = select_one(&table, nested, &[], &[at(&atoms, "$_")], &atoms, &mut heap).unwrap();
    assert_eq!(got.len(), 1, "head `{{2,'$$'}}` matches `{{2,'$$',0}}`");

    // A plain integer literal still compares exactly (`{3,9}` matches `{3,9,0}`).
    let nested = tup(&mut heap, &[make_small(3), make_small(9), at(&atoms, "_")]);
    let got = select_one(&table, nested, &[], &[at(&atoms, "$_")], &atoms, &mut heap).unwrap();
    assert_eq!(got.len(), 1, "head `{{3,9}}` matches `{{3,9,0}}`");

    // The runner shares the head decoder: the oracle's
    // `ets:match_spec_run(['$_',1], ets:match_spec_compile([{'$_',[],[true]}]))`
    // is `[true]` — the head `'$_'` matches the literal element `'$_'` and drops
    // `1`.
    let spec = spec1(&mut heap, dollar, &[], &[at(&atoms, "true")]);
    let input = list(&mut heap, &[dollar, make_small(1)]);
    let got = match_spec_run(input, spec, &atoms, &mut heap).expect("run");
    assert_eq!(got.len(), 1, "head `'$_'` matches the literal element `'$_'`");
    assert!(eq(got[0], at(&atoms, "true"), &atoms));
}

#[test]
fn head_variable_zero_binds_and_a_malformed_var_is_a_literal() {
    // erl: `ets:match_object(T, {'$0','_','_'})` -> every object (`$0` IS a
    // variable); `{'$x','_','_'}` -> `[]` (`$x` is an ordinary literal atom).
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::Set);
    table
        .insert(object(&mut heap, 1, at(&atoms, "a"), 10), &atoms)
        .unwrap();

    let head = tup(&mut heap, &[at(&atoms, "$0"), at(&atoms, "_"), at(&atoms, "_")]);
    let spec = compile(spec1(&mut heap, head, &[], &[at(&atoms, "$_")]), &atoms).unwrap();
    assert_eq!(table.select(&spec, &atoms, &mut heap).unwrap().len(), 1);

    let head = tup(&mut heap, &[at(&atoms, "$x"), at(&atoms, "_"), at(&atoms, "_")]);
    let spec = compile(spec1(&mut heap, head, &[], &[at(&atoms, "$_")]), &atoms).unwrap();
    assert!(table.select(&spec, &atoms, &mut heap).unwrap().is_empty());
}

#[test]
fn literal_head_compares_exactly() {
    // erl: `ets:match_object(T, {1,'_','_'})` -> `[{1,a,10}]` only.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::OrderedSet);
    for k in 1..=3 {
        table
            .insert(object(&mut heap, k, at(&atoms, "a"), k * 10), &atoms)
            .unwrap();
    }
    let head = tup(&mut heap, &[make_small(1), at(&atoms, "_"), at(&atoms, "_")]);
    let spec = compile(spec1(&mut heap, head, &[], &[at(&atoms, "$_")]), &atoms).unwrap();
    let got = table.select(&spec, &atoms, &mut heap).unwrap();
    assert_eq!(got.len(), 1);
    assert!(eq(got[0], object(&mut heap, 1, at(&atoms, "a"), 10), &atoms));
}

// ---------------------------------------------------------------------------
// Guards
// ---------------------------------------------------------------------------

#[test]
fn guard_comparisons() {
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::OrderedSet);
    for k in 1..=3 {
        table
            .insert(object(&mut heap, k, at(&atoms, "a"), k * 10), &atoms)
            .unwrap();
    }
    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2"), at(&atoms, "$3")]);
    let body = [at(&atoms, "$1")];

    // `>=` keeps the objects whose third element is at least 20 -> keys 2 and 3.
    for (op, threshold, expected) in [
        (">=", 20, 2usize),
        (">", 20, 1),
        ("<", 20, 1),
        ("=<", 10, 1),
        ("=:=", 20, 1),
        ("=/=", 20, 2),
        ("==", 20, 1),
        ("/=", 20, 2),
    ] {
        let guard = tup(
            &mut heap,
            &[at(&atoms, op), at(&atoms, "$3"), make_small(threshold)],
        );
        let spec = compile(spec1(&mut heap, head, &[guard], &body), &atoms).unwrap();
        let got = table.select(&spec, &atoms, &mut heap).unwrap();
        assert_eq!(got.len(), expected, "operator {op}");
    }
}

#[test]
fn guard_type_tests() {
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::OrderedSet);
    table
        .insert(tup(&mut heap, &[make_small(1), at(&atoms, "a")]), &atoms)
        .unwrap();
    table
        .insert(tup(&mut heap, &[make_small(2), make_small(7)]), &atoms)
        .unwrap();
    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);

    // `is_integer('$2')` keeps only `{2,7}`; `is_atom('$2')` only `{1,a}`.
    for (test, expected) in [("is_integer", 1usize), ("is_atom", 1)] {
        let guard = tup(&mut heap, &[at(&atoms, test), at(&atoms, "$2")]);
        let spec = compile(spec1(&mut heap, head, &[guard], &[at(&atoms, "$1")]), &atoms).unwrap();
        assert_eq!(
            table.select(&spec, &atoms, &mut heap).unwrap().len(),
            expected,
            "test {test}"
        );
    }

    // `not(is_integer('$2'))` flips it.
    let inner = tup(&mut heap, &[at(&atoms, "is_integer"), at(&atoms, "$2")]);
    let guard = tup(&mut heap, &[at(&atoms, "not"), inner]);
    let spec = compile(spec1(&mut heap, head, &[guard], &[at(&atoms, "$1")]), &atoms).unwrap();
    assert_eq!(table.select(&spec, &atoms, &mut heap).unwrap().len(), 1);
}

#[test]
fn guard_andalso_orelse_and_element_size_hd_tl_length() {
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::OrderedSet);
    for k in 1..=3 {
        table
            .insert(object(&mut heap, k, at(&atoms, "a"), k * 10), &atoms)
            .unwrap();
    }
    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2"), at(&atoms, "$3")]);
    let body = [at(&atoms, "$1")];

    // `andalso` — `$3 > 5` and `$3 < 25` -> keys 1 and 2.
    let less = tup(&mut heap, &[at(&atoms, ">"), at(&atoms, "$3"), make_small(5)]);
    let more = tup(&mut heap, &[at(&atoms, "<"), at(&atoms, "$3"), make_small(25)]);
    let guard = tup(&mut heap, &[at(&atoms, "andalso"), less, more]);
    let spec = compile(spec1(&mut heap, head, &[guard], &body), &atoms).unwrap();
    assert_eq!(table.select(&spec, &atoms, &mut heap).unwrap().len(), 2);

    // `orelse` — `$1 =:= 1` or `$1 =:= 3` -> keys 1 and 3.
    let one = tup(&mut heap, &[at(&atoms, "=:="), at(&atoms, "$1"), make_small(1)]);
    let three = tup(&mut heap, &[at(&atoms, "=:="), at(&atoms, "$1"), make_small(3)]);
    let guard = tup(&mut heap, &[at(&atoms, "orelse"), one, three]);
    let spec = compile(spec1(&mut heap, head, &[guard], &body), &atoms).unwrap();
    assert_eq!(table.select(&spec, &atoms, &mut heap).unwrap().len(), 2);

    // `element/2` — `element(3, {{'$1','$2','$3'}}) =:= 20` -> key 2.
    let rebuilt = tup(
        &mut heap,
        &[at(&atoms, "$1"), at(&atoms, "$2"), at(&atoms, "$3")],
    );
    let wrapped = tup(&mut heap, &[rebuilt]);
    let element = tup(
        &mut heap,
        &[at(&atoms, "element"), make_small(3), wrapped],
    );
    let guard = tup(
        &mut heap,
        &[at(&atoms, "=:="), element, make_small(20)],
    );
    let spec = compile(spec1(&mut heap, head, &[guard], &body), &atoms).unwrap();
    assert_eq!(table.select(&spec, &atoms, &mut heap).unwrap().len(), 1);

    // `size/1` — the rebuilt tuple has arity 3, so every object matches.
    let rebuilt = tup(
        &mut heap,
        &[at(&atoms, "$1"), at(&atoms, "$2"), at(&atoms, "$3")],
    );
    let wrapped = tup(&mut heap, &[rebuilt]);
    let size = tup(&mut heap, &[at(&atoms, "size"), wrapped]);
    let guard = tup(&mut heap, &[at(&atoms, "=:="), size, make_small(3)]);
    let spec = compile(spec1(&mut heap, head, &[guard], &body), &atoms).unwrap();
    assert_eq!(table.select(&spec, &atoms, &mut heap).unwrap().len(), 3);

    // `hd/1` over a list argument: `hd(['$1', 7]) =:= 2` -> key 2.
    let list_arg = list(&mut heap, &[at(&atoms, "$1"), make_small(7)]);
    let hd = tup(&mut heap, &[at(&atoms, "hd"), list_arg]);
    let guard = tup(&mut heap, &[at(&atoms, "=:="), hd, make_small(2)]);
    let spec = compile(spec1(&mut heap, head, &[guard], &body), &atoms).unwrap();
    assert_eq!(table.select(&spec, &atoms, &mut heap).unwrap().len(), 1);

    // `length(['$1', 7]) =:= 2` -> every object.
    let list_arg = list(&mut heap, &[at(&atoms, "$1"), make_small(7)]);
    let length = tup(&mut heap, &[at(&atoms, "length"), list_arg]);
    let guard = tup(&mut heap, &[at(&atoms, "=:="), length, make_small(2)]);
    let spec = compile(spec1(&mut heap, head, &[guard], &body), &atoms).unwrap();
    assert_eq!(table.select(&spec, &atoms, &mut heap).unwrap().len(), 3);

    // `tl(['$1', 7]) =:= [7]` -> every object.
    let list_arg = list(&mut heap, &[at(&atoms, "$1"), make_small(7)]);
    let tl = tup(&mut heap, &[at(&atoms, "tl"), list_arg]);
    let want = list(&mut heap, &[make_small(7)]);
    let guard = tup(&mut heap, &[at(&atoms, "=:="), tl, want]);
    let spec = compile(spec1(&mut heap, head, &[guard], &body), &atoms).unwrap();
    assert_eq!(table.select(&spec, &atoms, &mut heap).unwrap().len(), 3);
}

#[test]
fn multi_clause_first_match_wins() {
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::OrderedSet);
    table
        .insert(tup(&mut heap, &[make_small(1), make_small(5)]), &atoms)
        .unwrap();
    table
        .insert(tup(&mut heap, &[make_small(2), make_small(6)]), &atoms)
        .unwrap();

    let head_one = tup(&mut heap, &[make_small(1), at(&atoms, "_")]);
    let ok_body = list(&mut heap, &[at(&atoms, "ok")]);
    let first = tup(&mut heap, &[head_one, NIL, ok_body]);
    let x_body = list(&mut heap, &[at(&atoms, "x")]);
    let second = tup(&mut heap, &[at(&atoms, "_"), NIL, x_body]);
    let spec = compile(list(&mut heap, &[first, second]), &atoms).unwrap();
    let got = table.select(&spec, &atoms, &mut heap).unwrap();
    assert_eq!(got.len(), 2);
    assert!(eq(got[0], at(&atoms, "ok"), &atoms));
    assert!(eq(got[1], at(&atoms, "x"), &atoms));
}

// ---------------------------------------------------------------------------
// Bodies
// ---------------------------------------------------------------------------

#[test]
fn construct_body_result_values() {
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::OrderedSet);
    table
        .insert(tup(&mut heap, &[make_small(1), make_small(10)]), &atoms)
        .unwrap();
    table
        .insert(tup(&mut heap, &[make_small(2), make_small(20)]), &atoms)
        .unwrap();

    let inner = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let wrapped = tup(&mut heap, &[inner]);
    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let spec = compile(
        spec1(&mut heap, head, &[], &[wrapped]),
        &atoms,
    )
    .unwrap();
    let got = table.select(&spec, &atoms, &mut heap).unwrap();
    assert_eq!(got.len(), 2);
    let want = tup(&mut heap, &[make_small(1), make_small(10)]);
    assert!(eq(got[0], want, &atoms));
}

#[test]
fn constant_and_variable_bodies_alias_instead_of_copying() {
    // erl: `['$_']` returns the object; `['$1']` the key; `[ok]` the atom `ok`.
    // Each is a *single-element* body (a body's value is its last expression), so
    // the three cases are separate specs on the one matched object.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::Set);
    table
        .insert(object(&mut heap, 1, at(&atoms, "a"), 10), &atoms)
        .unwrap();

    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2"), at(&atoms, "$3")]);

    let whole = compile(spec1(&mut heap, head, &[], &[at(&atoms, "$_")]), &atoms).unwrap();
    let got = table.select(&whole, &atoms, &mut heap).unwrap();
    assert_eq!(got.len(), 1);
    assert!(eq(got[0], object(&mut heap, 1, at(&atoms, "a"), 10), &atoms));

    let key = compile(spec1(&mut heap, head, &[], &[at(&atoms, "$1")]), &atoms).unwrap();
    assert_eq!(
        table.select(&key, &atoms, &mut heap).unwrap(),
        vec![make_small(1)]
    );

    let constant = compile(spec1(&mut heap, head, &[], &[at(&atoms, "ok")]), &atoms).unwrap();
    let got = table.select(&constant, &atoms, &mut heap).unwrap();
    assert_eq!(got.len(), 1);
    assert!(eq(got[0], at(&atoms, "ok"), &atoms));
}

// ---------------------------------------------------------------------------
// Arithmetic
//
// Every expected value below is the system `erl`'s (OTP 29 / erts-17.1):
// `ets:select(T, [{'_', [], [EXPR]}])` / `ets:select(T, [{H, [GUARD], EXPR}])`.
// ---------------------------------------------------------------------------

#[test]
fn binary_arithmetic_body_results_match_the_oracle() {
    // erl: `ets:select(T, [{'_', [], [{'OP', 30, 6}]}])` on any object, one result:
    //   '+' -> [36]   '-' -> [24]    '*' -> [180]  'div' -> [5]   'rem' -> [0]
    //   'band' -> [6] 'bor' -> [30]  'bxor' -> [24] 'bsl' -> [1920] 'bsr' -> [0]
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::Set);
    table
        .insert(object(&mut heap, 1, at(&atoms, "a"), 10), &atoms)
        .unwrap();

    for (op, expected) in [
        ("+", 36i64),
        ("-", 24),
        ("*", 180),
        ("div", 5),
        ("rem", 0),
        ("band", 6),
        ("bor", 30),
        ("bxor", 24),
        ("bsl", 1920),
        ("bsr", 0),
    ] {
        let body = call(&mut heap, at(&atoms, op), &[make_small(30), make_small(6)]);
        let got = select_one(&table, at(&atoms, "_"), &[], &[body], &atoms, &mut heap).unwrap();
        assert_eq!(got, vec![make_small(expected)], "operator {op}");
    }
}

#[test]
fn arithmetic_over_bound_variables_unary_and_abs_match_the_oracle() {
    // erl: head `{'$1','$2','$3'}` on `{7,6,-3}`, body `{OP, ARGS}`:
    //   {'+','$2','$3'} -> [3]     {'-','$2','$3'} -> [9]
    //   {'*','$2','$3'} -> [-18]   {'div','$2','$3'} -> [-2]
    //   {'rem','$2','$3'} -> [0]   {'band','$2','$3'} -> [4]
    //   {'bor','$2','$3'} -> [-1]  {'bxor','$2','$3'} -> [-5]
    //   {'+','$3'} -> [-3]  {'-','$3'} -> [3]  {'abs','$3'} -> [3]
    //   {'+',5} -> [5]      {'-',5} -> [-5]    {'abs',-5} -> [5]
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::OrderedSet);
    table
        .insert(
            tup(&mut heap, &[make_small(7), make_small(6), make_small(-3)]),
            &atoms,
        )
        .unwrap();
    let head = tup(
        &mut heap,
        &[at(&atoms, "$1"), at(&atoms, "$2"), at(&atoms, "$3")],
    );
    let v2 = at(&atoms, "$2");
    let v3 = at(&atoms, "$3");

    for (op, args, expected) in [
        ("+", vec![v2, v3], 3i64),
        ("-", vec![v2, v3], 9),
        ("*", vec![v2, v3], -18),
        ("div", vec![v2, v3], -2),
        ("rem", vec![v2, v3], 0),
        ("band", vec![v2, v3], 4),
        ("bor", vec![v2, v3], -1),
        ("bxor", vec![v2, v3], -5),
        ("+", vec![v3], -3),
        ("-", vec![v3], 3),
        ("abs", vec![v3], 3),
    ] {
        let body = call(&mut heap, at(&atoms, op), &args);
        let got = select_one(&table, head, &[], &[body], &atoms, &mut heap).unwrap();
        assert_eq!(got, vec![make_small(expected)], "operator {op}");
    }

    for (op, operand, expected) in [("+", 5i64, 5i64), ("-", 5, -5), ("abs", -5, 5)] {
        let body = call(&mut heap, at(&atoms, op), &[make_small(operand)]);
        let got = select_one(&table, head, &[], &[body], &atoms, &mut heap).unwrap();
        assert_eq!(
            got,
            vec![make_small(expected)],
            "constant operator {op} on {operand}"
        );
    }
}

#[test]
fn arithmetic_in_a_guard_matches_the_oracle() {
    // erl: on `{7,6,-3}`, guard list `[G]` with body `['$_']`:
    //   {'=:=',{'+','$2','$3'},3}   -> the object
    //   {'=:=',{'div','$2','$3'},-2} -> the object
    //   {'=:=',{'band','$2','$3'},5} -> [] (the comparison is false)
    //   {div,1,0}                    -> [] (the guard raises, so the clause fails)
    //   {'<',{'+','$2','$3'},0}      -> [] (3 is not < 0)
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::OrderedSet);
    table
        .insert(
            tup(&mut heap, &[make_small(7), make_small(6), make_small(-3)]),
            &atoms,
        )
        .unwrap();
    let head = tup(
        &mut heap,
        &[at(&atoms, "$1"), at(&atoms, "$2"), at(&atoms, "$3")],
    );
    let body = [at(&atoms, "$_")];

    let cmp = |heap: &mut Heap, op: &str, args: &[Eterm], rhs: i64| -> Eterm {
        let expr = call(heap, at(&atoms, op), args);
        tup(
            heap,
            &[at(&atoms, "=:="), expr, make_small(rhs)],
        )
    };

    let sum = cmp(&mut heap, "+", &[at(&atoms, "$2"), at(&atoms, "$3")], 3);
    assert_eq!(
        select_one(&table, head, &[sum], &body, &atoms, &mut heap)
            .unwrap()
            .len(),
        1,
        "'$2' + '$3' is 3"
    );

    let quotient = cmp(&mut heap, "div", &[at(&atoms, "$2"), at(&atoms, "$3")], -2);
    assert_eq!(
        select_one(&table, head, &[quotient], &body, &atoms, &mut heap)
            .unwrap()
            .len(),
        1,
        "'$2' div '$3' is -2"
    );

    let wrong = cmp(&mut heap, "band", &[at(&atoms, "$2"), at(&atoms, "$3")], 5);
    assert!(select_one(&table, head, &[wrong], &body, &atoms, &mut heap)
        .unwrap()
        .is_empty());

    // A guard that raises inside the arithmetic (`1 div 0`) fails its clause.
    let raising = call(
        &mut heap,
        at(&atoms, "div"),
        &[make_small(1), make_small(0)],
    );
    assert!(select_one(&table, head, &[raising], &body, &atoms, &mut heap)
        .unwrap()
        .is_empty());

    // A guard that evaluates to a non-boolean (`3`) also fails its clause.
    let non_boolean = call(
        &mut heap,
        at(&atoms, "+"),
        &[at(&atoms, "$2"), at(&atoms, "$3")],
    );
    assert!(
        select_one(&table, head, &[non_boolean], &body, &atoms, &mut heap)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn arithmetic_outside_the_small_integer_subset_is_a_typed_error() {
    // erl: `/` ALWAYS yields a float (`4/2` -> 2.0); `MAX_SMALL + 1` and
    //      `MAX_SMALL * 2` overflow into bignums; `1 + 1.5` is 2.5. None of those is
    //      answerable as a small integer by the default hook, so each is the typed
    //      `unsupported_ets` raise rather than a wrong result. `1 div 0`/`1 rem 0`
    //      raise `badarg` in the oracle.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::Set);
    table
        .insert(object(&mut heap, 1, at(&atoms, "a"), 10), &atoms)
        .unwrap();
    let head = at(&atoms, "_");

    // `/` — the oracle's result is always a float.
    let div = call(&mut heap, at(&atoms, "/"), &[make_small(4), make_small(2)]);
    let err = select_one(&table, head, &[], &[div], &atoms, &mut heap).unwrap_err();
    assert_unsupported_arithmetic(err, "'/'");

    // Overflow into a bignum (`MAX_SMALL + 1`).
    let overflow = call(
        &mut heap,
        at(&atoms, "+"),
        &[make_small(MAX_SMALL), make_small(1)],
    );
    let err = select_one(&table, head, &[], &[overflow], &atoms, &mut heap).unwrap_err();
    assert_unsupported_arithmetic(err, "MAX_SMALL + 1");

    // Overflow through `*` (`MAX_SMALL * 2`).
    let overflow = call(
        &mut heap,
        at(&atoms, "*"),
        &[make_small(MAX_SMALL), make_small(2)],
    );
    let err = select_one(&table, head, &[], &[overflow], &atoms, &mut heap).unwrap_err();
    assert_unsupported_arithmetic(err, "MAX_SMALL * 2");

    // Overflow through `bsl` (the shifted-out bits must not be dropped: a bare
    // `<<` would answer `0` here instead of the oracle's bignum).
    let overflow = call(
        &mut heap,
        at(&atoms, "bsl"),
        &[make_small(2), make_small(62)],
    );
    let err = select_one(&table, head, &[], &[overflow], &atoms, &mut heap).unwrap_err();
    assert_unsupported_arithmetic(err, "2 bsl 62");

    // A float operand (`1 + 1.5` is 2.5 in the oracle).
    let fractional = float(&mut heap, 1.5);
    let float_sum = call(&mut heap, at(&atoms, "+"), &[make_small(1), fractional]);
    let err = select_one(&table, head, &[], &[float_sum], &atoms, &mut heap).unwrap_err();
    assert_unsupported_arithmetic(err, "1 + 1.5");

    // …and unary `+` on a float is refused the same way (it is not the identity
    // this hook can answer with).
    let unary_float = call(&mut heap, at(&atoms, "+"), &[fractional]);
    let err = select_one(&table, head, &[], &[unary_float], &atoms, &mut heap).unwrap_err();
    assert_unsupported_arithmetic(err, "unary + on 1.5");

    // `div`/`rem` by zero — the oracle raises `badarg`.
    for op in ["div", "rem"] {
        let body = call(&mut heap, at(&atoms, op), &[make_small(1), make_small(0)]);
        let err = select_one(&table, head, &[], &[body], &atoms, &mut heap).unwrap_err();
        assert!(
            matches!(err, BeamError::BadArg),
            "{op} by zero is badarg"
        );
    }
}

#[test]
fn shift_counts_negative_reverse_direction_and_bsr_saturates() {
    // erl (OTP 29 / erts-17.1), quoted exactly:
    //   1 bsl -1  -> 0      1 bsr -1   -> 2
    //   8 bsr -1  -> 16     8 bsl -1   -> 4
    //   1 bsr 100 -> 0      -1 bsr 100 -> -1
    //   1 bsr 63  -> 0      -1 bsr 63  -> -1
    //   0 bsl 100 -> 0      0 bsr -100 -> 0
    //   2 bsl 57  -> 288230376151711744   288230376151711744 bsr 57 -> 2
    // and, through the match spec (`{'OP', A, B}` as the body):
    //   {'bsl',8,-1} -> [4]   {'bsr',1,-1} -> [2]   {'bsl',0,64} -> [0]
    // i.e. the C `erts_shift` negates the `bsr` count and lets the resulting sign
    // pick the direction (so `bsl`/`bsr` merely swap for a negative count), and
    // saturates a right shift of a 60-bit small by 59 or more to the sign's
    // `0`/`-1` (`erl_arith.c:196-247`).
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::Set);
    table
        .insert(object(&mut heap, 1, at(&atoms, "a"), 10), &atoms)
        .unwrap();
    let head = at(&atoms, "_");

    for (op, a, b, expected) in [
        ("bsl", 1i64, -1i64, 0i64),
        ("bsr", 1, -1, 2),
        ("bsr", 8, -1, 16),
        ("bsl", 8, -1, 4),
        ("bsr", 1, 100, 0),
        ("bsr", -1, 100, -1),
        ("bsr", 1, 63, 0),
        ("bsr", -1, 63, -1),
        ("bsl", 0, 100, 0),
        ("bsr", 0, -100, 0),
        ("bsl", 2, 57, 1 << 58),
        ("bsr", 1 << 58, 57, 2),
    ] {
        let body = call(&mut heap, at(&atoms, op), &[make_small(a), make_small(b)]);
        let got = select_one(&table, head, &[], &[body], &atoms, &mut heap).unwrap();
        assert_eq!(
            got,
            vec![make_small(expected)],
            "{a} {op} {b} must be {expected}"
        );
    }

    // A left shift that leaves the 60-bit small range stays the typed
    // `unsupported` (`1 bsl 100` is a bignum in the oracle), and so does a `bsr`
    // with a negative count that big (`1 bsr -100` is exactly `1 bsl 100`).
    for (op, a, b) in [("bsl", 1i64, 100i64), ("bsr", 1, -100)] {
        let body = call(&mut heap, at(&atoms, op), &[make_small(a), make_small(b)]);
        let err = select_one(&table, head, &[], &[body], &atoms, &mut heap).unwrap_err();
        assert_unsupported_arithmetic(err, &format!("{a} {op} {b} widens to a bignum"));
    }
}

/// A custom arithmetic hook that answers `/` with the truncated integer quotient,
/// so a test can prove [`set_arith_hook`] really is consulted (and that everything
/// it does not handle can defer to [`DefaultArithHook`]).
#[derive(Debug)]
struct TruncatingDivHook;

impl ArithHook for TruncatingDivHook {
    fn eval(
        &self,
        op: ArithOp,
        args: &[Eterm],
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> beam_common::Result<Eterm> {
        match (op, args) {
            (ArithOp::Div, [left, right]) => {
                Ok(make_small(signed_value(*left) / signed_value(*right)))
            }
            _ => DefaultArithHook.eval(op, args, atoms, heap),
        }
    }
}

#[test]
fn a_custom_arith_hook_takes_over_and_the_default_returns() {
    // The seam `eval` reaches for: with no hook, `/` is outside the default's
    // small-integer subset (its Erlang result is a float); with one installed, `/`
    // is whatever the hook answers; clearing it restores the default.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::Set);
    table
        .insert(object(&mut heap, 1, at(&atoms, "a"), 10), &atoms)
        .unwrap();
    let head = at(&atoms, "_");
    let body = call(&mut heap, at(&atoms, "/"), &[make_small(4), make_small(2)]);

    let err = select_one(&table, head, &[], &[body], &atoms, &mut heap).unwrap_err();
    assert_unsupported_arithmetic(err, "'/' with no hook");

    set_arith_hook(Some(Rc::new(TruncatingDivHook) as Rc<dyn ArithHook>));
    // The custom hook answers `/`; everything else still goes through the default.
    let got = select_one(&table, head, &[], &[body], &atoms, &mut heap).unwrap();
    assert_eq!(got, vec![make_small(2)]);
    let product = call(
        &mut heap,
        at(&atoms, "*"),
        &[make_small(30), make_small(6)],
    );
    assert_eq!(
        select_one(&table, head, &[], &[product], &atoms, &mut heap).unwrap(),
        vec![make_small(180)]
    );

    set_arith_hook(None);
    let err = select_one(&table, head, &[], &[body], &atoms, &mut heap).unwrap_err();
    assert_unsupported_arithmetic(err, "'/' after clearing the hook");
}

#[test]
fn malformed_arithmetic_arities_are_badarg() {
    // erl rejects every one of these at compile time (`ets:select/2` -> badarg):
    // `{'+'}`, `{'+',1,2,3}`, `{'div',1}`, `{'div',1,2,3}`, `{abs,1,2}`.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    for (op, args) in [
        ("+", vec![]),
        ("+", vec![make_small(1), make_small(2), make_small(3)]),
        ("div", vec![make_small(1)]),
        ("div", vec![make_small(1), make_small(2), make_small(3)]),
        ("abs", vec![make_small(1), make_small(2)]),
    ] {
        let body = call(&mut heap, at(&atoms, op), &args);
        let spec = spec1(&mut heap, at(&atoms, "_"), &[], &[body]);
        assert!(
            matches!(compile(spec, &atoms), Err(BeamError::BadArg)),
            "{op}/{} must be badarg",
            args.len()
        );
    }
}

// ---------------------------------------------------------------------------
// select_delete / select_count
// ---------------------------------------------------------------------------

#[test]
fn select_delete_and_count_only_act_on_a_true_body_result() {
    // erl: guard `{'>','$2',15}` with body `[true]` deletes 2 objects and
    //      `ets:select_count/2` returns 2; body `[false]` returns 0.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::OrderedSet);
    for k in 1..=3 {
        table
            .insert(object(&mut heap, k, at(&atoms, "a"), k * 10), &atoms)
            .unwrap();
    }
    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2"), at(&atoms, "$3")]);
    let guard = tup(&mut heap, &[at(&atoms, ">"), at(&atoms, "$3"), make_small(15)]);

    let false_clause = compile(
        spec1(&mut heap, head, &[guard.clone()], &[at(&atoms, "false")]),
        &atoms,
    )
    .unwrap();
    assert_eq!(table.select_count(&false_clause, &atoms, &mut heap).unwrap(), 0);
    assert_eq!(table.select_delete(&false_clause, &atoms, &mut heap).unwrap(), 0);

    let true_clause = compile(
        spec1(&mut heap, head, &[guard.clone()], &[at(&atoms, "true")]),
        &atoms,
    )
    .unwrap();
    assert_eq!(table.select_count(&true_clause, &atoms, &mut heap).unwrap(), 2);

    let before = table.memory();
    assert_eq!(table.select_delete(&true_clause, &atoms, &mut heap).unwrap(), 2);
    assert_eq!(table.size(), 1);
    assert!(table.member(make_small(1), &atoms));
    // The deleted objects' table-owned storage is reclaimed (`TermStore::free`).
    // `Table::memory` is `TermStore::words` (the *live* word count, which `free`
    // lowers) plus the interior's index storage — `0` for an `ordered_set` — so two
    // deletions strictly shrink the figure rather than merely not growing it.
    assert!(
        table.memory() < before,
        "deleting must reclaim the removed objects' storage"
    );

    // A non-`true` body result never deletes (it is not `[true]`).
    let one = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2"), at(&atoms, "$3")]);
    let var_clause = compile(spec1(&mut heap, one, &[], &[at(&atoms, "$1"), at(&atoms, "$2")]), &atoms)
        .unwrap();
    assert_eq!(table.select_delete(&var_clause, &atoms, &mut heap).unwrap(), 0);
    assert_eq!(table.size(), 1);
}

#[test]
fn a_nested_list_guard_never_matches() {
    // erl: the guard list is flat — each element must evaluate to `true`; a nested
    //      list builds a list value, so `[[Cond]]` matches nothing.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::Set);
    table
        .insert(object(&mut heap, 1, at(&atoms, "a"), 10), &atoms)
        .unwrap();
    let inner = list(&mut heap, &[at(&atoms, "true")]);
    let guards = list(&mut heap, &[inner]);
    let body = list(&mut heap, &[at(&atoms, "$_")]);
    let clause = tup(&mut heap, &[at(&atoms, "_"), guards, body]);
    let spec = compile(list(&mut heap, &[clause]), &atoms).unwrap();
    assert!(table.select(&spec, &atoms, &mut heap).unwrap().is_empty());
}

#[test]
fn a_raising_guard_skips_the_clause() {
    // erl: `{'=:=',{element,9,'$1'},1}` raises, so the clause is skipped (`[]`).
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::Set);
    table
        .insert(object(&mut heap, 1, at(&atoms, "a"), 10), &atoms)
        .unwrap();
    let element = tup(
        &mut heap,
        &[at(&atoms, "element"), make_small(9), at(&atoms, "$1")],
    );
    let guard = tup(&mut heap, &[at(&atoms, "=:="), element, make_small(1)]);
    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "_"), at(&atoms, "_")]);
    let spec = compile(spec1(&mut heap, head, &[guard], &[at(&atoms, "$_")]), &atoms).unwrap();
    assert!(table.select(&spec, &atoms, &mut heap).unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// match_object / match / match_delete
// ---------------------------------------------------------------------------

#[test]
fn match_object_returns_whole_objects_and_match_returns_bindings() {
    // erl: `ets:match_object(T, {'$1','_','$3'})` -> every object;
    //      `ets:match(T, {'$1','_','$3'})` -> `[[1,10],[2,20],[3,30]]`
    //      (bound `'$N'` ascending).
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::OrderedSet);
    for k in 1..=3 {
        table
            .insert(object(&mut heap, k, at(&atoms, "a"), k * 10), &atoms)
            .unwrap();
    }
    let pattern = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "_"), at(&atoms, "$3")]);

    let objects = table.match_object(pattern, &atoms, &mut heap).unwrap();
    assert_eq!(objects.len(), 3);

    let bindings = table.match_(pattern, &atoms, &mut heap).unwrap();
    assert_eq!(bindings.len(), 3);
    for (index, object) in [(1i64, 10i64), (2, 20), (3, 30)] {
        let want = list(&mut heap, &[make_small(index), make_small(object)]);
        assert!(eq(bindings[(index - 1) as usize], want, &atoms));
    }

    // A bare variable pattern: `match/2` -> one single-element binding list per
    // object; `match_object/2` -> every object.
    let _ = pattern;
    let all = table.match_object(at(&atoms, "$1"), &atoms, &mut heap).unwrap();
    assert_eq!(all.len(), 3);
    let seeded = table.match_(at(&atoms, "$1"), &atoms, &mut heap).unwrap();
    assert_eq!(seeded.len(), 3);

    // `'_'` binds nothing: `match/2` -> an empty binding list per object (erl:
    // `[[],[],[]]`).
    let none = table.match_(at(&atoms, "_"), &atoms, &mut heap).unwrap();
    assert_eq!(none.len(), 3);
    for binding in none {
        assert!(is_nil(binding));
    }
}

#[test]
fn match_delete_removes_the_matching_objects() {
    // erl: `ets:match_delete(T, {1,'_','_'})` leaves `[{3,c,30},{2,b,20}]`.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::OrderedSet);
    for k in 1..=3 {
        table
            .insert(object(&mut heap, k, at(&atoms, "a"), k * 10), &atoms)
            .unwrap();
    }
    let pattern = tup(&mut heap, &[make_small(1), at(&atoms, "_"), at(&atoms, "_")]);
    assert_eq!(table.match_delete(pattern, &atoms, &mut heap).unwrap(), 1);
    assert_eq!(table.size(), 2);
    assert!(!table.member(make_small(1), &atoms));

    // A bare `'$1'` pattern deletes everything.
    assert_eq!(
        table
            .match_delete(at(&atoms, "$1"), &atoms, &mut heap)
            .unwrap(),
        2
    );
    assert!(table.is_empty());
}

// ---------------------------------------------------------------------------
// Malformed specs
// ---------------------------------------------------------------------------

#[test]
fn malformed_specs_are_badarg() {
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    // A spec that is not a list.
    assert!(matches!(compile(make_small(1), &atoms), Err(BeamError::BadArg)));

    // A clause that is not a 3-tuple.
    let two = tup(&mut heap, &[at(&atoms, "_"), NIL]);
    assert!(matches!(
        compile(list(&mut heap, &[two]), &atoms),
        Err(BeamError::BadArg)
    ));

    // An empty body list (`erl` -> badarg).
    let clause = tup(&mut heap, &[at(&atoms, "_"), NIL, NIL]);
    assert!(matches!(
        compile(list(&mut heap, &[clause]), &atoms),
        Err(BeamError::BadArg)
    ));

    // An unknown guard call (`erl` -> badarg).
    let guard = tup(&mut heap, &[at(&atoms, "is_foo"), at(&atoms, "$1")]);
    let head = tup(&mut heap, &[at(&atoms, "$1")]);
    let guard_list = list(&mut heap, &[guard]);
    let body_list = list(&mut heap, &[at(&atoms, "$_")]);
    let clause = tup(&mut heap, &[head, guard_list, body_list]);
    assert!(matches!(
        compile(list(&mut heap, &[clause]), &atoms),
        Err(BeamError::BadArg)
    ));

    // An unbound `'$N'` in the body (`erl` -> badarg).
    let body_list = list(&mut heap, &[at(&atoms, "$2")]);
    let clause = tup(&mut heap, &[at(&atoms, "_"), NIL, body_list]);
    assert!(matches!(
        compile(list(&mut heap, &[clause]), &atoms),
        Err(BeamError::BadArg)
    ));

    // A bare tuple body element is a *call*, not a tuple (`erl` -> badarg).
    let body_tuple = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let body = list(&mut heap, &[body_tuple]);
    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let clause = tup(&mut heap, &[head, NIL, body]);
    assert!(matches!(
        compile(list(&mut heap, &[clause]), &atoms),
        Err(BeamError::BadArg)
    ));
}

// ---------------------------------------------------------------------------
// fun2ms-shaped table-level battery
//
// `ets:fun2ms/1` (the `ms_transform` parse transform) lowers a `fun` pattern into a
// match spec of the exact shape `[{{Head}, [Guards], [Body]}]`, using `'_'` for a
// wildcard, `'$N'` for a bound variable, `'$_'` for the whole object and `'$$'` for
// every binding. Each spec below is quoted verbatim from the oracle's own transform
// output and each expectation is the oracle's `erl` result (OTP 29 / erts-17.1),
// `ets:select/2` and friends run on the same `{K,V,W}` table. An `ordered_set`'s
// vector is asserted in order; a `set`'s navigation order is unspecified, so its
// results are compared as a sorted value set.
// ---------------------------------------------------------------------------

/// Assert a select-style result equals `want` element-by-element. `Eterm`'s
/// `PartialEq` is bitwise (it does not compare boxed tuples/lists), so boxed
/// results are compared with the crate's structural `eq`.
fn assert_terms(got: &[Eterm], want: &[Eterm], atoms: &AtomTable, what: &str) {
    assert_eq!(got.len(), want.len(), "{what}: result length");
    for (index, (g, w)) in got.iter().zip(want).enumerate() {
        assert!(eq(*g, *w, atoms), "{what}: element {index}");
    }
}

/// The 60-bit small integers of `terms`, ascending (a `set`'s order is
/// unspecified, so its results are compared as a sorted value set).
fn small_ints(terms: &[Eterm]) -> Vec<i64> {
    let mut values: Vec<i64> = terms.iter().map(|term| signed_value(*term)).collect();
    values.sort_unstable();
    values
}

/// The battery's shared `ordered_set` holding `{1,a,10}`, `{2,b,20}`, `{3,a,30}`.
fn fun2ms_table(atoms: &AtomTable, heap: &mut Heap) -> Table {
    let mut table = new_table(TableKind::OrderedSet);
    for (key, value, third) in [(1i64, "a", 10i64), (2, "b", 20), (3, "a", 30)] {
        let object = object(heap, key, at(atoms, value), third);
        table.insert(object, atoms).unwrap();
    }
    table
}

/// The same three `{K,V,W}` objects, ascending by key.
fn fun2ms_objects(heap: &mut Heap, atoms: &AtomTable) -> Vec<Eterm> {
    vec![
        object(heap, 1, at(atoms, "a"), 10),
        object(heap, 2, at(atoms, "b"), 20),
        object(heap, 3, at(atoms, "a"), 30),
    ]
}

#[test]
fn fun2ms_guard_on_the_key_returning_the_key() {
    // `ets:fun2ms(fun({K,_V,_W}) when K < 3 -> K end)`
    //   == [{{'$1','$2','$3'},[{'<','$1',3}],['$1']}]
    // erl: on [{1,a,10},{2,b,20},{3,a,30}] -> [1,2] (`K < 3` keeps keys 1 and 2).
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let table = fun2ms_table(&atoms, &mut heap);

    let head = tup(
        &mut heap,
        &[at(&atoms, "$1"), at(&atoms, "$2"), at(&atoms, "$3")],
    );
    let guard = tup(
        &mut heap,
        &[at(&atoms, "<"), at(&atoms, "$1"), make_small(3)],
    );
    let spec = compile(
        spec1(&mut heap, head, &[guard], &[at(&atoms, "$1")]),
        &atoms,
    )
    .unwrap();
    assert_eq!(
        table.select(&spec, &atoms, &mut heap).unwrap(),
        vec![make_small(1), make_small(2)]
    );

    // The same spec on a `set` — the order is unspecified, so compare the value set.
    let mut set = new_table(TableKind::Set);
    for object in fun2ms_objects(&mut heap, &atoms) {
        set.insert(object, &atoms).unwrap();
    }
    let got = set.select(&spec, &atoms, &mut heap).unwrap();
    assert_eq!(small_ints(&got), vec![1, 2]);
}

#[test]
fn fun2ms_nested_tuple_head_with_a_reordering_body() {
    // `ets:fun2ms(fun({_P,{Q,R},_}) -> {R,Q} end)`
    //   == [{{'$1',{'$2','$3'},'_'},[],[{{'$3','$2'}}]}]
    // erl: on {1,{10,20},100} -> [{20,10}] (the body reorders the bound elements).
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::OrderedSet);

    let inner = tup(&mut heap, &[make_small(10), make_small(20)]);
    let object = tup(&mut heap, &[make_small(1), inner, make_small(100)]);
    table.insert(object, &atoms).unwrap();

    let nested = tup(&mut heap, &[at(&atoms, "$2"), at(&atoms, "$3")]);
    let head = tup(&mut heap, &[at(&atoms, "$1"), nested, at(&atoms, "_")]);
    let swapped = tup(&mut heap, &[at(&atoms, "$3"), at(&atoms, "$2")]);
    let body = tup(&mut heap, &[swapped]);
    let spec = compile(spec1(&mut heap, head, &[], &[body]), &atoms).unwrap();

    let want = tup(&mut heap, &[make_small(20), make_small(10)]);
    let got = table.select(&spec, &atoms, &mut heap).unwrap();
    assert_terms(&got, &[want], &atoms, "nested-tuple head reorder");
}

#[test]
fn fun2ms_list_element_pattern_in_the_head() {
    // `ets:fun2ms(fun({_P,[Q,R],_}) -> [Q,R] end)`
    //   == [{{'$1',['$2','$3'],'_'},[],[['$2','$3']]}]
    // erl: on {1,[10,20],100} -> [[10,20]] (a list element in the head binds `'$2'`
    //      and `'$3'` element-wise).
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::OrderedSet);

    let elements = list(&mut heap, &[make_small(10), make_small(20)]);
    let object = tup(&mut heap, &[make_small(1), elements, make_small(100)]);
    table.insert(object, &atoms).unwrap();

    let pattern = list(&mut heap, &[at(&atoms, "$2"), at(&atoms, "$3")]);
    let head = tup(&mut heap, &[at(&atoms, "$1"), pattern, at(&atoms, "_")]);
    let result = list(&mut heap, &[at(&atoms, "$2"), at(&atoms, "$3")]);
    let spec = compile(spec1(&mut heap, head, &[], &[result]), &atoms).unwrap();

    let want = list(&mut heap, &[make_small(10), make_small(20)]);
    let got = table.select(&spec, &atoms, &mut heap).unwrap();
    assert_terms(&got, &[want], &atoms, "list-element pattern");
}

#[test]
fn fun2ms_dollar_dollar_body_yields_every_binding() {
    // In a *body* `'$$'` is the list of every bound head variable, ascending.
    // erl: `ets:select(T, [{{'$1','$2','$3'},[],['$$']}])`
    //        -> [[1,a,10],[2,b,20],[3,a,30]]
    //      `ets:select(T, [{{'_','_','_'},[],['$$']}])` -> [[],[],[]].
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let table = fun2ms_table(&atoms, &mut heap);

    let head = tup(
        &mut heap,
        &[at(&atoms, "$1"), at(&atoms, "$2"), at(&atoms, "$3")],
    );
    let spec = compile(spec1(&mut heap, head, &[], &[at(&atoms, "$$")]), &atoms).unwrap();
    let got = table.select(&spec, &atoms, &mut heap).unwrap();

    let first = list(&mut heap, &[make_small(1), at(&atoms, "a"), make_small(10)]);
    let second = list(&mut heap, &[make_small(2), at(&atoms, "b"), make_small(20)]);
    let third = list(&mut heap, &[make_small(3), at(&atoms, "a"), make_small(30)]);
    assert_terms(&got, &[first, second, third], &atoms, "'$$' body");

    // With no head variables there is nothing to collect: `'$$'` is `[]`.
    let wildcard = tup(
        &mut heap,
        &[at(&atoms, "_"), at(&atoms, "_"), at(&atoms, "_")],
    );
    let spec = compile(spec1(&mut heap, wildcard, &[], &[at(&atoms, "$$")]), &atoms).unwrap();
    let got = table.select(&spec, &atoms, &mut heap).unwrap();
    assert_eq!(got.len(), 3);
    for term in got {
        assert!(
            is_nil(term),
            "an unbound head yields the empty binding list"
        );
    }
}

#[test]
fn fun2ms_dollar_underscore_body_yields_the_whole_object() {
    // `'$_'` is the whole matched object.
    // erl: `ets:select(T, [{{'$1','$2','$3'},[],['$_']}])`
    //        -> [{1,a,10},{2,b,20},{3,a,30}].
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let table = fun2ms_table(&atoms, &mut heap);

    let head = tup(
        &mut heap,
        &[at(&atoms, "$1"), at(&atoms, "$2"), at(&atoms, "$3")],
    );
    let spec = compile(spec1(&mut heap, head, &[], &[at(&atoms, "$_")]), &atoms).unwrap();
    let got = table.select(&spec, &atoms, &mut heap).unwrap();
    let want = fun2ms_objects(&mut heap, &atoms);
    assert_terms(&got, &want, &atoms, "'$_' body");
}

#[test]
fn fun2ms_guard_conjunctions_comma_and_andalso() {
    // A comma in the source guard becomes two guard-*list* elements (a
    // conjunction); an `andalso` stays nested inside one clause guard.
    // `ets:fun2ms(fun({K,V,_W}) when V =:= a, K > 1 -> K end)`
    //   == [{{'$1','$2','$3'},[{'=:=','$2',a},{'>','$1',1}],['$1']}]
    // `ets:fun2ms(fun({K,V,_W}) when V =:= a andalso K > 1 -> K end)`
    //   == [{{'$1','$2','$3'},[{'andalso',{'=:=','$2',a},{'>','$1',1}}],['$1']}]
    // erl: both select -> [3] (`V` is `a` for keys 1 and 3, but only key 3 is > 1,
    //      so the comma is a conjunction, not an `orelse`).
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let table = fun2ms_table(&atoms, &mut heap);

    let head = tup(
        &mut heap,
        &[at(&atoms, "$1"), at(&atoms, "$2"), at(&atoms, "$3")],
    );
    let is_a = tup(
        &mut heap,
        &[at(&atoms, "=:="), at(&atoms, "$2"), at(&atoms, "a")],
    );
    let over_one = tup(
        &mut heap,
        &[at(&atoms, ">"), at(&atoms, "$1"), make_small(1)],
    );
    let body = [at(&atoms, "$1")];

    // Comma: two guard-list elements.
    let comma = compile(spec1(&mut heap, head, &[is_a, over_one], &body), &atoms).unwrap();
    assert_eq!(
        table.select(&comma, &atoms, &mut heap).unwrap(),
        vec![make_small(3)]
    );

    // `andalso`: one nested guard element.
    let andalso = tup(&mut heap, &[at(&atoms, "andalso"), is_a, over_one]);
    let spec = compile(spec1(&mut heap, head, &[andalso], &body), &atoms).unwrap();
    assert_eq!(
        table.select(&spec, &atoms, &mut heap).unwrap(),
        vec![make_small(3)]
    );
}

#[test]
fn fun2ms_multi_clause_spec_is_first_match_wins() {
    // `ets:fun2ms(fun({K,V,W}) when V =:= a -> {K,V,W};
    //                ({_K,_V,W2}) -> W2 end)`
    //   == [{{'$1','$2','$3'},[{'=:=','$2',a}],[{{'$1','$2','$3'}}]},
    //       {{'$1','$2','$3'},[],['$3']}]
    // erl: on [{1,a,10},{2,b,20},{3,a,30}] -> [{1,a,10},20,{3,a,30}]: objects 1 and 3
    //      take the first clause (body = the rebuilt object); object 2's guard fails
    //      and falls through to the fallback clause (body = `'$3'` = 20).
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let table = fun2ms_table(&atoms, &mut heap);

    let head = tup(
        &mut heap,
        &[at(&atoms, "$1"), at(&atoms, "$2"), at(&atoms, "$3")],
    );
    let guard = tup(
        &mut heap,
        &[at(&atoms, "=:="), at(&atoms, "$2"), at(&atoms, "a")],
    );
    let whole = tup(
        &mut heap,
        &[at(&atoms, "$1"), at(&atoms, "$2"), at(&atoms, "$3")],
    );
    let wrapped = tup(&mut heap, &[whole]);
    let guards = list(&mut heap, &[guard]);
    let first_body = list(&mut heap, &[wrapped]);
    let clause_one = tup(&mut heap, &[head, guards, first_body]);
    let fallback_body = list(&mut heap, &[at(&atoms, "$3")]);
    let clause_two = tup(&mut heap, &[head, NIL, fallback_body]);
    let spec = compile(list(&mut heap, &[clause_one, clause_two]), &atoms).unwrap();

    let got = table.select(&spec, &atoms, &mut heap).unwrap();
    let first = object(&mut heap, 1, at(&atoms, "a"), 10);
    let third = object(&mut heap, 3, at(&atoms, "a"), 30);
    assert_terms(
        &got,
        &[first, make_small(20), third],
        &atoms,
        "first clause wins",
    );
}

#[test]
fn fun2ms_filter_spec_select_count_counts_only_true() {
    // `ets:fun2ms(fun({_K,V,_W}) when V =:= a -> true end)`
    //   == [{{'$1','$2','$3'},[{'=:=','$2',a}],[true]}]
    // erl: `ets:select_count(T, ...)` -> 2. `ets:select_count/2` counts only the
    //      objects whose *body* evaluates to `true`, so the same spec with the body
    //      `'$_'` selects 2 objects yet counts 0.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let table = fun2ms_table(&atoms, &mut heap);

    let head = tup(
        &mut heap,
        &[at(&atoms, "$1"), at(&atoms, "$2"), at(&atoms, "$3")],
    );
    let guard = tup(
        &mut heap,
        &[at(&atoms, "=:="), at(&atoms, "$2"), at(&atoms, "a")],
    );

    let filter = compile(
        spec1(&mut heap, head, &[guard], &[at(&atoms, "true")]),
        &atoms,
    )
    .unwrap();
    assert_eq!(table.select_count(&filter, &atoms, &mut heap).unwrap(), 2);

    let projected = compile(
        spec1(&mut heap, head, &[guard], &[at(&atoms, "$_")]),
        &atoms,
    )
    .unwrap();
    assert_eq!(
        table.select_count(&projected, &atoms, &mut heap).unwrap(),
        0
    );
    assert_eq!(
        table.select(&projected, &atoms, &mut heap).unwrap().len(),
        2
    );
}

#[test]
fn fun2ms_shaped_match_object_with_a_wildcard_first_element() {
    // `ets:match_object(T, {'_','$1'})` is the bare-pattern shorthand
    // `[{{'_','$1'},[],['$_']}]`. erl: on [{1,a},{2,b},{3,a}]
    //   -> [{1,a},{2,b},{3,a}] (`'_'` matches any first element; the 2-tuple
    //      pattern matches only 2-tuples).
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::OrderedSet);
    for (key, value) in [(1, "a"), (2, "b"), (3, "a")] {
        let object = tup(&mut heap, &[make_small(key), at(&atoms, value)]);
        table.insert(object, &atoms).unwrap();
    }

    let pattern = tup(&mut heap, &[at(&atoms, "_"), at(&atoms, "$1")]);
    let got = table.match_object(pattern, &atoms, &mut heap).unwrap();
    let want = vec![
        tup(&mut heap, &[make_small(1), at(&atoms, "a")]),
        tup(&mut heap, &[make_small(2), at(&atoms, "b")]),
        tup(&mut heap, &[make_small(3), at(&atoms, "a")]),
    ];
    assert_terms(&got, &want, &atoms, "match_object {'_','$1'}");
}

#[test]
fn fun2ms_filter_spec_drives_select_delete() {
    // `ets:fun2ms(fun({_K,V,_W}) when V =:= a -> true end)`
    //   == [{{'$1','$2','$3'},[{'=:=','$2',a}],[true]}]
    // erl: `ets:select_delete(T, ...)` on [{1,a,10},{2,b,20},{3,a,30}] -> 2,
    //      leaving [{2,b,20}].
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = fun2ms_table(&atoms, &mut heap);

    let head = tup(
        &mut heap,
        &[at(&atoms, "$1"), at(&atoms, "$2"), at(&atoms, "$3")],
    );
    let guard = tup(
        &mut heap,
        &[at(&atoms, "=:="), at(&atoms, "$2"), at(&atoms, "a")],
    );
    let spec = compile(
        spec1(&mut heap, head, &[guard], &[at(&atoms, "true")]),
        &atoms,
    )
    .unwrap();

    assert_eq!(table.select_delete(&spec, &atoms, &mut heap).unwrap(), 2);
    assert_eq!(table.size(), 1);
    let remaining = table.tab2list();
    let want = object(&mut heap, 2, at(&atoms, "b"), 20);
    assert_terms(&remaining, &[want], &atoms, "the surviving object");
}

// ===========================================================================
// Match-spec completeness battery
//
// Every expectation below is the system `erl`'s (OTP 29 / erts-17.1), probed with
// `ets:select/2` (and, for the divergent gaps, `ets:match_spec_run/2`); the probe
// result is quoted beside each assertion.
// ===========================================================================

/// A byte-aligned binary term on the test heap.
fn binary(heap: &mut Heap, bytes: &[u8]) -> Eterm {
    let words = heap_bits_size_words(bytes.len() * 8);
    // SAFETY: `alloc_words(words)` hands back exactly the `heap_bits_size_words(n_bits)`
    // words `write_binary` writes; the heap outlives the term (it is never reset here).
    unsafe {
        let hp = heap
            .alloc_words(words)
            .expect("test heap has room for a binary");
        write_binary(hp, bytes).expect("a byte-aligned binary")
    }
}

/// A flat-map (`#{…}`, at most [`MAP_SMALL_MAP_LIMIT`] keys) term on the test heap.
fn flat_map(heap: &mut Heap, pairs: &[(Eterm, Eterm)], atoms: &AtomTable) -> Eterm {
    let words = map_words_capacity(pairs.len());
    // SAFETY: `alloc_words(map_words_capacity(len))` reserves the exact upper bound
    // `write_map` writes; every returned word is written by the call and the heap
    // outlives the term.
    unsafe {
        let hp = heap.alloc_words(words).expect("test heap has room for a map");
        write_map(hp, pairs, atoms).expect("a well-formed flat map")
    }
}

/// The stored `{K,V,W}` objects reordered by their small-integer key (element 0), so a
/// hash kind's unspecified iteration order can be compared as a value multiset.
fn sorted_by_key(mut objects: Vec<Eterm>) -> Vec<Eterm> {
    objects.sort_by_key(|object| {
        // SAFETY: every element is a stored `{K,V,W}` tuple, so element 0 is live.
        unsafe { signed_value(tuple_element(*object, 0)) }
    });
    objects
}

/// The `match/2` binding lists reordered by their small-integer key (the list head).
fn sorted_bindings_by_key(mut bindings: Vec<Eterm>) -> Vec<Eterm> {
    bindings.sort_by_key(|binding| {
        // SAFETY: every element is a non-empty binding list built by `match/2`.
        unsafe { signed_value(car(*binding)) }
    });
    bindings
}

#[test]
fn guard_connectives_short_circuit_versus_strict_match_the_oracle() {
    // oracle (OTP 29 / erts-17.1) on a 2-object `{1,10}`/`{2,20}` table, guard list `[G]`:
    //   {'orelse',true,{'hd',5}}   -> both  (`orelse` short-circuits at `true`, so the
    //                                        raising operand is never evaluated)
    //   {'or',true,{'hd',5}}       -> []    (`or` is STRICT: it evaluates `{hd,5}`, which
    //                                        raises, so the clause fails)
    //   {'andalso',false,{'hd',5}} -> []    {'and',false,{'hd',5}} -> [] (both fail)
    //   {'andalso',true,true} -> both       {'and',true,true} -> both
    //   {'andalso',true,false} -> []        {'andalso',false,true} -> []
    //   {'orelse',false,false} -> []        {'orelse',false,true} -> both
    //   {'or',false,false} -> []            {'or',false,true} -> both
    //   {'not',true} -> []                  {'not',false} -> both
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::OrderedSet);
    table
        .insert(tup(&mut heap, &[make_small(1), make_small(10)]), &atoms)
        .unwrap();
    table
        .insert(tup(&mut heap, &[make_small(2), make_small(20)]), &atoms)
        .unwrap();
    let head = tup(&mut heap, &[at(&atoms, "_"), at(&atoms, "_")]);
    let body = [at(&atoms, "$_")];

    let yes = || at(&atoms, "true");
    let no = || at(&atoms, "false");
    let raising = |heap: &mut Heap| call(heap, at(&atoms, "hd"), &[make_small(5)]);

    let count = |heap: &mut Heap, guard: Eterm| -> usize {
        select_one(&table, head, &[guard], &body, &atoms, heap)
            .unwrap()
            .len()
    };

    let t = at(&atoms, "true");
    let f = at(&atoms, "false");
    // `orelse` short-circuits; `or` is strict and meets the raise.
    let raising_term = raising(&mut heap);
    let g = call(&mut heap, at(&atoms, "orelse"), &[t, raising_term]);
    assert_eq!(count(&mut heap, g), 2, "orelse short-circuits at true");
    let raising_term = raising(&mut heap);
    let g = call(&mut heap, at(&atoms, "or"), &[t, raising_term]);
    assert_eq!(count(&mut heap, g), 0, "or is strict and the operand raises");
    // `andalso`/`and` with a false first operand both fail the clause.
    let raising_term = raising(&mut heap);
    let g = call(&mut heap, at(&atoms, "andalso"), &[f, raising_term]);
    assert_eq!(count(&mut heap, g), 0, "andalso false");
    let raising_term = raising(&mut heap);
    let g = call(&mut heap, at(&atoms, "and"), &[f, raising_term]);
    assert_eq!(count(&mut heap, g), 0, "and false");
    // Truth tables.
    for (name, left, right, expected) in [
        ("andalso", true, true, 2usize),
        ("andalso", true, false, 0),
        ("andalso", false, true, 0),
        ("and", true, true, 2),
        ("orelse", false, false, 0),
        ("orelse", false, true, 2),
        ("or", false, false, 0),
        ("or", false, true, 2),
    ] {
        let l = if left { t } else { f };
        let r = if right { t } else { f };
        let g = call(&mut heap, at(&atoms, name), &[l, r]);
        assert_eq!(count(&mut heap, g), expected, "{name}({left},{right})");
    }
    // `not/1`.
    let g = call(&mut heap, at(&atoms, "not"), &[yes()]);
    assert_eq!(count(&mut heap, g), 0, "not true");
    let g = call(&mut heap, at(&atoms, "not"), &[no()]);
    assert_eq!(count(&mut heap, g), 2, "not false");
}

#[test]
fn comparison_operators_agree_on_exact_versus_arithmetic_for_int_and_float() {
    // oracle on `{1,1}`/`{2,1.0}` (ordered_set), guard `{Op,'$2',1}` body `['$1']`:
    //   '=:=' -> [1]    '=/=' -> [2]    '==' -> [1,2]   '/=' -> []
    //   '<'   -> []     '=<'  -> [1,2]  '>'  -> []      '>=' -> [1,2]
    // (`=:=`/`=/=` are exact, so the float `1.0` is distinct from the integer `1`; the
    //  arithmetic family treats them as equal, per `beam_term::compare`.)
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::OrderedSet);
    table
        .insert(tup(&mut heap, &[make_small(1), make_small(1)]), &atoms)
        .unwrap();
    let one_point_zero = float(&mut heap, 1.0);
    table
        .insert(tup(&mut heap, &[make_small(2), one_point_zero]), &atoms)
        .unwrap();
    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);

    for (op, expected) in [
        ("=:=", vec![1i64]),
        ("=/=", vec![2]),
        ("==", vec![1, 2]),
        ("/=", vec![]),
        ("<", vec![]),
        ("=<", vec![1, 2]),
        (">", vec![]),
        (">=", vec![1, 2]),
    ] {
        let guard = call(&mut heap, at(&atoms, op), &[at(&atoms, "$2"), make_small(1)]);
        let got = select_one(&table, head, &[guard], &[at(&atoms, "$1")], &atoms, &mut heap)
            .unwrap();
        let want: Vec<Eterm> = expected.iter().map(|k| make_small(*k)).collect();
        assert_eq!(got, want, "operator {op}");
    }
}

#[test]
fn every_guard_type_test_matches_the_oracle() {
    // oracle on `{1,a}`,`{2,7}`,`{3,1.5}`,`{4,{x}}`,`{5,[1,2]}`,`{6,<<"hi">>}`,
    // `{7,#{a=>1}}`,`{8,true}` (ordered_set), guard `{Test,'$2'}` body `['$1']`:
    //   is_atom -> [1,8]  is_integer -> [2]  is_float -> [3]  is_number -> [2,3]
    //   is_tuple -> [4]   is_list -> [5]     is_binary -> [6] is_bitstring -> [6]
    //   is_map -> [7]     is_boolean -> [8]
    //   is_pid/is_port/is_reference/is_function -> [] (no stored term is one of those)
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::OrderedSet);

    let atom_obj = tup(&mut heap, &[make_small(1), at(&atoms, "a")]);
    let int_obj = tup(&mut heap, &[make_small(2), make_small(7)]);
    let fractional = float(&mut heap, 1.5);
    let float_obj = tup(&mut heap, &[make_small(3), fractional]);
    let inner = tup(&mut heap, &[at(&atoms, "x")]);
    let tuple_obj = tup(&mut heap, &[make_small(4), inner]);
    let elements = list(&mut heap, &[make_small(1), make_small(2)]);
    let list_obj = tup(&mut heap, &[make_small(5), elements]);
    let bytes = binary(&mut heap, b"hi");
    let binary_obj = tup(&mut heap, &[make_small(6), bytes]);
    let map = flat_map(&mut heap, &[(at(&atoms, "a"), make_small(1))], &atoms);
    let map_obj = tup(&mut heap, &[make_small(7), map]);
    let bool_obj = tup(&mut heap, &[make_small(8), at(&atoms, "true")]);
    for object in [
        atom_obj, int_obj, float_obj, tuple_obj, list_obj, binary_obj, map_obj, bool_obj,
    ] {
        table.insert(object, &atoms).unwrap();
    }

    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    for (test, expected) in [
        ("is_atom", vec![1i64, 8]),
        ("is_integer", vec![2]),
        ("is_float", vec![3]),
        ("is_number", vec![2, 3]),
        ("is_tuple", vec![4]),
        ("is_list", vec![5]),
        ("is_binary", vec![6]),
        ("is_bitstring", vec![6]),
        ("is_map", vec![7]),
        ("is_boolean", vec![8]),
        ("is_pid", vec![]),
        ("is_port", vec![]),
        ("is_reference", vec![]),
        ("is_function", vec![]),
    ] {
        let guard = call(&mut heap, at(&atoms, test), &[at(&atoms, "$2")]);
        let got = select_one(&table, head, &[guard], &[at(&atoms, "$1")], &atoms, &mut heap)
            .unwrap();
        let want: Vec<Eterm> = expected.iter().map(|k| make_small(*k)).collect();
        assert_eq!(got, want, "type test {test}");
    }
}

#[test]
fn guard_bifs_element_abs_size_hd_tl_length_match_the_oracle() {
    // oracle on `{1,1}`/`{2,1.0}` (ordered_set):
    //   {'=:=',{abs,'$2'},1}          -> [1] (`abs(1)` is 1; `abs(1.0)` is 1.0 and
    //                                         `1.0 =:= 1` is false)
    //   {'=:=',{'hd','$2'},1}         -> []  (hd/1 on an integer raises -> clause skipped)
    //   {'=:=',{'tl','$2'},[]}        -> []  (tl/1 on an integer raises)
    //   {'=:=',{'length','$2'},1}     -> []
    //   {'=:=',{'size','$2'},1}       -> []
    //   {'=:=',{'element',1,'$2'},1}  -> []  (element/2 on an integer raises)
    //   {'=:=',{'size',{{'$1','$2'}}},2}              -> [1,2] (the rebuilt tuple is arity 2)
    //   {'=:=',{'element',2,{{'$1','$2'}}},'$2'}      -> [1,2]
    //   {'=:=',{'abs',-5},5}                          -> [1,2] (a constant small int)
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::OrderedSet);
    table
        .insert(tup(&mut heap, &[make_small(1), make_small(1)]), &atoms)
        .unwrap();
    let one_point_zero = float(&mut heap, 1.0);
    table
        .insert(tup(&mut heap, &[make_small(2), one_point_zero]), &atoms)
        .unwrap();
    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);

    for (name, arg, rhs) in [
        ("abs", at(&atoms, "$2"), make_small(1)),
        ("hd", at(&atoms, "$2"), make_small(1)),
        ("length", at(&atoms, "$2"), make_small(1)),
    ] {
        let call_term = call(&mut heap, at(&atoms, name), &[arg]);
        let guard = tup(&mut heap, &[at(&atoms, "=:="), call_term, rhs]);
        let got = select_one(&table, head, &[guard], &[at(&atoms, "$1")], &atoms, &mut heap)
            .unwrap();
        let want: Vec<Eterm> = if name == "abs" {
            vec![make_small(1)]
        } else {
            vec![]
        };
        assert_eq!(got, want, "guard {name}/1");
    }

    // `tl`/`size`/`element` all raise on an integer operand.
    let tl_call = call(&mut heap, at(&atoms, "tl"), &[at(&atoms, "$2")]);
    let guard = tup(&mut heap, &[at(&atoms, "=:="), tl_call, NIL]);
    assert!(select_one(&table, head, &[guard], &[at(&atoms, "$1")], &atoms, &mut heap)
        .unwrap()
        .is_empty());
    let size_call = call(&mut heap, at(&atoms, "size"), &[at(&atoms, "$2")]);
    let guard = tup(&mut heap, &[at(&atoms, "=:="), size_call, make_small(1)]);
    assert!(select_one(&table, head, &[guard], &[at(&atoms, "$1")], &atoms, &mut heap)
        .unwrap()
        .is_empty());
    let element_call = call(
        &mut heap,
        at(&atoms, "element"),
        &[make_small(1), at(&atoms, "$2")],
    );
    let guard = tup(&mut heap, &[at(&atoms, "=:="), element_call, make_small(1)]);
    assert!(select_one(&table, head, &[guard], &[at(&atoms, "$1")], &atoms, &mut heap)
        .unwrap()
        .is_empty());

    // The positive `element/2`/`size/1` cases over a freshly built tuple.
    let rebuilt = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let wrapped = tup(&mut heap, &[rebuilt]);
    let element_call = call(&mut heap, at(&atoms, "element"), &[make_small(2), wrapped]);
    let guard = tup(
        &mut heap,
        &[at(&atoms, "=:="), element_call, at(&atoms, "$2")],
    );
    let got = select_one(&table, head, &[guard], &[at(&atoms, "$1")], &atoms, &mut heap).unwrap();
    assert_eq!(got, vec![make_small(1), make_small(2)], "element/2 == '$2'");

    let rebuilt = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let wrapped = tup(&mut heap, &[rebuilt]);
    let size_call = call(&mut heap, at(&atoms, "size"), &[wrapped]);
    let guard = tup(&mut heap, &[at(&atoms, "=:="), size_call, make_small(2)]);
    let got = select_one(&table, head, &[guard], &[at(&atoms, "$1")], &atoms, &mut heap).unwrap();
    assert_eq!(got, vec![make_small(1), make_small(2)], "size/1 == 2");

    // A constant small-integer `abs/1` in a guard.
    let abs_call = call(&mut heap, at(&atoms, "abs"), &[make_small(-5)]);
    let guard = tup(&mut heap, &[at(&atoms, "=:="), abs_call, make_small(5)]);
    let got = select_one(&table, head, &[guard], &[at(&atoms, "$1")], &atoms, &mut heap).unwrap();
    assert_eq!(got, vec![make_small(1), make_small(2)], "abs(-5) == 5");
}

#[test]
fn const_yields_its_argument_in_guards_and_bodies() {
    // oracle: guard `{'=:=',{const,5},5}` -> both objects; body `[{const,{a,b}}]` ->
    // `[{a,b},{a,b}]`. `{const, Term}` is the only way a *map*/any literal reaches a
    // body without the expression decoder rejecting it.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::OrderedSet);
    table
        .insert(tup(&mut heap, &[make_small(1), make_small(10)]), &atoms)
        .unwrap();
    table
        .insert(tup(&mut heap, &[make_small(2), make_small(20)]), &atoms)
        .unwrap();
    let head = tup(&mut heap, &[at(&atoms, "_"), at(&atoms, "_")]);

    let constant = call(&mut heap, at(&atoms, "const"), &[make_small(5)]);
    let guard = tup(&mut heap, &[at(&atoms, "=:="), constant, make_small(5)]);
    let got = select_one(&table, head, &[guard], &[at(&atoms, "$_")], &atoms, &mut heap).unwrap();
    assert_eq!(got.len(), 2, "const(5) =:= 5");

    let pair = tup(&mut heap, &[at(&atoms, "a"), at(&atoms, "b")]);
    let constant = call(&mut heap, at(&atoms, "const"), &[pair]);
    let got = select_one(&table, head, &[], &[constant], &atoms, &mut heap).unwrap();
    let want = tup(&mut heap, &[at(&atoms, "a"), at(&atoms, "b")]);
    assert_terms(&got, &[want, want], &atoms, "const({a,b}) body");

    // A `{const, Map}` body yields the map (a *bare* map body is a documented gap — see
    // `documented_capability_gaps_diverge_from_the_oracle`).
    let map = flat_map(&mut heap, &[(at(&atoms, "a"), make_small(1))], &atoms);
    let constant = call(&mut heap, at(&atoms, "const"), &[map]);
    let got = select_one(&table, head, &[], &[constant], &atoms, &mut heap).unwrap();
    assert_eq!(got.len(), 2, "const(map) body");
    for term in got {
        // SAFETY: the body value is a live term; `is_map` classifies it through its
        // tag/header word only, which is sound for any value word.
        assert!(unsafe { is_map(term) }, "the body value is the map");
    }
}

#[test]
fn head_patterns_match_proper_improper_and_cons_lists() {
    // oracle on `{1,[10,20,30]}` (set):
    //   head {'_',['$1','$2','$3']} body ['$3'] -> [30]
    //   head {'_',[10,20|'$1']}     body ['$1'] -> [[30]]   (an improper head IS allowed:
    //                                                        `'$1'` binds the tail)
    //   head {'_',[10|'$1']}        body ['$1'] -> [[20,30]]
    //   head {'_',['$1'|'$2']}      body ['$1'] -> [10]
    //   head {'_',[10,20,30]}       body ['$_'] -> [{1,[10,20,30]}]
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::Set);
    let elements = list(&mut heap, &[make_small(10), make_small(20), make_small(30)]);
    let object = tup(&mut heap, &[make_small(1), elements]);
    table.insert(object, &atoms).unwrap();
    let first = at(&atoms, "$1");
    let second = at(&atoms, "$2");

    // A proper three-element list head binding each position.
    let pattern = list(&mut heap, &[first, second, at(&atoms, "$3")]);
    let head = tup(&mut heap, &[at(&atoms, "_"), pattern]);
    let got = select_one(&table, head, &[], &[at(&atoms, "$3")], &atoms, &mut heap).unwrap();
    assert_eq!(got, vec![make_small(30)], "['$1','$2','$3'] binds '$3'");

    // `[10,20|'$1']` — the tail `'$1'` binds `[30]`.
    let tail = heap.cons(make_small(20), first).expect("heap");
    let pattern = heap.cons(make_small(10), tail).expect("heap");
    let head = tup(&mut heap, &[at(&atoms, "_"), pattern]);
    let got = select_one(&table, head, &[], &[first], &atoms, &mut heap).unwrap();
    let want = list(&mut heap, &[make_small(30)]);
    assert_terms(&got, &[want], &atoms, "[10,20|'$1'] binds the tail");

    // `[10|'$1']` — the tail binds `[20,30]`.
    let pattern = heap.cons(make_small(10), first).expect("heap");
    let head = tup(&mut heap, &[at(&atoms, "_"), pattern]);
    let got = select_one(&table, head, &[], &[first], &atoms, &mut heap).unwrap();
    let want = list(&mut heap, &[make_small(20), make_small(30)]);
    assert_terms(&got, &[want], &atoms, "[10|'$1'] binds the tail");

    // `['$1'|'$2']` — the head binds `10`.
    let pattern = heap.cons(first, second).expect("heap");
    let head = tup(&mut heap, &[at(&atoms, "_"), pattern]);
    let got = select_one(&table, head, &[], &[first], &atoms, &mut heap).unwrap();
    assert_eq!(got, vec![make_small(10)], "['$1'|'$2'] binds the head");

    // An exact literal list head matches the whole object.
    let pattern = list(
        &mut heap,
        &[make_small(10), make_small(20), make_small(30)],
    );
    let head = tup(&mut heap, &[at(&atoms, "_"), pattern]);
    let got = select_one(&table, head, &[], &[at(&atoms, "$_")], &atoms, &mut heap).unwrap();
    assert_eq!(got.len(), 1, "an exact list head matches");
}

#[test]
fn construct_bodies_build_fresh_tuples_lists_and_their_combination() {
    // oracle on `{1,[10,20]}` (ordered_set):
    //   body [{{'$2','$1'}}]        -> [{[10,20],1}]
    //   body [{{'$1',['$2','$2']}}] -> [{1,[[10,20],[10,20]]}]
    //   body [{{{'$1'},['$2']}}]    -> badarg (a nested construct needs `{{…}}` at each
    //                                          level; a bare `{'$1'}` is a *call*)
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::OrderedSet);
    let elements = list(&mut heap, &[make_small(10), make_small(20)]);
    let object = tup(&mut heap, &[make_small(1), elements]);
    table.insert(object, &atoms).unwrap();
    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);

    // `{{'$2','$1'}}` — a tuple construct from the two bound variables.
    let rebuilt = tup(&mut heap, &[at(&atoms, "$2"), at(&atoms, "$1")]);
    let wrapped = tup(&mut heap, &[rebuilt]);
    let got = select_one(&table, head, &[], &[wrapped], &atoms, &mut heap).unwrap();
    let elements = list(&mut heap, &[make_small(10), make_small(20)]);
    let want = tup(&mut heap, &[elements, make_small(1)]);
    assert_terms(&got, &[want], &atoms, "{{'$2','$1'}}");

    // `{{'$1',['$2','$2']}}` — a tuple whose second element is a list construct.
    let inner_list = list(&mut heap, &[at(&atoms, "$2"), at(&atoms, "$2")]);
    let tuple = tup(&mut heap, &[at(&atoms, "$1"), inner_list]);
    let wrapped = tup(&mut heap, &[tuple]);
    let got = select_one(&table, head, &[], &[wrapped], &atoms, &mut heap).unwrap();
    let inner_list = list(&mut heap, &[make_small(10), make_small(20)]);
    let inner_list = list(&mut heap, &[inner_list, inner_list]);
    let want = tup(&mut heap, &[make_small(1), inner_list]);
    assert_terms(&got, &[want], &atoms, "{{'$1',['$2','$2']}}");

    // A nested construct that is not itself written `{{…}}` is `badarg`.
    let one_tuple = tup(&mut heap, &[at(&atoms, "$1")]);
    let list_construct = list(&mut heap, &[at(&atoms, "$2")]);
    let nested = tup(&mut heap, &[one_tuple, list_construct]);
    let wrapped = tup(&mut heap, &[nested]);
    let spec = spec1(&mut heap, head, &[], &[wrapped]);
    assert!(
        matches!(compile(spec, &atoms), Err(BeamError::BadArg)),
        "a bare single-element tuple in a construct is a call -> badarg"
    );
}

#[test]
fn a_failing_body_expression_becomes_the_atom_exit_and_evaluation_continues() {
    // oracle on `{1,10}`/`{2,20}` (set):
    //   body [{hd,5}]    -> ['EXIT','EXIT'] (the failure is the C `FAIL_TERM`, `EXIT`)
    //   body [{hd,5},7]  -> [7,7]           (the failure is discarded and the last value
    //                                        supplies the clause's value)
    let atoms = atoms_with_exit();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::OrderedSet);
    table
        .insert(tup(&mut heap, &[make_small(1), make_small(10)]), &atoms)
        .unwrap();
    table
        .insert(tup(&mut heap, &[make_small(2), make_small(20)]), &atoms)
        .unwrap();
    let head = tup(&mut heap, &[at(&atoms, "_"), at(&atoms, "_")]);

    let raising = call(&mut heap, at(&atoms, "hd"), &[make_small(5)]);
    let got = select_one(&table, head, &[], &[raising], &atoms, &mut heap).unwrap();
    assert_terms(
        &got,
        &[at(&atoms, "EXIT"), at(&atoms, "EXIT")],
        &atoms,
        "a failing body yields EXIT",
    );

    let raising = call(&mut heap, at(&atoms, "hd"), &[make_small(5)]);
    let got = select_one(&table, head, &[], &[raising, make_small(7)], &atoms, &mut heap).unwrap();
    assert_eq!(
        got,
        vec![make_small(7), make_small(7)],
        "a later body expression supplies the value"
    );

    // The failed body also makes `select_count`/`select_delete` see `EXIT`, never `true`.
    let raising = call(&mut heap, at(&atoms, "hd"), &[make_small(5)]);
    let spec = compile(spec1(&mut heap, head, &[], &[raising]), &atoms).unwrap();
    assert_eq!(table.select_count(&spec, &atoms, &mut heap).unwrap(), 0);
    let mut table = new_table(TableKind::OrderedSet);
    table
        .insert(tup(&mut heap, &[make_small(1), make_small(10)]), &atoms)
        .unwrap();
    let raising = call(&mut heap, at(&atoms, "hd"), &[make_small(5)]);
    let spec = compile(spec1(&mut heap, head, &[], &[raising]), &atoms).unwrap();
    assert_eq!(table.select_delete(&spec, &atoms, &mut heap).unwrap(), 0);
}

#[test]
fn dollar_zero_binds_and_malformed_dollar_atoms_are_literal_atoms() {
    // oracle on `{1,a}`/`{2,7}` (ordered_set):
    //   head {'$0','_'} body ['$_'] -> both        (`'$0'` IS variable 0)
    //   head {'$0','_'} body ['$0'] -> [1,2]
    //   head {'$x','_'} / {'$1x','_'} / {'$01','_'} -> [] (ordinary literal atoms)
    //   body ['$x'] -> ['$x','$x'] (a literal atom, not a variable)
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::OrderedSet);
    table
        .insert(tup(&mut heap, &[make_small(1), at(&atoms, "a")]), &atoms)
        .unwrap();
    table
        .insert(tup(&mut heap, &[make_small(2), make_small(7)]), &atoms)
        .unwrap();

    let head = tup(&mut heap, &[at(&atoms, "$0"), at(&atoms, "_")]);
    let got = select_one(&table, head, &[], &[at(&atoms, "$_")], &atoms, &mut heap).unwrap();
    assert_eq!(got.len(), 2, "'$0' binds every object");
    let head = tup(&mut heap, &[at(&atoms, "$0"), at(&atoms, "_")]);
    let got = select_one(&table, head, &[], &[at(&atoms, "$0")], &atoms, &mut heap).unwrap();
    assert_eq!(got, vec![make_small(1), make_small(2)], "'$0' in a body");

    for malformed in ["$x", "$1x", "$01"] {
        let head = tup(&mut heap, &[at(&atoms, malformed), at(&atoms, "_")]);
        let got = select_one(&table, head, &[], &[at(&atoms, "$_")], &atoms, &mut heap).unwrap();
        assert!(got.is_empty(), "`{malformed}` is a literal atom that matches nothing");
    }

    let head = tup(&mut heap, &[at(&atoms, "_"), at(&atoms, "_")]);
    let got = select_one(&table, head, &[], &[at(&atoms, "$x")], &atoms, &mut heap).unwrap();
    assert_terms(
        &got,
        &[at(&atoms, "$x"), at(&atoms, "$x")],
        &atoms,
        "['$x'] is the literal atom",
    );
}

#[test]
fn nested_repeated_variable_enforces_equality_both_ways() {
    // oracle: head `{'_',{'$1','$1'}}` on `{1,{7,7}}`,`{2,{5,8}}` -> only the first;
    // and the repeated variable is also usable in the body.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::OrderedSet);
    let equal = tup(&mut heap, &[make_small(7), make_small(7)]);
    table
        .insert(tup(&mut heap, &[make_small(1), equal]), &atoms)
        .unwrap();
    let unequal = tup(&mut heap, &[make_small(5), make_small(8)]);
    table
        .insert(tup(&mut heap, &[make_small(2), unequal]), &atoms)
        .unwrap();

    let inner = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$1")]);
    let head = tup(&mut heap, &[at(&atoms, "_"), inner]);
    let got = select_one(&table, head, &[], &[at(&atoms, "$1")], &atoms, &mut heap).unwrap();
    assert_eq!(got, vec![make_small(7)], "only the {{7,7}} object matches");

    let inner = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$1")]);
    let head = tup(&mut heap, &[at(&atoms, "_"), inner]);
    let inner_body = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$1")]);
    let wrapped = tup(&mut heap, &[inner_body]);
    let got = select_one(&table, head, &[], &[wrapped], &atoms, &mut heap).unwrap();
    let want = tup(&mut heap, &[make_small(7), make_small(7)]);
    assert_terms(&got, &[want], &atoms, "the repeated binding in a body");
}

#[test]
fn a_map_head_pattern_compares_as_an_exact_literal() {
    // oracle: T = [{1,#{a=>1}},{2,#{a=>2}}]; head `{'_',#{a=>1}}` -> `[{1,#{a=>1}}]`
    // (a map with *no* wildcard/variable in the head is a plain exact literal here).
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::Set);
    let one = flat_map(&mut heap, &[(at(&atoms, "a"), make_small(1))], &atoms);
    let two = flat_map(&mut heap, &[(at(&atoms, "a"), make_small(2))], &atoms);
    let object_one = tup(&mut heap, &[make_small(1), one]);
    let object_two = tup(&mut heap, &[make_small(2), two]);
    table.insert(object_one, &atoms).unwrap();
    table.insert(object_two, &atoms).unwrap();

    let pattern = flat_map(&mut heap, &[(at(&atoms, "a"), make_small(1))], &atoms);
    let head = tup(&mut heap, &[at(&atoms, "_"), pattern]);
    let got = select_one(&table, head, &[], &[at(&atoms, "$_")], &atoms, &mut heap).unwrap();
    assert_terms(&got, &[object_one], &atoms, "an exact map head");
}

#[test]
fn an_unbound_variable_in_a_guard_or_a_body_is_badarg() {
    // oracle: a `'$N'` the head does not bind makes the whole `ets:select/2` `badarg`,
    // whether it appears in a guard or in a body.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let head = tup(&mut heap, &[at(&atoms, "_"), at(&atoms, "_")]);

    let guard = tup(&mut heap, &[at(&atoms, "=:="), at(&atoms, "$5"), make_small(1)]);
    let spec = spec1(&mut heap, head, &[guard], &[at(&atoms, "$_")]);
    assert!(
        matches!(compile(spec, &atoms), Err(BeamError::BadArg)),
        "an unbound guard variable is badarg"
    );

    let head = tup(&mut heap, &[at(&atoms, "_"), at(&atoms, "_")]);
    let spec = spec1(&mut heap, head, &[], &[at(&atoms, "$5")]);
    assert!(
        matches!(compile(spec, &atoms), Err(BeamError::BadArg)),
        "an unbound body variable is badarg"
    );
}

/// The three constructs below are **documented capability gaps** of this crate
/// (`src/select/mod.rs` "Known gaps"), each a typed outcome — never a wrong result.
/// In every case the system `erl` (OTP 29 / erts-17.1) ACCEPTS the construct, so these
/// are **recorded divergences**, not parity claims: this crate is strictly less
/// capable here (reported to the source owner, not asserted as oracle behaviour).
#[test]
fn documented_capability_gaps_are_typed_and_diverge_from_the_oracle() {
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::OrderedSet);
    table
        .insert(tup(&mut heap, &[make_small(1), make_small(10)]), &atoms)
        .unwrap();
    table
        .insert(tup(&mut heap, &[make_small(2), make_small(20)]), &atoms)
        .unwrap();
    let head = tup(&mut heap, &[at(&atoms, "_"), at(&atoms, "_")]);
    let map = flat_map(&mut heap, &[(at(&atoms, "a"), make_small(1))], &atoms);

    // 1. A map as a guard/body *expression* is **constructed** here, as the oracle
    //    does: body `[#{a=>1}]` -> one map per matched object.
    let spec = compile(spec1(&mut heap, head, &[], &[map]), &atoms)
        .expect("a map body compiles (it is constructed, not a literal)");
    let got = table.select(&spec, &atoms, &mut heap).unwrap();
    assert_eq!(got.len(), 2, "a map body yields one constructed map per object");
    for term in &got {
        assert!(
            eq(*term, map, &atoms),
            "the constructed map equals the spec's map literal"
        );
    }

    // 2. A wildcard inside a HEAD map under-matches here (the head map is a literal);
    //    the oracle matches structurally (`#{a=>'_'}` matches `#{a=>1}` and `#{a=>2}`).
    let mut map_table = new_table(TableKind::Set);
    let one = flat_map(&mut heap, &[(at(&atoms, "a"), make_small(1))], &atoms);
    let two = flat_map(&mut heap, &[(at(&atoms, "a"), make_small(2))], &atoms);
    let object_one = tup(&mut heap, &[make_small(1), one]);
    let object_two = tup(&mut heap, &[make_small(2), two]);
    map_table.insert(object_one, &atoms).unwrap();
    map_table.insert(object_two, &atoms).unwrap();
    let wildcard_map = flat_map(&mut heap, &[(at(&atoms, "a"), at(&atoms, "_"))], &atoms);
    let head = tup(&mut heap, &[at(&atoms, "_"), wildcard_map]);
    let got = select_one(&map_table, head, &[], &[at(&atoms, "$_")], &atoms, &mut heap).unwrap();
    assert!(
        got.is_empty(),
        "DIVERGENCE: the oracle matches a wildcard map head structurally; this crate \
         compares the head map as an exact literal"
    );

    // 3. `self/0` has no process identity at the ETS layer, so it raises and its clause
    //    is skipped; the oracle returns the pid (`{self} =:= {self}` matches).
    let self_call = call(&mut heap, at(&atoms, "self"), &[]);
    let guard = tup(
        &mut heap,
        &[at(&atoms, "=:="), self_call, self_call],
    );
    let got = select_one(&table, head, &[guard], &[at(&atoms, "$_")], &atoms, &mut heap).unwrap();
    assert!(
        got.is_empty(),
        "DIVERGENCE: the oracle matches `{{self}} =:= {{self}}`; this crate's `self/0` raises"
    );
}

// ===========================================================================
// Traversal-order semantics for the FORWARD select family
//
// oracle on `{3,c,30}`/`{1,a,10}`/`{2,b,20}` (inserted in that scrambled order):
//   ordered_set -> select/match_object/match all `[{1,a,10},{2,b,20},{3,c,30}]`
//                  (exact ascending arithmetic key order) and select_count -> 3
//   set/bag/duplicate_bag -> the SAME objects in an *unspecified* order (the oracle
//                  happens to emit `[{3,c,30},{2,b,20},{1,a,10}]`); the multiset is
//                  pinned, never the hash order.
// ===========================================================================

/// The three traversal objects `{1,a,10}`,`{2,b,20}`,`{3,c,30}`, ascending by key.
fn traversal_objects(heap: &mut Heap, atoms: &AtomTable) -> Vec<Eterm> {
    vec![
        object(heap, 1, at(atoms, "a"), 10),
        object(heap, 2, at(atoms, "b"), 20),
        object(heap, 3, at(atoms, "c"), 30),
    ]
}

/// The `match/2` bindings `[K,V,W]` for the same three objects, ascending by key.
fn traversal_bindings(heap: &mut Heap, atoms: &AtomTable) -> Vec<Eterm> {
    vec![
        list(heap, &[make_small(1), at(atoms, "a"), make_small(10)]),
        list(heap, &[make_small(2), at(atoms, "b"), make_small(20)]),
        list(heap, &[make_small(3), at(atoms, "c"), make_small(30)]),
    ]
}

/// The `{K,V,W}` table for the traversal-order tests (inserted in scrambled order).
fn traversal_table(kind: TableKind, atoms: &AtomTable, heap: &mut Heap) -> Table {
    let mut table = new_table(kind);
    for (key, value, third) in [(3i64, "c", 30i64), (1, "a", 10), (2, "b", 20)] {
        let object = object(heap, key, at(atoms, value), third);
        table.insert(object, atoms).unwrap();
    }
    table
}

/// Assert the traversal `got` is exactly `expect` for an `ordered_set`, or the same
/// **multiset** for a hash kind (whose iteration order is unspecified).
fn assert_traversal(
    kind: TableKind,
    got: &[Eterm],
    expect: &[Eterm],
    atoms: &AtomTable,
    what: &str,
) {
    if kind == TableKind::OrderedSet {
        assert_terms(got, expect, atoms, &format!("{what}: ordered_set exact order"));
    } else {
        let sorted = sorted_by_key(got.to_vec());
        assert_terms(&sorted, expect, atoms, &format!("{what}: hash-kind multiset"));
    }
}

#[test]
fn forward_select_family_traversal_order_is_pinned_for_every_kind() {
    for kind in KINDS {
        let atoms = atoms();
        let mut heap = Heap::new(8192).expect("test heap");
        let table = traversal_table(kind, &atoms, &mut heap);
        let expect = traversal_objects(&mut heap, &atoms);

        // `select` with an `'$_'` body.
        let head = tup(
            &mut heap,
            &[at(&atoms, "$1"), at(&atoms, "$2"), at(&atoms, "$3")],
        );
        let spec = compile(spec1(&mut heap, head, &[], &[at(&atoms, "$_")]), &atoms).unwrap();
        let got = table.select(&spec, &atoms, &mut heap).unwrap();
        assert_traversal(kind, &got, &expect, &atoms, "select");

        // `match_object` over the bare pattern.
        let pattern = tup(
            &mut heap,
            &[at(&atoms, "$1"), at(&atoms, "$2"), at(&atoms, "$3")],
        );
        let got = table.match_object(pattern, &atoms, &mut heap).unwrap();
        assert_traversal(kind, &got, &expect, &atoms, "match_object");

        // `match` over the bare pattern (each result is a binding list).
        let pattern = tup(
            &mut heap,
            &[at(&atoms, "$1"), at(&atoms, "$2"), at(&atoms, "$3")],
        );
        let got = table.match_(pattern, &atoms, &mut heap).unwrap();
        let expect_bindings = traversal_bindings(&mut heap, &atoms);
        if kind == TableKind::OrderedSet {
            assert_terms(&got, &expect_bindings, &atoms, "match: ordered_set exact order");
        } else {
            let sorted = sorted_bindings_by_key(got);
            assert_terms(&sorted, &expect_bindings, &atoms, "match: hash-kind multiset");
        }

        // `select_count` counts only the objects whose body is `true`.
        let head = tup(
            &mut heap,
            &[at(&atoms, "$1"), at(&atoms, "$2"), at(&atoms, "$3")],
        );
        let spec = compile(spec1(&mut heap, head, &[], &[at(&atoms, "true")]), &atoms).unwrap();
        assert_eq!(
            table.select_count(&spec, &atoms, &mut heap).unwrap(),
            3,
            "kind {kind:?}: select_count"
        );
    }
}

#[test]
fn forward_delete_family_traversal_is_pinned_for_every_kind() {
    // oracle: `ets:select_delete(T, [{{'$1','$2','$3'},[],[true]}])` deletes all three
    // objects (returns 2 with a value guard — see the hash multiset check below) and
    // `ets:match_delete(T, {'$1','$2','$3'})` deletes all three objects. Both are
    // order-independent (they remove by value), so the *count* is pinned exactly and the
    // surviving table is asserted empty.
    for kind in KINDS {
        let atoms = atoms();
        let mut heap = Heap::new(8192).expect("test heap");

        let mut table = traversal_table(kind, &atoms, &mut heap);
        let head = tup(
            &mut heap,
            &[at(&atoms, "$1"), at(&atoms, "$2"), at(&atoms, "$3")],
        );
        let spec = compile(spec1(&mut heap, head, &[], &[at(&atoms, "true")]), &atoms).unwrap();
        assert_eq!(
            table.select_delete(&spec, &atoms, &mut heap).unwrap(),
            3,
            "kind {kind:?}: select_delete count"
        );
        assert!(table.is_empty(), "kind {kind:?}: select_delete emptied the table");

        // `match_delete` returns the deleted count in this crate (the BIF returns `true`).
        let mut table = traversal_table(kind, &atoms, &mut heap);
        let pattern = tup(
            &mut heap,
            &[at(&atoms, "$1"), at(&atoms, "$2"), at(&atoms, "$3")],
        );
        assert_eq!(
            table.match_delete(pattern, &atoms, &mut heap).unwrap(),
            3,
            "kind {kind:?}: match_delete count"
        );
        assert!(table.is_empty(), "kind {kind:?}: match_delete emptied the table");
    }
}

#[test]
fn ordered_set_merges_arithmetic_key_duplicates_while_hash_kinds_keep_them_distinct() {
    // oracle: insert `{1,a}` then `{1.0,b}`:
    //   ordered_set         -> size 1, tab2list `[{1.0,b}]`, lookup(1) `[{1.0,b}]`
    //                          (keys compare arithmetically; the last-inserted key *term*
    //                           is the one iterated)
    //   set/bag/duplicate_bag -> size 2, lookup(1) `[{1,a}]`, lookup(1.0) `[{1.0,b}]`
    //                          (exact `=:=` key equality)
    let one_point_zero = |heap: &mut Heap| float(heap, 1.0);
    for kind in KINDS {
        let atoms = atoms();
        let mut heap = Heap::new(8192).expect("test heap");
        let mut table = new_table(kind);
        let key_one = make_small(1);
        let key_float = one_point_zero(&mut heap);
        let object_int = tup(&mut heap, &[key_one, at(&atoms, "a")]);
        let object_float = tup(&mut heap, &[key_float, at(&atoms, "b")]);
        table.insert(object_int, &atoms).unwrap();
        table.insert(object_float, &atoms).unwrap();

        if kind == TableKind::OrderedSet {
            assert_eq!(table.size(), 1, "ordered_set merges 1 and 1.0");
            let listed = table.tab2list();
            assert_terms(
                &listed,
                &[object_float],
                &atoms,
                "ordered_set iterates the last-inserted key term",
            );
        } else {
            assert_eq!(table.size(), 2, "{kind:?} keeps 1 and 1.0 distinct");
            let int_hit = table.lookup(key_one, &atoms);
            assert_terms(&int_hit, &[object_int], &atoms, "lookup(1)");
            let float_hit = table.lookup(key_float, &atoms);
            assert_terms(&float_hit, &[object_float], &atoms, "lookup(1.0)");
        }
    }
}

// ===========================================================================
// The guard-error classification (P0) and the guard-BIF battery
//
// Every expectation below is the system `erl`'s (OTP 29 / erts-17.1), probed with
// `ets:select/2` / `ets:select_count/2` / `ets:select_delete/2` /
// `ets:select_replace/2` (and `ets:match_spec_run/2` for the run-level cases); the
// probe result is quoted beside each assertion.
//
// The two guard-error rules `exec::guards_hold` (P0) enforces are:
//   * a guard the ORACLE itself raises (`badarg`) — `element/2` out of range, `hd`
//     of a non-list, `div`/`rem` by zero, `map_get/2` with an absent key, … — makes
//     the clause fail (`Ok(false)`), with NO error (the C `matchCatch`);
//   * a guard this runtime CANNOT evaluate (a capability gap, e.g. the default
//     `ArithHook` refusing `/` or bignum/float arithmetic) is a typed error that is
//     PROPAGATED, never turned into a silent non-match (workspace `CONTEXT.md` §3).
// ===========================================================================

/// `select` a one-clause spec `{head, [guard], ['$1']}` and return the matched keys.
fn guard_select_keys(
    table: &Table,
    head: Eterm,
    guard: Eterm,
    atoms: &AtomTable,
    heap: &mut Heap,
) -> Vec<Eterm> {
    select_one(table, head, &[guard], &[at(atoms, "$1")], atoms, heap)
        .expect("the spec compiles and the guard runs to completion")
}

/// `{'=:=', Left, Right}` — the guard form the battery below builds.
fn exact_guard(heap: &mut Heap, atoms: &AtomTable, left: Eterm, right: Eterm) -> Eterm {
    tup(heap, &[at(atoms, "=:="), left, right])
}

/// Assert a refusal is the crate's typed `unsupported_ets` capability-gap raise whose
/// reason atom starts with `prefix`.
fn assert_typed_gap(err: BeamError, prefix: &str) {
    let raise = err
        .into_raise()
        .expect("a capability gap is an explicit raise");
    assert_eq!(raise.class, beam_common::ExceptionClass::Error, "{prefix}");
    let reason = raise
        .reason
        .as_atom()
        .expect("the reason is an atom")
        .to_string();
    assert!(
        reason.starts_with(prefix),
        "expected a raise whose reason starts with `{prefix}`, got `{reason}`"
    );
}

/// The ordered guard-probe table: one object per value shape.
///
/// oracle (OTP 29 / erts-17.1), `ordered_set`
/// `[{1,10},{2,{7,8}},{3,<<"hello">>},{4,#{a=>1,b=>2}},{5,[1,2,3]},{6,1.5},{7,-3}]`.
fn guard_probe_table(atoms: &AtomTable, heap: &mut Heap) -> Table {
    let mut table = new_table(TableKind::OrderedSet);
    let inner = tup(heap, &[make_small(7), make_small(8)]);
    let bin = binary(heap, b"hello");
    let map = flat_map(
        heap,
        &[
            (at(atoms, "a"), make_small(1)),
            (at(atoms, "b"), make_small(2)),
        ],
        atoms,
    );
    let lst = list(heap, &[make_small(1), make_small(2), make_small(3)]);
    let fl = float(heap, 1.5);
    table
        .insert(tup(heap, &[make_small(1), make_small(10)]), atoms)
        .unwrap();
    table
        .insert(tup(heap, &[make_small(2), inner]), atoms)
        .unwrap();
    table
        .insert(tup(heap, &[make_small(3), bin]), atoms)
        .unwrap();
    table
        .insert(tup(heap, &[make_small(4), map]), atoms)
        .unwrap();
    table
        .insert(tup(heap, &[make_small(5), lst]), atoms)
        .unwrap();
    table
        .insert(tup(heap, &[make_small(6), fl]), atoms)
        .unwrap();
    table
        .insert(tup(heap, &[make_small(7), make_small(-3)]), atoms)
        .unwrap();
    table
}

#[test]
fn unevaluable_guard_is_a_typed_error_never_a_silent_non_match() {
    // oracle (OTP 29 / erts-17.1) on the `{1,10}`/`{2,20}` table, head `{'$1','$2'}`:
    //   `ets:select(T, [{{'$1','$2'},[{'=:=',{'/',4,2},2.0}],['$1']}])` -> `[1,2]`
    // `4/2` is `2.0`, so the guard HOLDS for both objects and the body returns each
    // key. This crate's default `ArithHook` refuses `/` (its Erlang result is always a
    // float), so the guard cannot be evaluated: every select-family entry must return
    // the typed `unsupported_ets` raise — never `[]`/`0`/`Ok`, which would be a silent
    // under-match (the P0 guard-error classification).
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::Set);
    table
        .insert(tup(&mut heap, &[make_small(1), make_small(10)]), &atoms)
        .unwrap();
    table
        .insert(tup(&mut heap, &[make_small(2), make_small(20)]), &atoms)
        .unwrap();
    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let rhs = float(&mut heap, 2.0);
    let div = call(&mut heap, at(&atoms, "/"), &[make_small(4), make_small(2)]);
    let guard = exact_guard(&mut heap, &atoms, div, rhs);
    let spec = compile(spec1(&mut heap, head, &[guard], &[at(&atoms, "$_")]), &atoms).unwrap();

    assert_unsupported_arithmetic(table.select(&spec, &atoms, &mut heap).unwrap_err(), "'/'");
    assert_unsupported_arithmetic(
        table.select_count(&spec, &atoms, &mut heap).unwrap_err(),
        "'/'",
    );
    assert_unsupported_arithmetic(
        table.select_delete(&spec, &atoms, &mut heap).unwrap_err(),
        "'/'",
    );
    assert_unsupported_arithmetic(
        table.select_replace(&spec, &atoms, &mut heap).unwrap_err(),
        "'/'",
    );
    assert_eq!(
        table.size(),
        2,
        "a refused select_delete/select_replace left the table unchanged"
    );
}

#[test]
fn an_oracle_raising_guard_skips_the_clause_without_an_error() {
    // The A1-vs-A2 distinction: A1's `/` guard is a *capability gap* (typed error);
    // the guards here RAISE in the ORACLE itself, so the clause is merely unmet and
    // the whole call succeeds with an empty/zero result. oracle (OTP 29 / erts-17.1)
    // on `{1,10}`/`{2,20}`, head `{'$1','$2'}`:
    //   `{'=:=',{element,9,'$1'},1}`  -> select `[]`, select_count `0`, select_delete `0`
    //   `{'=:=',{hd,'$1'},1}`         -> likewise
    //   `{'=:=',{'div',1,0},1}`       -> likewise
    // (`element/2` out of range, `hd` of a non-list/tuple and division by zero each
    // raise `badarg`; the C `matchCatch` turns that into "guard unmet".)
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::Set);
    table
        .insert(tup(&mut heap, &[make_small(1), make_small(10)]), &atoms)
        .unwrap();
    table
        .insert(tup(&mut heap, &[make_small(2), make_small(20)]), &atoms)
        .unwrap();
    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let body = [at(&atoms, "$_")];

    for (what, lhs) in [
        (
            "element/2 out of range",
            call(
                &mut heap,
                at(&atoms, "element"),
                &[make_small(9), at(&atoms, "$1")],
            ),
        ),
        (
            "hd/1 of a non-list",
            call(&mut heap, at(&atoms, "hd"), &[at(&atoms, "$1")]),
        ),
        (
            "div by zero",
            call(&mut heap, at(&atoms, "div"), &[make_small(1), make_small(0)]),
        ),
    ] {
        let guard = exact_guard(&mut heap, &atoms, lhs, make_small(1));
        let spec = compile(spec1(&mut heap, head, &[guard], &body), &atoms).unwrap();
        assert_eq!(
            table.select(&spec, &atoms, &mut heap).unwrap(),
            Vec::<Eterm>::new(),
            "{what}: select is empty, not an error"
        );
        assert_eq!(
            table.select_count(&spec, &atoms, &mut heap).unwrap(),
            0,
            "{what}: select_count is 0"
        );
        assert_eq!(
            table.select_delete(&spec, &atoms, &mut heap).unwrap(),
            0,
            "{what}: select_delete deletes nothing"
        );
        assert_eq!(
            table.select_replace(&spec, &atoms, &mut heap).unwrap(),
            0,
            "{what}: select_replace replaces nothing"
        );
        assert_eq!(table.size(), 2, "{what}: the table is unchanged");
    }
}

#[test]
fn guard_bifs_match_the_oracle_for_map_tuple_binary_float_and_numeric_operands() {
    // oracle (OTP 29 / erts-17.1) on `guard_probe_table`, head `{'$1','$2'}`, body
    // `['$1']`, one guard `[G]` — each guard returns the listed keys:
    //   {'=:=',{map_size,'$2'},2}         -> [4]
    //   {'=:=',{map_get,a,'$2'},1}        -> [4]
    //   {'=:=',{is_map_key,b,'$2'},true}  -> [4]
    //   {'=:=',{tuple_size,'$2'},2}       -> [2]
    //   {'=:=',{byte_size,'$2'},5}        -> [3]
    //   {'=:=',{bit_size,'$2'},40}        -> [3]
    //   {'=:=',{float,'$2'},1.5}          -> [6]
    //   {'=:=',{float,'$1'},1.0}          -> [1]
    //   {'=:=',{trunc,'$2'},1}            -> [6]
    //   {'=:=',{round,'$2'},2}            -> [6]
    //   {'=:=',{abs,'$2'},3}              -> [7]
    //   {'=:=',{abs,'$2'},1.5}            -> [6]
    let atoms = atoms();
    let mut heap = Heap::new(65536).expect("test heap");
    let table = guard_probe_table(&atoms, &mut heap);
    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);

    let unary = |heap: &mut Heap, name: &str| {
        call(heap, at(&atoms, name), &[at(&atoms, "$2")])
    };

    let lhs = unary(&mut heap, "map_size");
    let g = exact_guard(&mut heap, &atoms, lhs, make_small(2));
    assert_eq!(
        guard_select_keys(&table, head, g, &atoms, &mut heap),
        vec![make_small(4)]
    );

    let lhs = call(
        &mut heap,
        at(&atoms, "map_get"),
        &[at(&atoms, "a"), at(&atoms, "$2")],
    );
    let g = exact_guard(&mut heap, &atoms, lhs, make_small(1));
    assert_eq!(
        guard_select_keys(&table, head, g, &atoms, &mut heap),
        vec![make_small(4)]
    );

    let lhs = call(
        &mut heap,
        at(&atoms, "is_map_key"),
        &[at(&atoms, "b"), at(&atoms, "$2")],
    );
    let g = exact_guard(&mut heap, &atoms, lhs, at(&atoms, "true"));
    assert_eq!(
        guard_select_keys(&table, head, g, &atoms, &mut heap),
        vec![make_small(4)]
    );

    let lhs = unary(&mut heap, "tuple_size");
    let g = exact_guard(&mut heap, &atoms, lhs, make_small(2));
    assert_eq!(
        guard_select_keys(&table, head, g, &atoms, &mut heap),
        vec![make_small(2)]
    );

    let lhs = unary(&mut heap, "byte_size");
    let g = exact_guard(&mut heap, &atoms, lhs, make_small(5));
    assert_eq!(
        guard_select_keys(&table, head, g, &atoms, &mut heap),
        vec![make_small(3)]
    );

    let lhs = unary(&mut heap, "bit_size");
    let g = exact_guard(&mut heap, &atoms, lhs, make_small(40));
    assert_eq!(
        guard_select_keys(&table, head, g, &atoms, &mut heap),
        vec![make_small(3)]
    );

    let lhs = unary(&mut heap, "float");
    let rhs = float(&mut heap, 1.5);
    let g = exact_guard(&mut heap, &atoms, lhs, rhs);
    assert_eq!(
        guard_select_keys(&table, head, g, &atoms, &mut heap),
        vec![make_small(6)]
    );

    let lhs = call(&mut heap, at(&atoms, "float"), &[at(&atoms, "$1")]);
    let rhs = float(&mut heap, 1.0);
    let g = exact_guard(&mut heap, &atoms, lhs, rhs);
    assert_eq!(
        guard_select_keys(&table, head, g, &atoms, &mut heap),
        vec![make_small(1)]
    );

    let lhs = unary(&mut heap, "trunc");
    let g = exact_guard(&mut heap, &atoms, lhs, make_small(1));
    assert_eq!(
        guard_select_keys(&table, head, g, &atoms, &mut heap),
        vec![make_small(6)]
    );

    let lhs = unary(&mut heap, "round");
    let g = exact_guard(&mut heap, &atoms, lhs, make_small(2));
    assert_eq!(
        guard_select_keys(&table, head, g, &atoms, &mut heap),
        vec![make_small(6)]
    );

    let lhs = unary(&mut heap, "abs");
    let g = exact_guard(&mut heap, &atoms, lhs, make_small(3));
    assert_eq!(
        guard_select_keys(&table, head, g, &atoms, &mut heap),
        vec![make_small(7)]
    );

    let lhs = unary(&mut heap, "abs");
    let rhs = float(&mut heap, 1.5);
    let g = exact_guard(&mut heap, &atoms, lhs, rhs);
    assert_eq!(
        guard_select_keys(&table, head, g, &atoms, &mut heap),
        vec![make_small(6)]
    );
}

#[test]
fn guard_bifs_with_constant_operands_match_every_object() {
    // oracle: on `guard_probe_table` each of these guards is a CONSTANT expression
    // that holds for every object, so `ets:select/2` returns all seven keys
    // `[1,2,3,4,5,6,7]`:
    //   {'=:=',{trunc,1.9},1}    {'=:=',{round,2.5},3}    {'=:=',{round,-2.5},-3}
    //   {'=:=',{trunc,-1.9},-1}  {'=:=',{abs,-3},3}       {'=:=',{abs,-1.5},1.5}
    //   {'=:=',{float,1},1.0}
    // (`round/1` rounds half away from zero: `round(2.5)` is 3, `round(-2.5)` is -3.)
    let atoms = atoms();
    let mut heap = Heap::new(65536).expect("test heap");
    let table = guard_probe_table(&atoms, &mut heap);
    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let all = vec![
        make_small(1),
        make_small(2),
        make_small(3),
        make_small(4),
        make_small(5),
        make_small(6),
        make_small(7),
    ];

    // trunc(1.9) =:= 1
    let f = float(&mut heap, 1.9);
    let lhs = call(&mut heap, at(&atoms, "trunc"), &[f]);
    let g = exact_guard(&mut heap, &atoms, lhs, make_small(1));
    assert_eq!(guard_select_keys(&table, head, g, &atoms, &mut heap), all);

    // round(2.5) =:= 3
    let f = float(&mut heap, 2.5);
    let lhs = call(&mut heap, at(&atoms, "round"), &[f]);
    let g = exact_guard(&mut heap, &atoms, lhs, make_small(3));
    assert_eq!(guard_select_keys(&table, head, g, &atoms, &mut heap), all);

    // round(-2.5) =:= -3
    let f = float(&mut heap, -2.5);
    let lhs = call(&mut heap, at(&atoms, "round"), &[f]);
    let g = exact_guard(&mut heap, &atoms, lhs, make_small(-3));
    assert_eq!(guard_select_keys(&table, head, g, &atoms, &mut heap), all);

    // trunc(-1.9) =:= -1
    let f = float(&mut heap, -1.9);
    let lhs = call(&mut heap, at(&atoms, "trunc"), &[f]);
    let g = exact_guard(&mut heap, &atoms, lhs, make_small(-1));
    assert_eq!(guard_select_keys(&table, head, g, &atoms, &mut heap), all);

    // abs(-3) =:= 3
    let lhs = call(&mut heap, at(&atoms, "abs"), &[make_small(-3)]);
    let g = exact_guard(&mut heap, &atoms, lhs, make_small(3));
    assert_eq!(guard_select_keys(&table, head, g, &atoms, &mut heap), all);

    // abs(-1.5) =:= 1.5
    let f = float(&mut heap, -1.5);
    let lhs = call(&mut heap, at(&atoms, "abs"), &[f]);
    let rhs = float(&mut heap, 1.5);
    let g = exact_guard(&mut heap, &atoms, lhs, rhs);
    assert_eq!(guard_select_keys(&table, head, g, &atoms, &mut heap), all);

    // float(1) =:= 1.0
    let lhs = call(&mut heap, at(&atoms, "float"), &[make_small(1)]);
    let rhs = float(&mut heap, 1.0);
    let g = exact_guard(&mut heap, &atoms, lhs, rhs);
    assert_eq!(guard_select_keys(&table, head, g, &atoms, &mut heap), all);
}

#[test]
fn guard_bifs_on_a_non_map_or_non_binary_skip_the_clause() {
    // oracle: `5` is neither a map nor a bitstring, so each guard below RAISES in the
    // oracle and the clause is simply unmet — `ets:select/2` is `[]`, never an error:
    //   {'=:=',{map_get,a,5},1}        -> []
    //   {'=:=',{map_size,5},1}         -> []
    //   {'=:=',{bit_size,5},1}         -> []
    //   {'=:=',{is_map_key,a,5},true}  -> []
    let atoms = atoms();
    let mut heap = Heap::new(65536).expect("test heap");
    let table = guard_probe_table(&atoms, &mut heap);
    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);

    let lhs = call(
        &mut heap,
        at(&atoms, "map_get"),
        &[at(&atoms, "a"), make_small(5)],
    );
    let g = exact_guard(&mut heap, &atoms, lhs, make_small(1));
    assert!(guard_select_keys(&table, head, g, &atoms, &mut heap).is_empty());

    let lhs = call(&mut heap, at(&atoms, "map_size"), &[make_small(5)]);
    let g = exact_guard(&mut heap, &atoms, lhs, make_small(1));
    assert!(guard_select_keys(&table, head, g, &atoms, &mut heap).is_empty());

    let lhs = call(&mut heap, at(&atoms, "bit_size"), &[make_small(5)]);
    let g = exact_guard(&mut heap, &atoms, lhs, make_small(1));
    assert!(guard_select_keys(&table, head, g, &atoms, &mut heap).is_empty());

    let lhs = call(
        &mut heap,
        at(&atoms, "is_map_key"),
        &[at(&atoms, "a"), make_small(5)],
    );
    let g = exact_guard(&mut heap, &atoms, lhs, at(&atoms, "true"));
    assert!(guard_select_keys(&table, head, g, &atoms, &mut heap).is_empty());
}

#[test]
fn map_construction_in_a_guard_and_a_body_matches_the_oracle() {
    // The spec-term map `#{'$1' => '$2'}` is a CONSTRUCTION evaluated per object:
    // each object's key/value is bound to `'$1'`/`'$2'`, so a one-pair map is built
    // from that object's own values.
    // oracle (OTP 29 / erts-17.1) on `guard_probe_table`:
    //   guard `{'=:=',{map_get,'$1',#{'$1' => '$2'}},'$2'}` -> all seven keys
    //         (`map_get(Key, #{Key => Value})` is `Value`)
    //   body  `[{map_size,#{'$1' => '$2'}}]`               -> `[1,1,1,1,1,1,1]`
    let atoms = atoms();
    let mut heap = Heap::new(65536).expect("test heap");
    let table = guard_probe_table(&atoms, &mut heap);
    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);

    let spec_map = flat_map(
        &mut heap,
        &[(at(&atoms, "$1"), at(&atoms, "$2"))],
        &atoms,
    );
    let lhs = call(
        &mut heap,
        at(&atoms, "map_get"),
        &[at(&atoms, "$1"), spec_map],
    );
    let g = exact_guard(&mut heap, &atoms, lhs, at(&atoms, "$2"));
    let got = guard_select_keys(&table, head, g, &atoms, &mut heap);
    assert_eq!(
        got,
        vec![
            make_small(1),
            make_small(2),
            make_small(3),
            make_small(4),
            make_small(5),
            make_small(6),
            make_small(7)
        ],
        "the constructed map in a guard matches every object"
    );

    let spec_map = flat_map(
        &mut heap,
        &[(at(&atoms, "$1"), at(&atoms, "$2"))],
        &atoms,
    );
    let body_call = call(&mut heap, at(&atoms, "map_size"), &[spec_map]);
    let spec = compile(spec1(&mut heap, head, &[], &[body_call]), &atoms).unwrap();
    let got = table.select(&spec, &atoms, &mut heap).unwrap();
    assert_eq!(got, vec![make_small(1); 7], "a constructed map body");
}

// ---------------------------------------------------------------------------
// Typed divergences (this crate is strictly LESS capable than the oracle)
//
// Each is a typed `unsupported_ets` raise — never a wrong result — reported to the
// source owner rather than asserted as oracle behaviour. `self/0` is a different
// KIND of gap: an honest SEMANTIC under-match (it raises `badarg`, so its clause is
// skipped, while the oracle evaluates it against the calling process); that case is
// pinned by `documented_capability_gaps_are_typed_and_diverge_from_the_oracle` above
// and is only referenced here.
// ---------------------------------------------------------------------------

#[test]
fn list_to_tuple_is_not_a_guard_bif_and_is_badarg_like_the_oracle() {
    // The oracle's `guard_tab` (`erl_db_util.c:572-976`) has no `list_to_tuple/1`, so
    // `ets:select(T, [{{'$1','$2'},[{'=:=',{list_to_tuple,'$1'},5}],['$1']}])` is
    // `error:badarg` — the whole call is rejected. This crate's `decode_call`
    // fallthrough returns the same `badarg`, so the behaviour MATCHES the oracle
    // (this is not a divergence, just an unsupported construct both reject).
    let atoms = atoms();
    let mut heap = Heap::new(65536).expect("test heap");
    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let lhs = call(&mut heap, at(&atoms, "list_to_tuple"), &[at(&atoms, "$1")]);
    let g = exact_guard(&mut heap, &atoms, lhs, make_small(5));
    let spec = spec1(&mut heap, head, &[g], &[at(&atoms, "$1")]);
    assert!(
        matches!(compile(spec, &atoms), Err(BeamError::BadArg)),
        "list_to_tuple/1 is not a guard BIF: `badarg`, as the oracle"
    );
}

#[test]
fn abs_on_a_bignum_is_a_typed_gap() {
    // oracle: `abs/1` on a bignum returns a bignum (`abs(-(1 bsl 60))` is `1 bsl 60`),
    // so a guard over it is evaluated. This crate's match-spec `abs/1` has no bignum
    // arithmetic, so it is the typed `unsupported_ets` raise — never a `badarg` that
    // would silently under-match.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::OrderedSet);
    table
        .insert(tup(&mut heap, &[make_small(1), make_small(10)]), &atoms)
        .unwrap();
    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    // A 1-digit bignum `1 bsl 60` (beyond the 60-bit small-integer range).
    // SAFETY: `big_size_words(1)` (2) exclusively-owned words for a 1-digit bignum;
    // the heap outlives the term.
    let big = unsafe {
        let hp = heap.alloc_words(big_size_words(1)).expect("bignum words");
        write_big(hp, false, &[1u64 << 60])
    };
    let lhs = call(&mut heap, at(&atoms, "abs"), &[big]);
    let g = exact_guard(&mut heap, &atoms, lhs, make_small(1));
    let spec = compile(spec1(&mut heap, head, &[g], &[at(&atoms, "true")]), &atoms).unwrap();
    let err = table.select_count(&spec, &atoms, &mut heap).unwrap_err();
    assert_typed_gap(err, "unsupported_ets: match-spec 'abs'");
}

#[test]
fn trunc_and_round_beyond_the_small_range_are_a_typed_gap() {
    // oracle: `trunc(1.0e30)` / `round(1.0e30)` return a BIGNUM
    // (`1000000000000000019884624838656`), so the guard is evaluated. This crate's
    // match-spec integral conversion is small-int only, so a result outside the 60-bit
    // small range is the typed `unsupported_ets` raise — never a truncated wrong
    // integer.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::OrderedSet);
    table
        .insert(tup(&mut heap, &[make_small(1), make_small(10)]), &atoms)
        .unwrap();
    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);

    let big_float = float(&mut heap, 1.0e30);
    let lhs = call(&mut heap, at(&atoms, "trunc"), &[big_float]);
    let g = exact_guard(&mut heap, &atoms, lhs, make_small(1));
    let spec = compile(spec1(&mut heap, head, &[g], &[at(&atoms, "true")]), &atoms).unwrap();
    let err = table.select_count(&spec, &atoms, &mut heap).unwrap_err();
    assert_typed_gap(err, "unsupported_ets: match-spec 'trunc'/'round'");

    let big_float = float(&mut heap, 1.0e30);
    let lhs = call(&mut heap, at(&atoms, "round"), &[big_float]);
    let g = exact_guard(&mut heap, &atoms, lhs, make_small(1));
    let spec = compile(spec1(&mut heap, head, &[g], &[at(&atoms, "true")]), &atoms).unwrap();
    let err = table.select_count(&spec, &atoms, &mut heap).unwrap_err();
    assert_typed_gap(err, "unsupported_ets: match-spec 'trunc'/'round'");
}

#[test]
fn byte_size_and_bit_size_read_an_off_heap_refc_binary_correctly() {
    // oracle (OTP 29 / erts-17.1): `byte_size(<<"0123456789abcdefghij">>)` is 20 and
    // `bit_size(...)` is 160, whether the binary is on-heap or an off-heap refc binary.
    // This crate's `expr::bitstring_length` classifies the on-heap `[BinRef][ErlSubBits]`
    // pair (root = the `ErlSubBits`) as `TermClass::Bitstring` and reads `end - start`,
    // so both sizes come back CORRECT — oracle parity, not a typed gap. (Its
    // `TermClass::BinRef` → `unsupported_ets` arm is reachable only for a *bare*
    // `BinRef` term, which is not a user-visible term.)
    //
    // The table-owned deep copy rejects `BinRef`/`SubBits` (a documented `TermStore`
    // limitation), so the guard runs over a `match_spec_run` input element, which is
    // not copied.
    let atoms = atoms();
    let mut heap = Heap::new(65536).expect("test heap");
    let payload = RefcBinary::new(b"0123456789abcdefghij").expect("refc payload");
    // SAFETY: exactly `ERL_REFC_BITS_SIZE` (8) exclusively-owned words are reserved at
    // `hp` (the `write_refc_binary` contract); `payload.data_ptr()` is a live,
    // word-aligned pointer to `20 * 8` bits of off-heap storage kept alive by `payload`
    // for the whole test, and `null` is the (empty) off-heap chain link.
    let refc = unsafe {
        let hp = heap
            .alloc_words(ERL_REFC_BITS_SIZE)
            .expect("8 words for the pair");
        write_refc_binary(
            hp,
            payload.data_ptr() as *mut Eterm,
            core::ptr::null_mut(),
            payload.data_ptr(),
            0,
            20 * 8,
        )
    };
    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let object = tup(&mut heap, &[make_small(4), refc]);
    let input = list(&mut heap, &[object]);
    let body = [at(&atoms, "$1")];

    let bs = call(&mut heap, at(&atoms, "byte_size"), &[at(&atoms, "$2")]);
    let g = exact_guard(&mut heap, &atoms, bs, make_small(20));
    let spec = spec1(&mut heap, head, &[g], &body);
    assert_eq!(
        match_spec_run(input, spec, &atoms, &mut heap).unwrap(),
        vec![make_small(4)],
        "byte_size of the off-heap refc binary is 20"
    );

    let bits = call(&mut heap, at(&atoms, "bit_size"), &[at(&atoms, "$2")]);
    let g = exact_guard(&mut heap, &atoms, bits, make_small(160));
    let spec = spec1(&mut heap, head, &[g], &body);
    assert_eq!(
        match_spec_run(input, spec, &atoms, &mut heap).unwrap(),
        vec![make_small(4)],
        "bit_size of the off-heap refc binary is 160"
    );
}
