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
    jit_deallocate, jit_deallocate_return, jit_publish_y, jit_refresh_y, jit_return, EXT_CALL,
    EXT_CALL_LAST, EXT_CALL_ONLY, RUNTIME_MACHINE_DISP, RUNTIME_XREGS_DISP, RUNTIME_YBASE_DISP,
    RUNTIME_YEND_DISP,
};
use super::*;
use crate::jit::emitter::Reg;
use crate::jit::Tier;
use crate::machine::{Continuation, MAX_REGISTER};
use crate::MinimalContext;
use beam_loader::{
    Block, Export, Function, Import, ImportRef, Instr, Label, Operand, Reg as RegRef, TestKind,
    NO_FAIL,
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
    // Generated code loads `rt.x_regs` from `[rt + 8]` (`RUNTIME_XREGS_DISP`) and reads
    // the current frame's Y window through `rt.y_base`/`rt.y_end`
    // (`RUNTIME_YBASE_DISP`/`RUNTIME_YEND_DISP`, the guarded Y load/store in
    // `arith::emit_y_load`/`emit_y_store`); the helper is handed `rt` itself. This pins
    // all four offsets against the real `JitRuntime` field addresses.
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
        y_base: std::ptr::null_mut(),
        y_end: std::ptr::null_mut(),
    };
    let base = &rt as *const JitRuntime as usize;
    let machine_field = &rt.machine as *const *mut Machine as usize;
    let x_regs_field = &rt.x_regs as *const *mut Eterm as usize;
    let y_base_field = &rt.y_base as *const *mut Eterm as usize;
    let y_end_field = &rt.y_end as *const *mut Eterm as usize;
    assert_eq!(machine_field - base, RUNTIME_MACHINE_DISP as usize);
    assert_eq!(x_regs_field - base, RUNTIME_XREGS_DISP as usize);
    assert_eq!(y_base_field - base, RUNTIME_YBASE_DISP as usize);
    assert_eq!(y_end_field - base, RUNTIME_YEND_DISP as usize);
    // The window follows the x-register base, so the three pointer fields are
    // consecutive machine words.
    assert_eq!(
        RUNTIME_YBASE_DISP,
        RUNTIME_XREGS_DISP + std::mem::size_of::<*const u8>() as i32
    );
    assert_eq!(
        RUNTIME_YEND_DISP,
        RUNTIME_YBASE_DISP + std::mem::size_of::<*const u8>() as i32
    );
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

    let plus = refs[0];
    let atom = module.name;

    // The fast path claims both a Y **destination** and a Y **operand**: the slot is
    // read/written through the guarded current-frame window (`RUNTIME_YBASE_DISP`/
    // `RUNTIME_YEND_DISP`), so the shape lowers and records `FastDest::Y`.
    let y_destination_instr = gc_bif(plus, args.clone(), Operand::Y(0));
    let y_destination = fast_arith(&module, &y_destination_instr).expect("a Y destination lowers");
    assert_eq!(y_destination.dst, FastDest::Y(0));
    assert!(
        fast_arith(
            &module,
            &gc_bif(plus, vec![Operand::X(0), Operand::Y(1)], Operand::X(0))
        )
        .is_some(),
        "a Y operand lowers"
    );
    assert!(
        fast_arith(
            &module,
            &gc_bif(plus, vec![Operand::Y(1), Operand::X(0)], Operand::X(0))
        )
        .is_some(),
        "a Y operand lowers on either side"
    );

    // Shapes the fast path still refuses: a non-small constant operand (`atom`/`nil`),
    // a wrong argument count, an out-of-bound x-register operand or destination, and a
    // Y slot the emitted `disp32` cannot address.
    for instr in [
        gc_bif(plus, vec![Operand::X(0), Operand::Atom(atom)], Operand::X(0)),
        gc_bif(plus, vec![Operand::X(0), Operand::Nil], Operand::X(0)),
        gc_bif(plus, vec![Operand::X(0)], Operand::X(0)),
        gc_bif(
            plus,
            vec![Operand::X(MAX_REGISTER), Operand::Integer(1)],
            Operand::X(0),
        ),
        gc_bif(plus, args.clone(), Operand::X(MAX_REGISTER)),
        gc_bif(plus, args.clone(), Operand::Y(u32::MAX)),
        gc_bif(plus, vec![Operand::X(0), Operand::Y(u32::MAX)], Operand::X(0)),
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

/// A frame-preserving `call` whose continuation address falls **inside** an IR block
/// (the loader cuts a function into blocks only at labels) gets its own native
/// **sub-block** in each layout, so the driver has a native entry at the address
/// the callee returns to rather than one interpreter round-trip per remaining
/// instruction of the block. The sub-block is *not* a block: the driver's block table,
/// `block_count` and the code map still describe IR blocks only.
#[test]
fn a_call_continuation_inside_a_block_gets_its_own_native_sub_block() {
    let (module, _) = local_call_case();
    let labels = crate::interpreter::build_label_map(&module);
    let function = compile_function(&module, &labels, 0).expect("f/0 compiles");

    // `f/0`'s `call` sits at pc 3 inside the `Label(2)` block `[1, 5)`, so the
    // continuation pc 4 is strictly inside that block.
    assert!(function.entries[4].is_none(), "pc 4 is not an IR block start");
    assert!(function.sub_entries[4].is_some(), "pc 4 has a sub-block entry");
    // An IR block start is still served by the block table, and gains no sub-block.
    assert!(function.entries[1].is_some(), "pc 1 is the `Label(2)` block start");
    assert!(function.sub_entries[1].is_none(), "a block start needs no sub-block");
    // A sub-block is not a block: the summary counts the two IR blocks only.
    assert_eq!(function.summary.block_count, 2, "only the IR blocks are counted");

    // The artifact consults the sub-block table only after the block table, so a block
    // start still wins and the continuation address is natively enterable.
    let compiled = CompiledModule::lazy(&module);
    let (block, _) = compiled.native_entry(&module, 0, 1).expect("pc 1 is a block");
    assert_eq!(compiled.entry(0, 1), Some(block), "the block entry wins");
    let (sub, _) = compiled.native_entry(&module, 0, 4).expect("pc 4 is a sub-block");
    assert_eq!(
        compiled.entry(0, 4),
        Some(sub),
        "the continuation address is natively enterable"
    );
    assert_eq!(compiled.block_count(), 2, "sub-blocks add no block");

    // The eager layout lowers the same continuation sub-blocks, so the continuation
    // address is natively enterable there too; a block start still wins and the
    // sub-block is still not counted as a block.
    let eager = compile_module(&module).expect("the module compiles eagerly");
    let (eager_block, _) = eager.native_entry(&module, 0, 1).expect("pc 1 is a block");
    assert_eq!(eager.entry(0, 1), Some(eager_block), "the block entry wins");
    assert!(
        eager.entry(0, 4).is_some(),
        "the eager layout has a sub-block at the continuation address"
    );
    assert_eq!(eager.block_count(), 3, "sub-blocks add no block");
}

/// The **eager** layout — the one the mandated `jit_bench` harness compiles — lowers the
/// same continuation sub-blocks as the lazy layout, so its driver has a native entry at the
/// `pc + 1` a frame-preserving call returns to even though that address is not an IR block
/// start. An address with no code at all still answers `None`, and `block_count` still
/// counts the non-empty IR blocks only (sub-blocks are not blocks).
#[test]
fn the_eager_layout_lowers_continuation_sub_blocks() {
    let (module, _) = local_call_case();
    let compiled = compile_module(&module).expect("the module compiles eagerly");

    // `f/0`'s `call` sits at pc 3 inside the `Label(2)` block `[1, 5)`, so the
    // continuation pc 4 is strictly inside that block: the eager artifact now has a
    // native entry there, in addition to the IR block start at pc 1.
    let (block, _) = compiled.native_entry(&module, 0, 1).expect("pc 1 is a block");
    assert_eq!(compiled.entry(0, 1), Some(block), "pc 1 is the block start");
    assert!(
        compiled.entry(0, 4).is_some(),
        "the eager continuation pc 4 has a native sub-block entry"
    );
    // An address with no code at all (past the end of the function) still has no entry.
    assert!(
        compiled.entry(0, module.functions[0].code.len() as u32).is_none(),
        "an address past the end of the function has no entry"
    );

    // A sub-block is not a block: `block_count` equals the number of non-empty IR blocks.
    let ir_blocks: usize = module.functions.iter().map(|f| f.blocks.len()).sum();
    assert_eq!(
        compiled.block_count(),
        ir_blocks,
        "sub-blocks add no compiled block"
    );
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
        y_base: std::ptr::null_mut(),
        y_end: std::ptr::null_mut(),
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
        y_base: std::ptr::null_mut(),
        y_end: std::ptr::null_mut(),
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

/// The host's **decline** (`Ok(None)`) is answered through the dispatching seam
/// ([`jit_step_ext`] with `Some(None)`), so the host's `resolve_external_call` is
/// consulted **exactly once** per declined `call_ext*` — not once by
/// [`jit_ext_call`]'s own resolution and again by the interpreter step it used to
/// fall back to. The result is unchanged: the synchronous `call_bif` route still
/// runs the BIF and advances by one.
#[test]
fn jit_ext_call_consults_the_host_seam_once_per_declined_call() {
    let (caller, (func, pc)) = ext_call_caller(Instr::CallExt {
        arity: 0,
        target: ImportRef(0),
    });
    // A counting resolver that declines *every* import: the host's seam answer is
    // `Ok(None)`, so the call must reach `call_bif` — but be asked only once.
    let asks = std::rc::Rc::new(std::cell::Cell::new(0usize));
    let counter = std::rc::Rc::clone(&asks);
    let mut ctx = MinimalContext::new();
    ctx.set_external_resolver(Box::new(move |_module, _function, _arity| {
        counter.set(counter.get() + 1);
        Ok(None)
    }));
    ctx.set_bif_hook(Box::new(|module, function, arity, _args| {
        assert_eq!(
            (module, function, arity),
            ("callee_x", "g", 0),
            "the declined import still reaches the synchronous BIF route"
        );
        Ok(make_small(7))
    }));

    let probe = run_ext_call(&caller, (func, pc), ctx, EXT_CALL, 0);

    assert_eq!(
        asks.get(),
        1,
        "the host's cross-module seam must be consulted exactly once per declined \
         `call_ext` (the duplicate `resolve_external_entry` pass is gone)"
    );
    assert_eq!(
        probe.word,
        encode_target(func, pc + 1),
        "a declined call_ext still runs the interpreter's CallExt and advances by one"
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
}

/// A module whose whole body is `rounds` `call_ext` calls to an import the host
/// declines: `erlang:'+'/2` on two small integers. Every call is therefore the
/// exact shape the duplicate-resolution bug covered.
///
/// `x(0)` is the accumulator (starting at `0`), `x(1)` is the constant `1`, so the
/// returned value is `rounds` — a figure a run that skipped, repeated or reordered
/// a call cannot produce.
fn declined_call_ext_module(rounds: usize) -> (Module, Atom) {
    let mut code = vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::Move {
            src: Operand::Integer(0),
            dst: Operand::X(0),
        },
        Instr::Move {
            src: Operand::Integer(1),
            dst: Operand::X(1),
        },
    ];
    for _ in 0..rounds {
        code.push(Instr::CallExt {
            arity: 2,
            target: ImportRef(0),
        });
    }
    code.push(Instr::Return);
    let module = build_module(
        "declined_ext",
        &[("erlang", "+", 2)],
        vec![("f", 0, code)],
        &[("f", 0, Label(2))],
    );
    let name = export_atom(&module, "f");
    (module, name)
}

/// End-to-end tier agreement over the duplicate-resolution path: a body of
/// `call_ext` instructions the host declines runs on the interpreter and on the
/// baseline tier, producing the same value **and** consulting the host's
/// cross-module seam the same number of times (once per call) on both.
///
/// This is the regression pin for the decline path: before `jit_step_ext`, the
/// baseline tier asked the host's seam twice per declined `call_ext` (once in
/// `jit_ext_call`'s own resolution, once inside the `jit_step_one` fallback's
/// `resolve_external_entry`) while the interpreter asked once, so any counting
/// host saw a tier-dependent seam-call count.
#[test]
fn the_baseline_tier_agrees_with_the_interpreter_on_the_declined_call_ext_seam_count() {
    const ROUNDS: usize = 5;
    let (module, name) = declined_call_ext_module(ROUNDS);

    let mut counts = Vec::new();
    for tier in [Tier::Interpreter, Tier::BaselineJit] {
        let asks = std::rc::Rc::new(std::cell::Cell::new(0usize));
        let counter = std::rc::Rc::clone(&asks);
        let mut ctx = MinimalContext::new();
        ctx.set_bif_hook(crate::jit::bench::support::arith_hook(None, None));
        ctx.set_external_resolver(Box::new(move |_module, _function, _arity| {
            counter.set(counter.get() + 1);
            Ok(None)
        }));

        let mut machine = Machine::new();
        let outcome = machine
            .run_export_outcome_with_tier(&module, name, 0, &[], &mut ctx, tier)
            .unwrap_or_else(|err| panic!("{tier:?}: the declined call_ext run returns: {err:?}"));
        assert_eq!(
            outcome,
            RunOutcome::Returned(make_small(ROUNDS as i64)),
            "{tier:?}: {ROUNDS} declined `erlang:'+'/2` calls accumulate to {ROUNDS}"
        );
        counts.push(asks.get());
    }

    assert_eq!(
        counts[0], ROUNDS,
        "the interpreter consults the host's cross-module seam once per call_ext"
    );
    assert_eq!(
        counts[1], counts[0],
        "the baseline tier must consult the host's cross-module seam the SAME number \
         of times as the interpreter ({ROUNDS} calls, no duplicate resolution)"
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

// ---------------------------------------------------------------------------
// Measurement (not an assertion): the declined `call_ext*` seam cost
// ---------------------------------------------------------------------------

/// The `call_ext` calls one measured run makes, and the timed run count.
///
/// Sized like the crate's JIT workloads (`WALK_LENGTH`/`CROSS_STEPS` are 64; this is
/// larger so the per-run setup — `Machine::reset` + the label-map build — is a small
/// fraction of a run, which is what makes the figure track the call itself).
const MEASURED_CALL_EXT_ROUNDS: usize = 256;

/// The timed runs per tier after one warm-up run.
const MEASURED_CALL_EXT_RUNS: u32 = 1_000;

/// **A measurement, never an assertion**: the cost of a body of
/// [`MEASURED_CALL_EXT_ROUNDS`] `call_ext` BIF calls whose host seam answers `Ok(None)`
/// ("run it synchronously"), on both tiers.
///
/// This is the shape the duplicate host-seam resolution covered: a native tier lowered
/// the `call_ext*` through [`jit_ext_call`], consulted the host's cross-module seam,
/// and — on the host's decline — handed the whole instruction back to [`jit_step_one`],
/// whose `call_ext*` arm consulted the **same** seam a second time. The fix routes that
/// decline through [`jit_step_ext`]'s `Some(None)` answer, so the seam is consulted once.
/// The seam-counter test above pins the **count**; this one reports the **cost**, so the
/// before/after figures for one build can be compared by running the same `#[ignore]`d
/// test against both.
///
/// `#[ignore]`d because it is a benchmark, not a unit test, and because it is
/// timing-sensitive: run it alone with
/// `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine
/// --lib -- --ignored --nocapture measure_declined_call_ext` (one test, so no other
/// workload's timing can contaminate it) and read the ratios across builds, never the
/// absolute nanoseconds (this is a shared, high-load sandbox).
#[test]
#[ignore = "measurement: run with --ignored --nocapture"]
fn measure_declined_call_ext() {
    let (module, name) = declined_call_ext_module(MEASURED_CALL_EXT_ROUNDS);

    for tier in [Tier::Interpreter, Tier::BaselineJit] {
        let mut ctx = MinimalContext::new();
        // The reference implementation of the BIF the calls reach synchronously; the
        // host's cross-module seam keeps its default `Ok(None)` (the decline).
        ctx.set_bif_hook(crate::jit::bench::support::arith_hook(None, None));
        let mut machine = Machine::new();

        // One warm-up run (the first execution through the artifact), not timed.
        machine
            .run_export_outcome_with_tier(&module, name, 0, &[], &mut ctx, tier)
            .unwrap_or_else(|err| panic!("{tier:?}: the warm-up run returns: {err:?}"));

        let start = std::time::Instant::now();
        for _ in 0..MEASURED_CALL_EXT_RUNS {
            let outcome = machine
                .run_export_outcome_with_tier(&module, name, 0, &[], &mut ctx, tier)
                .unwrap_or_else(|err| panic!("{tier:?}: a timed run returns: {err:?}"));
            assert_eq!(
                outcome,
                RunOutcome::Returned(make_small(MEASURED_CALL_EXT_ROUNDS as i64)),
                "{tier:?}: the measured body must still compute the same value"
            );
        }
        let elapsed = start.elapsed();
        let ns_per_run = elapsed.as_secs_f64() * 1e9 / f64::from(MEASURED_CALL_EXT_RUNS);
        println!(
            "measure_declined_call_ext | tier={:<12} runs={:<5} total={:>9.3} ms | \
             {:>12.1} ns/run | {:>8.2} ns/call_ext",
            tier.name(),
            MEASURED_CALL_EXT_RUNS,
            elapsed.as_secs_f64() * 1e3,
            ns_per_run,
            ns_per_run / MEASURED_CALL_EXT_ROUNDS as f64,
        );
    }
}

// ---------------------------------------------------------------------------
// Y-slot arithmetic: the frame-window publication across a frame change
//
// A `gc_bif '+'/2`/`'-'/2`/`'*'/2` whose operand or destination is a Y slot is
// lowered natively, reading/writing the current call frame's Y window through
// `JitRuntime::y_base`/`y_end`. The window is published by the block **prologue**
// (`jit_publish_y`, in place of the plain position store) and re-published after an
// in-block helper call (`jit_refresh_y`) only when the block still accesses Y later
// (`Allocation::refresh_y_after`). Both decisions are compile-time, so a Y-free block
// emits neither.
// ---------------------------------------------------------------------------

/// `f/1`'s body in the Y-slot arithmetic shape the loader emits for a clause whose
/// `gc_bif '+'/2` reads a Y operand and writes a Y destination:
/// `Allocate{2}` · `Move X(0)->Y(0)` · `GcBif '+'/2 [Y(0),X(0)] -> Y(1)` ·
/// `Move Y(1)->X(0)` · `Deallocate 2` · `Return`.
fn y_slot_arith_module(name: &str) -> (Module, Atom) {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::Allocate {
            stack_need: beam_loader::HeapNeed::Words(2),
            live: 2,
        },
        Instr::Move {
            src: Operand::X(0),
            dst: Operand::Y(0),
        },
        gc_bif(
            ImportRef(0),
            vec![Operand::Y(0), Operand::X(0)],
            Operand::Y(1),
        ),
        Instr::Move {
            src: Operand::Y(1),
            dst: Operand::X(0),
        },
        Instr::Deallocate(2),
        Instr::Return,
    ];
    let module = build_module(
        name,
        &[("erlang", "+", 2)],
        vec![("f", 1, code)],
        &[("f", 1, Label(2))],
    );
    let atom = export_atom(&module, "f");
    (module, atom)
}

/// The `GcBif` instruction of [`y_slot_arith_module`], as the compile-time lowerer
/// sees it (`fast_arith` takes the instruction, not a code index).
fn y_slot_arith_instr() -> Instr {
    gc_bif(
        ImportRef(0),
        vec![Operand::Y(0), Operand::X(0)],
        Operand::Y(1),
    )
}

/// Run exported `name/1` with one small-int argument on both tiers and hand the two
/// outcomes to `check`.
///
/// The unary twin of [`with_both_tiers`] (that one hard-codes arity 0): each run builds
/// its boxed terms on its **own** [`MinimalContext`] heap, so the comparison has to
/// happen *inside* this call — returning the outcomes and comparing them after the
/// contexts were dropped would dereference freed memory.
fn with_both_tiers_unary<R>(
    module: &Module,
    name: Atom,
    arg: i64,
    check: impl FnOnce(&Result<RunOutcome>, &Result<RunOutcome>) -> R,
) -> R {
    let mut interpreter = Machine::new();
    let mut interpreter_ctx = AddCtx::new();
    let interpreter_outcome = interpreter.run_export_outcome_with_tier(
        module,
        name,
        1,
        &[make_small(arg)],
        &mut interpreter_ctx,
        Tier::Interpreter,
    );
    let mut baseline = Machine::new();
    let mut baseline_ctx = AddCtx::new();
    let baseline_outcome = baseline.run_export_outcome_with_tier(
        module,
        name,
        1,
        &[make_small(arg)],
        &mut baseline_ctx,
        Tier::BaselineJit,
    );
    check(&interpreter_outcome, &baseline_outcome)
}

/// A context that answers `erlang:'+'/2` with the **true** small-int sum of its two
/// arguments (the interpreter route a `gc_bif` that could not be lowered takes), and
/// delegates everything else to a [`MinimalContext`].
///
/// The Y-slot arithmetic tests need the two routes to agree: the baseline tier inlines
/// the small-int fast path, while the interpreter reaches this hook.
struct AddCtx {
    inner: MinimalContext,
}

impl AddCtx {
    fn new() -> AddCtx {
        AddCtx {
            inner: MinimalContext::new(),
        }
    }

    /// The small integer a tagged word encodes (the inverse of `make_small`).
    fn small_of(term: Eterm) -> i64 {
        (term.raw() as i64 - SMALL_TAG as i64) >> SMALL_SHIFT
    }
}

impl ExecutionContext for AddCtx {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        if module == "erlang" && function == "+" && arity == 2 {
            return Ok(make_small(Self::small_of(args[0]) + Self::small_of(args[1])));
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

/// The Y-operand / Y-destination `gc_bif '+'/2` shape both **lowers** (the compile-time
/// fast path claims it behind its frame-window guard) and **executes identically** on
/// the baseline tier and the interpreter.
#[test]
fn a_y_slot_arith_shape_lowers_and_agrees_across_tiers() {
    let (module, name) = y_slot_arith_module("yslot_eq");
    assert!(
        fast_arith(&module, &y_slot_arith_instr()).is_some(),
        "a Y-operand / Y-destination small-int '+'/2 must lower at compile time"
    );
    with_both_tiers_unary(&module, name, 10, |interpreter, baseline| {
        assert_same_outcome("y-slot arith", interpreter, baseline, &module.atom_table);
        match (interpreter, baseline) {
            (Ok(RunOutcome::Returned(value)), _) => {
                assert_eq!(*value, make_small(20), "10 + 10");
            }
            other => panic!("the y-slot arithmetic did not return: {other:?}"),
        }
    });
}

/// The published Y window must be **refreshed across a frame change**.
///
/// `f/1` pushes a 2-slot frame, poisons its own Y window (`Y(0) = 555`, `Y(1) = 1000`)
/// and then makes a local `Call` to a region that pushes **its own** 2-slot frame and
/// runs `'+'(Y(0), 7) -> Y(1)` there. If the callee's block read the caller's stale
/// window it would compute `555 + 7 = 562` (and write into the caller's Y(1)); the
/// correct answer is `10 + 7 = 17`. The callee block's prologue publishes the window
/// while the *caller's* frame is on top, and its `Allocate` then pushes the callee
/// frame — so the `'+'` reaches its Y slot only through the post-deopt `jit_refresh_y`
/// the frame change forces, on both tiers.
#[test]
fn a_y_slot_read_after_a_frame_change_sees_the_new_frame() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::Allocate {
            stack_need: beam_loader::HeapNeed::Words(2),
            live: 1,
        },
        Instr::Move {
            src: Operand::X(0),
            dst: Operand::Y(0),
        },
        Instr::Move {
            src: Operand::Y(0),
            dst: Operand::X(0),
        },
        // Poison the caller frame's own Y window, so a stale window read by the
        // callee is observably wrong rather than accidentally equal.
        Instr::Move {
            src: Operand::Integer(555),
            dst: Operand::Y(0),
        },
        Instr::Move {
            src: Operand::Integer(1000),
            dst: Operand::Y(1),
        },
        Instr::Call {
            arity: 1,
            target: Label(3),
        },
        Instr::Move {
            src: Operand::X(0),
            dst: Operand::Y(1),
        },
        Instr::Move {
            src: Operand::Y(1),
            dst: Operand::X(0),
        },
        Instr::Deallocate(2),
        Instr::Return,
        // The callee region: a forward local `Call` target inside the same function,
        // exactly how the loader decomposes a non-tail-recursive clause.
        Instr::Label(Label(3)),
        Instr::Allocate {
            stack_need: beam_loader::HeapNeed::Words(2),
            live: 1,
        },
        Instr::Move {
            src: Operand::X(0),
            dst: Operand::Y(0),
        },
        gc_bif(
            ImportRef(0),
            vec![Operand::Y(0), Operand::Integer(7)],
            Operand::Y(1),
        ),
        Instr::Move {
            src: Operand::Y(1),
            dst: Operand::X(0),
        },
        Instr::Deallocate(2),
        Instr::Return,
    ];
    let module = build_module(
        "yslot_frame",
        &[("erlang", "+", 2)],
        vec![("f", 1, code)],
        &[("f", 1, Label(2))],
    );
    let name = export_atom(&module, "f");
    assert!(
        fast_arith(
            &module,
            &gc_bif(
                ImportRef(0),
                vec![Operand::Y(0), Operand::Integer(7)],
                Operand::Y(1)
            )
        )
        .is_some(),
        "the callee's Y-slot '+'/2 must lower too"
    );
    with_both_tiers_unary(&module, name, 10, |interpreter, baseline| {
        assert_same_outcome("y-slot frame change", interpreter, baseline, &module.atom_table);
        match (interpreter, baseline) {
            (Ok(RunOutcome::Returned(value)), _) => assert_eq!(
                *value,
                make_small(17),
                "the callee must read its own frame (10 + 7), never the caller's poisoned window"
            ),
            other => panic!("the frame-change arithmetic did not return: {other:?}"),
        }
    });
}

/// A Y access with **no live frame's slot to name** is rejected identically by both
/// tiers.
///
/// The instruction names `Y(0)` inside a function that never pushes a frame, so the
/// process's only frame owns zero Y slots and the window is empty. The emitted guard's
/// `lea` lands at (or past) `y_end`, so it branches to `jit_step_one`, and the
/// interpreter's own `machine.y(0)` judgement — a typed error, never a wrong value and
/// never a panic — surfaces on the baseline tier unchanged.
#[test]
fn a_y_access_with_no_live_frame_is_rejected_identically_on_both_tiers() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        gc_bif(
            ImportRef(0),
            vec![Operand::Y(0), Operand::X(0)],
            Operand::X(0),
        ),
        Instr::Return,
    ];
    let module = build_module(
        "yslot_noframe",
        &[("erlang", "+", 2)],
        vec![("f", 1, code)],
        &[("f", 1, Label(2))],
    );
    let name = export_atom(&module, "f");
    assert!(
        fast_arith(&module, &y_slot_arith_instr()).is_some(),
        "the shape still lowers; the frame window is a run-time guard"
    );
    with_both_tiers_unary(&module, name, 10, |interpreter, baseline| {
        assert!(
            interpreter.is_err(),
            "the interpreter must reject a Y slot in an empty frame: {interpreter:?}"
        );
        assert!(
            baseline.is_err(),
            "the baseline tier must reject a Y slot in an empty frame: {baseline:?}"
        );
        assert_same_outcome("empty-frame Y slot", interpreter, baseline, &module.atom_table);
    });
}

/// An out-of-frame Y slot (`Y(1)` in a frame that allocated exactly **one** slot) is
/// rejected identically by both tiers.
///
/// The emitted guard only ever *rejects* (branching to `jit_step_one`), so the
/// interpreter's own judgement of the malformed frame — a typed error, never a wrong
/// value and never a panic — surfaces on the baseline tier unchanged.
#[test]
fn an_out_of_frame_y_slot_is_rejected_identically_on_both_tiers() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::Allocate {
            stack_need: beam_loader::HeapNeed::Words(1),
            live: 1,
        },
        Instr::Move {
            src: Operand::X(0),
            dst: Operand::Y(0),
        },
        gc_bif(
            ImportRef(0),
            vec![Operand::Y(0), Operand::X(0)],
            Operand::Y(1),
        ),
        Instr::Move {
            src: Operand::Y(1),
            dst: Operand::X(0),
        },
        Instr::Deallocate(1),
        Instr::Return,
    ];
    let module = build_module(
        "yslot_bad",
        &[("erlang", "+", 2)],
        vec![("f", 1, code)],
        &[("f", 1, Label(2))],
    );
    let name = export_atom(&module, "f");
    assert!(
        fast_arith(&module, &y_slot_arith_instr()).is_some(),
        "the shape still lowers; the frame window is a run-time guard"
    );
    with_both_tiers_unary(&module, name, 10, |interpreter, baseline| {
        assert!(
            interpreter.is_err(),
            "the interpreter must reject an out-of-frame Y slot: {interpreter:?}"
        );
        assert!(
            baseline.is_err(),
            "the baseline tier must reject an out-of-frame Y slot: {baseline:?}"
        );
        assert_same_outcome(
            "out-of-frame Y slot",
            interpreter,
            baseline,
            &module.atom_table,
        );
    });
}

