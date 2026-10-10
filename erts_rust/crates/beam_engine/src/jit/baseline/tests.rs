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

//! In-module unit tests for the baseline tier: the block-target codec and
//! runtime-block layout (`runtime`), and the register allocator plus the
//! small-int arithmetic fast path (`emit`).
use super::emit::*;
use super::runtime::{RUNTIME_MACHINE_DISP, RUNTIME_XREGS_DISP};
use super::*;
use crate::jit::emitter::Reg;
use crate::jit::Tier;
use crate::machine::MAX_REGISTER;
use crate::MinimalContext;
use beam_loader::{
    Block, Export, Function, Import, ImportRef, Instr, Label, Operand, Reg as RegRef, NO_FAIL,
};
use beam_term::{make_small, MAX_SMALL, MIN_SMALL, NIL};
use std::cmp::Ordering;
use std::collections::BTreeMap;

#[test]
fn target_encoding_round_trips() {
    for (func, pc) in [(0u32, 0u32), (0, 1), (1, 0), (3, 12), (1024, 65_535)] {
        let word = encode_target(func, pc);
        assert!(word >= 0, "a valid target must be non-negative");
        assert_ne!(word, HALT);
        assert_ne!(word, ERROR);
        assert_eq!(decode_target(word), Some((func, pc)));
    }
}

#[test]
fn sentinels_are_distinct_from_every_valid_target() {
    assert_eq!(HALT, -1);
    assert_eq!(ERROR, -2);
    assert_ne!(HALT, ERROR);
    assert_eq!(decode_target(HALT), None);
    assert_eq!(decode_target(ERROR), None);
    // The smallest encodable target is `(0, 0)`; it is not a sentinel.
    assert_eq!(decode_target(encode_target(0, 0)), Some((0, 0)));
    // Any negative word decodes to nothing (the compiler never emits one).
    assert_eq!(decode_target(-3), None);
    assert_eq!(decode_target(i64::MIN), None);
}

#[test]
fn the_runtime_layout_matches_the_offsets_generated_code_uses() {
    // Generated code loads `rt.x_regs` from `[rt + 8]` (`RUNTIME_XREGS_DISP`) and
    // the helper is handed `rt` itself; this pins the two offsets.
    let module = Module::default();
    let labels = LabelMap::new();
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let rt = JitRuntime {
        machine: &mut machine,
        x_regs: machine.x.as_mut_ptr(),
        module: &module,
        labels: &labels,
        ctx: &mut ctx,
        error: None,
        enter: None,
    };
    let base = &rt as *const JitRuntime as usize;
    let machine_field = &rt.machine as *const *mut Machine as usize;
    let x_regs_field = &rt.x_regs as *const *mut Eterm as usize;
    assert_eq!(machine_field - base, RUNTIME_MACHINE_DISP as usize);
    assert_eq!(x_regs_field - base, RUNTIME_XREGS_DISP as usize);
    assert_eq!(RUNTIME_SLOT_DISP, 0);
}

#[test]
fn cached_slots_index_the_x_register_array_not_the_machine_struct() {
    // The array base the driver hands the runtime is `machine.x`'s data pointer,
    // so a block's `[base + n*8]` addressing reads the x-registers.
    let mut machine = Machine::new();
    machine.set_x(3, make_small(17)).unwrap();
    let base = machine.x.as_ptr();
    // SAFETY: `base` is `machine.x`'s data pointer, the array is not resized
    // between here and the read, and index 3 is in bounds (`MAX_REGISTER` long).
    let word = unsafe { *base.add(3) };
    assert_eq!(word, make_small(17));
}

#[test]
fn allocation_prefers_the_first_appearing_cacheable_registers() {
    let code = vec![
        Instr::Move {
            src: Operand::X(3),
            dst: Operand::X(1),
        },
        Instr::Move {
            src: Operand::Integer(7),
            dst: Operand::X(1),
        },
        Instr::Move {
            src: Operand::Nil,
            dst: Operand::Y(0),
        },
    ];
    let allocation = allocate_registers(&Module::default(), &code, 0, code.len());
    // x3 (src) then x1 (dst) — the `Y` destination is not cacheable.
    assert_eq!(
        allocation.slots,
        vec![(3, Reg::R12), (1, Reg::R13)]
    );
    assert_eq!(allocation.slot(3), Some(Reg::R12));
    assert_eq!(allocation.slot(1), Some(Reg::R13));
    assert_eq!(allocation.slot(2), None);
}

#[test]
fn allocation_ignores_registers_outside_the_engine_bound() {
    // A register index the engine can never hold must not be cached: reading it
    // would be out of bounds, so the helper reports the faithful `bad_register`.
    let code = vec![Instr::Move {
        src: Operand::Integer(1),
        dst: Operand::X(MAX_REGISTER),
    }];
    let allocation = allocate_registers(&Module::default(), &code, 0, code.len());
    assert!(allocation.slots.is_empty());
}

#[test]
fn constant_words_match_the_operand_seam() {
    assert_eq!(constant_word(&Operand::Integer(5)), Some(make_small(5).raw() as i64));
    assert_eq!(constant_word(&Operand::Nil), Some(NIL.raw() as i64));
    assert_eq!(
        constant_word(&Operand::Character(u32::from(b'a'))),
        Some(make_small(97).raw() as i64)
    );
    assert_eq!(constant_word(&Operand::X(0)), None);
    assert_eq!(constant_word(&Operand::Y(0)), None);
}

#[test]
fn x_register_reads_both_plain_and_typed_operands() {
    assert_eq!(x_register(&Operand::X(4)), Some(4));
    assert_eq!(
        x_register(&Operand::Typed {
            reg: RegRef::X(4),
            ty: beam_loader::TypeIndex(0),
        }),
        Some(4)
    );
    assert_eq!(x_register(&Operand::Y(4)), None);
    assert_eq!(x_register(&Operand::Nil), None);
}

// -- the small-int arithmetic fast path ---------------------------------

