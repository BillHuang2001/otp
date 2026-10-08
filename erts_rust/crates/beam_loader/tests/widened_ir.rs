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

//! Integration tests for the **widened typed IR** of `beam_loader` over the
//! committed oracle fixture `tests/fixtures/coverage.beam`.
//!
//! `coverage.beam` is a real `erlc` image (see `tests/fixtures/README.md` for the
//! verified wire summary and the captured `beam_disasm:file/1` ground truth). It
//! exercises a broad slice of idiomatic Erlang — tuples, lists, closures
//! (`make_fun3`/`call_fun2`), `try`/`catch`, float arithmetic, maps and bitstrings
//! — so it is the fixture that proves those opcode families lower to their **typed**
//! [`Instr`] variants instead of the lossless [`Instr::Generic`] fallback. It also
//! carries a non-empty `LitT` (3 literals) and a one-entry `FunT`, so the
//! literal/fun resolution API is covered end to end.
//!
//! Every expected value is derived from `tests/fixtures/README.md` (the verified
//! disassembly), never guessed. `/workspace/target`-style `src/` assertions are
//! avoided: the loader's public API is the only surface touched here.
use beam_codec::term::{flatmap_parts, tuple_arity, tuple_element};
use beam_loader::{
    load, load_file, AllocKind, Atom, HeapNeed, ImportRef, Instr, Label, MapWriteArgs, Module,
    Operand, Reg, TestKind, TypeIndex, NO_FAIL,
};
use beam_term::prelude::atom_val;

/// Loads the committed `coverage.beam` oracle fixture.
fn load_coverage() -> Module {
    load_file(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/coverage.beam"
    ))
    .expect("the coverage oracle fixture must load")
}

/// Loads the committed `slice.beam` oracle fixture.
fn load_slice() -> Module {
    load_file(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/slice.beam"
    ))
    .expect("the slice oracle fixture must load")
}

/// The interned atom for `name` in `module`.
fn atom(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("atom `{name}` is interned"))
}

/// The first lowered instruction of `module` matching `pred`, or `None`.
fn find<'a>(module: &'a Module, pred: impl Fn(&Instr) -> bool) -> Option<&'a Instr> {
    module
        .functions
        .iter()
        .flat_map(|f| f.code.iter())
        .find(|instr| pred(instr))
}

/// Every lowered instruction of `module` matching `pred`, in stream order.
fn filter<'a>(module: &'a Module, pred: impl Fn(&Instr) -> bool) -> Vec<&'a Instr> {
    module
        .functions
        .iter()
        .flat_map(|f| f.code.iter())
        .filter(|instr| pred(instr))
        .collect()
}

/// Every lowered instruction of `module`, in function/stream order.
fn instructions(module: &Module) -> Vec<&Instr> {
    module
        .functions
        .iter()
        .flat_map(|f| f.code.iter())
        .collect()
}

// ---- tuples -----------------------------------------------------------------

