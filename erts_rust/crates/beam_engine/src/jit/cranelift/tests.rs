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

//! In-module unit tests for the Cranelift hot tier: the backend's naming and
//! availability, the per-function native entries, the reported inlined set, and
//! a direct native call into a compiled entry to prove native code really runs.
use super::*;
use crate::jit::baseline::HALT;
use crate::{Machine, MinimalContext};
use beam_common::BeamError;
use beam_loader::{load_file, Instr};
use beam_term::{is_small, make_small, signed_value};

/// The `beam_loader` oracle fixture (reused, never copied).
const SLICE_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../beam_loader/tests/fixtures/slice.beam"
);

/// A second oracle fixture whose functions contain a **non-tail local call** (e.g.
/// `apply_adder/2` calls `make_adder/1` before applying the closure), so the
/// continuation pc `call_site + 1` is exercisable — `slice.beam`'s own calls are all
/// tail calls.
const COVERAGE_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../beam_loader/tests/fixtures/coverage.beam"
);

/// The slice BIF hook: `erlang:'+'/2` and `erlang:'-'/2` over small integers.
fn arith_hook(
    module: &str,
    function: &str,
    arity: u32,
    args: &[Eterm],
) -> core::result::Result<Eterm, BeamError> {
    if module != "erlang" || arity != 2 || args.len() != 2 {
        return Err(BeamError::BadArg);
    }
    if !is_small(args[0]) || !is_small(args[1]) {
        return Err(BeamError::BadArith);
    }
    let (a, b) = (signed_value(args[0]), signed_value(args[1]));
    match function {
        "+" => Ok(make_small(a + b)),
        "-" => Ok(make_small(a - b)),
        _ => Err(BeamError::BadArg),
    }
}

#[test]
fn the_tier_names_itself_and_reports_available() {
    let backend = CraneliftJit::new();
    assert_eq!(JitBackend::name(&backend), "jit-cranelift");
    assert!(JitBackend::available(&backend));
}

#[test]
fn compile_emits_one_native_entry_per_non_empty_function() {
    let module = load_file(SLICE_BEAM).expect("load slice.beam");
    let mut backend = CraneliftJit::new();
    let compiled = backend.compile(&module).expect("compile slice.beam");

    let expected: usize = module
        .functions
        .iter()
        .filter(|function| {
            !function.code.is_empty() && function.labels.contains_key(&function.entry_label)
        })
        .count();
    assert!(expected > 0, "the fixture must have functions");
    assert_eq!(compiled.function_count(), expected);

    // Every compiled function's entry is non-null and its entry `pc` is the
    // function's own entry label.
    for (index, function) in module.functions.iter().enumerate() {
        let Some(&pc) = function.labels.get(&function.entry_label) else {
            continue;
        };
        let func = index as u32;
        assert_ne!(compiled.entry(func), Some(std::ptr::null()));
        assert_eq!(compiled.entry_pc(func), Some(pc as u32));
    }
}

#[test]
fn a_function_without_an_entry_label_is_not_compiled() {
    // An empty module compiles to an empty artifact (no functions, no entries) —
    // the driver keeps interpreting it.
    let module = Module::default();
    let mut backend = CraneliftJit::new();
    let compiled = backend.compile(&module).expect("compile an empty module");
    assert_eq!(compiled.function_count(), 0);
    assert_eq!(compiled.highest_native_register(), None);
}

#[test]
fn the_compiled_function_runs_the_export_natively() {
    // Evidence that native code really is entered and executed (rather than the
    // driver silently interpreting): build the runtime block by hand, call the
    // compiled `slice:add/2` entry **directly** and check both the control word
    // its native loop returns and the x-register it left behind.
    let module = load_file(SLICE_BEAM).expect("load slice.beam");
    let add = module
        .atom_table
        .lookup("add")
        .expect("the fixture interns `add`");
    let entry_label = module.export(add, 2).expect("add/2 is exported");
    let labels = crate::interpreter::build_label_map(&module);
    let (func, pc) = crate::interpreter::resolve_label(&module, entry_label)
        .expect("the export label resolves");

    let mut backend = CraneliftJit::new();
    let compiled = backend.compile(&module).expect("compile slice.beam");
    // The function is entered at its own entry address; any other address is the
    // driver's interpreter step.
    assert_eq!(compiled.entry_pc(func), Some(pc));
    let code = CompiledEntries { compiled: &compiled };

    let mut machine = Machine::new();
    machine.reset();
    machine.set_x(0, make_small(2)).expect("seed x0");
    machine.set_x(1, make_small(3)).expect("seed x1");
    machine.push_frame(0).expect("push the bottom frame");
    machine.set_literals(module.literals.clone());

    let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
    let x_regs: *mut Eterm = machine.x.as_mut_ptr();
    let machine_ptr: *mut Machine = &mut machine;
    let ctx_ptr = &mut ctx as *mut dyn ExecutionContext;
    let mut rt = JitRuntime {
        machine: machine_ptr,
        x_regs,
        y_base: std::ptr::null_mut(),
        y_end: std::ptr::null_mut(),
        module: &module as *const Module,
        entry_module: &module as *const Module,
        labels: &labels as *const _,
        ctx: ctx_ptr,
        error: None,
        enter: None,
        // No literal area is bound for this hand-built runtime block. The tier's
        // inline set never reads an `Operand::Literal` (a `move` with a literal
        // source is a shape the inline path declines and deopts to `jit_step_one`),
        // so a null area is inert here — exactly as the driver's own "null means
        // reject every access" discipline documents.
        literals_base: std::ptr::null(),
        literals_end: std::ptr::null(),
    };

    let word = code
        .enter(&mut rt, &module, func, pc)
        .expect("the entry address is compiled");
    // The native CFG ran to the function's `return`: the bottom frame's
    // continuation, i.e. HALT.
    assert_eq!(word, HALT);
    // SAFETY: `machine_ptr` points at the live `machine` above; no native code is
    // running here.
    assert_eq!(unsafe { (*machine_ptr).x(0) }, Some(make_small(5)));
}