/// A module carrying `imports`, enough to exercise [`fast_arith`].
fn module_with_imports(imports: &[(&str, &str, u32)]) -> (Module, Vec<ImportRef>) {
    let mut module = Module::default();
    module.name = module.atom_table.intern("m");
    let mut refs = Vec::new();
    for (module_name, function_name, arity) in imports {
        let module_atom = module.atom_table.intern(module_name);
        let function_atom = module.atom_table.intern(function_name);
        module.imports.push(Import {
            module: module_atom,
            function: function_atom,
            arity: *arity,
        });
        refs.push(ImportRef((module.imports.len() - 1) as u32));
    }
    (module, refs)
}

/// A `GcBif` instruction over `bif` with the given operands.
fn gc_bif(bif: ImportRef, args: Vec<Operand>, dst: Operand) -> Instr {
    Instr::GcBif {
        arity: args.len() as u8,
        bif,
        fail: NO_FAIL,
        live: args.len() as u32,
        args,
        dst,
    }
}

#[test]
fn fast_path_constants_match_the_frozen_small_encoding() {
    // The guard's nibble and the two range bounds are derived from
    // `beam_term`, so they must equal what `beam_term` itself encodes.
    assert_eq!(SMALL_MASK, 0xF);
    assert_eq!(SMALL_TAG, 0xF);
    assert_eq!(SMALL_SHIFT, 4);
    assert_eq!(MIN_SMALL_WORD, make_small(MIN_SMALL).raw() as i64);
    assert_eq!(MAX_SMALL_WORD, make_small(MAX_SMALL).raw() as i64);
    assert_eq!(MIN_SMALL_WORD, i64::MIN + 15);
    assert_eq!(MAX_SMALL_WORD, i64::MAX);
    // Every small integer's word is tagged and inside the window the range
    // check accepts (which is why the emitted code needs no re-tag check).
    for value in [MIN_SMALL, MIN_SMALL + 1, -1, 0, 1, MAX_SMALL - 1, MAX_SMALL] {
        let word = make_small(value).raw() as i64;
        assert_eq!(word & SMALL_MASK, SMALL_TAG, "word for {value}");
        assert!(
            (MIN_SMALL_WORD..=MAX_SMALL_WORD).contains(&word),
            "word for {value} outside the accepted window"
        );
    }
    // The scratch registers must not overlap the cached x-registers or the
    // block's reserved `rbx`/`rsp`.
    for reg in [FAST_A, FAST_B, FAST_T, FAST_C, FAST_R] {
        assert!(!CACHE_REGISTERS.contains(&reg), "{reg:?} is a cache register");
        assert!(!SAVED_REGISTERS.contains(&reg), "{reg:?} is callee-saved");
    }
}

#[test]
fn tagged_word_arithmetic_reproduces_make_small() {
    // The emitted fast path is exact because `make_small(v) == v*16 + 15`,
    // so `a_word + b_word - 15 == make_small(va + vb)` whenever neither step
    // overflows an `i64`; otherwise the `jo`/range guards bail to the helper.
    for (a, b) in [
        (0, 0), (2, 3), (-4, 10), (7, -20), (-1, -1),
        (MAX_SMALL - 2, 1), (MIN_SMALL + 2, -1), (MAX_SMALL, 0), (MIN_SMALL, 0),
        (MAX_SMALL, MAX_SMALL), (MIN_SMALL, -1),
    ] {
        let a_word = make_small(a).raw() as i64;
        let b_word = make_small(b).raw() as i64;
        let sum = a_word.checked_add(b_word).and_then(|s| s.checked_sub(SMALL_TAG));
        let difference = a_word.checked_sub(b_word).and_then(|d| d.checked_add(SMALL_TAG));
        for (word, expected) in [
            (sum, a.checked_add(b)),
            (difference, a.checked_sub(b)),
        ] {
            match word {
                Some(word) if (MIN_SMALL_WORD..=MAX_SMALL_WORD).contains(&word) => {
                    assert_eq!(
                        word,
                        make_small(expected.expect("in-range word")).raw() as i64,
                        "{a} / {b}"
                    );
                }
                // Out of the window: the range check bails, and the true
                // value genuinely is not a small integer.
                Some(_) => assert!(
                    matches!(expected, Some(v) if v < MIN_SMALL || v > MAX_SMALL),
                    "{a} / {b} produced an out-of-window word in range"
                ),
                // An `i64` overflow: the `jo` guard bails.
                None => {}
            }
        }
    }
}

#[test]
fn fast_arith_recognizes_only_the_three_small_int_bifs() {
    let (module, refs) = module_with_imports(&[
        ("erlang", "+", 2),
        ("erlang", "-", 2),
        ("erlang", "*", 2),
        ("erlang", "+", 3),
        ("lists", "+", 2),
    ]);
    let args = vec![Operand::X(0), Operand::Integer(1)];
    for (index, expected) in [
        (0usize, Some(SmallArith::Add)),
        (1, Some(SmallArith::Sub)),
        (2, Some(SmallArith::Mul)),
        (3, None),
        (4, None),
    ] {
        let instr = gc_bif(refs[index], args.clone(), Operand::X(0));
        assert_eq!(
            fast_arith(&module, &instr).map(|fast| fast.op),
            expected,
            "import {index}"
        );
    }
    // Shapes the fast path refuses: a Y destination, a Y operand, a non-small
    // constant operand, a wrong argument count, an out-of-bound register.
    let plus = refs[0];
    let atom = module.name;
    for instr in [
        gc_bif(plus, args.clone(), Operand::Y(0)),
        gc_bif(plus, vec![Operand::X(0), Operand::Y(1)], Operand::X(0)),
        gc_bif(plus, vec![Operand::X(0), Operand::Atom(atom)], Operand::X(0)),
        gc_bif(plus, vec![Operand::X(0), Operand::Nil], Operand::X(0)),
        gc_bif(plus, vec![Operand::X(0)], Operand::X(0)),
        gc_bif(
            plus,
            vec![Operand::X(MAX_REGISTER), Operand::Integer(1)],
            Operand::X(0),
        ),
        gc_bif(plus, args.clone(), Operand::X(MAX_REGISTER)),
    ] {
        assert!(fast_arith(&module, &instr).is_none(), "{instr:?}");
    }
}