#[test]
fn lowers_the_tuple_opcodes_to_typed_variants() {
    let module = load_coverage();

    // `put_tuple2` — the 4-element construction in `tuple_ops/1` (`{C,B,A,C-A}`).
    let four = find(&module, |i| {
        matches!(i, Instr::PutTuple2 { elements, .. } if elements.len() == 4)
    })
    .expect("a 4-element `put_tuple2` lowers to Instr::PutTuple2");
    match four {
        Instr::PutTuple2 { dst, elements } => {
            assert_eq!(*dst, Operand::X(0));
            assert_eq!(
                elements,
                &[
                    Operand::X(2),
                    Operand::X(0),
                    Operand::X(1),
                    Operand::X(3),
                ]
            );
        }
        other => panic!("expected Instr::PutTuple2, got {other:?}"),
    }

    // A second, 3-element `put_tuple2` (`{B,A,A+B}`).
    assert!(
        find(&module, |i| matches!(i, Instr::PutTuple2 { elements, .. } if elements.len() == 3))
            .is_some(),
        "the 3-element `put_tuple2` also lowers to Instr::PutTuple2"
    );

    // `get_tuple_element Source Element Dst` — the `{tr,{x,0},…},2,{x,2}` access.
    let get = find(&module, |i| {
        matches!(i, Instr::GetTupleElement { element: 2, .. })
    })
    .expect("a `get_tuple_element` lowers to Instr::GetTupleElement");
    match get {
        Instr::GetTupleElement {
            source,
            element,
            dst,
        } => {
            assert_eq!(*source, Operand::X(0));
            assert_eq!(*element, 2);
            assert_eq!(*dst, Operand::X(2));
        }
        other => panic!("expected Instr::GetTupleElement, got {other:?}"),
    }

    // `select_tuple_arity Tuple Fail {Arity,Label}*` — the `case` on the 2-/3-tuple.
    let select = find(&module, |i| matches!(i, Instr::SelectTupleArity { .. }))
        .expect("a `select_tuple_arity` lowers to Instr::SelectTupleArity");
    match select {
        Instr::SelectTupleArity {
            tuple,
            fail,
            destinations,
        } => {
            assert_eq!(
                *tuple,
                Operand::Typed {
                    reg: Reg::X(0),
                    ty: TypeIndex(1),
                }
            );
            assert_eq!(*fail, Label(5));
            assert_eq!(
                destinations,
                &[(Operand::Integer(2), Label(4)), (Operand::Integer(3), Label(3))]
            );
        }
        other => panic!("expected Instr::SelectTupleArity, got {other:?}"),
    }
}

// ---- lists ------------------------------------------------------------------

#[test]
fn lowers_the_list_opcodes_to_typed_variants() {
    let module = load_coverage();

    // `get_list {x,0},{x,1},{x,0}` in `list_ops/1`.
    let get = find(&module, |i| matches!(i, Instr::GetList { .. }))
        .expect("a `get_list` lowers to Instr::GetList");
    match get {
        Instr::GetList { source, head, tail } => {
            assert_eq!(*source, Operand::X(0));
            assert_eq!(*head, Operand::X(1));
            assert_eq!(*tail, Operand::X(0));
        }
        other => panic!("expected Instr::GetList, got {other:?}"),
    }

    // `put_list {y,0},{x,0},{x,0}` in `list_ops/1`.
    let put = find(&module, |i| matches!(i, Instr::PutList { .. }))
        .expect("a `put_list` lowers to Instr::PutList");
    match put {
        Instr::PutList { head, tail, dst } => {
            assert_eq!(*head, Operand::Y(0));
            assert_eq!(*tail, Operand::X(0));
            assert_eq!(*dst, Operand::X(0));
        }
        other => panic!("expected Instr::PutList, got {other:?}"),
    }
}

// ---- funs -------------------------------------------------------------------

#[test]
fn lowers_the_closure_opcodes_to_typed_variants() {
    let module = load_coverage();

    // `make_fun3 Index Dst {Env}` — the closure in `make_adder/1` captures `x0`.
    let make = find(&module, |i| matches!(i, Instr::MakeFun3 { .. }))
        .expect("a `make_fun3` lowers to Instr::MakeFun3");
    match make {
        Instr::MakeFun3 { index, dst, env } => {
            assert_eq!(*index, 0);
            assert_eq!(*dst, Operand::X(0));
            assert_eq!(env, &[Operand::X(0)]);
        }
        other => panic!("expected Instr::MakeFun3, got {other:?}"),
    }

    // `call_fun2 Tag Arity Func` in `apply_adder/2`: the tag is the file's `FunT`
    // index (`0`), the func is the typed register `x1` (disasm resolves the tag to
    // the fun's label `{f,37}`).
    let call = find(&module, |i| matches!(i, Instr::CallFun2 { .. }))
        .expect("a `call_fun2` lowers to Instr::CallFun2");
    match call {
        Instr::CallFun2 { tag, arity, func } => {
            assert_eq!(*tag, Operand::Integer(0));
            assert_eq!(*arity, 1);
            assert_eq!(
                func,
                &Operand::Typed {
                    reg: Reg::X(1),
                    ty: TypeIndex(2),
                }
            );
        }
        other => panic!("expected Instr::CallFun2, got {other:?}"),
    }
}