#[test]
fn inlined_moves_are_reported_as_touched_registers() {
    // `slice.beam`'s `add/2` body moves x-registers around, so the artifact must
    // record the highest one the inlined fast path addresses in `machine.x`.
    let module = load_file(SLICE_BEAM).expect("load slice.beam");
    let compiled = CraneliftJit::new()
        .compile(&module)
        .expect("compile slice.beam");
    assert!(
        compiled.highest_native_register().is_some(),
        "the fixture must inline at least one move"
    );
}

/// The pcs a compiled function is expected to be natively enterable at — the
/// independent test-side oracle for [`CompiledFunctions::is_enterable`]: the IR block
/// starts (label positions) plus every non-tail-call continuation `call_site + 1`,
/// each restricted to an address that holds a real instruction.
fn expected_enter_pcs(function: &beam_loader::Function) -> std::collections::BTreeSet<u32> {
    let mut pcs: std::collections::BTreeSet<u32> = std::collections::BTreeSet::new();
    for &pc in function.labels.values() {
        pcs.insert(pc as u32);
    }
    for (index, instr) in function.code.iter().enumerate() {
        let stores_continuation = matches!(
            instr,
            Instr::Call { .. }
                | Instr::CallExt { .. }
                | Instr::CallFun { .. }
                | Instr::CallFun2 { .. }
                | Instr::Apply { .. }
        );
        if stores_continuation {
            pcs.insert(index as u32 + 1);
        }
    }
    pcs.retain(|&pc| (pc as usize) < function.code.len());
    pcs
}

#[test]
fn a_compiled_function_is_enterable_at_every_block_start_and_call_continuation() {
    // The pc-entry contract: a compiled function may be natively entered at its entry
    // `pc`, at every other IR block start, and at every non-tail-call continuation
    // `call_site + 1` — but at no interior pc that no boundary names.
    let module = load_file(COVERAGE_BEAM).expect("load coverage.beam");
    let compiled = CraneliftJit::new()
        .compile(&module)
        .expect("compile coverage.beam");

    let mut saw_continuation = false;
    let mut saw_unrelated = false;
    for (index, function) in module.functions.iter().enumerate() {
        let Some(&entry_pc) = function.labels.get(&function.entry_label) else {
            continue;
        };
        let func = index as u32;
        // The entry `pc` is always enterable (the pre-existing behaviour).
        assert!(
            compiled.is_enterable(func, entry_pc as u32),
            "the entry pc of function {func} must be enterable"
        );

        // Every non-tail local call's continuation is enterable — the capability this
        // pc-entry dispatch adds (before it, only the entry pc was).
        for (pc, instr) in function.code.iter().enumerate() {
            if matches!(instr, Instr::Call { .. }) && pc + 1 < function.code.len() {
                assert!(
                    compiled.is_enterable(func, pc as u32 + 1),
                    "the continuation after the call at {pc} in function {func} must be enterable"
                );
                saw_continuation = true;
            }
        }

        // The enterable set is exactly the derived boundary set: an interior pc that no
        // label or call continuation names is not enterable.
        let expected = expected_enter_pcs(function);
        for pc in 0..function.code.len() as u32 {
            assert_eq!(
                compiled.is_enterable(func, pc),
                expected.contains(&pc),
                "is_enterable(func={func}, pc={pc}) must match the derived boundary set"
            );
            if !expected.contains(&pc) {
                saw_unrelated = true;
            }
        }
    }

    assert!(
        saw_continuation,
        "the fixture must contain a non-tail local call"
    );
    assert!(
        saw_unrelated,
        "the fixture must contain an interior pc no boundary names"
    );

    // An uncompiled function index and an out-of-range pc are never enterable.
    assert!(!compiled.is_enterable(u32::MAX, 0));
    assert!(!compiled.is_enterable(0, u32::MAX));
}