#[test]
fn allocation_caches_the_fast_path_operands_and_destination() {
    let (module, refs) = module_with_imports(&[("erlang", "+", 2)]);
    let code = vec![
        Instr::Label(Label(1)),
        gc_bif(refs[0], vec![Operand::X(4), Operand::Integer(1)], Operand::X(2)),
        Instr::Return,
    ];
    let allocation = allocate_registers(&module, &code, 0, code.len());
    // x4 (the operand) then x2 (the destination), in that order.
    assert_eq!(allocation.slots, vec![(4, Reg::R12), (2, Reg::R13)]);
    assert_eq!(allocation.slot(2), Some(Reg::R13));

    // A `gc_bif` outside the fast path caches nothing.
    let (module, refs) = module_with_imports(&[("erlang", "div", 2)]);
    let code = vec![gc_bif(
        refs[0],
        vec![Operand::X(4), Operand::Integer(1)],
        Operand::X(2),
    )];
    assert!(allocate_registers(&module, &code, 0, code.len()).slots.is_empty());
}

// -- the run-entry bookkeeping on the export path ------------------------

/// A one-function module exercising the **export** entry: `func_label` 1, entry
/// label 2, body `code`, exported as `name/0`, with the loader's label-delimited
/// block decomposition (so the native tier compiles real blocks rather than none
/// — see the same helper in `crate::machine::run`'s tests).
fn exported_module(name: &str, code: Vec<Instr>) -> (Module, Atom) {
    let mut module = Module::default();
    module.name = module.atom_table.intern("entry_mod");
    let function_name = module.atom_table.intern(name);
    let labels: BTreeMap<Label, usize> = code
        .iter()
        .enumerate()
        .filter_map(|(i, ins)| match ins {
            Instr::Label(l) => Some((*l, i)),
            _ => None,
        })
        .collect();
    let code_len = code.len();
    let mut starts: Vec<(usize, Label)> = labels.iter().map(|(l, at)| (*at, *l)).collect();
    starts.sort_by_key(|(at, _)| *at);
    starts.dedup_by_key(|(at, _)| *at);
    let mut blocks = Vec::with_capacity(starts.len());
    for (index, (start, label)) in starts.iter().enumerate() {
        let end = starts.get(index + 1).map_or(code_len, |(at, _)| *at);
        if *start < end {
            blocks.push(Block {
                label: *label,
                start: *start,
                end,
            });
        }
    }
    module.functions = vec![Function {
        func_label: Label(1),
        entry_label: Label(2),
        module: module.name,
        name: function_name,
        arity: 0,
        code,
        blocks,
        labels,
    }];
    module.exports = vec![Export {
        function: function_name,
        arity: 0,
        label: Label(2),
    }];
    (module, function_name)
}

/// The native baseline **export** entry must perform the same per-run bookkeeping
/// as the interpreter: [`Machine::enter_run`] records the run's entry `(func,
/// arity)` (`Machine::entry_call`), which backs `process_info/2`'s
/// `initial_call`/`current_function`. `run_native_outcome` used to skip that call,
/// so a process entered through the baseline export path reported a different
/// run-entry position than the interpreter for the same module/export.
#[test]
fn the_export_entry_records_the_run_entry_call_like_the_interpreter() {
    let (module, name) = exported_module(
        "f",
        vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::Move {
                src: Operand::Integer(5),
                dst: Operand::X(0),
            },
            Instr::Return,
        ],
    );

    // Both tiers run the same module/export and return the same value.
    let mut interpreter = Machine::new();
    let mut interpreter_ctx = MinimalContext::new();
    assert_eq!(
        interpreter
            .run_export_outcome_with_tier(
                &module,
                name,
                0,
                &[],
                &mut interpreter_ctx,
                Tier::Interpreter,
            )
            .expect("interpreter run"),
        RunOutcome::Returned(make_small(5))
    );

    let mut baseline = Machine::new();
    let mut baseline_ctx = MinimalContext::new();
    assert_eq!(
        baseline
            .run_export_outcome_with_tier(
                &module,
                name,
                0,
                &[],
                &mut baseline_ctx,
                Tier::BaselineJit,
            )
            .expect("baseline run"),
        RunOutcome::Returned(make_small(5))
    );

    // The interpreter's own export entry records the entry function/arity; the
    // baseline export entry must record exactly the same.
    assert_eq!(
        interpreter.entry_call(),
        Some((0, 0)),
        "the interpreter records the run's entry function/arity"
    );
    assert_eq!(
        baseline.entry_call(),
        interpreter.entry_call(),
        "the baseline export entry must record the same entry call"
    );

    // Both the raw execution position and the derived `{M,F,A}` introspection
    // agree across the two tiers for the same module/export.
    assert_eq!(
        baseline.current_code(),
        interpreter.current_code(),
        "the two tiers end at the same interpreted position"
    );
    assert_eq!(
        baseline.frame_depth(),
        interpreter.frame_depth(),
        "the two tiers leave the same frame depth"
    );
    for (label, (interpreter_term, baseline_term)) in [
        (
            "initial_call",
            (
                interpreter.initial_call(&module, &mut MinimalContext::new()),
                baseline.initial_call(&module, &mut MinimalContext::new()),
            ),
        ),
        (
            "current_function",
            (
                interpreter.current_function(&module, &mut MinimalContext::new()),
                baseline.current_function(&module, &mut MinimalContext::new()),
            ),
        ),
    ] {
        let interpreter_term = interpreter_term.expect("interpreter introspection");
        let baseline_term = baseline_term.expect("baseline introspection");
        assert!(interpreter_term.is_some(), "{label}: a completed run");
        assert!(
            baseline_term.is_some(),
            "{label}: the baseline export entry must report it too"
        );
        assert_eq!(
            beam_term::cmp(
                interpreter_term.unwrap(),
                baseline_term.unwrap(),
                &module.atom_table
            ),
            Ordering::Equal,
            "{label} must agree across the interpreter and baseline tiers"
        );
    }
}

