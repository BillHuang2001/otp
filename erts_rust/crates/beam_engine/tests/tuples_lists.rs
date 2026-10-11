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

//! Tuples + lists instruction-family tests.
//!
//! Two halves:
//!
//! * **Fixture-driven integration** — load the committed oracle `.beam`s
//!   (`tests/fixtures/{tuples,lists}.beam`, produced by the system `erlc`) and run
//!   their exports end to end through [`Machine::run_export`], asserting exact
//!   results. These cover `put_tuple2`/`get_tuple_element`/`select_tuple_arity` and
//!   `put_list`/`get_list`/`get_hd`/`get_tl` as the compiler actually emits them.
//! * **Synthetic modules** — hand-built [`Module`]s (the loader is not involved)
//!   that also reach the *failure* paths a compiler-generated module guards with a
//!   preceding type test: a `get_tuple_element`/`get_list`/`get_hd`/`get_tl` on a
//!   non-matching term raises `{badmatch, Value}` — a `Step::Goto` into a live catch
//!   frame, or a typed [`BeamError`] out of [`Machine::run_export`] when there is
//!   none. A `select_tuple_arity` on a non-tuple source has no arity at all (C's
//!   `get_arity` yields `NIL`), so it matches no alternative and takes the switch's
//!   `fail` branch — raising `{badmatch, Value}` only when the switch is exhaustive
//!   (`NO_FAIL`).
//!
//! Every error is a typed [`BeamError`] — no test panics on runtime data.

use std::collections::BTreeMap;

use beam_common::{BeamError, ExceptionClass};
use beam_engine::{terms, Machine, MinimalContext};
use beam_loader::{load_file, Function, Instr, Label, Module, Operand, NO_FAIL};
use beam_term::{make_atom, make_small, Atom, Eterm, NIL};

/// The committed tuple fixture (`erlc tuples.erl`).
const TUPLES_BEAM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tuples.beam");
/// The committed list fixture (`erlc lists.erl`).
const LISTS_BEAM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/lists.beam");

// ---------------------------------------------------------------------------
// Fixture-driven integration
// ---------------------------------------------------------------------------

fn load(path: &str) -> Module {
    load_file(path).unwrap_or_else(|e| panic!("load {path}: {e:?}"))
}

/// `module:function/arity` for an export of `module`.
fn export_atom(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("atom {name:?} not interned"))
}

/// Run an export with `args` on a fresh machine over `ctx`.
fn run_export_with(
    module: &Module,
    name: &str,
    arity: u32,
    args: &[Eterm],
    ctx: &mut MinimalContext,
) -> Result<Eterm, BeamError> {
    let mut machine = Machine::new();
    machine.run_export(module, export_atom(module, name), arity, args, ctx)
}

#[test]
fn fixture_mk_builds_a_two_element_tuple() {
    // mk(A, B) -> {A, B}  (put_tuple2)
    let module = load(TUPLES_BEAM);
    let mut ctx = MinimalContext::new();
    let result = run_export_with(&module, "mk", 2, &[make_small(3), make_small(4)], &mut ctx)
        .expect("mk(3, 4)");
    assert_eq!(terms::tuple_arity(result).unwrap(), 2);
    assert_eq!(terms::tuple_element(result, 0).unwrap(), make_small(3));
    assert_eq!(terms::tuple_element(result, 1).unwrap(), make_small(4));
}

#[test]
fn fixture_first_reads_element_zero() {
    // first({A, _B}) -> A  (get_tuple_element)
    let module = load(TUPLES_BEAM);
    let mut ctx = MinimalContext::new();
    let arg = terms::make_tuple(&mut ctx, &[make_small(5), make_small(6)]).unwrap();
    let result = run_export_with(&module, "first", 1, &[arg], &mut ctx).expect("first({5,6})");
    assert_eq!(result, make_small(5));
}

