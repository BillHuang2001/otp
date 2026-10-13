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
//! runtime-block layout (`runtime`), the register allocator plus the
//! small-int arithmetic fast path (`emit`), and the native cross-module
//! `call_ext*` transfer (a driver-mediated module-context switch).
//!
//! This file is **deliberately long**: it is one comprehensive `#[cfg(test)]` suite over
//! the whole baseline backend (the split `mod.rs`/`runtime.rs`/`emit/` is exercised here
//! in one place, so a change to any of them has its unit tests beside it) — a
//! legitimately-large test module, not a file hiding a second concern. Suites that need a
//! compiled real `.beam` live in the crate's `tests/` (`jit_real_module`, `jit_bench`).
use super::emit::*;
use super::runtime::{
    EXT_CALL, EXT_CALL_LAST, EXT_CALL_ONLY, RUNTIME_MACHINE_DISP, RUNTIME_XREGS_DISP,
};
use super::*;
use crate::jit::emitter::Reg;
use crate::jit::Tier;
use crate::machine::{Continuation, MAX_REGISTER};
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
// a collecting helper must not leave a cached heap term stale
// ---------------------------------------------------------------------------

/// The sentinel a [`MovingCtx`] writes into the **from-space** element word of the
/// tuple it relocates. A compiled block that re-uses its pre-relocation cached word
/// (instead of reloading from `machine.x` after the collecting helper) observes this.
const POISONED_ELEMENT: i64 = 777;

/// A context that genuinely **moves a heap term** at the heap-admission point.
///
/// A Stage-4 per-process copying collector relocates the live young heap and rewrites
/// every root **in place**: the heap tuple in `x_regs[0]` is moved into a fresh block
/// and the moved word is written back into the root set. The from-space copy is then
/// poisoned — its element word set to [`POISONED_ELEMENT`], its header left a valid
/// `arityval(1)` so an inlined tuple read on the stale pointer still succeeds — and the
/// context's arena never frees, so a stale read is a deterministic wrong answer
/// (`777`) rather than a wild pointer: exactly the observable this experiment needs.
struct MovingCtx {
    inner: MinimalContext,
    relocations: u32,
}

impl MovingCtx {
    fn new() -> MovingCtx {
        MovingCtx {
            inner: MinimalContext::new(),
            relocations: 0,
        }
    }
}

impl ExecutionContext for MovingCtx {
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
        // Reachable through the defaulted `ensure_heap_full`, which forwards the whole
        // x-register file here — so `x_regs[0]` is a live, published root.
        let old = x_regs[0];
        if unsafe { beam_term::is_tuple(old) } {
            let old_hdr = beam_term::tuple_val(old);
            // SAFETY: `old` is a live boxed tuple on the context heap (its arena is not
            // freed before the context drops) and `old_hdr` addresses its `arityval(1)`
            // header, so `[0]` is that header word and `[1]` its sole element.
            let (header, element) = unsafe { (*old_hdr, *old_hdr.add(1)) };
            let fresh = self.inner.alloc_words(2)?;
            // SAFETY: `fresh` is a non-null 2-word block owned by the context heap and
            // not aliased elsewhere; the two stores initialize both its words.
            unsafe {
                *fresh = header;
                *fresh.add(1) = element;
                // The stale-word trap: a block that reads its pre-relocation cached
                // word sees this instead of the moved term's real element.
                *old_hdr.add(1) = make_small(POISONED_ELEMENT);
            }
            let moved = beam_term::make_tuple(fresh);
            // Preserve the boxed word's literal-area bit, as a real relocation must.
            let word = moved.raw() | (old.raw() & beam_term::TAG_LITERAL_PTR);
            x_regs[0] = Eterm::from_raw(word);
            self.relocations += 1;
        }
        self.inner.ensure_heap(heap_need, live, x_regs, stack)
    }

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        self.inner.alloc_words(n)
    }
}