// ---------------------------------------------------------------------------
// structural deopt coverage
//
// A compiled block lowers the instructions it knows natively and hands every
// other one to `jit_step_one` — the *interpreter's* own step — keeping the
// block going when that step advanced by exactly one (`Step::Next`). These
// tests pin that contract for representatives of each un-lowered shape: the
// native tier must produce the interpreter's exact outcome (a value or a typed
// error), never a wrong result, never a panic and never a silent continue.
// ---------------------------------------------------------------------------

/// Build a module named `name` with `imports` from `(function_name, arity, code)`
/// triples — each decomposed into the loader's label-delimited blocks, so the
/// native tier compiles real blocks — exporting every
/// `(function_name, arity, entry_label)`.
///
/// The general form of [`exported_module`]. Label ids must be unique
/// module-wide (the module's label map is global, exactly like a real `.beam`).
fn build_module(
    name: &str,
    imports: &[(&str, &str, u32)],
    functions: Vec<(&str, u32, Vec<Instr>)>,
    exports: &[(&str, u32, Label)],
) -> Module {
    let mut module = Module::default();
    module.name = module.atom_table.intern(name);
    for (module_name, function_name, arity) in imports {
        let module_atom = module.atom_table.intern(module_name);
        let function_atom = module.atom_table.intern(function_name);
        module.imports.push(Import {
            module: module_atom,
            function: function_atom,
            arity: *arity,
        });
    }
    for (function_name, arity, code) in functions {
        let name_atom = module.atom_table.intern(function_name);
        let labels: BTreeMap<Label, usize> = code
            .iter()
            .enumerate()
            .filter_map(|(i, ins)| match ins {
                Instr::Label(l) => Some((*l, i)),
                _ => None,
            })
            .collect();
        let code_len = code.len();
        let mut starts: Vec<(usize, Label)> = labels.iter().map(|(l, at)| (*at, *l)).collect();
        starts.sort_by_key(|(at, _)| *at);
        starts.dedup_by_key(|(at, _)| *at);
        let mut blocks = Vec::with_capacity(starts.len());
        for (index, (start, label)) in starts.iter().enumerate() {
            let end = starts.get(index + 1).map_or(code_len, |(at, _)| *at);
            if *start < end {
                blocks.push(Block {
                    label: *label,
                    start: *start,
                    end,
                });
            }
        }
        let func_label = starts.first().map(|(_, l)| *l).unwrap_or(Label(0));
        let entry_label = exports
            .iter()
            .find(|(n, a, _)| *n == function_name && *a == arity)
            .map(|(_, _, l)| *l)
            .unwrap_or(func_label);
        module.functions.push(Function {
            func_label,
            entry_label,
            module: module.name,
            name: name_atom,
            arity,
            code,
            blocks,
            labels,
        });
    }
    for (function_name, arity, label) in exports {
        let function_atom = module.atom_table.intern(function_name);
        module.exports.push(Export {
            function: function_atom,
            arity: *arity,
            label: *label,
        });
    }
    module
}

/// The atom `name` in `module`'s table (interned by [`build_module`]).
fn export_atom(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("{name} must be interned"))
}

/// A one-function module exported as `f/0`, with the body starting at
/// `Label(2)` (so the run entry is a label *after* the function label, exactly
/// as the loader decomposes a real function).
fn body_module(code: Vec<Instr>) -> (Module, Atom) {
    let module = build_module("deopt", &[], vec![("f", 0, code)], &[("f", 0, Label(2))]);
    let atom = export_atom(&module, "f");
    (module, atom)
}

/// The reason of a typed error as its atom, else a `Debug` rendering — the only
/// way to compare two `BeamError`s (they are not `PartialEq`: a raise carries an
/// opaque live-reason slot).
fn error_reason(err: &beam_common::BeamError) -> String {
    match err {
        beam_common::BeamError::Raise(raise) => raise
            .reason
            .as_atom()
            .map(str::to_string)
            .unwrap_or_else(|| format!("{raise:?}")),
        other => format!("{other:?}"),
    }
}

/// Run exported `name/0` on both tiers with a **fresh** machine/context each and
/// hand the two outcomes to `check`.
///
/// Each run builds on its own [`MinimalContext`] heap, and a returned **boxed**
/// term is a pointer into that heap. A `MinimalContext` owns its heap and must
/// outlive every boxed term built through it, so the comparison has to happen
/// *inside* this function: returning the outcomes and comparing them after the
/// contexts have been dropped would dereference freed memory (the boxed-term
/// cases would then be compared against recycled heap words, not values).
fn with_both_tiers<R>(
    module: &Module,
    name: Atom,
    check: impl FnOnce(&Result<RunOutcome>, &Result<RunOutcome>) -> R,
) -> R {
    let mut interpreter = Machine::new();
    let mut interpreter_ctx = MinimalContext::new();
    let interpreter_outcome = interpreter.run_export_outcome_with_tier(
        module,
        name,
        0,
        &[],
        &mut interpreter_ctx,
        Tier::Interpreter,
    );
    let mut baseline = Machine::new();
    let mut baseline_ctx = MinimalContext::new();
    let baseline_outcome = baseline.run_export_outcome_with_tier(
        module,
        name,
        0,
        &[],
        &mut baseline_ctx,
        Tier::BaselineJit,
    );
    check(&interpreter_outcome, &baseline_outcome)
}

/// Assert the two tiers' outcomes are observably identical: structurally equal
/// terms (they are built on two separate heaps, so raw-word equality would be
/// wrong) or the same typed-error reason.
fn assert_same_outcome(
    label: &str,
    interpreter: &Result<RunOutcome>,
    baseline: &Result<RunOutcome>,
    atoms: &beam_term::AtomTable,
) {
    match (interpreter, baseline) {
        (Ok(RunOutcome::Returned(a)), Ok(RunOutcome::Returned(b))) => assert_eq!(
            beam_term::cmp(*a, *b, atoms),
            Ordering::Equal,
            "{label}: the tiers returned different terms"
        ),
        (Err(ia), Err(ib)) => assert_eq!(
            error_reason(ia),
            error_reason(ib),
            "{label}: the tiers raised different errors"
        ),
        (Ok(a), Ok(b)) => assert_eq!(a, b, "{label}: the tiers reported different outcomes"),
        (a, b) => panic!("{label}: the tiers disagree: interpreter {a:?}, baseline {b:?}"),
    }
}