// ---------------------------------------------------------------------------
// Y-slot `move`: the source and/or the destination may be an in-frame Y slot
// ---------------------------------------------------------------------------
//
// A `move` whose source and/or destination is a `Y` slot is lowered natively behind
// the **same shared frame-window guard** the arithmetic fast path uses
// (`operands.rs::emit_guarded_y_load`/`emit_guarded_y_store` § the guarded Y-window
// access), so the shape is purely syntactic [`move_shape`] and the run-time guard can
// only ever *reject*. The compile-time Y gate ([`uses_y`]) must claim **exactly** those
// moves — otherwise a later in-block Y access after a helper call would read a window
// published before a `machine.stack` reallocation.
// ---------------------------------------------------------------------------

/// `f/0`'s body as a chain of Y-slot moves — `Allocate{2}` · `Move 7 -> Y(0)` (a **Y
/// destination**) · `Move Y(0) -> Y(1)` (**Y source and destination**) ·
/// `Move Y(1) -> X(0)` (a **Y source**) · `Deallocate 2` · `Return`.
fn y_slot_move_code() -> Vec<Instr> {
    vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::Allocate {
            stack_need: beam_loader::HeapNeed::Words(2),
            live: 0,
        },
        Instr::Move {
            src: Operand::Integer(7),
            dst: Operand::Y(0),
        },
        Instr::Move {
            src: Operand::Y(0),
            dst: Operand::Y(1),
        },
        Instr::Move {
            src: Operand::Y(1),
            dst: Operand::X(0),
        },
        Instr::Deallocate(2),
        Instr::Return,
    ]
}