// ---- exceptions -------------------------------------------------------------

#[test]
fn lowers_the_exception_opcodes_to_typed_variants() {
    let module = load_coverage();

    // `'try' {y,0},{f,21}` in `safe_div/2`.
    match find(&module, |i| matches!(i, Instr::Try { .. }))
        .expect("a `try` lowers to Instr::Try")
    {
        Instr::Try { frame, fail } => {
            assert_eq!(*frame, Operand::Y(0));
            assert_eq!(*fail, Label(21));
        }
        other => panic!("expected Instr::Try, got {other:?}"),
    }

    // `try_end {y,0}`.
    match find(&module, |i| matches!(i, Instr::TryEnd { .. }))
        .expect("a `try_end` lowers to Instr::TryEnd")
    {
        Instr::TryEnd { frame } => assert_eq!(*frame, Operand::Y(0)),
        other => panic!("expected Instr::TryEnd, got {other:?}"),
    }

    // `try_case {y,0}`.
    match find(&module, |i| matches!(i, Instr::TryCase { .. }))
        .expect("a `try_case` lowers to Instr::TryCase")
    {
        Instr::TryCase { frame } => assert_eq!(*frame, Operand::Y(0)),
        other => panic!("expected Instr::TryCase, got {other:?}"),
    }

    // `raise Trace Value` — the `{bif,raise,…}` fold, `x2` (trace), `x1` (value).
    match find(&module, |i| matches!(i, Instr::Raise { .. }))
        .expect("a `raise` lowers to Instr::Raise")
    {
        Instr::Raise { trace, value } => {
            assert_eq!(*trace, Operand::X(2));
            assert_eq!(*value, Operand::X(1));
        }
        other => panic!("expected Instr::Raise, got {other:?}"),
    }
}

// ---- floats -----------------------------------------------------------------

#[test]
fn lowers_the_float_opcodes_to_typed_variants() {
    let module = load_coverage();

    // `fconv {x,0},{fr,0}` / `fconv {x,1},{fr,1}` (two in `safe_div/2`).
    let fconvs = filter(&module, |i| matches!(i, Instr::Fconv { .. }));
    assert_eq!(fconvs.len(), 2, "two `fconv` instructions lower");
    match fconvs[0] {
        Instr::Fconv { src, dst } => {
            assert_eq!(*src, Operand::X(0));
            assert_eq!(*dst, Operand::FloatReg(0));
        }
        other => panic!("expected Instr::Fconv, got {other:?}"),
    }

    // `fdiv {f,0},[{fr,0},{fr,1}],{fr,0}` — the dedicated `fdiv/4` opcode.
    match find(&module, |i| matches!(i, Instr::Fdiv { .. }))
        .expect("an `fdiv` lowers to Instr::Fdiv")
    {
        Instr::Fdiv {
            fail,
            lhs,
            rhs,
            dst,
        } => {
            assert_eq!(*fail, NO_FAIL);
            assert_eq!(*lhs, Operand::FloatReg(0));
            assert_eq!(*rhs, Operand::FloatReg(1));
            assert_eq!(*dst, Operand::FloatReg(0));
        }
        other => panic!("expected Instr::Fdiv, got {other:?}"),
    }

    // `fmove {fr,0},{x,0}`.
    match find(&module, |i| matches!(i, Instr::Fmove { .. }))
        .expect("an `fmove` lowers to Instr::Fmove")
    {
        Instr::Fmove { src, dst } => {
            assert_eq!(*src, Operand::FloatReg(0));
            assert_eq!(*dst, Operand::X(0));
        }
        other => panic!("expected Instr::Fmove, got {other:?}"),
    }
}