/// `put_tuple2` — a boxed-term constructor the block never inlines.
fn put_tuple2_case() -> (Module, Atom) {
    body_module(vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::PutTuple2 {
            dst: Operand::X(0),
            elements: vec![Operand::Integer(1), Operand::Integer(2)],
        },
        Instr::Return,
    ])
}

/// `put_list` — a cons-cell constructor the block never inlines.
fn put_list_case() -> (Module, Atom) {
    body_module(vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::PutList {
            head: Operand::Integer(1),
            tail: Operand::Nil,
            dst: Operand::X(0),
        },
        Instr::Return,
    ])
}

/// `call` — the frame-preserving local call the block never inlines (only the
/// **tail** forms, `call_last`/`call_only`, are linked natively). The callee is
/// a leaf whose `return` follows the caller's continuation, so the caller's own
/// frame is popped by `deallocate_return`.
fn local_call_case() -> (Module, Atom) {
    let module = build_module(
        "deopt_call",
        &[],
        vec![
            (
                "f",
                0,
                vec![
                    Instr::Label(Label(1)),
                    Instr::Label(Label(2)),
                    Instr::Allocate {
                        stack_need: beam_loader::HeapNeed::Words(0),
                        live: 0,
                    },
                    Instr::Call {
                        arity: 0,
                        target: Label(3),
                    },
                    Instr::DeallocateReturn(0),
                ],
            ),
            (
                "g",
                0,
                vec![
                    Instr::Label(Label(3)),
                    Instr::Move {
                        src: Operand::Integer(7),
                        dst: Operand::X(0),
                    },
                    Instr::Return,
                ],
            ),
        ],
        &[("f", 0, Label(2))],
    );
    let atom = export_atom(&module, "f");
    (module, atom)
}

/// `has_map_fields` — a map op the block never inlines. The source is not a map,
/// so the family takes the live `fail` branch.
fn has_map_fields_case() -> (Module, Atom) {
    body_module(vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::Move {
            src: Operand::Integer(0),
            dst: Operand::X(0),
        },
        Instr::HasMapFields {
            fail: Label(4),
            source: Operand::X(0),
            keys: vec![Operand::Integer(1)],
        },
        Instr::Move {
            src: Operand::Integer(1),
            dst: Operand::X(0),
        },
        Instr::Return,
        Instr::Label(Label(4)),
        Instr::Move {
            src: Operand::Integer(2),
            dst: Operand::X(0),
        },
        Instr::Return,
    ])
}

/// `bs_test_tail2` — a bitstring-match op the block never inlines, with a
/// non-match-context operand: a typed `unsupported` on **both** tiers.
fn bs_test_tail2_case() -> (Module, Atom) {
    body_module(vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::Move {
            src: Operand::Integer(0),
            dst: Operand::X(0),
        },
        Instr::BsTestTail2 {
            fail: NO_FAIL,
            ctx: Operand::X(0),
            bits: 8,
        },
        Instr::Return,
    ])
}

/// `bs_start_match3` on a non-binary source: the family takes the live `fail`
/// branch (the deopt path, exercised on the native tier too).
fn bs_start_match3_case() -> (Module, Atom) {
    body_module(vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::Move {
            src: Operand::Integer(0),
            dst: Operand::X(0),
        },
        Instr::BsStartMatch3 {
            fail: Label(4),
            bin: Operand::X(0),
            live: 0,
            dst: Operand::X(1),
        },
        Instr::Move {
            src: Operand::Integer(1),
            dst: Operand::X(0),
        },
        Instr::Return,
        Instr::Label(Label(4)),
        Instr::Move {
            src: Operand::Integer(2),
            dst: Operand::X(0),
        },
        Instr::Return,
    ])
}

#[test]
fn un_lowered_instructions_deopt_to_the_interpreter_with_identical_outcomes() {
    let cases = [
        ("put_tuple2", put_tuple2_case()),
        ("put_list", put_list_case()),
        ("call", local_call_case()),
        ("has_map_fields", has_map_fields_case()),
        ("bs_test_tail2", bs_test_tail2_case()),
        ("bs_start_match3", bs_start_match3_case()),
    ];
    for (label, (module, name)) in cases {
        with_both_tiers(&module, name, |interpreter, baseline| {
            assert_same_outcome(label, interpreter, baseline, &module.atom_table);
        });
    }

    // Concrete anchors — the cross-tier comparison above would be vacuous if
    // both sides raised the same trivial error.
    let (module, name) = has_map_fields_case();
    with_both_tiers(&module, name, |interpreter, baseline| {
        assert_eq!(
            interpreter.as_ref().expect("interpreter run"),
            &RunOutcome::Returned(make_small(2)),
            "`has_map_fields` on a non-map takes the live fail branch"
        );
        assert_eq!(
            baseline.as_ref().expect("baseline run"),
            &RunOutcome::Returned(make_small(2)),
            "the deopt produced the interpreter's exact value"
        );
    });

    let (module, name) = local_call_case();
    with_both_tiers(&module, name, |interpreter, baseline| {
        assert_eq!(
            interpreter.as_ref().expect("interpreter run"),
            &RunOutcome::Returned(make_small(7)),
            "the leaf callee's `move`/`return` transferred x(0)"
        );
        assert_eq!(
            baseline.as_ref().expect("baseline run"),
            &RunOutcome::Returned(make_small(7)),
            "the deopted `call`/`deallocate_return` frame discipline is identical"
        );
    });

    let (module, name) = put_tuple2_case();
    with_both_tiers(&module, name, |interpreter, baseline| {
        for (label, outcome) in [("interpreter", interpreter), ("baseline", baseline)] {
            match outcome.as_ref().expect("run") {
                RunOutcome::Returned(value) => assert!(
                    !beam_term::is_small(*value),
                    "{label}: `put_tuple2` must return a boxed tuple, not a small int"
                ),
                other => panic!("{label}: unexpected outcome {other:?}"),
            }
        }
    });

    // The non-match-context bitstring test is the same typed error on both tiers.
    let (module, name) = bs_test_tail2_case();
    with_both_tiers(&module, name, |interpreter, baseline| {
        for err in [interpreter, baseline] {
            let err = err
                .as_ref()
                .expect_err("a non-context operand is a typed error");
            assert_eq!(
                error_reason(err),
                "unsupported_opcode: bs_test_tail2: operand is not a match context"
            );
        }
    });
}