#[test]
fn fixture_swap_reverses_the_tuple() {
    // swap({A, B}) -> {B, A}  (get_tuple_element x2 + put_tuple2)
    let module = load(TUPLES_BEAM);
    let mut ctx = MinimalContext::new();
    let arg = terms::make_tuple(&mut ctx, &[make_small(1), make_small(2)]).unwrap();
    let result = run_export_with(&module, "swap", 1, &[arg], &mut ctx).expect("swap({1,2})");
    assert_eq!(terms::tuple_arity(result).unwrap(), 2);
    assert_eq!(terms::tuple_element(result, 0).unwrap(), make_small(2));
    assert_eq!(terms::tuple_element(result, 1).unwrap(), make_small(1));
}

#[test]
fn fixture_classify_dispatches_on_tuple_arity() {
    // classify/1 switches on the tuple size (select_tuple_arity).
    let module = load(TUPLES_BEAM);
    let a = make_atom(export_atom(&module, "a"));
    let b = make_atom(export_atom(&module, "b"));
    let c = make_atom(export_atom(&module, "c"));
    for (elems, expected) in [(vec![a], 1_i64), (vec![a, b], 2), (vec![a, b, c], 3)] {
        let mut ctx = MinimalContext::new();
        let arg = terms::make_tuple(&mut ctx, &elems).unwrap();
        let result = run_export_with(&module, "classify", 1, &[arg], &mut ctx).expect("classify/1");
        // Give the arity context in the message when the switch picks the wrong arm.
        assert_eq!(result, make_small(expected), "arity {}", elems.len());
    }
}

#[test]
fn fixture_build_makes_a_cons_cell() {
    // build(H, T) -> [H | T]  (put_list)
    let module = load(LISTS_BEAM);
    let mut ctx = MinimalContext::new();
    let tail = terms::make_list(&mut ctx, &[make_small(2), make_small(3)]).unwrap();
    let result = run_export_with(&module, "build", 2, &[make_small(1), tail], &mut ctx)
        .expect("build(1, [2,3])");
    assert_eq!(
        terms::list_elements(result).unwrap(),
        vec![make_small(1), make_small(2), make_small(3)]
    );
}

#[test]
fn fixture_build_allows_an_improper_tail() {
    // `[H | T]` with a non-list `T` is a legal term.
    let module = load(LISTS_BEAM);
    let mut ctx = MinimalContext::new();
    let result = run_export_with(
        &module,
        "build",
        2,
        &[make_small(1), make_small(2)],
        &mut ctx,
    )
    .expect("build(1, 2)");
    assert_eq!(terms::cons_head(result).unwrap(), make_small(1));
    assert_eq!(terms::cons_tail(result).unwrap(), make_small(2));
}

#[test]
fn fixture_head_reads_the_first_element() {
    // head([H | _T]) -> H  (get_hd)
    let module = load(LISTS_BEAM);
    let mut ctx = MinimalContext::new();
    let arg = terms::make_list(&mut ctx, &[make_small(7), make_small(8), make_small(9)]).unwrap();
    let result = run_export_with(&module, "head", 1, &[arg], &mut ctx).expect("head([7,8,9])");
    assert_eq!(result, make_small(7));
}

#[test]
fn fixture_tail_returns_the_rest() {
    // tail([_H | T]) -> T  (get_tl)
    let module = load(LISTS_BEAM);
    let mut ctx = MinimalContext::new();
    let arg = terms::make_list(&mut ctx, &[make_small(7), make_small(8), make_small(9)]).unwrap();
    let result = run_export_with(&module, "tail", 1, &[arg], &mut ctx).expect("tail([7,8,9])");
    assert_eq!(
        terms::list_elements(result).unwrap(),
        vec![make_small(8), make_small(9)]
    );
}

#[test]
fn fixture_split_returns_head_and_tail() {
    // split([H | T]) -> {H, T}  (get_list + put_tuple2)
    let module = load(LISTS_BEAM);
    let mut ctx = MinimalContext::new();
    let arg = terms::make_list(&mut ctx, &[make_small(1), make_small(2)]).unwrap();
    let result = run_export_with(&module, "split", 1, &[arg], &mut ctx).expect("split([1,2])");
    assert_eq!(terms::tuple_arity(result).unwrap(), 2);
    assert_eq!(terms::tuple_element(result, 0).unwrap(), make_small(1));
    let rest = terms::tuple_element(result, 1).unwrap();
    assert_eq!(terms::list_elements(rest).unwrap(), vec![make_small(2)]);
}