// ---- maps -------------------------------------------------------------------

#[test]
fn lowers_the_map_opcodes_to_typed_variants() {
    let module = load_coverage();

    // `put_map_assoc {f,0},{literal,#{a=>1,b=>2}},{x,2},2,{list,[{atom,c},{x,1}]}`.
    match find(&module, |i| matches!(i, Instr::PutMapAssoc(_)))
        .expect("a `put_map_assoc` lowers to Instr::PutMapAssoc")
    {
        Instr::PutMapAssoc(b) => {
            let MapWriteArgs {
                fail,
                source,
                dst,
                live,
                pairs,
            } = &**b;
            assert_eq!(*fail, NO_FAIL);
            assert_eq!(*source, Operand::Literal(2));
            assert_eq!(*dst, Operand::X(2));
            assert_eq!(*live, 2);
            assert_eq!(pairs, &[(Operand::Atom(atom(&module, "c")), Operand::X(1))]);
        }
        other => panic!("expected Instr::PutMapAssoc, got {other:?}"),
    }

    // `put_map_exact {f,0},{x,2},{x,1},3,{list,[{atom,a},{x,1}]}`.
    match find(&module, |i| matches!(i, Instr::PutMapExact(_)))
        .expect("a `put_map_exact` lowers to Instr::PutMapExact")
    {
        Instr::PutMapExact(b) => {
            let MapWriteArgs {
                fail,
                source,
                dst,
                live,
                pairs,
            } = &**b;
            assert_eq!(*fail, NO_FAIL);
            assert_eq!(*source, Operand::X(2));
            assert_eq!(*dst, Operand::X(1));
            assert_eq!(*live, 3);
            assert_eq!(pairs, &[(Operand::Atom(atom(&module, "a")), Operand::X(1))]);
        }
        other => panic!("expected Instr::PutMapExact, got {other:?}"),
    }

    // `get_map_elements {f,27},{tr,{x,1},…},{list,[{x,0},{x,0}]}`.
    match find(&module, |i| matches!(i, Instr::GetMapElements { .. }))
        .expect("a `get_map_elements` lowers to Instr::GetMapElements")
    {
        Instr::GetMapElements {
            fail,
            source,
            pairs,
        } => {
            assert_eq!(*fail, Label(27));
            assert!(
                matches!(source, Operand::Typed { reg: Reg::X(1), .. }),
                "the map source is the typed register x1, got {source:?}"
            );
            assert_eq!(pairs, &[(Operand::X(0), Operand::X(0))]);
        }
        other => panic!("expected Instr::GetMapElements, got {other:?}"),
    }
}

// ---- bitstrings -------------------------------------------------------------

