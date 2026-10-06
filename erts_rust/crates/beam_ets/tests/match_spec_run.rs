//! End-to-end tests for the table-independent match-spec runner
//! (`ets:match_spec_run/2`) over raw match-spec clause lists.
//!
//! Every expectation below was derived by running the same spec against the system
//! `erl` (OTP 29 / erts-17.1, `erl -noshell -eval '…' -s init stop`); the probe
//! results are quoted next to the assertion. The oracle's `ets:match_spec_run/2`
//! takes a spec **already compiled** by `ets:match_spec_compile/1`, so the probes
//! wrap each raw clause list in `ets:match_spec_compile/1`; this crate has no
//! compiled-spec term and [`match_spec_run`] compiles the raw list itself.
//!
//! Note: `'$$'` written literally in Erlang source is the one-character atom `'$'`,
//! so the all-bindings probes use `list_to_atom([36,36])`.

use beam_alloc::Heap;
use beam_common::BeamError;
use beam_ets::prelude::*;
use beam_term::prelude::*;

/// The atoms the specs below reference. `true`/`false` must be interned: a body
/// whose value is an internal boolean is rendered as the atom it names.
fn atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    for name in [
        "_", "$_", "$$", "$1", "$2", "true", "false", "foo", "a", "b", "x", "y", "ok", ">",
    ] {
        atoms.intern(name);
    }
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

/// `[ {Head, Guards, Body} ]` for a single clause.
fn spec1(heap: &mut Heap, head: Eterm, guards: &[Eterm], body: &[Eterm]) -> Eterm {
    let guard_list = list(heap, guards);
    let body_list = list(heap, body);
    let clause = tup(heap, &[head, guard_list, body_list]);
    list(heap, &[clause])
}

/// Assert two term sequences are structurally equal, element by element.
fn assert_terms_eq(got: &[Eterm], want: &[Eterm], atoms: &AtomTable) {
    assert_eq!(got.len(), want.len(), "result length");
    for (index, (g, w)) in got.iter().zip(want).enumerate() {
        assert!(eq(*g, *w, atoms), "result {index} differs");
    }
}

// ---------------------------------------------------------------------------
// Head matching
// ---------------------------------------------------------------------------

#[test]
fn head_matches_the_element_itself_whatever_its_shape() {
    // erl: `ets:match_spec_run([foo,1,{1,a}],
    //      ets:match_spec_compile([{'$1',[],[{{'$1'}}]}]))` -> `[{foo},{1},{{1,a}}]`.
    // The head matches the element itself — an atom, an integer, a tuple — not
    // only a tuple.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    let inner = tup(&mut heap, &[at(&atoms, "$1")]);
    let body_element = tup(&mut heap, &[inner]);
    let spec = spec1(&mut heap, at(&atoms, "$1"), &[], &[body_element]);

    let tuple_element = tup(&mut heap, &[make_small(1), at(&atoms, "a")]);
    let input = list(
        &mut heap,
        &[at(&atoms, "foo"), make_small(1), tuple_element],
    );

    let got = match_spec_run(input, spec, &atoms, &mut heap).expect("run");

    let foo = tup(&mut heap, &[at(&atoms, "foo")]);
    let one = tup(&mut heap, &[make_small(1)]);
    let tuple_in_tuple = tuple_element;
    let wrapped = tup(&mut heap, &[tuple_in_tuple]);
    let want = [foo, one, wrapped];
    assert_terms_eq(&got, &want, &atoms);
}

#[test]
fn repeated_head_variable_enforces_equality() {
    // erl: `ets:match_spec_run([{1,1},{1,2}],
    //      ets:match_spec_compile([{{'$1','$1'},[],['$1']}]))` -> `[1]`
    // (`{1,2}` has unequal elements, so its clause does not match).
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$1")]);
    let spec = spec1(&mut heap, head, &[], &[at(&atoms, "$1")]);

    let equal = tup(&mut heap, &[make_small(1), make_small(1)]);
    let unequal = tup(&mut heap, &[make_small(1), make_small(2)]);
    let input = list(&mut heap, &[equal, unequal]);

    let got = match_spec_run(input, spec, &atoms, &mut heap).expect("run");
    assert_terms_eq(&got, &[make_small(1)], &atoms);
}