// ---------------------------------------------------------------------------
// Synthetic-module scaffolding
// ---------------------------------------------------------------------------

/// Map every `Instr::Label` in `code` to its index.
fn labels_of(code: &[Instr]) -> BTreeMap<Label, usize> {
    let mut labels = BTreeMap::new();
    for (index, instr) in code.iter().enumerate() {
        if let Instr::Label(label) = instr {
            labels.insert(*label, index);
        }
    }
    labels
}

/// A builder for synthetic modules with an owned atom table.
struct Fixture {
    module: Module,
}

impl Fixture {
    fn new() -> Fixture {
        let mut module = Module::default();
        module.name = module.atom_table.intern("m");
        Fixture { module }
    }

    fn atom(&mut self, name: &str) -> Atom {
        self.module.atom_table.intern(name)
    }

    /// Append a function; returns its entry label (the lowest-numbered label).
    fn add_function(&mut self, name: &str, arity: u32, code: Vec<Instr>) -> Label {
        let name = self.atom(name);
        let module = self.module.name;
        let labels = labels_of(&code);
        let entry = labels.keys().next().copied().unwrap_or(Label(0));
        self.module.functions.push(Function {
            func_label: entry,
            entry_label: entry,
            module,
            name,
            arity,
            code,
            blocks: Vec::new(),
            labels,
        });
        entry
    }

    fn finish(self) -> Module {
        self.module
    }
}

/// Run `entry` with `args` on a fresh machine, over the **caller-owned** `ctx`.
///
/// `ctx` is passed in rather than created here on purpose: the run allocates heap
/// terms (the result tuple/list, and any recorded `{badmatch, _}` exception reason)
/// inside `ctx`'s arena, and `MinimalContext` frees that arena when *it* is dropped.
/// A context local to this helper would drop at the closing brace and leave every
/// returned term dangling, so the caller owns the context for at least as long as
/// it inspects the result.
fn run_inspected(
    module: &Module,
    entry: Label,
    args: &[Eterm],
    ctx: &mut MinimalContext,
) -> (Result<Eterm, BeamError>, Machine) {
    let mut machine = Machine::new();
    let result = machine.run_label(module, entry, args.len() as u32, args, ctx);
    (result, machine)
}

/// Run `entry` with `args` on a fresh machine, over the **caller-owned** `ctx`.
fn run(
    module: &Module,
    entry: Label,
    args: &[Eterm],
    ctx: &mut MinimalContext,
) -> Result<Eterm, BeamError> {
    run_inspected(module, entry, args, ctx).0
}

/// Assert that `result` raised `{badmatch, value}` with class `error`.
///
/// The recorded reason term lives in the caller's `ctx` arena, which must still be
/// alive here.
fn assert_badmatch(
    result: Result<Eterm, BeamError>,
    machine: &Machine,
    badmatch: Atom,
    value: Eterm,
) {
    assert!(
        matches!(result, Err(BeamError::Raise(_))),
        "expected a raise, got {result:?}"
    );
    let (class, reason) = machine.pending_exception().expect("a recorded exception");
    assert_eq!(class, ExceptionClass::Error);
    assert_eq!(terms::tuple_arity(reason).unwrap(), 2);
    assert_eq!(
        terms::tuple_element(reason, 0).unwrap(),
        make_atom(badmatch)
    );
    assert_eq!(terms::tuple_element(reason, 1).unwrap(), value);
}

// ---------------------------------------------------------------------------
// put_tuple2
// ---------------------------------------------------------------------------

#[test]
fn put_tuple2_builds_a_tuple_from_operands() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::PutTuple2 {
            dst: Operand::X(0),
            elements: vec![
                Operand::Integer(7),
                Operand::X(1),
                Operand::Atom(Atom::from_raw(0)),
            ],
        },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("f", 2, code);
    let module = fixture.finish();
    let mut ctx = MinimalContext::new();
    let result = run(&module, entry, &[NIL, make_small(9)], &mut ctx).unwrap();
    assert_eq!(terms::tuple_arity(result).unwrap(), 3);
    assert_eq!(terms::tuple_element(result, 0).unwrap(), make_small(7));
    assert_eq!(terms::tuple_element(result, 1).unwrap(), make_small(9));
    assert_eq!(
        terms::tuple_element(result, 2).unwrap(),
        make_atom(Atom::from_raw(0))
    );
}