/// Every Y-slot `move` shape (a `Y` source, a `Y` destination, or both) **lowers** at
/// compile time and **executes identically** on the baseline tier and the interpreter.
#[test]
fn a_y_source_and_a_y_destination_move_lower_and_agree_across_tiers() {
    let code = y_slot_move_code();
    for instr in &code {
        if let Instr::Move { src, dst } = instr {
            assert!(
                move_shape(src, dst).is_some(),
                "a Y-slot move must lower at compile time: {src:?} -> {dst:?}"
            );
        }
    }
    let module = build_module("yslot_move", &[], vec![("f", 0, code)], &[("f", 0, Label(2))]);
    let name = export_atom(&module, "f");
    with_both_tiers(&module, name, |interpreter, baseline| {
        assert_same_outcome("y-slot move", interpreter, baseline, &module.atom_table);
        match (interpreter, baseline) {
            (Ok(RunOutcome::Returned(value)), _) => {
                assert_eq!(*value, make_small(7), "the constant travelled through Y(0)/Y(1)");
            }
            other => panic!("the Y-slot moves did not return: {other:?}"),
        }
    });
}

/// A `move` naming a Y slot **outside** the current frame is rejected identically by
/// both tiers — as a destination and as a source.
///
/// The emitted guard only ever *rejects* (branching to `jit_step_one`), so the
/// interpreter's own `y(k)` judgement — a typed error, never a wrong value and never a
/// panic — surfaces on the baseline tier unchanged. This is exactly the case a
/// fabricated value would show up in: `Move 7 -> Y(1)` must not silently succeed.
#[test]
fn an_out_of_frame_y_move_is_rejected_identically_on_both_tiers() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::Allocate {
            stack_need: beam_loader::HeapNeed::Words(1),
            live: 0,
        },
        // A frame of exactly one Y slot: `Y(1)` is outside it, both as a store …
        Instr::Move {
            src: Operand::Integer(7),
            dst: Operand::Y(1),
        },
        // … and as a load.
        Instr::Move {
            src: Operand::Y(1),
            dst: Operand::X(0),
        },
        Instr::Deallocate(1),
        Instr::Return,
    ];
    for instr in &code {
        if let Instr::Move { src, dst } = instr {
            assert!(
                move_shape(src, dst).is_some(),
                "the shape still lowers; the frame window is a run-time guard: {src:?} -> {dst:?}"
            );
        }
    }
    let module = build_module("yslot_move_bad", &[], vec![("f", 0, code)], &[("f", 0, Label(2))]);
    let name = export_atom(&module, "f");
    with_both_tiers(&module, name, |interpreter, baseline| {
        assert!(
            interpreter.is_err(),
            "the interpreter must reject an out-of-frame Y move: {interpreter:?}"
        );
        assert!(
            baseline.is_err(),
            "the baseline tier must reject an out-of-frame Y move: {baseline:?}"
        );
        assert_same_outcome(
            "out-of-frame Y move",
            interpreter,
            baseline,
            &module.atom_table,
        );
    });
}