#[test]
fn lowers_the_bitstring_opcodes_to_typed_variants() {
    let module = load_coverage();

    // `bs_start_match3 {f,31},1,[{x,0}],{x,1}` (disasm folds it into `{test,…}`).
    match find(&module, |i| matches!(i, Instr::BsStartMatch3 { .. }))
        .expect("a `bs_start_match3` lowers to Instr::BsStartMatch3")
    {
        Instr::BsStartMatch3 {
            fail,
            bin,
            live,
            dst,
        } => {
            assert_eq!(*fail, Label(31));
            assert_eq!(*bin, Operand::X(0));
            assert_eq!(*live, 1);
            assert_eq!(*dst, Operand::X(1));
        }
        other => panic!("expected Instr::BsStartMatch3, got {other:?}"),
    }

    // `bs_get_position {x,1},{x,0},2`.
    match find(&module, |i| matches!(i, Instr::BsGetPosition { .. }))
        .expect("a `bs_get_position` lowers to Instr::BsGetPosition")
    {
        Instr::BsGetPosition { ctx, dst, live } => {
            assert_eq!(*ctx, Operand::X(1));
            assert_eq!(*dst, Operand::X(0));
            assert_eq!(*live, 2);
        }
        other => panic!("expected Instr::BsGetPosition, got {other:?}"),
    }

    // `bs_set_position {x,1},{x,0}`.
    match find(&module, |i| matches!(i, Instr::BsSetPosition { .. }))
        .expect("a `bs_set_position` lowers to Instr::BsSetPosition")
    {
        Instr::BsSetPosition { ctx, pos } => {
            assert_eq!(*ctx, Operand::X(1));
            assert_eq!(*pos, Operand::X(0));
        }
        other => panic!("expected Instr::BsSetPosition, got {other:?}"),
    }

    // `bs_get_tail {x,1},{x,0},2`.
    match find(&module, |i| matches!(i, Instr::BsGetTail { .. }))
        .expect("a `bs_get_tail` lowers to Instr::BsGetTail")
    {
        Instr::BsGetTail { ctx, dst, live } => {
            assert_eq!(*ctx, Operand::X(1));
            assert_eq!(*dst, Operand::X(0));
            assert_eq!(*live, 2);
        }
        other => panic!("expected Instr::BsGetTail, got {other:?}"),
    }

    // `bs_match {f,30},{x,1},{commands,[…]}` — the command list is kept flattened.
    match find(&module, |i| matches!(i, Instr::BsMatch { .. }))
        .expect("a `bs_match` lowers to Instr::BsMatch")
    {
        Instr::BsMatch {
            fail,
            ctx,
            commands,
        } => {
            assert_eq!(*fail, Label(30));
            assert_eq!(*ctx, Operand::X(1));
            assert!(!commands.is_empty(), "the flattened command list is retained");
        }
        other => panic!("expected Instr::BsMatch, got {other:?}"),
    }

    // `bs_create_bin {f,0},0,2,8,{x,0},{list,[…]}` — the segments stay flattened.
    match find(&module, |i| matches!(i, Instr::BsCreateBin { .. }))
        .expect("a `bs_create_bin` lowers to Instr::BsCreateBin")
    {
        Instr::BsCreateBin {
            fail,
            alloc,
            live,
            unit,
            dst,
            segments,
        } => {
            assert_eq!(*fail, NO_FAIL);
            assert_eq!(*alloc, 0);
            assert_eq!(*live, 2);
            assert_eq!(*unit, 8);
            assert_eq!(*dst, Operand::X(0));
            assert!(!segments.is_empty(), "the flattened segment list is retained");
        }
        other => panic!("expected Instr::BsCreateBin, got {other:?}"),
    }
}

// ---- `bs_create_bin` with an allocation-list `Alloc` ------------------------

/// Loads the committed `bsalloc.beam` oracle fixture.
fn load_bsalloc() -> Module {
    load_file(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/bsalloc.beam"
    ))
    .expect("the bsalloc oracle fixture must load")
}

#[test]
fn lowers_bs_create_bin_with_an_allocation_list_alloc() {
    // `bsalloc.beam`'s single `bs_create_bin` carries
    // `{alloc,[{words,4},{floats,0},{funs,1}]}` as its `Alloc` operand: the
    // compiler hoists a block's heap need into the `bs_create_bin` it precedes
    // (`beam_ssa_codegen.erl:342-352`). The loader must accept the `z,3` form —
    // `dets_v9.beam`/`shell_docs.beam` (and 87 other real OTP modules) failed the
    // whole load with `BadArg` before it did.
    let module = load_bsalloc();
    let instr = find(&module, |i| matches!(i, Instr::BsCreateBin { .. }))
        .expect("a `bs_create_bin` lowers to Instr::BsCreateBin");
    match instr {
        Instr::BsCreateBin {
            fail,
            alloc,
            live,
            unit,
            dst,
            segments,
        } => {
            assert_eq!(*fail, NO_FAIL);
            // 4 heap words + 0 floats + 1 fun (the loader's `number`-words
            // placeholder for a fun entry) — the word total the real loader
            // computes with `marshal_allocation_list` (`beam_file.c:2234-2284`).
            assert_eq!(*alloc, 5);
            assert_eq!(*live, 2);
            assert_eq!(*unit, 8);
            assert_eq!(*dst, Operand::X(2));
            assert!(!segments.is_empty(), "the flattened segment list is retained");
        }
        other => panic!("expected Instr::BsCreateBin, got {other:?}"),
    }
}