#[test]
fn put_tuple2_with_no_elements_is_the_empty_tuple() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::PutTuple2 {
            dst: Operand::X(0),
            elements: vec![],
        },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("f", 0, code);
    let module = fixture.finish();
    let mut ctx = MinimalContext::new();
    let result = run(&module, entry, &[], &mut ctx).unwrap();
    assert_eq!(terms::tuple_arity(result).unwrap(), 0);
    assert_eq!(result, beam_term::tuple0());
}

// ---------------------------------------------------------------------------
// get_tuple_element
// ---------------------------------------------------------------------------

#[test]
fn get_tuple_element_reads_the_indexed_element() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::GetTupleElement {
            source: Operand::X(0),
            element: 1,
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let tuple = terms::make_tuple(&mut ctx, &[make_small(10), make_small(20)]).unwrap();
    assert_eq!(
        run(&module, entry, &[tuple], &mut ctx).unwrap(),
        make_small(20)
    );
}

#[test]
fn get_tuple_element_out_of_range_raises_badmatch() {
    // `element 5` of a 2-tuple: the C opcode would read out of bounds; the engine
    // is the safety net.
    let code = vec![
        Instr::Label(Label(1)),
        Instr::GetTupleElement {
            source: Operand::X(0),
            element: 5,
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let badmatch = fixture.atom("badmatch");
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let tuple = terms::make_tuple(&mut ctx, &[make_small(1), make_small(2)]).unwrap();
    let (result, machine) = run_inspected(&module, entry, &[tuple], &mut ctx);
    assert_badmatch(result, &machine, badmatch, tuple);
}

#[test]
fn get_tuple_element_on_a_non_tuple_raises_badmatch() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::GetTupleElement {
            source: Operand::X(0),
            element: 0,
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let badmatch = fixture.atom("badmatch");
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let value = make_small(42);
    let (result, machine) = run_inspected(&module, entry, &[value], &mut ctx);
    assert_badmatch(result, &machine, badmatch, value);
}

#[test]
fn get_tuple_element_badmatch_without_the_atom_is_the_typed_error() {
    // A module that never interned `badmatch` (the normal case for compiled code)
    // cannot materialize the tag, so the canonical typed error is returned instead
    // of minting a wrong atom.
    let code = vec![
        Instr::Label(Label(1)),
        Instr::GetTupleElement {
            source: Operand::X(0),
            element: 0,
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();
    let mut ctx = MinimalContext::new();
    let (result, machine) = run_inspected(&module, entry, &[make_small(1)], &mut ctx);
    assert!(matches!(result, Err(BeamError::BadMatch)), "{result:?}");
    assert_eq!(machine.pending_exception(), None);
}

// ---------------------------------------------------------------------------
// select_tuple_arity
// ---------------------------------------------------------------------------

#[test]
fn select_tuple_arity_dispatches_on_the_arity() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::SelectTupleArity {
            tuple: Operand::X(0),
            fail: Label(2),
            destinations: vec![
                (Operand::Integer(1), Label(10)),
                (Operand::Integer(2), Label(11)),
            ],
        },
        Instr::Label(Label(2)),
        Instr::Move {
            src: Operand::Integer(0),
            dst: Operand::X(0),
        },
        Instr::Return,
        Instr::Label(Label(10)),
        Instr::Move {
            src: Operand::Integer(100),
            dst: Operand::X(0),
        },
        Instr::Return,
        Instr::Label(Label(11)),
        Instr::Move {
            src: Operand::Integer(200),
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let t1 = terms::make_tuple(&mut ctx, &[make_small(0)]).unwrap();
    assert_eq!(
        run(&module, entry, &[t1], &mut ctx).unwrap(),
        make_small(100)
    );
    let t2 = terms::make_tuple(&mut ctx, &[make_small(0), make_small(0)]).unwrap();
    assert_eq!(
        run(&module, entry, &[t2], &mut ctx).unwrap(),
        make_small(200)
    );
}

#[test]
fn select_tuple_arity_falls_through_to_the_fail_label() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::SelectTupleArity {
            tuple: Operand::X(0),
            fail: Label(2),
            destinations: vec![(Operand::Integer(1), Label(10))],
        },
        Instr::Label(Label(2)),
        Instr::Move {
            src: Operand::Integer(0),
            dst: Operand::X(0),
        },
        Instr::Return,
        Instr::Label(Label(10)),
        Instr::Move {
            src: Operand::Integer(100),
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    // A 3-tuple has no `1` dest, so the switch falls to the fail label.
    let arg = terms::make_tuple(&mut ctx, &[make_small(0), make_small(0), make_small(0)]).unwrap();
    assert_eq!(
        run(&module, entry, &[arg], &mut ctx).unwrap(),
        make_small(0)
    );
}

#[test]
fn select_tuple_arity_without_a_fail_label_raises_badmatch() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::SelectTupleArity {
            tuple: Operand::X(0),
            fail: NO_FAIL,
            destinations: vec![(Operand::Integer(2), Label(10))],
        },
        Instr::Label(Label(10)),
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let badmatch = fixture.atom("badmatch");
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let arg = terms::make_tuple(&mut ctx, &[make_small(0), make_small(0), make_small(0)]).unwrap();
    let (result, machine) = run_inspected(&module, entry, &[arg], &mut ctx);
    assert_badmatch(result, &machine, badmatch, arg);
}

#[test]
fn select_tuple_arity_on_a_non_tuple_branches_to_the_fail_label() {
    // C's `select_val_lin.get_arity` (ground truth `emu/select_instrs.tab:122-128`)
    // sets the switch value of a non-tuple source to `NIL`, and `execute`
    // (`:130-153`) then finds no alternative equal to it and takes the ordinary
    // `$FAIL($Fail)` jump — so a non-tuple source branches to `fail` exactly like an
    // arity that matches no destination.
    let code = vec![
        Instr::Label(Label(1)),
        Instr::SelectTupleArity {
            tuple: Operand::X(0),
            fail: Label(2),
            destinations: vec![(Operand::Integer(1), Label(10))],
        },
        Instr::Label(Label(2)),
        Instr::Move {
            src: Operand::Integer(0),
            dst: Operand::X(0),
        },
        Instr::Return,
        Instr::Label(Label(10)),
        Instr::Move {
            src: Operand::Integer(100),
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    // `[]`, a small int and an atom: none is a tuple, so the switch takes `fail`.
    let an_atom = make_atom(export_atom(&module, "f"));
    for source in [NIL, make_small(3), an_atom] {
        assert_eq!(
            run(&module, entry, &[source], &mut ctx).unwrap(),
            make_small(0),
            "a non-tuple source ({source:?}) must branch to the fail label"
        );
    }
}

#[test]
fn select_tuple_arity_on_a_non_tuple_without_a_fail_label_raises_badmatch() {
    // The `NO_FAIL` (exhaustive) switch has no branch target, so its no-match path —
    // which a non-tuple source also takes — raises `{badmatch, Value}` exactly as an
    // unmatched arity does.
    let code = vec![
        Instr::Label(Label(1)),
        Instr::SelectTupleArity {
            tuple: Operand::X(0),
            fail: NO_FAIL,
            destinations: vec![(Operand::Integer(2), Label(10))],
        },
        Instr::Label(Label(10)),
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let badmatch = fixture.atom("badmatch");
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let value = make_small(7);
    let (result, machine) = run_inspected(&module, entry, &[value], &mut ctx);
    assert_badmatch(result, &machine, badmatch, value);
}

#[test]
fn select_tuple_arity_unknown_label_is_a_typed_error() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::SelectTupleArity {
            tuple: Operand::X(0),
            fail: Label(500),
            destinations: vec![(Operand::Integer(1), Label(10))],
        },
        Instr::Label(Label(10)),
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    // A 2-tuple matches no dest, so the missing `fail` label is dereferenced.
    let arg = terms::make_tuple(&mut ctx, &[make_small(0), make_small(0)]).unwrap();
    let err = run(&module, entry, &[arg], &mut ctx).unwrap_err();
    assert!(matches!(err, BeamError::BadArg), "{err:?}");
}

// ---------------------------------------------------------------------------
// set_tuple_element
// ---------------------------------------------------------------------------

#[test]
fn set_tuple_element_overwrites_in_place_and_get_tuple_element_reads_it_back() {
    // The end-to-end destructive sequence: `put_tuple2` builds `{1, 2, 3}`, an
    // `set_tuple_element` overwrites its element word 1 with `X(1)`, and a
    // `get_tuple_element` on the **same** tuple reads that word back. The result can
    // only be the value the write stored — the freshly built tuple held `2` there.
    let code = vec![
        Instr::Label(Label(1)),
        Instr::PutTuple2 {
            dst: Operand::X(0),
            elements: vec![
                Operand::Integer(1),
                Operand::Integer(2),
                Operand::Integer(3),
            ],
        },
        Instr::SetTupleElement {
            element: Operand::X(1),
            tuple: Operand::X(0),
            offset: 1,
        },
        Instr::GetTupleElement {
            source: Operand::X(0),
            element: 1,
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("f", 2, code);
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let result = run(&module, entry, &[NIL, make_small(20)], &mut ctx).unwrap();
    assert_eq!(result, make_small(20));
}

#[test]
fn set_tuple_element_offset_zero_is_the_first_element_word() {
    // Offset 0 is the tuple's first **data** word (the arityval header precedes it),
    // exactly the word `get_tuple_element` with index 0 reads.
    let code = vec![
        Instr::Label(Label(1)),
        Instr::PutTuple2 {
            dst: Operand::X(0),
            elements: vec![Operand::Integer(1), Operand::Integer(2)],
        },
        Instr::SetTupleElement {
            element: Operand::X(1),
            tuple: Operand::X(0),
            offset: 0,
        },
        Instr::GetTupleElement {
            source: Operand::X(0),
            element: 0,
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("f", 2, code);
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let result = run(&module, entry, &[NIL, make_small(42)], &mut ctx).unwrap();
    assert_eq!(result, make_small(42));
}

#[test]
fn a_set_element_leaves_the_other_elements_and_arity_intact() {
    // After writing element 1, reading a **different** element (2) of the same tuple
    // still succeeds and returns its original word — the header (arity) and the
    // untouched words survive the in-place store.
    let code = vec![
        Instr::Label(Label(1)),
        Instr::PutTuple2 {
            dst: Operand::X(0),
            elements: vec![
                Operand::Integer(1),
                Operand::Integer(2),
                Operand::Integer(3),
            ],
        },
        Instr::SetTupleElement {
            element: Operand::X(1),
            tuple: Operand::X(0),
            offset: 1,
        },
        Instr::GetTupleElement {
            source: Operand::X(0),
            element: 2,
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("f", 2, code);
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let result = run(&module, entry, &[NIL, make_small(20)], &mut ctx).unwrap();
    assert_eq!(result, make_small(3));
}

// ---------------------------------------------------------------------------
// put_list
// ---------------------------------------------------------------------------

#[test]
fn put_list_builds_a_cons_cell() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::PutList {
            head: Operand::X(0),
            tail: Operand::X(1),
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("f", 2, code);
    let module = fixture.finish();
    let mut ctx = MinimalContext::new();
    let result = run(&module, entry, &[make_small(1), make_small(2)], &mut ctx).unwrap();
    assert_eq!(terms::cons_head(result).unwrap(), make_small(1));
    assert_eq!(terms::cons_tail(result).unwrap(), make_small(2));
}

#[test]
fn put_list_builds_a_proper_list_from_nil() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::PutList {
            head: Operand::X(0),
            tail: Operand::Nil,
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();
    let mut ctx = MinimalContext::new();
    let result = run(&module, entry, &[make_small(5)], &mut ctx).unwrap();
    assert_eq!(terms::list_elements(result).unwrap(), vec![make_small(5)]);
}

// ---------------------------------------------------------------------------
// get_list / get_hd / get_tl
// ---------------------------------------------------------------------------

#[test]
fn get_list_writes_head_and_tail() {
    // Observe both halves: split into x1/x2, then rebuild them as `{H, T}`.
    let code = vec![
        Instr::Label(Label(1)),
        Instr::GetList {
            source: Operand::X(0),
            head: Operand::X(1),
            tail: Operand::X(2),
        },
        Instr::PutTuple2 {
            dst: Operand::X(0),
            elements: vec![Operand::X(1), Operand::X(2)],
        },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let tail = terms::make_list(&mut ctx, &[make_small(2), make_small(3)]).unwrap();
    let arg = terms::make_cons(&mut ctx, make_small(1), tail).unwrap();
    let result = run(&module, entry, &[arg], &mut ctx).unwrap();
    assert_eq!(terms::tuple_arity(result).unwrap(), 2);
    assert_eq!(terms::tuple_element(result, 0).unwrap(), make_small(1));
    assert_eq!(
        terms::list_elements(terms::tuple_element(result, 1).unwrap()).unwrap(),
        vec![make_small(2), make_small(3)]
    );
}

#[test]
fn get_hd_reads_the_head() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::GetHd {
            source: Operand::X(0),
            head: Operand::X(0),
        },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let arg = terms::make_list(&mut ctx, &[make_small(8), make_small(9)]).unwrap();
    assert_eq!(
        run(&module, entry, &[arg], &mut ctx).unwrap(),
        make_small(8)
    );
}

#[test]
fn get_tl_reads_the_tail() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::GetTl {
            source: Operand::X(0),
            tail: Operand::X(0),
        },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let arg = terms::make_list(&mut ctx, &[make_small(8), make_small(9)]).unwrap();
    let result = run(&module, entry, &[arg], &mut ctx).unwrap();
    assert_eq!(terms::list_elements(result).unwrap(), vec![make_small(9)]);
}

#[test]
fn get_list_on_nil_raises_badmatch() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::GetList {
            source: Operand::X(0),
            head: Operand::X(0),
            tail: Operand::X(1),
        },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let badmatch = fixture.atom("badmatch");
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();
    let mut ctx = MinimalContext::new();
    let (result, machine) = run_inspected(&module, entry, &[NIL], &mut ctx);
    assert_badmatch(result, &machine, badmatch, NIL);
}

#[test]
fn get_hd_on_a_non_list_raises_badmatch() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::GetHd {
            source: Operand::X(0),
            head: Operand::X(0),
        },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let badmatch = fixture.atom("badmatch");
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();
    let mut ctx = MinimalContext::new();
    let value = make_small(11);
    let (result, machine) = run_inspected(&module, entry, &[value], &mut ctx);
    assert_badmatch(result, &machine, badmatch, value);
}

#[test]
fn get_tl_on_nil_raises_badmatch_and_without_the_atom_is_typed() {
    // Same opcode, two modules: with the `badmatch` atom interned the raise carries
    // the `{badmatch, Value}` reason; without it the canonical typed error is used.
    let code = vec![
        Instr::Label(Label(1)),
        Instr::GetTl {
            source: Operand::X(0),
            tail: Operand::X(0),
        },
        Instr::Return,
    ];

    let mut with_atom = Fixture::new();
    let badmatch = with_atom.atom("badmatch");
    let entry = with_atom.add_function("f", 1, code.clone());
    let module = with_atom.finish();
    let mut ctx = MinimalContext::new();
    let (result, machine) = run_inspected(&module, entry, &[NIL], &mut ctx);
    assert_badmatch(result, &machine, badmatch, NIL);

    let mut without_atom = Fixture::new();
    let entry = without_atom.add_function("f", 1, code);
    let module = without_atom.finish();
    let mut ctx = MinimalContext::new();
    assert!(matches!(
        run(&module, entry, &[NIL], &mut ctx),
        Err(BeamError::BadMatch)
    ));
}