/// A context that answers `erlang:'+'/2` with a fixed small-int result (recording
/// the raw argument words it was handed) and delegates the heap and every other
/// BIF to a [`MinimalContext`].
struct SumCtx {
    inner: MinimalContext,
    args: Vec<Vec<Eterm>>,
}

impl SumCtx {
    fn new() -> SumCtx {
        SumCtx {
            inner: MinimalContext::new(),
            args: Vec::new(),
        }
    }
}

impl ExecutionContext for SumCtx {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        if module == "erlang" && function == "+" && arity == 2 {
            self.args.push(args.to_vec());
            return Ok(make_small(42));
        }
        self.inner.call_bif(module, function, arity, args)
    }

    fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        x_regs: &mut [Eterm],
        stack: &mut [Eterm],
    ) -> Result<()> {
        self.inner.ensure_heap(heap_need, live, x_regs, stack)
    }

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        self.inner.alloc_words(n)
    }
}

/// A `gc_bif` whose argument is a **boxed** (non-immediate) term cannot take the
/// inlined small-int fast path: the run-time guard rejects it and the block
/// deopts to the interpreter step, which dispatches the BIF to the host. Both
/// tiers must reach the host with the same (boxed) argument and return its
/// value.
#[test]
fn a_gc_bif_with_a_boxed_argument_deopts_and_reaches_the_host_bif() {
    let module = build_module(
        "boxed_bif",
        &[("erlang", "+", 2)],
        vec![(
            "f",
            0,
            vec![
                Instr::Label(Label(1)),
                Instr::Label(Label(2)),
                Instr::PutTuple2 {
                    dst: Operand::X(0),
                    elements: vec![Operand::Integer(1)],
                },
                Instr::GcBif {
                    arity: 2,
                    bif: ImportRef(0),
                    fail: NO_FAIL,
                    live: 2,
                    args: vec![Operand::X(0), Operand::Integer(0)],
                    dst: Operand::X(1),
                },
                Instr::Move {
                    src: Operand::X(1),
                    dst: Operand::X(0),
                },
                Instr::Return,
            ],
        )],
        &[("f", 0, Label(2))],
    );
    let name = export_atom(&module, "f");

    let mut interpreter = Machine::new();
    let mut interpreter_ctx = SumCtx::new();
    assert_eq!(
        interpreter
            .run_export_outcome_with_tier(
                &module,
                name,
                0,
                &[],
                &mut interpreter_ctx,
                Tier::Interpreter,
            )
            .expect("interpreter run"),
        RunOutcome::Returned(make_small(42))
    );
    let mut baseline = Machine::new();
    let mut baseline_ctx = SumCtx::new();
    assert_eq!(
        baseline
            .run_export_outcome_with_tier(
                &module,
                name,
                0,
                &[],
                &mut baseline_ctx,
                Tier::BaselineJit,
            )
            .expect("baseline run"),
        RunOutcome::Returned(make_small(42))
    );

    for (label, ctx) in [("interpreter", &interpreter_ctx), ("baseline", &baseline_ctx)] {
        assert_eq!(ctx.args.len(), 1, "{label}: exactly one erlang:'+'/2 call");
        assert_eq!(ctx.args[0].len(), 2, "{label}: two arguments");
        assert!(
            !beam_term::is_small(ctx.args[0][0]),
            "{label}: the first argument is the boxed tuple `put_tuple2` built"
        );
        assert_eq!(
            ctx.args[0][1],
            make_small(0),
            "{label}: the second argument is the immediate operand"
        );
    }
}

/// A context whose cross-module-call seam resolves `callee_x:g/0` to the shared
/// `Arc` handle — the same handle the engine keeps alive so a suspended frame
/// can return into the callee's module.
fn seam_context(callee: &std::sync::Arc<Module>) -> MinimalContext {
    let handle = std::sync::Arc::clone(callee);
    MinimalContext::with_external_resolver(Box::new(move |module, function, arity| {
        if module == "callee_x" && function == "g" && arity == 0 {
            Ok(Some((std::sync::Arc::clone(&handle), Label(1))))
        } else {
            Ok(None)
        }
    }))
}

/// A `call_ext` the host resolves through [`ExecutionContext::resolve_external_call`]
/// runs the callee **in this machine** on a module-context switch, and the native tier
/// drives that switch exactly as the interpreter does: the compiled block's per-instruction
/// deopt reports the resolved target with the [`ENTER`] word (never a wrong result, never a
/// silent continue), the driver performs the context switch and enters the callee's own
/// compiled blocks — or, for a callee the tier has no blocks for, its documented
/// per-instruction interpreter step — and the callee's `return` unwinds back into the
/// caller's frame.
#[test]
fn a_resolved_cross_module_call_runs_on_the_baseline_tier() {
    let callee_module = build_module(
        "callee_x",
        &[],
        vec![(
            "g",
            0,
            vec![
                Instr::Label(Label(1)),
                Instr::Move {
                    src: Operand::Integer(99),
                    dst: Operand::X(0),
                },
                Instr::Return,
            ],
        )],
        &[("g", 0, Label(1))],
    );
    let callee = std::sync::Arc::new(callee_module);
    let caller = build_module(
        "caller_x",
        &[("callee_x", "g", 0)],
        vec![(
            "f",
            0,
            vec![
                Instr::Label(Label(1)),
                Instr::Label(Label(2)),
                Instr::Allocate {
                    stack_need: beam_loader::HeapNeed::Words(0),
                    live: 0,
                },
                Instr::CallExt {
                    arity: 0,
                    target: ImportRef(0),
                },
                Instr::DeallocateReturn(0),
            ],
        )],
        &[("f", 0, Label(2))],
    );
    let name = export_atom(&caller, "f");

    // The interpreter resolves the seam and runs the callee in this machine: the
    // callee's `return` comes back to the caller's frame and `x(0)` survives.
    let mut interpreter = Machine::new();
    let mut interpreter_ctx = seam_context(&callee);
    assert_eq!(
        interpreter
            .run_export_outcome_with_tier(
                &caller,
                name,
                0,
                &[],
                &mut interpreter_ctx,
                Tier::Interpreter,
            )
            .expect("interpreter run through the seam"),
        RunOutcome::Returned(make_small(99))
    );

    // The baseline tier drives the same module-context switch (through the block's
    // `ENTER` word), so it produces the identical result.
    let mut baseline = Machine::new();
    let mut baseline_ctx = seam_context(&callee);
    assert_eq!(
        baseline
            .run_export_outcome_with_tier(
                &caller,
                name,
                0,
                &[],
                &mut baseline_ctx,
                Tier::BaselineJit,
            )
            .expect("baseline run through the seam"),
        RunOutcome::Returned(make_small(99))
    );
}

