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
// the per-instruction reduction charge on the driver's deopt path
// ---------------------------------------------------------------------------

/// The non-allocating loop `label 1 | bif0 host:counter/0 x0 | jump 1`, exported as
/// `run/0`, and left with **no** block decomposition on purpose: the native tier then
/// compiles no block, so every instruction takes the driver's documented
/// per-instruction deopt ([`Machine::step_instruction`]) rather than being inlined —
/// which is exactly the path the reduction hook is charged on.
fn yield_loop_module() -> (Module, Atom) {
    let mut module = Module::default();
    module.name = module.atom_table.intern("loop");
    let function_name = module.atom_table.intern("run");
    let host = module.atom_table.intern("host");
    let counter = module.atom_table.intern("counter");
    module.imports.push(Import {
        module: host,
        function: counter,
        arity: 0,
    });
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Bif0 {
            bif: ImportRef(0),
            dst: Operand::X(0),
        },
        Instr::Jump { target: Label(1) },
    ];
    let labels = code
        .iter()
        .enumerate()
        .filter_map(|(i, ins)| match ins {
            Instr::Label(l) => Some((*l, i)),
            _ => None,
        })
        .collect();
    module.functions = vec![Function {
        func_label: Label(1),
        entry_label: Label(1),
        module: module.name,
        name: function_name,
        arity: 0,
        code,
        blocks: Vec::new(),
        labels,
    }];
    module.exports = vec![Export {
        function: function_name,
        arity: 0,
        label: Label(1),
    }];
    (module, function_name)
}

/// A host that serves `host:counter/0` (returning the call count, and stopping the
/// loop with a typed error once `calls > stop_after`) and whose reduction hook asks
/// for a cooperative yield once it has accepted `yield_after` charges.
struct YieldCtx {
    stop_after: u32,
    calls: u32,
    charges: u32,
    yield_after: Option<u32>,
}

impl ExecutionContext for YieldCtx {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        _args: &[Eterm],
    ) -> Result<Eterm> {
        if module == "host" && function == "counter" && arity == 0 {
            self.calls += 1;
            return if self.calls > self.stop_after {
                Err(error::unsupported("stop"))
            } else {
                Ok(make_small(self.calls as i64))
            };
        }
        Err(error::unsupported(&format!("bif {module}:{function}")))
    }

    fn ensure_heap(
        &mut self,
        _heap_need: u32,
        _live: u32,
        _x_regs: &mut [Eterm],
        _stack: &mut [Eterm],
    ) -> Result<()> {
        Ok(())
    }

    fn reduce_outcome(&mut self, n: u32) -> Result<ReduceOutcome> {
        assert_eq!(n, 1, "the driver charges exactly one reduction per deopt");
        self.charges += 1;
        if let Some(budget) = self.yield_after {
            if self.charges >= budget {
                return Ok(ReduceOutcome::Yield);
            }
        }
        Ok(ReduceOutcome::Continue)
    }
}

/// The driver's deopt path is priced like the interpreter's loop: one charge per
/// interpreted instruction, and a `Yield` answer stops the run at the **next**
/// instruction (the step is applied first).
#[test]
fn the_deopt_path_charges_one_reduction_per_instruction_and_reports_a_yield() {
    let (module, run) = yield_loop_module();
    let mut machine = Machine::new();
    let mut ctx = YieldCtx {
        stop_after: u32::MAX,
        calls: 0,
        charges: 0,
        yield_after: Some(3),
    };

    let outcome = machine
        .run_export_outcome_with_tier(&module, run, 0, &[], &mut ctx, Tier::BaselineJit)
        .expect("the deopt path reports the pre-emption");
    // Charges #1/#2/#3 were the `label` at code 0, the `bif0` at code 1 and the
    // `jump` at code 2; the step was applied first, so the yield names the jump's
    // target — the loop head at code 0, never a re-execution of the jump.
    assert_eq!(outcome, RunOutcome::Yield { func: 0, pc: 0 });
    assert_eq!(ctx.charges, 3, "one charge per deopt instruction");
    assert_eq!(
        machine.reductions(),
        3,
        "the machine's slice counter agrees on the deopt path"
    );
    assert_eq!(ctx.calls, 1);
}

/// A non-blocking native entry cannot suspend a pre-empted process, so it folds the
/// deopt yield into the same typed pre-emption error the interpreter's non-blocking
/// entry produces.
#[test]
fn a_non_blocking_native_entry_folds_a_deopt_yield_into_the_typed_error() {
    let (module, run) = yield_loop_module();
    let mut machine = Machine::new();
    let mut ctx = YieldCtx {
        stop_after: u32::MAX,
        calls: 0,
        charges: 0,
        yield_after: Some(1),
    };

    let err = machine
        .run_with_tier(&module, run, 0, &[], &mut ctx, Tier::BaselineJit)
        .expect_err("a non-blocking entry cannot suspend a pre-empted process");
    match err {
        beam_common::BeamError::Raise(raise) => assert_eq!(
            raise.reason.as_atom(),
            Some("unsupported_opcode: run: process pre-empted (reduction budget exhausted)")
        ),
        other => panic!("expected the typed pre-emption error, got {other:?}"),
    }
}

/// The reported yield address is a real, resumable position: resuming there (the
/// interpreter's `resume_outcome` re-enters the same suspended machine) continues
/// the loop instead of re-executing the instruction already charged.
#[test]
fn a_deopt_yield_address_resumes_the_loop_to_its_terminal_outcome() {
    let (module, run) = yield_loop_module();
    let mut machine = Machine::new();
    // The `counter/0` BIF stops the loop on its 3rd call, so the resumed run ends
    // deterministically.
    let mut ctx = YieldCtx {
        stop_after: 2,
        calls: 0,
        charges: 0,
        yield_after: Some(2),
    };

    let outcome = machine
        .run_export_outcome_with_tier(&module, run, 0, &[], &mut ctx, Tier::BaselineJit)
        .expect("the pre-emption is reported, not raised");
    // Charge #2 was the `bif0` (call #1); its `Step::Next` moved the position to 2.
    assert_eq!(outcome, RunOutcome::Yield { func: 0, pc: 2 });

    // Resume with the yield switched off: the loop runs on to the terminal raise.
    ctx.yield_after = None;
    let err = machine
        .resume_outcome(&module, 0, 2, &mut ctx)
        .expect_err("the counter BIF ends the loop");
    match err {
        beam_common::BeamError::Raise(raise) => {
            assert_eq!(raise.reason.as_atom(), Some("unsupported_opcode: stop"))
        }
        other => panic!("expected the host's typed error, got {other:?}"),
    }
    assert_eq!(ctx.calls, 3, "the failing 3rd call is not charged");
}