// ---- stack frames / heap need ----------------------------------------------

#[test]
fn lowers_stack_and_heap_need_operands_in_both_encodings() {
    let module = load_coverage();

    // `init_yregs {list,[{y,0}]}` in `safe_div/2`.
    match find(&module, |i| matches!(i, Instr::InitYregs { .. }))
        .expect("an `init_yregs` lowers to Instr::InitYregs")
    {
        Instr::InitYregs { registers } => assert_eq!(registers, &[Operand::Y(0)]),
        other => panic!("expected Instr::InitYregs, got {other:?}"),
    }

    // `allocate 1,2` — the plain `u` stack-need form (`HeapNeed::Words`).
    match find(&module, |i| matches!(i, Instr::Allocate { .. }))
        .expect("an `allocate` lowers to Instr::Allocate")
    {
        Instr::Allocate { stack_need, live } => {
            assert_eq!(*stack_need, HeapNeed::Words(1));
            assert_eq!(*live, 2);
        }
        other => panic!("expected Instr::Allocate, got {other:?}"),
    }

    // `test_heap` in BOTH encodings: three plain (`{test_heap,5,4}`,
    // `{test_heap,4,3}`, `{test_heap,2,1}`) and two `{alloc,[…]}` forms.
    let mut words_totals = Vec::new();
    let mut alloc_totals = Vec::new();
    for instr in filter(&module, |i| matches!(i, Instr::TestHeap { .. })) {
        let heap_need = match instr {
            Instr::TestHeap { heap_need, .. } => heap_need,
            other => panic!("expected Instr::TestHeap, got {other:?}"),
        };
        match heap_need {
            HeapNeed::Words(n) => words_totals.push(*n),
            HeapNeed::Alloc(entries) => {
                let total = heap_need.words();
                match total {
                    // `make_adder/1`: `{alloc,[{words,1},{floats,0},{funs,1}]}`.
                    2 => assert_eq!(
                        entries,
                        &[
                            AllocKind {
                                kind: 0,
                                number: 1,
                                words: 1
                            },
                            AllocKind {
                                kind: 1,
                                number: 0,
                                words: 0
                            },
                            AllocKind {
                                kind: 2,
                                number: 1,
                                words: 1
                            },
                        ]
                    ),
                    // `safe_div/2`: `{alloc,[{words,3},{floats,1},{funs,0}]}`.
                    5 => assert_eq!(
                        entries,
                        &[
                            AllocKind {
                                kind: 0,
                                number: 3,
                                words: 3
                            },
                            AllocKind {
                                kind: 1,
                                number: 1,
                                words: 2
                            },
                            AllocKind {
                                kind: 2,
                                number: 0,
                                words: 0
                            },
                        ]
                    ),
                    other => panic!("unexpected alloc-form `test_heap` word total {other}"),
                }
                alloc_totals.push(total);
            }
        }
    }
    words_totals.sort_unstable();
    alloc_totals.sort_unstable();
    assert_eq!(
        words_totals,
        vec![2, 4, 5],
        "the three plain-integer `test_heap`s"
    );
    assert_eq!(
        alloc_totals,
        vec![2, 5],
        "the two allocation-list `test_heap`s"
    );
}

// ---- BIFs and type tests ----------------------------------------------------