/// A `move` naming a Y index the emitted `disp32` cannot address is refused at
/// **compile** time (a deopt — the interpreter reports the faithful error), and the
/// compile-time Y gate agrees: a refused move accesses no window, so it must not make
/// the block publish one.
#[test]
fn an_unaddressable_y_move_index_is_refused_at_compile_time() {
    let module = Module::default();
    assert!(!y_addressable(u32::MAX));
    for (src, dst) in [
        (Operand::X(0), Operand::Y(u32::MAX)),
        (Operand::Y(u32::MAX), Operand::X(0)),
        (Operand::Y(u32::MAX), Operand::Y(0)),
        (Operand::Integer(1), Operand::Y(u32::MAX)),
    ] {
        assert!(
            move_shape(&src, &dst).is_none(),
            "an un-addressable Y index must be refused at compile time: {src:?} -> {dst:?}"
        );
        assert!(
            !uses_y(&module, &Instr::Move { src, dst }),
            "a refused Y move accesses no window"
        );
    }

    // The largest index the `lea ... disp32` still addresses IS claimed, so the refusal
    // is the displacement bound and nothing else.
    let biggest = (i32::MAX / std::mem::size_of::<Eterm>() as i32) as u32;
    assert!(y_addressable(biggest));
    assert!(move_shape(&Operand::X(0), &Operand::Y(biggest)).is_some());
}

/// The compile-time Y gate ([`uses_y`]) is true **exactly** for the `move` shapes the
/// inliner claims *and* that read the frame window — the invariant `Allocation::y_last`
/// / `refresh_y_after` rest on. A move the inliner refuses deopts and reads nothing, so
/// the gate must be false for it too.
#[test]
fn the_move_y_gate_matches_the_move_inliner_exactly() {
    let module = Module::default();
    let typed_y = Operand::Typed {
        reg: RegRef::Y(1),
        ty: beam_loader::TypeIndex(0),
    };
    let typed_x = Operand::Typed {
        reg: RegRef::X(2),
        ty: beam_loader::TypeIndex(0),
    };
    let moves = vec![
        // x-only: lowered, but no window read.
        Instr::Move {
            src: Operand::X(0),
            dst: Operand::X(1),
        },
        Instr::Move {
            src: Operand::Integer(1),
            dst: Operand::X(1),
        },
        Instr::Move {
            src: Operand::Nil,
            dst: Operand::X(1),
        },
        Instr::Move {
            src: typed_x,
            dst: Operand::X(1),
        },
        // Y source and/or destination, plain and `Typed`: lowered AND a window read.
        Instr::Move {
            src: Operand::X(0),
            dst: Operand::Y(0),
        },
        Instr::Move {
            src: Operand::Integer(1),
            dst: Operand::Y(0),
        },
        Instr::Move {
            src: Operand::Y(0),
            dst: Operand::X(0),
        },
        Instr::Move {
            src: Operand::Y(0),
            dst: Operand::Y(1),
        },
        Instr::Move {
            src: typed_y.clone(),
            dst: Operand::X(0),
        },
        Instr::Move {
            src: Operand::X(0),
            dst: typed_y.clone(),
        },
        // refused shapes: an out-of-bound register or Y index (`fr`/literal operands
        // cannot even be spelled), so the whole instruction deopts.
        Instr::Move {
            src: Operand::X(MAX_REGISTER),
            dst: Operand::X(0),
        },
        Instr::Move {
            src: Operand::X(0),
            dst: Operand::X(MAX_REGISTER),
        },
        Instr::Move {
            src: Operand::Y(u32::MAX),
            dst: Operand::X(0),
        },
        Instr::Move {
            src: Operand::X(0),
            dst: Operand::Y(u32::MAX),
        },
    ];

    for instr in &moves {
        let Instr::Move { src, dst } = instr else {
            unreachable!("the table holds only moves")
        };
        let shape = move_shape(src, dst);
        let claimed_reads_y = shape.as_ref().map_or(false, MoveShape::uses_y);
        // The operand-level restatement of the same predicate: the shape is claimed and
        // one of its two sides is an addressable Y slot. The inliner claims exactly the
        // shapes `move_shape` accepts (it refuses nothing else), so the three must agree.
        let in_bound_y =
            |operand: &Operand| matches!(y_register(operand), Some(k) if y_addressable(k));
        let expected = shape.is_some() && (in_bound_y(src) || in_bound_y(dst));
        assert_eq!(claimed_reads_y, expected, "{src:?} -> {dst:?}");
        assert_eq!(
            uses_y(&module, instr),
            expected,
            "the Y gate must match the inliner for {src:?} -> {dst:?}"
        );
    }

    // An x-only move is still charged its x-registers, and a Y slot never is.
    let y_only = Instr::Move {
        src: Operand::Y(0),
        dst: Operand::Y(1),
    };
    let Instr::Move { src, dst } = &y_only else {
        unreachable!()
    };
    assert!(move_shape(src, dst).expect("Y->Y lowers").registers().is_empty());
}

/// A block whose inlined Y moves follow an in-block helper call is covered by the
/// compile-time gate: `y_last` names the last Y access, so the helper's deopt arm
/// (`Allocate`, which may reallocate `machine.stack`) re-publishes the window
/// (`refresh_y_after`) before the move reads it.
#[test]
fn the_y_window_gate_covers_the_inlined_y_moves() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::Allocate {
            stack_need: beam_loader::HeapNeed::Words(2),
            live: 0,
        },
        Instr::Move {
            src: Operand::Integer(7),
            dst: Operand::Y(0),
        },
        Instr::Move {
            src: Operand::Y(0),
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let module = build_module(
        "yslot_move_gate",
        &[],
        vec![("f", 0, code)],
        &[("f", 0, Label(2))],
    );
    let code = &module.functions[0].code;
    let allocation = allocate_registers(&module, code, 1, code.len());
    assert_eq!(
        allocation.y_last,
        Some(4),
        "the block's last Y access is the Y-source move at pc 4"
    );
    assert!(
        allocation.refresh_y_after(3),
        "the Y-destination move's own fallback is followed by the Y-source move"
    );
    assert!(
        !allocation.refresh_y_after(4),
        "nothing follows the last Y access, so its own fallback refreshes nothing"
    );

    let labels = crate::interpreter::build_label_map(&module);
    let function = compile_function(&module, &labels, 0).expect("f/0 compiles");
    assert!(
        code_contains_helper(&function, jit_publish_y as *const ()),
        "a block with an inlined Y move publishes the window from its prologue"
    );
    assert!(
        code_contains_helper(&function, jit_refresh_y as *const ()),
        "the `allocate` deopt re-publishes the window before the Y move reads it"
    );
}