#[test]
fn head_dollar_underscore_never_matches_a_non_atom() {
    // erl: `ets:match_spec_run([foo,1,{1,a}],
    //      ets:match_spec_compile([{'$_',[],[true]}]))` -> `[]`. `'$_'` is only the
    // whole object in a BODY; in a HEAD it is not a wildcard, so it never matches a
    // non-atom (nor a tuple).
    //
    // KNOWN DIVERGENCE (not asserted — the head decoder is owned by the match-spec
    // `pattern` module, not by this runner): the oracle additionally matches the
    // *literal* atom `'$_'` there (`ets:match_spec_run(['$_'], …)` -> `[true]`, and
    // `list_to_atom([36,36])` likewise), because the C `dmc_one_term` emits `matchEq`
    // for any atom that is not `'_'`/`'$N'` (`erl_db_util.c:4117-4121`). This crate's
    // `pattern::decode` returns `Pattern::Fail` for `'$_'`/`'$$'`, so it never matches.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    for head in ["$_", "$$"] {
        let spec = spec1(&mut heap, at(&atoms, head), &[], &[at(&atoms, "true")]);
        let tuple_element = tup(&mut heap, &[make_small(1), at(&atoms, "a")]);
        let non_atoms = list(
            &mut heap,
            &[at(&atoms, "foo"), make_small(1), tuple_element],
        );
        let got = match_spec_run(non_atoms, spec, &atoms, &mut heap).expect("run");
        assert!(
            got.is_empty(),
            "`{head}` in a head must not match a non-atom"
        );
    }
}
// ---------------------------------------------------------------------------
// Clauses, order, bodies
// ---------------------------------------------------------------------------

#[test]
fn the_first_matching_clause_wins() {
    // erl: `ets:match_spec_run([x,y],
    //      ets:match_spec_compile([{'$1',[],[a]},{'$1',[],[b]}]))` -> `[a,a]`
    // (both clauses match every element; the FIRST one's body is used).
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    let body_a = list(&mut heap, &[at(&atoms, "a")]);
    let first = tup(&mut heap, &[at(&atoms, "$1"), NIL, body_a]);
    let body_b = list(&mut heap, &[at(&atoms, "b")]);
    let second = tup(&mut heap, &[at(&atoms, "$1"), NIL, body_b]);
    let spec = list(&mut heap, &[first, second]);
    let input = list(&mut heap, &[at(&atoms, "x"), at(&atoms, "y")]);

    let got = match_spec_run(input, spec, &atoms, &mut heap).expect("run");
    assert_terms_eq(&got, &[at(&atoms, "a"), at(&atoms, "a")], &atoms);
}

#[test]
fn order_is_preserved_and_unmatched_elements_are_dropped() {
    // erl: `ets:match_spec_run([{1,2},{3,4}],
    //      ets:match_spec_compile([{{'$1','$2'},[],[{{'$2','$1'}}]}]))`
    //      -> `[{2,1},{4,3}]` (input order preserved, unlike a table's select).
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let swapped = tup(&mut heap, &[at(&atoms, "$2"), at(&atoms, "$1")]);
    let body_swap = tup(&mut heap, &[swapped]);
    let spec = spec1(&mut heap, head, &[], &[body_swap]);

    let first = tup(&mut heap, &[make_small(1), make_small(2)]);
    let second = tup(&mut heap, &[make_small(3), make_small(4)]);
    let input = list(&mut heap, &[first, second]);

    let got = match_spec_run(input, spec, &atoms, &mut heap).expect("run");

    let swapped_first = tup(&mut heap, &[make_small(2), make_small(1)]);
    let swapped_second = tup(&mut heap, &[make_small(4), make_small(3)]);
    assert_terms_eq(&got, &[swapped_first, swapped_second], &atoms);

    // erl: `[{{'$1','$1'},[],['$1']}]` over `[{1,1},{2,3},{4,4}]` -> `[1,4]`
    // (`{2,3}` is dropped, the survivors keep their relative order).
    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$1")]);
    let spec = spec1(&mut heap, head, &[], &[at(&atoms, "$1")]);
    let equal_a = tup(&mut heap, &[make_small(1), make_small(1)]);
    let unequal = tup(&mut heap, &[make_small(2), make_small(3)]);
    let equal_b = tup(&mut heap, &[make_small(4), make_small(4)]);
    let input = list(&mut heap, &[equal_a, unequal, equal_b]);

    let got = match_spec_run(input, spec, &atoms, &mut heap).expect("run");
    assert_terms_eq(&got, &[make_small(1), make_small(4)], &atoms);
}

#[test]
fn body_whole_object_is_the_element() {
    // erl: `ets:match_spec_run([a,b], ets:match_spec_compile([{'$1',[],['$_']}]))`
    //      -> `[a,b]` (`'$_'` in a BODY is the whole element).
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    let spec = spec1(&mut heap, at(&atoms, "$1"), &[], &[at(&atoms, "$_")]);
    let input = list(&mut heap, &[at(&atoms, "a"), at(&atoms, "b")]);

    let got = match_spec_run(input, spec, &atoms, &mut heap).expect("run");
    assert_terms_eq(&got, &[at(&atoms, "a"), at(&atoms, "b")], &atoms);
}