#[test]
fn lowers_bif2_and_type_tests_to_typed_variants() {
    let module = load_coverage();

    // `bif2` for `element/2` (ImpT index 2) in `tuple_ops/1`.
    match find(&module, |i| matches!(i, Instr::Bif { arity: 2, .. }))
        .expect("a `bif2` lowers to Instr::Bif")
    {
        Instr::Bif {
            arity,
            bif,
            fail,
            args,
            dst,
        } => {
            assert_eq!(*arity, 2);
            assert_eq!(*bif, ImportRef(2));
            assert_eq!(*fail, NO_FAIL);
            assert_eq!(args.len(), 2);
            assert_eq!(args[0], Operand::Integer(1));
            assert!(matches!(args[1], Operand::Typed { reg: Reg::X(0), .. }));
            assert_eq!(*dst, Operand::X(0));

            // The import really is `{erlang, element, 2}`.
            let import = &module.imports[bif.0 as usize];
            assert_eq!(module.atom_name(import.module), Some("erlang"));
            assert_eq!(module.atom_name(import.function), Some("element"));
            assert_eq!(import.arity, 2);
        }
        other => panic!("expected Instr::Bif, got {other:?}"),
    }

    // The `is_*` family lowers to `Instr::Test` with the right `TestKind`.
    for kind in [
        TestKind::IsEqExact,
        TestKind::IsGe,
        TestKind::IsInteger,
        TestKind::IsNil,
        TestKind::IsNonemptyList,
        TestKind::IsTuple,
    ] {
        assert!(
            find(&module, |i| matches!(i, Instr::Test { kind: k, .. } if *k == kind)).is_some(),
            "{kind:?} lowers to Instr::Test"
        );
    }
}

// ---- no lossy fallback ------------------------------------------------------

#[test]
fn covered_opcodes_never_fall_back_to_generic() {
    let module = load_coverage();

    // The generic-opcode numbers `coverage.beam` carries that the widened IR now
    // models (from `tests/fixtures/README.md`); none may survive as a lossless
    // `Instr::Generic`. The set is the widening exercised by this fixture:
    // bif2 + the is_* family, test_heap/init_yregs, tuples, lists, funs,
    // exceptions, floats, maps and bitstrings.
    const COVERED: &[u8] = &[
        11,  // bif2
        16,  // test_heap
        40, 45, 52, 56, 57, // is_ge, is_integer, is_nil, is_nonempty_list, is_tuple
        43,  // is_eq_exact
        60, 66, 164, // select_tuple_arity, get_tuple_element, put_tuple2
        65, 69, // get_list, put_list
        171, 178, // make_fun3, call_fun2
        104, 105, 106, 108, // try, try_end, try_case, raise
        96, 97, 101, // fmove, fconv, fdiv
        154, 155, 158, // put_map_assoc, put_map_exact, get_map_elements
        165, 166, 167, 168, 177, 182, // bs_get_tail, bs_start_match3, bs_get_position,
        //                                 bs_set_position, bs_create_bin, bs_match
        172, // init_yregs
    ];

    for instr in instructions(&module) {
        if let Instr::Generic { op, offset, .. } = instr {
            assert!(
                !COVERED.contains(&op.as_u8()),
                "opcode {} (`{}`) at offset {offset} still lowers to Instr::Generic",
                op.as_u8(),
                op.name()
            );
        }
    }
}

// ---- both fixtures + the pre-existing slice variants -------------------------

#[test]
fn both_oracle_fixtures_load_end_to_end() {
    // The two public entry points (`load_file` / `load`) agree on both fixtures.
    let slice_file = load_slice();
    let coverage_file = load_coverage();

    let slice_bytes = load(include_bytes!("fixtures/slice.beam")).expect("slice loads from bytes");
    let coverage_bytes =
        load(include_bytes!("fixtures/coverage.beam")).expect("coverage loads from bytes");

    assert_eq!(slice_file.code_version, slice_bytes.code_version);
    assert_eq!(slice_file.functions.len(), slice_bytes.functions.len());
    assert_eq!(coverage_file.code_version, coverage_bytes.code_version);
    assert_eq!(coverage_file.functions.len(), coverage_bytes.functions.len());
}