// ---------------------------------------------------------------------------
// the Y-window publication is gated at compile time
// ---------------------------------------------------------------------------
/// Whether the function's sealed machine code contains a `movabs r11, <helper>`
/// immediate — the `49 BB <imm64>` shape every helper call emits (the same probe
/// `the_compiled_block_wires_the_ext_call_helper_into_native_code` uses).
fn code_contains_helper(function: &super::code::CompiledFunction, helper: *const ()) -> bool {
    let start = function.buffer.code_ptr() as usize;
    let len = function.buffer.len();
    // SAFETY: `[start, start + len)` is the function's live, sealed executable mapping,
    // which `function` owns and keeps mapped for this borrow; reading it as bytes is
    // valid and does not execute it.
    let code = unsafe { std::slice::from_raw_parts(start as *const u8, len) };
    let mut needle = vec![0x49u8, 0xBB];
    needle.extend_from_slice(&(helper as u64).to_le_bytes());
    code.windows(needle.len())
        .any(|window| window == needle.as_slice())
}

/// `Allocation::y_last` / `Allocation::refresh_y_after` gate both Y-window emissions,
/// and the emitted code follows: a block whose body accesses a Y slot publishes the
/// window in its prologue (`jit_publish_y`) and re-publishes it after an in-block
/// helper call while a later instruction still accesses Y (`jit_refresh_y`), while a
/// Y-free block emits neither.
///
/// The code-generation probe searches the sealed machine code for each helper's
/// `movabs r11, <helper>` immediate. The publish is a **substitution** for the plain
/// position store `jit_set_position` — the two share the identical three-argument call
/// shape, so their byte lengths are equal and a length comparison would not isolate the
/// gate; the helper-address probe pins it exactly instead.
#[test]
fn the_y_window_publication_is_gated_at_compile_time() {
    // Two `gc_bif`s in one block: the first's fallback (emitted on the **continue** arm)
    // is gated on the second, later Y access, so both the prologue publish and the
    // gated refresh are emitted.
    let y_module = build_module(
        "ywin_y",
        &[("erlang", "+", 2)],
        vec![(
            "f",
            0,
            vec![
                Instr::Label(Label(1)),
                Instr::Label(Label(2)),
                Instr::Move {
                    src: Operand::X(0),
                    dst: Operand::Y(0),
                },
                gc_bif(
                    ImportRef(0),
                    vec![Operand::Y(0), Operand::Integer(1)],
                    Operand::Y(1),
                ),
                gc_bif(
                    ImportRef(0),
                    vec![Operand::Y(1), Operand::Integer(1)],
                    Operand::X(0),
                ),
                Instr::Return,
            ],
        )],
        &[("f", 0, Label(2))],
    );
    let y_code = &y_module.functions[0].code;
    let allocation = allocate_registers(&y_module, y_code, 1, y_code.len());
    // pc 4 (the second `gc_bif`) is the block's last Y access.
    assert_eq!(allocation.y_last, Some(4), "the last Y access is at pc 4");
    assert!(
        !allocation.refresh_y_after(4),
        "nothing follows the last Y access, so its own fallback refreshes nothing"
    );
    assert!(
        allocation.refresh_y_after(3),
        "the first `gc_bif`'s fallback is followed by the second Y access"
    );
    assert!(allocation.refresh_y_after(2), "the `move` is followed by a Y access");

    let labels = crate::interpreter::build_label_map(&y_module);
    let y_function = compile_function(&y_module, &labels, 0).expect("f/0 compiles");
    assert!(
        code_contains_helper(&y_function, jit_publish_y as *const ()),
        "a Y-touching block publishes the window from its prologue"
    );
    assert!(
        code_contains_helper(&y_function, jit_refresh_y as *const ()),
        "a Y-touching block refreshes the window after an in-block helper call"
    );

    // The same block over x-registers only: no Y access, so `y_last` is `None` and
    // neither helper is emitted anywhere.
    let x_module = build_module(
        "ywin_x",
        &[("erlang", "+", 2)],
        vec![(
            "f",
            0,
            vec![
                Instr::Label(Label(1)),
                Instr::Label(Label(2)),
                Instr::Move {
                    src: Operand::X(0),
                    dst: Operand::X(1),
                },
                gc_bif(
                    ImportRef(0),
                    vec![Operand::X(0), Operand::Integer(1)],
                    Operand::X(1),
                ),
                gc_bif(
                    ImportRef(0),
                    vec![Operand::X(1), Operand::Integer(1)],
                    Operand::X(0),
                ),
                Instr::Return,
            ],
        )],
        &[("f", 0, Label(2))],
    );
    let x_code = &x_module.functions[0].code;
    let allocation = allocate_registers(&x_module, x_code, 1, x_code.len());
    assert!(allocation.y_last.is_none(), "the x-register block accesses no Y slot");
    for pc in 1..x_code.len() as u32 {
        assert!(
            !allocation.refresh_y_after(pc),
            "a Y-free block refreshes nothing at pc {pc}"
        );
    }
    let labels = crate::interpreter::build_label_map(&x_module);
    let x_function = compile_function(&x_module, &labels, 0).expect("f/0 compiles");
    assert!(
        !code_contains_helper(&x_function, jit_publish_y as *const ()),
        "a Y-free block emits the plain position store, not the window publish"
    );
    assert!(
        !code_contains_helper(&x_function, jit_refresh_y as *const ()),
        "a Y-free block emits no window refresh"
    );
}

// ---------------------------------------------------------------------------
// the continuation sub-block runs natively
// ---------------------------------------------------------------------------

/// A frame-preserving `call`'s continuation address — which falls **inside** an IR
/// block — executes on a native sub-block in the lazy per-function layout the runtime
/// driver uses (`CodeCache::compiled_handle`), and the run returns the same value as
/// the interpreter.
///
/// `local_call_case`'s `f/0` calls `g/0` at pc 3; the continuation pc 4 is strictly
/// inside `f/0`'s `Label(2)` block `[1, 5)`, so the driver re-enters `f/0` at the
/// sub-block after `g/0` returns rather than stepping the interpreter. The compile-time
/// assertions pin the LAZY per-function table (`entries[4]` empty, `sub_entries[4]`
/// present) as the sub-block path that ran.
#[test]
fn a_continuation_sub_block_executes_and_agrees_across_tiers() {
    let (module, name) = local_call_case();
    let labels = crate::interpreter::build_label_map(&module);
    let function = compile_function(&module, &labels, 0).expect("f/0 compiles");
    assert!(function.entries[4].is_none(), "pc 4 is not an IR block start");
    assert!(
        function.sub_entries[4].is_some(),
        "pc 4 is a continuation sub-block of the lazy function"
    );

    with_both_tiers(&module, name, |interpreter, baseline| {
        assert_same_outcome(
            "continuation sub-block",
            interpreter,
            baseline,
            &module.atom_table,
        );
        match interpreter {
            Ok(RunOutcome::Returned(value)) => {
                assert_eq!(*value, make_small(7), "g/0 moved 7 into x(0)")
            }
            other => panic!("the continuation sub-block run did not return: {other:?}"),
        }
    });
}

// ---------------------------------------------------------------------------
// the frame-discipline inliners: `deallocate` / `return` / `deallocate_return`
//
// The three instructions that end a function body are lowered natively instead of
// paying a full `jit_step_one` interpreter step. Each makes one dedicated helper
// round-trip (`jit_deallocate`/`jit_return`/`jit_deallocate_return`) that performs
// only *this* instruction's `Machine` work — the interpreter's own
// `Machine::pop_frame_checked` and `Machine::continuation()` calls — and hands the
// outcome back as the block-ABI word the driver already decodes. The agreement
// tests below pin the interpreter-identical outcome; the code probes pin that the
// lowering is the dedicated helper (a deopt would carry `jit_step_one` instead).
// ---------------------------------------------------------------------------

/// A **separate** `deallocate N; return` pair (the hand-built shape — the loader
/// fuses it into `Instr::DeallocateReturn`, but a synthetic module keeps both
/// instructions) is lowered as **one** fused helper round-trip and returns the
/// interpreter's value.
#[test]
fn a_separate_deallocate_return_pair_is_fused_and_agrees_across_tiers() {
    let (module, name) = body_module(vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::Allocate {
            stack_need: beam_loader::HeapNeed::Words(0),
            live: 0,
        },
        Instr::Move {
            src: Operand::Integer(7),
            dst: Operand::X(0),
        },
        Instr::Deallocate(0),
        Instr::Return,
    ]);

    let labels = crate::interpreter::build_label_map(&module);
    let function = compile_function(&module, &labels, 0).expect("f/0 compiles");
    assert!(
        code_contains_helper(&function, jit_deallocate_return as *const ()),
        "the adjacent `deallocate 0; return` pair lowers as one fused helper round-trip"
    );
    assert!(
        !code_contains_helper(&function, jit_deallocate as *const ()),
        "the pair is fused, so no lone `deallocate` helper is emitted"
    );

    with_both_tiers(&module, name, |interpreter, baseline| {
        assert_same_outcome(
            "separate deallocate/return",
            interpreter,
            baseline,
            &module.atom_table,
        );
        match interpreter {
            Ok(RunOutcome::Returned(value)) => {
                assert_eq!(*value, make_small(7), "the constant reached x(0)")
            }
            other => panic!("the frame-exit body did not return: {other:?}"),
        }
    });
}

/// The loader's fused superinstruction `Instr::DeallocateReturn(N)` — the shape every
/// compiler-produced function body ends with — is lowered through the dedicated
/// helper and returns the interpreter's value.
#[test]
fn the_fused_deallocate_return_lowers_and_agrees_across_tiers() {
    let (module, name) = body_module(vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::Allocate {
            stack_need: beam_loader::HeapNeed::Words(0),
            live: 0,
        },
        Instr::Move {
            src: Operand::Integer(9),
            dst: Operand::X(0),
        },
        Instr::DeallocateReturn(0),
    ]);

    let labels = crate::interpreter::build_label_map(&module);
    let function = compile_function(&module, &labels, 0).expect("f/0 compiles");
    assert!(
        code_contains_helper(&function, jit_deallocate_return as *const ()),
        "the fused superinstruction lowers through `jit_deallocate_return`"
    );

    with_both_tiers(&module, name, |interpreter, baseline| {
        assert_same_outcome(
            "fused deallocate_return",
            interpreter,
            baseline,
            &module.atom_table,
        );
        match interpreter {
            Ok(RunOutcome::Returned(value)) => {
                assert_eq!(*value, make_small(9), "the constant reached x(0)")
            }
            other => panic!("the fused exit did not return: {other:?}"),
        }
    });
}