/// A compiled block's cached x-registers **must be reloaded** after a helper that can
/// collect: a collecting host relocates the young heap and rewrites `machine.x` in
/// place, so a callee-saved cache register is stale the moment the helper returns. The
/// invariant this guards is the `fill` in `emit_continue_check` (see `super::emit`): a
/// block that continued at `pc + 1` on the stale cache would read a from-space term —
/// here the poisoned element of the pre-relocation tuple — and produce a wrong result.
///
/// The design **isolates the collection boundary**: the tuple is the run's argument, so
/// the block's prologue loads it into a cache register and the *only* helper between
/// that load and the native read is the collecting `test_heap`. `get_tuple_element
/// X(0) 0 X(0)` is then an inlined read of the cached register
/// (`try_inline_get_tuple_element`, see `super::emit`); a missing post-helper reload
/// returns the poisoned from-space element ([`POISONED_ELEMENT`]) instead of the real
/// element (`5`).
#[test]
fn a_collecting_helper_does_not_leave_a_cached_heap_term_stale() {
    let module = build_module(
        "moving_collect",
        &[],
        vec![(
            "f",
            1,
            vec![
                Instr::Label(Label(1)),
                Instr::Label(Label(2)),
                Instr::TestHeap {
                    heap_need: beam_loader::HeapNeed::Words(2),
                    live: 1,
                },
                Instr::GetTupleElement {
                    source: Operand::X(0),
                    element: 0,
                    dst: Operand::X(0),
                },
                Instr::Return,
            ],
        )],
        &[("f", 1, Label(2))],
    );
    let name = export_atom(&module, "f");

    // The read must come from a **cached** register, or the experiment proves nothing:
    // a register addressed directly in `machine.x` is always up to date after a move.
    let compiled = compile_module(&module).expect("compile the module");
    assert!(
        compiled.cached_registers().contains(&0),
        "x0 must be held in a machine register by the body block, got {:?}",
        compiled.cached_registers()
    );

    let mut ctx = MovingCtx::new();
    // Seed the run's argument with a heap tuple `{5}` the context owns. Its arena is
    // pointer-stable and never freed, so the term stays valid (and readable) for the
    // whole run — which is what makes a stale read a wrong answer rather than UB.
    let arg = {
        let block = ctx.inner.alloc_words(2).expect("reserve the tuple block");
        // SAFETY: `block` is a non-null 2-word block owned by the context heap and not
        // aliased elsewhere; the two stores initialize both of its words.
        unsafe {
            *block = beam_term::make_arityval(1);
            *block.add(1) = make_small(5);
        }
        beam_term::make_tuple(block)
    };

    let mut machine = Machine::new();
    let outcome = run_compiled_outcome(&compiled, &mut machine, &module, name, 1, &[arg], &mut ctx)
        .expect("the run deopts and returns");
    assert_eq!(
        ctx.relocations, 1,
        "the `test_heap` must reach the collecting admission point exactly once"
    );
    assert_eq!(
        outcome,
        RunOutcome::Returned(make_small(5)),
        "the inlined tuple read must reload x0 after the collecting helper — a stale \
         cached word returns the poisoned from-space element"
    );
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

// ---------------------------------------------------------------------------
// the register-file guard: whole-module for the eager layout, per-function for
// the lazy one
// ---------------------------------------------------------------------------

/// A two-function module whose `f/0` **tail-calls** into `g/0`'s first block, and
/// whose `g/0` touches a far higher x-register than `f/0` does.
///
/// The eager layout links `f`'s `call_only` **directly** into `g`'s block inside the
/// one shared mapping, so once the driver has entered `f` natively, `g`'s code runs
/// **without any further guard**. A per-function entry bound would therefore let a
/// short register file enter `f` and then have the linked `g` block access a register
/// that file does not cover — the unsound narrowing this fixture pins. `f`'s exported
/// entry block (`Label(2)`) starts at pc 1 and touches only `x0`; `g`'s block touches
/// `x7`.
fn tail_call_to_a_wider_function() -> Module {
    build_module(
        "guard_bounds",
        &[],
        vec![
            (
                "f",
                0,
                vec![
                    Instr::Label(Label(1)),
                    Instr::Label(Label(2)),
                    Instr::Move {
                        src: Operand::Integer(1),
                        dst: Operand::X(0),
                    },
                    Instr::CallOnly {
                        arity: 0,
                        target: Label(3),
                    },
                ],
            ),
            (
                "g",
                0,
                vec![
                    Instr::Label(Label(3)),
                    Instr::Move {
                        src: Operand::Integer(7),
                        dst: Operand::X(7),
                    },
                    Instr::Return,
                ],
            ),
        ],
        &[("f", 0, Label(2))],
    )
}

/// The eager artifact's entry guard must be the **whole-module** bound (a linked tail
/// call runs another function's code without returning to the driver), while the lazy
/// artifact's guard stays the **entered function's** own bound (each function owns its
/// mapping and a cross-function transfer round-trips through the driver).
#[test]
fn the_eager_guard_covers_the_whole_module_but_the_lazy_guard_the_entered_function() {
    let module = tail_call_to_a_wider_function();
    // `f/0`'s exported entry block (`Label(2)`) starts at pc 1.
    let f_pc = 1;

    // --- eager: the seal-time link runs `g`'s code straight out of `f`'s block, so
    //     only the whole-module bound is safe to gate entry on.
    let eager = compile_module(&module).expect("the module compiles eagerly");
    let module_bound = eager
        .highest_native_register()
        .expect("the module touches registers natively");
    assert_eq!(
        module_bound, 7,
        "`g`'s block is the module's highest natively-touched register (x7)"
    );
    let (_, f_bound) = eager
        .native_entry(&module, 0, f_pc)
        .expect("`f`'s entry block compiles");
    assert_eq!(
        f_bound,
        Some(module_bound),
        "the eager artifact's entry guard must be the whole-module bound: a linked \
         tail call runs another function's code without returning to the driver"
    );
    assert!(
        module_bound > 0,
        "`g`'s bound is strictly higher than `f`'s own x0, so the guard is not vacuous"
    );

    // A file covering `f`'s own x0 but not the module's x7 is exactly the machine the
    // pre-fix per-function bound would have let enter `f` and then run `g`'s `x7`
    // access out of bounds. The whole-module guard refuses it.
    let mut machine = Machine::with_max_register(1);
    assert_eq!(machine.x.len(), 1, "the register file covers only x0");
    let mut ctx = MinimalContext::new();
    let labels = LabelMap::new();
    let mut rt = JitRuntime {
        machine: &mut machine,
        x_regs: machine.x.as_mut_ptr(),
        module: &module,
        labels: &labels,
        ctx: &mut ctx,
        error: None,
        enter: None,
    };
    assert_eq!(
        enter_compiled(&eager, &module, &mut rt as *mut JitRuntime<'_>, 0, f_pc),
        None,
        "a register file covering only `f`'s own x0 must be refused entry to the \
         eager artifact (the linked `g` block touches x7)"
    );

    // --- lazy: each function owns its mapping and a cross-function transfer returns
    //     the `(func, pc)` word to the driver, which re-checks the target function's
    //     own bound — so the entered function's own bound is the correct guard here.
    let lazy = CompiledModule::lazy(&module);
    let (_, lazy_f_bound) = lazy
        .native_entry(&module, 0, f_pc)
        .expect("`f` compiles on demand for the lazy artifact");
    assert_eq!(
        lazy_f_bound,
        Some(0),
        "the lazy artifact guards entry to `f` by `f`'s OWN bound (x0), not the module's"
    );
    let (_, lazy_g_bound) = lazy
        .native_entry(&module, 1, 0)
        .expect("`g` compiles on demand for the lazy artifact");
    assert_eq!(
        lazy_g_bound,
        Some(module_bound),
        "the lazy artifact guards entry to `g` by `g`'s own (still x7) bound"
    );
    assert!(
        lazy_f_bound.expect("`f` touches x0") < module_bound,
        "the lazy per-function guard stays strictly narrower than the whole-module one \
         — safe only because a lazy cross-function transfer returns to the driver"
    );
}
// ---------------------------------------------------------------------------
// resuming a suspension parked inside a cross-module callee
// ---------------------------------------------------------------------------

/// A tiny in-memory mailbox for the cross-module resume test: a `Vec` of messages
/// plus a scan position, so the test can fill the queue **between** the suspension
/// and the resume.
///
/// Only the scan methods this receive shape reaches are overridden; the marker
/// family keeps the [`Mailbox`] trait's typed `unsupported` default (the receive
/// below has no `recv_marker_*` instruction).
#[derive(Clone, Default)]
struct ResumeMailbox {
    /// The queued messages, in arrival order.
    messages: std::rc::Rc<std::cell::RefCell<Vec<Eterm>>>,
    /// The scan position `loop_rec` peeks at; `loop_rec_end` advances it.
    scan: std::rc::Rc<std::cell::Cell<usize>>,
}

impl ResumeMailbox {
    /// An empty mailbox.
    fn new() -> ResumeMailbox {
        ResumeMailbox::default()
    }

    /// Queue `message` as the next arrival.
    fn push(&self, message: Eterm) {
        self.messages.borrow_mut().push(message);
    }

    /// The queued messages, in arrival order.
    fn messages(&self) -> Vec<Eterm> {
        self.messages.borrow().clone()
    }
}

impl crate::Mailbox for ResumeMailbox {
    fn send_message(&mut self, _to: Eterm, message: Eterm) -> Result<()> {
        self.push(message);
        Ok(())
    }

    fn receive_next(&mut self) -> Result<Option<Eterm>> {
        let scan = self.scan.get();
        Ok(self.messages.borrow().get(scan).copied())
    }

    fn receive_skip(&mut self) -> Result<()> {
        self.scan.set(self.scan.get() + 1);
        Ok(())
    }

    fn receive_remove(&mut self) -> Result<()> {
        let scan = self.scan.get();
        let mut messages = self.messages.borrow_mut();
        if scan < messages.len() {
            messages.remove(scan);
        }
        self.scan.set(0);
        Ok(())
    }

    fn receive_reset(&mut self) -> Result<()> {
        self.scan.set(0);
        Ok(())
    }

    // The receive below has no `recv_marker_*` instruction, so the marker family is
    // the seam's honest "this mailbox cannot serve it" answer, never a silent no-op.
    fn receive_marker_reserve(&mut self) -> Result<Eterm> {
        Err(crate::error::unsupported("receive_marker_reserve"))
    }

    fn receive_marker_bind(&mut self, _marker: Eterm, _reference: Eterm) -> Result<()> {
        Err(crate::error::unsupported("receive_marker_bind"))
    }

    fn receive_marker_use(&mut self, _reference: Eterm) -> Result<()> {
        Err(crate::error::unsupported("receive_marker_use"))
    }

    fn receive_marker_clear(&mut self, _reference: Eterm) -> Result<()> {
        Err(crate::error::unsupported("receive_marker_clear"))
    }
}

/// A context whose cross-module-call seam resolves `callee_x:g/0` to `entry`
/// (against the shared `Arc` handle the engine keeps alive while a frame can return
/// into the callee) and whose mailbox is `mailbox`.
fn cross_resume_context(
    callee: &std::sync::Arc<Module>,
    entry: Label,
    mailbox: ResumeMailbox,
) -> MinimalContext {
    let handle = std::sync::Arc::clone(callee);
    let mut ctx = MinimalContext::with_external_resolver(Box::new(move |module, function, arity| {
        if module == "callee_x" && function == "g" && arity == 0 {
            Ok(Some((std::sync::Arc::clone(&handle), entry)))
        } else {
            Ok(None)
        }
    }));
    ctx.set_mailbox(Box::new(mailbox));
    ctx
}

/// The caller/callee pair the cross-module resume test drives: `caller_x:f/0`
/// allocates a frame and `call_ext`s `callee_x:g/0`, whose body scans its mailbox
/// (`loop_rec`/`remove_message`/`return`) and suspends on an empty one
/// (`wait`), resuming at the `loop_rec`.
///
/// The callee's `wait` re-enters **after** the `allocate`, exactly like the compiler's
/// own receive loop, so a resume pushes no second frame.
fn cross_resume_modules() -> (Module, Atom, std::sync::Arc<Module>) {
    let callee_module = build_module(
        "callee_x",
        &[],
        vec![(
            "g",
            0,
            vec![
                Instr::Label(Label(1)), // the function label
                Instr::Label(Label(2)), // the export entry (`allocate`)
                Instr::Allocate {
                    stack_need: beam_loader::HeapNeed::Words(0),
                    live: 0,
                },
                Instr::Label(Label(3)), // the `loop_rec` the `wait` resumes at
                Instr::LoopRec {
                    fail: Label(4),
                    dst: Operand::X(0),
                },
                Instr::RemoveMessage,
                Instr::Deallocate(0),
                Instr::Return,
                Instr::Label(Label(4)), // the empty-mailbox arm
                Instr::Wait { target: Label(3) },
            ],
        )],
        &[("g", 0, Label(2))],
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
    (caller, name, callee)
}

/// Park `caller_x:f/0` on an empty mailbox, deliver one message and resume — all on
/// `tier` — returning `(parked, resumed, remaining messages)`.
fn park_and_resume(
    caller: &Module,
    name: Atom,
    callee: &std::sync::Arc<Module>,
    tier: Tier,
) -> (RunOutcome, RunOutcome, Vec<Eterm>) {
    let mailbox = ResumeMailbox::new();
    let mut ctx = cross_resume_context(callee, Label(2), mailbox.clone());
    let mut machine = Machine::new();

    let parked = machine
        .run_export_outcome_with_tier(caller, name, 0, &[], &mut ctx, tier)
        .expect("the empty mailbox parks the process");
    let (func, pc) = match parked {
        RunOutcome::Wait { func, pc } => (func, pc),
        ref other => panic!("expected a wait, got {other:?}"),
    };
    // The process is parked **inside the callee**, i.e. in a non-zero module
    // context: the caller's `call_ext` entered the callee's context, whose registered
    // handle and module must survive the suspension for the resume to land in it.
    assert_ne!(
        machine.current_context(),
        0,
        "the suspension must be parked in the callee's module context, not the caller's"
    );

    mailbox.push(make_small(42));
    let resumed = machine
        .resume_outcome_with_tier(caller, func, pc, &mut ctx, tier)
        .expect("the resume completes");
    (parked, resumed, mailbox.messages())
}

/// A suspension parked inside a **cross-module callee** resumes on the native tier and
/// completes with the interpreter's exact result.
///
/// The caller `call_ext`s `callee_x:g/0` through the host's
/// [`ExecutionContext::resolve_external_call`](crate::ExecutionContext::resolve_external_call)
/// seam, so the callee runs in the caller's **own** machine and its `receive` parks
/// the process in the callee's module context. [`Machine::resume_outcome_with_tier`]
/// with [`Tier::BaselineJit`] therefore starts the driver in that context (read from
/// `Machine::current_context`), re-binds the callee's module, label map and literal
/// area, resolves its compiled blocks through `NativeCode::module_blocks`, re-enters
/// the callee's `(func, pc)`, and the resumed slice unwinds back through the caller's
/// frame to the identical [`RunOutcome::Returned`].
///
/// Without the cross-module resume handling this is the exact case that failed: a
/// resume that assumed the entry module/context would either run the callee's address
/// as a caller-module address (a wrong result) or report a typed `unsupported`.
#[test]
fn a_suspension_inside_a_cross_module_callee_resumes_on_the_baseline_tier() {
    let (caller, name, callee) = cross_resume_modules();

    let (interpreter_parked, interpreter_resumed, interpreter_rest) =
        park_and_resume(&caller, name, &callee, Tier::Interpreter);
    let (baseline_parked, baseline_resumed, baseline_rest) =
        park_and_resume(&caller, name, &callee, Tier::BaselineJit);

    assert!(
        matches!(&interpreter_parked, RunOutcome::Wait { .. }),
        "the interpreter must park too, got {interpreter_parked:?}"
    );
    assert_eq!(
        baseline_parked, interpreter_parked,
        "both tiers must report the same resume address for the parked receive"
    );
    assert_eq!(
        interpreter_resumed,
        RunOutcome::Returned(make_small(42)),
        "the resumed receive yields the delivered message through the caller's return"
    );
    assert_eq!(
        baseline_resumed, interpreter_resumed,
        "a resume inside a cross-module callee must be interpreter-identical on the baseline tier"
    );
    assert!(
        baseline_rest.is_empty() && interpreter_rest.is_empty(),
        "`remove_message` unlinked the matched message on both tiers"
    );
}

// ---------------------------------------------------------------------------
// the native `call_ext*` arm: `jit_ext_call` (resolution + frame discipline)
//
// `baseline/runtime.rs::jit_ext_call` — over the pure-Rust `resolve_ext_entry` —
// is the helper the `baseline/emit/mod.rs::emit_ext_call` arm wires into every
// compiled block's `call_ext*` lowering, so those instructions no longer deopt
// unconditionally. For an import the host resolves through
// `ExecutionContext::resolve_external_call`, the helper returns the [`ENTER`]
// word and hands the resolved `(context, func, pc)` to the driver through
// `JitRuntime::enter`, applying the variant's frame discipline (`CallExt` stores
// `Continuation::Address { func, pc + 1, context }` first, `CallExtOnly` stores
// nothing, `CallExtLast` resolves *then* pops). Anything the host cannot run here
// — `Ok(None)`, an unresolved import/entry label, a failed frame discipline —
// mutates nothing and falls back to `jit_step_one` (the interpreter's own step),
// so the synchronous `call_bif` path and every one of its errors are
// byte-identical to the tier before the arm existed.
// ---------------------------------------------------------------------------

/// A caller module `caller_x` importing `callee_x:g/0`, whose `f/0` allocates a
/// zero-Y frame and then executes `call` at the function's last code index.
///
/// Returns the module and the `(func, pc)` of `call` (`func` is always `0`).
fn ext_call_caller(call: Instr) -> (Module, (u32, u32)) {
    let code = vec![
        Instr::Label(Label(1)), // the function label
        Instr::Label(Label(2)), // the export entry
        Instr::Allocate {
            stack_need: beam_loader::HeapNeed::Words(0),
            live: 0,
        },
        call,
    ];
    let pc = (code.len() - 1) as u32;
    let module = build_module(
        "caller_x",
        &[("callee_x", "g", 0)],
        vec![("f", 0, code)],
        &[("f", 0, Label(2))],
    );
    (module, (0, pc))
}

/// The callee `callee_x:g/0` the seam resolves to: its export entry is
/// `Label(1)` at code index `0`.
fn ext_call_callee() -> std::sync::Arc<Module> {
    std::sync::Arc::new(build_module(
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
    ))
}

/// The observable result of one [`jit_ext_call`] invocation on a fresh machine
/// holding a single live zero-Y frame.
struct ExtCallProbe {
    /// The block-ABI word the helper returned.
    word: i64,
    /// The `(context, func, pc)` the helper handed the driver, when it resolved.
    enter: Option<(u32, u32, u32)>,
    /// The typed error the helper stored, if any.
    error: Option<beam_common::BeamError>,
    /// The machine's frame depth after the call.
    frame_depth: usize,
    /// The caller frame's continuation after the call (`None` once popped).
    continuation: Option<Continuation>,
    /// `x(0)` after the call.
    x0: Option<Eterm>,
}

/// Invoke [`jit_ext_call`] for `kind`/`deallocate` at `(func, pc)` on a fresh
/// machine holding a single live zero-Y frame, with `ctx` as the context.
fn run_ext_call(
    caller: &Module,
    (func, pc): (u32, u32),
    mut ctx: MinimalContext,
    kind: u32,
    deallocate: u32,
) -> ExtCallProbe {
    let mut machine = Machine::new();
    machine
        .push_frame(0)
        .expect("the caller's zero-Y frame");
    let labels = crate::interpreter::build_label_map(caller);
    let x_regs = machine.x.as_mut_ptr();
    let mut rt = JitRuntime {
        machine: &mut machine,
        x_regs,
        module: caller,
        labels: &labels,
        ctx: &mut ctx,
        error: None,
        enter: None,
    };
    // `caller_x`'s only import is `callee_x:g/0`, i.e. `ImportRef(0)`.
    let word = jit_ext_call(&mut rt as *mut JitRuntime<'_>, func, pc, 0, kind, deallocate);
    ExtCallProbe {
        word,
        enter: rt.enter,
        error: rt.error.take(),
        frame_depth: machine.frame_depth(),
        continuation: machine.continuation().ok(),
        x0: machine.x(0),
    }
}

/// `call_ext` resolves through the host's seam: the helper returns the [`ENTER`]
/// word, hands the driver the callee's `(context, func, pc)`, and — **first** —
/// stores the caller's return address (`Continuation::Address { func, pc + 1,
/// context }`) in the live frame, exactly like the interpreter's `Instr::CallExt`.
#[test]
fn jit_ext_call_resolves_call_ext_into_an_enter_and_stores_the_continuation() {
    let (caller, (func, pc)) = ext_call_caller(Instr::CallExt {
        arity: 0,
        target: ImportRef(0),
    });
    let callee = ext_call_callee();
    let probe = run_ext_call(&caller, (func, pc), seam_context(&callee), EXT_CALL, 0);

    assert_eq!(probe.word, ENTER, "a resolved call_ext reports the ENTER word");
    let (context, callee_func, callee_pc) = probe
        .enter
        .expect("the resolved target is handed to the driver through rt.enter");
    assert_eq!(
        context, 1,
        "the callee is the machine's first registered module context"
    );
    assert_eq!(
        (callee_func, callee_pc),
        (0, 0),
        "the callee's export entry `g/0` is `Label(1)` -> `(func 0, pc 0)`"
    );
    assert_eq!(probe.frame_depth, 1, "call_ext keeps the caller's frame");
    assert_eq!(
        probe.continuation,
        Some(Continuation::Address {
            func,
            pc: pc + 1,
            context: 0,
        }),
        "call_ext stores its return address (its own func, pc + 1, caller context)"
    );
}

/// `call_ext_only` is a tail call: it stores **no** continuation — the callee
/// returns along the frame's *existing* continuation (the bottom `Halt` here) —
/// but still hands the driver the resolved target.
#[test]
fn jit_ext_call_only_enters_without_storing_a_continuation() {
    let (caller, (func, pc)) = ext_call_caller(Instr::CallExtOnly {
        arity: 0,
        target: ImportRef(0),
    });
    let callee = ext_call_callee();
    let probe = run_ext_call(&caller, (func, pc), seam_context(&callee), EXT_CALL_ONLY, 0);

    assert_eq!(
        probe.word, ENTER,
        "a resolved call_ext_only reports the ENTER word"
    );
    assert_eq!(
        probe.enter.map(|(c, f, p)| (c, f, p)),
        Some((1, 0, 0)),
        "the callee's export entry is handed to the driver"
    );
    assert_eq!(probe.frame_depth, 1, "a tail call reuses the caller's frame");
    assert_eq!(
        probe.continuation,
        Some(Continuation::Halt),
        "call_ext_only stores nothing: the frame keeps its existing continuation"
    );
}

/// `call_ext_last` resolves the target **then** pops the caller's frame (the
/// interpreter's own `pop_frame_checked`), so a resolved tail call leaves no
/// frame behind.
#[test]
fn jit_ext_call_last_resolves_then_pops_the_frame() {
    let (caller, (func, pc)) = ext_call_caller(Instr::CallExtLast {
        arity: 0,
        target: ImportRef(0),
        deallocate: 0,
    });
    let callee = ext_call_callee();
    let probe = run_ext_call(&caller, (func, pc), seam_context(&callee), EXT_CALL_LAST, 0);

    assert_eq!(
        probe.word, ENTER,
        "a resolved call_ext_last reports the ENTER word"
    );
    assert_eq!(
        probe.enter,
        Some((1, 0, 0)),
        "the callee's entry is resolved before the pop"
    );
    assert_eq!(
        probe.frame_depth, 0,
        "call_ext_last pops the caller's frame"
    );
    assert_eq!(
        probe.continuation, None,
        "there is no caller frame left to hold a continuation"
    );
}

/// A host that cannot run the callee (`resolve_external_call` -> `Ok(None)`)
/// falls back to [`jit_step_one`] — the interpreter's own step — which runs the
/// *whole* instruction through the synchronous `call_bif` path: the helper
/// reports the advance (`func, pc + 1`), `x(0)` gets the BIF's result, `rt.enter`
/// stays `None` and the cross-module frame discipline never runs.
#[test]
fn jit_ext_call_falls_back_to_the_synchronous_path_when_no_callee_resolves() {
    let (caller, (func, pc)) = ext_call_caller(Instr::CallExt {
        arity: 0,
        target: ImportRef(0),
    });
    // No resolver is installed, so the seam answers `Ok(None)`; the BIF hook is
    // then the synchronous dispatch path and answers `callee_x:g/0` with `7`.
    let seen = std::rc::Rc::new(std::cell::RefCell::new(None::<(String, String, u32)>));
    let record = std::rc::Rc::clone(&seen);
    let ctx = MinimalContext::with_bif_hook(Box::new(move |module, function, arity, _args| {
        *record.borrow_mut() = Some((module.to_string(), function.to_string(), arity));
        Ok(make_small(7))
    }));
    let probe = run_ext_call(&caller, (func, pc), ctx, EXT_CALL, 0);

    assert_eq!(
        *seen.borrow(),
        Some(("callee_x".to_string(), "g".to_string(), 0)),
        "the fallback dispatches the caller's import through call_bif"
    );
    assert_eq!(
        probe.word,
        encode_target(func, pc + 1),
        "the fallback runs the interpreter's CallExt, which advances by one"
    );
    assert_eq!(
        probe.enter, None,
        "no cross-module transfer is reported on the synchronous path"
    );
    assert_eq!(
        probe.x0,
        Some(make_small(7)),
        "the synchronous call_bif wrote its result to x(0)"
    );
    assert_eq!(
        probe.frame_depth, 1,
        "the synchronous path keeps the caller's frame"
    );
    assert_eq!(
        probe.continuation,
        Some(Continuation::Halt),
        "call_ext stores no continuation on the BIF path"
    );
}

/// A `call_ext_last` whose `Deallocate` operand is **stale** (its frame size no
/// longer matches the live frame — the shape `trim` produces) must not fabricate
/// a wrong `ENTER`: the frame discipline's checked pop rejects the mismatch, the
/// helper mutates nothing and falls back to [`jit_step_one`], which re-runs the
/// instruction and reports the interpreter's exact `deallocate: frame size
/// mismatch` error. The caller's frame is left intact.
#[test]
fn jit_ext_call_last_rejects_a_stale_deallocate_with_the_checked_pop_error() {
    // The live frame has zero Y slots; the operand claims two -> a mismatch.
    let (caller, (func, pc)) = ext_call_caller(Instr::CallExtLast {
        arity: 0,
        target: ImportRef(0),
        deallocate: 2,
    });
    let callee = ext_call_callee();
    let probe = run_ext_call(&caller, (func, pc), seam_context(&callee), EXT_CALL_LAST, 2);

    assert_eq!(
        probe.word, ERROR,
        "a failed frame discipline is the typed ERROR word, never a wrong ENTER"
    );
    assert_eq!(
        probe.enter, None,
        "nothing is handed to the driver when the discipline fails"
    );
    assert_eq!(
        probe.frame_depth, 1,
        "the rejected pop leaves the caller's frame intact"
    );
    let err = probe
        .error
        .expect("the fallback's typed error is stored in rt.error");
    assert_eq!(
        error_reason(&err),
        "unsupported_opcode: deallocate: frame size mismatch"
    );
}

/// The emit arm actually wires the helper into a compiled block: the artifact's
/// machine code contains the `movabs r11, <jit_ext_call>` immediate that
/// `emit_ext_call` emits (the same `movabs r11, helper` shape every helper call
/// uses), so a `call_ext*` no longer lowers to an unconditional deopt.
#[test]
fn the_compiled_block_wires_the_ext_call_helper_into_native_code() {
    let (caller, _) = ext_call_caller(Instr::CallExt {
        arity: 0,
        target: ImportRef(0),
    });
    let compiled = compile_module(&caller).expect("the caller module compiles");
    assert!(compiled.block_count() > 0, "the caller module produced a block");

    // `mov r11, imm64` (the `movabs` form) is `49 BB <imm64 little-endian>`; the
    // immediate is the helper's address.
    let helper = jit_ext_call as *const () as usize;
    let mut needle = vec![0x49u8, 0xBB];
    needle.extend_from_slice(&(helper as u64).to_le_bytes());

    let (start, end) = compiled.code_address_range();
    // SAFETY: `[start, end)` is the artifact's live, sealed executable mapping,
    // which `compiled` owns and keeps mapped for this borrow; reading it as bytes
    // is valid and does not execute it.
    let code = unsafe { std::slice::from_raw_parts(start as *const u8, end - start) };

    assert!(
        code.windows(needle.len())
            .any(|window| window == needle.as_slice()),
        "the compiled block must `movabs r11, <jit_ext_call>`; its absence means the \
         emit arm did not wire the native `call_ext*` helper"
    );
}

// ---------------------------------------------------------------------------
// the artifact owns the module-wide label map
// ---------------------------------------------------------------------------

/// The compiled artifact **owns** the module-wide label map, so a run of it resolves
/// its entry label and imports against that one map instead of rebuilding a per-module
/// value on every run.
///
/// Before this, the native driver called [`interpreter::build_label_map`] again on
/// every `run_compiled`/`run_compiled_outcome` (in `enter_machine`) **and** on every
/// resume (in `run_compiled_resume_outcome`) — a whole traversal of the module's
/// per-function `BTreeMap` label tables — even though the map is a property of the
/// (immutable) module the artifact was compiled from. It is now built once, when the
/// artifact is created: at `compile_module` for the eager layout,
/// [`CompiledModule::lazy`] for the lazy one, and the driver reuses it through
/// [`NativeCode::labels`].
///
/// The counter is thread-local, so it sees only this test's own builds. The one build
/// still attributable to a run is the export-entry resolution
/// (`interpreter::resolve_label`, out of this crate-node's scope) — the assertion is
/// therefore "at most one per run, never two", which is exactly what distinguishes the
/// driver's map reuse from the old double build.
#[test]
fn runs_reuse_the_artifacts_label_map_instead_of_rebuilding_it() {
    // `label; label; move 7 -> x0; return`, exported as `f/0`: every run returns 7.
    let (module, name) = exported_module(
        "f",
        vec![
            Instr::Label(Label(1)),
            Instr::Label(Label(2)),
            Instr::Move {
                src: Operand::Integer(7),
                dst: Operand::X(0),
            },
            Instr::Return,
        ],
    );

    crate::interpreter::reset_label_map_builds();
    // The eager artifact stores the module-wide map it built at compile time.
    let compiled = compile_module(&module).expect("the module compiles");
    let built_at_compilation = crate::interpreter::label_map_builds();
    assert!(
        built_at_compilation >= 1,
        "the eager artifact holds a module-wide label map built at compile time, got \
         {built_at_compilation}"
    );

    // One exported run.
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let value = run_compiled(&compiled, &mut machine, &module, name, 0, &[], &mut ctx)
        .expect("the exported run returns");
    assert_eq!(value, make_small(7), "the exported run returns the moved x(0)");
    let built_by_one_run = crate::interpreter::label_map_builds() - built_at_compilation;
    assert!(
        built_by_one_run <= 1,
        "a run of an artifact that holds its own label map must NOT rebuild it in the \
         driver: this run built {built_by_one_run} maps, and two would mean the \
         driver's per-run build is back"
    );

    // Two more runs: the per-run cost is a constant, so nothing accumulates.
    for run in 1..3 {
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let value = run_compiled(&compiled, &mut machine, &module, name, 0, &[], &mut ctx)
            .expect("the exported run returns");
        assert_eq!(value, make_small(7), "run {run} returns the moved x(0)");
    }
    assert_eq!(
        crate::interpreter::label_map_builds() - built_at_compilation,
        3 * built_by_one_run,
        "every run of one eager artifact builds the same number of maps: the driver \
         adds none of its own"
    );

    // The label-addressed entry resolves against the run's map, so on top of the
    // export-path constant it adds nothing at all.
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let outcome = run_compiled_label_outcome(
        &compiled,
        &mut machine,
        &module,
        Label(2),
        0,
        &[],
        &mut ctx,
    )
    .expect("the label run returns");
    assert_eq!(outcome, RunOutcome::Returned(make_small(7)));
    assert_eq!(
        crate::interpreter::label_map_builds() - built_at_compilation,
        3 * built_by_one_run,
        "the label-addressed entry builds no map: it resolves against the artifact's"
    );

    // The lazy artifact builds exactly one map, at creation.
    let lazy = CompiledModule::lazy(&module);
    assert_eq!(
        crate::interpreter::label_map_builds() - built_at_compilation,
        3 * built_by_one_run + 1,
        "CompiledModule::lazy builds exactly one map, at creation"
    );

    // Its first entry compiles the function (a one-time cost); every run after that
    // still pays only the export-path constant, never a second map.
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let value = run_compiled(&lazy, &mut machine, &module, name, 0, &[], &mut ctx)
        .expect("the lazy artifact's first entry compiles the function and returns");
    assert_eq!(value, make_small(7));
    let after_first_entry = crate::interpreter::label_map_builds();

    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let value = run_compiled(&lazy, &mut machine, &module, name, 0, &[], &mut ctx)
        .expect("the second lazy run returns");
    assert_eq!(value, make_small(7));
    assert_eq!(
        crate::interpreter::label_map_builds() - after_first_entry,
        built_by_one_run,
        "a lazy run whose function is already compiled costs the same per-run constant \
         as an eager one (the driver rebuilds nothing, and the lazy artifact's map is \
         the one built at creation)"
    );
}