// ---------------------------------------------------------------------------
// tier-entry parity: the native entry performs the same per-run bookkeeping as
// the interpreter's
// ---------------------------------------------------------------------------

/// The native export entry must record the same run-entry position as the
/// interpreter's **for an export that is not the module's first function**:
/// `entry_call` names function index `1`, which only the interpreter's own
/// `enter_run` (the phase-8 green thread) can record — so the two agree only if
/// `run_native_outcome` ran the interpreter's own `enter_run`.
///
/// `current_code` at the end of the run is the position of the last position *recorded*,
/// and both tiers record the trailing `return`: the interpreter steps it, and the native
/// tier's compiled block reaches it through its documented per-instruction deopt. The
/// block prologue's own block-entry record (`(1, 0)`) is therefore superseded — the
/// separate linked-chain test pins the case where a natively executed block is the last
/// thing to record.
#[test]
fn the_baseline_entry_records_the_same_run_position_as_the_interpreter() {
    let module = build_module(
        "entry_parity",
        &[],
        vec![
            ("noop", 0, vec![Instr::Label(Label(1)), Instr::Return]),
            (
                "f",
                0,
                vec![
                    Instr::Label(Label(2)),
                    Instr::Move {
                        src: Operand::Integer(5),
                        dst: Operand::X(0),
                    },
                    Instr::Return,
                ],
            ),
        ],
        &[("f", 0, Label(2))],
    );
    let name = export_atom(&module, "f");

    let mut interpreter = Machine::new();
    let mut interpreter_ctx = MinimalContext::new();
    assert_eq!(
        interpreter
            .run_export_outcome_with_tier(
                &module,
                name,
                0,
                &[],
                &mut interpreter_ctx,
                Tier::Interpreter,
            )
            .expect("interpreter run"),
        RunOutcome::Returned(make_small(5))
    );
    let mut baseline = Machine::new();
    let mut baseline_ctx = MinimalContext::new();
    assert_eq!(
        baseline
            .run_export_outcome_with_tier(
                &module,
                name,
                0,
                &[],
                &mut baseline_ctx,
                Tier::BaselineJit,
            )
            .expect("baseline run"),
        RunOutcome::Returned(make_small(5))
    );

    // The export lives in function index 1: the entry call and the ending
    // position must name it on both tiers.
    assert_eq!(
        interpreter.entry_call(),
        Some((1, 0)),
        "the interpreter records the run's entry function/arity"
    );
    assert_eq!(
        baseline.entry_call(),
        interpreter.entry_call(),
        "the native entry must run the same `enter_run`"
    );
    assert_eq!(interpreter.current_code(), Some((1, 2)));
    assert_eq!(
        baseline.current_code(),
        interpreter.current_code(),
        "both tiers end at the same interpreted position"
    );
    assert_eq!(
        baseline.frame_depth(),
        interpreter.frame_depth(),
        "both tiers leave the same frame depth"
    );

    for (label, interpreter_term, baseline_term) in [
        (
            "initial_call",
            interpreter.initial_call(&module, &mut MinimalContext::new()),
            baseline.initial_call(&module, &mut MinimalContext::new()),
        ),
        (
            "current_function",
            interpreter.current_function(&module, &mut MinimalContext::new()),
            baseline.current_function(&module, &mut MinimalContext::new()),
        ),
    ] {
        let interpreter_term = interpreter_term
            .expect("interpreter introspection")
            .expect("a completed run");
        let baseline_term = baseline_term
            .expect("baseline introspection")
            .expect("the native entry must report it too");
        assert_eq!(
            beam_term::cmp(interpreter_term, baseline_term, &module.atom_table),
            Ordering::Equal,
            "{label} must agree across the interpreter and baseline tiers"
        );
    }
}

// ---------------------------------------------------------------------------
// root-set spill at a helper boundary
// ---------------------------------------------------------------------------

/// A context that records the x-register slice it is handed at the **heap
/// admission point** (`ensure_heap`, which `Machine::ensure_heap` reaches
/// through the defaulted `ensure_heap_full`), delegating the heap itself to a
/// [`MinimalContext`].
struct RootProbeCtx {
    inner: MinimalContext,
    x_slices: Vec<Vec<Eterm>>,
    lives: Vec<u32>,
    needs: Vec<u32>,
}

impl RootProbeCtx {
    fn new() -> RootProbeCtx {
        RootProbeCtx {
            inner: MinimalContext::new(),
            x_slices: Vec::new(),
            lives: Vec::new(),
            needs: Vec::new(),
        }
    }
}

impl ExecutionContext for RootProbeCtx {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        self.inner.call_bif(module, function, arity, args)
    }

    fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        x_regs: &mut [Eterm],
        stack: &mut [Eterm],
    ) -> Result<()> {
        self.needs.push(heap_need);
        self.lives.push(live);
        self.x_slices.push(x_regs.to_vec());
        self.inner.ensure_heap(heap_need, live, x_regs, stack)
    }

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        self.inner.alloc_words(n)
    }
}