/// A `deallocate` operand that no longer matches the live frame is rejected by the
/// tier's checked pop with the interpreter's **exact** typed error, and nothing is
/// fabricated — the `jit_step_one` fallback the interpreter would take is gone, so
/// the helper's own error is the only path.
#[test]
fn a_checked_pop_mismatch_is_rejected_identically_on_both_tiers() {
    // A frame of exactly one Y slot; the `deallocate` operand claims two.
    let (module, name) = body_module(vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::Allocate {
            stack_need: beam_loader::HeapNeed::Words(1),
            live: 0,
        },
        Instr::Deallocate(2),
        Instr::Return,
    ]);

    with_both_tiers(&module, name, |interpreter, baseline| {
        assert!(
            interpreter.is_err(),
            "the interpreter's checked pop rejects the mismatch: {interpreter:?}"
        );
        assert!(
            baseline.is_err(),
            "the baseline tier's checked pop rejects the mismatch too: {baseline:?}"
        );
        assert_same_outcome(
            "stale deallocate operand",
            interpreter,
            baseline,
            &module.atom_table,
        );
        assert_eq!(
            error_reason(interpreter.as_ref().expect_err("the pop raised")),
            "unsupported_opcode: deallocate: frame size mismatch",
            "the tier must report the interpreter's own checked-pop error"
        );
    });
}

/// A bare `return` with **no** `deallocate` (a frame-less function's exit) is lowered
/// through `jit_return` and follows the bottom frame's `Continuation::Halt`, yielding
/// `x(0)` on both tiers.
#[test]
fn a_return_on_the_bottom_frame_halts_identically_on_both_tiers() {
    let (module, name) = body_module(vec![
        Instr::Label(Label(1)),
        Instr::Label(Label(2)),
        Instr::Move {
            src: Operand::Integer(5),
            dst: Operand::X(0),
        },
        Instr::Return,
    ]);

    let labels = crate::interpreter::build_label_map(&module);
    let function = compile_function(&module, &labels, 0).expect("f/0 compiles");
    assert!(
        code_contains_helper(&function, jit_return as *const ()),
        "a bare `return` lowers through the dedicated `jit_return` helper"
    );

    with_both_tiers(&module, name, |interpreter, baseline| {
        assert_same_outcome(
            "bottom-frame return",
            interpreter,
            baseline,
            &module.atom_table,
        );
        assert!(
            matches!(interpreter, Ok(RunOutcome::Returned(value)) if *value == make_small(5)),
            "the bottom `Halt` must yield x(0): {interpreter:?}"
        );
    });
}

// ===========================================================================
// PERFORMANCE PROBE — the two-process `send_recv` copy-bound shape
// ===========================================================================
//
// `tools/bench` measures the baseline tier only through the **eager** artifact
// (`jit_bench`), so the tier's real cost — the **lazy** per-function path the
// runtime drives (`run_baseline_cached*` / `Machine::run_*_with_tier` /
// `resume_outcome_with_tier`) — has no in-process measurement. This probe closes
// that gap for the ONE workload where the baseline tier is dearer than the
// interpreter (`tools/bench/RESULTS.md` §3.9 / §8 item 2): two processes
// ping-ponging a message, each `receive` **suspending** on an empty mailbox and
// being resumed by the counterpart's `send`.
//
// The IR mirrors the shape the system `erlc` emits for that workload
// (`erlc -S` on OTP 29 / erts-17.1; see `../CONTEXT.md` and RESULTS.md §8):
// `send` / `loop_rec` / `is_tuple` + `test_arity` + `get_tuple_element` +
// `is_eq_exact` (Y-annotated boxed operand) / `remove_message` / `wait` /
// `wait_timeout` / `put_tuple2` / `bif self` / `gc_bif '+'/'-'` on a `Y` operand /
// a linked `call_last` / a frame-preserving `call` (continuation sub-block).
//
// It is a *measurement*, not an assertion of performance: it pins cross-tier
// **result** equality plus the structural counters, and prints ns/op for both
// tiers together with `/proc/loadavg` (this host is shared — ratios and counters
// are the signal, absolute latency is not).

/// Warmup ops (discarded): populates the per-thread compiled-block cache so the
/// timed pass measures steady state, not first-entry compilation.
const PROBE_WARMUP_OPS: u32 = 1_000;
/// Timed ops: one op = one full `send`+`receive`+suspend+resume round trip.
const PROBE_MEASURE_OPS: u32 = 20_000;
/// Counting-pass ops: fewer, because recording re-formats an opcode name per deopt.
const PROBE_COUNT_OPS: u32 = 64;

/// The two "pids" the probe uses — small integers, so the mailbox routes on a
/// plain word compare (the engine never inspects a pid term, only the host does).
const PROBE_PID_A: i64 = 1;
const PROBE_PID_B: i64 = 2;

/// A mailbox that routes a `send` to the **peer** process's inbox when the
/// destination is the peer pid, and to its own inbox otherwise.
///
/// The two inboxes are shared `Rc`s the probe owns, so the cooperative driver
/// below can observe both when the ping-pong completes.
#[derive(Clone)]
struct PeeredMailbox {
    /// This process's inbox.
    own: std::rc::Rc<std::cell::RefCell<Vec<Eterm>>>,
    /// The counterpart's inbox (where a `send` addressed to the peer pid lands).
    peer: std::rc::Rc<std::cell::RefCell<Vec<Eterm>>>,
    /// The peer's pid term.
    peer_pid: Eterm,
    /// This process's scan position (`loop_rec_end` advances it).
    scan: std::rc::Rc<std::cell::Cell<usize>>,
}

impl PeeredMailbox {
    fn new(
        own: std::rc::Rc<std::cell::RefCell<Vec<Eterm>>>,
        peer: std::rc::Rc<std::cell::RefCell<Vec<Eterm>>>,
        peer_pid: Eterm,
    ) -> PeeredMailbox {
        PeeredMailbox {
            own,
            peer,
            peer_pid,
            scan: std::rc::Rc::new(std::cell::Cell::new(0)),
        }
    }
}

impl crate::Mailbox for PeeredMailbox {
    fn send_message(&mut self, to: Eterm, message: Eterm) -> Result<()> {
        if to == self.peer_pid {
            self.peer.borrow_mut().push(message);
        } else {
            self.own.borrow_mut().push(message);
        }
        Ok(())
    }

    fn receive_next(&mut self) -> Result<Option<Eterm>> {
        let scan = self.scan.get();
        Ok(self.own.borrow().get(scan).copied())
    }

    fn receive_skip(&mut self) -> Result<()> {
        self.scan.set(self.scan.get() + 1);
        Ok(())
    }

    fn receive_remove(&mut self) -> Result<()> {
        let scan = self.scan.get();
        let mut own = self.own.borrow_mut();
        if scan < own.len() {
            own.remove(scan);
        }
        self.scan.set(0);
        Ok(())
    }

    fn receive_reset(&mut self) -> Result<()> {
        self.scan.set(0);
        Ok(())
    }

