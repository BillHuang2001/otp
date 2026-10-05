//! End-to-end tests for the match-spec compiler/executor and the `select`/`match`
//! family.
//!
//! Every expectation below was derived by running the same spec against the system
//! `erl` (OTP 29 / erts-17.1, `erl -noshell -eval '…' -s init stop`); the probe
//! results are quoted next to the assertion.

use beam_alloc::Heap;
use beam_term::prelude::*;
use beam_common::BeamError;
use beam_ets::prelude::*;

/// A table of the atoms the specs below reference. `true`/`false` must be
/// interned: guard results are the atoms they name.
fn atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    for name in [
        "_", "$_", "$$", "$0", "$x", "$1", "$2", "$3", "true", "false", "ok", "x", "a", "b",
        "element", "abs", "size", "hd", "tl", "length", "self", "andalso", "orelse", "not",
        "is_integer", "is_atom", "is_boolean", "is_list", "is_number", "is_tuple", "is_foo",
        "=:=", "=/=", "==", "/=", "<", "=<", ">", ">=",
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
    let clause = tup(heap, &[head, list(heap, guards), list(heap, body)]);
    list(heap, &[clause])
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
fn nested_list_head_binds_and_body_returns_in_body_order() {
    // erl: `ets:select(T,[{ {['$1','$2'],'_'}, [], ['$2','$1'] }])` on
    //     `{[1,2],5}` and `{[3,4],6}` -> `[2,1,4,3]`.
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
    assert_eq!(
        got,
        vec![make_small(2), make_small(1), make_small(4), make_small(3)]
    );
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
fn head_dollar_underscore_and_dollar_dollar_never_match() {
    // erl: `ets:match_object(T, {'$_','_','_'})` -> `[]`.
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    for name in ["$_", "$$"] {
        let mut table = new_table(TableKind::Set);
        table
            .insert(object(&mut heap, 1, at(&atoms, "a"), 10), &atoms)
            .unwrap();
        let head = tup(&mut heap, &[at(&atoms, name), at(&atoms, "_"), at(&atoms, "_")]);
        let spec = compile(spec1(&mut heap, head, &[], &[at(&atoms, "$_")]), &atoms).unwrap();
        assert!(
            table.select(&spec, &atoms, &mut heap).unwrap().is_empty(),
            "`{name}` in a head must never match"
        );
    }
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
    let first = tup(
        &mut heap,
        &[head_one, NIL, list(&mut heap, &[at(&atoms, "ok")])],
    );
    let second = tup(
        &mut heap,
        &[at(&atoms, "_"), NIL, list(&mut heap, &[at(&atoms, "x")])],
    );
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
fn construct_body_builds_a_fresh_tuple() {
    // erl: body `[{{'$1','$2'}}]` yields `{K,V}` per object.
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
    let body = list(&mut heap, &[wrapped]);
    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]);
    let spec = compile(spec1(&mut heap, head, &[], &[body]).into_term(&mut heap), &atoms);
    let _ = spec;
}

// `spec1` takes a slice of body elements; the construct form above needs a single
// element, so the test above builds the spec directly instead.
trait IntoTerm {
    fn into_term(self, heap: &mut Heap) -> Eterm;
}
impl IntoTerm for Eterm {
    fn into_term(self, _heap: &mut Heap) -> Eterm {
        self
    }
}

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
    let atoms = atoms();
    let mut heap = Heap::new(8192).expect("test heap");
    let mut table = new_table(TableKind::Set);
    table
        .insert(object(&mut heap, 1, at(&atoms, "a"), 10), &atoms)
        .unwrap();

    let head = tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2"), at(&atoms, "$3")]);
    let spec = compile(
        spec1(&mut heap, head, &[], &[at(&atoms, "$_"), at(&atoms, "$1"), at(&atoms, "ok")]),
        &atoms,
    )
    .unwrap();
    let got = table.select(&spec, &atoms, &mut heap).unwrap();
    assert_eq!(got.len(), 3);
    assert!(eq(got[0], object(&mut heap, 1, at(&atoms, "a"), 10), &atoms));
    assert_eq!(got[1], make_small(1));
    assert!(eq(got[2], at(&atoms, "ok"), &atoms));
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
    assert!(
        table.memory() <= before,
        "deleting must not grow the table's storage"
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
    let clause = tup(
        &mut heap,
        &[at(&atoms, "_"), guards, list(&mut heap, &[at(&atoms, "$_")])],
    );
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
    let clause = tup(
        &mut heap,
        &[head, list(&mut heap, &[guard]), list(&mut heap, &[at(&atoms, "$_")])],
    );
    assert!(matches!(
        compile(list(&mut heap, &[clause]), &atoms),
        Err(BeamError::BadArg)
    ));

    // An unbound `'$N'` in the body (`erl` -> badarg).
    let clause = tup(
        &mut heap,
        &[at(&mut atoms_head(), "_"), NIL, list(&mut heap, &[make_small(0)])],
    );
    let _ = clause;
    let clause = tup(
        &mut heap,
        &[
            at(&atoms, "_"),
            NIL,
            list(&mut heap, &[at(&atoms, "$2")]),
        ],
    );
    assert!(matches!(
        compile(list(&mut heap, &[clause]), &atoms),
        Err(BeamError::BadArg)
    ));

    // A bare tuple body element is a *call*, not a tuple (`erl` -> badarg).
    let body = list(&mut heap, &[tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")])]);
    let clause = tup(
        &mut heap,
        &[
            tup(&mut heap, &[at(&atoms, "$1"), at(&atoms, "$2")]),
            NIL,
            body,
        ],
    );
    assert!(matches!(
        compile(list(&mut heap, &[clause]), &atoms),
        Err(BeamError::BadArg)
    ));
}

fn atoms_head() -> AtomTable {
    atoms()
}