#[test]
fn the_slice_fixture_still_lowers_to_its_pre_existing_variants() {
    let module = load_slice();

    assert!(
        find(&module, |i| matches!(i, Instr::Move { .. })).is_some(),
        "`move` still lowers to Instr::Move"
    );
    assert!(
        find(&module, |i| matches!(i, Instr::GcBif { arity: 2, .. })).is_some(),
        "`gc_bif2` still lowers to Instr::GcBif"
    );
    assert!(
        find(&module, |i| matches!(i, Instr::Test { kind: TestKind::IsEqExact, .. })).is_some(),
        "`is_eq_exact` still lowers to Instr::Test"
    );
    assert!(
        find(&module, |i| matches!(i, Instr::Swap { .. })).is_some(),
        "`swap` still lowers to Instr::Swap"
    );
    assert!(
        find(&module, |i| matches!(i, Instr::CallOnly { .. })).is_some(),
        "`call_only` still lowers to Instr::CallOnly"
    );
    assert!(
        find(&module, |i| matches!(i, Instr::CallExtOnly { .. })).is_some(),
        "`call_ext_only` still lowers to Instr::CallExtOnly"
    );
    assert!(
        find(&module, |i| matches!(i, Instr::Return)).is_some(),
        "`return` still lowers to Instr::Return"
    );
}

// ---- literals and funs ------------------------------------------------------

#[test]
fn resolves_the_coverage_literals_and_fun_descriptor() {
    let module = load_coverage();

    assert_eq!(module.literals().len(), 3);
    assert!(module.literals().heap().is_some());
    assert_eq!(module.literal(3), None, "an index past the table is `None`");

    // 0: `{error, division_by_zero}`.
    let first = module.literal(0).expect("literal 0");
    assert_eq!(tuple_arity(first).expect("a tuple"), 2);
    assert_eq!(
        module.atom_name(atom_val(tuple_element(first, 0).expect("element 0"))),
        Some("error")
    );
    assert_eq!(
        module.atom_name(atom_val(tuple_element(first, 1).expect("element 1"))),
        Some("division_by_zero")
    );

    // 1: `{error, badarith}`.
    let second = module.literal(1).expect("literal 1");
    assert_eq!(tuple_arity(second).expect("a tuple"), 2);
    assert_eq!(
        module.atom_name(atom_val(tuple_element(second, 0).expect("element 0"))),
        Some("error")
    );
    assert_eq!(
        module.atom_name(atom_val(tuple_element(second, 1).expect("element 1"))),
        Some("badarith")
    );

    // 2: `#{a => 1, b => 2}`.
    let third = module.literal(2).expect("literal 2");
    let (keys, values) = flatmap_parts(third).expect("a flatmap");
    assert_eq!(keys.len(), 2);
    assert_eq!(values.len(), 2);
    assert_eq!(module.atom_name(atom_val(keys[0])), Some("a"));
    assert_eq!(module.atom_name(atom_val(keys[1])), Some("b"));

    // The single `FunT` entry, addressed by `make_fun3`'s index 0.
    assert_eq!(module.funs().len(), 1);
    let fun = module.fun(0).expect("the fixture's single lambda");
    assert_eq!(module.atom_name(fun.function), Some("-make_adder/1-fun-0-"));
    assert_eq!(fun.arity, 2);
    assert_eq!(fun.label, Label(37));
    assert_eq!(fun.index, 0);
    assert_eq!(fun.num_free, 1);
    assert_eq!(fun.uniq, 3_533_555);
    assert_eq!(module.fun(1), None, "an index past the table is `None`");
}

#[test]
fn the_slice_fixture_has_no_literals_and_no_funs() {
    let module = load_slice();

    assert!(module.literals().is_empty());
    assert_eq!(module.literal(0), None);
    assert!(module.funs().is_empty());
    assert_eq!(module.fun(0), None);
}