    // The probe's receive shape carries no `recv_marker_*` instruction, so the
    // marker family stays the seam's honest typed "cannot serve it" answer.
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

/// The two inboxes plus the two pids the probe's processes route between.
struct ProbeRouter {
    a_inbox: std::rc::Rc<std::cell::RefCell<Vec<Eterm>>>,
    b_inbox: std::rc::Rc<std::cell::RefCell<Vec<Eterm>>>,
    a_pid: Eterm,
    b_pid: Eterm,
}

impl ProbeRouter {
    fn new() -> ProbeRouter {
        ProbeRouter {
            a_inbox: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
            b_inbox: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
            a_pid: make_small(PROBE_PID_A),
            b_pid: make_small(PROBE_PID_B),
        }
    }
}

/// A BIF hook serving the three BIFs the probe's modules import:
/// `erlang:self/0` (the process's own pid), and `erlang:'-'/2` / `erlang:'+'/2`
/// over small integers. The baseline tier inlines the arithmetic fast path, so
/// the hook is only reached on the interpreter tier — which is exactly the point.
fn probe_bif_hook(self_pid: Eterm) -> crate::context::BifHook {
    Box::new(
        move |module: &str, function: &str, arity: u32, args: &[Eterm]| {
            if module == "erlang" && function == "self" && arity == 0 {
                return Ok(self_pid);
            }
            if module == "erlang"
                && arity == 2
                && (function == "-" || function == "+")
                && args.len() == 2
            {
                let lhs = beam_term::signed_value(args[0]);
                let rhs = beam_term::signed_value(args[1]);
                let value = if function == "-" { lhs - rhs } else { lhs + rhs };
                return Ok(make_small(value));
            }
            Err(crate::error::unsupported(&format!(
                "srbench bif {module}:{function}/{arity}"
            )))
        },
    )
}

/// The driver-process context: `bench/3`'s own pid, its inbox as `own` and the
/// worker's as `peer`.
fn probe_bench_context(router: &ProbeRouter) -> MinimalContext {
    let mut ctx = MinimalContext::with_bif_hook(probe_bif_hook(router.a_pid));
    ctx.set_mailbox(Box::new(PeeredMailbox::new(
        std::rc::Rc::clone(&router.a_inbox),
        std::rc::Rc::clone(&router.b_inbox),
        router.b_pid,
    )));
    ctx
}

/// The worker-process context: `worker/1`'s own pid, its inbox as `own` and the
/// driver's as `peer`.
fn probe_worker_context(router: &ProbeRouter) -> MinimalContext {
    let mut ctx = MinimalContext::with_bif_hook(probe_bif_hook(router.b_pid));
    ctx.set_mailbox(Box::new(PeeredMailbox::new(
        std::rc::Rc::clone(&router.b_inbox),
        std::rc::Rc::clone(&router.a_inbox),
        router.a_pid,
    )));
    ctx
}

/// `srbench:bench/3` — the driver loop of the copy-bound workload.
///
/// One iteration: build the payload with a **frame-preserving local call**
/// (`call` → the continuation sub-block path), `self()`, `put_tuple2`, `send`,
/// then the receive match (`loop_rec` / is_tuple / test_arity /
/// get_tuple_element / is_eq_exact against a `Y` operand / remove_message /
/// is_eq_exact against the bound variable), the two arithmetic `gc_bif`s on a `Y`
/// operand, and a **linked** `call_last` back to the entry. An empty mailbox
/// suspends at `wait_timeout`, resuming at the `loop_rec` label.
fn probe_bench_code() -> Vec<Instr> {
    vec![
        Instr::Label(Label(1)), // the function label
        Instr::Label(Label(2)), // the export entry
        Instr::Allocate {
            stack_need: beam_loader::HeapNeed::Words(4),
            live: 3,
        },
        Instr::InitYregs {
            registers: vec![Operand::Y(0)],
        },
        Instr::Move {
            src: Operand::X(0),
            dst: Operand::Y(1),
        }, // y(1) = N
        Instr::Move {
            src: Operand::X(1),
            dst: Operand::Y(2),
        }, // y(2) = Worker
        Instr::Move {
            src: Operand::X(2),
            dst: Operand::Y(3),
        }, // y(3) = Acc
        Instr::Label(Label(3)), // the loop head (also the tail-call target)
        Instr::Test {
            kind: TestKind::IsEqExact,
            fail: Label(4),
            args: vec![Operand::X(0), Operand::Integer(0)],
        },
        Instr::Move {
            src: Operand::X(2),
            dst: Operand::X(0),
        },
        Instr::DeallocateReturn(4),
        Instr::Label(Label(4)), // N /= 0
        Instr::Call {
            arity: 0,
            target: Label(31),
        }, // T = payload()
        Instr::TestHeap {
            heap_need: beam_loader::HeapNeed::Words(3),
            live: 1,
        },
        Instr::Bif {
            arity: 0,
            bif: ImportRef(0),
            fail: NO_FAIL,
            args: vec![],
            dst: Operand::X(1),
        }, // self()
        Instr::PutTuple2 {
            dst: Operand::X(1),
            elements: vec![Operand::X(1), Operand::X(0)],
        }, // {self(), T}
        Instr::Move {
            src: Operand::X(0),
            dst: Operand::Y(0),
        }, // y(0) = T (the `case T2 of T` bound variable)
        Instr::Move {
            src: Operand::Y(2),
            dst: Operand::X(0),
        }, // x(0) = Worker
        Instr::Send,
        Instr::Label(Label(5)), // the receive loop head (the resume target)
        Instr::LoopRec {
            fail: Label(7),
            dst: Operand::X(0),
        },
        Instr::Test {
            kind: TestKind::IsTuple,
            fail: Label(6),
            args: vec![Operand::X(0)],
        },
        Instr::Test {
            kind: TestKind::TestArity,
            fail: Label(6),
            args: vec![Operand::X(0), Operand::Integer(2)],
        },
        Instr::GetTupleElement {
            source: Operand::X(0),
            element: 0,
            dst: Operand::X(1),
        },
        Instr::Test {
            kind: TestKind::IsEqExact,
            fail: Label(6),
            args: vec![Operand::X(1), Operand::Y(2)],
        }, // the sender must be the worker (a `Y`-annotated boxed operand)
        Instr::GetTupleElement {
            source: Operand::X(0),
            element: 1,
            dst: Operand::X(0),
        },
        Instr::RemoveMessage,
        Instr::Test {
            kind: TestKind::IsEqExact,
            fail: Label(8),
            args: vec![Operand::X(0), Operand::Y(0)],
        }, // `case T2 of T`
        Instr::GcBif {
            arity: 2,
            bif: ImportRef(1),
            fail: NO_FAIL,
            live: 3,
            args: vec![Operand::Y(1), Operand::Integer(1)],
            dst: Operand::X(0),
        }, // x(0) = N - 1
        Instr::GcBif {
            arity: 2,
            bif: ImportRef(2),
            fail: NO_FAIL,
            live: 3,
            args: vec![Operand::Y(3), Operand::Integer(1)],
            dst: Operand::X(2),
        }, // x(2) = Acc + 1
        Instr::Move {
            src: Operand::Y(2),
            dst: Operand::X(1),
        }, // x(1) = Worker
        Instr::CallLast {
            arity: 3,
            target: Label(2),
            deallocate: 4,
        },
        Instr::Label(Label(6)), // a non-matching message
        Instr::LoopRecEnd { target: Label(5) },
        Instr::Label(Label(7)), // an empty mailbox
        Instr::WaitTimeout {
            target: Label(5),
            time: Operand::Integer(30000),
        },
        Instr::Timeout,
        Instr::DeallocateReturn(4),
        Instr::Label(Label(8)), // `T2 =/= T`
        Instr::CaseEnd {
            value: Operand::X(0),
        },
    ]
}

/// `srbench:payload/0` — a leaf that builds a 3-tuple (the per-iteration term the
/// workload copies). Reached through the frame-preserving `call` above, so the
/// caller's continuation is a **sub-block** entry.
fn probe_payload_code() -> Vec<Instr> {
    vec![
        Instr::Label(Label(31)),
        Instr::TestHeap {
            heap_need: beam_loader::HeapNeed::Words(4),
            live: 0,
        },
        Instr::PutTuple2 {
            dst: Operand::X(0),
            elements: vec![Operand::Integer(1), Operand::Integer(2), Operand::Integer(3)],
        },
        Instr::Return,
    ]
}

/// `srbench:worker/1` — the counterpart process: receive `{From, T}`, reply
/// `{self(), T}`, loop. Suspends at `wait` on an empty mailbox.
fn probe_worker_code() -> Vec<Instr> {
    vec![
        Instr::Label(Label(41)), // the function + export entry label
        Instr::Allocate {
            stack_need: beam_loader::HeapNeed::Words(2),
            live: 1,
        },
        Instr::InitYregs {
            registers: vec![Operand::Y(0)],
        },
        Instr::Label(Label(42)), // the receive loop head (the resume target)
        Instr::LoopRec {
            fail: Label(45),
            dst: Operand::X(0),
        },
        Instr::Test {
            kind: TestKind::IsTuple,
            fail: Label(43),
            args: vec![Operand::X(0)],
        },
        Instr::Test {
            kind: TestKind::TestArity,
            fail: Label(43),
            args: vec![Operand::X(0), Operand::Integer(2)],
        },
        Instr::Move {
            src: Operand::X(0),
            dst: Operand::Y(0),
        }, // y(0) = the message
        Instr::RemoveMessage,
        Instr::GetTupleElement {
            source: Operand::Y(0),
            element: 0,
            dst: Operand::X(0),
        }, // x(0) = From (a `Y` source, so the reader deopts)
        Instr::TestHeap {
            heap_need: beam_loader::HeapNeed::Words(3),
            live: 1,
        },
        Instr::Bif {
            arity: 0,
            bif: ImportRef(0),
            fail: NO_FAIL,
            args: vec![],
            dst: Operand::X(2),
        }, // self()
        Instr::GetTupleElement {
            source: Operand::Y(0),
            element: 1,
            dst: Operand::X(3),
        }, // x(3) = T
        Instr::PutTuple2 {
            dst: Operand::X(1),
            elements: vec![Operand::X(2), Operand::X(3)],
        }, // x(1) = {self(), T}
        Instr::Send,
        Instr::CallLast {
            arity: 1,
            target: Label(41),
            deallocate: 2,
        },
        Instr::Label(Label(43)), // a non-matching message
        Instr::LoopRecEnd { target: Label(42) },
        Instr::Label(Label(45)), // an empty mailbox
        Instr::Wait { target: Label(42) },
    ]
}

/// The probe's two modules, with their export atoms.
fn probe_modules() -> (Module, Atom, Module, Atom) {
    let bench = build_module(
        "srbench",
        &[("erlang", "self", 0), ("erlang", "-", 2), ("erlang", "+", 2)],
        vec![("bench", 3, probe_bench_code()), ("payload", 0, probe_payload_code())],
        &[("bench", 3, Label(2))],
    );
    let bench_atom = export_atom(&bench, "bench");
    let worker = build_module(
        "srworker",
        &[("erlang", "self", 0)],
        vec![("worker", 1, probe_worker_code())],
        &[("worker", 1, Label(41))],
    );
    let worker_atom = export_atom(&worker, "worker");
    (bench, bench_atom, worker, worker_atom)
}

/// Drive `ops` round trips of the two-process ping-pong on `tier`, returning
/// `(result, reductions_charged, inbox_leftovers)`.
///
/// The two machines are driven cooperatively: the worker parks first (empty
/// inbox), the driver sends and parks, and then the two alternate — worker resume
/// (consume + reply), driver resume (consume + next send) — until the driver's
/// counter reaches zero and `bench/3` returns the iteration count.
fn drive_probe(
    bench: &Module,
    bench_atom: Atom,
    worker: &Module,
    worker_atom: Atom,
    ops: u32,
    tier: Tier,
) -> Result<(Eterm, u64, (usize, usize))> {
    let router = ProbeRouter::new();
    let mut driver_ctx = probe_bench_context(&router);
    let mut worker_ctx = probe_worker_context(&router);
    let mut driver = Machine::new();
    let mut worker_machine = Machine::new();
    let mut reductions = 0u64;

    // The worker parks on the empty inbox first.
    let mut worker_outcome = worker_machine.run_export_outcome_with_tier(
        worker,
        worker_atom,
        1,
        &[router.a_pid],
        &mut worker_ctx,
        tier,
    )?;
    reductions += worker_machine.reductions();

    // The driver sends the first message and parks waiting for the reply.
    let mut driver_outcome = driver.run_export_outcome_with_tier(
        bench,
        bench_atom,
        3,
        &[make_small(ops as i64), router.b_pid, make_small(0)],
        &mut driver_ctx,
        tier,
    )?;
    reductions += driver.reductions();

    let mut result = None;
    for _ in 0..=ops {
        if let RunOutcome::Returned(value) = driver_outcome {
            result = Some(value);
            break;
        }
        let (func, pc) = match driver_outcome {
            RunOutcome::Wait { func, pc } => (func, pc),
            RunOutcome::Returned(_) => unreachable!("handled above"),
        };
        let (worker_func, worker_pc) = match worker_outcome {
            RunOutcome::Wait { func, pc } => (func, pc),
            RunOutcome::Returned(_) => {
                return Err(crate::error::unsupported(
                    "srbench probe: the worker returned before the driver",
                ))
            }
        };
        worker_outcome = worker_machine.resume_outcome_with_tier(
            worker,
            worker_func,
            worker_pc,
            &mut worker_ctx,
            tier,
        )?;
        reductions += worker_machine.reductions();

        driver_outcome =
            driver.resume_outcome_with_tier(bench, func, pc, &mut driver_ctx, tier)?;
        reductions += driver.reductions();
    }

    let value = result.ok_or_else(|| {
        crate::error::unsupported("srbench probe: the driver never returned")
    })?;
    let a_left = router.a_inbox.borrow().len();
    let b_left = router.b_inbox.borrow().len();
    Ok((value, reductions, (a_left, b_left)))
}

/// `/proc/loadavg` as a string (empty when unreadable) — the probe's timing
/// numbers are only meaningful next to the host load.
fn probe_loadavg() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "<unavailable>".to_string())
}