/// A block caches x-registers in machine registers, so it **must** spill them
/// back to `machine.x` before it calls a helper: the helper *is* the interpreter
/// and reads `machine.x`, and a host's `ensure_heap` sees exactly the
/// interpreter's root set through it.
///
/// The experiment is a block that caches `x0..x3` with four distinct small ints
/// and then reaches a `test_heap` (an un-lowered, heap-admitting instruction).
/// A missing spill would hand the host the reset (`NIL`) values instead of the
/// moved ones — a deterministic, observable failure rather than a silent pass.
#[test]
fn the_block_spills_its_cached_registers_before_a_helper_call() {
    let module = build_module(
        "spill_probe",
        &[],
        vec![(
            "f",
            0,
            vec![
                Instr::Label(Label(1)),
                Instr::Label(Label(2)),
                Instr::Move {
                    src: Operand::Integer(11),
                    dst: Operand::X(0),
                },
                Instr::Move {
                    src: Operand::Integer(22),
                    dst: Operand::X(1),
                },
                Instr::Move {
                    src: Operand::Integer(33),
                    dst: Operand::X(2),
                },
                Instr::Move {
                    src: Operand::Integer(44),
                    dst: Operand::X(3),
                },
                Instr::TestHeap {
                    heap_need: beam_loader::HeapNeed::Words(8),
                    live: 4,
                },
                Instr::Return,
            ],
        )],
        &[("f", 0, Label(2))],
    );
    let name = export_atom(&module, "f");

    // The block really must cache the four registers, or the spilled slice could
    // be correct by accident (a register addressed in `machine.x` needs no spill).
    let compiled = compile_module(&module).expect("compile the module");
    for register in 0..4u32 {
        assert!(
            compiled.cached_registers().contains(&register),
            "x{register} must be held in a machine register by the body block, \
             got {:?}",
            compiled.cached_registers()
        );
    }

    let mut machine = Machine::new();
    let mut ctx = RootProbeCtx::new();
    let outcome = run_compiled_outcome(&compiled, &mut machine, &module, name, 0, &[], &mut ctx)
        .expect("the run deopts and returns");
    assert_eq!(
        outcome,
        RunOutcome::Returned(make_small(11)),
        "the helper ran the `test_heap` and the run returned x(0)"
    );

    assert_eq!(
        ctx.x_slices.len(),
        1,
        "exactly one heap admission point was reached"
    );
    assert_eq!(ctx.lives, vec![4], "the instruction's own live count");
    assert_eq!(ctx.needs, vec![8], "the instruction's own heap need");
    let spilled = &ctx.x_slices[0];
    for (register, value) in [(0usize, 11i64), (1, 22), (2, 33), (3, 44)] {
        assert_eq!(
            spilled[register],
            make_small(value),
            "x{register} must hold its moved value at the helper boundary — the \
             block must have spilled its resident"
        );
    }
}

// ---------------------------------------------------------------------------
// the block-entry position store
// ---------------------------------------------------------------------------

/// A position recorded from a **natively executed block's own prologue**.
///
/// Every compiled block opens by calling `jit_set_position(rt, func, pc)` with its
/// own `(func, pc)`, so `Machine::current_code` advances at **block granularity**
/// even when a linked branch carries control from one block to the next without ever
/// returning to the driver. The observation uses a block whose execution ends through
/// the linked tail call's **checked frame pop**: a `deallocate` that no longer matches
/// the live frame is raised by `jit_tail_call`, which is not an *interpreted*
/// instruction (the interpreter's own `call_last` does the pop in its instruction
/// body). So the recorded position is not overwritten afterwards, and the tier's
/// position is exactly the entry of the last block the linked chain reached.
///
/// The interpreter steps the `call_last` instruction, so it records that instruction's
/// address; the native tier, which linked the `jump` into the successor block and then
/// popped the frame in the helper, records that successor block's entry. Both report the
/// same error — the position is the only difference, and it is the block-granularity
/// contract this store exists for.
#[test]
fn a_linked_chain_records_the_last_block_entry_as_the_position() {
    // `f/0`: block A (`Label(1)`, an inlined `move`, a linked `jump`) transfers straight
    // into block B (`Label(2)`, the linked `call_last` whose frame pop fails), so the
    // last block the chain reached is B, at pc 3.
    let module = build_module(
        "linked_position",
        &[],
        vec![(
            "f",
            0,
            vec![
                Instr::Label(Label(1)),
                Instr::Move {
                    src: Operand::Integer(5),
                    dst: Operand::X(0),
                },
                Instr::Jump { target: Label(2) },
                Instr::Label(Label(2)),
                Instr::CallLast {
                    arity: 0,
                    target: Label(3),
                    deallocate: 5,
                },
                Instr::Label(Label(3)),
                Instr::Return,
            ],
        )],
        &[("f", 0, Label(1))],
    );
    let name = export_atom(&module, "f");

    let mut baseline = Machine::new();
    let mut baseline_ctx = MinimalContext::new();
    let baseline_err = baseline
        .run_export_outcome_with_tier(
            &module,
            name,
            0,
            &[],
            &mut baseline_ctx,
            Tier::BaselineJit,
        )
        .expect_err("the linked tail call's checked frame pop raises");
    assert_eq!(
        baseline.current_code(),
        Some((0, 3)),
        "the native tier records the entry of the last linked block, not an \
         interpreted instruction"
    );

    let mut interpreter = Machine::new();
    let mut interpreter_ctx = MinimalContext::new();
    let interpreter_err = interpreter
        .run_export_outcome_with_tier(
            &module,
            name,
            0,
            &[],
            &mut interpreter_ctx,
            Tier::Interpreter,
        )
        .expect_err("the interpreter's own `call_last` pop raises too");
    assert_eq!(
        interpreter.current_code(),
        Some((0, 4)),
        "the interpreter steps the `call_last` instruction and records its address"
    );

    // The tier and the interpreter raise the identical error — only the recorded
    // position differs, exactly as the block-granularity contract intends.
    assert_eq!(error_reason(&baseline_err), error_reason(&interpreter_err));
}