#[test]
fn body_all_bindings_lists_every_head_variable() {
    // erl: `ets:match_spec_run([{1,2},{3,4}],
    //      ets:match_spec_compile([{{'$1','$2'},[],['$$']}]))` -> `[[1,2],[3,4]]`
    // (the all-bindings atom `'$$'`, built with `list_to_atom([36,36])`).
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let spec = spec1(&mut heap, head, &[], &[at(&atoms, "$$")]);

    let first = tup(&mut heap, &[make_small(1), make_small(2)]);
    let second = tup(&mut heap, &[make_small(3), make_small(4)]);
    let input = list(&mut heap, &[first, second]);

    let got = match_spec_run(input, spec, &atoms, &mut heap).expect("run");

    let bindings_first = list(&mut heap, &[make_small(1), make_small(2)]);
    let bindings_second = list(&mut heap, &[make_small(3), make_small(4)]);
    assert_terms_eq(&got, &[bindings_first, bindings_second], &atoms);
}

#[test]
fn guards_filter_the_elements() {
    // erl: `ets:match_spec_run([1,2,3],
    //      ets:match_spec_compile([{'$1',[{'>','$1',1}],['$1']}]))` -> `[2,3]`.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    let guard = tup(
        &mut heap,
        &[at(&atoms, ">"), at(&atoms, "$1"), make_small(1)],
    );
    let spec = spec1(&mut heap, at(&atoms, "$1"), &[guard], &[at(&atoms, "$1")]);
    let input = list(&mut heap, &[make_small(1), make_small(2), make_small(3)]);

    let got = match_spec_run(input, spec, &atoms, &mut heap).expect("run");
    assert_terms_eq(&got, &[make_small(2), make_small(3)], &atoms);
}

#[test]
fn a_literal_body_is_repeated_per_match() {
    // erl: `ets:match_spec_run([foo,1], ets:match_spec_compile([{'$1',[],[ok]}]))`
    //      -> `[ok,ok]`.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    let spec = spec1(&mut heap, at(&atoms, "$1"), &[], &[at(&atoms, "ok")]);
    let input = list(&mut heap, &[at(&atoms, "foo"), make_small(1)]);

    let got = match_spec_run(input, spec, &atoms, &mut heap).expect("run");
    assert_terms_eq(&got, &[at(&atoms, "ok"), at(&atoms, "ok")], &atoms);
}

// ---------------------------------------------------------------------------
// Empty input, no match, malformed input
// ---------------------------------------------------------------------------

#[test]
fn empty_list_yields_no_results() {
    // erl: `ets:match_spec_run([], ets:match_spec_compile([{'$1',[],[{{'$1'}}]}]))`
    //      -> `[]`.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    let inner = tup(&mut heap, &[at(&atoms, "$1")]);
    let body_element = tup(&mut heap, &[inner]);
    let spec = spec1(&mut heap, at(&atoms, "$1"), &[], &[body_element]);

    let got = match_spec_run(NIL, spec, &atoms, &mut heap).expect("run");
    assert!(got.is_empty());
}

#[test]
fn no_matching_clause_drops_every_element() {
    // erl: `ets:match_spec_run([{1,2}],
    //      ets:match_spec_compile([{{'$1','$1'},[],['$1']}]))` -> `[]`.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$1")]);
    let spec = spec1(&mut heap, head, &[], &[at(&atoms, "$1")]);

    let element = tup(&mut heap, &[make_small(1), make_small(2)]);
    let input = list(&mut heap, &[element]);

    let got = match_spec_run(input, spec, &atoms, &mut heap).expect("run");
    assert!(got.is_empty());
}

#[test]
fn improper_list_and_non_list_are_badarg() {
    // erl: `ets:match_spec_run([a|b], Compiled)` -> `error:badarg`;
    //      `ets:match_spec_run(foo, Compiled)` -> `error:badarg`.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");

    let spec = spec1(&mut heap, at(&atoms, "$1"), &[], &[at(&atoms, "$1")]);

    let improper = heap.cons(at(&atoms, "a"), at(&atoms, "b")).expect("heap");
    assert!(matches!(
        match_spec_run(improper, spec, &atoms, &mut heap),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        match_spec_run(at(&atoms, "foo"), spec, &atoms, &mut heap),
        Err(BeamError::BadArg)
    ));
}

#[test]
fn malformed_spec_is_badarg() {
    // erl: `ets:match_spec_run([foo], 1)` -> `error:badarg`; a spec clause that is
    // not a 3-tuple is likewise rejected by `ets:match_spec_compile/1`.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let input = list(&mut heap, &[at(&atoms, "foo")]);

    // A spec that is not a list.
    assert!(matches!(
        match_spec_run(input, make_small(1), &atoms, &mut heap),
        Err(BeamError::BadArg)
    ));

    // A clause that is not a `{Head, Guards, Body}` 3-tuple.
    let two = tup(&mut heap, &[at(&atoms, "$1"), NIL]);
    let bad_spec = list(&mut heap, &[two]);
    assert!(matches!(
        match_spec_run(input, bad_spec, &atoms, &mut heap),
        Err(BeamError::BadArg)
    ));
}