/// The printed structural counter block for one tier's counting pass.
fn print_probe_counters(label: &str, counters: &counters::Counters) {
    println!("--- {label}: baseline structural counters (one pass, recording ON) ---");
    println!(
        "  jit_step_one={} jit_step_ext={} | block_entry lookups={} hits={} entered={} | \
         driver round_trips={} driver_interp_steps={}",
        counters.step_one,
        counters.step_ext,
        counters.block_entry_lookups,
        counters.block_entry_hits,
        counters.block_entry_natives,
        counters.driver_round_trips,
        counters.driver_interp_steps,
    );
    println!(
        "  prologue: jit_set_position={} jit_publish_y={} jit_refresh_y={}",
        counters.set_position, counters.publish_y, counters.refresh_y
    );
    println!(
        "  call_ext: resolved={} declined={} fallback_to_step_one={}",
        counters.ext_call_resolved, counters.ext_call_declined, counters.ext_call_fallback
    );
    println!("  driver round trips by word: {:?}", counters.round_trips_by_word);
    let mut rows: Vec<(&String, &u64)> = counters.deopts_by_variant.iter().collect();
    rows.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
    println!("  deopts by variant (descending):");
    for (name, count) in rows {
        println!("    {name:<26} {count}");
    }
}

/// The focused, in-process measurement of the two-process copy-bound shape: it
/// pins cross-tier result equality, prints ns/op for **both** tiers with
/// `/proc/loadavg`, and then runs one recording pass per tier to dump the
/// structural counters (per-opcode deopts, block-entry/round-trip and prologue
/// counts, and the reductions each tier charges).
///
/// This is a *diagnosis* harness: it asserts only correctness and reaches no
/// conclusion from the wall-clock numbers (the host is shared).
#[test]
fn the_two_process_copy_bound_shape_is_measured_on_both_tiers() {
    counters::set_enabled(false);
    counters::reset();

    let (bench, bench_atom, worker, worker_atom) = probe_modules();

    // 1. Correctness on both tiers, on a small op count.
    for tier in [Tier::Interpreter, Tier::BaselineJit] {
        let (value, _reductions, leftovers) =
            drive_probe(&bench, bench_atom, &worker, worker_atom, 16, tier)
                .unwrap_or_else(|e| panic!("{tier:?}: {e:?}"));
        assert_eq!(
            value,
            make_small(16),
            "{tier:?}: bench/3 must return the iteration count"
        );
        assert_eq!(
            leftovers,
            (0, 0),
            "{tier:?}: every message must be consumed by both processes"
        );
    }

    println!("=== srbench two-process copy-bound probe ===");
    println!("loadavg (start): {}", probe_loadavg());

    // 2. Timed passes: warmup (populates the compiled-block cache) then measure.
    for tier in [Tier::Interpreter, Tier::BaselineJit] {
        let _ = drive_probe(
            &bench,
            bench_atom,
            &worker,
            worker_atom,
            PROBE_WARMUP_OPS,
            tier,
        )
        .expect("the warmup pass completes");
        let start = std::time::Instant::now();
        let (value, reductions, _) = drive_probe(
            &bench,
            bench_atom,
            &worker,
            worker_atom,
            PROBE_MEASURE_OPS,
            tier,
        )
        .expect("the timed pass completes");
        let elapsed = start.elapsed();
        assert_eq!(value, make_small(PROBE_MEASURE_OPS as i64));
        println!(
            "{:>12} tier: {:>11.2} ns/op over {} ops in {:?} (reductions charged across all slices: {})",
            tier.name(),
            elapsed.as_nanos() as f64 / PROBE_MEASURE_OPS as f64,
            PROBE_MEASURE_OPS,
            elapsed,
            reductions,
        );
    }

    // 3. Counting passes (recording ON) — one per tier, small op count.
    for tier in [Tier::Interpreter, Tier::BaselineJit] {
        counters::reset();
        counters::set_enabled(true);
        let (value, reductions, _) = drive_probe(
            &bench,
            bench_atom,
            &worker,
            worker_atom,
            PROBE_COUNT_OPS,
            tier,
        )
        .expect("the counting pass completes");
        counters::set_enabled(false);
        let snapshot = counters::snapshot();
        assert_eq!(value, make_small(PROBE_COUNT_OPS as i64));
        println!(
            "=== counting pass: {:>12} tier, {} ops, reductions charged = {} ===",
            tier.name(),
            PROBE_COUNT_OPS,
            reductions,
        );
        print_probe_counters(tier.name(), &snapshot);
    }

    println!("loadavg (end): {}", probe_loadavg());
}

/// **The entry-lookup pin**: the baseline tier's per-slice artifact lookup must not
/// re-hash the module's decoded IR.
///
/// `CodeCache::compiled_handle` takes a bare `&Module` on the tier-entry path
/// (`run_baseline_cached{,_outcome,_label_outcome,_resume_outcome}`), and its
/// address-keyed resolution memo can only be filled from a shared `Arc<Module>`
/// (the `Weak` liveness proof a pointer key needs). So a host that holds the
/// module as an `Arc` and warms the memo once — the way the cross-module callee
/// path already does — makes every later slice lookup an O(1) probe; a host that
/// does not pays one full `module_fingerprint` re-hash **per slice**.
///
/// This test measures the re-hash count on the two-process copy-bound shape and
/// asserts the *warmed* one is zero, which is what makes the tier's own per-slice
/// overhead visible instead of being dominated by the IR hash. It names the tier
/// explicitly (`Tier::BaselineJit`).
#[test]
fn the_entry_lookup_does_not_rehash_the_module_ir_once_the_memo_is_warmed() {
    let (bench, bench_atom, worker, worker_atom) = probe_modules();
    let bench = std::sync::Arc::new(bench);
    let worker = std::sync::Arc::new(worker);

    // Cold: the tier-entry lookup takes the content-hash path on every slice.
    let before = crate::jit::cache::fingerprint_calls();
    let (value, _, _) = drive_probe(&bench, bench_atom, &worker, worker_atom, 16, Tier::BaselineJit)
        .expect("the cold probe completes");
    let cold = crate::jit::cache::fingerprint_calls() - before;
    assert_eq!(value, make_small(16));
    assert!(
        cold >= 32,
        "the unwarmed tier-entry path is expected to re-hash the IR per slice, saw {cold}"
    );

    // Warm: an `Arc`-backed warm of the same two addresses (the `Arc` payload IS the
    // `Module`, so the driver's later `&Module` lookups land on the same memo key).
    crate::jit::cache::cached_module_blocks(&bench).expect("stage bench");
    crate::jit::cache::cached_module_blocks(&worker).expect("stage worker");

    let before = crate::jit::cache::fingerprint_calls();
    let (value, _, leftovers) =
        drive_probe(&bench, bench_atom, &worker, worker_atom, 64, Tier::BaselineJit)
            .expect("the warmed probe completes");
    let warm = crate::jit::cache::fingerprint_calls() - before;
    assert_eq!(value, make_small(64));
    assert_eq!(leftovers, (0, 0));
    assert_eq!(
        warm, 0,
        "a warmed entry module must be resolved without re-hashing its IR"
    );
}

/// **The receive/Send zero-deopt pin**: the `send` + receive-control family must be
/// lowered as native code, not paid as a per-instruction `jit_step_one` deopt.
///
/// The two-process copy-bound probe exercises exactly this family (`send`, `loop_rec`,
/// `loop_rec_end`, `wait_timeout`, `timeout`, `remove_message`) through the real mailbox
/// seam, so a recording pass over it names the tier's deopts by opcode. With the family
/// emitted through the `jit_receive` helper (`super::receive`) none of its opcodes may
/// appear among the deopts, and the parser/ext-helper counter must stay zero — the tier
/// is named explicitly (`Tier::BaselineJit`).
///
/// The total deopt count is pinned too: the family accounted for the majority of the
/// pre-lowering deopts (642 of 1734 per 64 ops — `LoopRec` 257, `RemoveMessage` 128,
/// `Send` 128, `Wait` 65, `WaitTimeout` 64), so the count must fall by at least that
/// much. The remaining deopts are the structural/metadata opcodes (`PutTuple2`,
/// `TestHeap`, `Allocate`, `InitYregs`, `Bif`, `GetTupleElement`, `Test`, `Call`) that
/// are still the documented per-instruction deopt.
#[test]
fn the_receive_and_send_family_is_lowered_without_a_jit_step_one_deopt() {
    let (bench, bench_atom, worker, worker_atom) = probe_modules();

    counters::reset();
    counters::set_enabled(true);
    let (value, _, leftovers) = drive_probe(
        &bench,
        bench_atom,
        &worker,
        worker_atom,
        PROBE_COUNT_OPS,
        Tier::BaselineJit,
    )
    .expect("the counting pass completes");
    counters::set_enabled(false);
    let snapshot = counters::snapshot();

    assert_eq!(value, make_small(PROBE_COUNT_OPS as i64));
    assert_eq!(leftovers, (0, 0), "every message must be consumed");
    assert_eq!(
        snapshot.step_ext, 0,
        "the probe's modules make no `call_ext*` call"
    );
    for opcode in [
        "Send",
        "LoopRec",
        "LoopRecEnd",
        "Wait",
        "WaitTimeout",
        "Timeout",
        "RemoveMessage",
    ] {
        assert!(
            !snapshot.deopts_by_variant.contains_key(opcode),
            "`{opcode}` must be lowered natively, but it deopted {} time(s); \
             deopts by variant: {:?}",
            snapshot.deopts_by_variant.get(opcode).copied().unwrap_or(0),
            snapshot.deopts_by_variant,
        );
    }
    assert!(
        snapshot.step_one <= 1100,
        "the receive/Send family accounted for 642 of the 1734 pre-lowering deopts, so the \
         total must have fallen below 1100; saw {} ({:?})",
        snapshot.step_one,
        snapshot.deopts_by_variant,
    );
    print_probe_counters(Tier::BaselineJit.name(), &snapshot);
}
